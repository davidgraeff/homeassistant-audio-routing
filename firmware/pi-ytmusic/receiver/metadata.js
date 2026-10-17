/**
 * Report what this receiver is playing to the audio-router add-on.
 *
 * The add-on models now-playing per *source*, and this receiver is one of its RTP
 * sources — so it reports the same way the Bluetooth bridge does: `POST
 * /api/now_playing/report` (see ../../../docs/api-reference.md) with the RTP port
 * that identifies which source this is. Nothing here knows (or needs to know) the
 * source ids the add-on assigned.
 *
 * **Where the metadata comes from — not from the phone.** The Lounge protocol
 * carries video ids and positions only: a `Video` is `{id, client, context}`, and
 * the only `thumbnail` fields in yt-cast-receiver are Google-account avatars. So
 * the title comes from mpv (`media-title`, which its `ytdl_hook` fills in from
 * yt-dlp), and the artwork from the deterministic thumbnail URL for the video id —
 * no extra HTTP request, no API call.
 *
 * Title, artist, album and square album art come from the **catalogue** when it can
 * name the track (`catalogue.js` — YouTube Music's own queue/`get_queue` answers, one
 * batched request per queue change, no extra resolve). A local queue knows them before
 * the track starts; a cast id is named a moment later via `describe()`. Until then, and
 * whenever the catalogue cannot answer, mpv's `media-title` is the fallback — which is
 * the *video* title for `ytdl_hook`'s own resolution, and merely the URL's basename for
 * a prefetched direct URL, so it is never allowed to overwrite a catalogue title.
 */

const DEFAULT_API_PORT = 8099;
/** How often to refresh the position while playing. The add-on throttles
 *  publishing to ~5 s and Home Assistant extrapolates in between, so anything
 *  faster is pure radio traffic on a Pi that shares its antenna with Bluetooth. */
const POSITION_INTERVAL_MS = 5000;
/** Give up on a report quickly: a wedged add-on must not pile up requests, and a
 *  missed update is worthless — the next one is seconds away. */
const REQUEST_TIMEOUT_MS = 4000;

/** Cover art for a YouTube video id, straight from the id. */
export function artworkUrl(videoId) {
  return videoId ? `https://i.ytimg.com/vi/${videoId}/hqdefault.jpg` : null;
}

function emptyTrack() {
  return { videoId: null, title: null, artist: null, album: null, artworkUrl: null, durationMs: null,
    state: 'stopped', named: false };
}

export default class MetadataReporter {
  #url;
  #rtpPort;
  #logger;
  #timer = null;
  /** Last body sent, so an unchanged report is not sent again. */
  #lastSent = null;
  /** Current track, accumulated from several sources (see the class comment). */
  #track = emptyTrack();
  #mpv = null;

  constructor({ host, apiPort = DEFAULT_API_PORT, rtpPort, logger = console }) {
    this.#url = `http://${host}:${apiPort}/api/now_playing/report`;
    this.#rtpPort = rtpPort;
    this.#logger = logger;
  }

  /**
   * Observe the mpv properties that describe the track.
   *
   * `media-title` is observed rather than polled because it arrives *late*: mpv
   * only knows it once `ytdl_hook` has resolved the URL, which is seconds after
   * `loadfile` on this hardware.
   */
  attach(mpv) {
    this.#mpv = mpv;
    mpv.observeProperty('media-title', (value) => {
      if (typeof value === 'string' && value.trim() && !this.#track.named) {
        this.#track.title = value.trim();
        this.#report();
      }
    });
    mpv.observeProperty('duration', (value) => {
      if (typeof value === 'number' && value > 0) {
        this.#track.durationMs = Math.round(value * 1000);
        this.#report();
      }
    });
    mpv.observeProperty('pause', (value) => {
      // Only meaningful while a track is loaded: mpv sits paused while idle.
      if (this.#track.videoId) {
        this.#track.state = value ? 'paused' : 'playing';
        this.#report();
      }
    });
  }

