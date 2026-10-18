// CastQueueHandler: next/previous for a cast queue from the list the phone sent, and only
// past its end from YouTube. Fakes stand in for the library's handler and the catalogue.

import assert from 'node:assert/strict';
import { beforeEach, test } from 'node:test';
import CastQueueHandler from '../castqueue.js';
import { quietLogger, track } from './fakes.js';

const MUSIC = { key: 'YTMUSIC', name: 'YouTube Music' };
const ids = [ 'vid00000001', 'vid00000002', 'vid00000003' ];
const video = (id, index, playlistId = 'PLmine') => ({ id, client: MUSIC, context: { playlistId, index } });

let fallback;
let catalogue;
let handler;

beforeEach(() => {
  fallback = { asked: [], answer: { previous: null, next: null },
    async getPreviousNextVideos(target) { this.asked.push(target.id); return this.answer; },
    setLogger() {}, reset() {} };
  catalogue = { available: true, asked: [], async queue(playlistId, videoId) {
    this.asked.push({ playlistId, videoId });
    return { items: [ track(7), track(8), track(9) ] };
  } };
  handler = new CastQueueHandler({ fallback, catalogue });
  handler.setLogger(quietLogger);
});

test('the neighbours come from the list the phone sent, without asking YouTube', async () => {
  const r = await handler.getPreviousNextVideos(video('vid00000002', 1), { id: 'PLmine', videoIds: ids });
  assert.deepEqual(r.previous, video('vid00000001', 0));
  assert.deepEqual(r.next, video('vid00000003', 2));
  assert.deepEqual(fallback.asked, []);
  assert.deepEqual(catalogue.asked, []);
});

test('a song listed twice is placed by its own index, not its first appearance', async () => {
  const twice = [ 'vid00000001', 'vid00000002', 'vid00000001', 'vid00000004' ];
  const r = await handler.getPreviousNextVideos(video('vid00000001', 2), { id: 'PLmine', videoIds: twice });
  assert.equal(r.next.id, 'vid00000004');
  assert.equal(r.previous.id, 'vid00000002');
});

test('past the end of the list, YouTube decides (autoplay) and the list keeps its previous', async () => {
  fallback.answer = { previous: null, next: video('vid00000099', 0, 'RDAMVMvid00000003') };
  const r = await handler.getPreviousNextVideos(video('vid00000003', 2), { id: 'PLmine', videoIds: ids });
  assert.equal(r.next.id, 'vid00000099');
  assert.equal(r.previous.id, 'vid00000002');
  assert.deepEqual(catalogue.asked, [], 'the end of a list has nothing after it to look up');
});

test('a song missing from the list is looked up in the signed-in catalogue when YouTube has nothing', async () => {
  // What a private list or Liked Music gets from the library's anonymous request.
  const r = await handler.getPreviousNextVideos(video('vid00000008', 5, 'LM'), { id: 'LM', videoIds: [] });
  assert.deepEqual(catalogue.asked, [ { playlistId: 'LM', videoId: 'vid00000008' } ]);
  assert.deepEqual(r.next, { id: 'vid00000009', client: MUSIC, context: { playlistId: 'LM', index: 2 } });
});

test('a failing autoplay lookup is not a failed queue', async () => {
  fallback.getPreviousNextVideos = async () => {
    throw new Error('TV client gone');
  };
  const r = await handler.getPreviousNextVideos(video('vid00000003', 2), { id: 'PLmine', videoIds: ids });
  assert.equal(r.next, null);
  assert.equal(r.previous.id, 'vid00000002');
});
