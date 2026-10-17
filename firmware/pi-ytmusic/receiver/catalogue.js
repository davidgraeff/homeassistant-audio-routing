/**
 * What YouTube Music calls things: track metadata by id, the queue a list plays, the
 * signed-in library — cached, over the resolver daemon's catalogue ops.
 *
 * WHY THIS EXISTS
 * ---------------
 * The Lounge plane carries ids only (a `Video` is `{id, client, context}`), so the
 * receiver used to know a track by mpv's `media-title` and nothing else: no artist, no
 * album, no name for anything still in the queue — and with a prefetched direct URL,
 * mpv's title is the URL's basename. And without a phone there is no queue at all.
 * Both are answered by the requests the YouTube Music app itself makes (see
 * `ytm_catalogue.py`), which run inside the resolver daemon because they authenticate
 * with the cookie jar and the jar has exactly one writer.
 *
 * Everything here degrades to "unknown" rather than failing: a missing daemon, a
 * signed-out jar or a changed response layout must never stop a track from playing.
 */

/** Track metadata kept by id. A queue of a few hundred is the realistic ceiling. */
const TRACK_CACHE_MAX = 500;
/** `music/get_queue` answers a batch; this is also the daemon's own cap. */
const HYDRATE_BATCH = 50;
/** The library changes when the user plays something elsewhere; a few minutes is fresh
 *  enough for a browse tree and keeps a mirror's polling from becoming request traffic. */
const LIBRARY_TTL_MS = 3 * 60 * 1000;

export default class Catalogue {
  #daemon = null;
  #logger;
  /** videoId -> {video_id, title, artist, album, duration, thumbnail} */
  #tracks = new Map();
  /** videoIds being hydrated right now, so overlapping asks do not duplicate. */
  #pending = new Set();
  /** feed -> {at, value} */
  #library = new Map();

  constructor({ logger = console } = {}) {
    this.#logger = logger;
  }

  setLogger(logger) {
    this.#logger = logger;
  }

  /** Attach the resolver daemon once it runs (`ytdlp.js`), or null to detach. */
  setDaemon(daemon) {
    this.#daemon = daemon;
  }

  /** Whether catalogue reads can be made at all right now. */
  get available() {
    return !!this.#daemon?.hasCatalogue;
  }

  /** Cached metadata for one id, or null. Never fetches. */
  track(videoId) {
    return this.#tracks.get(videoId) ?? null;
  }

  /** Remember tracks learned elsewhere (a queue page carries full metadata). */
  remember(tracks) {
    for (const t of tracks ?? []) {
      if (t?.video_id) {
        this.#tracks.delete(t.video_id);
        this.#tracks.set(t.video_id, t);
      }
    }
    while (this.#tracks.size > TRACK_CACHE_MAX) {
      this.#tracks.delete(this.#tracks.keys().next().value);
    }
  }

  /**
   * Make sure metadata for `videoIds` is cached, fetching what is missing in one batch.
   * Resolves to the ids that were newly learned (so a caller can re-report). Never throws.
   */
  async hydrate(videoIds) {
    const missing = [ ...new Set(videoIds ?? []) ]
      .filter((id) => id && !this.#tracks.has(id) && !this.#pending.has(id))
      .slice(0, HYDRATE_BATCH);
    if (!missing.length || !this.available) {
      return [];
    }
    missing.forEach((id) => this.#pending.add(id));
    try {
      const reply = await this.#daemon.tracks(missing);
      this.remember(reply.items);
      return (reply.items ?? []).map((t) => t.video_id);
    }
    catch (e) {
      this.#logger.debug?.(`[catalogue] could not name ${missing.length} track(s): ${e.message}`);
      return [];
    }
    finally {
      missing.forEach((id) => this.#pending.delete(id));
    }
  }

  /**
   * The queue YouTube Music would play for `playlistId`, from `videoId` if given.
   * Throws with a sentence when it cannot be had — this is a user's explicit request.
   */
  async queue(playlistId, videoId = null) {
    this.#requireAvailable();
    const reply = await this.#daemon.queue(playlistId, { videoId });
    this.remember(reply.items);
    return reply;
  }

  /** The next page of a queue. Throws like `queue`. */
  async continueQueue(playlistId, continuation) {
    this.#requireAvailable();
    const reply = await this.#daemon.queue(playlistId, { continuation });
    this.remember(reply.items);
    return reply;
  }

  /**
   * A library feed (`library` | `playlists` | `history`), cached for LIBRARY_TTL_MS.
   * Resolves `{signed_in, items, error?}` — never throws, because a browse tree with an
   * empty, explained node beats a browse tree that fails to open.
   */
  async library(feed) {
    const hit = this.#library.get(feed);
    if (hit && Date.now() - hit.at < LIBRARY_TTL_MS) {
      return hit.value;
    }
    if (!this.available) {
      return { signed_in: false, items: [], error: 'resolver daemon unavailable' };
    }
    try {
      const reply = await this.#daemon.library(feed);
      if (feed === 'history') {
        this.remember(reply.items);
      }
      const value = { signed_in: !!reply.signed_in, items: reply.items ?? [] };
      this.#library.set(feed, { at: Date.now(), value });
      return value;
    }
    catch (e) {
      this.#logger.warn(`[catalogue] library feed ${feed} failed: ${e.message}`);
      return { signed_in: false, items: [], error: e.message };
    }
  }

  #requireAvailable() {
    if (!this.available) {
      throw new Error('YouTube Music cannot be asked right now (the resolver daemon is not running)');
    }
  }
}
