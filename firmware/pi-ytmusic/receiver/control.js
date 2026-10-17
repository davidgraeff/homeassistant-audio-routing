/**
 * The receiver's control API: play, pause, skip, start a playlist and browse — without a
 * phone. What Home Assistant's `media_player` for this source talks to.
 *
 * WHY IT EXISTS
 * -------------
 * The phone's YouTube Music app was the only control surface, which is fine for whoever
 * holds the phone and useless for a wall display. Everything here goes through the same
 * `Player` methods the Lounge messages call (`player.js`), so a phone that is connected
 * sees every change the moment it happens — pause from Home Assistant and the phone shows
 * paused — and the phone stays in charge of anything *it* cast. What has no phone at all
 * (a playlist started from Home Assistant) plays from the receiver's own queue.
 *
 * HOW IT IS FOUND
 * ---------------
 * The receiver announces this API to the audio router (`POST /api/control/report`, keyed
 * by its RTP port, heartbeat every minute, withdrawn on shutdown); the router shows it on
 * the source as `control`, and the Home Assistant integration reads it from there. The
 * router never relays a command — it is only where the two meet.
 *
 * SECURITY
 * --------
 * No authentication, like the router's own API, so it binds **loopback by default**: in
 * the add-on, Home Assistant and the router share this host. On the Pi it is off unless
 * a port is configured, because there it would have to listen on the LAN. Nothing here
 * writes a credential; the worst a caller can do is play, skip or read the history.
 *
 * Wire format: JSON in and out; failures are `{error: "<sentence>"}` with a 4xx/5xx.
 *   GET  /api/state[?queue=N]          transport state, current track, the next N queue items
 *   POST /api/command {command[, position]}   play | pause | play_pause | stop | next | previous | seek | clear
 *   POST /api/play {media_id[, media_type]}   start a playlist / a track's radio / a queue entry
 *   GET  /api/browse?id=…[&lang=de]    one browse node with its children
 */

import { createServer } from 'http';
import { artworkUrl } from './metadata.js';

/** Bumped when the wire format changes incompatibly; the router passes it on. */
export const PROTOCOL = 1;
/** What this API can do, as announced. The integration maps these to HA features. */
export const FEATURES = [ 'play', 'pause', 'stop', 'next', 'previous', 'seek', 'play_media', 'browse_media', 'queue' ];
/** Re-announce this often; the router forgets an announcement after 180 s. */
const HEARTBEAT_MS = 60_000;
const ROUTER_TIMEOUT_MS = 3000;
/** A command body is a few dozen bytes. */
const MAX_BODY_BYTES = 16 * 1024;
/** Queue items in a state read unless asked otherwise — enough for a "next up" list. */
const DEFAULT_STATE_QUEUE = 10;
const MAX_STATE_QUEUE = 100;
/** How many recently played lists the browse tree offers. */
const RECENT_COUNT = 12;

const VIDEO_ID = /^[A-Za-z0-9_-]{11}$/;
const PLAYLIST_ID = /^[A-Za-z0-9_-]{2,80}$/;

/** Browse labels. The tree is the receiver's, so its words are too; HA passes its language. */
const LABELS = {
  en: { root: 'YouTube Music', queue: 'Queue', recent: 'Recently played', library: 'Library', history: 'History',
    mix: 'Mix', album: 'Album', playlist: 'Playlist' },
  de: { root: 'YouTube Music', queue: 'Warteschlange', recent: 'Zuletzt gespielt', library: 'Mediathek',
    history: 'Verlauf', mix: 'Mix', album: 'Album', playlist: 'Playlist' },
};

export const MEDIA_PREFIX = 'ytmusic:';
export const NODE = {
  root: 'ytmusic:root',
  queue: 'ytmusic:queue',
  recent: 'ytmusic:recent',
  library: 'ytmusic:library',
  history: 'ytmusic:history',
};

export const playlistMediaId = (playlistId, videoId = null) =>
  `${MEDIA_PREFIX}playlist:${playlistId}${videoId ? `:${videoId}` : ''}`;
