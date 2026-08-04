// Live daemon state over one WebSocket, as a Svelte store. Auto-connects when
// first subscribed and reconnects on drop.
//
// The socket carries **typed frames** (bridge-daemon/src/routing.rs): the routing
// matrix, and the listings the Outputs page would otherwise have to re-fetch
// (`outputs`, `discovered`, `agents`). The matrix arrives on every registry change
// *and* every 250 ms for the live meters; the listing frames arrive only when their
// payload actually changed, which is why a component may react to a listing frame
// directly but must never re-fetch on a matrix frame.
//
// The REST endpoints stay the source of truth for the first paint: a page fetches
// on mount and this store keeps it fresh afterwards. Listing slices are therefore
// `null` until the daemon sends one — "nothing pushed yet", not "empty".

import { readable } from 'svelte/store';
import { wsUrl } from './api';
import type { AgentInfo, OutputInfo, RoutingMatrix } from './types';

export interface RoutingState {
  matrix: RoutingMatrix;
  /** Adopted outputs, as last pushed. `null` before the first push. */
  outputs: OutputInfo[] | null;
  /** Offered (discovered + ignored) outputs, as last pushed. */
  discovered: OutputInfo[] | null;
  /** Paired hosts and pending pair requests, as last pushed. */
  agents: AgentInfo[] | null;
  connected: boolean;
}

const EMPTY: RoutingMatrix = { sources: [], outputs: [], links: [] };

/** A frame as it comes off the wire, before we know which kind it is. `outputs`
 * means different things per kind — `RoutingNode[]` in a matrix frame,
 * `OutputInfo[]` in a listing frame — so it stays `unknown` here and is narrowed
 * once `type` is known. */
interface Frame {
  type?: 'matrix' | 'outputs' | 'discovered' | 'agents';
  outputs?: unknown;
  agents?: unknown;
}

export const routing = readable<RoutingState>(
  { matrix: EMPTY, outputs: null, discovered: null, agents: null, connected: false },
  (set) => {
    let ws: WebSocket | null = null;
    let stopped = false;
    let retry: ReturnType<typeof setTimeout> | null = null;
    // Held across reconnects so a dropped socket doesn't blank the page before the
    // new one has re-sent everything.
    let state: RoutingState = { matrix: EMPTY, outputs: null, discovered: null, agents: null, connected: false };

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
          // 'matrix', and an untyped frame from a daemon older than typed frames —
          // which *was* the matrix, bare.
          default:
            publish({ matrix: frame as unknown as RoutingMatrix, connected: true });
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
