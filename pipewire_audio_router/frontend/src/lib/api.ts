// Typed REST client for the bridge daemon.
//
// Everything is resolved relative to the document's base URL, so the same
// build works served directly at `:8099/` and proxied under Home Assistant
// ingress at `/api/hassio_ingress/<token>/` — the API then lives at
// `<base>/api/...` in both cases.

import type {
  AddOutputRequest,
  AirplaySourceInfo,
  MediaPlayerInfo,
  OpResponse,
  OutputInfo,
  RoutingMatrix,
  RtpSourceInfo,
  VolumeResponse,
  WyomingAnnounce,
} from './types';

const BASE = new URL('.', document.baseURI);

function httpUrl(path: string): string {
  return new URL(path.replace(/^\//, ''), BASE).toString();
}

/** WebSocket URL for `path`, on ws/wss matching the page protocol. */
export function wsUrl(path: string): string {
  const u = new URL(path.replace(/^\//, ''), BASE);
  u.protocol = u.protocol === 'https:' ? 'wss:' : 'ws:';
  return u.toString();
}

export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const resp = await fetch(httpUrl(path), {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await resp.text();
  const data = text ? JSON.parse(text) : null;
  if (!resp.ok) {
    const message =
      data && typeof data === 'object' && 'message' in data && (data as { message?: unknown }).message
        ? String((data as { message: unknown }).message)
        : `HTTP ${resp.status} ${resp.statusText}`;
    throw new ApiError(message, resp.status);
  }
  return data as T;
}

export const api = {
  // Routing matrix
  routing: () => request<RoutingMatrix>('GET', 'api/routing'),
  link: (source: string, output: string) =>
    request<OpResponse>('POST', 'api/routing/link', { source, output }),
  unlink: (source: string, output: string) =>
    request<OpResponse>('POST', 'api/routing/unlink', { source, output }),
  /** Forget all routing for an offline entity (matrix remove-✕). */
  forgetEntity: (nodeName: string) =>
    request<OpResponse>('DELETE', `api/routing/entity/${encodeURIComponent(nodeName)}`),

  // Media players (volume + announce)
  mediaPlayers: () => request<MediaPlayerInfo[]>('GET', 'api/media_players'),
  setVolume: (nodeId: number, volume: number) =>
    request<VolumeResponse>('POST', `api/media_players/${nodeId}/volume`, { volume }),
  announceUrl: (nodeId: number, url: string, duckVolume: number) =>
    request<OpResponse>('POST', `api/media_players/${nodeId}/announce`, { url, duck_volume: duckVolume }),
  announceWyoming: (nodeId: number, wyoming: WyomingAnnounce, duckVolume: number) =>
    request<OpResponse>('POST', `api/media_players/${nodeId}/announce`, { wyoming, duck_volume: duckVolume }),

  // RAOP outputs (CRUD)
  outputs: () => request<OutputInfo[]>('GET', 'api/outputs'),
  addOutput: (output: AddOutputRequest) => request<OpResponse>('POST', 'api/outputs', output),
  removeOutput: (nodeName: string) => request<OpResponse>('DELETE', `api/outputs/${nodeName}`),

  // AirPlay-receive source (single)
  airplaySource: () => request<AirplaySourceInfo>('GET', 'api/source/airplay'),
  setAirplaySource: (name: string, latencyMsec: number, authSetup: boolean) =>
    request<OpResponse>('PUT', 'api/source/airplay', { name, latency_msec: latencyMsec, auth_setup: authSetup }),
  disableAirplaySource: () => request<OpResponse>('DELETE', 'api/source/airplay'),

  // Sendspin per-device volume (virtual outputs; volume is carried in-band over
  // the sendspin protocol, not a PipeWire node volume). Map is node_name -> 0-100.
  sendspinVolumes: () => request<Record<string, number>>('GET', 'api/sendspin/volumes'),
  setSendspinVolume: (nodeName: string, volume: number) =>
    request<OpResponse>('PUT', 'api/sendspin/volume', { node_name: nodeName, volume }),

  // RTP source (single — Bluetooth bridge firmware target)
  rtpSource: () => request<RtpSourceInfo>('GET', 'api/source/rtp'),
  setRtpSource: (port: number, latencyMsec: number, sourceAddr: string) =>
    request<OpResponse>('PUT', 'api/source/rtp', { port, latency_msec: latencyMsec, source_addr: sourceAddr }),
  disableRtpSource: () => request<OpResponse>('DELETE', 'api/source/rtp'),
};
