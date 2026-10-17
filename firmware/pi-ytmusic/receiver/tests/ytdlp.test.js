// YtdlpDaemon's respawn accounting, against a fake daemon process (fixtures/fake_ytdlpd.mjs).
//
// The bug this pins down: every exit counted towards MAX_RESPAWNS for the life of the
// receiver, including the restarts resolver.js asks for itself, so a week of 403 windows
// gave up on a healthy daemon and left every track on the per-track spawn.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, beforeEach, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import YtdlpDaemon from '../ytdlp.js';
import { quietLogger } from './fakes.js';

const FAKE = fileURLToPath(new URL('./fixtures/fake_ytdlpd.mjs', import.meta.url));

let dir;
let daemon;

beforeEach(() => {
  fs.chmodSync(FAKE, 0o755);
  dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ytdlpd-'));
  daemon = new YtdlpDaemon({ python: FAKE, socketPath: path.join(dir, 'd.sock'), modes: {},
    prewarm: false, respawnDelayMs: 20, logger: quietLogger });
});

afterEach(async () => {
  delete process.env.FAKE_YTDLPD_CRASH;
  await daemon.stop();
  fs.rmSync(dir, { recursive: true, force: true });
});

async function until(predicate, ms = 10000) {
  const deadline = Date.now() + ms;
  while (!predicate()) {
    assert.ok(Date.now() < deadline, 'timed out');
    await new Promise((r) => setTimeout(r, 20));
  }
}

test('restarts the resolver asks for never add up to giving up', async () => {
  await daemon.start();
  for (let i = 0; i < 8; i++) {
    daemon.invalidate(null, 'every rung 403d', 'vid00000001', { escalate: true });
    assert.equal(await daemon.waitUntilRunning(), true, `restart ${i + 1} came back`);
  }
  assert.equal(daemon.abandoned, false);
});

test('a daemon that keeps crashing is still given up on', async () => {
  await daemon.start();
  // From here on every respawn dies before it listens.
  process.env.FAKE_YTDLPD_CRASH = '1';
  daemon.request({ op: 'crash' }, 1000).catch(() => {});
  await until(() => daemon.abandoned);
  assert.equal(daemon.running, false);
});
