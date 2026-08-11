// Typed REST client for the bridge daemon.
//
// Everything is resolved relative to the document's base URL, so the same
// build works served directly at `:8099/` and proxied under Home Assistant
// ingress at `/api/hassio_ingress/<token>/` — the API then lives at
// `<base>/api/...` in both cases.

import type {
  AgentInfo,
  AirplayClient,
  AlignGroup,
  AlignSessionMode,
  AlignState,
  AnnounceRequest,
  AnnounceResponse,
  AnnouncementGroup,
  AirplaySourceCfg,
  AppSettings,
  AppSettingsUpdate,
  DuckHold,
  MeasureMode,
  MeasureStatus,
  MicStatus,
  Refusal,
  SignalCheck,
  MusicGroup,
  NodesResponse,
  OpResponse,
  OutputInfo,
  RoutingMatrix,
  RtpSourceCfg,
  SendspinCodec,
  SourceKind,
  SourcesResponse,
  SourceView,
  StatusInfo,
  SyncSettingsInfo,
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

/** Push channel for the measurement run (plan §11: "progress should be pushed, not
 *  polled"). Sends one whole `MeasureStatus` on connect and one on every change.
 *  Kept here beside the REST client so both halves of the same endpoint are in one
 *  place; resolve it with `wsUrl()`, which handles HA ingress. */
export const MEASURE_WS_PATH = 'api/align/measure/ws';

/** Shortest name an output may be renamed to, mirroring the daemon's rule
 *  (outputs_store.rs `MIN_NAME_CHARS`) so the UI can refuse before the round trip
 *  instead of surfacing a 400. */
export const MIN_OUTPUT_NAME_CHARS = 3;

export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
    /** The parsed error body, when the endpoint sent a structured one. Kept
     *  because some endpoints say considerably more than a sentence: an alignment
     *  measurement answers with a whole `Refusal` (kind, the member to blame, the
     *  estimator's own verdict), and reducing that to `message` would throw away
     *  exactly the part that tells the user what to do — see `refusalOf`. */
    public body?: unknown,
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
    throw new ApiError(message, resp.status, data);
  }
  return data as T;
}

/** The `Refusal` an alignment-measurement call was rejected with, or `null` if the
 *  failure was something else (a network error, a 500, a non-JSON body).
 *
 *  Every `/api/align/measure*` rejection is a `Refusal` with a 400 or 409 — the
 *  daemon deliberately never 500s on one, because each is a state the user can act
 *  on (api.rs `refusal_status`). This is how the UI keeps the *kind* and the
 *  blamed member instead of collapsing everything into one sentence. */
