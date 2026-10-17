/**
 * Resolves a video id to a direct audio URL with **one** InnerTube request, the way
 * NewPipe does.
 *
 * WHY THIS EXISTS
 * ---------------
 * yt-dlp's default clients answer with URLs that carry a JS challenge (`n`, sometimes
 * `sig`), so a resolve has to fetch the watch page, the player JS and several `/player`
 * responses and then solve the challenge: ~5.4 s per process on the add-on host, ~2.6 s
 * of it pages that playback never reads. The `VISIONOS` client (YouTube's Apple Vision Pro
 * app) answers `/player` with plain `url` fields — no `signatureCipher`, no `n` — and asks
 * for no PO token. Measured from the add-on host: ~110 ms for `/player`, opus itag 251
 * offered, and every URL played, including a video whose URLs from every yt-dlp client
 * were answering 403 at the time.
 *
 * The request mirrors NewPipeExtractor (`ClientsConstants.java`,
 * `YoutubeStreamHelper.getVisionOsPlayerResponse`): a `visitor_id` request for the
 * visitorData the client must present, then `/player` on `youtubei.googleapis.com`.
 * yt-dlp's own `visionos` client is *not* equivalent — with its older version string and
 * a desktop Safari User-Agent it was answered "Sign in to confirm you're not a bot" for
 * the same videos this request played seconds later.
 *
 * WHAT IT CANNOT DO
 * -----------------
 * It is anonymous: no cookie jar, so no Premium-only formats, and videos made for kids or
 * age-restricted ones come back unplayable. It is also a client YouTube may stop serving
 * at any time. Both are why `resolver.js` treats this as a first attempt in front of the
 * yt-dlp ladder, never as a replacement for it — every failure here is a fall-through.
 */

import { randomInt } from 'crypto';

/** NewPipeExtractor's values (dev, 2026-10). The app version is what YouTube checks. */
const CLIENT_NAME = 'VISIONOS';
const CLIENT_VERSION = '1.04';
const DEVICE_MODEL = 'RealityDevice17,1';
const OS_VERSION = '26.6.0.23O770';
const UA_OS_VERSION = '26_6_0';

const VISITOR_URL = 'https://www.youtube.com/youtubei/v1/visitor_id?prettyPrint=false';
const PLAYER_URL = 'https://youtubei.googleapis.com/youtubei/v1/player';
/**
 * How long one visitorData is reused. NewPipe fetches one per extraction; reusing it saves
 * a round trip per track, and any failure drops it, so a stale one costs one fall-through.
 */
const VISITOR_MAX_AGE_MS = 6 * 60 * 60 * 1000;
/** Per request. Both answer in ~100 ms; anything near this is better spent on yt-dlp. */
const REQUEST_TIMEOUT_MS = 5000;
/** Alphabet of the `cpn` and `t` nonces (YouTube's content-playback-nonce alphabet). */
const NONCE_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';
/** Opus, audio-only, 48 kHz: the router's rate, so nothing resamples on the way. */
const PREFERRED_ITAG = 251;

export default class VisionOsResolver {
  #country;
  #language;
  #logger;
  /** { data, at } — see VISITOR_MAX_AGE_MS. */
  #visitor = null;

  constructor({ country = 'US', language = 'en', logger = console } = {}) {
    this.#country = country;
    this.#language = language;
    this.#logger = logger;
  }

  setLogger(logger) {
    this.#logger = logger;
  }

  /**
   * The User-Agent the URL has to be fetched with. Part of the answer, exactly as the
   * headers are for a yt-dlp resolve: `#verify` and mpv both send it.
   */
  get userAgent() {
    return `com.google.visionos.youtube/${CLIENT_VERSION}(${DEVICE_MODEL}; U; CPU visionOS `
      + `${UA_OS_VERSION} like Mac OS X; ${this.#country})`;
  }

