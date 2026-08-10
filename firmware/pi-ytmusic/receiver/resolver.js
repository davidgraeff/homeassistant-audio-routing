/**
 * Pre-resolves upcoming tracks to direct stream URLs, so a track change does not
 * stall on `yt-dlp`.
 *
 * WHY THIS EXISTS
 * ---------------
 * Resolution is *slow* here, and measured (Pi Zero 2 W, authenticated):
 *
 *     anonymous, no JS challenge        ~6 s
 *     authenticated, node 22 (JIT)      ~22 s
 *     authenticated, quickjs (no JIT)   ~90 s
 *
 * The gap is YouTube's `n` signature challenge, which only *authenticated*
 * requests trigger — and it is paid **per yt-dlp process**, because everything
 * expensive (the downloaded player JS in `_code_cache`, derived data in
 * `_player_cache`, PO tokens in the `memory` cache provider) lives in
 * process-local state. mpv's `ytdl_hook` spawns a fresh yt-dlp per track, so every
 * track pays full price.
 *
 * Rather than making resolution fast, this makes it happen *early*: while track N
 * plays, resolve track N+1 and keep its direct URL. `doPlay` then hands mpv a
 * plain https URL, which skips `ytdl_hook` entirely and starts in about a second.
 *
 * WHY NOT mpv's OWN PLAYLIST PREFETCH
 * -----------------------------------
 * Appending upcoming tracks to mpv's playlist would let mpv advance on its own,
 * and the cast session's queue bookkeeping (yt-cast-receiver's `Playlist`, which
 * the phone's UI mirrors) would drift out of sync with what is actually playing.
 * The queue stays authoritative; only the *resolution* is pre-done.
 */

import { spawn } from 'child_process';
import fs from 'fs';
import os from 'os';

/**
 * A resolve must never compete with playback.
 *
 * Measured on the Pi Zero 2 W (2026-08-10): one resolve is `yt-dlp` at ~26 % of a
 * core *plus* the JS runtime it spawns for the `n` challenge at **>100 % and ~88 MB
 * RSS**, for ~20 s — and because `prefetch()` is deliberately fired while track N is
 * playing, that lands mid-song on a 4-core 1 GHz box with ~80 MB free. mpv and this
 * both ran at nice 0, so the resolve could take CPU straight off the player.
 *
 * `nice` and `ionice` are set by exec'ing through the wrappers rather than after the
 * fact: the priority then applies from the first instruction, and — the part that
 * matters here — it is **inherited by the JS runtime yt-dlp spawns**, which is the
 * expensive process. `--js-runtimes` gives yt-dlp a plain binary path, so there is no
 * other way to reach that child.
 *
 * The unit's `CPUWeight` cannot express this: it throttles the whole service cgroup,
 * mpv included, i.e. the player is punished together with the offender.
 */
const RESOLVE_NICE = 19;
/** `ionice -c 3` (idle): SD-card reads for the Python/extractor import yield too. */
const IONICE_CLASS = 3;

/**
 * Build the argv prefix that lowers a resolve's priority, using whichever wrappers
 * this box actually has. Falls back to `os.setPriority` in `#run` when neither is
 * present, which covers CPU but not I/O.
 */
function priorityPrefix() {
  const prefix = [];
  if (fs.existsSync('/usr/bin/nice')) {
    prefix.push('/usr/bin/nice', '-n', String(RESOLVE_NICE));
  }
  if (fs.existsSync('/usr/bin/ionice')) {
    prefix.push('/usr/bin/ionice', '-c', String(IONICE_CLASS));
  }
  return prefix;
}

/**
 * Treat a URL as stale this long before YouTube's own `expire=`. Generous because
 * a URL is fetched minutes before it is used, and a just-expired URL costs a
 * fallback resolve at exactly the wrong moment.
 */
const EXPIRY_MARGIN_MS = 5 * 60 * 1000;
/** Hard cap on how long a resolve may run before being abandoned. */
const RESOLVE_TIMEOUT_MS = 150000;
/** Budget for the two-byte URL verification (see #verify). */
const VERIFY_TIMEOUT_MS = 8000;
/** Cache bound. The queue only ever needs the next one or two. */
const MAX_ENTRIES = 8;

