// Shapes returned by the bridge-daemon REST API (see docs/api-reference.md).

/** What one source is currently playing, from the socket's `now_playing` frame
 *  (bridge-daemon/src/now_playing.rs). Keyed by source node name. Every
 *  descriptive field is optional: producers differ in what they can say — an
 *  AirPlay sender gives title/artist/album and cover art, AVRCP gives no artwork
 *  at all, and YouTube Music gives one combined title. */
export interface NowPlaying {
  state: 'playing' | 'paused' | 'stopped';
  title?: string;
  artist?: string;
  album?: string;
  duration_ms?: number;
  position_ms?: number;
  /** Unix ms at which `position_ms` was true. A consumer that wants a moving
   *  position extrapolates from this; the daemon publishes at most every 5 s. */
  position_updated_at?: number;
  artwork?:
    | { kind: 'url'; url: string }
    /** `path` is daemon-relative and already rev-stamped, so it can be used as an
     *  `<img src>` as-is — including behind Home Assistant ingress. */
    | { kind: 'embedded'; rev: number; mime: string; len: number; path: string };
}

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
  /** Current volume 0.0–1.0, pushed live over the routing WS so the slider syncs
   * (including physical changes a device reports).
   *
   * **This field is the capability contract the UI gates on.** It is populated
   * exactly when the daemon can drive that output's level — sendspin and AirPlay 2
   * in-band, a pw-sink host through the receiver agent — and `null`/absent when it
   * genuinely cannot: an agent-less host, a sink with neither a device route nor node
   * volume, or a source. So render a volume control iff `volume != null || muted !=
   * null`; do **not** test the output kind. Enumerating kinds is what previously hid
   * the control from every pw-sink host the daemon could already drive.
   *
   * `null` on an output that *does* have a control means "level not reported yet, but
   * settable" — <VolumeControl> keeps the slider live and only changes its tooltip. */
  volume?: number | null;
  /** Current mute state, same capability contract as `volume` above. Deliberately
   * `null` rather than `false` when unknown: a missing agent reading as "unmuted"
   * would put a mute button on screen that silently does nothing. */
  muted?: boolean | null;
  /** Outputs only: the diagnosed reason this output can't carry audio right now;
   * absent when nothing is known to be wrong. Turns "not connected" from a state
   * you have to guess about into one with a stated cause. AirPlay 2 only so far. */
  last_error?: string | null;
  /** Outputs only: a speaker-timing measurement holds this output right now, so
   * nothing routed to it is playing (the hold is exclusive). Absent — not `false` —
   * when nothing is aligning, so a badge keyed on its presence needs no extra test.
   *
   * It arrives on the matrix rather than from the alignment API on purpose: this is
   * the channel the daemon already pushes on every change, so the state appears and
   * **disappears** with the hold. A page that polled the session for it once claimed
   * a hold the idle timeout had already released. */
  held?: boolean | null;
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
  /** Per-output latency override in ms; null = the type's built-in default.
   * For AirPlay 2 it's the render delay; for a PipeWire host it's the receiver's
   * playout delay (its jitter buffer). Not meaningful for sendspin (uses a
   * separate static-delay knob). */
  latency_ms: number | null;
  /** What this output is actually running: the override above, or the daemon's
   * default when there is none. Read this for slider positions and "reset to
   * default" hints instead of hardcoding a default in the UI. null for kinds with
   * no such knob (sendspin). */
  latency_effective_ms?: number | null;
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
  /** pw-sink only: is this host's agent paired? false = it is asking to be, which
   * is what a *discovered* pw-sink output is — pairing it is the Add for this kind.
   * undefined = not a pw-sink output. */
  pwsink_paired?: boolean;
  /** pw-sink only: the code to compare with the one that machine's own agent logged,
   * before pairing it. Present only while it is waiting to be paired. */
  pwsink_pair_code?: string;
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
  /** Why this output can't play right now, as a sentence to show the user; absent
   * when nothing is known to be wrong. AirPlay 2 only so far (set by the daemon's
   * liveness probe or a failed connect). Before this existed, a receiver that
   * refused every connection looked identical to a working one in the UI. */
  last_error?: string;
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
  /** Decode+network headroom every **Opus** stream gets, in ms — the term that keeps
   * an Opus group above `group_lead_ms` however low that is set. Tunable, because the
   * shipped 250 ms is a conservative guess rather than a measurement. */
  opus_floor_ms: number;
  /** Lowest value the daemon accepts for `opus_floor_ms`: the Opus block size, since
   * nothing can be sent before a whole block exists. */
  opus_floor_min_ms: number;
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

// ---- Latency alignment (align/calibrate.rs) ------------------------------

/** What kind of speaker an alignment member is (`align::calibrate::MemberKind`).
 *
 *  Useful for naming a member and for its **delay** knob — a sendspin device's knob is an
 *  advance and has a different range, a pw-sink playout delay has a hard floor. It is
 *  **not** what decides whether a member can be levelled or silenced: that is a per-output
 *  capability the daemon resolves per position and publishes as
 *  `AlignState.level_channels` (plan §7, §12.3.2). This doc used to claim pw-sink had
 *  neither a level nor a mute; a host's agent supplies the level (W20) and the relay
 *  supplies a universal mute (W17), so both halves were wrong and any UI branching on the
 *  kind for those two questions is wrong with them. */
