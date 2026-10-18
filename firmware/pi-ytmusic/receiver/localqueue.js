/**
 * The receiver's own queue, for playback nobody cast.
 *
 * A cast session's queue belongs to the phone: yt-cast-receiver's `Playlist` mirrors
 * it, and next/previous ask YouTube for the neighbours of the current item in the
 * phone's list. Started from Home Assistant there is no phone and no session, so the
 * receiver holds the list itself — the page of tracks YouTube Music's `next` answered
 * for the playlist, plus the continuation token that fetches the next page (a radio mix
 * is endless, a long playlist is paged).
 *
 * Pure bookkeeping, no I/O: `player.js` decides when to play and when to fetch more.
 * Items are the catalogue's track objects (`{video_id, title, artist, album, duration,
 * thumbnail}`).
 */

/** Fetch the next page when the current item is this close to the end. */
const REFILL_THRESHOLD = 2;
/** A radio can be paged forever; the queue a person can scroll through cannot. */
const MAX_ITEMS = 500;

export default class LocalQueue {
  #items;
  #index;

  constructor({ playlistId, title = null, items = [], continuation = null, startIndex = 0 }) {
    this.playlistId = playlistId;
    this.title = title;
    this.continuation = continuation;
    this.#items = [ ...items ];
    this.#index = Math.min(Math.max(0, startIndex), Math.max(0, this.#items.length - 1));
  }

  get index() {
    return this.#index;
  }

  get length() {
    return this.#items.length;
  }

  get current() {
    return this.#items[this.#index] ?? null;
  }

  /** The item after the current one, without moving — what to prefetch. */
  get upcoming() {
    return this.#items[this.#index + 1] ?? null;
  }

  get hasNext() {
    return this.#index < this.#items.length - 1 || !!this.continuation;
  }

  get hasPrevious() {
    return this.#index > 0;
  }

  /** Whether the next page should be fetched now (there is one, and the end is near). */
  get needsMore() {
    return !!this.continuation && this.#items.length < MAX_ITEMS
      && this.#index >= this.#items.length - 1 - REFILL_THRESHOLD;
  }

  /** Move to the next item; returns it, or null at the end (the index stays). */
  advance() {
    if (this.#index >= this.#items.length - 1) {
      return null;
    }
    this.#index += 1;
    return this.current;
  }

  /** Move to the previous item; returns it, or null at the start. */
  back() {
    if (this.#index <= 0) {
      return null;
    }
    this.#index -= 1;
    return this.current;
  }

  /** Make `index` current; returns it, or null when out of range. */
  jump(index) {
    if (!Number.isInteger(index) || index < 0 || index >= this.#items.length) {
      return null;
    }
    this.#index = index;
    return this.current;
  }

  /** Add a fetched page. A track already queued is not queued twice — a radio's next
   *  page can repeat the seam. */
  append(items, continuation) {
    const seen = new Set(this.#items.map((t) => t.video_id));
    for (const t of items ?? []) {
      if (t?.video_id && !seen.has(t.video_id) && this.#items.length < MAX_ITEMS) {
        seen.add(t.video_id);
        this.#items.push(t);
      }
    }
    this.continuation = this.#items.length < MAX_ITEMS ? (continuation ?? null) : null;
  }

  /**
   * Replace the list with a fetched page while keeping what plays.
   *
   * For a queue started before its list was known — `player.js` plays a requested song at
   * once and fetches its radio in parallel. The current track stays current: at its place
   * in the page when the page has it (its richer catalogue entry replacing a bare id), in
   * front of it otherwise.
   */
  adopt(items, continuation) {
    const page = (items ?? []).filter((t) => t?.video_id).slice(0, MAX_ITEMS);
    if (!page.length) {
      return;
    }
    const current = this.current;
    const at = current ? page.findIndex((t) => t.video_id === current.video_id) : -1;
    if (current && at < 0) {
      this.#items = [ current, ...page ].slice(0, MAX_ITEMS);
      this.#index = 0;
    }
    else {
      this.#items = page;
      this.#index = Math.max(0, at);
    }
    this.continuation = this.#items.length < MAX_ITEMS ? (continuation ?? null) : null;
  }

  /** Items around the current one: `before` behind it, `after` ahead, with their index. */
  window({ before = 0, after = 50 } = {}) {
    const start = Math.max(0, this.#index - before);
    const end = Math.min(this.#items.length, this.#index + after + 1);
    return this.#items.slice(start, end).map((t, i) => ({ ...t, index: start + i, current: start + i === this.#index }));
  }
}
