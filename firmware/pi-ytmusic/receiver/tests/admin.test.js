// The admin page's settings endpoint, against a real server on a free port.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { createServer } from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import startAdmin from '../admin.js';
import PlayLimit from '../playlimit.js';
import { quietLogger } from './fakes.js';

let server;
let base;
let limit;

before(async () => {
  const port = await new Promise((resolve) => {
    const probe = createServer().listen(0, '127.0.0.1', () => {
      const { port: p } = probe.address();
      probe.close(() => resolve(p));
    });
  });
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'admin-'));
  limit = new PlayLimit({ file: path.join(dir, 'settings.json'), logger: quietLogger }).load();
  server = startAdmin({ port, bind: '127.0.0.1', device: { sinkName: 'ytm-out' }, router: { host: '127.0.0.1', apiPort: 1 },
    rtpPort: 46002, jar: { file: path.join(dir, 'cookies.txt') }, probes: {}, playLimit: limit, logger: quietLogger });
  await new Promise((r) => server.once('listening', r));
  base = `http://127.0.0.1:${port}`;
});

after(() => server.close());

const post = (body) => fetch(`${base}/api/settings`, { method: 'POST', headers: { 'Content-Type': 'application/json' },
  body: typeof body === 'string' ? body : JSON.stringify(body) });

test('the maximum continuous play is set through the admin page, and nonsense is refused', async () => {
  const ok = await post({ max_continuous_play_minutes: 120 });
  assert.equal(ok.status, 200);
  assert.equal((await ok.json()).playback_limit.max_minutes, 120);
  assert.equal(limit.maxMinutes, 120);
  const bad = await post({ max_continuous_play_minutes: -5 });
  assert.equal(bad.status, 400);
  assert.match((await bad.json()).error, /whole number of minutes/);
  assert.equal((await post('{')).status, 400);
  assert.equal(limit.maxMinutes, 120, 'a refused value changes nothing');
});

test('behind Home Assistant ingress the same route answers under its prefix', async () => {
  const res = await fetch(`${base}/api/hassio_ingress/abc123/api/settings`, { method: 'POST',
    headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ max_continuous_play_minutes: 180 }) });
  assert.equal(res.status, 200);
  assert.equal(limit.maxMinutes, 180);
});