export type AlignMemberKind = 'sendspin' | 'airplay2' | 'pwsink';

export interface AlignMember {
  node_name: string;
  kind: AlignMemberKind;
  /** Live PipeWire node id; null for virtual sendspin/AirPlay 2 devices. */
  node_id: number | null;
}

/** Which acoustic promise a *session* is making (align/group.rs `AlignMode`).
 *
 *  Not the same enum as `MeasureMode` below, and deliberately so: this is the
 *  session's promise (including `manual`, which runs no measurement at all), while
 *  `MeasureMode` is what the measurement state machine implements. `multi_position`
 *  and `sweet_spot` are the same promise under the two names. */
export type AlignSessionMode = 'multi_position' | 'near_field' | 'manual';

/** Why one member's reading has to be thrown away: something legitimately
 *  outranked the alignment's exclusive hold (plan §12.3).
 *
 *  Exclusivity is deliberately not absolute — a barge-in announcement and a voice
 *  duck both win, because nobody wants a fire alarm suppressed by a calibration.
 *  The report is the load-bearing part: without it the gate catches the same event
 *  as unstable amplitude and blames the user's hand for what a doorbell did. */
export type InterferenceCause =
  | { kind: 'barge_in'; announcement: number }
  | { kind: 'duck_hold'; hold: number };

export interface Interference {
  /** The held output it happened on. */
  member: string;
  cause: InterferenceCause;
  /** Milliseconds since the hold was formed. */
  at_ms: number;
  /** The sentence to show, pre-rendered by the daemon so every consumer quotes the
   *  same one. Show it verbatim. */
  reason: string;
}

/** Current calibration session state (`/api/align`). */
export interface AlignState {
  active: boolean;
  /** The session's identity: the selected output set for a session started from the
   *  alignment wizard — which is every session this UI starts — or the source set when one
   *  was started through `POST /api/align/start {sources}` by something else. Either
   *  way, the thing whoever started it picked. */
  sources: string[];
  /** The fixed member everything is aligned against. */
  reference: string | null;
  /** The member currently being tuned (audible alongside the reference). */
  target: string | null;
  members: AlignMember[];
  /** Playback level (0–100) last applied — the fallback for a member the session has
   *  not given a level to yet. */
  volume: number;
  /** Per-member calibration level (0–100), keyed by node name.
   *
   *  Session-owned so it survives a page reload; a member the session has never applied
   *  a level to is **absent** (not 0, not null), so read `levels[node] ?? volume`.
   *  Deliberately not persisted across sessions: the right level depends on where the
   *  phone is standing, so a stored value would be a good seed and a bad promise. */
  levels: Record<string, number>;
  /** Which acoustic promise this session is making. */
  mode: AlignSessionMode;
  /** The outputs held exclusively for the session — its temporary group. */
  outputs: string[];
  /** The members audible right now: one while a level is being set or a member
   *  measured, two for the by-ear comparison. */
  audible: string[];
  /** Exclusivity violations recorded so far, newest last. A *peek* — reading this
   *  does not clear it. */
  interference: Interference[];
  /** The routing this session is displacing while it holds these speakers: what
   *  the UI shows as "these speakers will stop playing what they play now". */
  displaced: RoutingLink[];
  /** How much longer the session may sit **idle** before the daemon tears it down and
   *  gives the speakers back — whole seconds **relative to this frame**, `null` when
   *  nothing is running.
   *
   *  Relative rather than an absolute instant on purpose: the browser's clock and the
   *  daemon's differ by an unknown amount, so the client turns this into its *own*
   *  deadline on arrival, counts down locally, and re-syncs on the next frame.
   *
   *  **`0` does not mean gone.** It means the deadline has passed and the watchdog will
   *  take the session at its next check, up to `timeout_slack_s` later — so the
   *  disappearance is rendered when a frame says `active: false`, never by this hitting
   *  zero.
   *
   *  What refreshes it is doing something *to* the run, not looking at it: soloing,
   *  changing a level, a re-scoping start, or the deliberate `POST
   *  /api/align/still-here`. Reading a proposal refreshes nothing, and neither does
   *  holding the status socket open — which is the point, since a forgotten open tab is
   *  the same hazard as a closed one. */
  closes_in_s: number | null;
  /** The whole idle allowance, in seconds — what `closes_in_s` counts down from. Read
   *  from here rather than hard-coded so the sentence the UI writes ("15 minutes without
   *  a change") cannot drift from the daemon's own number. */
  idle_timeout_s: number;
  /** How much *later* than `closes_in_s` the close can actually happen, because the
   *  daemon's watchdog is a poller. This is the size of the word "about": a UI must not
   *  count a user down to a precise second it does not have. */
  timeout_slack_s: number;
  /** How each member's level is reached **at this position**, keyed by node name —
   *  the daemon's own resolved answer (`align::calibrate::LevelChannel`).
   *
   *  Read this; never re-derive it from `AlignMember.kind`. It is a **per-output**
   *  capability, not a property of the transport (plan §7, §12.3.2): a pw-sink host with
   *  a live receiver agent is levellable and the same host without one is not, so two
   *  members of one kind differ and one member's answer changes when its agent drops
   *  mid-walk. A UI that guesses from the kind gets it wrong in both directions — it
   *  hides a slider that works, and offers one that writes into nothing.
   *
   *  Absent (or a node not in the map) means **not resolved yet**: no session, or a member
   *  that has not been through an audibility pass. That is "unknown", never "none". */
  level_channels: Record<string, LevelChannel>;
  /** Members with no level knob this daemon can reach — the `none` entries above, as a
   *  list. They constrain the others' levels instead of being tuned. */
  unlevellable: string[];
  /** One sentence saying what that costs, written by the daemon so every consumer says
   *  the same thing; null when every member has a level knob. The part users do not
   *  guess: such a member sets the clip ceiling, so turning the *others* down cannot
   *  rescue a measurement it is spoiling. */
  level_note: string | null;
}