export function refusalOf(e: unknown): Refusal | null {
  if (!(e instanceof ApiError)) return null;
  const b = e.body;
  if (!b || typeof b !== 'object') return null;
  const r = b as Partial<Refusal>;
  return typeof r.kind === 'string' && typeof r.message === 'string' ? (b as Refusal) : null;
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

  // Receiver agents (pwrouter-agent). Diagnostics only: a host waiting to pair is a
  // discovered `pwsink` output, so the *decisions* are the output calls below —
  // `adoptOutput` pairs it, `unpairOutput` revokes, `ignoreOutput` hides it.
  agents: () => request<AgentInfo[]>('GET', 'api/agents'),

  // Outputs: the devices the user has *added* (adopted). Everything else in the
  // app — the routing matrix, group editors, Align, the HA integration — means
  // these by "output".
  outputs: () => request<OutputInfo[]>('GET', 'api/outputs'),
  /** Devices discovery found but the user hasn't added: `state` is 'discovered'
   * or 'ignored'. Both come in one listing so the Outputs page's "show ignored"
   * checkbox filters client-side instead of refetching. */
  discoveredOutputs: () => request<OutputInfo[]>('GET', 'api/outputs/discovered'),
  /** Add a discovered device: routable from now on, and (with the setting on) an
   * HA media_player. Any routing it had before starts applying again.
   *
   * For a receiver host this is also the pairing: the daemon mints its token first
   * (plan §8), so "Pair" and "Add" are one click rather than two decisions. */
  adoptOutput: (nodeName: string) =>
    request<OpResponse>('POST', `api/outputs/${encodeURIComponent(nodeName)}/adopt`),
  /** Unpair a receiver host — the pw-sink form of removing. Revokes the token *and*
   * clears routing, group membership and adoption. Its agent keeps dialling in, so
   * the host returns under "Discovered" as pairable. */
  unpairOutput: (nodeName: string) =>
    request<OpResponse>('POST', `api/outputs/${encodeURIComponent(nodeName)}/unpair`),
  /** Dismiss a discovered device — hidden unless "show ignored" is on. Also
   * clears any routing / group membership it had. */
  ignoreOutput: (nodeName: string) =>
    request<OpResponse>('POST', `api/outputs/${encodeURIComponent(nodeName)}/ignore`),
  /** Remove an output: back to undecided. Stops being routable, loses its HA
   * media_player, and its routing + group membership are forgotten. Still on the
   * network ⇒ it reappears under "Discovered". */
  removeOutput: (nodeName: string) =>
    request<OpResponse>('DELETE', `api/outputs/${encodeURIComponent(nodeName)}`),
  /** Rename an output. The name is shown everywhere the output appears (here, the
   * routing graph, group chips, the HA media_player); `null` drops the override so
   * it goes back to its discovered name. Rejected below MIN_OUTPUT_NAME_CHARS. */
  renameOutput: (nodeName: string, name: string | null) =>
    request<OpResponse>('PUT', `api/outputs/${encodeURIComponent(nodeName)}/name`, { name }),
  /** Per-output render delay in ms (AirPlay 2); null resets to default. */
  setOutputLatency: (nodeName: string, latencyMs: number | null) =>
    request<OpResponse>('PUT', `api/outputs/${encodeURIComponent(nodeName)}/latency`, { latency_ms: latencyMs }),

  // AirPlay-2 wire sample-rate mode: 'auto' (negotiate 48 kHz, fall back to
  // 44.1 kHz) or 'fixed_44100'. Restarts that receiver's group at the new rate.
  setAp2RateMode: (nodeName: string, mode: 'auto' | 'fixed_44100') =>
    request<OpResponse>('PUT', `api/outputs/${encodeURIComponent(nodeName)}/ap2-rate`, { mode }),

  /** Per-sendspin-output wire codec. Rejected (with a reason) if that codec isn't
   * currently usable — the add-on can't encode it, or the device doesn't decode it. */
  setSendspinCodec: (nodeName: string, codec: SendspinCodec) =>
    request<OpResponse>('PUT', `api/outputs/${encodeURIComponent(nodeName)}/sendspin-codec`, { codec }),

  // Per-device announcement (announce.rs). Plays a clip (built-in test/tone,
  // or a url) to a set of per-device-sender outputs with duck/overlay —
  // the backend-agnostic path (Sendspin now, AirPlay 2 later), used by the
  // Outputs tab's Play tone / Play announcement diagnostics.
  announce: (req: AnnounceRequest) => request<AnnounceResponse>('POST', 'api/announce', req),

  // Duck holds (overlay_mixer.rs) — read-only here. Voice ducking is driven by
  // the Home Assistant integration, which is what knows a satellite's room; the
  // UI only *shows* what is ducked, so a speaker that sounds quiet is
  // explainable without reading the daemon log.
  duckHolds: () => request<DuckHold[]>('GET', 'api/duck'),

  // Per-source AirPlay senders (clients). Each AirPlay source id has its own
  // receiver, so these are scoped by the source id (encodeURIComponent'd).
  // `key` matches the sender by advertised name if known, else by IP.
  listSourceClients: (id: string) =>
    request<AirplayClient[]>('GET', `api/sources/${encodeURIComponent(id)}/clients`),
  forgetSourceClient: (id: string, key: string) =>
    request<OpResponse>('POST', `api/sources/${encodeURIComponent(id)}/clients/forget`, { key }),
  banSourceClient: (id: string, key: string, banned: boolean) =>
    request<OpResponse>('POST', `api/sources/${encodeURIComponent(id)}/clients/ban`, { key, banned }),
  setSourceClientPriority: (id: string, key: string, priority: number) =>
    request<OpResponse>('POST', `api/sources/${encodeURIComponent(id)}/clients/priority`, { key, priority }),
  disconnectSourceClient: (id: string, key: string) =>
    request<OpResponse>('POST', `api/sources/${encodeURIComponent(id)}/clients/disconnect`, { key }),
  setSourcePolicy: (id: string, preventTakeover: boolean) =>
    request<OpResponse>('PUT', `api/sources/${encodeURIComponent(id)}/policy`, { prevent_takeover: preventTakeover }),

  // Sendspin per-device volume (virtual outputs; volume is carried in-band over
  // the sendspin protocol, not a PipeWire node volume). Map is node_name -> 0-100.
  sendspinVolumes: () => request<Record<string, number>>('GET', 'api/sendspin/volumes'),
  setSendspinVolume: (nodeName: string, volume: number) =>
    request<OpResponse>('PUT', 'api/sendspin/volume', { node_name: nodeName, volume }),
  setSendspinMute: (nodeName: string, muted: boolean) =>
    request<OpResponse>('PUT', 'api/sendspin/mute', { node_name: nodeName, muted }),
  /** Ask one sendspin device to discard buffered-but-unplayed audio and re-anchor
   *  (`stream/clear`), without ending its stream or disturbing its groupmates.
   *  The recovery action for a device that is being sent audio and renders none. */
  sendspinClear: (nodeName: string) =>
    request<OpResponse>('POST', 'api/sendspin/clear', { node_name: nodeName }),

  // AirPlay-2 per-device volume/mute (virtual outputs; volume is an in-band RTSP
  // SET_PARAMETER to the receiver). Volume is 0.0–1.0. No receiver→daemon
  // feedback yet, so the matrix reflects the last-set level.
  setAp2Volume: (nodeName: string, volume: number) =>
    request<OpResponse>('PUT', 'api/ap2/volume', { node_name: nodeName, volume }),
  setAp2Mute: (nodeName: string, muted: boolean) =>
    request<OpResponse>('PUT', 'api/ap2/mute', { node_name: nodeName, muted }),

  // Sendspin per-device static delay (ms, 0-5000; 0 clears). Map node_name -> ms.
  sendspinDelays: () => request<Record<string, number>>('GET', 'api/sendspin/delays'),
  setSendspinDelay: (nodeName: string, delayMs: number) =>
    request<OpResponse>('PUT', 'api/sendspin/delay', { node_name: nodeName, delay_ms: delayMs }),

  // Group sync settings (daemon-wide presentation lead, ms).
  syncSettings: () => request<SyncSettingsInfo>('GET', 'api/sync/settings'),
  /** `opusFloorMs` is optional: the group lead alone is the common case, and the
   *  daemon leaves the floor untouched when it is absent. */
  setGroupLead: (groupLeadMs: number, opusFloorMs?: number) =>
    request<OpResponse>('PUT', 'api/sync/settings', {
      group_lead_ms: groupLeadMs,
      ...(opusFloorMs == null ? {} : { opus_floor_ms: opusFloorMs }),
    }),

  // General app settings (announce duck default, discovery on/off). PUT is a
  // partial update — send only the fields you're changing.
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
  /** `duck` omitted ⇒ the daemon applies the configured default duck level. */
  createAnnouncementGroup: (name: string, targets: string[], priority: number, duck?: number) =>
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
  /** Start a session on an arbitrary **selection of speakers** (plan §12.1): a
   *  temporary exclusive group is formed around exactly these, whatever they are
   *  routed to now. This is the wizard's entry point, and the mode travels with it
   *  so the session's promise is the one picked on the wizard's first page.
   *
   *  Costs a reconnect wave in both directions (§12.3.1), which is why the whole run
   *  forms **one** hold over its entire scope and then scopes each position by
   *  audibility — see `alignAudible`. */
  alignStartOutputs: (outputs: string[], mode: AlignSessionMode) =>
    request<AlignState>('POST', 'api/align/start', { outputs, mode }),
  /** Make exactly these members audible at `level`, muting the rest — plan §12.2's
   *  solo, generalised to a set. Live mutes, so this is free: it is how a run scopes
   *  a position without re-forming the hold. An empty set silences every member. */
  alignAudible: (audible: string[], level?: number) =>
    request<AlignState>('POST', 'api/align/audible', { audible, ...(level == null ? {} : { level }) }),
  alignSelect: (reference: string, target: string) =>
    request<AlignState>('POST', 'api/align/select', { reference, target }),
  alignVolume: (volume: number) => request<AlignState>('POST', 'api/align/volume', { volume }),
  alignStop: () => request<AlignState>('DELETE', 'api/align'),
  /** Microphone-ingest status (align_mic.rs). Polled only while capturing — it
   *  feeds the level meter, and a meter fed from the *daemon* is what proves the
   *  whole path works rather than just the browser's microphone. The capture
   *  socket itself is `wsUrl('api/align/mic/ws')`, driven by lib/mic.svelte.ts. */
  micStatus: () => request<MicStatus>('GET', 'api/align/mic'),

  /** Is the level good enough to measure? Grades the weaker of the click track's
   *  two tones by peak SNR — the meter above cannot answer this (see SignalCheck).
   *  Session-independent and side-effect free, so it is safe to poll while the
   *  user is still adjusting speaker volume. */
  micSignal: () => request<SignalCheck>('GET', 'api/align/mic/signal'),

  // Microphone-assisted alignment measurement (align_measure.rs, plan §11). One
  // run at a time, process-wide, riding beside the by-ear session above: it needs
  // that session playing the click track on every member off one clock.
  //
  // All five reject with a `Refusal` rather than a bare error string — use
  // `refusalOf(e)` on the thrown ApiError to keep the kind, the blamed member and
  // the estimator's own verdict.
  /** The whole run: phase, per-member progress, gate, observations, proposal,
   *  verification, refusal.
   *
   *  The push channel is `wsUrl(MEASURE_WS_PATH)` — one full status on connect and
   *  one per change. This stays the fallback rather than a legacy path: a wizard that
   *  shows nothing because the socket did not open is worse than one that polls, so
   *  `lib/measure.svelte.ts` polls until the socket has actually delivered something
   *  and goes back to polling if it drops. */
  measureStatus: () => request<MeasureStatus>('GET', 'api/align/measure'),
  /** Begin learning + measuring. Returns the run's first status (phase `arming`);
   *  everything after that arrives by polling `measureStatus`. `near_field` is
   *  refused with `mode_unsupported` by design — it is W8. */
  measureStart: (mode: MeasureMode) => request<MeasureStatus>('POST', 'api/align/measure/start', { mode }),
  /** Write the solved delays, then settle and verify. Never automatic (plan §11):
   *  the user has seen the deltas and their confidence first. Refused with the
   *  blocking check's own refusal when the proposal is blocked. */
  measureApply: () => request<MeasureStatus>('POST', 'api/align/measure/apply'),
  /** Restore the start-of-session delay snapshot (plan §9.4). The write phase is
   *  destructive to a previously-tuned setup, so this stays available afterwards —
   *  including after the run was abandoned. */
  measureRevert: () => request<MeasureStatus>('POST', 'api/align/measure/revert'),
  /** Abandon the run, leaving delays untouched. Any delays already written stay
   *  written (and revertable) — abandoning is not an undo. */
  measureAbandon: () => request<MeasureStatus>('DELETE', 'api/align/measure'),

  // Dynamic input sources — collection CRUD (multi-source refactor). Supersedes
  // the singular /api/source/{airplay,rtp} endpoints above. The backend routes
  // land in a later phase, so these 404 until then. `add`/`update` take a
  // partial per-kind config; the daemon fills defaults + allocates ports.
  // Returns the configured sources *and* the mDNS-discovered Bluetooth bridges
  // that none of them is listening for yet (see `SourcesResponse`). The daemon
  // re-probes stale bridge diagnostics pages while serving this, so `diag_ok` is
  // fresh — which is why the Sources tab reads the offer list from here rather
  // than from a separate endpoint.
  listSources: () => request<SourcesResponse>('GET', 'api/sources'),
  addSource: (body: {
    label: string;
    kind: SourceKind;
    airplay?: Partial<AirplaySourceCfg>;
    rtp?: Partial<RtpSourceCfg>;
  }) => request<SourceView>('POST', 'api/sources', body),
  updateSource: (
    id: string,
    body: { label?: string; airplay?: Partial<AirplaySourceCfg>; rtp?: Partial<RtpSourceCfg> },
  ) => request<SourceView>('PUT', `api/sources/${encodeURIComponent(id)}`, body),
  deleteSource: (id: string) =>
    request<OpResponse>('DELETE', `api/sources/${encodeURIComponent(id)}`),
};
