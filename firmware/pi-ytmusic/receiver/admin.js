/**
 * The receiver's own admin page: is it working, and the two things that must be
 * true before a cast is ever audible.
 *
 * The phone's YouTube Music app is the control surface for *playback* — this page
 * is not a remote. It exists because casting only produces sound when two things
 * outside that app are set up:
 *
 *   1. the audio router has an **RTP source** listening on the port this receiver
 *      sends to (without it the audio goes nowhere and now-playing reports 404);
 *   2. there is a **cookie jar** yt-dlp can resolve with (without it playback is
 *      anonymous, which YouTube tolerates only briefly).
 *
 * Both used to be workstation errands — creating the source by hand in the
 * router's UI, and `push_cookies.py` over ssh + `docker exec` — which is precisely
 * what kept this add-on from being installable in one click. This page is those
 * two errands, plus the answer to "why is nothing playing".
 *
 * Deliberately dependency-free and build-free: one `http` server and one static
 * HTML file. The app is shared verbatim with the Raspberry Pi role, which is
 * installed by `scp -r firmware/pi-ytmusic` and has to stay self-contained — a
 * Vite build here would break that, and `node_modules` is not the place for a
 * settings page.
 *
 * **Only started when YTCR_ADMIN_PORT is set.** In the add-on that port is
 * ingress, so Home Assistant has already authenticated whoever is looking. On the
 * Pi it is off unless asked for, because this page writes a Google credential and
 * there is nothing in front of it there to ask who you are.
 */

import { createServer } from 'http';
import { execFile } from 'child_process';
import { existsSync } from 'fs';
import { readFile, rename, stat, unlink, writeFile } from 'fs/promises';
import path from 'path';
import { fileURLToPath } from 'url';

const HERE = path.dirname(fileURLToPath(import.meta.url));

/** The RTP source the router must have for this receiver to be audible.
 *
 *  `rate` is not a preference: the whole path is 48 kHz (YouTube's Opus, the
 *  `ytm-out` sink, the router's graph), so a source at any other rate inserts a
 *  resampler in the one place nothing should resample. `ignore_ssrc` matters after
 *  *this* service restarts: the RTP stream comes back with a new SSRC, and a source
 *  still pinned to the old one drops every packet while looking perfectly healthy. */
const REQUIRED_RTP = { rate: 48000, ignore_ssrc: true };
/** Shown as the source's name in the router's UI and matrix. */
const SOURCE_LABEL = 'YouTube Music';

/** Give up on the router quickly — this is a page someone is waiting on. */
const ROUTER_TIMEOUT_MS = 3000;
/** A cold resolve on a Pi is slow, and this button is explicitly "prove it". */
const PROBE_TIMEOUT_MS = 90_000;
/** Cheap-but-not-free facts, cached for as long as they can't meaningfully change. */
const TTL_MS = { ytdlp: 60_000, sink: 5000 };
/** A Netscape jar of a browser's Google cookies is a few kB; 1 MB is already absurd. */
const MAX_UPLOAD_BYTES = 1024 * 1024;

/** Cache a promise-returning probe for `ttl`, so a page refresh is not a spawn storm. */
function cached(ttl, fn) {
  let at = 0;
  let value;
  return async () => {
    const now = Date.now();
    if (now - at > ttl) {
      at = now;
      value = await fn().catch(() => null);
    }
    return value;
  };
}

/** execFile as a promise that resolves the outcome instead of throwing on exit≠0. */
function exec(file, args, { timeout = 10_000, input = null } = {}) {
  return new Promise((resolve) => {
    const child = execFile(file, args, { timeout, encoding: 'utf8' }, (error, stdout, stderr) => {
      resolve({ ok: !error, code: error?.code ?? 0, stdout: stdout ?? '', stderr: stderr ?? '' });
    });
    if (input !== null) {
      child.stdin.end(input);
    }
  });
}

async function readBody(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_UPLOAD_BYTES) {
      throw new Error(`body larger than ${MAX_UPLOAD_BYTES} bytes`);
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}

function json(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(payload),
    // Status is live by definition, and an ingress page is re-fetched constantly.
    'Cache-Control': 'no-store',
  });
  res.end(payload);
}