/** How one member's playback level is reached while the session runs — the daemon's
 *  resolved per-output answer (`align::calibrate::LevelChannel`).
 *
 *  There is deliberately no `relay` variant as there is for the *mute*: the relay hook has
 *  a mute and no gain, so `none` is a real outcome rather than a degraded one. The mute has
 *  a universal fallback and therefore needs no reporting — every member can be silenced. */
export type LevelChannel =
  /** sendspin's live per-device level. */
  | 'sendspin_live'
  /** The AP2 receiver's own volume, imposed for the session's duration and given back at
   *  teardown. */
  | 'ap2_snapshot'
  /** The host's sink level over its receiver agent — same borrow-and-restore shape as
   *  `ap2_snapshot`, and only restorable because the host reports it. */
  | 'out_of_band'
  /** Nothing this daemon can reach: no agent answering, a sink with no volume lever, or a
   *  future output kind. */
  | 'none';

/** Microphone-ingest status (`/api/align/mic`, align/mic.rs) — what the level
 *  meter and the capture pre-flight read. Counters cover the *current* capture;
 *  a reconnect starts them over. */
/** Whether the *level* is good enough to measure — which the meter cannot say,
 *  because `MicStatus.peak` is a decaying broadband peak read against an 8 ms
 *  burst once per second. `GET api/align/mic/signal`. */
export interface SignalCheck {
  verdict: 'good' | 'marginal' | 'too_quiet' | 'unusable';
  /** One sentence naming the problem and the action. Show it verbatim. */
  message: string;
  sample_rate: number;
  periods: number;
  gap: boolean;
  clipped: boolean;
  /** The channel that decides the verdict; null when nothing was analysed. */
  worst_peak_snr_db: number | null;
  channels: SignalChannel[];
}

export interface SignalChannel {
  label: string;
  center_hz: number;
  peak_snr_db: number;
  second_peak_ratio: number;
  /** Meaningless in isolation — shown only so instability is visible. */
  phase_ms: number;
  periods_used: number;
}

export interface MicStatus {
  connected: boolean;
  /** Rate the capture declared; 0 before the first one. */
  sample_rate: number;
  frames_received: number;
  blocks_received: number;
  /** Sequence discontinuities: any window spanning one is unusable. */
  gap_count: number;
  /** Decaying peak level, 0.0–1.0. */
  peak: number;
  /** Sticky: a measurement is refused on a capture that clipped at all. */
  clipped: boolean;
  clip_count: number;
  buffered_frames: number;
  capacity_frames: number;
}

// ---- Microphone-assisted alignment measurement (align/measure.rs) --------
// The measurement run rides *beside* the by-ear session of `AlignState` above:
// it needs that session playing the click track, and it solos one member at a
// time through the same live-mute machinery. Everything here mirrors
// bridge-daemon/src/align/measure.rs field-for-field; the plan is
// docs/mic-alignment-plan.md §§5, 8-11.

/** Which acoustic promise a run makes (plan §1). Both are implemented and
 *  orchestrated differently: `sweet_spot` measures every member itself from wherever
 *  the phone is sitting (optionally once per listening position — see `ChainProgress`),
 *  while `near_field` is driven by the *user* walking to each speaker in turn (see
 *  `WalkProgress`). They align different things, which is why one is never silently
 *  substituted for the other.
 *
 *  What the daemon still refuses is `mode_unsupported` on a near-field run asked to
 *  **link to an earlier run's** aligned set (W8b): nothing stores a finished run's
 *  delays, so there is nothing to propagate a shift into. The UI never sends that. */
export type MeasureMode = 'sweet_spot' | 'near_field';

/** Plan §8's state machine. `proposed` is the state that parks the run waiting
 *  for the user, which is what makes `apply` an explicit step (§11).
 *
 *  `positioning` and `walking` are the two states in which the run is **alive but
 *  waiting for the person holding the phone** — a chain parked between listening
 *  positions, a walk parked between speakers. Neither is terminal: the group is still
 *  held, the click is still playing, and a chain's aligned set is carrying provisional
 *  delays that a second run would strand. So they must not be rendered as "finished"
 *  (see `isRunning` in lib/measure.svelte.ts). */
export type MeasurePhase =
  | 'idle'
  | 'arming'
  | 'learning'
  /** Near field: parked, waiting for "I am at this speaker now". */
  | 'walking'
  /** Chained multi-position: parked, waiting for the next position. */
  | 'positioning'
  | 'measuring'
  | 'solving'
  | 'proposed'
  | 'writing'
  | 'settling'
  | 'verifying'
  | 'done'
  | 'refused';

/** Machine-readable half of a refusal. Every one of these reaches the UI with a
 *  sentence in `Refusal.message` — plan §5.5: "it didn't work" is not an
 *  acceptable thing to show. */
