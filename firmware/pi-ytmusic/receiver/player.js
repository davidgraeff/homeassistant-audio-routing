/**
 * `Player` implementation backed by mpv (see mpv.js).
 *
 * yt-cast-receiver ships no player on purpose: DIAL + the Lounge API are a
 * **control plane only** — the phone sends "play video id X at position Y" and
 * never sends audio, so we are the player. Every abstract method here maps onto
 * one or two mpv IPC calls; mpv itself owns resolution (`yt-dlp` via its
 * `ytdl_hook`), decoding, buffering and seeking.
 *
 * Two behaviours are load-bearing and easy to get wrong:
 *
 *   - `doPlay` must not resolve `true` until playback has actually started. The
 *     base class flips its state to PLAYING when we resolve, and that state is
 *     what the phone's UI renders — resolving early makes the app claim it is
 *     playing while `yt-dlp` is still working.
 *   - End of track has to be reported back, or the queue never advances. mpv's
 *     `end-file` event does that, but it fires for *stop* too, so the reason has
 *     to be checked (`eof` = track finished, `stop`/`quit` = we did it).
 *
 * TWO KINDS OF QUEUE
 * ------------------
 * A cast session's queue is the phone's: yt-cast-receiver's `Playlist` mirrors it and
 * the base class's next/previous walk it. Playback started from Home Assistant has no
 * phone, so it runs from the receiver's own queue (`LocalQueue`, filled from YouTube
 * Music's `next` via `catalogue.js`) — "local" below. Both drive this one player and
 * both go through the base class's play/pause/stop, which is what reports every state
 * change to a connected phone, so a phone that is watching stays in sync either way.
 *
 * The phone always wins: the moment its queue changes (it cast something, queued a
 * song, picked another), local playback ends and the cast queue is authoritative again.
 */

import { Constants, Player } from 'yt-cast-receiver';
import LocalQueue from './localqueue.js';

const { PLAYER_STATUSES, CLIENTS } = Constants;
/** The cast queue's events, i.e. "a phone changed what is queued". */
const CAST_QUEUE_EVENTS = [ 'playlistSet', 'playlistAdded', 'playlistUpdated', 'playlistCleared',
  'videoSelected', 'videoAdded', 'videoRemoved' ];
/**
 * The cast-queue events that are somebody choosing something, which start the auto-stop's
 * count again. Not `playlistUpdated`: a phone sends a bare `updatePlaylist` on its own —
 * one arrived mid-playback with nobody touching it — and counting those would reset the
 * count on every song, so a cast would never stop.
 */
const CHOICE_EVENTS = new Set([ 'playlistSet', 'playlistAdded', 'videoSelected', 'videoAdded' ]);
/** Skip at most this many unplayable tracks in a row before giving up on a local queue. */
const MAX_LOCAL_SKIPS = 3;
/** How many upcoming cast-queue ids to name for the control API's queue. */
const CAST_HYDRATE_AHEAD = 25;

/**
 * How long to allow for resolve + first audio before calling a play attempt failed.
 *
 * Generous because resolution is genuinely slow on this hardware, and measured
 * rather than guessed: on the Pi Zero 2 W an *authenticated* resolve costs ~22 s
 * with node 22 (and ~90 s with quickjs, which is why node 22 is installed);
 * anonymous is ~6 s. The first version of this used 60 s and reported every track
 * as failed while yt-dlp was still working.
 */
const PLAY_START_TIMEOUT_MS = 150000;
/**
 * How long a stream mpv could not open may wait for the audio cache's copy of it before the
 * retry asks yt-dlp instead. The copy is fetched in ranges alongside the stream and is
 * normally on disk within half a second; a yt-dlp resolve costs several.
 */
const CACHE_RESCUE_MS = 3000;
/** How often the auto-stop counts playing time (playlimit.js). Its limit is in hours. */
const LIMIT_SAMPLE_MS = 30 * 1000;

