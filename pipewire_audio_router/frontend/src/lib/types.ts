// Shapes returned by the bridge-daemon REST API (see docs/api-reference.md).

export interface RoutingNode {
  /** Stable node name — the primary key for routing (survives reloads/churn). */
  node_name: string;
  display_name: string;
  /** In the live graph right now. `false` = offline (configured/previously
   * routed but currently absent) — shown grayed; routing kept and reapplied.
   *
   * For the dialed backends this is *reachability*: an AP2 receiver answers on
   * :7000, and a pw-sink target advertises over mDNS, long before (or without
   * ever) accepting a session. Whether audio is really carried is `streaming`. */
  present: boolean;
  /** Outputs only: is a session to this output actually up, i.e. is audio routed
   * to it really being carried? `false` = present/reachable but nothing attached,
   * so the route delivers nothing and its wire must not animate. Absent when the
   * question doesn't apply: sources, and sendspin devices (which always have a
   * sender while adopted). Same rule as the Outputs page badge and the announce
   * arbiter. */
  streaming?: boolean | null;
  /** Outputs only: manually-configured (`true`) vs mDNS auto-discovered
   * (`false`) — drives the "auto" badge. Always `true` for sources. */
  configured: boolean;
  /** Live PipeWire object id when present (for volume calls); null offline. */
  node_id: number | null;
  /** Recent peak level 0.0–1.0 for the meter (sources, while the matrix is
   * open); 0 for outputs/unmetered. */
  peak: number;
  /** Current volume 0.0–1.0 for outputs whose volume the daemon tracks
   * out-of-band (sendspin devices) — pushed live over the routing WS so the
   * slider syncs, including physical changes the device reports. Absent for
   * sources. */
  volume?: number | null;
  /** Current mute state for outputs whose mute the daemon tracks out-of-band
   * (sendspin devices), pushed live over the routing WS. Absent otherwise. */
  muted?: boolean | null;
  /** Estimated buffering (ms) this node adds to the path — the configured
   * jitter/playout buffer, not a measured value. Sources: ingest jitter buffer
   * (RTP / AirPlay). Outputs: playout lead (sendspin group send-ahead + static
   * delay, or AP2 render delay). Absent when unknown. */
  latency_ms?: number | null;
  /** Cumulative xrun (dropped-cycle) count from the PipeWire profiler — the same
   * figure as `pw-top`'s ERR. Present only for real graph nodes while the graph
   * page is open (profiling is armed on demand); absent for virtual outputs and
   * when profiling is off. A rising value marks where dropouts originate. */
  xruns?: number | null;
}

export interface RoutingLink {
  source: string;
  output: string;
}

export interface RoutingMatrix {
  sources: RoutingNode[];
  outputs: RoutingNode[];
  /** Persisted routing intent, by stable name — the linked cells (may include
   * a link to a currently-offline endpoint, shown grayed). */
  links: RoutingLink[];
}

/** A named music group (groups_store.rs): outputs that play the same stream in
 * sync. Membership is exclusive (an output is in at most one music group). */
export interface MusicGroup {
  id: string;
  name: string;
  members: string[];
}

/** A named announcement group: reusable target outputs for announcements, with a
 * priority and duck level. Overlaps music/other announcement groups freely. */
export interface AnnouncementGroup {
  id: string;
  name: string;
  targets: string[];
  priority: number;
  duck: number;
}

export type Encryption = 'none' | 'RSA' | 'auth_setup';

/** Whether the user has added a discovered device as one of their outputs.
 * Discovery only *offers* a device: until it's adopted it isn't routable and
 * gets no Home Assistant media_player. */
export type OutputAdoption = 'adopted' | 'discovered' | 'ignored';