// --- the audio router ------------------------------------------------------

/**
 * The router's REST API, as much of it as this page needs.
 *
 * Sources are a keyed collection (`/api/sources`), so "our" source is found by the
 * only thing both ends agree on: the UDP port this receiver transmits to. Not by
 * label — the user may rename it, and renaming it must not orphan it.
 */
class Router {
  #base;
  #rtpPort;

  constructor({ host, apiPort, rtpPort }) {
    this.#base = `http://${host}:${apiPort}`;
    this.#rtpPort = rtpPort;
  }

  get base() {
    return this.#base;
  }

  async #fetch(pathname, init = {}) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), ROUTER_TIMEOUT_MS);
    try {
      const res = await fetch(`${this.#base}${pathname}`, { ...init, signal: controller.signal });
      const text = await res.text();
      let body = null;
      try {
        body = text ? JSON.parse(text) : null;
      }
      catch {
        body = null; // /health answers plain text
      }
      return { ok: res.ok, status: res.status, body, text };
    }
    finally {
      clearTimeout(timer);
    }
  }

  /** Everything the page shows about the router, including why it can't be reached. */
  async status() {
    let sources;
    try {
      sources = await this.#fetch('/api/sources');
    }
    catch (e) {
      // node's fetch says only "fetch failed"; the cause carries the actual reason
      // (ECONNREFUSED = the add-on is not running), which is the whole answer here.
      const why = e.name === 'AbortError'
        ? `no answer within ${ROUTER_TIMEOUT_MS} ms`
        : `${e.message}${e.cause?.code ? ` (${e.cause.code})` : ''}`;
      return { base: this.#base, reachable: false, error: why, source: null, mismatch: [] };
    }
    if (!sources.ok) {
      return { base: this.#base, reachable: true, error: `GET /api/sources → ${sources.status}`,
        source: null, mismatch: [] };
    }
    const found = (sources.body?.sources ?? []).find((s) => s.kind === 'rtp' && s.rtp?.port === this.#rtpPort);
    return {
      base: this.#base,
      reachable: true,
      error: null,
      source: found ?? null,
      mismatch: found ? mismatches(found.rtp) : [],
    };
  }

  /**
   * Make the router have a correct source on our port: create it, or correct the
   * one that is already there.
   *
   * The PUT sends the *existing* config with only the wrong fields replaced,
   * because `PUT /api/sources/{id}` replaces the whole config object — a partial
   * body would quietly reset the latency the user tuned.
   */
  async ensureSource() {
    const before = await this.status();
    if (!before.reachable) {
      return { ok: false, error: before.error, router: before };
    }
    if (!before.source) {
      const res = await this.#fetch('/api/sources', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          label: SOURCE_LABEL,
          kind: 'rtp',
          rtp: { port: this.#rtpPort, source_addr: '0.0.0.0', ...REQUIRED_RTP },
        }),
      });
      if (!res.ok) {
        // The router validates port collisions itself; pass its own words through.
        return { ok: false, error: res.body?.message ?? `POST /api/sources → ${res.status}`,
          router: await this.status() };
      }
      return { ok: true, action: 'created', router: await this.status() };
    }
    if (before.mismatch.length === 0) {
      return { ok: true, action: 'unchanged', router: before };
    }
    const res = await this.#fetch(`/api/sources/${encodeURIComponent(before.source.id)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ rtp: { ...before.source.rtp, ...REQUIRED_RTP } }),
    });
    if (!res.ok) {
      return { ok: false, error: res.body?.message ?? `PUT /api/sources → ${res.status}`,
        router: await this.status() };
    }
    return { ok: true, action: 'corrected', router: await this.status() };
  }
}

/** Which of the required RTP settings the router's source gets wrong, in words. */
function mismatches(rtp) {
  const out = [];
  if (rtp.rate !== REQUIRED_RTP.rate) {
    out.push(`sample rate is ${rtp.rate} Hz, needs ${REQUIRED_RTP.rate} Hz`);
  }
  if (rtp.ignore_ssrc !== REQUIRED_RTP.ignore_ssrc) {
    out.push('"ignore SSRC" is off, so audio stops after this receiver restarts');
  }
  return out;
}

