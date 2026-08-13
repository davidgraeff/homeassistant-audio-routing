// Typed REST client for the bridge daemon.
//
// Everything is resolved relative to the document's base URL, so the same
// build works served directly at `:8099/` and proxied under Home Assistant
// ingress at `/api/hassio_ingress/<token>/` — the API then lives at
// `<base>/api/...` in both cases.

import type {
  AgentInfo,
  AirplayClient,
  AlignSessionMode,
  AlignState,
  AnnounceRequest,
  AnnounceResponse,
  AnnouncementGroup,
  AirplaySourceCfg,
  AppSettings,
  AppSettingsUpdate,
  DuckHold,
  MeasureChannels,
  MeasureLogList,
  MeasureMode,
  MeasureStatus,
  MicStatus,
  Refusal,
  SignalCheck,
  MusicGroup,
  NodesResponse,
  OpResponse,
  OutputInfo,
  Preset,
  PresetsInfo,
  RoutingMatrix,
  RtpSourceCfg,
  RunDocument,
  SendspinCodec,
  SourceKind,
  SourcesResponse,
  SourceView,
  StatusInfo,
  SyncSettingsInfo,
} from './types';

const BASE = new URL('.', document.baseURI);

/** Path-segment encoder. Node names and client keys go in the path now, so this is used
 *  by most of the client rather than a handful of calls. */
const enc = encodeURIComponent;