export const trackMediaId = (videoId) => `${MEDIA_PREFIX}track:${videoId}`;
export const queueMediaId = (index) => `${MEDIA_PREFIX}queue:${index}`;

/**
 * What a `play_media` id asks for, or null when it means nothing here.
 *
 * Accepts this API's own ids (what browse hands out), and — for automations written by
 * hand — a music.youtube.com / youtube.com / youtu.be URL, or a bare id when the media
 * type says what it is.
 */
export function parseMediaId(mediaId, mediaType = null) {
  if (typeof mediaId !== 'string') {
    return null;
  }
  const id = mediaId.trim();
  if (id.startsWith(MEDIA_PREFIX)) {
    const [ kind, a, b ] = id.slice(MEDIA_PREFIX.length).split(':');
    if (kind === 'playlist' && PLAYLIST_ID.test(a ?? '') && (b === undefined || VIDEO_ID.test(b))) {
      return { kind: 'playlist', playlistId: a, videoId: b ?? null };
    }
    if (kind === 'track' && VIDEO_ID.test(a ?? '') && b === undefined) {
      return { kind: 'track', videoId: a };
    }
    if (kind === 'queue' && /^\d{1,4}$/.test(a ?? '') && b === undefined) {
      return { kind: 'queue', index: Number(a) };
    }
    return null;
  }
  if (/^https?:\/\//i.test(id)) {
    let url;
    try {
      url = new URL(id);
    }
    catch {
      return null;
    }
    const host = url.hostname.replace(/^(www|m|music)\./, '');
    if (host !== 'youtube.com' && host !== 'youtu.be') {
      return null;
    }
    const list = url.searchParams.get('list');
    const v = host === 'youtu.be' ? url.pathname.slice(1) : url.searchParams.get('v');
    if (list && PLAYLIST_ID.test(list)) {
      return { kind: 'playlist', playlistId: list, videoId: v && VIDEO_ID.test(v) ? v : null };
    }
    if (v && VIDEO_ID.test(v)) {
      return { kind: 'track', videoId: v };
    }
    return null;
  }
  const type = (mediaType ?? '').toLowerCase();
  if ((type === 'playlist' || type === 'album') && PLAYLIST_ID.test(id)) {
    return { kind: 'playlist', playlistId: id, videoId: null };
  }
  if ([ 'track', 'music', 'video', 'song' ].includes(type) && VIDEO_ID.test(id)) {
    return { kind: 'track', videoId: id };
  }
  return null;
}

/** One track as the wire shows it, from a catalogue/queue item. */
function trackView(t, { mediaId = null, playable = true } = {}) {
  return {
    media_id: mediaId ?? trackMediaId(t.video_id),
    video_id: t.video_id,
    title: t.title ?? null,
    artist: t.artist ?? null,
    album: t.album ?? null,
    duration: t.duration ?? null,
    artwork_url: t.thumbnail ?? artworkUrl(t.video_id),
    ...(t.index !== undefined ? { index: t.index, current: !!t.current } : {}),
    playable,
  };
}

function node(id, title, { kind = 'directory', playable = false, expandable = true, thumbnail = null,
  subtitle = null, children, ...extra } = {}) {
  return { id, title, subtitle, kind, playable, expandable, thumbnail,
    ...(children !== undefined ? { children } : {}), ...extra };
}

function listNode(entry, labels) {
  const kind = entry.kind ?? 'playlist';
  return node(playlistMediaId(entry.playlist_id), entry.title || labels[kind] || entry.playlist_id, {
    kind,
    playable: true,
    thumbnail: entry.thumbnail ?? null,
    subtitle: entry.subtitle ?? labels[kind] ?? null,
    ...(entry.played_at ? { played_at: entry.played_at } : {}),
  });
}

/** The control API's whole behaviour, separated from the HTTP plumbing so it can be tested. */
export class Controller {
  #player;
  #catalogue;
  #history;
  #sender;
  #info;

  constructor({ player, catalogue = null, history = null, sender = () => null, info = {} }) {
    this.#player = player;
    this.#catalogue = catalogue;
    this.#history = history;
    this.#sender = sender;
    this.#info = info;
  }