export interface OutputInfo {
  node_name: string;
  name: string;
  /** Is `name` one the user typed, rather than the one the device announces?
   *  Only then is there anything to clear, so only then does the Outputs page
   *  offer "use the announced name again". */
  renamed: boolean;
  /** 'airplay2' (AirPlay 2), 'sendspin', or 'pwsink' (remote PipeWire host via
   * module-rtp-session) — for the Type column / badge. */
  kind: 'airplay2' | 'sendspin' | 'pwsink';
  /** In the live graph now. */
  present: boolean;
  /** Manual store entry (`true`) vs mDNS auto-discovered (`false`). */
  configured: boolean;
  /** The user's verdict on this discovered device (daemon's outputs_store):
   * 'adopted' = one of our outputs (routable, exposed to Home Assistant),
   * 'discovered' = found on the network, awaiting a decision, 'ignored' =
   * dismissed. `GET /api/outputs` returns only 'adopted'; the discovered
   * listing returns the other two. */
  state: OutputAdoption;
  /** Connection details — known only for configured AirPlay entries (else null). */
  ip: string | null;
  port: number | null;
  encryption: string | null;
  /** Per-output latency override in ms; null = the type's built-in default
   * (1500 ms). For AirPlay 2 it's the render delay. Not meaningful for sendspin
   * (uses a separate static-delay knob). */
  latency_ms: number | null;
  /** AirPlay 2 only: PTP-lock health. true = receiver is exchanging gPTP with our
   * grandmaster; false = present but not exchanging gPTP; undefined = not an AP2
   * output / PTP not started. NB: realtime playback does NOT require a lock (a lone
   * receiver free-runs) — false is only alarming when `ptp_relevant` is true. */
  ptp_locked?: boolean;
  /** AirPlay 2 only: seconds since the last gPTP packet from the receiver (lock
   * age); undefined if never seen / not AP2. Small = healthy. */
  ptp_lock_age_s?: number;
  /** AirPlay 2 only: receiver advertises PTP support (features bit 41). undefined
   * if not AP2 / features unseen. A device that doesn't advertise PTP never locks. */
  ptp_supported?: boolean;
  /** AirPlay 2 only: a live PTP lock is actually relevant right now — i.e. the
   * receiver is in a ≥2-member AP2 group where drift is audible. undefined/false
   * ⇒ a lone realtime receiver, which plays fine unlocked. */
  ptp_relevant?: boolean;
  /** AirPlay 2 only: decoded capability flags from the `features` TXT (Diagnostics
   * card). undefined if not AP2 / features unseen. */
  ap2_features?: {
    /** Canonical `0xLOWER,0xUPPER` bitmask string. */
    raw: string;
    /** bit 41 — PTP timing supported. */
    ptp: boolean;
    /** bit 40 — buffered-audio mode (PTP mandatory in that mode). */
    buffered_audio: boolean;
    /** bit 48 — HomeKit transient pairing (how we connect). */
    transient_pairing: boolean;
  };
  /** AirPlay 2 only: wire sample-rate mode — 'auto' (negotiate 48 kHz, fall back
   * to 44.1 kHz) or 'fixed_44100'. undefined for non-AP2 outputs. */
  ap2_rate_mode?: 'auto' | 'fixed_44100';
  /** AirPlay 2 only: the effective wire rate (Hz) the output will use (48000 or
   * 44100), reflecting the mode + learned capability. undefined for non-AP2. */
  ap2_rate?: number;
  /** AirPlay 2 only: device-authoritative volume 0.0–1.0 — READ from the receiver
   * (or last user-set), or undefined/null when UNKNOWN (receiver didn't report and
   * the user hasn't set it). Show unknown honestly (blank / 0), never a fake 100%. */
  ap2_volume?: number | null;
  /** AirPlay 2 only: mute state. undefined for non-AP2. */
  ap2_muted?: boolean | null;
  /** pw-sink only: a remote module-rtp-session receiver has completed the
   * AppleMIDI handshake and is being streamed to right now. false = discovered +
   * routed but the receiver hasn't connected yet; undefined = not a pw-sink
   * output. Distinct from `present` (mDNS visibility). */
  pwsink_streaming?: boolean;
  /** pw-sink only: the host's own master volume (0.0-1.0) as reported by its
   * agent. Absent while no agent is connected — never fabricate a level, the
   * value belongs to that desktop. */
  pwsink_volume?: number;
  /** pw-sink only: the host's mute state, as reported. */
  pwsink_muted?: boolean;
  /** pw-sink only: which sink on that host plays our stream (display only). */
  pwsink_sink_name?: string;
  /** pw-sink only: whether the agent is currently ducking the host's *other*
   * applications for an announcement. */
  pwsink_ducked?: boolean;
  /** sendspin only: stored wire-codec choice — 'auto' | 'pcm' | 'opus' | 'flac'. */
  sendspin_codec?: SendspinCodec;
  /** sendspin only: the codec the stream actually uses (the choice narrowed by what
   * the add-on can encode and what the device advertised). */
  sendspin_codec_active?: string;
  /** sendspin only: every codec the picker offers, with availability + why not. */
  sendspin_codec_options?: CodecOption[];
  /** sendspin only: buffer this device asks us to keep queued (ms). Undefined until
   * it has connected and reported one; can change with the wire codec. */
  sendspin_min_buffer_ms?: number;
  /** sendspin only: startup lead it would like (ms) — diagnostics only. */
  sendspin_required_lead_ms?: number;
  /** sendspin only: send-ahead its stream actually uses (ms). */
  sendspin_send_ahead_ms?: number;
}

