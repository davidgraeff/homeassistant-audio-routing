// Live routing-matrix state over the daemon's WebSocket, as a Svelte store.
// Auto-connects when first subscribed and reconnects on drop; a fresh matrix
// snapshot arrives on every PipeWire registry change (no client polling).

import { readable } from 'svelte/store';
import { wsUrl } from './api';
import type { RoutingMatrix } from './types';

export interface RoutingState {
  matrix: RoutingMatrix;
  connected: boolean;
}

const EMPTY: RoutingMatrix = { sources: [], outputs: [], links: [] };

export const routing = readable<RoutingState>({ matrix: EMPTY, connected: false }, (set) => {
  let ws: WebSocket | null = null;
  let stopped = false;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let matrix = EMPTY;

  const connect = () => {
    ws = new WebSocket(wsUrl('api/routing/ws'));
    ws.onopen = () => set({ matrix, connected: true });
    ws.onmessage = (ev) => {
      try {
        matrix = JSON.parse(ev.data) as RoutingMatrix;
        set({ matrix, connected: true });
      } catch {
        /* ignore malformed frame */
      }
    };
    ws.onclose = () => {
      set({ matrix, connected: false });
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
});

/** Whether a given (source, output) pair is routed, by stable node name. */
export function isLinked(matrix: RoutingMatrix, source: string, output: string): boolean {
  return matrix.links.some((l) => l.source === source && l.output === output);
}
