/**
 * Client for the long-lived yt-dlp resolver (`ytdlp_daemon.py`).
 *
 * WHY A DAEMON AT ALL
 * -------------------
 * See the header of `ytdlp_daemon.py`: everything expensive about an authenticated
 * resolve is process-local state in yt-dlp (the Python import, the downloaded player
 * JS, derived player data, PO tokens), so one process per track paid for all of it
 * every time. This keeps one process and asks it per track.
 *
 * Same shape as `mpv.js` on purpose — spawn, connect to a unix socket, correlate
 * newline-delimited JSON by `request_id`, forward the child's stderr into our log,
 * respawn if it dies — because it is the same problem and the operational behaviour
 * should be the same. Two differences worth knowing:
 *
 *   - It is **optional**. `resolver.js` falls back to spawning `yt-dlp` per track
 *     when there is no daemon, so a Python that cannot import `yt_dlp` (or an
 *     install where the venv layout differs) degrades to the old behaviour instead
 *     of breaking playback.
 *   - It is spawned through `nice`/`ionice` (`priority.js`), which is also how the
 *     JS runtime it spawns for the `n` challenge inherits the low priority. Being
 *     long-lived does not change that requirement: the CPU burst is per resolve, not
 *     per process.
 */

import EventEmitter from 'events';
import net from 'net';
import { spawn } from 'child_process';
import fs from 'fs';
import os from 'os';
import { fileURLToPath } from 'url';
import { RESOLVE_NICE, priorityPrefix } from './priority.js';

/** The daemon script, resolved next to this module so both deployments find it. */
const SCRIPT_PATH = fileURLToPath(new URL('./ytdlp_daemon.py', import.meta.url));
/** How long to wait for the daemon's socket to appear after spawning it. */
const SOCKET_WAIT_MS = 20000;
/** Backoff before respawning a daemon that died unexpectedly. */
const RESPAWN_DELAY_MS = 3000;
/** Backoff when *we* asked it to exit: somebody is waiting on the next resolve. */
const ESCALATED_RESPAWN_DELAY_MS = 250;
/** How long `waitUntilRunning` will wait for a respawn to finish. */
const RESPAWN_WAIT_MS = 15000;
/** Give up on a daemon that keeps dying, and let the caller spawn per track instead. */
const MAX_RESPAWNS = 5;

export default class YtdlpDaemon extends EventEmitter {
  #python;
  #socketPath;
  /** label -> argv, the resolve strategies the daemon is started with. */
  #modes;
  #prewarm;
  #logger;
  #proc = null;
  #socket = null;
  #buffer = '';
  #nextId = 1;
  #pending = new Map();
  #shuttingDown = false;
  #respawns = 0;
  #giveUp = false;
  #started = false;
  /** Set while an exit we asked for is in flight, so the respawn is not delayed. */
  #escalating = false;
  /**
   * Set from the moment a restart is requested until the replacement is ready.
   *
   * Distinct from `#escalating`, which the exit handler clears: without it,
   * `waitUntilRunning` returned *immediately* after an escalation, because `running` was
   * still true — the child had not exited yet — and the caller then fell through to a
   * per-track spawn, paying the full JS challenge it was trying to avoid.
   */
  #restartPending = false;

  #jscResident;
  #jscPreprocessedCache;

  constructor({ python, socketPath, modes, prewarm = true, jscResident = null,
    jscPreprocessedCache = true, logger = console }) {
    super();
    this.#python = python;
    this.#socketPath = socketPath;
    this.#modes = modes;
    this.#prewarm = prewarm;
    this.#jscResident = jscResident;
    this.#jscPreprocessedCache = jscPreprocessedCache;
    this.#logger = logger;
  }

  setLogger(logger) {
    this.#logger = logger;
  }

  /** Whether requests can be sent right now. */
  get running() {
    return !!this.#proc && this.#proc.exitCode === null && !!this.#socket;
  }

  /** Whether this daemon has failed often enough that callers should stop waiting for it. */
  get abandoned() {
    return this.#giveUp;
  }