export type SendspinCodec = 'auto' | 'pcm' | 'opus' | 'flac';

/** One entry in a sendspin output's codec picker (`/api/outputs`). */
export interface CodecOption {
  codec: SendspinCodec;
  /** False ⇒ greyed out; `reason` says why. */
  available: boolean;
  reason?: string;
}

export interface SyncSettingsInfo {
  /** Group presentation lead in ms (sendspin `send_ahead`), as configured. */
  group_lead_ms: number;
  /** Largest buffering requirement reported by a present sendspin device
   * (`min_buffer_ms` + its static delay). The daemon raises every group's send-ahead
   * to at least this, so configuring less has no effect. 0 = nothing reported yet.
   * Can change when a device's wire codec changes (decode warmup differs). */
  group_lead_floor_ms: number;
  /** What the daemon actually uses: max(group_lead_ms, group_lead_floor_ms). */
  group_lead_effective_ms: number;
  /** Which devices set the floor, largest first — for explaining the number. */
  group_lead_floor_sources: LeadFloorSource[];
}

/** One device's contribution to the send-ahead floor. */
export interface LeadFloorSource {
  node_name: string;
  name: string;
  /** The codec it's streaming — its requirement changes with this. */
  codec: string;
  /** What the device itself asked for, if its firmware reports anything. */
  min_buffer_ms?: number;
  /** The add-on's own minimum for that codec, used when the device is silent. */
  codec_minimum_ms: number;
  /** Its speaker delay: it plays this much early, so audio must be sent that much
   * sooner or its chunks arrive already late and get dropped. */
  static_delay_ms: number;
  /** Its effective per-speaker send-ahead: the larger of the two, plus the delay. */
  required_ms: number;
  /** 'reported' or 'codec-minimum'. */
  reason: string;
}

/** General, daemon-wide app settings (settings_store.rs) — the Settings page. */
export interface AppSettings {
  /** Level surviving sources duck to for an announce that omits its own (0–1). */
  default_duck: number;
  /** Runtime mDNS discovery on/off. */
  discovery_enabled: boolean;
  /** Whether sendspin devices apply a static-delay change to the running stream.
   * Current ESPHome firmware does not, so a delay change restarts the group
   * stream; enable for future firmware that honors a live SetStaticDelay. */
  sendspin_delay_live: boolean;
  /** Whether the HA integration also exposes each individual output as its own
   * media_player entity. Default off: the integration creates one entity per
   * music group and per announcement group; this adds a per-output entity for
   * directly addressing a single speaker regardless of its group. */
  expose_outputs_as_media_players: boolean;
}

/** Partial settings update; omitted fields are left unchanged. */
export type AppSettingsUpdate = Partial<AppSettings>;

/** Host capability / weak-system assessment (`host_assessment.rs`). */
export type HostVerdict = 'adequate' | 'marginal' | 'underpowered';
export interface HostAssessment {
  /** Human-readable CPU model. */
  cpu_model: string;
  /** Number of logical CPUs. */
  cores: number;
  /** Target architecture, e.g. "aarch64", "x86_64". */
  arch: string;
  /** Total system RAM in MiB. */
  mem_total_mb: number;
  /** Whether the process can obtain realtime (SCHED_FIFO) scheduling. */
  rt_available: boolean;
  /** Coarse verdict for realtime multi-room audio. */
  verdict: HostVerdict;
  /** Short human-readable explanation of the verdict. */
  note: string;
}

