// Live daemon state over one WebSocket, as a Svelte store. Auto-connects when
// first subscribed and reconnects on drop.
//
// The socket carries **typed frames** (bridge-daemon/src/routing.rs): the routing
// matrix, the listings the Outputs page would otherwise have to re-fetch
// (`outputs`, `discovered`, `agents`), per-source now-playing metadata
// (`now_playing` — what each input is playing, shown on the source cards), and the
// fast lane — `meters`, the peaks and xrun counts that move
// without any graph change. **Every frame is deduped daemon-side**, so each arrives
// only when its payload actually changed; a component may react to any of them
// directly, but must never re-fetch on one.
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
import { wsUrl } from './api';
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

/** A frame as it comes off the wire, before we know which kind it is. `outputs`
 * means different things per kind — `RoutingNode[]` in a matrix frame,
 * `OutputInfo[]` in a listing frame — so it stays `unknown` here and is narrowed
 * once `type` is known. */
interface Frame {
  type?: 'matrix' | 'outputs' | 'discovered' | 'agents' | 'now_playing' | 'meters';
  outputs?: unknown;
  agents?: unknown;
  sources?: unknown;
  nodes?: unknown;
}

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
    let ws: WebSocket | null = null;
    let stopped = false;
    let retry: ReturnType<typeof setTimeout> | null = null;
    // Held across reconnects so a dropped socket doesn't blank the page before the
    // new one has re-sent everything.
    let state: RoutingState = INITIAL;

    const publish = (patch: Partial<RoutingState>) => {
      state = { ...state, ...patch };
      set(state);
    };

    const connect = () => {
      ws = new WebSocket(wsUrl('api/routing/ws'));
      ws.onopen = () => publish({ connected: true });
      ws.onmessage = (ev) => {
        let frame: Frame;
        try {
          frame = JSON.parse(ev.data) as Frame;
        } catch {
          return; // malformed frame: ignore rather than tear the socket down
        }
        switch (frame.type) {
          case 'outputs':
            publish({ outputs: frame.outputs as OutputInfo[], connected: true });
            break;
          case 'discovered':
            publish({ discovered: frame.outputs as OutputInfo[], connected: true });
            break;
          case 'agents':
            publish({ agents: frame.agents as AgentInfo[], connected: true });
            break;
          case 'now_playing':
            // Per-source now-playing metadata (docs/source-metadata-plan.md).
            // Replaced wholesale, not merged: the frame is the complete picture, so
            // a source that stopped playing is expressed by its absence.
            publish({ nowPlaying: (frame.sources ?? {}) as Record<string, NowPlaying>, connected: true });
            break;
          case 'meters':
            // The fast lane. A node absent from `nodes` has nothing to report, i.e.
            // zero — so this replaces the slice wholesale instead of merging into it,
            // which is what lets a level decay back to silence.
            publish({ meters: (frame.nodes ?? {}) as Record<string, MeterSample>, connected: true });
            break;
          case 'matrix':
            publish({ matrix: frame as unknown as RoutingMatrix, connected: true });
            break;
          default:
            // An untyped frame is a matrix from a daemon older than typed frames —
            // which *was* the matrix, bare. Anything else is a frame kind newer than
            // this UI: ignore it rather than mis-read it as a matrix.
            if (frame.type === undefined) {
              publish({ matrix: frame as unknown as RoutingMatrix, connected: true });
            } else {
              publish({ connected: true });
            }
        }
      };
      ws.onclose = () => {
        publish({ connected: false });
        if (!stopped) retry = setTimeout(connect, 2000);
      };
      ws.onerror = () => ws?.close();
    };
    connect();

    return () => {
      stopped = true;
      if (retry) clearTimeout(retry);
      ws?.close();
    };
  },
);

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
