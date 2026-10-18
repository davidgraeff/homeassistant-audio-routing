// AudioCache against a local HTTP server that serves byte ranges the way googlevideo does.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { createServer } from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, before, beforeEach, test } from 'node:test';
import AudioCache from '../audiocache.js';
import { quietLogger } from './fakes.js';

const SONG = Buffer.alloc(25 * 1024 * 1024, 7); // three 10 MiB ranges
let server;
let base;
let requests;
let dir;

before(async () => {
  server = createServer((req, res) => {
    requests.push({ url: req.url, range: req.headers.range, ua: req.headers['user-agent'] });
    if (req.url.startsWith('/forbidden')) {
      res.writeHead(403).end();
      return;
    }
    const [ , from, to ] = /bytes=(\d+)-(\d+)/.exec(req.headers.range ?? '') ?? [];
    const start = Number(from);
    const end = Math.min(Number(to), SONG.length - 1);
    res.writeHead(206, { 'Content-Range': `bytes ${start}-${end}/${SONG.length}` });
    res.end(SONG.subarray(start, end + 1));
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${server.address().port}`;
});

after(() => server.close());

beforeEach(() => {
  requests = [];
  dir = fs.mkdtempSync(path.join(os.tmpdir(), 'audiocache-'));
});

const url = (p, mime = 'audio/webm') => `${base}${p}?mime=${encodeURIComponent(mime)}&clen=${SONG.length}`;

test('a song is fetched in 10 MiB ranges with the resolver\'s headers, and found afterwards', async () => {
  const cache = new AudioCache({ dir, maxBytes: 100 << 20, logger: quietLogger });
  await cache.init();
  cache.fetch('vid00000001', url('/a'), { 'User-Agent': 'visionos UA' });
  cache.fetch('vid00000001', url('/a'), {}); // already on its way: not fetched twice
  await cache.idle();
  assert.deepEqual(requests.map((r) => r.range),
    [ 'bytes=0-10485759', 'bytes=10485760-20971519', 'bytes=20971520-26214399' ]);
  assert.ok(requests.every((r) => r.ua === 'visionos UA'));
  const file = cache.lookup('vid00000001');
  assert.equal(file, path.join(dir, 'vid00000001.webm'));
  assert.equal(fs.statSync(file).size, SONG.length);
  // A restart finds it again.
  const again = new AudioCache({ dir, maxBytes: 100 << 20, logger: quietLogger });
  await again.init();
  assert.equal(again.has('vid00000001'), true);
});

test('the least recently played songs go first once the limit is passed', async () => {
  const cache = new AudioCache({ dir, maxBytes: 60 << 20, logger: quietLogger });
  await cache.init();
  cache.fetch('vid00000001', url('/1'));
  cache.fetch('vid00000002', url('/2', 'audio/mp4'));
  await cache.idle();
  // Milliseconds apart, as plays are: "least recently" has to be decidable.
  await new Promise((r) => setTimeout(r, 5));
  cache.lookup('vid00000001'); // played again: now the most recent
  await new Promise((r) => setTimeout(r, 5));
  cache.fetch('vid00000003', url('/3'));
  await cache.idle();
  assert.equal(cache.has('vid00000002'), false, 'the least recently played was evicted');
  assert.equal(cache.has('vid00000001'), true);
  assert.equal(cache.has('vid00000003'), true);
  assert.equal(fs.existsSync(path.join(dir, 'vid00000002.m4a')), false);
});

test('a failed download leaves nothing behind, and a leftover .part is cleared at start', async () => {
  fs.writeFileSync(path.join(dir, 'vid00000009.webm.part'), 'half');
  const cache = new AudioCache({ dir, maxBytes: 100 << 20, logger: quietLogger });
  await cache.init();
  assert.deepEqual(fs.readdirSync(dir), []);
  cache.fetch('vid00000001', url('/forbidden'));
  await cache.idle();
  assert.equal(cache.has('vid00000001'), false);
  assert.deepEqual(fs.readdirSync(dir), []);
});

test('a song larger than the whole cache is not fetched at all', async () => {
  const cache = new AudioCache({ dir, maxBytes: 1 << 20, logger: quietLogger });
  await cache.init();
  cache.fetch('vid00000001', url('/a'));
  await cache.idle();
  assert.equal(requests.length, 0);
});

test('a range answered 503 is tried again, and a waiting caller gets the file', async () => {
  let refusals = 1;
  const flaky = createServer((req, res) => {
    if (refusals-- > 0) {
      res.writeHead(503).end();
      return;
    }
    const [ , from, to ] = /bytes=(\d+)-(\d+)/.exec(req.headers.range);
    const end = Math.min(Number(to), SONG.length - 1);
    res.writeHead(206, { 'Content-Range': `bytes ${from}-${end}/${SONG.length}` });
    res.end(SONG.subarray(Number(from), end + 1));
  });
  await new Promise((r) => flaky.listen(0, '127.0.0.1', r));
  try {
    const cache = new AudioCache({ dir, maxBytes: 100 << 20, logger: quietLogger });
    await cache.init();
    assert.equal(await cache.ready('vid00000001', 10), null, 'nothing on its way yet');
    cache.fetch('vid00000001', `http://127.0.0.1:${flaky.address().port}/a?mime=audio%2Fwebm&clen=${SONG.length}`);
    assert.equal(await cache.ready('vid00000001', 10000), path.join(dir, 'vid00000001.webm'));
  }
  finally {
    flaky.close();
  }
});
