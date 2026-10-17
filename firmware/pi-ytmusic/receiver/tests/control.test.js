// control.js — what a play_media id means, and the Controller's answers, against a fake
// player. The real player is covered by player.test.js.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Controller, FEATURES, parseMediaId, playlistMediaId, queueMediaId, RequestError, trackMediaId } from '../control.js';
import { FakeCatalogue, track } from './fakes.js';

test('the API\'s own ids parse, and malformed ones do not', () => {
  assert.deepEqual(parseMediaId(playlistMediaId('PLabc')), { kind: 'playlist', playlistId: 'PLabc', videoId: null });
  assert.deepEqual(parseMediaId(playlistMediaId('PLabc', 'dQw4w9WgXcQ')),
    { kind: 'playlist', playlistId: 'PLabc', videoId: 'dQw4w9WgXcQ' });
  assert.deepEqual(parseMediaId(trackMediaId('dQw4w9WgXcQ')), { kind: 'track', videoId: 'dQw4w9WgXcQ' });
  assert.deepEqual(parseMediaId(queueMediaId(7)), { kind: 'queue', index: 7 });
  for (const bad of [ 'ytmusic:playlist:PL/x', 'ytmusic:track:short', 'ytmusic:queue:-1', 'ytmusic:nope:x',
    'ytmusic:playlist:PLabc:not-a-video-id', '', null, 42 ]) {
    assert.equal(parseMediaId(bad), null, String(bad));
  }
});

test('YouTube and YouTube Music links parse, other hosts do not', () => {
  assert.deepEqual(parseMediaId('https://music.youtube.com/playlist?list=OLAK5uy_abc'),
    { kind: 'playlist', playlistId: 'OLAK5uy_abc', videoId: null });
  assert.deepEqual(parseMediaId('https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVMdQw4w9WgXcQ'),
    { kind: 'playlist', playlistId: 'RDAMVMdQw4w9WgXcQ', videoId: 'dQw4w9WgXcQ' });
  assert.deepEqual(parseMediaId('https://youtu.be/dQw4w9WgXcQ'), { kind: 'track', videoId: 'dQw4w9WgXcQ' });
  assert.equal(parseMediaId('https://example.com/watch?v=dQw4w9WgXcQ'), null);
});

test('a bare id needs the media type to say what it is', () => {
  assert.deepEqual(parseMediaId('PLabc', 'playlist'), { kind: 'playlist', playlistId: 'PLabc', videoId: null });
  assert.deepEqual(parseMediaId('dQw4w9WgXcQ', 'music'), { kind: 'track', videoId: 'dQw4w9WgXcQ' });
  assert.equal(parseMediaId('PLabc'), null);
});

/** A player double that records what the controller asked of it. */
function fakePlayer(describe = {}) {
  const calls = [];
  const base = { status: 'playing', video_id: 'vid00000001', track: track(1), position: 12, duration: 180,
    queue: { origin: 'local', playlist_id: 'PLx', title: 'Mix', index: 0, length: 3, more: false, can_next: true,
      can_previous: false, items: [ { ...track(1), index: 0, current: true }, { ...track(2), index: 1, current: false },
        { ...track(3), index: 2, current: false } ] },
    error: null, ...describe };
  return {
    calls,
    async describe() { return structuredClone(base); },
    async playLocal(req) { calls.push([ 'playLocal', req ]); return { playlist_id: req.playlistId, title: 'T', length: 3, index: 0 }; },
    async jumpTo(i) { calls.push([ 'jumpTo', i ]); return i < 3; },
    async resume() { calls.push([ 'resume' ]); return true; },
    async pause() { calls.push([ 'pause' ]); return true; },
    async stop() { calls.push([ 'stop' ]); return true; },
    async next() { calls.push([ 'next' ]); return true; },
    async previous() { calls.push([ 'previous' ]); return true; },
    async seek(p) { calls.push([ 'seek', p ]); return true; },
    async clearLocal() { calls.push([ 'clearLocal' ]); return true; },
  };
}

test('state reports the track, the upcoming queue and what can be done', async () => {
  const c = new Controller({ player: fakePlayer(), catalogue: new FakeCatalogue(), info: { name: 'Musik', rtpPort: 46002 },
    sender: () => ({ name: 'Pixel', client: 'YouTube Music' }) });
  const s = await c.state();
  assert.equal(s.state, 'playing');
  assert.equal(s.origin, 'local');
  assert.equal(s.track.media_id, trackMediaId('vid00000001'));
  assert.equal(s.track.title, 'Song 1');
  assert.equal(s.track.position, 12);
  assert.ok(s.track.position_at > 0);
  assert.equal(s.playlist.media_id, playlistMediaId('PLx'));
  // The current track is the `track`; the queue lists what comes after it.
  assert.deepEqual(s.queue.items.map((t) => t.media_id), [ queueMediaId(1), queueMediaId(2) ]);
  assert.ok(s.queue.items.every((t) => t.playable));
  assert.deepEqual(s.cast, { connected: true, sender: 'Pixel', client: 'YouTube Music' });
  assert.deepEqual(s.features, FEATURES);
  assert.equal(s.rtp_port, 46002);
});