function httpUrl(path: string): string {
  return new URL(path.replace(/^\//, ''), BASE).toString();
}

/** WebSocket URL for `path`, on ws/wss matching the page protocol. */
export function wsUrl(path: string): string {
  const u = new URL(path.replace(/^\//, ''), BASE);
  u.protocol = u.protocol === 'https:' ? 'wss:' : 'ws:';
  return u.toString();
}

/** The one push socket. Topics are subscribed by message on it — see `lib/events.ts`,
 *  which owns the connection, the reconnect and the topic refcounting. Named here beside
 *  the REST client so both halves of the API are in one place; resolve it with `wsUrl()`,
 *  which handles HA ingress. */
export const EVENTS_WS_PATH = 'api/events';

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
    request<OpResponse>('DELETE', `api/routing/entity/${enc(nodeName)}`),

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
    request<OpResponse>('POST', `api/outputs/${enc(nodeName)}/adopt`),
  /** Unpair a receiver host — the pw-sink form of removing. Revokes the token *and*
   * clears routing, group membership and adoption. Its agent keeps dialling in, so
   * the host returns under "Discovered" as pairable. */
  unpairOutput: (nodeName: string) =>
    request<OpResponse>('POST', `api/outputs/${enc(nodeName)}/unpair`),
  /** Dismiss a discovered device — hidden unless "show ignored" is on. Also
   * clears any routing / group membership it had. */
  ignoreOutput: (nodeName: string) =>
    request<OpResponse>('POST', `api/outputs/${enc(nodeName)}/ignore`),
  /** Remove an output: back to undecided. Stops being routable, loses its HA
   * media_player, and its routing + group membership are forgotten. Still on the
   * network ⇒ it reappears under "Discovered". */
  removeOutput: (nodeName: string) =>
    request<OpResponse>('DELETE', `api/outputs/${enc(nodeName)}`),
  /** Rename an output. The name is shown everywhere the output appears (here, the
   * routing graph, group chips, the HA media_player); `null` drops the override so
   * it goes back to its discovered name. Rejected below MIN_OUTPUT_NAME_CHARS. */
  renameOutput: (nodeName: string, name: string | null) =>
    request<OpResponse>('PUT', `api/outputs/${enc(nodeName)}/name`, { name }),
  /** One output's level, `0.0`–`1.0` on every kind — the daemon converts to whatever
   *  that output's transport wants (sendspin 0–100 in-band, AP2's RTSP parameter, a
   *  PipeWire host's own cubic lever through its agent). A name whose kind has no such
   *  knob is a 400 and an unknown one a 404, so this can no longer be reached with the
   *  wrong kind — which used to be stored as an intent and answered `ok: true`. */
  setOutputVolume: (nodeName: string, volume: number) =>
    request<OpResponse>('PUT', `api/outputs/${enc(nodeName)}/volume`, { volume }),
  setOutputMute: (nodeName: string, muted: boolean) =>
    request<OpResponse>('PUT', `api/outputs/${enc(nodeName)}/mute`, { muted }),
  /** One output's timing knob in ms; `null` puts it back on its default.
   *
   *  **The polarity is per kind** and `GET /api/outputs` reports it: for sendspin this is
   *  an *advance* (the device plays that much earlier) and writing it costs that speaker a
   *  reconnect — tens of seconds of silence — while for AirPlay 2 and a PipeWire host it
   *  is a delay applied live. */
  setOutputDelay: (nodeName: string, delayMs: number | null) =>
    request<OpResponse>('PUT', `api/outputs/${enc(nodeName)}/delay`, { delay_ms: delayMs }),
  /** Ask one output to recover: a sendspin `stream/clear` (discard buffered audio and
   *  re-anchor, one frame, groupmates untouched) or a fresh AirPlay-2 session with its
   *  PTP peer re-armed. For an output that is reachable and being sent audio yet plays
   *  nothing. */
  resyncOutput: (nodeName: string) => request<OpResponse>('POST', `api/outputs/${enc(nodeName)}/resync`),

  // AirPlay-2 wire sample-rate mode: 'auto' (negotiate 48 kHz, fall back to
  // 44.1 kHz) or 'fixed_44100'. Restarts that receiver's group at the new rate.
  setAp2RateMode: (nodeName: string, mode: 'auto' | 'fixed_44100') =>
    request<OpResponse>('PUT', `api/outputs/${enc(nodeName)}/ap2-rate`, { mode }),

  /** Per-sendspin-output wire codec. Rejected (with a reason) if that codec isn't
   * currently usable — the add-on can't encode it, or the device doesn't decode it. */
  setSendspinCodec: (nodeName: string, codec: SendspinCodec) =>
    request<OpResponse>('PUT', `api/outputs/${enc(nodeName)}/sendspin-codec`, { codec }),

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
    request<AirplayClient[]>('GET', `api/sources/${enc(id)}/clients`),
  forgetSourceClient: (id: string, key: string) =>
    request<OpResponse>('DELETE', `api/sources/${enc(id)}/clients/${enc(key)}`),
  banSourceClient: (id: string, key: string, banned: boolean) =>
    request<OpResponse>('PUT', `api/sources/${enc(id)}/clients/${enc(key)}/ban`, { banned }),
  setSourceClientPriority: (id: string, key: string, priority: number) =>
    request<OpResponse>('PUT', `api/sources/${enc(id)}/clients/${enc(key)}/priority`, { priority }),
  disconnectSourceClient: (id: string, key: string) =>
    request<OpResponse>('POST', `api/sources/${enc(id)}/clients/${enc(key)}/disconnect`),
  setSourcePolicy: (id: string, preventTakeover: boolean) =>
    request<OpResponse>('PUT', `api/sources/${enc(id)}/policy`, { prevent_takeover: preventTakeover }),

  /** Desired sendspin volumes (0–100) by node name, sparse — the one listing that is
   *  genuinely per-kind: only sendspin keeps intent for a device it cannot reach. */
  sendspinVolumes: () => request<Record<string, number>>('GET', 'api/sendspin/volumes'),
  /** Desired sendspin static delays (ms) by node name, same rule. */
  sendspinDelays: () => request<Record<string, number>>('GET', 'api/sendspin/delays'),

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

  // Named music/announcement groups (store/groups.rs). `musicGroups` answers for
  // the **active** preset; a membership write may name another one (`preset` in
  // the patch) — that is how the preset bar edits a grouping that isn't in force.
  musicGroups: () => request<MusicGroup[]>('GET', 'api/groups/music'),
  createMusicGroup: (name: string, members: string[], preset?: string) =>
    request<{ ok: boolean; group?: MusicGroup; message?: string }>('POST', 'api/groups/music', { name, members, preset }),
  updateMusicGroup: (id: string, patch: { name?: string; members?: string[]; preset?: string }) =>
    request<{ ok: boolean; group?: MusicGroup; message?: string }>('PUT', `api/groups/music/${id}`, patch),
  deleteMusicGroup: (id: string) => request<OpResponse>('DELETE', `api/groups/music/${id}`),
  routeMusicGroup: (id: string, source: string) => request<OpResponse>('POST', `api/groups/music/${id}/route`, { source }),
  unrouteMusicGroup: (id: string) => request<OpResponse>('DELETE', `api/groups/music/${id}/route`),
  // Music-group presets: the whole grouping as a switchable thing. `activatePreset`
  // is the only one that moves speakers — the rest are edits to stored intent.
  presets: () => request<PresetsInfo>('GET', 'api/presets'),
  /** `copyFrom` seeds the new preset with that preset's grouping — what the UI
   *  passes by default, since a variant is nearly always an edit of one. */
  createPreset: (name: string, copyFrom?: string) =>
    request<{ ok: boolean; preset?: Preset; message?: string }>('POST', 'api/presets', { name, copy_from: copyFrom }),
  renamePreset: (id: string, name: string) =>
    request<{ ok: boolean; preset?: Preset; message?: string }>('PUT', `api/presets/${enc(id)}`, { name }),
  deletePreset: (id: string) => request<OpResponse>('DELETE', `api/presets/${enc(id)}`),
  activatePreset: (id: string) => request<OpResponse>('POST', `api/presets/${enc(id)}/activate`),

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

  // Latency alignment: the session that holds speakers, plays the click and owns
  // their levels and mutes. One at a time, process-wide.
  //
  // Only the *selection* entry point below is used: a session's identity is the set of
  // speakers the user picked in the alignment wizard (plan §12.1), in every mode including
  // by-ear. The daemon's other entry point (`POST /api/align/start {sources}`, "hold
  // whatever this source is playing to") has no client here on purpose — a source set
  // was the old framing, and two ways to name the one session is how two pages come to
  // believe they each own it.
  alignStatus: () => request<AlignState>('GET', 'api/align'),
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
  /** Measure one member through one channel of its stereo pair, or both again. */
  alignChannels: (nodeName: string, channels: MeasureChannels) =>
    request<AlignState>('POST', `api/align/members/${enc(nodeName)}/channel`, { channels }),
  /** Postpone the session's idle teardown by one whole allowance, changing nothing else
   *  (`AlignState.closes_in_s`).
   *
   *  **Only ever from a click.** The daemon's timeout exists so that a tab nobody is
   *  watching cannot leave a room muted, and an open socket or a status poll therefore
   *  counts for nothing — a forgotten *open* tab is the same hazard as a closed one. What
   *  makes this call legitimate is that a person pressed something a second ago; calling
   *  it on a timer would put the leak back, invisibly. */
  alignStillHere: () => request<AlignState>('POST', 'api/align/still-here'),
  alignStop: () => request<AlignState>('DELETE', 'api/align'),
  /** Microphone-ingest status (align/mic.rs). Polled only while capturing — it
   *  feeds the level meter, and a meter fed from the *daemon* is what proves the
   *  whole path works rather than just the browser's microphone. The capture
   *  socket itself is `wsUrl('api/align/mic/ws')`, driven by lib/mic.svelte.ts. */
  micStatus: () => request<MicStatus>('GET', 'api/align/mic'),

  /** Is the level good enough to measure? Grades the weaker of the click track's
   *  two tones by peak SNR — the meter above cannot answer this (see SignalCheck).
   *  Session-independent and side-effect free, so it is safe to poll while the
   *  user is still adjusting speaker volume. */
  micSignal: () => request<SignalCheck>('GET', 'api/align/mic/signal'),

  // Microphone-assisted alignment measurement (align/measure.rs, plan §11). One
  // run at a time, process-wide, riding beside the by-ear session above: it needs
  // that session playing the click track on every member off one clock.
  //
  // Every one of them rejects with a `Refusal` rather than a bare error string — use
  // `refusalOf(e)` on the thrown ApiError to keep the kind, the blamed member and
  // the estimator's own verdict.
  /** The whole run: phase, per-member progress, gate, observations, proposal,
   *  verification, refusal.
   *
   *  The push channel is the `measure` topic on `/api/events` — the current status on
   *  one per change. This stays the fallback rather than a legacy path: a wizard that
   *  shows nothing because the socket did not open is worse than one that polls, so
   *  `lib/measure.svelte.ts` polls until the socket has actually delivered something
   *  and goes back to polling if it drops. */
  measureStatus: () => request<MeasureStatus>('GET', 'api/align/measure'),
  /** Begin learning + measuring. Returns the run's first status (phase `arming`);
   *  everything after that arrives by polling `measureStatus`.
   *
   *  `chain` turns the multi-position mode into plan §1.1's **chain**: the run parks in
   *  `positioning` and takes one `measurePosition` per listening spot, then
   *  `measureFinish`. `false` is the single-position case — a chain with one step, which
   *  needs none of those calls. Ignored for `near_field`, which walks instead. */
  measureStart: (mode: MeasureMode, chain = false) =>
    request<MeasureStatus>('POST', 'api/align/measure/start', { mode, chain }),
  /** Near field's "I am at this speaker now" (plan §1, W8a).
   *
   *  **This call is the near-field measurement loop.** Nothing in a mixed capture says
   *  which speaker the phone is closest to — per-speaker excitation is a separate work
   *  package — so the user points, and the run then solos, levels, gates and measures
   *  that one member. One pass per speaker, in whatever order the user walks.
   *
   *  `level` overrides the level to measure at; omitted uses the level the session last
   *  applied to this speaker, i.e. whatever `alignAudible` was called with while the
   *  user stood there watching the signal check (plan §12.2 folds the level into each
   *  arrival, because at arm's length the danger is clipping, not being too quiet).
   *
   *  Refused — never a 500 — when the run is not a walk, is busy taking a reading, has
   *  already measured this speaker, or has never heard of it. */
  measureArrival: (nodeName: string, level?: number) =>
    request<MeasureStatus>('POST', `api/align/measure/arrival/${enc(nodeName)}`, level == null ? {} : { level }),
  /** Near field's **closure** reading: "I have walked back to the speaker I started at."
   *
   *  The difference between the anchor's two readings is the mic-vs-audio clock drift
   *  accumulated over the whole walk, and feeding it to the drift fit is what makes a
   *  one-pass walk trustworthy at all (plan §5.3). Refused until every member has been
   *  visited — a walk with a hole in it has nothing to close — and an implausible
   *  closure refuses the *whole* walk, because its correction went to every member. */
  measureClose: () => request<MeasureStatus>('POST', 'api/align/measure/close'),
  /** One listening position of a chain: `members` are the speakers to align from where
   *  the user is standing now, `overlaps` are already-aligned speakers still audible
   *  here (plan §1.1).
   *
   *  **Two overlaps rather than one** is the design intent, not a nicety: the shift this
   *  step derives from them is applied as a common delay to *every* speaker aligned so
   *  far and anchors everything measured afterwards, so with one overlap nothing checks
   *  it. One is accepted and reported as reduced confidence; none is refused for every
   *  position after the first (`overlap_missing`).
   *
   *  Returns immediately with the run marked busy — the measurement itself is watched
   *  through `measureStatus`. A refusal here refuses the **step**: the chain stays
   *  parked, everything already aligned keeps its provisional delays, and the same
   *  position can be posted again. */
  measurePosition: (members: string[], overlaps: string[]) =>
    request<MeasureStatus>('POST', 'api/align/measure/position', { members, overlaps }),
  /** "Every held speaker is aligned at some position": renormalise the whole chain (a
   *  common shift, so no relative alignment changes) and propose **the one write**.
   *  Refused while any held speaker is still unaligned. */
  measureFinish: () => request<MeasureStatus>('POST', 'api/align/measure/finish'),
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

  /** The stored run transcripts, newest first (plan §11).
   *
   *  Read *before* offering a download, not instead of it: the listing is what says
   *  whether anything was recorded at all (a daemon with no writable `/data` records
   *  nothing) and which run the newest one is — the measurement status carries no
   *  transcript id, so "this run's log" is only ever "the newest one", and a UI that
   *  claimed otherwise would be guessing. */
  measureLog: () => request<MeasureLogList>('GET', 'api/align/measure/log'),
  /** One whole transcript, by id or `latest`. Refused (with a `Refusal`) when that run
   *  has already been dropped by retention. */
  measureLogRun: (run: string) =>
    request<RunDocument>('GET', `api/align/measure/log?run=${enc(run)}`),

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
  ) => request<SourceView>('PUT', `api/sources/${enc(id)}`, body),
  deleteSource: (id: string) =>
    request<OpResponse>('DELETE', `api/sources/${enc(id)}`),
};
