// Resolver's attempt order — the VISIONOS request first, the yt-dlp ladder behind it — over
// a stubbed `fetch` (InnerTube and googlevideo alike) and a fake daemon. No network.

import assert from 'node:assert/strict';
import { afterEach, beforeEach, test } from 'node:test';
import Resolver from '../resolver.js';
import VisionOsResolver, { pickAudio } from '../visionos.js';
import { quietLogger } from './fakes.js';

const VISIONOS_URL = 'https://rr1.googlevideo.example/videoplayback?itag=251&c=VISIONOS&expire=9999999999';
const DAEMON_URL = 'https://rr2.googlevideo.example/videoplayback?itag=251&c=WEB&expire=9999999999';

const audio = (itag, bitrate, extra = {}) => ({ itag, bitrate, mimeType: itag === 140
  ? 'audio/mp4; codecs="mp4a.40.2"' : 'audio/webm; codecs="opus"', url: `${VISIONOS_URL}&i=${itag}`, ...extra });

let calls;
let player;
let streamStatus;
let realFetch;
let daemon;

beforeEach(() => {
  calls = [];
  streamStatus = {};
  player = () => ({ playabilityStatus: { status: 'OK' },
    streamingData: { adaptiveFormats: [ audio(140, 130000), { ...audio(251, 140000), url: VISIONOS_URL } ] } });
  realFetch = globalThis.fetch;
  globalThis.fetch = async (url, init = {}) => {
    const u = new URL(url);
    calls.push({ url: u, init });
    if (u.pathname.endsWith('/visitor_id')) {
      return Response.json({ responseContext: { visitorData: `V${calls.length}` } });
    }
    if (u.pathname.endsWith('/player')) {
      return Response.json(player(JSON.parse(init.body)));
    }
    return new Response('ab', { status: streamStatus[u.host] ?? 206 });
  };
  daemon = { running: true, resolved: [], async resolve(videoId, mode) {
    this.resolved.push(mode);
    return { url: DAEMON_URL, headers: { 'User-Agent': 'yt-dlp UA' }, elapsed_ms: 1, cold: false };
  }, invalidate() {}, diagnose() {}, async waitUntilRunning() { return true; } };
});

afterEach(() => {
  globalThis.fetch = realFetch;
});

function resolver(extra = {}) {
  const r = new Resolver({ ytdlp: '/nonexistent/yt-dlp', jsRuntime: 'node', logger: quietLogger,
    fast: new VisionOsResolver({ logger: quietLogger }), ...extra });
  r.setDaemon(daemon);
  return r;
}

const playerCalls = () => calls.filter((c) => c.url.pathname.endsWith('/player'));
const visitorCalls = () => calls.filter((c) => c.url.pathname.endsWith('/visitor_id'));