  /**
   * `videoId` -> `{ url, headers, itag }`. Throws whenever the answer is not a plain,
   * audio-only URL; the caller falls back to yt-dlp.
   */
  async resolve(videoId) {
    try {
      const visitorData = await this.#visitorData();
      const response = await this.#post(
        `${PLAYER_URL}?prettyPrint=false&t=${nonce(12)}&id=${encodeURIComponent(videoId)}`,
        { ...this.#context(visitorData), videoId, cpn: nonce(16),
          contentCheckOk: true, racyCheckOk: true });
      const playability = response.playabilityStatus ?? {};
      if (playability.status !== 'OK') {
        throw new Error(`${playability.status ?? 'no status'}: ${playability.reason ?? 'no reason given'}`);
      }
      const format = pickAudio(response.streamingData?.adaptiveFormats ?? []);
      if (!format) {
        throw new Error('no plain audio-only format in the response');
      }
      return { url: format.url, headers: { 'User-Agent': this.userAgent }, itag: format.itag };
    }
    catch (e) {
      // Whatever went wrong, the next attempt starts from a fresh visitor: it costs
      // ~100 ms, and a visitor YouTube has started to distrust is the likeliest cause
      // that this side can do anything about.
      this.#visitor = null;
      throw e;
    }
  }

  async #visitorData() {
    if (this.#visitor && Date.now() - this.#visitor.at < VISITOR_MAX_AGE_MS) {
      return this.#visitor.data;
    }
    const data = (await this.#post(VISITOR_URL, this.#context(null))).responseContext?.visitorData;
    if (!data) {
      throw new Error('visitor_id answered without visitorData');
    }
    this.#visitor = { data, at: Date.now() };
    this.#logger.debug('[visionos] new visitorData');
    return data;
  }

  /** The request context NewPipe sends (`prepareJsonBuilder`), field for field. */
  #context(visitorData) {
    return {
      context: {
        client: {
          clientName: CLIENT_NAME,
          clientVersion: CLIENT_VERSION,
          clientScreen: 'WATCH',
          platform: 'MOBILE',
          ...(visitorData ? { visitorData } : {}),
          deviceMake: 'Apple',
          deviceModel: DEVICE_MODEL,
          osName: 'visionOS',
          osVersion: OS_VERSION,
          hl: this.#language,
          gl: this.#country,
          utcOffsetMinutes: 0,
        },
        request: { internalExperimentFlags: [], useSsl: true },
        user: { lockedSafetyMode: false },
      },
    };
  }

  async #post(url, body) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
    try {
      const res = await fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'User-Agent': this.userAgent,
          'X-Goog-Api-Format-Version': '2' },
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      if (!res.ok) {
        await res.arrayBuffer().catch(() => {});
        throw new Error(`${new URL(url).pathname} answered HTTP ${res.status}`);
      }
      return await res.json();
    }
    finally {
      clearTimeout(timer);
    }
  }
}

/**
 * The format to play: audio-only, with a plain URL that needs no challenge solved.
 *
 * itag 251 first, then the highest bitrate. A dubbed video lists one format per audio
 * track, so only the original (`audioIsDefault`) track is considered when tracks are
 * marked, and dynamic-range-compressed (`isDrc`) variants are passed over — both would
 * otherwise win on bitrate alone and play the wrong thing.
 */
export function pickAudio(formats) {
  let candidates = formats.filter((f) => f.mimeType?.startsWith('audio/') && f.url
    && !f.signatureCipher && !f.isDrc && !/[?&]n=/.test(f.url));
  if (candidates.some((f) => f.audioTrack)) {
    candidates = candidates.filter((f) => !f.audioTrack || f.audioTrack.audioIsDefault);
  }
  candidates.sort((a, b) => ((b.itag === PREFERRED_ITAG) - (a.itag === PREFERRED_ITAG))
    || ((b.bitrate ?? 0) - (a.bitrate ?? 0)));
  return candidates[0] ?? null;
}

function nonce(length) {
  let out = '';
  for (let i = 0; i < length; i++) {
    out += NONCE_ALPHABET[randomInt(NONCE_ALPHABET.length)];
  }
  return out;
}