/** Diagnostics status snapshot (`/api/status`). */
export interface StatusInfo {
  version: string;
  uptime_secs: number;
  /** Whether mDNS discovery is running right now. */
  discovery_enabled: boolean;
  /** Live PipeWire graph node count. */
  pipewire_nodes: number;
  /** mDNS-discovered sendspin devices currently tracked. */
  sendspin_devices: number;
  /** Persisted routing links. */
  routes: number;
  /** Host capability / weak-system assessment. */
  host: HostAssessment;
}

// ---- Latency alignment (calibrate.rs) -----------------------------------

export type AlignMemberKind = 'sendspin' | 'airplay2';

export interface AlignMember {
  node_name: string;
  kind: AlignMemberKind;
  /** Live PipeWire node id; null for virtual sendspin/AirPlay 2 devices. */
  node_id: number | null;
}

/** An alignable sync group (a source-set with its present members). */
export interface AlignGroup {
  sources: string[];
  members: AlignMember[];
}

/** Current calibration session state (`/api/align`). */
export interface AlignState {
  active: boolean;
  sources: string[];
  /** The fixed member everything is aligned against. */
  reference: string | null;
  /** The member currently being tuned (audible alongside the reference). */
  target: string | null;
  members: AlignMember[];
  /** Playback level (0–100) of the audible members. */
  volume: number;
}

/** One live PipeWire node (`/api/nodes`). */
export interface NodeInfo {
  node_id: number;
  node_name: string;
  media_class: string | null;
}

export interface NodesResponse {
  nodes: NodeInfo[];
  ports: unknown[];
}

/** A remembered AirPlay sender for one AirPlay source's receiver (the daemon's
 * `AirplayClientInfo`). Listed/managed via the per-source /clients endpoints. */
export interface AirplayClient {
  /** Stable identifier for forget/ban/priority calls: the name if known, else the IP. */
  key: string;
  /** Friendly device name once the sender advertised one; null if only seen by IP. */
  name: string | null;
  /** Most recent IP address this client connected from. */
  addr: string;
  /** Unix seconds of the most recent connection. */
  last_connected: number;
  /** Streaming to this AirPlay source right now. */
  connected: boolean;
  /** Future sessions from this client are refused (enforced at RTSP SETUP). */
  banned: boolean;
  /** Takeover priority: a higher-priority sender bumps a lower-priority one. */
  priority: number;
}

// ---- Dynamic input sources (multi-source refactor) ---------------------
// A source is either an AirPlay-receive endpoint or an RTP-receive endpoint.
// The daemon holds a keyed collection; the UI adds/edits/removes entries via
// the /api/sources CRUD endpoints (docs/multi-source-inputs-plan.md).

export type SourceKind = 'airplay' | 'rtp';

/** AirPlay-receive per-instance config. `port` is the RTSP port, allocated by
 *  the daemon on add (0 = allocate on next load) and stable across restarts. */
export interface AirplaySourceCfg {
  latency_msec: number;
  auth_setup: boolean;
  prevent_takeover: boolean;
  port: number;
}

/** RTP-receive per-instance config (same fields as the legacy single RTP source). */
export interface RtpSourceCfg {
  port: number;
  latency_msec: number;
  source_addr: string;
  ignore_ssrc: boolean;
  rate: number;
}

/** A Bluetooth→RTP bridge discovered over mDNS (`_pwrouter-btbridge._tcp`,
 *  published by `firmware/pi-bridge/setup_pi_bridge.py`).
 *
 *  The daemon cannot infer where an RTP source's audio comes from —
 *  `module-rtp-source` only knows the address it *listens* on — so the bridge
 *  announces itself. That buys two things: adopting a bridge with its stream
 *  parameters prefilled, and a link to its diagnostics page
 *  (`firmware/pi-bridge/bluetooth-testing-app/`).
 *
 *  **`diag_ok` gates the link, not `diag_url`.** The mDNS advert is installed by
 *  the setup script and outlives any particular run of the diagnostics app, so a
 *  bridge can be discovered while nothing is serving that port. The daemon probes
 *  it (verifying the response really is that app) before setting this. */
