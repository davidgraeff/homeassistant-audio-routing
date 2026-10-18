/**
 * Played songs, kept on disk up to a size limit, so a song heard before starts from a file.
 *
 * WHY THIS EXISTS
 * ---------------
 * The same playlists come round again and again, and a song on disk needs neither a
 * resolve nor YouTube's goodwill: no `/player` request, no 403 window, and nothing to
 * stall. It also fixes something mpv cannot: googlevideo throttles a single open-ended
 * GET — which is how mpv streams — to ~33 KB/s, about twice real time for opus at
 * 130 kbps, while ranged requests of up to 10 MiB are served at full speed (a whole song
 * in 66-490 ms, measured on the add-on). So this downloads in 10 MiB ranges, the chunk
 * size yt-dlp uses for the same reason.
 *
 * WHAT IS KEPT
 * ------------
 * One file per video id, `<id>.<ext>` (`webm` for opus, `m4a` for AAC), written as
 * `.part` and renamed only once complete — its length checked against the URL's `clen`
 * when it has one — so a reader never sees half a song. The least recently *played*
 * files go first when the total passes the limit; a hit touches the file's mtime, which is
 * the whole of the bookkeeping, so it survives restarts with nothing else to store.
 *
 * Downloads run one at a time, in the background: the current track while mpv already
 * streams it, and the next one while the current one plays. Failures are logged and
 * forgotten — the network path works without this, so nothing here may break it.
 */

import fs from 'fs';
import path from 'path';

/** Range size per request; above this googlevideo starts throttling. */
const CHUNK_BYTES = 10 * 1024 * 1024;
/** Per range request. A whole 10 MiB range took well under a second when measured. */
const CHUNK_TIMEOUT_MS = 30000;
/** Downloads waiting their turn; past this, new ones are dropped rather than queued. */
const MAX_PENDING = 8;
const VIDEO_ID = /^[\w-]{11}$/;

export default class AudioCache {
  #dir;
  #maxBytes;
  #logger;
  /** videoId -> { file, size, used } for complete files. */
  #entries = new Map();
  #total = 0;
  /** videoIds downloading or waiting to, so one song is never fetched twice at once. */
  #pending = new Set();
  #chain = Promise.resolve();

  constructor({ dir, maxBytes, logger = console }) {
    this.#dir = dir;
    this.#maxBytes = maxBytes;
    this.#logger = logger;
  }

  setLogger(logger) {
    this.#logger = logger;
  }

