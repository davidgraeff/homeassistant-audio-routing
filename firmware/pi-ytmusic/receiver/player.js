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
 */

import { Player } from 'yt-cast-receiver';

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

  constructor(mpv, metadata = null, resolver = null) {
    super();
    this.#mpv = mpv;
    this.#metadata = metadata;
    this.#resolver = resolver;

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
          await this.pause();
          await this.next();
        }
        catch (e) {
          this.logger.error(`[MpvPlayer] failed to advance queue: ${e.message}`);
        }
      })();
    });
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
    const wasCached = !!this.#resolver?.peek(video.id);
    this.logger.info(
      `[MpvPlayer] play ${video.id} from ${position}s${wasCached ? ' (prefetched)' : ''}`);
    const direct = await this.#resolver?.urlFor(video.id) ?? null;
    if (!direct && this.#resolver?.enabled) {
      this.logger.warn(`[MpvPlayer] falling back to mpv's own resolution for ${video.id}`);
    }
    const prefetched = !!direct;
    const resolvedAt = Date.now();
    const url = direct?.url ?? watchUrl;
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
    // resolving the title (metadata.js).
    this.#metadata?.trackStarted(video.id);
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

    const ok = await started;
    this.logger.info(
      `[MpvPlayer] ${video.id} ${ok ? 'loaded' : 'failed'} after ${since(startedAt)}s `
      + `(resolve ${((resolvedAt - startedAt) / 1000).toFixed(1)}s, mpv ${since(resolvedAt)}s)`);
    if (!ok) {
      this.#mpv.off('playback-restart', onRestart);
    }
    if (!ok && prefetched && this.#retriedFor !== video.id) {
      // The direct URL was accepted by `loadfile` but did not play — most likely
      // expired or rejected. Drop it and try once more via mpv's own resolution,
      // rather than reporting a dead track to the phone.
      //
      // The `#retriedFor` guard is load-bearing: without it, a fresh resolve is
      // also a "direct" URL, so a URL that consistently fails to play would
      // recurse forever.
      this.logger.warn(`[MpvPlayer] direct URL for ${video.id} did not play — retrying once`);
      this.#retriedFor = video.id;
      this.#resolver?.invalidate(video.id);
      return this.doPlay(video, position);
    }
    this.#retriedFor = null;
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
    if (!this.#resolver?.enabled) {
      return;
    }
    try {
      const next = this.queue?.next ?? this.queue?.autoplay ?? null;
      if (next?.id) {
        this.#resolver.prefetch(next.id);
      }
    }
    catch (e) {
      this.logger.debug(`[MpvPlayer] could not look up the next track: ${e.message}`);
    }
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