export default class MpvPlayer extends Player {
  #mpv;
  /** Optional now-playing reporter (metadata.js); absent when reporting is off. */
  #metadata = null;
  /** Optional pre-resolver (resolver.js); absent when prefetching is off. */
  #resolver = null;
  /** Last volume we set, so doGetVolume can answer while mpv is idle. */
  #volume = { level: 100, muted: false };
  /** Video id whose direct URL already got one retry, so retries cannot recurse. */
  #retriedFor = null;
  /** Track metadata and queues (catalogue.js); absent means ids only, as before. */
  #catalogue = null;
  /** Recently played lists (history.js); absent means nothing is recorded. */
  #history = null;
  /** The receiver's own queue while playing something nobody cast, else null. */
  #local = null;
  /** Set while *we* rewrite the cast queue, so our own change is not read as a phone's. */
  #selfQueueUpdate = false;
  /** The last thing that went wrong on a local request, for the control API to show. */
  #lastError = null;
  /** Bumped by every local start, so a superseded start's late failure says nothing. */
  #localGeneration = 0;
  /** Songs on disk (audiocache.js); absent means every play streams. */
  #cache = null;
  /**
   * Loads in flight in `doPlay`. A file that fails to *open* emits `end-file reason=error`
   * too, and that failure is `doPlay`'s to handle — it retries. Left to the end-file
   * handler as well, the same failure also skipped to the next track: observed, a requested
   * song replaced by its successor while its own retry was starting.
   */
  #loading = 0;
  /** Auto-stop after a maximum of continuous play (playlimit.js); absent means never. */
  #limit = null;