export interface BridgeInfo {
  /** mDNS instance fullname — stable identity to key on. */
  fullname: string;
  /** Human label from the advert (the bridge's Bluetooth / host name). */
  display_name: string;
  /** mDNS hostname, trailing dot trimmed. */
  hostname: string;
  /** Resolved address; `null` until mDNS resolves one (then `diag_url` is null too). */
  addr?: string | null;
  /** UDP port this bridge sends RTP to — what a source must listen on. */
  rtp_port: number;
  /** Its destination: this host's address, or a multicast group. */
  rtp_dest: string;
  rate: number;
  channels: number;
  /** Diagnostics page URL, or `null` while the address is unresolved. */
  diag_url?: string | null;
  /** The diagnostics app answered the last probe. Only offer the link if true. */
  diag_ok: boolean;
}

/** One configured input source as returned by the daemon. `present` = a live
 *  PipeWire node named `node_name` exists right now (generalizes the old
 *  "airplay running" / "rtp loaded" flags). Exactly one of `airplay`/`rtp` is
 *  populated, matching `kind`. */
export interface SourceView {
  id: string;
  label: string;
  kind: SourceKind;
  present: boolean;
  node_name: string;
  airplay?: AirplaySourceCfg | null;
  rtp?: RtpSourceCfg | null;
  /** The discovered bridge feeding this RTP source, when exactly one advertises
   *  its port (and multicast group). Null for AirPlay, when none advertises, and
   *  deliberately when *two* do — an ambiguous match would link to the wrong Pi. */
  bridge?: BridgeInfo | null;
}

/** `GET /api/sources`. `discovered_bridges` holds bridges no configured source
 *  is listening for — i.e. exactly what is missing, since an adopted bridge moves
 *  into its source's `bridge` field and out of this list. */
export interface SourcesResponse {
  sources: SourceView[];
  discovered_bridges: BridgeInfo[];
}

export interface MediaPlayerInfo {
  node_id: number;
  node_name: string;
  state: 'playing' | 'idle';
  volume: number | null;
}

export interface OpResponse {
  ok: boolean;
  message: string;
}

/** Per-device announcement (`POST /api/announce`, announce.rs): play a clip to a
 * set of output node names with per-device duck/overlay. Backend-agnostic — it
 * targets per-device senders (Sendspin today, AirPlay 2 later), not PipeWire
 * sink nodes. Provide exactly one audio source (here: `test` or `tone`). */
export interface AnnounceRequest {
  /** Output node names to announce to (e.g. `sendspin-dev-…`). */
  targets: string[];
  /** Built-in TTS test-announcement clip. */
  test?: boolean;
  /** Built-in calibration tone (a quick speaker-alive/wiring check). */
  tone?: boolean;
  /** Level (0–1) music ducks to while the clip plays; omit for the daemon default. */
  duck?: number;
}

export interface AnnounceResponse {
  ok: boolean;
  /** `"playing"` | `"queued"` | `"rejected"`. */
  admission: string;
  /** Queue position when `admission === "queued"`. */
  position?: number;
  /** Why it was rejected, when `admission === "rejected"`. */
  reason?: string;
  message: string;
}

export interface VolumeResponse {
  volume: number | null;
  message: string | null;
}

/** One row of `GET /api/agents`: a paired receiver host, or a pending pairing
 * request waiting for someone to approve it (docs/receiver-agent-plan.md §8).
 *
 * A pending row has no `node_name` — it has no routing identity until approved —
 * and carries the `code` the agent also logs, so the person approving can check
 * they are approving *that* host and not a stranger's request. */
export interface AgentInfo {
  /** `<machine-id>:<user>`; the handle every agent endpoint takes. */
  identity: string;
  /** `hostname (user)`. */
  label: string;
  node_name: string | null;
  paired: boolean;
  connected: boolean;
  code: string | null;
  state: {
    volume: number | null;
    muted: boolean | null;
    sink_name: string | null;
    receiving: boolean;
    ducked: boolean;
  } | null;
}