  async state({ queue = DEFAULT_STATE_QUEUE } = {}) {
    const after = Math.min(Math.max(0, Number(queue) || 0), MAX_STATE_QUEUE);
    const d = await this.#player.describe({ queueAfter: after, queueBefore: 0 });
    const local = d.queue.origin === 'local';
    const sender = this.#sender();
    return {
      protocol: PROTOCOL,
      name: this.#info.name ?? null,
      rtp_port: this.#info.rtpPort ?? null,
      features: FEATURES,
      state: d.status,
      origin: d.status === 'idle' ? null : d.queue.origin,
      cast: sender ? { connected: true, sender: sender.name ?? null, client: sender.client ?? null } : { connected: false },
      track: d.track ? {
        ...trackView(d.track),
        position: d.position,
        duration: d.duration || d.track.duration || null,
        position_at: Date.now(),
      } : null,
      playlist: d.queue.playlist_id ? {
        media_id: playlistMediaId(d.queue.playlist_id),
        playlist_id: d.queue.playlist_id,
        title: d.queue.title ?? null,
      } : null,
      queue: {
        index: d.queue.index,
        length: d.queue.length,
        more: d.queue.more,
        can_next: d.queue.can_next,
        can_previous: d.queue.can_previous,
        items: d.queue.items
          .filter((t) => !t.current)
          .map((t) => trackView(t, { mediaId: local ? queueMediaId(t.index) : trackMediaId(t.video_id), playable: local })),
      },
      catalogue: !!this.#catalogue?.available,
      error: d.error ?? null,
    };
  }