export type RefusalKind =
  | 'no_session'
  | 'session_lost'
  | 'session_changed'
  | 'mic_missing'
  | 'mic_lost'
  | 'mic_reconnected'
  | 'mode_unsupported'
  /** Near field: the walk's two readings of its first speaker disagree by more than
   *  clock drift can explain, so something moved and the *whole* walk is suspect. */
  | 'closure_error'
  /** Near field: an arrival/close call does not match where the walk is. */
  | 'walk_out_of_order'
  /** Near field: nobody said "I am at a speaker" before the walk's timeout. */
  | 'walk_timeout'
  /** Chaining: a `position`/`finish` call does not match where the chain is — an
   *  unknown speaker, one already aligned earlier, an overlap that was never aligned,
   *  or finishing while speakers are still unaligned. Refuses the **step**, not the
   *  run: everything aligned so far keeps its provisional delays (plan §1.1.4). */
  | 'chain_out_of_order'
  /** Chaining: a step after the first named no overlap, so nothing ties this position
   *  to the speakers already aligned (plan §1.1). */
  | 'overlap_missing'
  /** Chaining: the step's overlaps disagree by more than plausible geometry allows, so
   *  the common shift this step would apply to the *whole* aligned set cannot be
   *  trusted. Parks the chain and asks for the position again — never a dead end. */
  | 'overlap_disagreement'
  /** Chaining: the provisional delay line refused the value the ratchet asked for
   *  (past `relay_delay::MAX_DELAY_MS`). */
  | 'provisional_range'
  | 'estimator'
  | 'gate_timeout'
  /** Exclusivity was violated on a member and its reading could not be retaken: a
   *  barge-in announcement or a voice-duck hold outranked the alignment hold (plan
   *  §12.3). A legitimate loss rather than a bug — and it must be reported *as
   *  itself*, never softened into a timeout. */
  | 'interference'
  | 'ambiguous_spread'
  | 'transitivity'
  | 'repeatability'
  | 'knob_range'
  | 'residual_too_large'
  | 'write_failed'
  | 'cancelled'
  | 'internal';

/** The estimator's own verdict, when a refusal came from it
 *  (align/estimator.rs `RejectReason`). */
export type EstimatorReason =
  | 'low_snr'
  | 'ambiguous_peak'
  | 'unstable_phase'
  | 'clipped'
  | 'sequence_gap'
  | 'too_few_periods';

/** A refusal, in both machine and human form. `message` is written for the user
 *  and must be shown verbatim; `member` names the speaker when one is to blame. */
export interface Refusal {
  kind: RefusalKind;
  message: string;
  member?: string;
  estimator_reason?: EstimatorReason;
}

export type WarningKind =
  | 'send_ahead_high_water'
  | 'aec_suspected'
  | 'level_learning_skipped'
  | 'mic_reconnected'
  | 'no_drift_fit'
  /** Exclusivity was violated during the run, even though the affected window was
   *  retaken — it explains why the run took longer than it should have. */
  | 'interference'
  /** Near field's premise — the phone is *at* each speaker — is the user's to keep and
   *  nothing can check it. Raised on every walk, not only when something looks wrong. */
  | 'near_field_path_assumed'
  /** A chain step was linked through **one** overlap. That single reading is applied as
   *  a common shift to every speaker aligned so far *and* anchors everything measured
   *  after it, with nothing to check it — so the step is the chain's weakest and the
   *  chain's total error stops being boundable (plan §1.1). */
  | 'one_overlap'
  /** Every position of a chain is aligned at *its own* spot, so speakers aligned at
   *  different positions are related only through the overlaps — approximate in the
   *  doorway between two rooms. Raised on every chained run, because the failure it
   *  describes looks like a perfectly good result. */
  | 'chain_scope';

export interface Warning {
  kind: WarningKind;
  message: string;
}

/** One member's estimate for a single pass. Both bursts are measured (plan §2.2),
 *  which is what gives §10.2's cross-band check a second, independent axis. */
export interface MemberMeasurement {
  /** Arrival of the 3 kHz "A" burst, ms on the estimator's shared grid. */
  phase_a_ms: number;
  /** Arrival of the 1.5 kHz "B" burst, ms on the same grid. */
  phase_b_ms: number;
  /** Spread across pattern repeats — the sharpest discriminator the estimator
   *  has (plan §5.4.1), so it is shown wherever a delta is shown. */
  std_error_ms: number;
  peak_snr_db: number;
  second_peak_ratio: number;
  drift_ppm: number;
  periods_used: number;
}

/** One accepted measurement of one member in one pass. */
export interface MemberObservation extends MemberMeasurement {
  node_name: string;
  pass: number;
  /** Which capture the phases belong to. Observations from different epochs are
   *  never comparable (plan §1.2), so a changing epoch explains a restart. */
  grid_epoch: number;
  period_centre: number;
}

export interface MemberProgress {
  node_name: string;
  kind: AlignMemberKind;
  /** Calibration level this member was soloed at (0-100). */
  level: number;
  /** The value its knob had when the run started — i.e. what a revert restores.
   *  Whether that knob is an advance or a delay follows from `kind` (sendspin
   *  advances, AirPlay 2 and pw-sink delay), so never label it "delay" unconditionally. */
  current_delay_ms: number;
  passes_done: number;
  last: MemberMeasurement | null;
  /** What the gate is waiting for, or why the last attempt failed. */
  note: string | null;
}

