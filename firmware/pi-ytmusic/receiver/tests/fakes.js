/**
 * Test doubles shared by the receiver's tests: an mpv that "plays" instantly, a
 * catalogue that answers from a table, and a logger that keeps quiet.
 */

import EventEmitter from 'events';

export const quietLogger = { debug() {}, info() {}, warn() {}, error() {} };

/** Just enough of MpvClient: loadfile → file-loaded + playback-restart, stop → end-file. */
export class FakeMpv extends EventEmitter {
  loaded = [];
  props = { volume: 100, mute: false, 'time-pos': 12.5, duration: 200 };

  async command(name, ...args) {
    settle.activity += 1;
    if (name === 'loadfile') {
      if (this.loaded.length && this.playing) {
        this.emit('end-file', { reason: 'stop' });
      }
      this.loaded.push(args[0]);
      this.playing = true;
      setImmediate(() => {
        this.emit('file-loaded', {});
        this.emit('playback-restart', {});
      });
    }
    else if (name === 'stop') {
      this.playing = false;
      setImmediate(() => this.emit('end-file', { reason: 'stop' }));
    }
    return null;
  }

  async getProperty(name, fallback = null) {
    return this.props[name] ?? fallback;
  }

  async setProperty(name, value) {
    settle.activity += 1;
    this.props[name] = value;
    return true;
  }

  observeProperty() {
    return 1;
  }

  /** The current file reached its end. */
  finishTrack() {
    this.playing = false;
    this.emit('end-file', { reason: 'eof' });
  }
}

export function track(n, extra = {}) {
  const id = `vid${String(n).padStart(8, '0')}`;
  return { video_id: id, title: `Song ${n}`, artist: `Artist ${n}`, album: null, duration: 180,
    thumbnail: `https://i.ytimg.com/vi/${id}/hq720.jpg`, ...extra };
}

/** A catalogue answering from `pages` (playlistId -> page) and `continuations`. */
export class FakeCatalogue {
  available = true;
  asked = [];
  #tracks = new Map();

  constructor({ pages = {}, continuations = {} } = {}) {
    this.pages = pages;
    this.continuations = continuations;
  }

  track(id) {
    return this.#tracks.get(id) ?? null;
  }

  remember(items) {
    for (const t of items) {
      this.#tracks.set(t.video_id, t);
    }
  }

  async hydrate() {
    return [];
  }

  /** Set to a promise to hold `queue()` answers until it settles — a slow catalogue. */
  hold = null;

  async queue(playlistId, videoId = null) {
    this.asked.push({ playlistId, videoId });
    await this.hold;
    const page = this.pages[playlistId];
    if (!page) {
      throw new Error(`no such list ${playlistId}`);
    }
    this.remember(page.items);
    return structuredClone(page);
  }

  async continueQueue(playlistId, token) {
    this.asked.push({ playlistId, token });
    const page = this.continuations[token] ?? { items: [], continuation: null };
    this.remember(page.items);
    return structuredClone(page);
  }

  async library() {
    return { signed_in: false, items: [] };
  }
}

/**
 * Let the player's background work run: playback is started without being awaited, the
 * way the control API starts it. Waits until mpv has been idle for a few ticks.
 */
export async function settle() {
  for (let quiet = 0; quiet < 5;) {
    const before = settle.activity;
    await new Promise((r) => setTimeout(r, 10));
    quiet = settle.activity === before ? quiet + 1 : 0;
  }
}
settle.activity = 0;
