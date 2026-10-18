// PlayLimit: what counts as continuous play, and the saved limit. A fake clock throughout.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { beforeEach, test } from 'node:test';
import PlayLimit, { DEFAULT_MAX_MINUTES } from '../playlimit.js';
import { quietLogger } from './fakes.js';

const MIN = 60 * 1000;
let dir;
let file;

beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), 'playlimit-'));
  file = path.join(dir, 'settings.json');
});

/** Play (or not) for `minutes`, sampled every 30 s as the player does. */
function run(limit, minutes, playing, from) {
  let t = from;
  limit.sample(playing, t); // the state from `from` on, as the player's tick would see it
  for (let i = 0; i < minutes * 2; i++) {
    t += MIN / 2;
    limit.sample(playing, t);
  }
  return t;
}

test('three hours of playing reach the default limit; the grace period then cuts the song', () => {
  const limit = new PlayLimit({ file, logger: quietLogger }).load();
  assert.equal(limit.maxMinutes, DEFAULT_MAX_MINUTES);
  let t = run(limit, 179, true, 0);
  assert.equal(limit.reached, false);
  t = run(limit, 1, true, t);
  assert.equal(limit.reached, true);
  assert.equal(limit.overrun, false);
  run(limit, 15, true, t);
  assert.equal(limit.overrun, true);
});

test('a break of ten minutes starts the count again; a shorter pause does not count but keeps it', () => {
  const limit = new PlayLimit({ file, logger: quietLogger }).load();
  let t = run(limit, 60, true, 0);
  t = run(limit, 9, false, t);
  assert.equal(limit.describe().played_minutes, 60, 'a nine-minute pause neither counts nor resets');
  t = run(limit, 30, true, t);
  assert.equal(limit.describe().played_minutes, 90);
  run(limit, 10, false, t);
  assert.equal(limit.playedMs, 0, 'ten minutes off is a break');
});

test('starting something on purpose resets the count, and a suspended host does not count as play', () => {
  const limit = new PlayLimit({ file, logger: quietLogger }).load();
  run(limit, 100, true, 0);
  limit.reset();
  assert.equal(limit.playedMs, 0);
  limit.sample(true, 10_000_000);
  limit.sample(true, 10_000_000 + 60 * MIN); // an hour between two samples
  assert.ok(limit.playedMs <= 2 * MIN);
});

test('the limit is saved, survives a restart, and refuses what is not a limit', () => {
  const limit = new PlayLimit({ file, logger: quietLogger }).load();
  limit.setMaxMinutes(90);
  assert.equal(new PlayLimit({ file, logger: quietLogger }).load().maxMinutes, 90);
  limit.setMaxMinutes(0);
  run(limit, 600, true, 0);
  assert.equal(limit.reached, false, '0 never stops');
  for (const bad of [ -1, 1.5, 24 * 60 + 1, '60', null ]) {
    assert.throws(() => limit.setMaxMinutes(bad), /whole number of minutes/);
  }
  fs.writeFileSync(file, 'not json');
  assert.equal(new PlayLimit({ file, logger: quietLogger }).load().maxMinutes, DEFAULT_MAX_MINUTES);
});