/** Which way raising a member's knob moves its arrival (plan §2.4.1).
 *
 *  This is the correction W14 landed, and it inverts what the UI used to claim: a
 *  sendspin device *subtracts* `static_delay_ms` from the playback instant, so
 *  raising that knob makes the speaker play **earlier**. An AirPlay-2 render delay
 *  and a pw-sink playout delay both make it play **later**. A user shown the word
 *  "delay" for an advance has been told the opposite of the truth, which is why
 *  every number in the UI is paired with its polarity or with `effect`. */
export type KnobPolarity = 'advance' | 'delay';

/** One member's proposed write. */
export interface ProposedDelay {
  node_name: string;
  kind: AlignMemberKind;
  /** Measured arrival relative to the earliest member, ms (>= 0): the acoustic
   *  answer, before any knob arithmetic. */
  arrival_ms: number;
  /** Knob values, not necessarily delays — see `polarity`. */
  current_delay_ms: number;
  new_delay_ms: number;
  /** `new - current`. Its sign is how the **knob** moves, not how the sound moves:
   *  raising a sendspin advance makes that speaker play *earlier*. Never render
   *  this on its own — pair it with `polarity`, or use `effect`. */
  added_ms: number;
  std_error_ms: number;
  /** The member whose knob lands at the smallest value. An **outcome** of the
   *  interval intersection (plan §2.4.2), not a reference anyone chose — so it must
   *  not be presented as "the speaker the others were aligned to by decision". */
  is_reference: boolean;
  polarity: KnobPolarity;
  knob_min_ms: number;
  knob_max_ms: number;
  /** The arrivals this member can reach, on `arrival_ms`'s scale — the interval the
   *  common target had to fall inside. */
  achievable_lo_ms: number;
  achievable_hi_ms: number;
  /** The daemon's own sentence for what this write does, e.g. `"advance 12 ms (was
   *  0 ms) — plays 12 ms earlier"`. **Prefer this verbatim** over composing wording
   *  from the numbers: it is the one place the direction is guaranteed right. */
  effect: string;
}

/** Plan §10.2's cross-band check. `caveat` states what it cannot see; it is shown
 *  whether the check passes or fails, because a pass is *not* proof (plan §5.6). */
export interface TransitivityCheck {
  worst_pair: [string, string] | null;
  worst_ms: number;
  tolerance_ms: number;
  passed: boolean;
  caveat: string;
}

export interface RepeatabilityCheck {
  worst_member: string | null;
  worst_ms: number;
  tolerance_ms: number;
  passed: boolean;
}

/** Plan §10.3 — a documented seam, not an implementation. `state` is
 *  `"not_implemented"` today and `reason` says why. */
export interface MergedPeakCheck {
  state: string;
  reason: string;
}

export interface ResidualCheck {
  worst_member: string | null;
  worst_ms: number;
  tolerance_ms: number;
  passed: boolean;
}

/** Near field's closure reading: the walk's first speaker, measured again at the end.
 *  `tolerance_ms` is a *rate* bound (a long walk earns a larger allowance), and
 *  `caveat` states what a pass does and does not establish — show it verbatim. */
export interface ClosureReport {
  anchor: string;
  error_ms: number;
  span_periods: number;
  span_s: number;
  drift_ppm: number;
  tolerance_ms: number;
  passed: boolean;
  caveat: string;
}

/** The checks available *before* the write. Residual only exists afterwards
 *  (see `Verification`), because it re-measures what was written. */
export interface Checks {
  transitivity: TransitivityCheck;
  /** Null when only one pass was usable — then there is nothing to compare. Also null
   *  for a **near-field walk**, where it would be available but vacuous (the only
   *  member with two readings is the closure anchor, whose residual is zero by
   *  construction): reporting an identity as a green check would be dishonest. */
  repeatability: RepeatabilityCheck | null;
  merged_peak: MergedPeakCheck;
  /** Near field's closure. Null for multi-position, which has no walk to close. */
  closure?: ClosureReport | null;
}

/** What `apply` would write, and the confidence behind it. */
export interface Proposal {
  /** The member whose knob ends up smallest — everyone else moves towards it. An
   *  outcome of the solve, not an input (see `ProposedDelay.is_reference`). */
  reference: string;
  pattern_ms: number;
  /** Arrival spread across the group, ms. */
  spread_ms: number;
  /** Fitted mic-vs-audio clock drift, ppm. */
  drift_ppm: number;
  /** The common arrival the group is being moved to, on `arrival_ms`'s scale (0 = the
   *  earliest member as measured). Can be **negative**: a sendspin group is aligned
   *  earlier than anything currently arrives whenever a member already has an
   *  advance. */
  target_ms: number;
  /** The intersection of every member's achievable arrivals — the window `target_ms`
   *  was picked from. An empty intersection is what a `knob_range` refusal is. */
  feasible_lo_ms: number;
  feasible_hi_ms: number;
  /** The largest knob value this proposal writes. This is the quantity the solver
   *  minimised: both polarities cost latency (an AP2 delay directly, a sendspin
   *  advance through the group's send-ahead), so plan §9.2's "keep the delay small"
   *  generalises to "keep the biggest knob small". */
  largest_knob_ms: number;
  members: ProposedDelay[];
  checks: Checks;
  warnings: Warning[];
  /** Set when a check blocks the write. The numbers stay visible on purpose: a
   *  green residual with a failed transitivity check is the interesting failure
   *  and must not be hidden (plan §10). */
  blocked: Refusal | null;
}

