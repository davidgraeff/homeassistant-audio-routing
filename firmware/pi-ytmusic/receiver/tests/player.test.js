// MpvPlayer's local queue, driven through the real yt-cast-receiver `Player` base class
// (so the state events a phone would receive are the library's own) over a fake mpv.
//
// Needs `yt-cast-receiver` installed: `npm install` in receiver/ first.

import assert from 'node:assert/strict';
import { beforeEach, test } from 'node:test';
import MpvPlayer from '../player.js';
import { FakeCatalogue, FakeMpv, quietLogger, settle, track } from './fakes.js';

const page = (n, extra = {}) => ({ playlist_id: 'PLroad', title: 'Road trip',
  items: Array.from({ length: n }, (_, i) => track(i + 1)), continuation: null, ...extra });

let mpv;
let catalogue;
let recorded;
let player;
let states;

/** What the library would tell a phone: one entry per emitted state, as status names. */
const STATUS = { '-1': 'idle', 1: 'playing', 2: 'paused', 3: 'loading', 4: 'stopped' };

beforeEach(() => {
  mpv = new FakeMpv();
  catalogue = new FakeCatalogue({ pages: { PLroad: page(3), RDAMVMvid00000002: page(2, { playlist_id: 'RDAMVMvid00000002',
    title: 'Song 2 Mix', continuation: 'C1' }) },
  continuations: { C1: { items: [ track(7), track(8) ], continuation: null } } });
  recorded = [];
  const history = { record: (e) => recorded.push(e) && true, list: () => [], describe() {} };
  player = new MpvPlayer(mpv, null, null, { catalogue, history });
  player.setLogger(quietLogger);
  player.queue.setLogger(quietLogger);
  // What YouTubeApp installs: here a handler that knows no neighbours.
  player.queue.setRequestHandler({ async getPreviousNextVideosAbortable() { return { previous: null, next: null }; },
    reset() {}, setLogger() {} });
  states = [];
  player.on('state', ({ current }) => states.push(STATUS[current.status]));
});

test('a playlist plays from the receiver\'s own queue, and the state events say so', async () => {
  const started = await player.playLocal({ playlistId: 'PLroad' });
  assert.deepEqual(started, { playlist_id: 'PLroad', title: 'Road trip', length: 3, index: 0 });
  await settle();
  assert.equal(mpv.loaded.length, 1);
  assert.match(mpv.loaded[0], /vid00000001/);
  // loading then playing: exactly what a connected phone is sent.
  assert.deepEqual(states, [ 'loading', 'playing' ]);
  const d = await player.describe();
  assert.equal(d.status, 'playing');
  assert.equal(d.track.title, 'Song 1');
  assert.equal(d.queue.origin, 'local');
  assert.equal(d.queue.length, 3);
  assert.deepEqual(recorded, [ { playlist_id: 'PLroad', title: 'Road trip', thumbnail: track(1).thumbnail, origin: 'local' } ]);
});

test('next, previous and the end of a track walk the local queue', async () => {
  await player.playLocal({ playlistId: 'PLroad' });
  await settle();
  await player.next();
  await settle();
  assert.match(mpv.loaded.at(-1), /vid00000002/);
  mpv.finishTrack();
  await settle();
  assert.match(mpv.loaded.at(-1), /vid00000003/, 'end of track advances');
  await player.previous();
  await settle();
  assert.match(mpv.loaded.at(-1), /vid00000002/);
  await player.jumpTo(0);
  await settle();
  assert.match(mpv.loaded.at(-1), /vid00000001/);
  // At the first entry, previous restarts the track instead of failing.
  const before = mpv.loaded.length;
  await player.previous();
  await settle();
  assert.equal(mpv.loaded.length, before);
});

test('the queue stops at its end and play restarts where it was', async () => {
  await player.playLocal({ playlistId: 'PLroad', videoId: 'vid00000003' });
  await settle();
  assert.match(mpv.loaded.at(-1), /vid00000003/, 'started at the asked track');
  assert.equal(await player.next(), false);
  await settle();
  assert.equal((await player.describe()).status, 'stopped');
  await player.resume();
  await settle();
  assert.equal((await player.describe()).status, 'playing');
  assert.match(mpv.loaded.at(-1), /vid00000003/);
});

test('pause and resume go through the base class, so a phone would see them', async () => {
  await player.playLocal({ playlistId: 'PLroad' });
  await settle();
  states.length = 0;
  assert.equal(await player.pause(), true);
  assert.equal(mpv.props.pause, true);
  assert.equal(await player.resume(), true);
  assert.equal(mpv.props.pause, false);
  assert.deepEqual(states, [ 'paused', 'playing' ]);
});

test('a radio fetches its next page before it runs dry', async () => {
  await player.playLocal({ playlistId: 'RDAMVMvid00000002' });
  await settle();
  assert.ok(catalogue.asked.some((a) => a.token === 'C1'), 'refilled while the first track plays');
  await player.next();
  await player.next();
  await settle();
  assert.match(mpv.loaded.at(-1), /vid00000007/);
});

test('a phone casting something takes over from the local queue', async () => {
  await player.playLocal({ playlistId: 'PLroad' });
  await settle();
  // Exactly what YouTubeApp does with a phone's setPlaylist.
  await player.queue.updateByMessage({ name: 'setPlaylist', payload: { listId: 'PLphone',
    videoIds: 'aaaaaaaaaaa,bbbbbbbbbbb', currentIndex: '0', videoId: 'aaaaaaaaaaa' } }, null);
  const d = await player.describe();
  assert.equal(d.queue.origin, 'cast');
  assert.equal(d.queue.playlist_id, 'PLphone');
  assert.deepEqual(recorded.map((e) => [ e.playlist_id, e.origin ]), [ [ 'PLroad', 'local' ], [ 'PLphone', 'cast' ] ]);
});