  constructor(mpv, metadata = null, resolver = null,
    { catalogue = null, history = null, cache = null, limit = null } = {}) {
    super();
    this.#mpv = mpv;
    this.#metadata = metadata;
    this.#resolver = resolver;
    this.#catalogue = catalogue;
    this.#history = history;
    this.#cache = cache;
    this.#limit = limit;
    if (limit) {
      // unref: a sampling timer must never be what keeps the process alive.
      setInterval(() => void this.#sampleLimit(), LIMIT_SAMPLE_MS).unref?.();
    }

    const onCastQueue = (event) => this.#onCastQueue(event);
    for (const type of CAST_QUEUE_EVENTS) {
      this.queue.on(type, onCastQueue);
    }

    this.#mpv.on('end-file', (msg) => {
      const reason = msg.reason ?? 'unknown';
      // `reason` alone decides this. There used to be an `#expectingStop` flag as
      // well, set by doPlay to swallow the outgoing file's end-file — and it was the
      // cause of "the queue never advances after a song finishes": when mpv is IDLE
      // there IS no outgoing file, so the flag survived and swallowed the *current*
      // song's genuine EOF instead. Every first track of a session ended in silence.
      //
      // The flag was also unnecessary. Measured on mpv 0.40: replacing a playing
      // file emits `end-file reason=stop`, and an explicit `stop` command emits
      // `reason=stop` too — both already covered here. (end-file also carries
      // `playlist_entry_id`, if this ever needs to correlate precisely.)
      if (reason === 'stop' || reason === 'quit' || reason === 'redirect') {
        return;
      }
      if (reason === 'error' && this.#loading > 0) {
        return; // a file that would not open: doPlay's to retry, see #loading
      }
      if (reason === 'error') {
        // A single unplayable track must not end the session: log it and move on,
        // which is also what the phone's UI expects to happen.
        this.logger.error(`[MpvPlayer] playback error (${msg.file_error ?? 'unknown'}) — skipping to next`);
      }
      else {
        this.logger.info('[MpvPlayer] track ended — advancing queue');
      }
      void (async () => {
        try {
          if (this.#limitReached()) {
            await this.#autoStop('at the end of the song');
            return;
          }
          await this.pause();
          await this.next();
        }
        catch (e) {
          this.logger.error(`[MpvPlayer] failed to advance queue: ${e.message}`);
        }
      })();
    });
  }

  // --- auto-stop (playlimit.js) ---------------------------------------------------------

  /** Whether the next track change should be a stop instead. Counts the song just ended. */
  #limitReached() {
    this.#limit?.sample(true);
    return !!this.#limit?.reached;
  }

  async #sampleLimit() {
    this.#limit.sample(this.status === PLAYER_STATUSES.PLAYING);
    if (this.#limit.overrun && this.status === PLAYER_STATUSES.PLAYING) {
      // One song running far past the limit — an hour-long mix — is not waited for.
      await this.#autoStop('in the middle of an overlong song').catch((e) =>
        this.logger.error(`[MpvPlayer] auto-stop failed: ${e.message}`));
    }
  }

  /**
   * Stop because the limit was reached, through the base class so a phone sees it. A local
   * queue moves past the song that just ended first, so pressing play afterwards continues
   * the list instead of repeating it. The stop is a break: the count starts again.
   */
  async #autoStop(where) {
    const { played_minutes: played } = this.#limit.describe();
    this.logger.info(`[MpvPlayer] ${Math.floor(played / 60)} h ${played % 60} min of continuous play — `
      + `stopping ${where} (maximum ${this.#limit.maxMinutes} min, set on the admin page)`);
    if (where === 'at the end of the song') {
      this.#local?.advance();
    }
    this.#limit.reset();
    await this.stop();
  }

  async doPlay(video, position) {
    // Timed in three parts, because "20 s until I could hear music" is otherwise
    // unattributable: the resolve is logged by resolver.js, but everything after it —
    // mpv opening the URL, demuxing from a mid-stream position, filling `--audio-buffer`
    // — was invisible. `playback-restart` is mpv's own "audio is flowing again" event and
    // is the closest thing to when sound actually starts; the RTP hop and the router's
    // buffering (~0.6 s) sit downstream of even that.
    const startedAt = Date.now();
    const since = (t) => ((Date.now() - t) / 1000).toFixed(1);
    const onRestart = () => {
      this.#mpv.off('playback-restart', onRestart);
      this.logger.info(`[MpvPlayer] ${video.id} audio flowing after ${since(startedAt)}s`);
    };
    this.#mpv.on('playback-restart', onRestart);
    const watchUrl = `https://www.youtube.com/watch?v=${video.id}`;
    // Resolve through resolver.js rather than letting mpv's ytdl_hook do it.
    //
    // A prefetched URL is instant (~0.6 s vs ~30 s) — that is the win. A *cold*
    // resolve is NOT faster here than through mpv: measured 28-31 s either way,
    // with run-to-run variance larger than the difference. The reason to route the
    // cold path through the resolver anyway is **deduplication**: a play that
    // arrives while its own prefetch is still running joins that resolve instead of
    // starting a second one in parallel, which on a 4-core 1 GHz box would slow
    // both. The watch URL stays the fallback, so a resolver failure just degrades to
    // mpv's own behaviour.
    // A song on disk needs no resolve at all (audiocache.js).
    const file = this.#cache?.lookup(video.id) ?? null;
    const wasPrefetched = !file && !!this.#resolver?.peek(video.id);
    this.logger.info(
      `[MpvPlayer] play ${video.id} from ${position}s${file ? ' (cached)' : wasPrefetched ? ' (prefetched)' : ''}`);
    const direct = file ? null : await this.#resolver?.urlFor(video.id) ?? null;
    if (!file && !direct && this.#resolver?.enabled) {
      this.logger.warn(`[MpvPlayer] falling back to mpv's own resolution for ${video.id}`);
    }
    const prefetched = !!direct;
    const resolvedAt = Date.now();
    const url = file ?? direct?.url ?? watchUrl;
    if (direct) {
      // Fetched alongside mpv's own stream, in fast ranges, so the next time is a file.
      this.#cache?.fetch(video.id, direct.url, direct.headers);
    }
    // Hand mpv the User-Agent yt-dlp used, because the URL is issued against it: some
    // googlevideo URLs answer **403** to a request without a matching UA and 206 with
    // one (measured on the add-on, same URL, three fetchers). `ytdl_hook` passes these
    // headers when *it* resolves; a prefetched URL bypasses it, so they have to be
    // supplied here or the bypass silently loses playback on those tracks.
    //
    // Set as a property rather than a per-file option: `loadfile`'s option list is
    // comma-separated and the UA contains a comma ("(KHTML, like Gecko)"), which would
    // need mpv's `%<len>%` escaping. One long-lived mpv plays one stream at a time, so
    // a global is equivalent and unambiguous. Cleared on the fallback path so a stale
    // UA cannot follow ytdl_hook's own resolution.
    await this.#mpv.setProperty('user-agent', direct?.headers?.['User-Agent'] ?? '');
    // Tell the reporter which video this is *before* loading: the artwork URL is
    // derivable from the id alone, so it can be reported while yt-dlp is still
    // resolving the title (metadata.js). A local queue already knows title, artist and
    // album; a cast id is named by the catalogue as soon as it answers.
    this.#metadata?.trackStarted(video.id, this.#catalogue?.track(video.id) ?? null);
    this.#nameUpcoming(video.id);
    // Wait for mpv to report the file open (or fail) rather than trusting the
    // command's own acknowledgement, which only means "queued".
    const started = new Promise((resolve) => {
      const done = (ok) => {
        this.#mpv.off('file-loaded', onLoaded);
        this.#mpv.off('end-file', onEnd);
        clearTimeout(timer);
        resolve(ok);
      };
      const onLoaded = () => done(true);
      const onEnd = (msg) => {
        if (msg.reason === 'error') {
          done(false);
        }
      };
      const timer = setTimeout(() => {
        this.logger.error(`[MpvPlayer] ${video.id} did not start within ${PLAY_START_TIMEOUT_MS / 1000}s`);
        done(false);
      }, PLAY_START_TIMEOUT_MS);
      this.#mpv.on('file-loaded', onLoaded);
      this.#mpv.on('end-file', onEnd);
    });

    // `loadfile <url> [<flags> [<index> [<options>]]]` — the per-file options are
    // the FIFTH argument, and the fourth is an insertion index. Passing options in
    // the fourth slot (as older mpv accepted) fails with "invalid parameter" on
    // mpv 0.40, which is what shipped here: every track failed to load while the
    // cast session itself worked perfectly. `-1` means "no explicit index".
    // The 4-argument form is retried for older mpv builds, so this works either way.
    const options = [ 'pause=no' ];
    if (position > 0) {
      options.push(`start=${position}`);
    }
    let ok;
    this.#loading += 1;
    try {
      try {
        await this.#mpv.command('loadfile', url, 'replace', -1, options.join(','));
      }
      catch (e) {
        if (!/invalid parameter/i.test(e.message)) {
          this.logger.error(`[MpvPlayer] loadfile failed: ${e.message}`);
          return false;
        }
        this.logger.debug('[MpvPlayer] loadfile 5-arg form rejected; trying the older 4-arg form');
        try {
          await this.#mpv.command('loadfile', url, 'replace', options.join(','));
        }
        catch (e2) {
          this.logger.error(`[MpvPlayer] loadfile failed: ${e2.message}`);
          return false;
        }
      }
      ok = await started;
    }
    finally {
      this.#loading -= 1;
    }
    this.logger.info(
      `[MpvPlayer] ${video.id} ${ok ? 'loaded' : 'failed'} after ${since(startedAt)}s `
      + `(resolve ${((resolvedAt - startedAt) / 1000).toFixed(1)}s, mpv ${since(resolvedAt)}s)`);
    if (!ok) {
      this.#mpv.off('playback-restart', onRestart);
    }
    if (!ok && (prefetched || file) && this.#retriedFor !== video.id) {
      // The direct URL was accepted by `loadfile` but did not play — most likely
      // expired or rejected. Drop it and try once more via mpv's own resolution,
      // rather than reporting a dead track to the phone. A cached file that does not
      // play is dropped the same way, and the retry streams.
      //
      // The `#retriedFor` guard is load-bearing: without it, a fresh resolve is
      // also a "direct" URL, so a URL that consistently fails to play would
      // recurse forever.
      this.#retriedFor = video.id;
      if (file) {
        this.logger.warn(`[MpvPlayer] cached file for ${video.id} did not play — retrying once, streamed`);
        await this.#cache.remove(video.id);
      }
      else if (await this.#cache?.ready(video.id, CACHE_RESCUE_MS)) {
        // The stream would not open (googlevideo's intermittent 503), but the copy fetched
        // in ranges alongside it made it: play that rather than resolve the song again.
        this.logger.warn(`[MpvPlayer] stream for ${video.id} did not play — retrying once, from the cached copy`);
      }
      else {
        this.logger.warn(`[MpvPlayer] direct URL for ${video.id} did not play — retrying once`);
        this.#resolver?.invalidate(video.id);
      }
      return this.doPlay(video, position);
    }
    this.#retriedFor = null;
    if (!ok && !this.#local) {
      // Out of retries on a cast queue: move on, as the end-file handler would for a track
      // that broke while playing. (A local queue skips forward in #playLocalCurrentOrThrow.)
      // Deferred, because this runs inside the base class's own play().
      setImmediate(() => void (async () => {
        try {
          await this.pause();
          await this.next();
        }
        catch (e) {
          this.logger.error(`[MpvPlayer] failed to advance queue: ${e.message}`);
        }
      })());
    }
    if (ok) {
      this.#prefetchUpcoming();
    }
    return ok;
  }

  /**
   * Start resolving whatever comes next, while the current track plays.
   *
   * The queue is the cast session's, so this follows what the phone actually shows
   * — including its autoplay suggestion when the user queued nothing further.
   */
  #prefetchUpcoming() {
    if (this.#local) {
      // Fetch the queue's next page while this track plays, so a radio never runs dry
      // at a track change.
      void this.#refillLocal();
    }
    if (!this.#resolver?.enabled) {
      return;
    }
    try {
      const next = this.#local
        ? (this.#local.upcoming ? { id: this.#local.upcoming.video_id } : null)
        : (this.queue?.next ?? this.queue?.autoplay ?? null);
      if (!next?.id || this.#cache?.has(next.id)) {
        return;
      }
      if (!this.#cache) {
        this.#resolver.prefetch(next.id);
        return;
      }
      // With a cache the next song is downloaded whole while this one plays, so it
      // starts from a file — a ~5 MB song takes well under a second in ranges.
      this.logger.info(`[MpvPlayer] prefetching and caching ${next.id}`);
      void this.#resolver.urlFor(next.id).then((d) => {
        if (d) {
          this.#cache.fetch(next.id, d.url, d.headers);
        }
      });
    }
    catch (e) {
      this.logger.debug(`[MpvPlayer] could not look up the next track: ${e.message}`);
    }
  }

  /**
   * Name the current track and what follows it, for the now-playing report and the
   * control API's queue. One batched catalogue request; nothing waits on it.
   */
  #nameUpcoming(currentId) {
    if (!this.#catalogue?.available || this.#local) {
      return;
    }
    const ids = this.queue?.videoIds ?? [];
    const at = Math.max(0, ids.indexOf(currentId));
    const wanted = [ currentId, ...ids.slice(at, at + CAST_HYDRATE_AHEAD) ];
    void this.#catalogue.hydrate(wanted).then((learned) => {
      if (learned.includes(currentId)) {
        this.#metadata?.describe(currentId, this.#catalogue.track(currentId));
      }
    }).catch((e) => this.logger.debug(`[MpvPlayer] naming ${currentId} failed: ${e.message}`));
  }

  // --- the receiver's own queue -------------------------------------------------------

  /**
   * Play `playlistId` from the receiver's own queue — no phone involved.
   *
   * Resolves once the queue is known (one catalogue request) and playback has been
   * *started*; the first track's resolve then runs in the background, visible as the
   * `loading` state, so Home Assistant's service call returns in about a second rather
   * than after a cold resolve. Throws with a sentence when the list cannot be queued.
   *
   * With a `videoId` the song is known before its list is, so it does not wait for it:
   * the catalogue request (0.7-1.0 s on the add-on, the largest term of a start once
   * resolving took ~0.2 s) runs while the song already plays — see `#playLocalEarly`.
   */
  async playLocal({ playlistId, videoId = null }) {
    if (!this.#catalogue) {
      throw new Error('this receiver has no catalogue to build a queue from');
    }
    this.#limit?.reset();
    if (videoId) {
      return this.#playLocalEarly({ playlistId, videoId });
    }
    const page = await this.#catalogue.queue(playlistId, videoId);
    if (!page.items?.length) {
      throw new Error(`YouTube Music returned an empty queue for ${playlistId}`);
    }
    const start = videoId ? Math.max(0, page.items.findIndex((t) => t.video_id === videoId)) : 0;
    const queue = new LocalQueue({
      playlistId: page.playlist_id || playlistId,
      title: page.title ?? null,
      items: page.items,
      continuation: page.continuation ?? null,
      startIndex: start,
    });
    await this.#enterLocal(queue);
    this.#history?.record({ playlist_id: playlistId, title: queue.title,
      thumbnail: page.items[0]?.thumbnail ?? null, origin: 'local' });
    this.logger.info(
      `[MpvPlayer] playing ${playlistId}${queue.title ? ` ("${queue.title}")` : ''} from the receiver's own queue `
      + `(${queue.length} track(s) from #${start + 1})`);
    void this.#playLocalCurrent();
    return { playlist_id: queue.playlistId, title: queue.title, length: queue.length, index: queue.index };
  }

  /**
   * Play `videoId` now from a one-entry queue, and turn that into the list once the
   * catalogue answers. The answer is the same as `playLocal`'s, and arrives at the same
   * time; only the music no longer waits for it.
   *
   * If the catalogue fails, the song keeps playing as a queue of one — better than the old
   * behaviour of failing the request with nothing audible. If the song itself could not be
   * played by then, the list's next entry is started, as a failed local track always is.
   */
  async #playLocalEarly({ playlistId, videoId }) {
    const queue = new LocalQueue({ playlistId,
      items: [ { ...(this.#catalogue.track(videoId) ?? {}), video_id: videoId } ] });
    await this.#enterLocal(queue);
    const generation = this.#localGeneration;
    // Whether the first track is settled, and how. One that fails *after* the list arrived
    // needs nothing from here: #playLocalCurrentOrThrow skips forward through the adopted
    // list itself. Only a failure that came first, with a one-entry queue to skip in,
    // leaves the list unplayed.
    let firstOk = null;
    void this.#playLocalCurrent().then((ok) => {
      firstOk = ok;
    });
    let page = null;
    try {
      page = await this.#catalogue.queue(playlistId, videoId);
    }
    catch (e) {
      this.logger.warn(`[MpvPlayer] could not fetch ${playlistId} (${e.message}) — playing ${videoId} on its own`);
    }
    const current = () => this.#local === queue && this.#localGeneration === generation;
    if (page?.items?.length && current()) {
      queue.adopt(page.items, page.continuation ?? null);
      queue.title = page.title ?? null;
      if (page.playlist_id) {
        queue.playlistId = page.playlist_id;
      }
      if (firstOk === false) {
        if (queue.advance()) {
          void this.#playLocalCurrent();
        }
      }
      else {
        // The list did not exist when the first track started, so nothing was prefetched.
        this.#prefetchUpcoming();
      }
    }
    this.#history?.record({ playlist_id: playlistId, title: queue.title,
      thumbnail: page?.items?.[0]?.thumbnail ?? queue.current?.thumbnail ?? null, origin: 'local' });
    this.logger.info(
      `[MpvPlayer] playing ${playlistId}${queue.title ? ` ("${queue.title}")` : ''} from the receiver's own queue `
      + `(${queue.length} track(s) from #${queue.index + 1}, started before the list arrived)`);
    return { playlist_id: queue.playlistId, title: queue.title, length: queue.length, index: queue.index };
  }

  /** Jump to an entry of the local queue. False when there is no local queue or no such entry. */
  async jumpTo(index) {
    if (!this.#local?.jump(index)) {
      return false;
    }
    void this.#playLocalCurrent();
    return true;
  }

  async #enterLocal(queue) {
    this.#local = queue;
    this.#lastError = null;
    this.#localGeneration += 1;
    // Empty the cast queue, so a stale phone list cannot supply next/previous — and do
    // it through the same message the phone would send, so the library's bookkeeping
    // stays consistent. Our own change must not end the local queue it is making room for.
    this.#selfQueueUpdate = true;
    try {
      await this.queue.updateByMessage({ name: 'setPlaylist', payload: {} }, null);
    }
    catch (e) {
      this.logger.debug(`[MpvPlayer] could not clear the cast queue: ${e.message}`);
    }
    finally {
      this.#selfQueueUpdate = false;
    }
  }

  /**
   * Play the local queue's current entry, skipping forward past unplayable ones (at
   * most MAX_LOCAL_SKIPS in a row, so a dead jar cannot spin through a whole radio).
   */
  async #playLocalCurrent() {
    // Started without being awaited (the control API answers before the resolve), so it
    // must never reject: an unhandled rejection ends the process, and the cast session
    // with it.
    try {
      return await this.#playLocalCurrentOrThrow();
    }
    catch (e) {
      this.#lastError = e.message;
      this.logger.error(`[MpvPlayer] local playback failed: ${e.message}`);
      return false;
    }
  }

  async #playLocalCurrentOrThrow() {
    const generation = this.#localGeneration;
    for (let skips = 0; this.#local && skips <= MAX_LOCAL_SKIPS; skips++) {
      const track = this.#local.current;
      if (!track) {
        return false;
      }
      const ok = await this.play({ id: track.video_id, client: CLIENTS.YTMUSIC }, 0);
      if (ok || generation !== this.#localGeneration || !this.#local) {
        if (ok) {
          this.#lastError = null;
        }
        return ok;
      }
      this.#lastError = `could not play "${track.title ?? track.video_id}"`;
      await this.#refillLocal();
      if (!this.#local?.advance()) {
        break;
      }
      this.logger.warn(`[MpvPlayer] ${this.#lastError} — skipping to the next track`);
    }
    return false;
  }

  async #refillLocal() {
    const queue = this.#local;
    if (!queue?.needsMore || !this.#catalogue?.available) {
      return;
    }
    const token = queue.continuation;
    queue.continuation = null; // one fetch at a time; restored by `append`
    try {
      const page = await this.#catalogue.continueQueue(queue.playlistId, token);
      queue.append(page.items, page.continuation);
      this.logger.debug(`[MpvPlayer] local queue now ${queue.length} track(s)`);
    }
    catch (e) {
      this.logger.warn(`[MpvPlayer] could not fetch more of ${queue.playlistId}: ${e.message}`);
    }
  }

  /** A phone changed its queue: it is in charge again, and its list is worth remembering. */
  #onCastQueue(event) {
    if (this.#selfQueueUpdate) {
      return;
    }
    // A phone choosing something is somebody there: the auto-stop counts from here.
    if (CHOICE_EVENTS.has(event?.type)) {
      this.#limit?.reset();
    }
    if (this.#local) {
      this.logger.info(`[MpvPlayer] a phone changed the queue (${event?.type}) — leaving the receiver's own queue`);
      this.#local = null;
      this.#localGeneration += 1;
    }
    const listId = this.queue.id;
    if (listId && (event?.type === 'playlistSet' || event?.type === 'playlistAdded')
      && this.#history?.record({ playlist_id: listId, origin: 'cast' })) {
      void this.#describeCast(listId);
    }
  }

  /** A cast names its list but not its title; ask once, so "recently played" can show it. */
  async #describeCast(listId) {
    const known = this.#history?.list().find((e) => e.playlist_id === listId);
    if (!this.#catalogue?.available || known?.title) {
      return;
    }
    try {
      const page = await this.#catalogue.queue(listId);
      this.#history.describe(listId, { title: page.title, thumbnail: page.items?.[0]?.thumbnail ?? null });
    }
    catch (e) {
      this.logger.debug(`[MpvPlayer] could not name cast list ${listId}: ${e.message}`);
    }
  }

  // --- transport, aware of the local queue --------------------------------------------

  async next(AID) {
    if (!this.#local) {
      return super.next(AID);
    }
    await this.#refillLocal();
    if (!this.#local?.advance()) {
      this.logger.info('[MpvPlayer] end of the receiver\'s own queue');
      await this.stop(AID);
      return false;
    }
    return this.#playLocalCurrent();
  }

  async previous(AID) {
    if (!this.#local) {
      return super.previous(AID);
    }
    if (!this.#local.back()) {
      // At the first entry "previous" restarts it, as every music app does.
      return this.seek(0, AID);
    }
    return this.#playLocalCurrent();
  }

  async resume(AID) {
    // After a stop (or a cast session reset) the base class has no current video to
    // restart; the local queue still knows where it was.
    if (this.#local && this.status !== PLAYER_STATUSES.PLAYING && this.status !== PLAYER_STATUSES.PAUSED) {
      return this.#playLocalCurrent();
    }
    return super.resume(AID);
  }

  /**
   * yt-cast-receiver resets the player when the last phone disconnects. For a cast that
   * is right — the phone's session is over. For the receiver's own queue it is not: a
   * phone whose app reconnected on its own and then went away must not stop what Home
   * Assistant started. So only the cast bookkeeping is reset then.
   */
  async reset(AID) {
    if (!this.#local) {
      return super.reset(AID);
    }
    this.logger.info('[MpvPlayer] cast session reset — the receiver\'s own queue keeps playing');
    this.#selfQueueUpdate = true;
    try {
      this.queue.reset();
    }
    finally {
      this.#selfQueueUpdate = false;
    }
    return undefined;
  }

  /** End local playback for good (the control API's stop keeps the queue; this does not). */
  async clearLocal() {
    this.#local = null;
    this.#localGeneration += 1;
    return this.stop();
  }

  /**
   * Everything the control API reports, in one read: transport state, the current
   * track, and the queue around it — the local one, or the phone's.
   */
  async describe({ queueAfter = 50, queueBefore = 5 } = {}) {
    const status = this.status;
    const active = status === PLAYER_STATUSES.PLAYING || status === PLAYER_STATUSES.PAUSED;
    const current = this.queue.current;
    const videoId = active || status === PLAYER_STATUSES.LOADING ? current?.id ?? null : null;
    const name = (id) => this.#catalogue?.track(id) ?? null;
    let queue;
    if (this.#local) {
      queue = {
        origin: 'local',
        playlist_id: this.#local.playlistId,
        title: this.#local.title,
        index: this.#local.index,
        length: this.#local.length,
        more: !!this.#local.continuation,
        can_next: this.#local.hasNext,
        can_previous: this.#local.hasPrevious,
        items: this.#local.window({ before: queueBefore, after: queueAfter }),
      };
    }
    else {
      const ids = this.queue.videoIds ?? [];
      const at = current?.id ? ids.indexOf(current.id) : -1;
      const start = Math.max(0, at - queueBefore);
      const window = ids.slice(start, at >= 0 ? at + queueAfter + 1 : queueAfter);
      void this.#catalogue?.hydrate(window);
      const nav = this.getNavInfo();
      queue = {
        origin: ids.length || current ? 'cast' : null,
        playlist_id: this.queue.id ?? null,
        title: this.queue.id ? this.#history?.list().find((e) => e.playlist_id === this.queue.id)?.title ?? null : null,
        index: at >= 0 ? at : null,
        length: ids.length,
        more: false,
        can_next: nav.hasNext || !!this.queue.autoplay,
        can_previous: nav.hasPrevious,
        items: window.map((id, i) => ({ ...(name(id) ?? { video_id: id }), video_id: id, index: start + i,
          current: start + i === at })),
      };
    }
    return {
      status: statusName(status),
      video_id: videoId,
      track: videoId ? { ...(name(videoId) ?? {}), video_id: videoId } : null,
      position: active ? await this.getPosition() : null,
      duration: active ? await this.getDuration() : null,
      queue,
      error: this.#lastError,
    };
  }

  async doPause() {
    return this.#mpv.setProperty('pause', true);
  }

  async doResume() {
    return this.#mpv.setProperty('pause', false);
  }

  async doStop() {
    // Clear rather than let the add-on's TTL collect it, so Home Assistant's media
    // card collapses instead of freezing on the last track.
    this.#metadata?.stopped();
    try {
      await this.#mpv.command('stop');
      return true;
    }
    catch (e) {
      this.logger.warn(`[MpvPlayer] stop failed: ${e.message}`);
      return false;
    }
  }

  async doSeek(position) {
    try {
      await this.#mpv.command('seek', position, 'absolute');
      return true;
    }
    catch (e) {
      this.logger.warn(`[MpvPlayer] seek to ${position}s failed: ${e.message}`);
      return false;
    }
  }

  async doSetVolume(volume) {
    // Keep the phone's slider meaningful by driving mpv, and leave the router's
    // per-output volume as the separate fan-out control. The two compose.
    const okLevel = await this.#mpv.setProperty('volume', volume.level);
    const okMute = await this.#mpv.setProperty('mute', volume.muted);
    if (okLevel && okMute) {
      this.#volume = { level: volume.level, muted: volume.muted };
    }
    return okLevel && okMute;
  }

  async doGetVolume() {
    const level = await this.#mpv.getProperty('volume', this.#volume.level);
    const muted = await this.#mpv.getProperty('mute', this.#volume.muted);
    return { level: Math.round(level), muted: !!muted };
  }

  async doGetPosition() {
    // `time-pos` is the position of the *audio mpv has decoded*, so the phone's
    // progress bar runs slightly ahead of the sound: mpv's own output buffer, the
    // RTP jitter buffer and the output latency all sit downstream of here. That
    // drift is expected and deliberately not corrected.
    return (await this.#mpv.getProperty('time-pos', 0)) ?? 0;
  }

  async doGetDuration() {
    return (await this.#mpv.getProperty('duration', 0)) ?? 0;
  }
}

/** The base class's numeric status, as the word the control API speaks. */
export function statusName(status) {
  switch (status) {
    case PLAYER_STATUSES.PLAYING:
      return 'playing';
    case PLAYER_STATUSES.PAUSED:
      return 'paused';
    case PLAYER_STATUSES.LOADING:
      return 'loading';
    case PLAYER_STATUSES.STOPPED:
      return 'stopped';
    default:
      return 'idle';
  }
}
