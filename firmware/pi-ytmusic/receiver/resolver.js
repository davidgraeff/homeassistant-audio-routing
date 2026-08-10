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
 * Resolution itself is *also* made faster now, by not paying that process-local cost
 * per track: `ytdlp.js` keeps one long-lived yt-dlp process whose caches survive
 * across tracks, and this class asks it rather than spawning. The per-track spawn
 * survives as the fallback for when there is no usable daemon — see `#runOnce`.
 *
 * WHY NOT mpv's OWN PLAYLIST PREFETCH
 * -----------------------------------
 * Appending upcoming tracks to mpv's playlist would let mpv advance on its own,
 * and the cast session's queue bookkeeping (yt-cast-receiver's `Playlist`, which
 * the phone's UI mirrors) would drift out of sync with what is actually playing.
 * The queue stays authoritative; only the *resolution* is pre-done.
 */

import { spawn } from 'child_process';
import os from 'os';
import { RESOLVE_NICE, priorityPrefix } from './priority.js';

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
/** Pause before verify's single retry. Long enough for a transient rejection to pass,
 * short enough to stay inside a track change. */
const VERIFY_RETRY_DELAY_MS = 1500;
/** Cache bound. The queue only ever needs the next one or two. */
const MAX_ENTRIES = 8;
/**
 * Extra attempts that differ only in which InnerTube client yt-dlp asks.
 *
 * Ad enforcement is reported **per client** — the verbose log carries lines like
 * `Detected a 15s ad skippable after 5s for web_safari` — and every URL that has been
 * rejected with 403 belonged to an ad-carrying video, while neither zero-ad video ever
 * failed. So when one client's URL will not play, the cheapest thing to try is the same
 * video through a different client: no extra credentials, no extra risk, and the attempt
 * ladder in `#run` already gates each one with `#verify`.
 *
 * Ordered by **format**, then by whether it plays at all. Measured on the add-on *with the
 * cookie jar*, which matters — an anonymous sweep ranked `android_vr` highly and it turns out
 * to offer no formats at all once authenticated:
 *
 *     (default)      251 | opus | 136 kbps     <- audio-only, 48 kHz, resample-free
 *     web_embedded   251 | opus | 136 kbps     <- same format, different client
 *     web_safari      96 | mp4a | HLS
 *     mweb            18 | mp4a | muxed 360p   <- video bytes fetched and discarded
 *     web_music       18 | mp4a | muxed 360p
 *     android_vr      no formats (authenticated)
 *
 * So the two opus rungs come first and the lossy-but-working ones behind them: a worse
 * format that plays still beats a perfect one that 403s, but there is no reason to reach for
 * it before trying the other client that serves opus.
 *
 * Only reached when the attempt before it failed, and each mode's `YoutubeDL` is built
 * lazily, so an unused rung costs nothing.
 */
const DEFAULT_CLIENT_LADDER = [ 'web_embedded', 'web_safari', 'mweb' ];

export default class Resolver {
  #ytdlp;
  #cookies;
  #jsRuntime;
  #format;
  #cipherUrl;
  #cipherTimeout;
  #potUrl;
  #clientLadder;
  #cacheDir;
  #logger;
  /** Long-lived yt-dlp (ytdlp.js), or null to spawn one per track. */
  #daemon = null;
  /** videoId -> { url, headers, expiresAt } — headers are part of the answer, see #verify */
  #cache = new Map();
  /** videoId -> Promise, so a play racing its own prefetch waits instead of duplicating. */
  #inflight = new Map();

  constructor({ ytdlp, cookies = null, jsRuntime = null, format = 'bestaudio/best',
    cipherUrl = null, cipherTimeout = 8, potUrl = null, cacheDir = null,
    clientLadder = DEFAULT_CLIENT_LADDER, logger = console }) {
    this.#ytdlp = ytdlp;
    this.#cookies = cookies;
    this.#jsRuntime = jsRuntime;
    this.#format = format;
    this.#cipherUrl = cipherUrl;
    this.#cipherTimeout = cipherTimeout;
    this.#potUrl = potUrl;
    this.#clientLadder = clientLadder;
    this.#cacheDir = cacheDir;
    this.#logger = logger;
  }

