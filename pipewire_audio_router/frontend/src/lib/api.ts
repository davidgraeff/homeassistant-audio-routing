// Typed REST client for the bridge daemon.
//
// Everything is resolved relative to the document's base URL, so the same
// build works served directly at `:8099/` and proxied under Home Assistant
// ingress at `/api/hassio_ingress/<token>/` — the API then lives at
// `<base>/api/...` in both cases.

import type {
  AddOutputRequest,
  AirplayClient,
  AirplaySourceInfo,
  AlignGroup,
  AlignState,
  AppSettings,
  AppSettingsUpdate,
  Encryption,
  MediaPlayerInfo,
  NodesResponse,
  OpResponse,
  OutputInfo,
  RoutingMatrix,
  RtpSourceInfo,
  StatusInfo,
  SyncSettingsInfo,
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
  /** Reconfigure a manually-added output's connection details (IP/port/encryption). */
  configureOutput: (nodeName: string, cfg: { ip: string; port: number; encryption: Encryption }) =>
    request<OpResponse>('PUT', `api/outputs/${encodeURIComponent(nodeName)}`, cfg),
  /** Per-RAOP-output receiver latency in ms (`raop.latency.ms`); null resets to default. */
  setOutputLatency: (nodeName: string, latencyMs: number | null) =>
    request<OpResponse>('PUT', `api/outputs/${encodeURIComponent(nodeName)}/latency`, { latency_ms: latencyMs }),

  // AirPlay-receive source (single)
  airplaySource: () => request<AirplaySourceInfo>('GET', 'api/source/airplay'),
  setAirplaySource: (name: string, latencyMsec: number, authSetup: boolean) =>
    request<OpResponse>('PUT', 'api/source/airplay', { name, latency_msec: latencyMsec, auth_setup: authSetup }),
  disableAirplaySource: () => request<OpResponse>('DELETE', 'api/source/airplay'),
  airplayClients: () => request<AirplayClient[]>('GET', 'api/source/airplay/clients'),
  forgetAirplayClient: (key: string) =>
    request<OpResponse>('POST', 'api/source/airplay/clients/forget', { key }),
  banAirplayClient: (key: string, banned: boolean) =>
    request<OpResponse>('POST', 'api/source/airplay/clients/ban', { key, banned }),
  setAirplayClientPriority: (key: string, priority: number) =>
    request<OpResponse>('POST', 'api/source/airplay/clients/priority', { key, priority }),
  disconnectAirplayClient: (key: string) =>
    request<OpResponse>('POST', 'api/source/airplay/clients/disconnect', { key }),
  setAirplayPolicy: (preventTakeover: boolean) =>
    request<OpResponse>('PUT', 'api/source/airplay/policy', { prevent_takeover: preventTakeover }),

  // Sendspin per-device volume (virtual outputs; volume is carried in-band over
  // the sendspin protocol, not a PipeWire node volume). Map is node_name -> 0-100.
  sendspinVolumes: () => request<Record<string, number>>('GET', 'api/sendspin/volumes'),
  setSendspinVolume: (nodeName: string, volume: number) =>
    request<OpResponse>('PUT', 'api/sendspin/volume', { node_name: nodeName, volume }),

  // Sendspin per-device static delay (ms, 0-5000; 0 clears). Map node_name -> ms.
  sendspinDelays: () => request<Record<string, number>>('GET', 'api/sendspin/delays'),
  setSendspinDelay: (nodeName: string, delayMs: number) =>
    request<OpResponse>('PUT', 'api/sendspin/delay', { node_name: nodeName, delay_ms: delayMs }),

  // Group sync settings (daemon-wide presentation lead, ms).
  syncSettings: () => request<SyncSettingsInfo>('GET', 'api/sync/settings'),
  setGroupLead: (groupLeadMs: number) =>
    request<OpResponse>('PUT', 'api/sync/settings', { group_lead_ms: groupLeadMs }),

  // General app settings (announce duck default, discovery on/off, default RAOP
  // latency). PUT is a partial update — send only the fields you're changing.
  settings: () => request<AppSettings>('GET', 'api/settings'),
  setSettings: (patch: AppSettingsUpdate) => request<OpResponse>('PUT', 'api/settings', patch),

  // Diagnostics status snapshot + live PipeWire graph.
  status: () => request<StatusInfo>('GET', 'api/status'),
  nodes: () => request<NodesResponse>('GET', 'api/nodes'),

  // Latency alignment (by-ear calibration of a sync group).
  alignGroups: () => request<AlignGroup[]>('GET', 'api/align/groups'),
  alignStatus: () => request<AlignState>('GET', 'api/align'),
  alignStart: (sources: string[]) => request<AlignState>('POST', 'api/align/start', { sources }),
  alignSelect: (reference: string, target: string) =>
    request<AlignState>('POST', 'api/align/select', { reference, target }),
  alignVolume: (volume: number) => request<AlignState>('POST', 'api/align/volume', { volume }),
  alignStop: () => request<AlignState>('DELETE', 'api/align'),

  // RTP source (single — Bluetooth bridge firmware target)
  rtpSource: () => request<RtpSourceInfo>('GET', 'api/source/rtp'),
  setRtpSource: (port: number, latencyMsec: number, sourceAddr: string, ignoreSsrc: boolean) =>
    request<OpResponse>('PUT', 'api/source/rtp', {
      port,
      latency_msec: latencyMsec,
      source_addr: sourceAddr,
      ignore_ssrc: ignoreSsrc,
    }),
  disableRtpSource: () => request<OpResponse>('DELETE', 'api/source/rtp'),
};