  /** Read what is on disk and drop leftovers of interrupted downloads. Call once. */
  async init() {
    await fs.promises.mkdir(this.#dir, { recursive: true });
    for (const name of await fs.promises.readdir(this.#dir)) {
      const file = path.join(this.#dir, name);
      if (name.endsWith('.part')) {
        await fs.promises.rm(file, { force: true });
        continue;
      }
      const id = name.split('.')[0];
      if (!VIDEO_ID.test(id)) {
        continue;
      }
      try {
        const st = await fs.promises.stat(file);
        this.#add(id, file, st.size, st.mtimeMs);
      }
      catch {
        // vanished meanwhile
      }
    }
    await this.#evict();
    this.#logger.info(`[audiocache] ${this.#entries.size} song(s), ${mib(this.#total)} of ${mib(this.#maxBytes)} in ${this.#dir}`);
  }

  /** Whether `videoId` is on disk, without counting as a use. */
  has(videoId) {
    return this.#entries.has(videoId);
  }

  /** The file for `videoId`, or null. Counts as a use: it is what keeps a song cached. */
  lookup(videoId) {
    const entry = this.#entries.get(videoId);
    if (!entry) {
      return null;
    }
    entry.used = Date.now();
    const now = new Date();
    fs.promises.utimes(entry.file, now, now).catch(() => {});
    return entry.file;
  }

  /** Forget a file that turned out not to play. */
  async remove(videoId) {
    const entry = this.#entries.get(videoId);
    if (!entry) {
      return;
    }
    this.#entries.delete(videoId);
    this.#total -= entry.size;
    await fs.promises.rm(entry.file, { force: true });
  }

  /**
   * Download `url` for `videoId` in the background, unless it is cached or already on its
   * way. `headers` are the resolver's, which the URL is issued against. Never rejects.
   */
  fetch(videoId, url, headers = {}) {
    if (!VIDEO_ID.test(videoId) || this.#entries.has(videoId) || this.#pending.has(videoId)
      || !/^https?:\/\//.test(url ?? '')) {
      return;
    }
    if (this.#pending.size >= MAX_PENDING) {
      this.#logger.debug(`[audiocache] ${MAX_PENDING} downloads waiting — not caching ${videoId}`);
      return;
    }
    this.#pending.add(videoId);
    this.#chain = this.#chain
      .then(() => this.#download(videoId, url, headers))
      .catch((e) => this.#logger.warn(`[audiocache] could not cache ${videoId}: ${e.message}`))
      .finally(() => this.#pending.delete(videoId));
  }

  /** Resolves once every download queued so far has finished; for tests and shutdown. */
  idle() {
    return this.#chain;
  }

  async #download(videoId, url, headers) {
    const params = new URL(url).searchParams;
    const length = Number(params.get('clen')) || null;
    if (length && length > this.#maxBytes) {
      return;
    }
    const file = path.join(this.#dir, `${videoId}.${extensionFor(params.get('mime'))}`);
    const part = `${file}.part`;
    const started = Date.now();
    const out = await fs.promises.open(part, 'w');
    let written = 0;
    try {
      for (;;) {
        const end = length ? Math.min(length, written + CHUNK_BYTES) - 1 : written + CHUNK_BYTES - 1;
        const res = await getRange(url, { ...headers, Range: `bytes=${written}-${end}` });
        if (res.status === 416 && !length && written > 0) {
          break; // asked past the end of a file of unknown length
        }
        if (res.status !== 206 && res.status !== 200) {
          throw new Error(`HTTP ${res.status} at byte ${written}`);
        }
        const { body } = res;
        await out.write(body);
        written += body.length;
        const total = Number(/\/(\d+)$/.exec(res.contentRange ?? '')?.[1]) || length;
        if (res.status === 200 || body.length === 0 || (total && written >= total)) {
          if (total && written !== total) {
            throw new Error(`got ${written} of ${total} bytes`);
          }
          break;
        }
      }
    }
    catch (e) {
      await out.close().catch(() => {});
      await fs.promises.rm(part, { force: true });
      throw e;
    }
    await out.close();
    await fs.promises.rename(part, file);
    this.#add(videoId, file, written, Date.now());
    this.#logger.info(`[audiocache] cached ${videoId} (${mib(written)} in ${((Date.now() - started) / 1000).toFixed(1)}s)`);
    await this.#evict();
  }

  #add(videoId, file, size, used) {
    const old = this.#entries.get(videoId);
    if (old) {
      this.#total -= old.size;
    }
    this.#entries.set(videoId, { file, size, used });
    this.#total += size;
  }

  async #evict() {
    if (this.#total <= this.#maxBytes) {
      return;
    }
    const oldest = [ ...this.#entries ].sort((a, b) => a[1].used - b[1].used);
    for (const [ id ] of oldest) {
      if (this.#total <= this.#maxBytes) {
        break;
      }
      await this.remove(id);
      this.#logger.debug(`[audiocache] evicted ${id}`);
    }
  }
}

function extensionFor(mime) {
  if (/^audio\/webm/.test(mime ?? '')) {
    return 'webm';
  }
  if (/^audio\/mp4/.test(mime ?? '')) {
    return 'm4a';
  }
  return 'media';
}

/** One range request, read whole: `{ status, contentRange, body }`, bounded in time. */
async function getRange(url, headers) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), CHUNK_TIMEOUT_MS);
  try {
    const res = await fetch(url, { headers, signal: controller.signal });
    const body = Buffer.from(await res.arrayBuffer());
    return { status: res.status, contentRange: res.headers.get('content-range'), body };
  }
  finally {
    clearTimeout(timer);
  }
}

function mib(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}