test('a cast session reset does not stop what the receiver itself is playing', async () => {
  await player.playLocal({ playlistId: 'PLroad' });
  await settle();
  await player.reset();
  await settle();
  const d = await player.describe();
  assert.equal(d.status, 'playing');
  assert.equal(d.queue.origin, 'local');
  // ...while clearing it does stop, and leaves nothing to resume.
  await player.clearLocal();
  assert.equal((await player.describe()).status, 'stopped');
});

test('a list YouTube Music cannot queue is refused with a sentence and nothing plays', async () => {
  await assert.rejects(player.playLocal({ playlistId: 'PLnope' }), /no such list/);
  catalogue.pages.PLempty = page(0);
  await assert.rejects(player.playLocal({ playlistId: 'PLempty' }), /empty queue/);
  assert.equal(mpv.loaded.length, 0);
});

test('a requested song plays before its radio has been fetched, and the radio follows it', async () => {
  let release;
  catalogue.hold = new Promise((r) => {
    release = r;
  });
  const started = player.playLocal({ playlistId: 'RDAMVMvid00000002', videoId: 'vid00000002' });
  await settle();
  assert.equal(mpv.loaded.length, 1, 'playing while the catalogue is still answering');
  assert.match(mpv.loaded[0], /vid00000002/);
  release();
  const answer = await started;
  // At its own place in the radio (second, in this fixture), not restarted at the top.
  assert.deepEqual(answer, { playlist_id: 'RDAMVMvid00000002', title: 'Song 2 Mix', length: 2, index: 1 });
  await settle();
  assert.equal(mpv.loaded.length, 1, 'adopting the list does not restart the song');
  mpv.finishTrack();
  await settle();
  assert.match(mpv.loaded.at(-1), /vid00000007/, 'and the radio plays on, into its next page');
});

test('a song whose radio cannot be fetched still plays, as a queue of one', async () => {
  const answer = await player.playLocal({ playlistId: 'RDAMVMnope', videoId: 'vid00000005' });
  await settle();
  assert.equal(answer.length, 1);
  assert.match(mpv.loaded[0], /vid00000005/);
  assert.equal((await player.describe()).status, 'playing');
});

test('a cached song plays from its file, and a streamed one is handed to the cache', async () => {
  const fetched = [];
  const cache = {
    files: { vid00000001: '/cache/vid00000001.webm' },
    has(id) { return !!this.files[id]; },
    lookup(id) { return this.files[id] ?? null; },
    fetch(id, url) { fetched.push([ id, url ]); },
    async remove(id) { delete this.files[id]; },
  };
  const resolved = [];
  const resolver = { enabled: true, peek: () => null, prefetch() {}, invalidate() {},
    async urlFor(id) { resolved.push(id); return { url: `https://rr.example/${id}`, headers: { 'User-Agent': 'UA' } }; } };
  player = new MpvPlayer(mpv, null, resolver, { catalogue, cache });
  player.setLogger(quietLogger);
  player.queue.setLogger(quietLogger);
  player.queue.setRequestHandler({ async getPreviousNextVideosAbortable() { return { previous: null, next: null }; },
    reset() {}, setLogger() {} });
  await player.playLocal({ playlistId: 'PLroad' });
  await settle();
  assert.equal(mpv.loaded[0], '/cache/vid00000001.webm');
  assert.equal(mpv.props['user-agent'], '', 'a file needs no User-Agent');
  // The next song was resolved while the first played, and handed to the cache whole.
  assert.deepEqual(fetched, [ [ 'vid00000002', 'https://rr.example/vid00000002' ] ]);
  await player.next();
  await settle();
  assert.equal(mpv.loaded.at(-1), 'https://rr.example/vid00000002', 'not cached yet, so it streams');
  assert.deepEqual(resolved.filter((id) => id === 'vid00000001'), [], 'the cached song was never resolved');
});

test('a stream mpv cannot open is retried from the cached copy, and the queue does not skip it', async () => {
  const cache = {
    files: {},
    has(id) { return !!this.files[id]; },
    lookup(id) { return this.files[id] ?? null; },
    fetch() {},
    // The ranged download alongside the stream finished: what audiocache.js answers then.
    async ready(id) { this.files[id] = `/cache/${id}.webm`; return this.files[id]; },
    async remove(id) { delete this.files[id]; },
  };
  const invalidated = [];
  const resolver = { enabled: true, peek: () => null, prefetch() {}, invalidate(id) { invalidated.push(id); },
    async urlFor(id) { return { url: `https://rr.example/${id}`, headers: {} }; } };
  player = new MpvPlayer(mpv, null, resolver, { catalogue, cache });
  player.setLogger(quietLogger);
  player.queue.setLogger(quietLogger);
  player.queue.setRequestHandler({ async getPreviousNextVideosAbortable() { return { previous: null, next: null }; },
    reset() {}, setLogger() {} });
  mpv.failOpens = 1;
  await player.playLocal({ playlistId: 'PLroad' });
  await settle();
  assert.deepEqual(mpv.loaded, [ 'https://rr.example/vid00000001', '/cache/vid00000001.webm' ]);
  assert.deepEqual(invalidated, [], 'no second resolve: the file was there');
  const d = await player.describe();
  assert.equal(d.status, 'playing');
  assert.equal(d.queue.index, 0, 'the failed open did not also skip to the next song');
});
