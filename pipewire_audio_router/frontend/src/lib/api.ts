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
  AnnouncementGroup,
  AppSettings,
  AppSettingsUpdate,
  Encryption,
  MediaPlayerInfo,
  MusicGroup,
  NodesResponse,
  OpResponse,
  OutputInfo,
  RoutingMatrix,
  RtpSourceInfo,
  StatusInfo,
  SyncSettingsInfo,
  VolumeResponse,
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

  // Media players (volume). Ducked TTS/announce playback is driven by the Home
  // Assistant integration against the daemon's `/announce` endpoint, not the web
  // UI — the Outputs tab's Play tone / Play announcement buttons are the UI's
  // own (unducked) diagnostics (see `testTone`/`testAnnouncement`).
  mediaPlayers: () => request<MediaPlayerInfo[]>('GET', 'api/media_players'),
  setVolume: (nodeId: number, volume: number) =>
    request<VolumeResponse>('POST', `api/media_players/${nodeId}/volume`, { volume }),

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
  /** Diagnostic: play the calibration click once into this output's sink.
   * Blocks until the clip finishes; only a live sink (present RAOP) is a valid target. */
  testTone: (nodeName: string) =>
    request<OpResponse>('POST', `api/outputs/${encodeURIComponent(nodeName)}/test-tone`),
  /** Diagnostic: play the committed TTS test-announcement clip into this output's sink. */
  testAnnouncement: (nodeName: string) =>
    request<OpResponse>('POST', `api/outputs/${encodeURIComponent(nodeName)}/test-announcement`),

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

  // Named music/announcement groups (groups_store.rs).
  musicGroups: () => request<MusicGroup[]>('GET', 'api/groups/music'),
  createMusicGroup: (name: string, members: string[]) =>
    request<{ ok: boolean; group?: MusicGroup; message?: string }>('POST', 'api/groups/music', { name, members }),
  updateMusicGroup: (id: string, patch: { name?: string; members?: string[] }) =>
    request<{ ok: boolean; group?: MusicGroup; message?: string }>('PUT', `api/groups/music/${id}`, patch),
  deleteMusicGroup: (id: string) => request<OpResponse>('DELETE', `api/groups/music/${id}`),
  routeMusicGroup: (id: string, source: string) => request<OpResponse>('POST', `api/groups/music/${id}/route`, { source }),
  unrouteMusicGroup: (id: string) => request<OpResponse>('DELETE', `api/groups/music/${id}/route`),
  announcementGroups: () => request<AnnouncementGroup[]>('GET', 'api/groups/announcement'),
  createAnnouncementGroup: (name: string, targets: string[], priority: number, duck: number) =>
    request<{ ok: boolean; group?: AnnouncementGroup; message?: string }>('POST', 'api/groups/announcement', { name, targets, priority, duck }),
  updateAnnouncementGroup: (id: string, patch: { name?: string; targets?: string[]; priority?: number; duck?: number }) =>
    request<{ ok: boolean; group?: AnnouncementGroup; message?: string }>('PUT', `api/groups/announcement/${id}`, patch),
  deleteAnnouncementGroup: (id: string) => request<OpResponse>('DELETE', `api/groups/announcement/${id}`),
  announceToGroup: (announcementGroup: string) =>
    request<{ ok: boolean; admission: string; message: string }>('POST', 'api/announce', { announcement_group: announcementGroup, test: true }),

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
