/**
 * Auto-stop: the most this receiver plays without a break, before it stops by itself.
 *
 * WHY THIS EXISTS
 * ---------------
 * A radio never ends and a long playlist ends after hours, so music started and forgotten
 * — the phone in a pocket, Home Assistant's play button pressed on the way out — plays to
 * an empty room until somebody notices. This puts an upper bound on that.
 *
 * WHAT COUNTS
 * -----------
 * Only time spent *playing*. A pause or stop of BREAK_MS or more is a break and starts the
 * count again, and so does somebody starting something on purpose (`reset`: a play request
 * from Home Assistant, a phone changing the queue) — the limit is for unattended playback,
 * and both are proof of attendance. A song ending and the next one starting is not.
 *
 * Once the limit is reached the current song plays to its end and the queue then stops
 * rather than advance (`reached`); a song still going GRACE_MS past the limit — an hour-
 * long mix — is stopped where it is (`overrun`). The player asks both; this only counts.
 *
 * The limit itself is a setting of the admin page, kept in a small JSON file in the state
 * directory so it survives restarts.
 */

import fs from 'fs';
import path from 'path';

/** The admin page's default: three hours. */
export const DEFAULT_MAX_MINUTES = 180;
/** A day; anything longer is not a limit. 0 means no limit at all. */
export const MAX_MAX_MINUTES = 24 * 60;
/** Not playing for this long is a break: the count starts again. */
const BREAK_MS = 10 * 60 * 1000;
/** How far past the limit a single song may run before it is cut. */
const GRACE_MS = 15 * 60 * 1000;
/** A gap between samples longer than this is not counted as playing (a suspended host). */
const MAX_SAMPLE_GAP_MS = 2 * 60 * 1000;

export default class PlayLimit {
  #file;
  #logger;
  #maxMinutes = DEFAULT_MAX_MINUTES;
  /** Milliseconds of playing since the last break or reset. */
  #playedMs = 0;
  /** When the last sample was taken, and whether it saw playback. */
  #lastSample = null;
  /** When playback was last seen, for the break rule. */
  #lastPlayingAt = null;

  constructor({ file, logger = console }) {
    this.#file = file;
    this.#logger = logger;
  }

  setLogger(logger) {
    this.#logger = logger;
  }

  /** Read the saved limit, if there is one. A missing or broken file means the default. */
  load() {
    try {
      const saved = JSON.parse(fs.readFileSync(this.#file, 'utf8'));
      if (validMinutes(saved.max_continuous_play_minutes)) {
        this.#maxMinutes = saved.max_continuous_play_minutes;
      }
    }
    catch (e) {
      if (e.code !== 'ENOENT') {
        this.#logger.warn(`[playlimit] could not read ${this.#file} (${e.message}) — using ${DEFAULT_MAX_MINUTES} min`);
      }
    }
    return this;
  }

  get maxMinutes() {
    return this.#maxMinutes;
  }

  /** Change and save the limit. Throws with a sentence for a value that is not one. */
  setMaxMinutes(minutes) {
    if (!validMinutes(minutes)) {
      throw new Error(`the limit must be a whole number of minutes from 0 (never stop) to ${MAX_MAX_MINUTES}`);
    }
    this.#maxMinutes = minutes;
    const tmp = `${this.#file}.tmp`;
    fs.mkdirSync(path.dirname(this.#file), { recursive: true });
    fs.writeFileSync(tmp, `${JSON.stringify({ max_continuous_play_minutes: minutes }, null, 2)}\n`);
    fs.renameSync(tmp, this.#file);
    this.#logger.info(`[playlimit] maximum continuous play set to ${minutes ? `${minutes} min` : 'unlimited'}`);
  }

  /** Called regularly with whether the player is playing right now. */
  sample(playing, now = Date.now()) {
    if (this.#lastSample?.playing && playing) {
      this.#playedMs += Math.min(now - this.#lastSample.at, MAX_SAMPLE_GAP_MS);
    }
    if (playing) {
      this.#lastPlayingAt = now;
    }
    else if (this.#lastPlayingAt !== null && now - this.#lastPlayingAt >= BREAK_MS) {
      this.#playedMs = 0;
      this.#lastPlayingAt = null;
    }
    this.#lastSample = { at: now, playing };
  }

  /** Somebody started something on purpose: count from here. */
  reset() {
    this.#playedMs = 0;
    this.#lastPlayingAt = null;
    this.#lastSample = this.#lastSample ? { ...this.#lastSample, playing: false } : null;
  }

  get playedMs() {
    return this.#playedMs;
  }

  /** The limit is reached: let the current song end, then stop instead of advancing. */
  get reached() {
    return this.#maxMinutes > 0 && this.#playedMs >= this.#maxMinutes * 60 * 1000;
  }

  /** The current song has run GRACE_MS past the limit: stop it now. */
  get overrun() {
    return this.#maxMinutes > 0 && this.#playedMs >= this.#maxMinutes * 60 * 1000 + GRACE_MS;
  }

  /** For the admin page and the log. */
  describe() {
    return {
      max_minutes: this.#maxMinutes,
      played_minutes: Math.floor(this.#playedMs / 60000),
      reached: this.reached,
      break_minutes: BREAK_MS / 60000,
      grace_minutes: GRACE_MS / 60000,
    };
  }
}

function validMinutes(v) {
  return Number.isInteger(v) && v >= 0 && v <= MAX_MAX_MINUTES;
}