test('a phone\'s queue is shown but not jumped into', async () => {
  const player = fakePlayer({ queue: { origin: 'cast', playlist_id: 'RDx', title: null, index: 0, length: 2,
    more: false, can_next: true, can_previous: false,
    items: [ { video_id: 'vid00000001', index: 0, current: true }, { video_id: 'vid00000002', index: 1, current: false } ] } });
  const s = await new Controller({ player }).state();
  assert.equal(s.origin, 'cast');
  assert.deepEqual(s.queue.items.map((t) => [ t.media_id, t.playable ]), [ [ trackMediaId('vid00000002'), false ] ]);
});

test('commands map onto the player, and bad ones are refused with a sentence', async () => {
  const player = fakePlayer();
  const c = new Controller({ player });
  for (const cmd of [ 'play', 'pause', 'stop', 'next', 'previous', 'clear' ]) {
    await c.command(cmd);
  }
  await c.command('seek', { position: 42 });
  await c.command('play_pause');
  assert.deepEqual(player.calls.map((x) => x[0]),
    [ 'resume', 'pause', 'stop', 'next', 'previous', 'clearLocal', 'seek', 'pause' ]);
  await assert.rejects(c.command('seek', { position: -1 }), (e) => e instanceof RequestError && e.status === 400);
  await assert.rejects(c.command('rewind'), (e) => e.status === 400 && /unknown command/.test(e.message));
});

test('a playlist plays from its start or a given track; a song plays as its radio', async () => {
  const player = fakePlayer();
  const c = new Controller({ player, catalogue: new FakeCatalogue() });
  await c.play(playlistMediaId('OLAK5uy_album'));
  await c.play(playlistMediaId('PLabc', 'dQw4w9WgXcQ'));
  await c.play(trackMediaId('dQw4w9WgXcQ'));
  assert.deepEqual(player.calls, [
    [ 'playLocal', { playlistId: 'OLAK5uy_album', videoId: null } ],
    [ 'playLocal', { playlistId: 'PLabc', videoId: 'dQw4w9WgXcQ' } ],
    [ 'playLocal', { playlistId: 'RDAMVMdQw4w9WgXcQ', videoId: 'dQw4w9WgXcQ' } ],
  ]);
});

test('play refuses what it cannot do, and says why', async () => {
  const offline = new FakeCatalogue();
  offline.available = false;
  const c = new Controller({ player: fakePlayer(), catalogue: offline });
  await assert.rejects(c.play('nonsense'), (e) => e.status === 400);
  await assert.rejects(c.play(playlistMediaId('PLabc')), (e) => e.status === 503);
  await assert.rejects(c.play(queueMediaId(9)), (e) => e.status === 409);
  const failing = fakePlayer();
  failing.playLocal = async () => { throw new Error('YouTube Music returned an empty queue for PLabc'); };
  await assert.rejects(new Controller({ player: failing, catalogue: new FakeCatalogue() }).play(playlistMediaId('PLabc')),
    (e) => e.status === 502 && /empty queue/.test(e.message));
});

test('the browse root offers the queue, the recent lists and the account feeds, in the asked language', async () => {
  const history = { list: () => [ { playlist_id: 'RDAMVMdQw4w9WgXcQ', title: 'NGGYU Mix', kind: 'mix', played_at: 5 } ] };
  const c = new Controller({ player: fakePlayer(), catalogue: new FakeCatalogue(), history });
  const root = await c.browse(null, 'de');
  assert.deepEqual(root.children.map((n) => [ n.id, n.title ]), [
    [ 'ytmusic:queue', 'Warteschlange' ], [ 'ytmusic:recent', 'Zuletzt gespielt' ],
    [ 'ytmusic:library', 'Mediathek' ], [ 'ytmusic:history', 'Verlauf' ] ]);
  const recent = await c.browse('ytmusic:recent', 'en');
  assert.equal(recent.title, 'Recently played');
  assert.deepEqual(recent.children.map((n) => [ n.id, n.kind, n.playable, n.expandable, n.played_at ]),
    [ [ playlistMediaId('RDAMVMdQw4w9WgXcQ'), 'mix', true, true, 5 ] ]);
  const queue = await c.browse('ytmusic:queue');
  assert.deepEqual(queue.children.map((n) => [ n.id, n.current ]),
    [ [ queueMediaId(0), true ], [ queueMediaId(1), false ], [ queueMediaId(2), false ] ]);
});

test('a playlist node expands into tracks that start the list there', async () => {
  const catalogue = new FakeCatalogue({ pages: { PLabc: { playlist_id: 'PLabc', title: 'Road trip',
    items: [ track(1), track(2) ], continuation: null } } });
  const n = await new Controller({ player: fakePlayer(), catalogue }).browse(playlistMediaId('PLabc'));
  assert.equal(n.title, 'Road trip');
  assert.equal(n.playable, true);
  assert.deepEqual(n.children.map((x) => x.id), [ playlistMediaId('PLabc', 'vid00000001'), playlistMediaId('PLabc', 'vid00000002') ]);
  await assert.rejects(new Controller({ player: fakePlayer(), catalogue }).browse('ytmusic:nothing'), (e) => e.status === 404);
});

test('without a catalogue the root still offers what this receiver played', async () => {
  const c = new Controller({ player: fakePlayer({ queue: { origin: null, items: [], length: 0 } }), history: { list: () => [] } });
  const root = await c.browse(null);
  assert.deepEqual(root.children.map((n) => n.id), [ 'ytmusic:recent' ]);
});