test('the VISIONOS request answers, and yt-dlp is never asked', async () => {
  const got = await resolver().resolve('vid00000001');
  assert.equal(got.url, VISIONOS_URL);
  assert.match(got.headers['User-Agent'], /^com\.google\.visionos\.youtube\/1\.04\(RealityDevice17,1;/);
  assert.deepEqual(daemon.resolved, []);
  const [ req ] = playerCalls();
  assert.equal(req.url.host, 'youtubei.googleapis.com');
  assert.equal(req.url.searchParams.get('id'), 'vid00000001');
  const body = JSON.parse(req.init.body);
  assert.equal(body.context.client.clientName, 'VISIONOS');
  assert.equal(body.context.client.visitorData, 'V1');
  assert.equal(body.videoId, 'vid00000001');
  // The stream URL was verified with the same User-Agent mpv will send.
  const probe = calls.find((c) => c.url.host === 'rr1.googlevideo.example');
  assert.equal(probe.init.headers['User-Agent'], got.headers['User-Agent']);
});

test('one visitorData serves many tracks, and a failure throws it away', async () => {
  const r = resolver();
  await r.resolve('vid00000001');
  await r.resolve('vid00000002');
  assert.equal(visitorCalls().length, 1);

  player = () => ({ playabilityStatus: { status: 'LOGIN_REQUIRED', reason: 'Sign in to confirm your age' } });
  assert.equal((await r.resolve('vid00000003')).url, DAEMON_URL, 'yt-dlp took over');
  assert.deepEqual(daemon.resolved, [ 'local' ]);

  player = () => ({ playabilityStatus: { status: 'OK' },
    streamingData: { adaptiveFormats: [ { ...audio(251, 140000), url: VISIONOS_URL } ] } });
  await r.resolve('vid00000004');
  assert.equal(visitorCalls().length, 2, 'a fresh visitor after the failure');
});

test('a VISIONOS URL that does not play falls through to the yt-dlp ladder', async () => {
  streamStatus['rr1.googlevideo.example'] = 403;
  const got = await resolver().resolve('vid00000001');
  assert.equal(got.url, DAEMON_URL);
  assert.deepEqual(daemon.resolved, [ 'local' ]);
});

test('a VISIONOS URL that verified but did not play in mpv sends the retry to yt-dlp', async () => {
  const r = resolver();
  assert.equal((await r.urlFor('vid00000001')).url, VISIONOS_URL);
  // What player.js does when mpv could not play it: invalidate, then ask again once.
  r.invalidate('vid00000001');
  assert.equal((await r.urlFor('vid00000001')).url, DAEMON_URL);
  assert.equal(playerCalls().length, 1, 'no second VISIONOS request for that video');
  // Other videos still take the fast path.
  assert.equal((await r.urlFor('vid00000002')).url, VISIONOS_URL);
  // And a yt-dlp URL that fails does not mark the fast path.
  r.invalidate('vid00000001');
  assert.deepEqual(daemon.resolved, [ 'local' ]);
});

test('without the fast path, yt-dlp is asked first', async () => {
  const got = await resolver({ fast: null }).resolve('vid00000001');
  assert.equal(got.url, DAEMON_URL);
  assert.equal(calls.filter((c) => c.url.host.startsWith('youtubei')).length, 0);
});

test('the format is plain, audio-only opus 251 where offered, original track, no DRC', () => {
  assert.equal(pickAudio([ audio(140, 260000), audio(251, 140000) ]).itag, 251);
  assert.equal(pickAudio([ audio(140, 130000), audio(250, 70000) ]).itag, 140);
  assert.equal(pickAudio([ audio(251, 140000, { isDrc: true }), audio(250, 70000) ]).itag, 250);
  assert.equal(pickAudio([ audio(251, 140000, { audioTrack: { audioIsDefault: false } }),
    audio(250, 70000, { audioTrack: { audioIsDefault: true } }) ]).itag, 250);
  // Anything that would still need a challenge solved is not ours to play.
  assert.equal(pickAudio([ { ...audio(251, 1), url: undefined, signatureCipher: 's=x&url=y' } ]), null);
  assert.equal(pickAudio([ { ...audio(251, 1), url: `${VISIONOS_URL}&n=abc` } ]), null);
  assert.equal(pickAudio([ { itag: 18, bitrate: 1, mimeType: 'video/mp4', url: VISIONOS_URL } ]), null);
});

test('yt-dlp attempts skip the watch page and /next unless told not to', () => {
  const youtubeArg = (modes) => modes.local[modes.local.indexOf('--extractor-args',
    modes.local.indexOf('--js-runtimes')) + 1];
  assert.equal(youtubeArg(resolver().daemonModes), 'youtube:fetch_pot=never;player_skip=webpage,initial_data');
  assert.equal(youtubeArg(resolver({ playerSkip: null }).daemonModes), 'youtube:fetch_pot=never');
  // The ladder rungs carry it in the same single `youtube:` value as their client.
  assert.match(resolver().daemonModes['client-web_safari'].join(' '),
    /youtube:fetch_pot=never;player_skip=webpage,initial_data;player_client=web_safari/);
});