export default class Resolver {
  #ytdlp;
  #cookies;
  #jsRuntime;
  #format;
  #cipherUrl;
  #cipherTimeout;
  #logger;
  /** videoId -> { url, expiresAt } */
  #cache = new Map();
  /** videoId -> Promise, so a play racing its own prefetch waits instead of duplicating. */
  #inflight = new Map();

  constructor({ ytdlp, cookies = null, jsRuntime = null, format = 'bestaudio/best',
    cipherUrl = null, cipherTimeout = 8, logger = console }) {
    this.#ytdlp = ytdlp;
    this.#cookies = cookies;
    this.#jsRuntime = jsRuntime;
    this.#format = format;
    this.#cipherUrl = cipherUrl;
    this.#cipherTimeout = cipherTimeout;
    this.#logger = logger;
  }

  /** Swap in the receiver's logger once it exists (same pattern as mpv.js). */
  setLogger(logger) {
    this.#logger = logger;
  }

  /** Whether prefetching is possible at all (a yt-dlp path is configured). */
  get enabled() {
    return !!this.#ytdlp;
  }

  /**
   * A still-valid direct URL for `videoId`, or null.
   *
   * Deliberately does NOT consume the entry: a failed load retries, and the
   * second attempt should not pay for a fresh resolve.
   */
  peek(videoId) {
    const hit = this.#cache.get(videoId);
    if (!hit) {
      return null;
    }
    if (hit.expiresAt && hit.expiresAt <= Date.now()) {
      this.#logger.debug(`[resolver] cached URL for ${videoId} expired`);
      this.#cache.delete(videoId);
      return null;
    }
    return hit.url;
  }

  /** Drop a cached URL that turned out not to work. */
  invalidate(videoId) {
    this.#cache.delete(videoId);
  }

  /**
   * The direct URL for `videoId`, resolving now if necessary — cached, joined to an
   * in-flight prefetch, or freshly resolved. Returns null on failure so the caller
   * can fall back.
   *
   * Used on the play path even when nothing was prefetched — not because resolving
   * here is faster (measured 28-31 s either way, variance larger than the
   * difference) but because it **deduplicates**: a play that arrives while its own
   * prefetch is still running waits for that resolve instead of starting a second
   * one in parallel.
   */
  async urlFor(videoId) {
    const cached = this.peek(videoId);
    if (cached) {
      return cached;
    }
    if (!this.enabled) {
      return null;
    }
    try {
      return await this.resolve(videoId);
    }
    catch (e) {
      this.#logger.warn(`[resolver] resolve ${videoId} failed: ${e.message}`);
      return null;
    }
  }

