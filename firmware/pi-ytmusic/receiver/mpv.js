/**
 * Minimal mpv JSON-IPC client.
 *
 * mpv runs ONCE, long-lived, in `--idle` mode; every transport command is a
 * newline-delimited JSON message over its unix socket. Why one long-lived mpv
 * rather than one process per track:
 *
 *   - Process startup + `yt-dlp` extractor import costs *seconds* on a Pi Zero
 *     2 W; paying that per track would stall every transition.
 *   - The audio device stays open across tracks (mpv's gapless handling), so the
 *     RTP sink is not torn down and re-established between songs.
 *
 * Protocol: requests are `{command: [...], request_id: N}`; replies carry the
 * same `request_id` plus `error`/`data`; anything with an `event` key is an
 * asynchronous notification, re-emitted here under its own event name.
 */

import EventEmitter from 'events';
import net from 'net';
import { spawn } from 'child_process';
import fs from 'fs';
import acquireSingleton from './singleton.js';

/** How long to wait for mpv's IPC socket to appear after spawning it. */
const SOCKET_WAIT_MS = 15000;
/** How long to wait for a connect() to the IPC socket before calling it dead. */
const LIVENESS_TIMEOUT_MS = 1000;
/** Per-command reply timeout. Generous: a `loadfile` on a slow link can block. */
const COMMAND_TIMEOUT_MS = 20000;
/** Backoff before respawning an mpv that died unexpectedly. */
const RESPAWN_DELAY_MS = 2000;

export default class MpvClient extends EventEmitter {
  #binary;
  #args;
  #socketPath;
  #logger;
  #proc = null;
  #socket = null;
  #buffer = '';
  #nextRequestId = 1;
  #pending = new Map();
  #shuttingDown = false;
  /** Release function for the single-instance lock; held for this object's life. */
  #releaseSingleton = null;
  /** Whether `#socketPath` is ours, i.e. whether `stop()` may remove it. */
  #ownsSocketPath = false;
  /** Observed property id -> { name, onChange }, replayed after a respawn. */
  #observed = new Map();
  #nextObserveId = 1;

  constructor({ binary = 'mpv', args = [], socketPath, logger }) {
    super();
    this.#binary = binary;
    this.#args = args;
    this.#socketPath = socketPath;
    this.#logger = logger;
  }

  get running() {
    return !!this.#proc && this.#proc.exitCode === null && !!this.#socket;
  }

  /**
   * Swap in a real logger. The receiver's logger only exists after the receiver
   * is constructed, which is after this client — so construction takes a
   * placeholder and the caller upgrades it before `start()`.
   */
  setLogger(logger) {
    this.#logger = logger;
  }