/** Post-write verification (plan §10). */
export interface Verification {
  residual: ResidualCheck;
  transitivity: TransitivityCheck;
  merged_peak: MergedPeakCheck;
  observations: MemberObservation[];
  passed: boolean;
  /** What this verification actually covered, when that is less than "the group".
   *  Absent for a single-position run, where every member was re-measured.
   *
   *  A **chain** can only be checked where the phone is, which is the *last* position:
   *  the residual covers that position's own speakers and its overlaps, and the earlier
   *  positions are not re-measured — re-measuring them from here would read their path
   *  difference to this spot and report a correct chain as broken (plan §10.4). Show it
   *  verbatim wherever the residual is shown, or the user reads a last-room result as a
   *  whole-house one. */
  scope_note?: string;
}

// ---- Near field: the walk (W8a, plan §1, §1.2, §10.4) -------------------------
// The user walks to each speaker in turn and holds the phone *at* it, so the
// propagation path collapses and what is measured is the wire — right everywhere
// rather than at one seat. Three facts drive the UI and none of them is visible in
// the numbers:
//
//   * **the premise is the user's to keep.** A phone held a metre away instead of at
//     the driver adds ~3 ms to that speaker's reading, and nothing in the measurement
//     can tell that apart from the speaker genuinely being 3 ms late. The daemon
//     raises `near_field_path_assumed` on *every* walk for exactly this reason;
//   * **the last stop is a revisit, not a new speaker.** One pass per member has no
//     time baseline for the drift fit, and a walk through a house takes minutes — so
//     the first speaker is measured again at the end, and the difference *is* the
//     drift fit (plan §5.3). An implausible closure refuses the **whole** walk,
//     because the correction it carries was applied to every member;
//   * **verification walks again** (plan §10.4). A stationary residual after a correct
//     wire alignment measures each speaker's distance to wherever the phone stands —
//     tens of ms against a 2 ms tolerance — so it would fail every near-field run.
//     `WalkPurpose::Verify` is that second walk, and it is not a repeat or an error.

/** Whether a walk is acquiring the measurement or confirming what was written. */
export type WalkPurpose = 'measure' | 'verify';

/** What the walk expects the UI to do next. */
export type WalkAction =
  /** `POST measure/arrival` naming the speaker the user is standing at. */
  | 'arrival'
  /** Every member has been read: walk back to `WalkProgress.anchor` and
   *  `POST measure/close` for the closure reading. */
  | 'close'
  /** A reading is in progress; a second call is refused rather than queued. */
  | 'busy'
  /** The walk is over — look at `MeasureStatus.phase` for how it ended. Kept visible
   *  rather than cleared, because the closure numbers are part of the verdict. */
  | 'done';

/** A near-field walk's live state (`MeasureStatus.walk`). */
export interface WalkProgress {
  purpose: WalkPurpose;
  next: WalkAction;
  /** The first speaker measured — the one to come back to. Null before the walk has
   *  started. */
  anchor: string | null;
  /** Members measured so far, **in walk order**. That order is the abscissa of the
   *  drift correction, which is why it is a list and not a set. */
  measured: string[];
  /** Members still to visit. In no particular order: the walk order is the user's to
   *  choose (plan §12.1 — near field's UI owns it). */
  remaining: string[];
  /** The member being read right now. */
  reading: string | null;
  /** Times this walk has started over because the capture reconnected. A walk is one
   *  capture (plan §1.2), so a reconnect voids the readings — and costs the *user* a
   *  re-walk, not the daemon a loop. */
  restarts: number;
  /** What to do next, in the daemon's own words. */
  prompt: string;
  /** Set once the closure reading has been taken. */
  closure: ClosureReport | null;
  /** What this walk's result is coherent *with* — one walk is internally coherent and
   *  related to nothing else, not even an earlier walk sharing a speaker (linking two
   *  is W8b). Verbatim. */
  scope_note: string;
  /** Where each arrival's playback level comes from: it is set *at* the speaker, per
   *  arrival, because at arm's length the risk inverts from too-quiet to clipping
   *  (plan §12.2). Verbatim. */
  level_note: string;
}

// ---- Multi-position chaining (W12, plan §1.1) ---------------------------------
// One run, several listening spots: align what you can hear from here, walk, align the
// next set through *overlap* speakers you can hear from both places. Two facts drive the
// whole UI below and neither is visible in the numbers:
//
//   * a step's shift is applied to **every** speaker aligned so far, not just to the
//     overlap it was measured on (that is what keeps the earlier set internally aligned
//     and is the trick the feature rests on) — including speakers in rooms the user
//     cannot hear from where they are standing;
//   * nothing is **written** until `finish`. The delays live in the daemon's per-device
//     delay line, so the speakers already sound aligned while no knob has been touched
//     and nothing is persisted (plan §1.1.1).

/** What a chained run expects the UI to do next. */
export type ChainAction =
  /** `POST measure/position` with the speakers audible from here plus the overlaps. */
  | 'position'
  /** Every held speaker is aligned somewhere: `POST measure/finish` renormalises the
   *  chain and proposes the single write. A further position is still accepted. */
  | 'finish'
  /** A position is being measured; a second call would be refused. */
  | 'busy'
  /** The chain is over. Kept visible, because the per-step numbers are the verdict. */
  | 'done';