  /** Resolve in the background; never throws. This is the whole point of the class. */
  prefetch(videoId) {
    if (!videoId || !this.enabled || this.#cache.has(videoId) || this.#inflight.has(videoId)) {
      return;
    }
    this.#logger.info(`[resolver] prefetching ${videoId}`);
    this.resolve(videoId).catch((e) => this.#logger.warn(`[resolver] prefetch ${videoId} failed: ${e.message}`));
  }

  /** Resolve `videoId` to a direct URL, joining an in-flight resolve if there is one. */
  resolve(videoId) {
    const existing = this.#inflight.get(videoId);
    if (existing) {
      return existing;
    }
    const p = this.#run(videoId).finally(() => this.#inflight.delete(videoId));
    this.#inflight.set(videoId, p);
    return p;
  }

  /**
   * The two ways to solve the JS challenge, fastest first.
   *
   * These are mutually exclusive **on purpose**. yt-dlp picks one JS-challenge
   * provider by preference, the remote-cipher plugin registers no preference, and
   * every builtin runtime therefore outranks it — so passing `--js-runtimes`
   * alongside the plugin silently solves everything locally (measured: 24-29 s,
   * with `[jsc:node] Solving JS challenges using node` in the verbose log, and not
   * one request reaching the server).
   *
   * Offering *only* the remote provider is the only way to actually use it, which
   * is why the fallback is explicit here rather than left to yt-dlp: attempt 1 has
   * no local runtime, attempt 2 has no remote server.
   */
  #attempts() {
    const base = [ '--no-warnings', '--no-playlist', '-f', this.#format, '-g' ];
    if (this.#cookies) {
      base.push('--cookies', this.#cookies);
    }
    const remote = this.#cipherUrl
      ? [ {
        label: 'remote cipher',
        args: [ ...base, '--extractor-args',
          `youtubejsc-remotecipher:base_url=${this.#cipherUrl};timeout=${this.#cipherTimeout}` ],
      } ]
      : [];
    const local = this.#jsRuntime
      ? [ { label: `local ${this.#jsRuntime.split(':')[0]}`, args: [ ...base, '--js-runtimes', this.#jsRuntime ] } ]
      : [];
    return [ ...remote, ...local ];
  }

  /**
   * Prove a resolved URL actually plays, with a two-byte ranged request.
   *
   * Not paranoia — measured: the public yt-cipher instance returns a
   * *plausible-looking but wrong* `n` for roughly one request in three, and
   * googlevideo answers **403** only when you fetch it. Without this check those
   * URLs land in the cache and the failure surfaces as a dead track mid-session,
   * long after the resolve that caused it. Costs ~200 ms, and turns a wrong answer
   * into "that attempt failed, try the next one".
   */
  async #verify(url) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), VERIFY_TIMEOUT_MS);
    try {
      const res = await fetch(url, {
        headers: { Range: 'bytes=0-1' },
        signal: controller.signal,
      });
      // Drain so the socket can be reused/closed promptly.
      await res.arrayBuffer().catch(() => {});
      if (res.status !== 200 && res.status !== 206) {
        throw new Error(`stream URL rejected with HTTP ${res.status}`);
      }
    }
    finally {
      clearTimeout(timer);
    }
  }

  async #run(videoId) {
    const attempts = this.#attempts();
    let lastError = new Error('no resolution method configured');
    for (const [ i, attempt ] of attempts.entries()) {
      try {
        const url = await this.#runOnce(videoId, attempt);
        await this.#verify(url);
        this.#store(videoId, url);
        return url;
      }
      catch (e) {
        lastError = e;
        const more = i < attempts.length - 1;
        this.#logger.warn(
          `[resolver] ${attempt.label} failed for ${videoId}: ${e.message}`
          + `${more ? ` — falling back to ${attempts[i + 1].label}` : ''}`);
      }
    }
    throw lastError;
  }

  #runOnce(videoId, attempt) {
    // `--` last, so a video id can never be read as an option.
    const args = [ ...attempt.args, '--', `https://www.youtube.com/watch?v=${videoId}` ];

    return new Promise((resolve, reject) => {
      const started = Date.now();
      const prefix = priorityPrefix();
      const [ cmd, ...cmdArgs ] = [ ...prefix, this.#ytdlp, ...args ];
      const proc = spawn(cmd, cmdArgs, { stdio: [ 'ignore', 'pipe', 'pipe' ] });
      if (!prefix.length) {
        // No wrappers on this box: still get the CPU half of it. Best-effort — a
        // failure here must not fail the resolve.
        try {
          os.setPriority(proc.pid, RESOLVE_NICE);
        }
        catch (e) {
          this.#logger.debug(`[resolver] could not renice ${proc.pid}: ${e.message}`);
        }
      }
      let out = '';
      let err = '';
      const timer = setTimeout(() => {
        proc.kill('SIGKILL');
        reject(new Error(`resolve timed out after ${RESOLVE_TIMEOUT_MS / 1000}s`));
      }, RESOLVE_TIMEOUT_MS);
      proc.stdout.on('data', (d) => {
        out += d;
      });
      proc.stderr.on('data', (d) => {
        err += d;
      });
      proc.on('error', (e) => {
        clearTimeout(timer);
        reject(e);
      });
      proc.on('close', (code) => {
        clearTimeout(timer);
        const url = out.split('\n').map((l) => l.trim()).find((l) => l.startsWith('http'));
        if (code !== 0 || !url) {
          reject(new Error(err.trim().split('\n').pop() || `yt-dlp exited ${code}`));
          return;
        }
        const seconds = ((Date.now() - started) / 1000).toFixed(1);
        this.#logger.info(`[resolver] resolved ${videoId} in ${seconds}s via ${attempt.label}`);
        resolve(url);
      });
    });
  }

  #store(videoId, url) {
    // YouTube stamps its own lifetime into the URL; honour it rather than guessing.
    const m = /[?&]expire=(\d+)/.exec(url);
    const expiresAt = m ? (Number(m[1]) * 1000) - EXPIRY_MARGIN_MS : null;
    this.#cache.set(videoId, { url, expiresAt });
    while (this.#cache.size > MAX_ENTRIES) {
      this.#cache.delete(this.#cache.keys().next().value);
    }
  }
}
