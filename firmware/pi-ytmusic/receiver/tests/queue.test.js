// LocalQueue (the receiver's own queue) and PlayHistory (recently played lists).
// Pure bookkeeping — no mpv, no network. Run with: npm test (in receiver/).

import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import PlayHistory, { kindOf } from '../history.js';
import LocalQueue from '../localqueue.js';
import { quietLogger, track } from './fakes.js';

const items = (n, from = 1) => Array.from({ length: n }, (_, i) => track(from + i));

test('a queue walks forward and back and stops at its ends', () => {
  const q = new LocalQueue({ playlistId: 'PLx', items: items(3) });
  assert.equal(q.current.title, 'Song 1');
  assert.equal(q.back(), null, 'nothing before the first');
  assert.equal(q.advance().title, 'Song 2');
  assert.equal(q.upcoming.title, 'Song 3');
  assert.equal(q.advance().title, 'Song 3');
  assert.equal(q.advance(), null, 'nothing after the last');
  assert.equal(q.index, 2, 'a refused advance does not move');
  assert.equal(q.hasNext, false);
  assert.equal(q.back().title, 'Song 2');
});

test('a queue may start part-way and jump anywhere inside it', () => {
  const q = new LocalQueue({ playlistId: 'PLx', items: items(5), startIndex: 3 });
  assert.equal(q.current.title, 'Song 4');
  assert.equal(q.jump(0).title, 'Song 1');
  assert.equal(q.jump(5), null);
  assert.equal(q.jump(-1), null);
  assert.equal(q.index, 0);
});

test('a continuation means there is more, and asks for it near the end', () => {
  const q = new LocalQueue({ playlistId: 'RDx', items: items(5), continuation: 'T1' });
  assert.equal(q.needsMore, false, 'far from the end');
  q.jump(2);
  assert.equal(q.needsMore, true, 'within the refill threshold');
  q.jump(4);
  assert.equal(q.hasNext, true, 'the last item still has a next page');
  // A radio's next page can repeat the seam; a track is queued once.
  q.append([ track(5), track(6) ], 'T2');
  assert.equal(q.length, 6);
  assert.equal(q.continuation, 'T2');
  assert.equal(q.advance().title, 'Song 6');
  q.append([], null);
  assert.equal(q.hasNext, false);
});

test('the window marks the current entry and carries indices', () => {
  const q = new LocalQueue({ playlistId: 'PLx', items: items(10), startIndex: 4 });
  const w = q.window({ before: 1, after: 2 });
  assert.deepEqual(w.map((t) => [ t.index, t.current ]), [ [ 3, false ], [ 4, true ], [ 5, false ], [ 6, false ] ]);
});

const dir = mkdtempSync(path.join(path.dirname(fileURLToPath(import.meta.url)), '.history-'));
after(() => rmSync(dir, { recursive: true, force: true }));

test('history keeps the newest first, deduplicated, and survives a restart', () => {
  const h = new PlayHistory({ dir, logger: quietLogger });
  h.record({ playlist_id: 'PLa', title: 'A', origin: 'cast' }, 1000);
  h.record({ playlist_id: 'RDAMVMdQw4w9WgXcQ', origin: 'cast' }, 2000);
  h.record({ playlist_id: 'PLa', origin: 'local' }, 3000);
  assert.deepEqual(h.list().map((e) => e.playlist_id), [ 'PLa', 'RDAMVMdQw4w9WgXcQ' ]);
  // A field not given keeps what was known — a cast names no title.
  assert.equal(h.list()[0].title, 'A');
  assert.equal(h.list()[0].origin, 'local');
  assert.equal(h.list()[1].kind, 'mix');

  h.describe('RDAMVMdQw4w9WgXcQ', { title: 'Never Gonna Give You Up Mix', thumbnail: 'https://x/y.jpg' });
  const again = new PlayHistory({ dir, logger: quietLogger });
  assert.equal(again.list()[1].title, 'Never Gonna Give You Up Mix');
  assert.equal(again.list()[0].played_at, 3000);
});

test('a one-session remote queue is not worth remembering', () => {
  const h = new PlayHistory({ dir: mkdtempSync(path.join(dir, 'rq-')), logger: quietLogger });
  assert.equal(h.record({ playlist_id: 'RQabcdef' }), false);
  assert.equal(h.record({ playlist_id: '' }), false);
  assert.deepEqual(h.list(), []);
});

test('a corrupt history file is an empty list, not an error', () => {
  const d = mkdtempSync(path.join(dir, 'bad-'));
  writeFileSync(path.join(d, 'recent-playlists.json'), '{not json');
  const h = new PlayHistory({ dir: d, logger: quietLogger });
  assert.deepEqual(h.list(), []);
  h.record({ playlist_id: 'PLz' });
  assert.equal(JSON.parse(readFileSync(path.join(d, 'recent-playlists.json'), 'utf8')).playlists.length, 1);
});

test('kinds follow the id prefix', () => {
  assert.equal(kindOf('OLAK5uy_x'), 'album');
  assert.equal(kindOf('RDTMAK5uy_x'), 'mix');
  assert.equal(kindOf('PLx'), 'playlist');
  assert.equal(kindOf('LM'), 'playlist');
});
