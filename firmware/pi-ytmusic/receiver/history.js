/**
 * The playlists this receiver played recently — the "start one of the last few" list.
 *
 * WHY THE RECEIVER KEEPS ITS OWN
 * ------------------------------
 * The signed-in library ("recent activity", `catalogue.js`) is offered too, but this is
 * the list that is always right for *this* speaker: it records exactly what the house
 * played, whether a phone cast it (the Lounge `setPlaylist` names its list) or Home
 * Assistant started it, it works with no cookie jar at all, and it does not depend on a
 * private YouTube Music page layout staying put. It is also the only one with real
 * timestamps.
 *
 * Persisted as one small JSON file in the state directory (the add-on's /data, the Pi's
 * state dir), written atomically. A lost or corrupt file is an empty list, never an
 * error: this is a convenience, not state anything depends on.
 */

import { readFileSync, renameSync, writeFileSync } from 'fs';
import path from 'path';

/** How many to keep. A mirror shows a handful; the rest is headroom for dedupe. */
const MAX_ENTRIES = 25;
/**
 * List ids that cannot be played again later. `RQ…` is the Lounge's ad-hoc remote queue
 * — songs cast one by one — which exists only for that session.
 */
const UNREPLAYABLE = /^RQ/;
const FILE_NAME = 'recent-playlists.json';

export default class PlayHistory {
  #file;
  #logger;
  /** newest first: {playlist_id, title, thumbnail, kind, origin, played_at} */
  #entries = [];

  constructor({ dir = process.cwd(), logger = console } = {}) {
    this.#file = path.join(dir, FILE_NAME);
    this.#logger = logger;
    this.#load();
  }

  setLogger(logger) {
    this.#logger = logger;
  }

  get file() {
    return this.#file;
  }

  #load() {
    try {
      const data = JSON.parse(readFileSync(this.#file, 'utf8'));
      this.#entries = (Array.isArray(data?.playlists) ? data.playlists : [])
        .filter((e) => typeof e?.playlist_id === 'string')
        .slice(0, MAX_ENTRIES);
    }
    catch {
      this.#entries = [];
    }
  }

  #save() {
    const tmp = `${this.#file}.tmp`;
    try {
      writeFileSync(tmp, JSON.stringify({ playlists: this.#entries }, null, 1));
      renameSync(tmp, this.#file);
    }
    catch (e) {
      this.#logger.warn(`[history] could not save ${this.#file}: ${e.message}`);
    }
  }

  /**
   * A list started playing. Moves it to the front; fields given here win over what was
   * known, absent ones keep what was known (a phone's cast names no title).
   * Returns whether it was recorded.
   */
  record({ playlist_id: id, title = null, thumbnail = null, kind = null, origin = null }, now = Date.now()) {
    if (typeof id !== 'string' || !id || UNREPLAYABLE.test(id)) {
      return false;
    }
    const known = this.#entries.find((e) => e.playlist_id === id) ?? {};
    const entry = {
      playlist_id: id,
      title: title ?? known.title ?? null,
      thumbnail: thumbnail ?? known.thumbnail ?? null,
      kind: kind ?? known.kind ?? kindOf(id),
      origin: origin ?? known.origin ?? null,
      played_at: now,
    };
    this.#entries = [ entry, ...this.#entries.filter((e) => e.playlist_id !== id) ].slice(0, MAX_ENTRIES);
    this.#save();
    return true;
  }

  /** Fill in what was learned later (a cast's title, once the queue was looked up). */
  describe(id, { title = null, thumbnail = null } = {}) {
    const e = this.#entries.find((x) => x.playlist_id === id);
    if (!e || ((!title || e.title === title) && (!thumbnail || e.thumbnail === thumbnail))) {
      return;
    }
    e.title = title ?? e.title;
    e.thumbnail = thumbnail ?? e.thumbnail;
    this.#save();
  }

  /** The newest `n`, newest first. */
  list(n = MAX_ENTRIES) {
    return this.#entries.slice(0, n).map((e) => ({ ...e }));
  }
}

/** What kind of list an id is, by its prefix. */
export function kindOf(id) {
  if (id.startsWith('OLAK5uy_')) {
    return 'album';
  }
  if (id.startsWith('RD')) {
    return 'mix';
  }
  return 'playlist';
}
