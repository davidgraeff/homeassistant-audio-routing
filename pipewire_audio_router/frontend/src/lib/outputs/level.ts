/**
 * Whether an output may be levelled or muted, and where its **prefix** matters.
 *
 * The *writes* no longer live here: one endpoint per output does it —
 * `PUT /api/outputs/{node}/volume` and `/mute`, on one scale (0.0–1.0), dispatched
 * daemon-side to whatever transport that output has. Before that there was one endpoint
 * per kind on three different scales, so this module had to pick, and picking wrongly was
 * silent: a `pwsink-dev-*` name sent to `PUT /api/sendspin/volume` was stored as an
 * intent for a device that will never connect and answered `ok: true`, so the click
 * looked accepted and the next pushed frame put the old value back — the "mute flips
 * back on its own" symptom. A wrong name is now a 404.
 *
 * What is left is the node-name prefixes (still the only kind marker a routing-graph node
 * carries) and the read side below.
 */
import type { LevelCaps, RoutingNode } from '../types';

export const AP2_DEV_PREFIX = 'ap2-dev-';
export const SENDSPIN_DEV_PREFIX = 'sendspin-dev-';
export const PWSINK_DEV_PREFIX = 'pwsink-dev-';

/**
 * ---- The read side: may this output be levelled or muted at all? -------------------
 *
 * The same question as above, asked before offering a control, and it has exactly one
 * right answer: **the daemon's**. It is the only party that knows whether a receiver agent
 * is on the other end, and the answer changes while the page is open — so it is published
 * per output on the routing matrix (`RoutingNode.level_caps`) rather than reconstructed
 * here.
 *
 * Two wrong ways to ask it, both of which shipped:
 *
 *   * **from the output's kind** — hid the control from every pw-sink host the agent could
 *     already drive, and (in the alignment wizard) offered a dead one for AirPlay 2;
 *   * **from `volume != null || muted != null`** — that is "has a level arrived?", a
 *     different question. It gave the right answer only because sendspin and AP2 always
 *     report *some* mute; an AP2 receiver whose level was never read has a perfectly good
 *     knob, and the day a kind reports an honest `muted: null` the control vanishes.
 *
 * The value-based test is kept as a fallback for one case only: a daemon older than
 * `level_caps` (an add-on not yet updated behind a newer card). It is the previous
 * behaviour, so nothing regresses.
 */
const inferredFromValues = (n: Pick<RoutingNode, 'volume' | 'muted'>): LevelCaps => ({
  volume: n.volume != null || n.muted != null,
  mute: n.volume != null || n.muted != null,
});

/** What the daemon says it can drive on this output, per output and per moment. */
export function levelCaps(n: Pick<RoutingNode, 'volume' | 'muted' | 'level_caps'>): LevelCaps {
  return n.level_caps ?? inferredFromValues(n);
}

/** Should a level control be offered for this output at all? */
export function hasAnyLevelControl(n: Pick<RoutingNode, 'volume' | 'muted' | 'level_caps'>): boolean {
  const caps = levelCaps(n);
  return caps.volume || caps.mute;
}