  /** Spawn mpv and connect to its IPC socket. Resolves once commands can be sent. */
  async start() {
    // Before anything is spawned: a second mpv on this socket would also be a
    // second client on the same PipeWire sink, and the add-on would receive both
    // mixed into one RTP stream (singleton.js). Held for the life of this object,
    // so a respawn after mpv dies does not try to re-take a lock we still own.
    if (!this.#releaseSingleton) {
      this.#releaseSingleton = await acquireSingleton(`mpv:${this.#socketPath}`, this.#logger);
    }
    await this.#claimSocketPath();
    this.#ownsSocketPath = true;

    const args = [`--input-ipc-server=${this.#socketPath}`, ...this.#args];
    this.#logger.info(`[mpv] spawning: ${this.#binary} ${args.join(' ')}`);
    this.#proc = spawn(this.#binary, args, { stdio: ['ignore', 'pipe', 'pipe'] });

    // mpv's own diagnostics are the first place to look when playback fails
    // (yt-dlp errors surface here), so forward them into our log rather than
    // dropping them.
    const forward = (stream, level) => {
      stream.setEncoding('utf8');
      let partial = '';
      stream.on('data', (chunk) => {
        partial += chunk;
        const lines = partial.split('\n');
        partial = lines.pop() ?? '';
        for (const line of lines) {
          if (line.trim()) {
            this.#logger[level](`[mpv] ${line.trim()}`);
          }
        }
      });
    };
    forward(this.#proc.stdout, 'debug');
    forward(this.#proc.stderr, 'warn');

    this.#proc.on('exit', (code, signal) => {
      this.#logger.warn(`[mpv] exited (code=${code} signal=${signal})`);
      this.#teardownSocket();
      // Fail every in-flight command instead of leaving callers hanging.
      for (const [ , pending ] of this.#pending) {
        pending.reject(new Error('mpv exited'));
      }
      this.#pending.clear();
      if (!this.#shuttingDown) {
        // A dead player must not take the cast receiver down with it: the phone
        // stays connected and the next command has to find a working mpv.
        setTimeout(() => {
          if (!this.#shuttingDown) {
            this.start().catch((e) => this.#logger.error(`[mpv] respawn failed: ${e.message}`));
          }
        }, RESPAWN_DELAY_MS);
      }
    });

    await this.#connectWithRetry();
    this.#logger.info('[mpv] IPC connected');
    // A respawned mpv is a fresh process: anything observed before it died has to
    // be asked for again, or the metadata reporter goes silent after one crash.
    this.#replayObservations();
  }

  /**
   * Make `#socketPath` ours to bind, removing it only if it is genuinely stale.
   *
   * This replaces an unconditional `unlink()`, which threw away a free guarantee:
   * mpv's own bind of `--input-ipc-server` is a kernel-enforced mutex, so deleting
   * the socket first is what *allowed* a second mpv to take the path over while the
   * first was still playing into the sink. Worse, the first receiver's next
   * reconnect (after a respawn) would then land on the wrong mpv. A socket nobody
   * is listening on is stale and safe to remove; one that accepts a connection is
   * a live mpv and a hard stop.
   */
  async #claimSocketPath() {
    if (!fs.existsSync(this.#socketPath)) {
      return;
    }
    if (await this.#socketIsLive()) {
      throw new Error(
        `another mpv is already listening on ${this.#socketPath} — refusing to ` +
        'start a second player (both would mix into the same sink)');
    }
    this.#logger.info(`[mpv] removing stale IPC socket ${this.#socketPath}`);
    fs.unlinkSync(this.#socketPath);
  }

  /** Whether something is accepting connections on the IPC socket right now. */
  #socketIsLive() {
    return new Promise((resolve) => {
      const s = net.createConnection(this.#socketPath);
      const done = (live) => {
        clearTimeout(timer);
        s.removeAllListeners();
        s.destroy();
        resolve(live);
      };
      // A unix-socket connect is immediate or it fails; a timeout means neither
      // happened, which is not something to delete on a guess.
      const timer = setTimeout(() => done(true), LIVENESS_TIMEOUT_MS);
      s.once('connect', () => done(true));
      s.once('error', () => done(false));
    });
  }

  async #connectWithRetry() {
    const deadline = Date.now() + SOCKET_WAIT_MS;
    for (;;) {
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
          throw new Error(`mpv IPC socket ${this.#socketPath} did not appear: ${e.message}`);
        }
        await new Promise((r) => setTimeout(r, 250));
      }
    }
    this.#socket.setEncoding('utf8');
    this.#socket.on('data', (chunk) => this.#onData(chunk));
    this.#socket.on('error', (e) => this.#logger.warn(`[mpv] socket error: ${e.message}`));
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
        this.#logger.debug(`[mpv] unparseable IPC line: ${line}`);
        continue;
      }
      if (msg.event) {
        if (msg.event === 'property-change') {
          const observation = this.#observed.get(msg.id);
          if (observation) {
            try {
              observation.onChange(msg.data);
            }
            catch (e) {
              this.#logger.warn(`[mpv] observer for ${observation.name} threw: ${e.message}`);
            }
          }
        }
        this.emit('event', msg);
        this.emit(msg.event, msg);
        continue;
      }
      const pending = this.#pending.get(msg.request_id);
      if (!pending) {
        continue;
      }
      this.#pending.delete(msg.request_id);
      clearTimeout(pending.timer);
      if (msg.error && msg.error !== 'success') {
        pending.reject(new Error(msg.error));
      }
      else {
        pending.resolve(msg.data);
      }
    }
  }

  /** Send a command, resolving with its `data`. Rejects on mpv-reported errors. */
  command(...args) {
    if (!this.#socket) {
      return Promise.reject(new Error('mpv IPC not connected'));
    }
    const request_id = this.#nextRequestId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(request_id);
        reject(new Error(`mpv command timed out: ${JSON.stringify(args)}`));
      }, COMMAND_TIMEOUT_MS);
      this.#pending.set(request_id, { resolve, reject, timer });
      this.#socket.write(`${JSON.stringify({ command: args, request_id })}\n`);
    });
  }

  /**
   * Watch a property and call `onChange(value)` whenever mpv reports a new one.
   *
   * Observing rather than polling matters for the ones that arrive *late*:
   * `media-title` only exists once `ytdl_hook` has resolved the URL, seconds after
   * `loadfile` on this hardware, so a poll right after loading finds nothing.
   *
   * Registrations are remembered and re-issued after a respawn — an mpv that died
   * and came back is a fresh process that knows nothing about them.
   */
  observeProperty(name, onChange) {
    const id = this.#nextObserveId++;
    this.#observed.set(id, { name, onChange });
    if (this.running) {
      void this.command('observe_property', id, name).catch((e) =>
        this.#logger.warn(`[mpv] observe_property ${name}: ${e.message}`));
    }
    return id;
  }

  /** Re-issue every observation on a fresh mpv (called once the IPC is up). */
  #replayObservations() {
    for (const [ id, { name } ] of this.#observed) {
      void this.command('observe_property', id, name).catch((e) =>
        this.#logger.warn(`[mpv] observe_property ${name}: ${e.message}`));
    }
  }

  async getProperty(name, fallback = null) {
    try {
      const v = await this.command('get_property', name);
      return v ?? fallback;
    }
    catch (e) {
      // Absolutely routine: `time-pos`/`duration` have no value while idle.
      this.#logger.debug(`[mpv] get_property ${name}: ${e.message}`);
      return fallback;
    }
  }

  async setProperty(name, value) {
    try {
      await this.command('set_property', name, value);
      return true;
    }
    catch (e) {
      this.#logger.warn(`[mpv] set_property ${name}=${value}: ${e.message}`);
      return false;
    }
  }

  /** Stop mpv for good (no respawn). */
  async stop() {
    this.#shuttingDown = true;
    this.#teardownSocket();
    if (this.#proc && this.#proc.exitCode === null) {
      this.#proc.kill('SIGTERM');
    }
    // Only if the path is ours. A start() that was refused — because another mpv
    // holds the lock or the socket — must not delete the *healthy* instance's
    // socket on its way out, which would leave that receiver unable to reconnect
    // after its next respawn.
    if (this.#ownsSocketPath) {
      try {
        fs.unlinkSync(this.#socketPath);
      }
      catch { /* already gone */ }
      this.#ownsSocketPath = false;
    }
    // Released explicitly so a caller that stops us and carries on (a test, or a
    // failed startup that exits later) does not keep the next instance locked out.
    // Process death releases it anyway — that is the point of an abstract socket.
    this.#releaseSingleton?.();
    this.#releaseSingleton = null;
  }
}