  /** Swap in the receiver's logger once it exists (same pattern as mpv.js). */
  setLogger(logger) {
    this.#logger = logger;
  }

  /**
   * Attach the long-lived resolver, after it has been started.
   *
   * Set rather than constructed here because the daemon needs `daemonModes` — this
   * class stays the single source of truth for what a resolve's arguments are, so
   * the daemon and the fallback spawn can never drift apart.
   */
  setDaemon(daemon) {
    this.#daemon = daemon;
  }

  /**
   * The resolve strategies as `{ mode: argv }`, for `YtdlpDaemon`'s `--mode` options.
   * Same arguments the fallback spawn would use, minus the video URL.
   */
  get daemonModes() {
    return Object.fromEntries(this.#attempts().map((a) => [ a.mode, a.args ]));
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
    return { url: hit.url, headers: hit.headers };
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
    // `--print` rather than `-g`: the fallback spawn has to report the stream's
    // **headers** as well as its URL, because mpv is handed the URL directly and
    // `ytdl_hook` — which would otherwise supply them — is bypassed.
    const base = [ '--no-warnings', '--no-playlist', '-f', this.#format,
      '--print', '%(url)s', '--print', '%(http_headers)j' ];
    if (this.#cookies) {
      base.push('--cookies', this.#cookies);
    }
    if (this.#cacheDir) {
      // yt-dlp's own on-disk cache, pinned somewhere persistent. It holds the extracted
      // signature functions and the downloaded ejs challenge-solver scripts — the one part
      // of a cold resolve that survives a restart — and its default (`~/.cache/yt-dlp`) is
      // neither persistent nor reliably writable in the add-on's container.
      base.push('--cache-dir', this.#cacheDir);
    }
    if (this.#potUrl) {
      base.push('--extractor-args', `youtubepot-bgutilhttp:base_url=${this.#potUrl}`);
    }

    // EVERY `youtube:`-prefixed setting has to go in ONE `--extractor-args`, because a
    // second occurrence of the same prefix **replaces** the first rather than merging:
    //
    //   --extractor-args youtube:fetch_pot=never --extractor-args youtube:player_client=X
    //     -> {'player_client': ['X']}          … fetch_pot silently gone
    //   --extractor-args 'youtube:fetch_pot=never;player_client=X'
    //     -> {'fetch_pot': ['never'], 'player_client': ['X']}
    //
    // Found in a live log: every client-ladder rung had lost `fetch_pot=never` and was
    // warning about the unreachable PO-token server on each resolve. Other prefixes
    // (`youtubejsc-*`, `youtubepot-*`) are separate keys and safe as separate options.
    const youtubeArgs = [];
    if (!this.#potUrl) {
      // No provider configured, so switch PO-token fetching off explicitly. Left to its
      // default, the installed plugin probes http://127.0.0.1:4416 and warns per resolve.
      youtubeArgs.push('fetch_pot=never');
    }
    const withYoutubeArgs = (extra = []) => {
      const merged = [ ...youtubeArgs, ...extra ];
      return merged.length ? [ '--extractor-args', `youtube:${merged.join(';')}` ] : [];
    };

    const remote = this.#cipherUrl
      ? [ {
        mode: 'remote',
        label: 'remote cipher',
        // `--no-js-runtimes` states what this attempt means. yt-dlp enables `deno` by
        // default, so on a box that has one, a builtin runtime would outrank the plugin and
        // solve locally while the log still claimed the server was in use.
        args: [ ...base, '--no-js-runtimes', '--extractor-args',
          `youtubejsc-remotecipher:base_url=${this.#cipherUrl};timeout=${this.#cipherTimeout}`,
          ...withYoutubeArgs() ],
      } ]
      : [];
    const local = this.#jsRuntime
      ? [ {
        mode: 'local',
        label: `local ${this.#jsRuntime.split(':')[0]}`,
        args: [ ...base, '--js-runtimes', this.#jsRuntime, ...withYoutubeArgs() ],
      } ]
      : [];
    // Same solver, same cookies — only the client differs.
    const ladder = this.#jsRuntime
      ? this.#clientLadder.map((client) => ({
        mode: `client-${client}`,
        label: `client ${client}`,
        args: [ ...base, '--js-runtimes', this.#jsRuntime,
          ...withYoutubeArgs([ `player_client=${client}` ]) ],
      }))
      : [];
    return [ ...remote, ...local, ...ladder ];
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
  async #verify(url, headers) {
    // Retried once, because a 403 here is not reliably a verdict on the URL. Measured on
    // the add-on: a burst of rejections hit every path at once — remote cipher, resident
    // worker, a fresh process, mpv's own resolution — and minutes later the same video
    // verified fine everywhere, from the same cache and the same jar. That is YouTube
    // rejecting requests, not a bad answer, and treating it as a verdict turned one
    // track change into seven resolves, which is the wrong direction to push.
    try {
      await this.#verifyOnce(url, headers);
      return;
    }
    catch (e) {
      // A 403 is a verdict, not a hiccup: retrying it cost 1.7 s of every rescued track
      // (measured — the retry never once turned into a 200), and there is now somewhere
      // better to spend that time, namely the next client on the ladder. Anything else —
      // a timeout, a reset, a 5xx — is worth one retry.
      if (e.status && e.status >= 400 && e.status < 500) {
        throw e;
      }
      this.#logger.debug(`[resolver] verify failed (${e.message}) — one retry in ${VERIFY_RETRY_DELAY_MS}ms`);
    }
    await new Promise((r) => setTimeout(r, VERIFY_RETRY_DELAY_MS));
    await this.#verifyOnce(url, headers);
  }

  async #verifyOnce(url, headers) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), VERIFY_TIMEOUT_MS);
    try {
      const res = await fetch(url, {
        // yt-dlp's own headers, or the check lies. A bare request is answered **403**
        // by some googlevideo URLs while the identical request with the User-Agent
        // yt-dlp used gets 206 — so without these, this method rejects perfectly good
        // resolves, both attempts "fail", and playback falls through to mpv's slow
        // path. That was the whole of a reported failure on 2026-08-10.
        headers: { ...(headers ?? {}), Range: 'bytes=0-1' },
        signal: controller.signal,
      });
      // Drain so the socket can be reused/closed promptly.
      await res.arrayBuffer().catch(() => {});
      if (res.status !== 200 && res.status !== 206) {
        const e = new Error(`stream URL rejected with HTTP ${res.status}`);
        e.status = res.status;
        throw e;
      }
    }
    finally {
      clearTimeout(timer);
    }
  }

