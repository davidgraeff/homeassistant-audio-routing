// Live daemon state as a Svelte store, fed by the one push socket (`lib/events.ts`,
// `GET /api/events`).
//
// It subscribes to the topics this store is about — the routing `matrix`, the listings
// the Outputs page would otherwise re-fetch (`outputs`, `discovered`, `agents`),
// per-source `now_playing` metadata, and the fast lane `meters` (peaks and xrun counts,
// which move without any graph change). **Every topic is deduped daemon-side**, so a
// frame arrives only when its payload actually changed; a component may react to any of
// them directly, but must never re-fetch on one.
//
// The subscriptions live and die with this store's own subscribers, which is the point of
// per-topic subscription: with nobody looking at the graph the daemon is not asked for a
// matrix, and — because arming metering *is* the `meters` subscription — it is not taking
// peak taps or running the PipeWire profiler either.
//
// `meters` is deliberately kept as its own slice rather than merged into
// `matrix.sources`/`.outputs`: those arrays keep their identity between real graph
// changes, so the graph's derived layout does not recompute four times a second just
// because a level moved. Read the live figures through `peakOf`/`xrunsOf`.
//
// The REST endpoints stay the source of truth for the first paint: a page fetches
// on mount and this store keeps it fresh afterwards. Listing slices are therefore
// `null` until the daemon sends one — "nothing pushed yet", not "empty".

import { readable } from 'svelte/store';
import { onConnection, onTopic } from './events';
import type { AgentInfo, NowPlaying, OutputInfo, RoutingMatrix } from './types';

/** A node's live figures from the `meters` frame. A field is absent when it is
 *  zero — see `peakOf`/`xrunsOf`. */
export interface MeterSample {
  peak?: number;
  xruns?: number;
}

export interface RoutingState {
  matrix: RoutingMatrix;
  /** Adopted outputs, as last pushed. `null` before the first push. */
  outputs: OutputInfo[] | null;
  /** Offered (discovered + ignored) outputs, as last pushed. */
  discovered: OutputInfo[] | null;
  /** Paired hosts and pending pair requests, as last pushed. */
  agents: AgentInfo[] | null;
  /** Live peaks/xruns by node name, as last pushed. Nodes with nothing to report
   *  are absent, so this is empty while the house is silent. */
  meters: Record<string, MeterSample>;
  /** What each source is playing, by source node name, as last pushed. A source
   *  with nothing playing is absent — the frame is always the complete picture, so
   *  an absent entry means "cleared", not "unchanged". */
  nowPlaying: Record<string, NowPlaying>;
  connected: boolean;
}

const EMPTY: RoutingMatrix = { sources: [], outputs: [], links: [] };

const INITIAL: RoutingState = {
  matrix: EMPTY,
  outputs: null,
  discovered: null,
  agents: null,
  meters: {},
  nowPlaying: {},
  connected: false,
};

export const routing = readable<RoutingState>(INITIAL, (set) => {
  // Held across reconnects so a dropped socket doesn't blank the page before the new
  // one has re-sent everything.
  let state: RoutingState = INITIAL;
  const publish = (patch: Partial<RoutingState>) => {
    state = { ...state, ...patch };
    set(state);
  };

  const off = [
    onConnection((connected) => publish({ connected })),
    // The matrix frame is flat — it *was* the whole frame once — so the payload is the
    // frame itself.
    onTopic('matrix', (p) => publish({ matrix: p as RoutingMatrix })),
    onTopic('outputs', (p) => publish({ outputs: p as OutputInfo[] })),
    onTopic('discovered', (p) => publish({ discovered: p as OutputInfo[] })),
    onTopic('agents', (p) => publish({ agents: p as AgentInfo[] })),
    // Replaced wholesale, not merged: the frame is the complete picture, so a source
    // that stopped playing is expressed by its absence.
    onTopic('now_playing', (p) => publish({ nowPlaying: (p ?? {}) as Record<string, NowPlaying> })),
    // Same rule, and it is what lets a level decay back to silence: a node absent from
    // the frame has nothing to report, i.e. zero.
    onTopic('meters', (p) => publish({ meters: (p ?? {}) as Record<string, MeterSample> })),
  ];

  return () => {
    for (const stop of off) stop();
  };
});

/** Whether a given (source, output) pair is routed, by stable node name. */
export function isLinked(matrix: RoutingMatrix, source: string, output: string): boolean {
  return matrix.links.some((l) => l.source === source && l.output === output);
}

/** Live peak (0–1) for a node. Absent from the `meters` frame means no signal, so
 *  this is the only correct way to read a level — `RoutingNode.peak` is just the
 *  sample that happened to be taken when the matrix was last built. */
export function peakOf(state: RoutingState, nodeName: string): number {
  return state.meters[nodeName]?.peak ?? 0;
}

/** Live cumulative xrun count for a node, or `undefined` when it has never dropped
 *  a cycle (or profiling is off) — which is the case the UI renders as "no badge",
 *  and the baseline its rising-edge detection starts from. */
export function xrunsOf(state: RoutingState, nodeName: string): number | undefined {
  return state.meters[nodeName]?.xruns;
}

/** What a source is playing, or `null` when nothing is (or nothing was reported).
 *
 *  An entry with no title, artist or album is deliberately treated as nothing: a
 *  bare `stopped` is a producer mid-teardown, and rendering a row for it would give
 *  the card an empty second line. */
export function nowPlayingOf(state: RoutingState, nodeName: string): NowPlaying | null {
  const np = state.nowPlaying[nodeName];
  if (!np || !(np.title || np.artist || np.album)) return null;
  return np;
}

/** The artwork URL for a source, or `null`. Both kinds are usable directly as an
 *  `<img src>`: the embedded path is daemon-relative (so it also works behind Home
 *  Assistant ingress), and a producer-supplied URL is absolute. */
export function artworkOf(np: NowPlaying | null): string | null {
  if (!np?.artwork) return null;
  return np.artwork.kind === 'url' ? np.artwork.url : np.artwork.path;
}