/** How well a step's link to the already-aligned set could be checked (plan §1.1). */
export type OverlapConfidence =
  /** The first position: nothing was aligned yet, so this step *defines* the frame. */
  | 'origin'
  /** One overlap — anchored, but that single reading moves the whole aligned set and
   *  anchors everything after it with nothing to check it against. */
  | 'single'
  /** Two or more: their disagreement is an independent estimate of the joint's error. */
  | 'checked';

/** One overlap speaker as it read at the new position. */
export interface ChainOverlap {
  node_name: string;
  /** Its arrival at *this* position, on this step's own scale — already including the
   *  provisional delay it is carrying, which is what makes the chain work. */
  arrival_ms: number;
  /** The provisional delay it was carrying while this reading was taken. */
  applied_ms: number;
}

/** One member's provisional delay, as the relay is applying it *right now*. Nothing
 *  here is persisted: a daemon restart drops it and the stored config is untouched. */
export interface ProvisionalDelay {
  node_name: string;
  /** What the chain's arithmetic holds, ms (exact, so a step's Δ does not accumulate
   *  rounding). */
  delay_ms: number;
  /** What was actually pushed to the delay line, in whole ms — the same 1 ms
   *  granularity the final write has. For an overlap this is *observed* rather than
   *  assumed, because the next position measures the overlap through it. */
  applied_ms: number;
}

/** One position of a chain, after it was measured. */
export interface ChainStep {
  /** 1-based, in the order the user walked. */
  index: number;
  /** The speakers this position aligned (the ones not already aligned). */
  members: string[];
  /** The already-aligned speakers this position was linked through. */
  overlaps: ChainOverlap[];
  confidence: OverlapConfidence;
  /** Worst pairwise disagreement between the overlaps here, ms. Null for a first or
   *  single-overlap step, which have nothing to compare. */
  disagreement_ms: number | null;
  worst_pair: [string, string] | null;
  tolerance_ms: number;
  /** Where the already-aligned set is judged to arrive here: the mean of the overlap
   *  readings. Null for the first step. */
  anchor_ms: number | null;
  /** The common delay this step added to **every** member of the already-aligned set,
   *  ms. Non-zero exactly when a new speaker here arrived *later* than the aligned set
   *  does — and it goes to all of them, because a common delay preserves an aligned
   *  set's internal alignment. The single most surprising thing the feature does. */
  delta_ms: number;
  /** The arrival this position was aligned at, on this step's own scale. */
  target_ms: number;
  spread_ms: number;
  /** Drift fitted at *this* position; a chain has no single figure. */
  drift_ppm: number;
  /** Half of `disagreement_ms` — how far this joint's common shift can be out. Null
   *  when the step had nothing to check it with, which is what makes the chain's total
   *  unboundable (see `ChainError`). */
  joint_error_ms: number | null;
  /** Which capture this position was measured in. Positions may differ; no two steps'
   *  observations are ever compared. */
  grid_epoch: number;
  /** The §10 checks over this position's own readings. They **block the step**. */
  checks: Checks;
  note: string;
}

/** What a chain's accumulated error can and cannot be said to be (plan §1.1.4).
 *
 *  `joint_ms` is deliberately null — not a partial sum — as soon as any joint was
 *  linked through a single overlap: "a total that quietly left out the one joint it
 *  could not measure would be worse than none". Never compute one client-side. */
export interface ChainError {
  /** True when *every* joint was checked by two overlaps. */
  bounded: boolean;
  joint_ms: number | null;
  /** The daemon's sentence. Show it verbatim. */
  message: string;
}

/** A chained run's live state (`MeasureStatus.chain`). */
export interface ChainProgress {
  next: ChainAction;
  steps: ChainStep[];
  /** Every speaker aligned so far, in the order they were aligned. */
  aligned: string[];
  /** Held speakers not aligned at any position yet. `finish` refuses while this is
   *  non-empty: a speaker with no reading has nothing to write. */
  remaining: string[];
  /** What the relay is applying right now. Nothing is persisted. */
  provisional: ProvisionalDelay[];
  /** The smallest provisional delay in the aligned set, ms — the floor that ratchets
   *  upward because every step can only *add*. `finish` subtracts it globally, which is
   *  a common shift and therefore free. */
  floor_ms: number;
  /** The position being measured right now. */
  measuring: number | null;
  /** Times the position in flight was restarted because the capture reconnected. */
  restarts: number;
  /** What to do next, in the daemon's own words. */
  prompt: string;
  error: ChainError;
  /** Why the last position was rejected, when it was. The chain **stays alive**: the
   *  positions already aligned keep their provisional delays and the user can post this
   *  position again. Cleared as soon as a position is accepted. */
  refusal: Refusal | null;
  /** What a chain's result is coherent *with* — the doorway caveat, stated on every
   *  chained run rather than inferred from the numbers. Verbatim. */
  scope_note: string;
}

/** Why the loop-phase gate is not accepting a window yet (plan §8). */
export type GateReason =
  | 'mic_disconnected'
  | 'mic_reconnected'
  | 'sequence_gap'
  | 'clipped'
  | 'silent'
  /** A barge-in announcement or a duck hold hit this member. Exists so the failure
   *  names the doorbell: without it the level change an announcement causes is caught
   *  as unstable amplitude and reported as "hold the phone still". */
  | 'interference'
  | 'intermittent'
  | 'unstable_amplitude'
  | 'aec_suspected'
  | 'acquiring'
  | 'estimator';