// --- the cookie jar --------------------------------------------------------

/**
 * The jar on disk, judged by the same rules as `push_cookies.py`.
 *
 * The verdict comes from `cookie_jar.py` rather than a second implementation in
 * JavaScript: a jar this page accepts and the workstation tool refuses (or the
 * reverse) would be a bug nobody could explain from either end.
 */
class CookieJar {
  #file;
  #python;
  #script = path.join(HERE, 'cookie_jar.py');
  #ytdlp;
  #jsRuntime;
  #probeUrl;

  constructor({ file, python, ytdlp, jsRuntime, probeUrl = null }) {
    this.#file = file;
    this.#python = python;
    this.#ytdlp = ytdlp;
    this.#jsRuntime = jsRuntime;
    // The probe video is cookie_jar.py's to choose (it also knows how to read a
    // failure), so it is asked rather than copied. YTCR_PROBE_URL overrides.
    this.#probeUrl = probeUrl
      ? async () => probeUrl
      : cached(Infinity, async () => {
        const r = await exec(this.#python, [ this.#script, '--print-probe-url' ]);
        return r.ok ? r.stdout.trim() : null;
      });
  }

  get file() {
    return this.#file;
  }

  /** Ask cookie_jar.py what a jar text is worth. Null if the helper is unusable. */
  async #verdict(text) {
    const r = await exec(this.#python, [ this.#script, '-', '--json' ], { input: text });
    if (!r.ok && !r.stdout) {
      return null;
    }
    try {
      return JSON.parse(r.stdout);
    }
    catch {
      return null;
    }
  }

  async status() {
    let info;
    try {
      info = await stat(this.#file);
    }
    catch {
      return { path: this.#file, present: false, modified: null, verdict: null };
    }
    const text = await readFile(this.#file, 'utf8').catch(() => '');
    return {
      path: this.#file,
      present: true,
      modified: info.mtimeMs,
      size: info.size,
      verdict: await this.#verdict(text),
    };
  }

  /**
   * Install an uploaded jar — but refuse to roll a rotated one back by accident.
   *
   * `yt-dlp --cookies FILE` dumps refreshed cookies back into FILE, so the copy on
   * disk is a live credential that may well be *newer* than the export someone is
   * uploading. Writing the older one over it can invalidate the session outright,
   * which is why push_cookies.py has the same guard behind --force. `sourceModified`
   * is the browser's `File.lastModified`; without it there is nothing to compare and
   * the upload is taken at face value.
   */
  async install(text, { sourceModified = null, force = false } = {}) {
    const verdict = await this.#verdict(text);
    if (!verdict) {
      return { ok: false, code: 'helper_failed',
        error: `could not run ${this.#python} ${this.#script}` };
    }
    if (!verdict.ok) {
      return { ok: false, code: verdict.code, error: verdict.summary, verdict };
    }
    if (!force && sourceModified) {
      const existing = await stat(this.#file).catch(() => null);
      if (existing && existing.mtimeMs > sourceModified) {
        return { ok: false, code: 'would_roll_back', verdict,
          error: 'The jar already on disk is newer than this file. yt-dlp rewrites it as '
            + 'cookies rotate, so replacing it with an older export can invalidate the '
            + 'session. Upload again with "replace anyway" if this export is the fresh one.' };
      }
    }
    // Written 0600 and atomically: the file is a full Google credential, and a
    // half-written jar is one yt-dlp would refuse to parse mid-rotation.
    const tmp = `${this.#file}.tmp`;
    await writeFile(tmp, text, { mode: 0o600 });
    await rename(tmp, this.#file);
    return { ok: true, verdict };
  }

  async remove() {
    await unlink(this.#file).catch(() => {});
    return { ok: true };
  }

  /**
   * Prove the jar resolves a real video, with this box's own yt-dlp.
   *
   * The same check `push_cookies.py --check` runs over ssh, run locally where the
   * answer is actually needed. A failure is classified by cookie_jar.py, because
   * "the probe video died" and "no JavaScript runtime" are both *not* the cookies,
   * and replacing a good jar is the wrong response to either.
   */
  async check() {
    if (!existsSync(this.#file)) {
      return { ok: false, code: 'no_jar', error: 'No cookie jar installed yet.' };
    }
    if (!this.#ytdlp) {
      return { ok: false, code: 'no_ytdlp', error: 'No yt-dlp on this box (YTCR_YTDL_PATH unset).' };
    }
    const probeUrl = await this.#probeUrl();
    if (!probeUrl) {
      return { ok: false, code: 'no_probe_url',
        error: `could not run ${this.#python} ${this.#script} --print-probe-url` };
    }
    const r = await exec(this.#ytdlp, [
      '--js-runtimes', this.#jsRuntime,
      '--cookies', this.#file,
      '--skip-download', '--simulate', '--no-warnings',
      '--print', '%(title)s',
      probeUrl,
    ], { timeout: PROBE_TIMEOUT_MS });
    if (r.ok) {
      return { ok: true, title: r.stdout.trim() };
    }
    const output = (r.stderr || r.stdout).trim();
    const classified = await exec(this.#python, [ this.#script, '-', '--classify-probe-error' ],
      { input: output });
    let verdict = { code: 'unknown', message: 'Resolution failed.' };
    try {
      verdict = JSON.parse(classified.stdout);
    }
    catch { /* keep the fallback */ }
    return { ok: false, code: verdict.code, error: verdict.message, output };
  }
}

// --- the receiver's own state ----------------------------------------------

/** Whether the `ytm-out` sink actually loaded — the difference between "playing"
 *  and "playing to nowhere". run.sh warns about this once at boot; this is the
 *  same question, answerable later. Null when pw-cli isn't available (not an
 *  add-on), which the UI shows as "unknown" rather than "broken". */
function sinkProbe(sinkName) {
  return cached(TTL_MS.sink, async () => {
    const r = await exec('pw-cli', [ 'ls', 'Node' ], { timeout: 4000 });
    if (!r.ok && !r.stdout) {
      return null;
    }
    return r.stdout.includes(`node.name = "${sinkName}"`);
  });
}

function ytdlpVersionProbe(ytdlp) {
  return cached(TTL_MS.ytdlp, async () => {
    if (!ytdlp) {
      return null;
    }
    const r = await exec(ytdlp, [ '--version' ], { timeout: 15_000 });
    return r.ok ? r.stdout.trim() : null;
  });
}

/**
 * What mpv is doing, asked rather than tracked.
 *
 * mpv is the authority on this and answers over its IPC socket in microseconds, so
 * there is no second copy of the playback state to go stale. `media-title` is what
 * `ytdl_hook` filled in from yt-dlp, i.e. the actual track name.
 */
async function playback(mpv) {
  if (!mpv?.running) {
    return { state: 'down', title: null, position_ms: null, duration_ms: null };
  }
  const [ idle, paused, title, pos, dur ] = await Promise.all([
    mpv.getProperty('idle-active', true),
    mpv.getProperty('pause', true),
    mpv.getProperty('media-title', null),
    mpv.getProperty('time-pos', null),
    mpv.getProperty('duration', null),
  ]);
  const state = idle ? 'idle' : (paused ? 'paused' : 'playing');
  return {
    state,
    title: idle ? null : (typeof title === 'string' ? title : null),
    position_ms: typeof pos === 'number' ? Math.round(pos * 1000) : null,
    duration_ms: typeof dur === 'number' ? Math.round(dur * 1000) : null,
  };
}

// --- server ----------------------------------------------------------------

/**
 * Start the admin page. Returns the http.Server, or null when no port is set —
 * which is the normal case on the Pi (see this file's header).
 *
 * @param {object} o
 * @param {number|null} o.port           YTCR_ADMIN_PORT; null disables the page entirely.
 * @param {string|undefined} o.bind      Address to listen on; undefined = every interface.
 * @param {object} o.device              What this receiver advertises (name, ports, sink).
 * @param {object} o.router              `{host, apiPort}` of the audio-router add-on.
 * @param {number} o.rtpPort             The UDP port this receiver transmits to.
 * @param {object} o.jar                 `{file, python, ytdlp, jsRuntime}` for the cookie jar.
 * @param {object} o.probes              `{mpv, resolverDaemon, sender}` live state accessors.
 */
export default function startAdmin({ port, bind, device, router, rtpPort, jar, probes, logger = console }) {
  if (!port) {
    return null;
  }
  const api = new Router({ ...router, rtpPort });
  const cookies = new CookieJar(jar);
  const sink = sinkProbe(device.sinkName);
  const ytdlpVersion = ytdlpVersionProbe(jar.ytdlp);
  const page = path.join(HERE, 'admin.html');

  const handlers = {
    'GET /api/status': async () => {
      const [ routerStatus, cookieStatus, sinkPresent, version, play ] = await Promise.all([
        api.status(), cookies.status(), sink(), ytdlpVersion(), playback(probes.mpv),
      ]);
      return [ 200, {
        receiver: {
          version: process.env.ADDON_VERSION ?? null,
          device_name: device.name,
          screen_name: device.screenName,
          dial_port: device.dialPort,
          bind_address: device.bindAddress ?? null,
          sink_name: device.sinkName,
          sink_present: sinkPresent,
          resolver_daemon: probes.resolverDaemon(),
          ytdlp_version: version,
          sender: probes.sender(),
          playback: play,
        },
        rtp: { host: router.host, port: rtpPort, required: REQUIRED_RTP },
        router: routerStatus,
        cookies: cookieStatus,
      } ];
    },

    'POST /api/rtp-source': async () => {
      const result = await api.ensureSource();
      logger.info(`[admin] RTP source on :${rtpPort}: ${result.ok ? result.action : `failed — ${result.error}`}`);
      return [ result.ok ? 200 : 502, result ];
    },

    'POST /api/cookies': async (req, url) => {
      const text = await readBody(req);
      const modifiedHeader = Number(req.headers['x-jar-modified']);
      const result = await cookies.install(text, {
        sourceModified: Number.isFinite(modifiedHeader) && modifiedHeader > 0 ? modifiedHeader : null,
        force: url.searchParams.get('force') === '1',
      });
      // Never log the jar, its contents, or a cookie name — only the outcome.
      logger.info(`[admin] cookie jar upload: ${result.ok ? 'installed' : `refused (${result.code})`}`);
      return [ result.ok ? 200 : 400, { ...result, cookies: await cookies.status() } ];
    },

    'DELETE /api/cookies': async () => {
      await cookies.remove();
      logger.info('[admin] cookie jar removed — resolving anonymously from the next track');
      return [ 200, { ok: true, cookies: await cookies.status() } ];
    },

    'POST /api/cookies/check': async () => {
      const result = await cookies.check();
      logger.info(`[admin] cookie liveness check: ${result.ok ? `resolved ${result.title}` : result.code}`);
      return [ 200, result ];
    },
  };

  const server = createServer((req, res) => {
    // Relative to whatever base the page was served from, so the same handler
    // works behind Home Assistant ingress (a dynamic /api/hassio_ingress/<token>/
    // prefix) and on a bare port.
    const url = new URL(req.url, `http://${req.headers.host ?? 'localhost'}`);
    const route = `${req.method} ${url.pathname.replace(/^.*(?=\/api\/)/, '')}`;
    const handler = handlers[route];
    if (handler) {
      handler(req, url)
        .then(([ status, body ]) => json(res, status, body))
        .catch((e) => {
          logger.warn(`[admin] ${route} failed: ${e.message}`);
          json(res, 500, { ok: false, error: e.message });
        });
      return;
    }
    if (req.method !== 'GET' && req.method !== 'HEAD') {
      json(res, 405, { ok: false, error: `${req.method} not allowed here` });
      return;
    }
    // Everything else is the page. One file, so no path traversal to worry about.
    readFile(page).then((html) => {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' });
      res.end(html);
    }).catch((e) => {
      logger.error(`[admin] cannot read ${page}: ${e.message}`);
      json(res, 500, { ok: false, error: 'admin.html missing from this install' });
    });
  });

  server.on('error', (e) => {
    // A busy port must not take the receiver down with it: playback does not
    // depend on this page existing.
    logger.error(`[admin] not serving the admin page: ${e.message}`);
  });
  server.listen(port, bind, () => {
    logger.info(`[admin] admin page on ${bind ?? '0.0.0.0'}:${port}`);
  });
  return server;
}