  /**
   * A new track started (called from the player, which is told the video id, and the
   * catalogue's metadata for it when that is already known).
   */
  trackStarted(videoId, info = null) {
    // Reset rather than merge: a new video id means everything the previous track
    // contributed is stale, and the add-on treats a changed title the same way.
    this.#track = { ...emptyTrack(), videoId, state: 'playing' };
    this.#apply(info);
    this.#startPositionTimer();
    // The artwork is known from the id alone, so there is always something to send
    // now; with catalogue metadata it is the whole description.
    this.#report();
  }

  /** The catalogue named the current track after it started (a cast id). */
  describe(videoId, info) {
    if (!videoId || videoId !== this.#track.videoId || !info) {
      return;
    }
    this.#apply(info);
    this.#report();
  }

  #apply(info) {
    if (!info?.title) {
      return;
    }
    this.#track.title = info.title;
    this.#track.artist = info.artist ?? null;
    this.#track.album = info.album ?? null;
    this.#track.artworkUrl = info.thumbnail ?? null;
    this.#track.durationMs ??= info.duration ? info.duration * 1000 : null;
    this.#track.named = true;
  }

  /** Playback stopped for good: clear the add-on's entry. */
  stopped() {
    this.#stopPositionTimer();
    this.#track = emptyTrack();
    this.#lastSent = null;
    this.#post({});
  }

  /** Stop reporting (service shutdown). Leaves the add-on entry cleared. */
  close() {
    this.#stopPositionTimer();
  }

  #startPositionTimer() {
    if (this.#timer) {
      return;
    }
    this.#timer = setInterval(() => void this.#reportPosition(), POSITION_INTERVAL_MS);
    // Never hold the process open just to report a position.
    this.#timer.unref?.();
  }

  #stopPositionTimer() {
    if (this.#timer) {
      clearInterval(this.#timer);
      this.#timer = null;
    }
  }

  async #reportPosition() {
    if (!this.#mpv || !this.#track.videoId || this.#track.state !== 'playing') {
      return;
    }
    const pos = await this.#mpv.getProperty('time-pos', null);
    if (typeof pos !== 'number') {
      return;
    }
    // Position always counts as new, so it bypasses the dedupe below.
    this.#post({ ...this.#body(), position_ms: Math.max(0, Math.round(pos * 1000)) }, { dedupe: false });
  }

  #body() {
    const body = { state: this.#track.state };
    if (this.#track.title) {
      body.title = this.#track.title;
    }
    if (this.#track.artist) {
      body.artist = this.#track.artist;
    }
    if (this.#track.album) {
      body.album = this.#track.album;
    }
    if (this.#track.durationMs) {
      body.duration_ms = this.#track.durationMs;
    }
    const art = this.#track.artworkUrl ?? artworkUrl(this.#track.videoId);
    if (art) {
      body.artwork_url = art;
    }
    return body;
  }

  #report() {
    if (!this.#track.videoId) {
      return;
    }
    this.#post(this.#body());
  }

  async #post(body, { dedupe = true } = {}) {
    // An empty body is how the add-on is told to clear the entry, so it must not
    // be dropped as "nothing to say".
    const isClear = Object.keys(body).length === 0;
    const payload = JSON.stringify({ rtp_port: this.#rtpPort, ...body });
    if (dedupe && !isClear && payload === this.#lastSent) {
      return;
    }
    this.#lastSent = payload;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
    try {
      const res = await fetch(this.#url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: payload,
        signal: controller.signal,
      });
      if (!res.ok) {
        // 404 is the expected benign case: no RTP source configured on this port
        // (yet). Log it and let the next change retry.
        this.#logger.warn(
          `[metadata] add-on returned ${res.status}` +
          (res.status === 404 ? ` — is an RTP source configured on port ${this.#rtpPort}?` : ''));
        this.#lastSent = null;
      }
    }
    catch (e) {
      this.#logger.warn(`[metadata] could not reach the add-on: ${e.message}`);
      this.#lastSent = null;
    }
    finally {
      clearTimeout(timer);
    }
  }
}