  /** The labels the daemon knows, so `resolver.js` can only ask for modes it has. */
  get modes() {
    return Object.keys(this.#modes);
  }

  /**
   * Spawn the daemon and connect. Rejects if it cannot be started, which the caller
   * is expected to treat as "carry on without one" rather than as fatal.
   */
  async start() {
    if (!fs.existsSync(SCRIPT_PATH)) {
      throw new Error(`resolver daemon script missing: ${SCRIPT_PATH}`);
    }
    await this.#claimSocketPath();

    const args = [ SCRIPT_PATH, '--socket', this.#socketPath ];
    for (const [ label, argv ] of Object.entries(this.#modes)) {
      // One string per mode: the label, then the yt-dlp command line it stands for.
      // The daemon splits it with shlex, so quote anything with a space.
      args.push('--mode', `${label}=${argv.map(quoteArg).join(' ')}`);
    }
    if (this.#prewarm && !this.#started) {
      // First start only. A respawn happens *during* a session — often because something
      // was already failing — and a prewarm then adds another full resolve to whatever
      // is going wrong. Startup is the only moment when it is free.
      args.push('--prewarm');
    }
    this.#started = true;
    if (this.#jscResident) {
      args.push('--jsc-resident', this.#jscResident);
    }
    if (!this.#jscPreprocessedCache) {
      args.push('--no-jsc-preprocessed-cache');
    }

    const prefix = priorityPrefix();
    const [ cmd, ...cmdArgs ] = [ ...prefix, this.#python, ...args ];
    this.#logger.info(`[ytdlpd] spawning: ${this.#python} ${args.join(' ')}`);
    this.#proc = spawn(cmd, cmdArgs, { stdio: [ 'ignore', 'pipe', 'pipe' ] });
    if (!prefix.length) {
      try {
        os.setPriority(this.#proc.pid, RESOLVE_NICE);
      }
      catch (e) {
        this.#logger.debug(`[ytdlpd] could not renice ${this.#proc.pid}: ${e.message}`);
      }
    }

    // The daemon's own diagnostics include yt-dlp's, which is where a resolution
    // failure explains itself. Dropping them would make a broken cookie jar look
    // like a hang, exactly as documented for mpv's stderr in mpv.js.
    forwardLines(this.#proc.stdout, (line) => this.#logger.debug(`[ytdlpd] ${line}`));
    // The daemon tags every line it writes itself. Anything untagged reached fd 2
    // some other way — most plausibly a process yt-dlp spawned (the JS runtime for
    // the `n` challenge) inheriting this stderr, or a Python traceback. Tag it as
    // raw rather than passing it through: an unattributable line in the journal is
    // how an nginx 504 error page once turned up with no indication of who logged it.
    forwardLines(this.#proc.stderr, (line) =>
      this.#logger.info(line.startsWith('[ytdlpd]') ? line : `[ytdlpd:raw] ${line}`));

    this.#proc.on('exit', (code, signal) => {
      this.#teardownSocket();
      for (const [ , pending ] of this.#pending) {
        pending.reject(transportError('resolver daemon exited'));
      }
      this.#pending.clear();
      if (this.#shuttingDown) {
        return;
      }
      this.#respawns += 1;
      if (this.#respawns > MAX_RESPAWNS) {
        this.#giveUp = true;
        this.#logger.error(
          `[ytdlpd] exited (code=${code} signal=${signal}) ${this.#respawns} times — `
          + 'giving up on the daemon; resolves will spawn yt-dlp per track');
        return;
      }
      const delay = this.#escalating ? ESCALATED_RESPAWN_DELAY_MS : RESPAWN_DELAY_MS;
      this.#escalating = false;
      this.#logger.warn(
        `[ytdlpd] exited (code=${code} signal=${signal}) — respawning in ${delay / 1000}s`);
      setTimeout(() => {
        if (!this.#shuttingDown) {
          this.start().catch((e) => this.#logger.error(`[ytdlpd] respawn failed: ${e.message}`));
        }
      }, delay);
    });

    await this.#connectWithRetry();
    const hello = await this.request({ op: 'ping' }, 10000);
    this.#restartPending = false;
    this.#logger.info(
      `[ytdlpd] ready — yt-dlp ${hello.version}, modes: ${(hello.modes ?? []).join(', ')}`);
  }

  /**
   * Same stale-vs-live distinction as `MpvClient#claimSocketPath`: a bind is a
   * kernel-enforced mutex, so unlinking unconditionally is what would let two
   * daemons share a path. The daemon checks this too — this side exists so the
   * failure is reported by whoever can log it usefully.
   */
  async #claimSocketPath() {
    if (!fs.existsSync(this.#socketPath)) {
      return;
    }
    const live = await new Promise((resolve) => {
      const s = net.createConnection(this.#socketPath);
      const done = (value) => {
        clearTimeout(timer);
        s.removeAllListeners();
        s.destroy();
        resolve(value);
      };
      const timer = setTimeout(() => done(true), 1000);
      s.once('connect', () => done(true));
      s.once('error', () => done(false));
    });
    if (live) {
      throw new Error(`another resolver daemon is listening on ${this.#socketPath}`);
    }
    this.#logger.info(`[ytdlpd] removing stale socket ${this.#socketPath}`);
    fs.unlinkSync(this.#socketPath);
  }

  async #connectWithRetry() {
    const deadline = Date.now() + SOCKET_WAIT_MS;
    for (;;) {
      if (this.#proc?.exitCode !== null) {
        throw new Error(`resolver daemon exited before it listened (code=${this.#proc?.exitCode})`);
      }
      try {
        this.#socket = await new Promise((resolve, reject) => {
          const s = net.createConnection(this.#socketPath);
          s.once('connect', () => resolve(s));
          s.once('error', reject);
        });
        break;
      }
      catch (e) {
        if (Date.now() >= deadline) {
          throw new Error(`resolver daemon socket ${this.#socketPath} did not appear: ${e.message}`);
        }
        await new Promise((r) => setTimeout(r, 250));
      }
    }
    this.#socket.setEncoding('utf8');
    this.#socket.on('data', (chunk) => this.#onData(chunk));
    this.#socket.on('error', (e) => this.#logger.warn(`[ytdlpd] socket error: ${e.message}`));
    this.#socket.on('close', () => this.#teardownSocket());
  }

  #teardownSocket() {
    if (this.#socket) {
      this.#socket.removeAllListeners();
      this.#socket.destroy();
      this.#socket = null;
    }
    this.#buffer = '';
  }

  #onData(chunk) {
    this.#buffer += chunk;
    const lines = this.#buffer.split('\n');
    this.#buffer = lines.pop() ?? '';
    for (const line of lines) {
      if (!line.trim()) {
        continue;
      }
      let msg;
      try {
        msg = JSON.parse(line);
      }
      catch {
        this.#logger.debug(`[ytdlpd] unparseable reply: ${line.slice(0, 200)}`);
        continue;
      }
      const pending = this.#pending.get(msg.id);
      if (!pending) {
        continue;
      }
      this.#pending.delete(msg.id);
      clearTimeout(pending.timer);
      if (msg.error) {
        pending.reject(new Error(msg.error));
      }
      else {
        pending.resolve(msg);
      }
    }
  }

  /** Send one request, resolving with the whole reply object. */
  request(payload, timeoutMs) {
    if (!this.#socket) {
      return Promise.reject(transportError('resolver daemon not connected'));
    }
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(transportError(`resolver daemon timed out after ${timeoutMs / 1000}s`));
      }, timeoutMs);
      this.#pending.set(id, { resolve, reject, timer });
      this.#socket.write(`${JSON.stringify({ id, ...payload })}\n`);
    });
  }

  /**
   * Resolve `videoId` using the strategy registered under `mode`.
   *
   * @returns {Promise<{url: string, elapsed_ms: number, cold: boolean}>}
   */
  resolve(videoId, mode, timeoutMs) {
    return this.request({ op: 'resolve', video_id: videoId, mode }, timeoutMs);
  }

  /**
   * Tell the daemon a URL it produced did not work, so it drops the session state
   * behind that mode. Fire-and-forget: this is a hint on a failure path and must never
   * add a second failure of its own.
   */
  invalidate(mode, reason, videoId, { escalate = false } = {}) {
    if (!this.#socket) {
      return;
    }
    if (escalate) {
      // The exit is expected, so respawn at once rather than after the crash backoff:
      // a resolve is waiting on it.
      this.#escalating = true;
      this.#restartPending = true;
    }
    this.request({ op: 'invalidate', mode, reason, video_id: videoId, escalate }, 10000)
      .catch((e) => this.#logger.debug(`[ytdlpd] invalidate ${mode}: ${e.message}`));
  }

  /**
   * Ask for fresh session state before it is needed.
   *
   * yt-dlp's session state goes stale in minutes and the symptom is a URL that resolves
   * and then 403s — which costs a whole extra resolve on the play path. A sender
   * connecting is the one moment when that second is free: the user is still choosing
   * what to play.
   */
  refresh(reason) {
    if (!this.#socket) {
      return;
    }
    this.request({ op: 'invalidate', reason, proactive: true }, 10000)
      .catch((e) => this.#logger.debug(`[ytdlpd] refresh: ${e.message}`));
  }

  /**
   * Ask the daemon to snapshot a 403 while it is still happening.
   *
   * Fire-and-forget with a long timeout: it runs several resolves, so it must never sit
   * on the play path. The daemon throttles it.
   */
  diagnose(videoId, reason) {
    if (!this.#socket) {
      return;
    }
    this.request({ op: 'diagnose', video_id: videoId, reason }, 600000)
      .then((r) => {
        if (r?.report) {
          this.#logger.warn(`[ytdlpd] 403 diagnosed — report at ${r.report}`);
        }
      })
      .catch((e) => this.#logger.debug(`[ytdlpd] diagnose: ${e.message}`));
  }

  /**
   * Resolve once the daemon is answering again, or after RESPAWN_WAIT_MS.
   *
   * Used by `resolver.js` between attempts: a restart is the only recovery measured to
   * work, and waiting ~4 s for it beats the alternative of falling back to a per-track
   * `yt-dlp` spawn, which pays the full 15 s JS challenge with no resident worker.
   */
  async waitUntilRunning() {
    const deadline = Date.now() + RESPAWN_WAIT_MS;
    while ((this.#restartPending || !this.running)
      && !this.#giveUp && !this.#shuttingDown && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 200));
    }
    return this.running;
  }

  /** Stop the daemon for good (no respawn). */
  async stop() {
    this.#shuttingDown = true;
    this.#teardownSocket();
    if (this.#proc && this.#proc.exitCode === null) {
      // SIGTERM, not SIGKILL: the daemon flushes rotated cookies back to the jar on
      // the way out (see COOKIE_SAVE_INTERVAL_S), and losing that quietly ages the
      // credential every restart.
      this.#proc.kill('SIGTERM');
    }
  }
}

/**
 * A failure of the *channel*, not of the resolve.
 *
 * The distinction is load-bearing in `resolver.js`: a dead or wedged daemon should
 * be retried by spawning `yt-dlp` per track, while a resolve that genuinely failed
 * (no formats, bad cookies, a wrong `n`) must NOT be — spawning would take another
 * 20 s to reach the same answer, mid-song.
 */
function transportError(message) {
  const e = new Error(message);
  e.transport = true;
  return e;
}

/** Shell-quote an argv element for the daemon's `shlex.split`. */
function quoteArg(arg) {
  return /^[\w@%+=:,./-]+$/.test(arg) ? arg : `'${String(arg).replace(/'/g, `'\\''`)}'`;
}

/** Forward a child stream line by line, so partial chunks never split a message. */
function forwardLines(stream, emit) {
  stream.setEncoding('utf8');
  let partial = '';
  stream.on('data', (chunk) => {
    partial += chunk;
    const lines = partial.split('\n');
    partial = lines.pop() ?? '';
    for (const line of lines) {
      if (line.trim()) {
        emit(line.trim());
      }
    }
  });
}