  async #run(videoId) {
    let lastError = new Error('no resolution method configured');
    let ladderExhausted = false;

    for (let pass = 0; pass < 2; pass++) {
      // Pass 0 walks the whole ladder — the primary client, then each alternate. Pass 1
      // retries the primary ONLY: by then the alternates have been tried and what changed
      // is the process, not the client.
      const attempts = pass === 0 ? this.#attempts() : this.#attempts().slice(0, 1);
      for (const [ i, attempt ] of attempts.entries()) {
        let resolved;
        try {
          resolved = await this.#runOnce(videoId, attempt);
        }
        catch (e) {
          lastError = e;
          const more = i < attempts.length - 1;
          this.#logger.warn(
            `[resolver] ${attempt.label} failed for ${videoId}: ${e.message}`
            + `${more ? ` — falling back to ${attempts[i + 1].label}` : ''}`);
          continue;
        }
        try {
          await this.#verify(resolved.url, resolved.headers);
        }
        catch (e) {
          lastError = e;
          ladderExhausted = true;
          this.#logger.warn(
            `[resolver] ${attempt.label} resolved ${videoId} but the URL did not work `
            + `(${e.message})`);
          continue;
        }
        this.#store(videoId, resolved);
        return resolved;
      }

      // Restart only once the WHOLE ladder has produced unplayable URLs. Escalating on the
      // first failure would kill the daemon mid-ladder and push the remaining rungs onto
      // the per-track spawn path, which re-pays the entire JS challenge each time.
      if (pass > 0 || !ladderExhausted || !this.#daemon?.running) {
        break;
      }
      this.#logger.warn(
        `[resolver] every attempt for ${videoId} produced a URL that would not play — `
        + 'restarting the resolver and trying once more');
      this.#daemon.invalidate(null, lastError.message, videoId, { escalate: true });
      await this.#daemon.waitUntilRunning();
    }

    // Nothing worked. Record why *now*: these rejections come in windows that cannot be
    // reconstructed afterwards, and every post-mortem without a snapshot taken during one
    // has been guesswork.
    this.#daemon?.diagnose(videoId, lastError?.message ?? 'unknown');
    throw lastError;
  }

  /**
   * One attempt, through the long-lived daemon when there is one and a fresh
   * `yt-dlp` otherwise.
   *
   * A daemon *transport* failure falls through to the spawn: a dead or wedged
   * resolver must not become a dead player, and `ytdlp.js` will respawn it
   * meanwhile. A resolve that genuinely failed (no formats, stale cookies, a wrong
   * `n`) is re-thrown instead — spawning would spend another ~20 s reaching the same
   * answer, and `#run` already has a better move: the next attempt.
   */
  async #runOnce(videoId, attempt) {
    if (this.#daemon?.running) {
      try {
        const reply = await this.#daemon.resolve(videoId, attempt.mode, RESOLVE_TIMEOUT_MS);
        this.#logger.info(
          `[resolver] resolved ${videoId} in ${(reply.elapsed_ms / 1000).toFixed(1)}s `
          + `via ${attempt.label} (daemon${reply.cold ? ', cold' : ''})`);
        return { url: reply.url, headers: reply.headers ?? {} };
      }
      catch (e) {
        if (!e.transport) {
          throw e;
        }
        this.#logger.warn(
          `[resolver] daemon unusable (${e.message}) — spawning yt-dlp for ${videoId}`);
      }
    }
    return this.#spawnOnce(videoId, attempt);
  }

  #spawnOnce(videoId, attempt) {
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
        // Two `--print` templates, in the order `#attempts` passes them: the URL, then
        // the headers as JSON. Matched by shape rather than by line number, so extra
        // chatter on stdout cannot shift the parse.
        const lines = out.split('\n').map((l) => l.trim()).filter(Boolean);
        const url = lines.find((l) => l.startsWith('http'));
        if (code !== 0 || !url) {
          reject(new Error(err.trim().split('\n').pop() || `yt-dlp exited ${code}`));
          return;
        }
        let headers = {};
        const json = lines.find((l) => l.startsWith('{'));
        if (json) {
          try {
            headers = JSON.parse(json);
          }
          catch {
            this.#logger.debug('[resolver] could not parse the stream headers from yt-dlp');
          }
        }
        const seconds = ((Date.now() - started) / 1000).toFixed(1);
        this.#logger.info(`[resolver] resolved ${videoId} in ${seconds}s via ${attempt.label}`);
        resolve({ url, headers });
      });
    });
  }

  #store(videoId, { url, headers }) {
    // YouTube stamps its own lifetime into the URL; honour it rather than guessing.
    const m = /[?&]expire=(\d+)/.exec(url);
    const expiresAt = m ? (Number(m[1]) * 1000) - EXPIRY_MARGIN_MS : null;
    this.#cache.set(videoId, { url, headers, expiresAt });
    while (this.#cache.size > MAX_ENTRIES) {
      this.#cache.delete(this.#cache.keys().next().value);
    }
  }
}