  /** One transport command. Returns whether the player accepted it. */
  async command(command, { position } = {}) {
    const p = this.#player;
    switch (command) {
      case 'play':
        return p.resume();
      case 'pause':
        return p.pause();
      case 'play_pause':
        return (await this.#player.describe({ queueAfter: 0 })).status === 'playing' ? p.pause() : p.resume();
      case 'stop':
        return p.stop();
      case 'next':
        return p.next();
      case 'previous':
        return p.previous();
      case 'seek': {
        const to = Number(position);
        if (!Number.isFinite(to) || to < 0) {
          throw new RequestError(400, 'seek needs a position in seconds');
        }
        return p.seek(to);
      }
      case 'clear':
        return p.clearLocal();
      default:
        throw new RequestError(400, `unknown command '${command}'`);
    }
  }

  async play(mediaId, mediaType = null) {
    const what = parseMediaId(mediaId, mediaType);
    if (!what) {
      throw new RequestError(400, `not something this receiver can play: '${mediaId}'`);
    }
    if (what.kind === 'queue') {
      if (!(await this.#player.jumpTo(what.index))) {
        throw new RequestError(409, `there is no entry ${what.index} in the receiver's own queue`);
      }
      return { media_id: queueMediaId(what.index) };
    }
    if (!this.#catalogue?.available) {
      throw new RequestError(503, 'YouTube Music cannot be asked right now (the resolver daemon is not running)');
    }
    // A single song plays the way YouTube Music plays a tapped song: as the first track
    // of its radio, so the music does not stop after one track.
    const request = what.kind === 'track'
      ? { playlistId: `RDAMVM${what.videoId}`, videoId: what.videoId }
      : { playlistId: what.playlistId, videoId: what.videoId };
    try {
      return { media_id: mediaId, ...(await this.#player.playLocal(request)) };
    }
    catch (e) {
      throw new RequestError(502, e.message);
    }
  }

  async browse(id, lang = 'en') {
    const labels = LABELS[(lang ?? 'en').slice(0, 2).toLowerCase()] ?? LABELS.en;
    const nodeId = id || NODE.root;
    if (nodeId === NODE.root) {
      const children = [];
      const d = await this.#player.describe({ queueAfter: 0 });
      if (d.queue.length) {
        children.push(node(NODE.queue, labels.queue, { subtitle: d.queue.title ?? null }));
      }
      children.push(node(NODE.recent, labels.recent));
      if (this.#catalogue?.available) {
        children.push(node(NODE.library, labels.library), node(NODE.history, labels.history));
      }
      return node(NODE.root, labels.root, { children });
    }
    if (nodeId === NODE.queue) {
      const d = await this.#player.describe({ queueAfter: MAX_STATE_QUEUE, queueBefore: 0 });
      const local = d.queue.origin === 'local';
      return node(NODE.queue, labels.queue, {
        subtitle: d.queue.title ?? null,
        children: d.queue.items.map((t) => {
          const v = trackView(t, { mediaId: local ? queueMediaId(t.index) : trackMediaId(t.video_id), playable: local });
          return node(v.media_id, v.title ?? v.video_id, { kind: 'track', playable: local, expandable: false,
            thumbnail: v.artwork_url, subtitle: v.artist, current: !!t.current });
        }),
      });
    }
    if (nodeId === NODE.recent) {
      return node(NODE.recent, labels.recent, {
        children: (this.#history?.list(RECENT_COUNT) ?? []).map((e) => listNode(e, labels)),
      });
    }
    if (nodeId === NODE.library) {
      let lib = await this.#catalogue?.library('library') ?? { items: [] };
      if (!lib.items.length && lib.signed_in) {
        // The landing page's layout is the one most likely to move; the plain playlist
        // list is the steadier fallback.
        lib = await this.#catalogue.library('playlists');
      }
      return node(NODE.library, labels.library, {
        children: lib.items.map((e) => listNode(e, labels)),
        ...(lib.items.length ? {} : { error: lib.error ?? (lib.signed_in ? null : 'not signed in') }),
      });
    }
    if (nodeId === NODE.history) {
      const hist = await this.#catalogue?.library('history') ?? { items: [] };
      return node(NODE.history, labels.history, {
        children: hist.items.map((t) => {
          const v = trackView(t);
          return node(v.media_id, v.title ?? v.video_id, { kind: 'track', playable: true, expandable: false,
            thumbnail: v.artwork_url, subtitle: v.artist });
        }),
        ...(hist.items.length ? {} : { error: hist.error ?? (hist.signed_in ? null : 'not signed in') }),
      });
    }
    const what = parseMediaId(nodeId);
    if (what?.kind === 'playlist') {
      if (!this.#catalogue?.available) {
        throw new RequestError(503, 'YouTube Music cannot be asked right now (the resolver daemon is not running)');
      }
      let page;
      try {
        page = await this.#catalogue.queue(what.playlistId);
      }
      catch (e) {
        throw new RequestError(502, e.message);
      }
      const known = this.#history?.list().find((e) => e.playlist_id === what.playlistId);
      const kind = known?.kind ?? (what.playlistId.startsWith('RD') ? 'mix' : 'playlist');
      return node(playlistMediaId(what.playlistId), page.title ?? known?.title ?? what.playlistId, {
        kind,
        playable: true,
        thumbnail: page.items?.[0]?.thumbnail ?? known?.thumbnail ?? null,
        children: (page.items ?? []).map((t) => {
          const v = trackView(t, { mediaId: playlistMediaId(what.playlistId, t.video_id) });
          return node(v.media_id, v.title ?? v.video_id, { kind: 'track', playable: true, expandable: false,
            thumbnail: v.artwork_url, subtitle: v.artist });
        }),
      });
    }
    if (what?.kind === 'track') {
      const t = this.#catalogue?.track(what.videoId) ?? { video_id: what.videoId };
      const v = trackView(t);
      return node(v.media_id, v.title ?? v.video_id, { kind: 'track', playable: true, expandable: false,
        thumbnail: v.artwork_url, subtitle: v.artist, children: [] });
    }
    throw new RequestError(404, `no browse node '${nodeId}'`);
  }
}

export class RequestError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

function json(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(payload),
    'Cache-Control': 'no-store',
  });
  res.end(payload);
}

async function readJson(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_BODY_BYTES) {
      throw new RequestError(413, `body larger than ${MAX_BODY_BYTES} bytes`);
    }
    chunks.push(chunk);
  }
  const text = Buffer.concat(chunks).toString('utf8');
  if (!text) {
    return {};
  }
  try {
    const body = JSON.parse(text);
    if (body && typeof body === 'object' && !Array.isArray(body)) {
      return body;
    }
  }
  catch { /* fall through */ }
  throw new RequestError(400, 'body must be a JSON object');
}

/**
 * Serve the control API and announce it to the router. Returns `{close()}`, or null when
 * no port is configured.
 */
export default function startControl({ port, bind = '127.0.0.1', advertisedUrl, controller, router = null,
  logger = console }) {
  if (!port) {
    return null;
  }
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost');
    try {
      if (req.method === 'GET' && url.pathname === '/api/state') {
        return json(res, 200, await controller.state({ queue: url.searchParams.get('queue') ?? undefined }));
      }
      if (req.method === 'POST' && url.pathname === '/api/command') {
        const body = await readJson(req);
        const accepted = await controller.command(String(body.command ?? ''), body);
        return json(res, 200, { accepted: accepted !== false, state: await controller.state({ queue: 0 }) });
      }
      if (req.method === 'POST' && url.pathname === '/api/play') {
        const body = await readJson(req);
        return json(res, 200, await controller.play(body.media_id, body.media_type ?? null));
      }
      if (req.method === 'GET' && url.pathname === '/api/browse') {
        return json(res, 200, await controller.browse(url.searchParams.get('id'), url.searchParams.get('lang')));
      }
      return json(res, 404, { error: `no route ${req.method} ${url.pathname}` });
    }
    catch (e) {
      const status = e instanceof RequestError ? e.status : 500;
      if (status >= 500) {
        logger.warn(`[control] ${req.method} ${url.pathname} failed: ${e.message}`);
      }
      return json(res, status, { error: e.message });
    }
  });
  server.on('error', (e) => logger.error(`[control] could not serve on ${bind}:${port}: ${e.message}`));
  server.listen(port, bind, () => logger.info(`[control] control API on http://${bind}:${port} (announced as ${advertisedUrl})`));

  const announcer = router ? new Announcer({ ...router, url: advertisedUrl, logger }) : null;
  announcer?.start();
  return {
    async close() {
      await announcer?.stop();
      server.close();
    },
  };
}

