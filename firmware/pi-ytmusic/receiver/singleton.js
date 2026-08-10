/**
 * Single-instance guard.
 *
 * WHY THIS EXISTS
 * ---------------
 * Two mpv processes playing into `ytm-out` do not conflict in any way PipeWire
 * reports — a sink mixes its clients, so the add-on receives one RTP stream
 * carrying both, which sounds like stutter or garble and looks like a bug in the
 * audio path. It happened for real: a dev harness importing `mpv.js` ran
 * alongside `ytmusic-receiver.service`, and both had an mpv on the sink.
 *
 * The DIAL port bind in `index.js` already rejects a duplicate *receiver*, but
 * only after mpv has been spawned (and anything importing `mpv.js` directly never
 * reaches it). So the guard belongs here, ahead of the spawn, where every caller
 * passes.
 *
 * WHY AN ABSTRACT UNIX SOCKET
 * ---------------------------
 * A Linux abstract-namespace socket (a name starting with NUL, outside the
 * filesystem) is the one lock primitive that cannot go stale: the kernel owns the
 * name and drops it when the last holder's fd closes, including on SIGKILL. A
 * pidfile has to be validated and can name a recycled pid; a filesystem socket or
 * lockfile survives a crash and needs the very "is it stale?" dance this exists to
 * avoid. Node also has no `flock`.
 *
 * The holder answers a connection with its own pid, so the loser can say *who*
 * holds the lock rather than only that someone does.
 */

import net from 'net';
import { createHash } from 'crypto';

/** How long to wait for the holder to identify itself before giving up on a name. */
const PID_QUERY_TIMEOUT_MS = 500;

/**
 * Thrown when the lock is held. Distinct from a generic failure because it is an
 * operator mistake, not a crash — the caller can report it in one line instead of
 * a stack trace.
 */
export class AlreadyRunningError extends Error {
  constructor(message, holderPid = null) {
    super(message);
    this.name = 'AlreadyRunningError';
    this.holderPid = holderPid;
  }
}

/**
 * Abstract socket names are capped (~108 bytes) and the scope is a filesystem
 * path, so hash it rather than truncating — two sockets in the same directory
 * must not collapse onto one lock.
 */
function addressFor(scope) {
  return `\0pi-ytmusic.${createHash('sha1').update(scope).digest('hex').slice(0, 16)}`;
}

/** Ask whoever holds `address` for their pid. Null if they do not answer in time. */
function queryHolder(address) {
  return new Promise((resolve) => {
    const s = net.createConnection(address);
    let data = '';
    const done = (value) => {
      s.removeAllListeners();
      s.destroy();
      resolve(value);
    };
    const timer = setTimeout(() => done(null), PID_QUERY_TIMEOUT_MS);
    s.setEncoding('utf8');
    s.on('data', (chunk) => {
      data += chunk;
    });
    s.once('close', () => {
      clearTimeout(timer);
      const pid = Number.parseInt(data.trim(), 10);
      done(Number.isFinite(pid) && pid > 0 ? pid : null);
    });
    s.once('error', () => {
      clearTimeout(timer);
      done(null);
    });
  });
}

/**
 * Take the lock for `scope`, resolving with a `release()` function.
 *
 * @param {string} scope Identifies the resource being protected — pass the thing
 *   that actually collides (the mpv IPC socket path), so two deliberately
 *   separate instances with their own sockets and sinks can coexist.
 * @throws {AlreadyRunningError} if another process holds it.
 */
export default function acquireSingleton(scope, logger = console) {
  const address = addressFor(scope);
  return new Promise((resolve, reject) => {
    const server = net.createServer((conn) => {
      // Identify ourselves to a would-be second instance, then hang up. This is
      // the lock's only traffic; it is never a real channel.
      conn.end(`${process.pid}\n`);
    });
    // The lock must not be a reason for the process to stay alive.
    server.unref();
    server.once('error', async (e) => {
      if (e.code !== 'EADDRINUSE') {
        reject(e);
        return;
      }
      const pid = await queryHolder(address);
      reject(new AlreadyRunningError(
        `already running${pid ? ` as pid ${pid}` : ''} (lock on ${scope})`, pid));
    });
    server.once('listening', () => {
      logger.debug?.(`[singleton] holding lock for ${scope}`);
      resolve(() => {
        server.removeAllListeners('error');
        server.on('error', () => { /* closing a lock is best-effort */ });
        server.close();
      });
    });
    server.listen(address);
  });
}