/** What the gate is doing right now. This is what stops a slow run looking hung:
 *  mute settling, a reconnect that costs tens of seconds, or the phone moving all
 *  land here rather than as silence. */
export interface GateProgress {
  locked: boolean;
  periods: number;
  needed: number;
  waiting_for?: GateReason;
  message: string;
  restarts: number;
  member?: string;
}

/** `GET /api/align/measure` — the whole run, in one object. */
export interface MeasureStatus {
  phase: MeasurePhase;
  mode: MeasureMode;
  /** The source set identifying the group being measured. */
  sources: string[];
  sample_rate: number;
  /** One sentence describing what the run is doing, or why it stopped. */
  message: string;
  members: MemberProgress[];
  observations: MemberObservation[];
  proposal: Proposal | null;
  verification: Verification | null;
  refusal: Refusal | null;
  warnings: Warning[];
  gate?: GateProgress;
  /** Near field only: where the walk is and what it wants next. Absent for a
   *  multi-position run, and **retained after the walk ends** so the closure numbers
   *  stay readable on the review page. */
  walk?: WalkProgress | null;
  /** Chained multi-position only: where the chain is, what it wants next, the per-position
   *  numbers, and what the result is *not* coherent with. Absent for a single-position run
   *  (which is a chain with one step and needs no calls) and for a near-field walk. */
  chain?: ChainProgress;
  /** `POST measure/apply` will be accepted (i.e. parked in `proposed`, unblocked). */
  can_apply: boolean;
  /** `POST measure/revert` has a snapshot to restore (plan §9.4). */
  can_revert: boolean;
  /** The speakers a *pending revert* belongs to; null when there is nothing to
   *  revert. Non-null exactly while `can_revert` is true.
   *
   *  Retained across `abandon()`, which is the whole point: abandoning clears
   *  `sources` but keeps what was written revertable (§9.4), so without this the
   *  status alone could not say which group's panel should offer the undo — and a
   *  client-side memory of it would die with the page. */
  revert_scope: string[] | null;
  elapsed_s: number;
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

// ---- Run transcripts (`GET /api/align/measure/log`, plan §11) -----------------
//
// The daemon writes an append-only transcript per measurement run and keeps the last
// few, so a run can be investigated *after* it is over — which is when someone who had
// to bring speakers online by hand wants to know what the daemon actually saw.
//
// The event bodies are deliberately typed as `unknown` here. A transcript is forensic,
// not a contract a client computes with: the UI hands the file to a person, and typing
// each event kind would invite reading numbers out of it — a second, quieter copy of the
// estimator's own reporting that would drift from it.

/** One stored run, as the listing describes it — enough to tell whether it is the run
 *  currently on screen without fetching the whole thing. */
export interface RunSummary {
  id: string;
  /** Unix seconds, from the run's own first event. */
  started_unix: number;
  events: number;
  size_bytes: number;
  /** The last event's kind: `run_finished` for a complete transcript, or whatever the
   *  run was doing when the daemon stopped. */
  last_kind: string;
  /** The last event's sentence — for a finished run, its verdict. */
  last_message: string;
  /** The `run_started` header (mode, members, delays), when the file has one. */
  started?: unknown;
}

export interface MeasureLogList {
  /** Newest first. */
  runs: RunSummary[];
  /** How many are kept before the oldest is dropped. */
  retained: number;
  /** Where they live, when transcripts are enabled at all. Absent means the daemon has
   *  nowhere to write them, which is why the listing can legitimately be empty. */
  directory?: string;
}

/** One whole transcript, as one document — the form that can be read days later
 *  without this UI. */
export interface RunDocument {
  id: string;
  started_unix: number;
  events: unknown[];
  /** The run hit the per-transcript event bound, so the file is not the whole story. */
  truncated: boolean;
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

export interface OpResponse {
  ok: boolean;
  message: string;
}

/** One live duck hold (`GET /api/duck`, overlay_mixer.rs): an output whose music
 * is attenuated with no clip of its own — voice ducking, while an assistant in
 * that room talks through its own speaker. `level` is a gain (0.25 = quarter
 * volume). Transient: holds are leased, and released when the turn ends. */
export interface DuckHold {
  output: string;
  hold_id: number;
  level: number;
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

/** One row of `GET /api/agents`: a paired receiver host, or one asking to be
 * (docs/receiver-agent.md §8).
 *
 * Diagnostics only — the Outputs page reads the output listings instead, where a
 * host waiting to pair appears as a discovered `pwsink` output carrying the same
 * code. Every row has a `node_name`, pending ones included: it is settled at the
 * first hello so the card the user pairs is the target it becomes. */
export interface AgentInfo {
  /** `<machine-id>:<user>`; how the daemon identifies one agent. */
  identity: string;
  /** `hostname (user)`. */
  label: string;
  node_name: string;
  paired: boolean;
  connected: boolean;
  /** The pairing code, while it is waiting to be paired. */
  code: string | null;
  state: {
    volume: number | null;
    muted: boolean | null;
    sink_name: string | null;
    receiving: boolean;
    ducked: boolean;
  } | null;
}