/**
 * Keeps the router told where this API is: one announcement a minute, a withdrawal on
 * the way out. A `404` (no RTP source on our port yet) is expected before setup and is
 * logged once, not every minute.
 */
export class Announcer {
  #endpoint;
  #body;
  #logger;
  #timer = null;
  #lastOutcome = null;

  constructor({ host, apiPort, rtpPort, url, logger = console }) {
    this.#endpoint = `http://${host}:${apiPort}/api/control/report`;
    this.#body = { rtp_port: rtpPort, url, features: FEATURES, protocol: PROTOCOL };
    this.#logger = logger;
  }

  start() {
    void this.announce();
    this.#timer = setInterval(() => void this.announce(), HEARTBEAT_MS);
    this.#timer.unref?.();
  }

  async stop() {
    clearInterval(this.#timer);
    this.#timer = null;
    await this.#post({ ...this.#body, url: null });
  }

  async announce() {
    const outcome = await this.#post(this.#body);
    if (outcome !== this.#lastOutcome) {
      this.#lastOutcome = outcome;
      if (outcome === 'ok') {
        this.#logger.info(`[control] announced ${this.#body.url} to the audio router`);
      }
      else if (outcome === 404) {
        this.#logger.warn(`[control] the audio router has no RTP source on port ${this.#body.rtp_port} yet — `
          + 'Home Assistant cannot find the control API until it does');
      }
      else {
        this.#logger.warn(`[control] could not announce to the audio router (${outcome}); retrying every minute`);
      }
    }
    return outcome;
  }

  async #post(body) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), ROUTER_TIMEOUT_MS);
    try {
      const res = await fetch(this.#endpoint, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      await res.arrayBuffer().catch(() => {});
      return res.ok ? 'ok' : res.status;
    }
    catch (e) {
      return e.name === 'AbortError' ? 'timeout' : e.message;
    }
    finally {
      clearTimeout(timer);
    }
  }
}
