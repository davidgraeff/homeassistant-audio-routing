//! Manual routing UI backend: a source×output matrix —
//! REST endpoints to read the matrix and toggle links, plus a WebSocket
//! that pushes a fresh matrix snapshot on every registry change instead of
//! requiring the client to poll.
//!
//! **Invariant: anything that changes a field of [`RoutingNode`] or the link set
//! must notify `AppState::changes`.** The matrix frame is pushed on that signal and
//! on nothing else — there is no timer behind it (see [`handle_socket`]). Miss the
//! notification and the graph, the group editors and every Home Assistant entity
//! keep showing the old value until something unrelated happens; that visible
//! staleness is the intended failure mode, chosen over a periodic re-check that
//! would paper over the omission. `latency_ms` was exactly this bug, hidden for as
//! long as the matrix went out four times a second regardless.
//!
//! **Source/output classification is a heuristic over live registry
//! state, not a fixed list** — consistent with the rest of this project's
//! "trust the observed graph" approach:
//! - **Outputs** are the nodes `is_output_node` recognizes by name prefix
//!   (`SENDSPIN_NODE_PREFIX`). This is vestigial — nothing creates such a node
//!   any more, so `present_outputs` is always empty and every output's `node_id`
//!   is `None`; adopted virtual outputs are added separately below. Retiring the
//!   prefix changes this classification rule rather than deleting a dead branch,
//!   so it is deferred (docs/voice-duck-plan.md §7 L6).
//! - **Sources** are any node with at least one **non-monitor** output-
//!   direction port. Every sink in this project also exposes `monitor_*`
//!   output ports (monitor/capture taps) — excluding ports named with that
//!   prefix is what keeps a sink's own monitor tap from being
//!   misclassified as a routable source. Real sources observed so far
//!   (the native AirPlay input `airplay-in`, ad-hoc test nodes) all expose
//!   plain `output_*`/`capture_*` ports, never `monitor_*`.
//!
//! **Channel pairing**: linking a source to an output pairs ports by their
//! name's final `_`-separated suffix (`output_FL` ~ `send_FL` ~
//! `playback_FL` all pair as `FL`) — matches the convention already used
//! by hand in every test script in `tests/`, generalized here instead of
//! being hardcoded per source/output type.
//!
//! The directory adds the two modules the matrix cannot work without:
//! [`sync_group`] builds and reconciles the group sinks a routed source actually
//! plays through — the anchor plus one writer per member — and [`sync_settings`]
//! holds the timing that keeps those members together (group presentation lead,
//! per-device static delay) and pushes it into the senders when it changes.
//!
//! `sync_settings` persists, but it is not one of the pure `store/` modules for
//! exactly that reason: writing a setting reaches into the AP2, AppleMIDI and
//! sendspin codecs. See `store/mod.rs`.

pub(crate) mod sync_group;
pub(crate) mod sync_settings;

use crate::outputs::sendspin::discovery::SendspinDevice;
use crate::pw::thread::{LinkSpec, PortInfo, PwCommand, PwCommandSender, RegistryState, SharedState};
use crate::state::AppState;
use crate::store;
use crate::store::routing::{RoutingLink, SharedRouting};
use crate::util::locks::LockRecover;
use crate::util::node_names::{OutputKind, AP2_DEV_PREFIX, PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX, SENDSPIN_NODE_PREFIX};
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::oneshot;

/// Which level knobs the daemon can drive on one output **right now** — the capability
/// half of [`RoutingNode::volume`] / [`RoutingNode::muted`], which carry only values.
///
/// Two booleans rather than one, because they are genuinely independent: a pw-sink host
/// whose sink has no device route reports `channel_volumes` through the node's `Props` with
/// `mute: None`, so it is levellable while its mute is not reachable at all (the alignment
/// session then silences it through the relay instead — see
/// `align::calibrate::SilenceChannel`, which is the same question asked with a fallback).
///
/// Deliberately **not** derived from the output's kind. See [`level_caps`].
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct LevelCaps {
    /// A volume write can be expected to land: sendspin/AP2 in band, a pw-sink host while
    /// its agent reports a level.
    pub volume: bool,
    /// A mute write can be expected to land. Note this is the *output's own* mute, not
    /// alignment's ability to silence it — the relay can silence anything.
    pub mute: bool,
}

/// What the daemon can drive on one output, resolved **per output**.
///
/// The kind decides only where to look. For the two in-band backends the knobs are part of
/// the protocol, so they exist whether or not a level has ever been read — which is exactly
/// the case that made "is `volume` present?" the wrong test. For a pw-sink host the answer
/// is the receiver agent's, changes while the daemon is running, and is read from what the
/// host reports: `Some(level)` *is* the capability, the same rule
/// `align::calibrate::level_plan` uses through the `OutOfBandMute` seam.
fn level_caps(
    node_name: &str,
    pwsink_volumes: &std::collections::HashMap<String, f32>,
    pwsink_mutes: &std::collections::HashMap<String, bool>,
) -> Option<LevelCaps> {
    match OutputKind::of(node_name) {
        // In-band on the sendspin protocol and in-band over RTSP: both knobs always exist
        // for a device we have adopted, present or not (a write to an absent sendspin device
        // is stored as its desired level and applied when it connects).
        Some(OutputKind::Sendspin) | Some(OutputKind::Airplay2) => Some(LevelCaps { volume: true, mute: true }),
        Some(OutputKind::PwSink) => {
            Some(LevelCaps { volume: pwsink_volumes.contains_key(node_name), mute: pwsink_mutes.contains_key(node_name) })
        }
        // Sources, group sinks, real PipeWire nodes: not this API's business. A real node's
        // volume is PipeWire's own and is set through the graph, not here.
        None => None,
    }
}

#[derive(Serialize, Clone)]
pub struct RoutingNode {
    /// Stable node name — the primary key everything routes on. Survives module
    /// reloads and device disappearance/reappearance; the HA integration and
    /// the persisted routing intent (store/routing.rs) both key off it.
    node_name: String,
    display_name: String,
    /// Whether the node is in the live graph right now. `false` = configured or
    /// previously-routed but currently absent — shown grayed in the UI; its
    /// routing intent is kept and reapplied by reconcile() when it returns.
    ///
    /// For the dialed backends this is *reachability*, not delivery: an AP2
    /// receiver answers on :7000 and a pw-sink target advertises over mDNS long
    /// before (or without ever) accepting a session. See `streaming`.
    present: bool,
    /// Outputs only: **is a session to this output actually up**, i.e. is audio
    /// routed to it really being carried? `Some(false)` = present/reachable but
    /// nothing is attached, so a route to it exists on paper and delivers nothing —
    /// the UI must not animate that wire (this is the state that had the routing
    /// graph showing a happy flowing link while announcements to the same output
    /// were correctly refused). `None` = the question doesn't apply (sources; a
    /// sendspin device, which always has a sender while adopted). Same rule as the
    /// announce arbiter and the Outputs page, via
    /// [`crate::routing::sync_group::dialed_session_established`].
    #[serde(skip_serializing_if = "Option::is_none")]
    streaming: Option<bool>,
    /// Live PipeWire node id when present (needed for per-node ops like
    /// volume). `None` for offline entities.
    node_id: Option<u32>,
    /// Outputs only: a manually-configured store entry (`true`) vs an
    /// mDNS-auto-discovered one (`false`) — drives the "auto-discovered" badge.
    /// Always `true` for sources (they aren't mDNS-discovered here).
    configured: bool,
    /// Recent peak level (0.0–1.0) for the UI meter. Sources only (metered
    /// on-demand while the matrix is watched); `0.0` for outputs and unmetered.
    ///
    /// **A snapshot, not the live figure.** This is the sample taken when the matrix
    /// was built, which now happens only on a graph change — a WebSocket client
    /// tracks levels through [`Frame::Meters`] instead. It stays here so that
    /// `GET /api/routing` is a complete cold read.
    peak: f32,
    /// Current volume (0.0–1.0) as the daemon last knew it — pushed here so a UI slider
    /// syncs live over this WebSocket, including a physical change the device reports.
    ///
    /// **`None` means "not known", not "not possible".** For sendspin and AP2 it means no
    /// level has been heard or set yet; for a pw-sink host it means its agent is not
    /// reporting one. Whether there is a knob at all is [`Self::level_caps`] — do not infer
    /// it from this field being present, and never from the output's kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<f32>,
    /// Current mute state, same "known vs possible" rule as [`Self::volume`]: `None` is
    /// unknown, and deliberately not `false` — a missing agent reading as "unmuted" would
    /// put a mute button on screen that silently does nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    muted: Option<bool>,
    /// Outputs only: **which of the two level knobs this daemon can actually drive right
    /// now**, resolved per output rather than per kind. `None` for sources and for
    /// non-output nodes.
    ///
    /// Published because the alternative is every consumer guessing, and each of them
    /// guessing differently: the frontend gated its volume control on the *kind* (hiding it
    /// from every pw-sink host the agent could already drive), and the alignment wizard
    /// kept a second kind table that had both AP2 and pw-sink wrong. A capability is the
    /// daemon's answer — it is the only party that knows whether an agent is on the other
    /// end — so it is sent rather than reconstructed.
    ///
    /// The distinction this exists to make is **unknown vs unsupported**. Those were the
    /// same value while `volume`/`muted` were the only fields: a control could only be
    /// gated on "did a level arrive?", which is a different question and happened to give
    /// the right answer only because sendspin and AP2 always report *some* mute.
    #[serde(skip_serializing_if = "Option::is_none")]
    level_caps: Option<LevelCaps>,
    /// Estimated buffering (ms) this node contributes to the end-to-end path —
    /// the jitter/playout buffer configured for it, NOT a measured figure.
    /// Sources: the ingest jitter buffer (RTP `sess.latency.msec` / AirPlay
    /// producer prebuffer). Outputs: the playout lead (sendspin group send-ahead +
    /// per-device static delay; AP2 render delay; pw-sink receiver jitter buffer).
    /// The UI sums a route's source + output estimates to show its rough latency.
    /// `None` when unknown (e.g. an offline/unrecognized node). See build_matrix.
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u32>,
    /// Cumulative xrun (dropped-cycle) count for this node from the PipeWire
    /// profiler (pw/profiler.rs) — the same figure as `pw-top`'s `ERR`. Present only
    /// for real graph nodes while the matrix is being watched (profiling is armed
    /// on-demand); `None` for virtual outputs (sendspin/AP2 have no graph node)
    /// and whenever profiling is off. A rising value is where dropouts originate.
    ///
    /// Like `peak`, a snapshot: a WebSocket client watches it climb on
    /// [`Frame::Meters`], not here.
    #[serde(skip_serializing_if = "Option::is_none")]
    xruns: Option<u32>,
    /// Outputs only: the diagnosed reason this output cannot carry audio right now
    /// ([`crate::ap2_health`]); `None` when nothing is known to be wrong. Mirrors
    /// `/api/outputs`' `last_error` so the graph can label a dead endpoint instead
    /// of merely drawing it non-streaming — "routed but the receiver is refusing us"
    /// was previously indistinguishable here from "no session up yet". AirPlay-2
    /// only so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    /// Outputs only: an alignment run has taken this output over right now, so
    /// **nothing routed to it is playing** (`align/group.rs`, plan §12.3 — the hold is
    /// exclusive). Absent (rather than `false`) when nothing is aligning, like the two
    /// fields above: this is the rare state, and an absent key is one less thing in
    /// every ordinary frame.
    ///
    /// It rides the matrix rather than a status socket of its own because this is the
    /// per-output state channel the UI already consumes, and because it must **clear**
    /// the moment the hold does — a badge that outlives the hold is worse than no badge.
    /// That is why [`ExclusiveHold::release`](crate::align::group::ExclusiveHold::release)
    /// clears the registry entry *before* it notifies `changes`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    held: bool,
}

#[derive(Serialize, Clone)]
pub struct RoutingMatrix {
    // `pub(crate)` because `events.rs` serialises this as the `matrix` frame and its
    // tests build one; nothing outside the crate sees the struct at all.
    pub(crate) sources: Vec<RoutingNode>,
    pub(crate) outputs: Vec<RoutingNode>,
    /// Desired routing = persisted intent (store/routing.rs), by stable name.
    /// The UI renders these as the linked cells (including links to a currently
    /// offline endpoint, shown grayed); reconcile() makes the live graph match
    /// for pairs whose endpoints are both present.
    pub(crate) links: Vec<RoutingLink>,
}

/// Configured (not measured) buffering figures used to estimate each node's
/// contribution to end-to-end latency for the routing-graph readout. All in ms.
pub struct LatencyConfig {
    /// Per-source input buffer (ms) keyed by the source's PipeWire node name:
    /// an AirPlay receiver's producer prebuffer or an RTP receiver's jitter
    /// buffer. Covers every configured source instance (built from the source
    /// store), so it isn't tied to any fixed node name.
    pub source_latencies: std::collections::HashMap<String, u32>,
    /// Sendspin group presentation lead (`group_lead_ms`) — the base playout
    /// buffer every sendspin device rides on.
    pub group_lead_ms: u32,
    /// Per-sendspin-device static delay (added on top of the group lead).
    pub sendspin_delays: std::collections::BTreeMap<String, u16>,
    /// Per-AP2-device render delay; the default applies when a device has no
    /// override.
    pub ap2_delays: std::collections::BTreeMap<String, u16>,
    /// Default AP2 render delay when a device has no per-device override.
    pub ap2_default_ms: u32,
    /// Per-pw-sink-host playout delay = that receiver's jitter buffer
    /// (`sess.latency.msec`); the default applies when a host has no override.
    pub pwsink_jitters: std::collections::BTreeMap<String, u16>,
    /// Default pw-sink playout delay when a host has no per-host override.
    pub pwsink_default_ms: u32,
}

/// Estimated buffering (ms) a node contributes, from config. `None` when we
/// have no figure for the node (offline, or a kind whose buffer we don't model).
/// See [`RoutingNode::latency_ms`].
fn node_latency_ms(node_name: &str, lat: &LatencyConfig) -> Option<u32> {
    // Any configured source (AirPlay or RTP), by its node name.
    if let Some(ms) = lat.source_latencies.get(node_name) {
        return Some(*ms);
    }
    match OutputKind::of(node_name) {
        Some(OutputKind::Sendspin) => {
            let extra = lat.sendspin_delays.get(node_name).copied().unwrap_or(0);
            Some(lat.group_lead_ms + u32::from(extra))
        }
        Some(OutputKind::Airplay2) => Some(lat.ap2_delays.get(node_name).map(|ms| u32::from(*ms)).unwrap_or(lat.ap2_default_ms)),
        // The receiver's jitter buffer is the whole of what we configure on this
        // path. The rest of its budget (our capture quantum, the remote host's own
        // sink buffer) is real but not ours to know, so it is left out rather than
        // guessed — the same rule the other kinds follow.
        Some(OutputKind::PwSink) => Some(lat.pwsink_jitters.get(node_name).map(|ms| u32::from(*ms)).unwrap_or(lat.pwsink_default_ms)),
        None => None,
    }
}

fn is_output_node(node_name: &str) -> bool {
    node_name.starts_with(SENDSPIN_NODE_PREFIX)
}

pub(crate) fn output_display_name(node_name: &str) -> String {
    for prefix in [SENDSPIN_NODE_PREFIX, SENDSPIN_DEV_PREFIX, AP2_DEV_PREFIX, PWSINK_DEV_PREFIX] {
        if let Some(rest) = node_name.strip_prefix(prefix) {
            return rest.replace(['_', '-'], " ");
        }
    }
    node_name.to_string()
}

fn channel_suffix(port_name: &str) -> &str {
    port_name.rsplit('_').next().unwrap_or(port_name)
}

/// Build the matrix from live registry + adopted outputs + persisted intent.
///
/// Every routable entity is included even when it isn't in the graph right
/// now: an adopted output (whether or not it has saved routing), and any source
/// referenced by intent, appear as `present: false` (grayed in the UI) so its
/// routing survives disappearance and is reapplied on return. Outputs also
/// carry `configured` (store entry vs mDNS auto-discovered) for the badge.
///
/// **`adopted` is the gate**: a discovered device the user hasn't added on the
/// Outputs page is not routable, so it must not appear here — this listing is
/// also what the Home Assistant integration turns into `media_player` entities,
/// and an unadopted device must get neither a route nor an entity. Links whose
/// output isn't adopted are filtered out too (rather than dropped from the
/// store), so adding a device back restores the routing it had.
// Every argument is a distinct live store the matrix is assembled from; a
// "context" struct would just be this list under another name.
#[allow(clippy::too_many_arguments)]
fn build_matrix(
    reg: &RegistryState,
    devices: &BTreeMap<String, SendspinDevice>,
    ap2_devices: &BTreeMap<String, crate::outputs::ap2::discovery::Ap2Device>,
    // Connected receiver hosts (`node_name → label`, outputs/pwsink/agent.rs). The same
    // source `sync_group` builds its pw-sink members from, so this page and the audio
    // path cannot disagree about whether a host is there — they used to, because this
    // read mDNS discovery (`pwsink-dev-<host>`) while everything else used the pairing
    // (`pwsink-dev-<host>_<user>`), which showed a connected host as `present: false`.
    pwsink_hosts: &BTreeMap<String, String>,
    adopted: &std::collections::BTreeSet<String>,
    source_labels: &std::collections::HashMap<String, String>,
    // User-chosen output names (store/outputs.rs), keyed by node name. Wins over
    // whatever discovery reported — it is the whole point of a rename.
    output_labels: &BTreeMap<String, String>,
    meters: &crate::pw::metering::MeterHub,
    intent: &[RoutingLink],
    sendspin_volumes: &std::collections::HashMap<String, u8>,
    sendspin_mutes: &std::collections::HashMap<String, bool>,
    ap2_volumes: &std::collections::HashMap<String, f32>,
    ap2_mutes: &std::collections::HashMap<String, bool>,
    // Per-receiver-host volume/mute as the pwrouter-agent reports it (`HostState`).
    // Absent = we have no live agent for that host, or its sink has no volume lever at
    // all (the agent's own diagnostic calls that "lever: none") — either way the level
    // is genuinely unknown, which is the one thing the UI must not fabricate.
    pwsink_volumes: &std::collections::HashMap<String, f32>,
    pwsink_mutes: &std::collections::HashMap<String, bool>,
    // `ap2_connected`: AP2 outputs whose sender has a live command channel
    // (`Ap2Control::connected`) — half of the `streaming` verdict below; the
    // pw-sink half is a process-global, so it needs no parameter.
    ap2_connected: &std::collections::HashSet<String>,
    lat: &LatencyConfig,
    xruns: &std::collections::HashMap<String, u32>,
    // Outputs an alignment run holds exclusively right now ([`held_for_alignment`]) —
    // one snapshot per frame rather than a registry question per output, so a hold that
    // releases while the matrix is being built cannot produce a frame where half the
    // rows claim it.
    held: &std::collections::BTreeSet<String>,
) -> RoutingMatrix {
    use std::collections::BTreeSet;

    // Live nodes: highest id per name (newest), same "most recent wins" rule as
    // node_id_for — a same-named node can briefly outlive its owner.
    let mut present_outputs: BTreeMap<String, u32> = BTreeMap::new();
    let mut present_sources: BTreeMap<String, u32> = BTreeMap::new();
    for node in reg.nodes.values() {
        if is_output_node(&node.node_name) {
            let e = present_outputs.entry(node.node_name.clone()).or_insert(node.node_id);
            *e = (*e).max(node.node_id);
        } else {
            let has_real_source_port =
                reg.ports.values().any(|p| p.node_id == node.node_id && p.direction == "out" && !p.port_name.starts_with("monitor_"));
            if has_real_source_port {
                let e = present_sources.entry(node.node_name.clone()).or_insert(node.node_id);
                *e = (*e).max(node.node_id);
            }
        }
    }

    // Every output is now virtual + auto-discovered (sendspin + AP2 devices) —
    // audio reaches them via a group sink (routing/sync_group/mod.rs), not a live node here.

    // Union of every output name to show: present ∪ discovered devices ∪ intent
    // ∪ adopted — then narrowed to the adopted ones, the only routable outputs.
    // An adopted-but-undiscovered output is kept (grayed) so its routing is
    // visible and survives the device being off.
    let mut output_names: BTreeSet<String> = present_outputs.keys().cloned().collect();
    output_names.extend(devices.keys().cloned());
    output_names.extend(ap2_devices.keys().cloned());
    output_names.extend(pwsink_hosts.keys().cloned());
    output_names.extend(intent.iter().map(|l| l.output.clone()));
    output_names.extend(adopted.iter().cloned());
    output_names.retain(|n| adopted.contains(n));

    // Sources of dormant intent (an output that isn't adopted) don't count as
    // referenced — otherwise a source with nothing but such links would sit in
    // the matrix with no visible link to explain it.
    let mut source_names: BTreeSet<String> = present_sources.keys().cloned().collect();
    source_names.extend(intent.iter().filter(|l| adopted.contains(&l.output)).map(|l| l.source.clone()));

    let mut outputs: Vec<RoutingNode> = output_names
        .into_iter()
        .map(|name| {
            let node_id = present_outputs.get(&name).copied();
            let device = devices.get(&name);
            let ap2 = ap2_devices.get(&name);
            let pwsink_label = pwsink_hosts.get(&name);
            let display_name = output_labels
                .get(&name)
                .cloned()
                .or_else(|| device.map(|d| d.display_name.clone()))
                .or_else(|| ap2.map(|d| d.display_name.clone()))
                .or_else(|| pwsink_label.cloned())
                .unwrap_or_else(|| output_display_name(&name));
            RoutingNode {
                // Present if live in the graph, or a discovered sendspin/AP2/pw-sink
                // endpoint the liveness task still deems online. An offline endpoint
                // stays listed (grayed) until liveness removes it — an mDNS blip no
                // longer makes it vanish.
                present: node_id.is_some() || device.is_some_and(|d| d.present) || ap2.is_some_and(|d| d.present) || pwsink_label.is_some(),
                // Reachable is not the same as connected for the dialed backends —
                // report the session state separately so the UI can tell the two
                // apart instead of implying delivery from mere presence.
                streaming: crate::routing::sync_group::dialed_session_established(&name, ap2_connected),
                node_id,
                // Every output is now auto-discovered (sendspin + AP2); nothing is
                // manually configured anymore (the RAOP store is gone).
                configured: false,
                display_name,
                // The *values*, where they are known; `level_caps` below says whether there
                // is a knob at all. One match per question rather than a chain of
                // `starts_with` with an `else` that guesses: a fourth output kind now fails
                // to compile here instead of silently reporting no level.
                volume: match OutputKind::of(&name) {
                    // `None` when unknown: the sendspin store holds levels the device
                    // *reported* (`client/state`) or the user set, so an absent entry means
                    // we have never heard a level for this speaker. Reporting 1.0 there
                    // fabricated full scale — the one thing a volume control must never
                    // show, since these are dB scales where the top is near-max power.
                    Some(OutputKind::Sendspin) => sendspin_volumes.get(&name).map(|v| *v as f32 / 100.0),
                    // AP2 volume is device-authoritative: unknown until we have read it
                    // from the receiver or the user has set it.
                    Some(OutputKind::Airplay2) => ap2_volumes.get(&name).copied(),
                    // The host's own master level, as its agent reports it over the control
                    // lane (`DaemonMsg::SetVolume` drives the same lever).
                    Some(OutputKind::PwSink) => pwsink_volumes.get(&name).copied(),
                    None => None,
                },
                muted: match OutputKind::of(&name) {
                    Some(OutputKind::Sendspin) => Some(sendspin_mutes.get(&name).copied().unwrap_or(false)),
                    Some(OutputKind::Airplay2) => Some(ap2_mutes.get(&name).copied().unwrap_or(false)),
                    // `None` — not `Some(false)` — when the host reports no mute state: a
                    // missing agent must not read as "unmuted".
                    Some(OutputKind::PwSink) => pwsink_mutes.get(&name).copied(),
                    None => None,
                },
                level_caps: level_caps(&name, pwsink_volumes, pwsink_mutes),
                latency_ms: node_latency_ms(&name, lat),
                xruns: xruns.get(&name).copied(),
                last_error: crate::outputs::ap2::health::Ap2Health::global().get(&name),
                held: held.contains(&name),
                node_name: name,
                peak: 0.0, // outputs aren't metered
            }
        })
        .collect();

    let mut sources: Vec<RoutingNode> = source_names
        .into_iter()
        .map(|name| {
            let node_id = present_sources.get(&name).copied();
            // Show the source's user-facing label (from the source store) when we
            // know it; otherwise fall back to the raw node name. Works for every
            // source instance, not just a fixed-name AirPlay/RTP source.
            let display_name = source_labels.get(&name).cloned().unwrap_or_else(|| name.clone());
            let peak = meters.peak(&name);
            let latency_ms = node_latency_ms(&name, lat);
            let node_xruns = xruns.get(&name).copied();
            RoutingNode {
                present: node_id.is_some(),
                // Sources feed the graph locally — there is no session to be up.
                streaming: None,
                node_id,
                configured: true,
                display_name,
                node_name: name,
                peak,
                volume: None,
                muted: None,
                // A source's level is PipeWire's own, set through the graph rather than by
                // this API — so "no capability", not "capability unknown".
                level_caps: None,
                latency_ms,
                xruns: node_xruns,
                // Outputs-only: a source has no receiver to refuse us, and an alignment
                // hold takes speakers, never inputs.
                last_error: None,
                held: false,
            }
        })
        .collect();

    outputs.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    sources.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    // Only links between listed entities; intent pointing at an unadopted output
    // stays in the store, dormant, and reappears when that output is added.
    let mut links: Vec<RoutingLink> = intent.iter().filter(|l| adopted.contains(&l.output)).cloned().collect();
    links.sort();

    RoutingMatrix { sources, outputs, links }
}

/// The outputs an alignment run holds exclusively right now — what
/// [`RoutingNode::held`] is built from.
///
/// The hold registry is the single source of truth (`align/group.rs`); this only names
/// the seam, so a test can assert on the matrix's *input* where the hold is formed and
/// released, and on the mapping from that input to the field where the matrix is built.
/// Empty — and allocation-free — whenever nothing is aligning, which is almost always.
pub(crate) fn held_for_alignment() -> std::collections::BTreeSet<String> {
    crate::align::group::registry().reserved()
}

/// Snapshot the matrix from shared state (locks registry + routing intent).
/// Lock order is fixed here — routing first (released before the others), then
/// registry — to stay deadlock-free.
pub(crate) async fn build_snapshot(state: &AppState) -> RoutingMatrix {
    // Snapshot the sendspin volumes up front, before any sync (std::sync::Mutex)
    // locks are taken — the control is an async mutex and its guard must not be
    // held across the sync section below (and never across an await).
    let (sendspin_volumes, sendspin_mutes) = {
        let c = state.sendspin_control.lock().await;
        (c.volumes(), c.mutes())
    };
    // `connected()` comes from the same guard as the volumes: it's what makes an
    // AP2 output's `streaming` verdict (routing/mod.rs `RoutingNode::streaming`).
    let (ap2_volumes, ap2_mutes, ap2_connected) = {
        let c = state.ap2_control.lock().await;
        (c.volumes(), c.mutes(), c.connected())
    };
    let intent = store::routing::snapshot(&state.routing);
    let adopted = crate::store::outputs::adopted_snapshot(&state.outputs);
    let output_labels = crate::store::outputs::names_snapshot(&state.outputs);
    let devices = state.sendspin_devices.lock_recover().clone();
    let ap2_devices = state.ap2_devices.lock_recover().clone();
    // One guard for the hosts *and* their reported levels, so the matrix cannot show a
    // host as present while sourcing its volume from a different instant.
    let (pwsink_hosts, pwsink_volumes, pwsink_mutes) = {
        let a = state.agents.lock().await;
        let hosts = a.connected_targets();
        let mut vols = std::collections::HashMap::new();
        let mut mutes = std::collections::HashMap::new();
        for name in hosts.keys() {
            if let Some(st) = a.state(name) {
                if let Some(v) = st.volume {
                    vols.insert(name.clone(), v);
                }
                if let Some(m) = st.muted {
                    mutes.insert(name.clone(), m);
                }
            }
        }
        (hosts, vols, mutes)
    };
    let (lat, source_labels) = {
        use crate::sources::SourceConfig;
        let sources = state.sources.lock_recover();
        let sync = state.sync_settings.lock_recover();
        // Per-source input buffer + label, keyed by node name, for every
        // configured source instance (AirPlay or RTP).
        let mut source_latencies = std::collections::HashMap::new();
        let mut source_labels = std::collections::HashMap::new();
        for entry in sources.list() {
            let node_name = entry.node_name();
            let ms = match &entry.config {
                SourceConfig::Airplay(c) => c.latency_msec,
                SourceConfig::Rtp(c) => c.latency_msec,
            };
            source_latencies.insert(node_name.clone(), ms);
            source_labels.insert(node_name, entry.label.clone());
        }
        let lat = LatencyConfig {
            source_latencies,
            group_lead_ms: sync.group_lead_ms(),
            sendspin_delays: sync.sendspin_delays(),
            ap2_delays: sync.ap2_latencies(),
            ap2_default_ms: crate::outputs::ap2::server::AP2_RENDER_DELAY_MS,
            pwsink_jitters: sync.pwsink_jitters(),
            pwsink_default_ms: u32::from(crate::routing::sync_settings::DEFAULT_PWSINK_JITTER_MS),
        };
        (lat, source_labels)
    };
    let xruns = state.xruns.lock_recover().clone();
    let held = held_for_alignment();
    let reg = state.pw.lock_recover();
    build_matrix(
        &reg,
        &devices,
        &ap2_devices,
        &pwsink_hosts,
        &adopted,
        &source_labels,
        &output_labels,
        &state.meters,
        &intent,
        &sendspin_volumes,
        &sendspin_mutes,
        &ap2_volumes,
        &ap2_mutes,
        &pwsink_volumes,
        &pwsink_mutes,
        &ap2_connected,
        &lat,
        &xruns,
        &held,
    )
}

/// Present source nodes as `(node_name, node_id)` — the set the meter hub taps
/// while the matrix is being watched.
pub(crate) fn present_source_meters(matrix: &RoutingMatrix) -> Vec<(String, u32)> {
    matrix.sources.iter().filter_map(|s| s.node_id.map(|id| (s.node_name.clone(), id))).collect()
}

/// Every non-monitor output-direction port on `source_node_id` paired with the
/// same-channel input-direction port on `output_node_id`, as `LinkSpec`s
/// (resolved to PipeWire object ids) ready for a `CreateLinks` command. Empty
/// if either node doesn't exist or no channel suffixes match on both sides.
fn matched_port_specs(state: &SharedState, source_node_id: u32, output_node_id: u32) -> Vec<LinkSpec> {
    matched_port_specs_impl(state, source_node_id, output_node_id, false)
}

/// Like [`ensure_link_by_name`] but links the `source` node's **monitor** output
/// ports (a sink's `monitor_*` tap) into `output`'s inputs. Used to fan a group
/// anchor's monitor into a follower sink (e.g. an rtp-sink for the pw-sink
/// backend) — the anchor is the steady QUANT-1024 driver, the follower pulls
/// from its monitor at its own rate. Idempotency is handled by PipeWire (a
/// duplicate link is rejected); callers re-invoke freely.
pub async fn ensure_monitor_link_by_name(pw: &SharedState, pw_cmd: &PwCommandSender, source: &str, output: &str) {
    let ids = {
        let st = pw.lock_recover();
        node_id_for(&st, source).zip(node_id_for(&st, output))
    };
    let Some((source_id, output_id)) = ids else { return };
    let specs = matched_port_specs_impl(pw, source_id, output_id, true);
    if specs.is_empty() {
        return;
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::CreateLinks { specs, reply: reply_tx }).is_err() {
        return;
    }
    let _ = reply_rx.await;
}

/// `want_monitor` selects which of the source node's output ports to match:
/// `false` = normal outputs (excluding `monitor_*`), `true` = only `monitor_*`.
fn matched_port_specs_impl(state: &SharedState, source_node_id: u32, output_node_id: u32, want_monitor: bool) -> Vec<LinkSpec> {
    let state = state.lock_recover();
    if !state.nodes.contains_key(&source_node_id) || !state.nodes.contains_key(&output_node_id) {
        return Vec::new();
    }

    let source_ports: Vec<&PortInfo> = state
        .ports
        .values()
        .filter(|p| p.node_id == source_node_id && p.direction == "out" && p.port_name.starts_with("monitor_") == want_monitor)
        .collect();
    let output_ports: Vec<&PortInfo> = state.ports.values().filter(|p| p.node_id == output_node_id && p.direction == "in").collect();

    source_ports
        .iter()
        .filter_map(|sp| {
            let suffix = channel_suffix(&sp.port_name);
            output_ports.iter().find(|op| channel_suffix(&op.port_name) == suffix).map(|op| LinkSpec {
                out_node: source_node_id,
                out_port: sp.port_id,
                in_node: output_node_id,
                in_port: op.port_id,
            })
        })
        .collect()
}

/// Highest (newest) live node id for a stable node name, or `None` if the node
/// isn't present. Node names aren't unique at the PipeWire level and a
/// same-named node can briefly outlive its owner, so "highest id" reliably
/// means "most recently created" — same reasoning as sendspin_server's
/// wait_for_node_id.
pub(crate) fn node_id_for(state: &RegistryState, node_name: &str) -> Option<u32> {
    state.nodes.values().filter(|n| n.node_name == node_name).map(|n| n.node_id).max()
}

/// The set of sources feeding `output` in the intent (unique, sorted). Shared
/// with routing/sync_group/mod.rs, which keys sync groups by this source-set.
pub(crate) fn source_set_of<'a>(intent: &'a [RoutingLink], output: &str) -> std::collections::BTreeSet<&'a str> {
    intent.iter().filter(|l| l.output == output).map(|l| l.source.as_str()).collect()
}

/// Ensure the matched-channel PipeWire links from `source` to `output` exist,
/// by stable node name. Idempotent (CreateLinks skips already-linked ports);
/// a no-op if either node is absent or no channels match. Used by the sendspin
/// grouping reconciler to wire a source into a group sink (which isn't in the
/// routing intent by name — the member *devices* are).
pub async fn ensure_link_by_name(pw: &SharedState, pw_cmd: &PwCommandSender, source: &str, output: &str) {
    let ids = {
        let st = pw.lock_recover();
        node_id_for(&st, source).zip(node_id_for(&st, output))
    };
    let Some((source_id, output_id)) = ids else { return };
    let specs = matched_port_specs(pw, source_id, output_id);
    if specs.is_empty() {
        return;
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::CreateLinks { specs, reply: reply_tx }).is_err() {
        return;
    }
    let _ = reply_rx.await;
}

/// Reapply persisted routing intent (store/routing.rs) to the live graph: for
/// every stored `(source, output)` link whose *both* nodes are currently
/// present, ensure the matched-channel PipeWire links exist. Idempotent —
/// `CreateLinks` skips ports already linked — so this is safe to call on every
/// registry change. **Additive only**: it never removes links, so a manual
/// unlink (which also drops the intent) stays gone and this can't fight the
/// user. Intent whose endpoint is absent is simply left pending until the node
/// (re)appears and a later call links it.
///
/// After the RAOP output path was removed, every routable output is *virtual*
/// (sendspin/AP2 devices with no live PipeWire node), so `node_id_for(output)`
/// returns `None` and this loop no-ops for them — their audio path is built by
/// routing/sync_group/mod.rs from a group anchor, not by a direct link here. The loop is
/// kept (rather than deleted) so a future real-node output would still be
/// direct-linked, and it stays a cheap no-op for the current output kinds.
pub async fn reconcile(pw: &SharedState, pw_cmd: &PwCommandSender, routing: &SharedRouting) {
    let intent = store::routing::snapshot(routing);
    for link in &intent {
        let ids = {
            let st = pw.lock_recover();
            node_id_for(&st, &link.source).zip(node_id_for(&st, &link.output))
        };
        let Some((source_id, output_id)) = ids else { continue };
        let specs = matched_port_specs(pw, source_id, output_id);
        if specs.is_empty() {
            continue;
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if pw_cmd.send(PwCommand::CreateLinks { specs, reply: reply_tx }).is_err() {
            return; // PipeWire thread gone; nothing more to do
        }
        let _ = reply_rx.await;
    }
}

/// A routing operation, by stable node name (not ephemeral id) so it works
/// even when an endpoint isn't currently present — linking an offline device
/// just records the intent, which reconcile() applies when it appears.
#[derive(Deserialize)]
pub struct LinkPairRequest {
    /// Source node name.
    source: String,
    /// Output node name.
    output: String,
}

#[derive(Serialize)]
pub struct LinkOpResponse {
    ok: bool,
    message: String,
}

pub async fn get_routing(State(state): State<AppState>) -> Json<RoutingMatrix> {
    Json(build_snapshot(&state).await)
}

pub async fn link(State(state): State<AppState>, Json(req): Json<LinkPairRequest>) -> Json<LinkOpResponse> {
    // Human-initiated (routing-graph / API) — logged distinctly so a person's
    // actions can be told apart from stack-driven churn (reconcile, discovery,
    // source cycling) when reading logs. Grep `USER ACTION`.
    tracing::info!("USER ACTION: link '{}' → '{}' (routing graph)", req.source, req.output);
    // Only an adopted output is routable. The matrix never offers an unadopted
    // one, so this only fires for a direct API call (an automation, or a UI tab
    // left open while the device was removed) — but it must be refused rather
    // than persisted, or the link would sit in the store and quietly take effect
    // the moment the device were added.
    if !state.outputs.lock_recover().is_adopted(&req.output) {
        return Json(LinkOpResponse {
            ok: false,
            message: format!("'{}' isn't one of your outputs — add it on the Outputs page first", req.output),
        });
    }
    // Persist the desired route first; it's the source of truth and reconcile()
    // (re)applies it whenever both endpoints are present.
    if let Err(e) = state.routing.lock_recover().add(&req.source, &req.output) {
        return Json(LinkOpResponse { ok: false, message: format!("failed to persist routing intent: {e}") });
    }
    // Nudge the reconcilers (routing/groups/anchor) so intent changes take
    // effect promptly, not only on the next PipeWire registry event.
    let _ = state.changes.send(());
    // Every output is now virtual (sendspin/AP2): its audio path is built by
    // routing/sync_group/mod.rs from a group anchor, not a direct link here. If the output
    // has no live PipeWire node (the normal case), there's nothing to link now —
    // the reconcilers woken above build the path.
    let ids = {
        let st = state.pw.lock_recover();
        node_id_for(&st, &req.source).zip(node_id_for(&st, &req.output))
    };
    let Some((source_id, output_id)) = ids else {
        return Json(LinkOpResponse { ok: true, message: "saved; routing via sync group".to_string() });
    };
    let specs = matched_port_specs(&state.pw, source_id, output_id);
    if specs.is_empty() {
        return Json(LinkOpResponse { ok: true, message: "saved; no matching channel ports yet".to_string() });
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    if state.pw_cmd.send(PwCommand::CreateLinks { specs, reply: reply_tx }).is_err() {
        return Json(LinkOpResponse { ok: false, message: "pipewire thread unavailable".to_string() });
    }
    match reply_rx.await {
        Ok(Ok(message)) => Json(LinkOpResponse { ok: true, message }),
        Ok(Err(message)) => Json(LinkOpResponse { ok: false, message }),
        Err(_) => Json(LinkOpResponse { ok: false, message: "pipewire thread dropped the request".to_string() }),
    }
}

pub async fn unlink(State(state): State<AppState>, Json(req): Json<LinkPairRequest>) -> Json<LinkOpResponse> {
    // Human-initiated (routing-graph "remove link") — logged distinctly (grep
    // `USER ACTION`) so it's not confused with stack-driven teardown.
    tracing::info!("USER ACTION: unlink '{}' → '{}' (routing graph)", req.source, req.output);
    // Drop the intent first so reconcile() won't re-create what we remove.
    // Works for offline pairs too — this is purely by name.
    if let Err(e) = state.routing.lock_recover().remove(&req.source, &req.output) {
        return Json(LinkOpResponse { ok: false, message: format!("failed to persist routing intent: {e}") });
    }
    // Wake the reconcilers: for an RTP→RAOP route the live links live on the
    // anchor (not a direct source→output link), so the anchor reconciler must
    // run to tear them (and the anchor) down.
    let _ = state.changes.send(());
    // Destroy any live links if both ends are present; nothing to do otherwise.
    let ids = {
        let st = state.pw.lock_recover();
        node_id_for(&st, &req.source).zip(node_id_for(&st, &req.output))
    };
    let Some((source_id, output_id)) = ids else {
        return Json(LinkOpResponse { ok: true, message: "unlinked (endpoint offline; intent cleared)".to_string() });
    };
    // Node-level unlink: remove every channel link feeding this output from this
    // source. Destroy is idempotent, so any that raced away are harmless.
    let link_ids: Vec<u32> = {
        let st = state.pw.lock_recover();
        st.links.values().filter(|l| l.output_node == source_id && l.input_node == output_id).map(|l| l.link_id).collect()
    };
    if link_ids.is_empty() {
        return Json(LinkOpResponse { ok: true, message: "no live links to remove".to_string() });
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    if state.pw_cmd.send(PwCommand::DestroyLinks { link_ids, reply: reply_tx }).is_err() {
        return Json(LinkOpResponse { ok: false, message: "pipewire thread unavailable".to_string() });
    }
    match reply_rx.await {
        Ok(Ok(message)) => Json(LinkOpResponse { ok: true, message }),
        Ok(Err(message)) => Json(LinkOpResponse { ok: false, message }),
        Err(_) => Json(LinkOpResponse { ok: false, message: "pipewire thread dropped the request".to_string() }),
    }
}

/// Forget all persisted routing for a stable node name — used by the matrix's
/// remove-✕ on an *offline* entity (an output or source that's configured/
/// previously-routed but currently absent). Purely intent-side: nothing live
/// to touch. After this the entity drops out of the matrix (no references
/// left); if it's a real device it'll reappear on its own, unrouted.
pub async fn forget_entity(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<LinkOpResponse> {
    tracing::info!("USER ACTION: forget entity '{}' (routing graph)", node_name);
    match state.routing.lock_recover().remove_entity(&node_name) {
        Ok(()) => Json(LinkOpResponse { ok: true, message: format!("forgot routing for '{node_name}'") }),
        Err(e) => Json(LinkOpResponse { ok: false, message: format!("failed to forget '{node_name}': {e}") }),
    }
}

/// One node's live figures on the fast lane. Both fields are omitted when there
/// is nothing to say, and a node with nothing to say is left out of the frame
/// entirely — that is what makes an idle house cost zero bytes.
#[derive(serde::Serialize)]
pub struct MeterSample {
    /// Recent peak level (0.0–1.0). Sources only, as in [`RoutingNode::peak`].
    #[serde(skip_serializing_if = "Option::is_none")]
    peak: Option<f32>,
    /// Cumulative xrun count from the PipeWire profiler, as in
    /// [`RoutingNode::xruns`].
    #[serde(skip_serializing_if = "Option::is_none")]
    xruns: Option<u32>,
}

/// The node names a meters frame may mention: everything the last matrix showed.
pub(crate) fn matrix_node_names(matrix: &RoutingMatrix) -> Vec<String> {
    matrix.sources.iter().chain(matrix.outputs.iter()).map(|n| n.node_name.clone()).collect()
}

/// Build the fast-lane payload for `nodes`, dropping the ones with nothing to
/// report.
///
/// Deliberately does **not** call `build_snapshot`: it takes the meter hub's and
/// the profiler's own locks and nothing else — no PipeWire registry lock, no
/// sendspin/AP2/agent async mutexes — because this runs four times a second per
/// client and the registry lock is shared with the PipeWire thread.
pub(crate) fn meter_samples(state: &AppState, nodes: &[String]) -> BTreeMap<String, MeterSample> {
    let xruns = state.xruns.lock_recover().clone();
    build_meter_samples(nodes, |name| state.meters.peak(name), &xruns)
}

/// The pure part of [`meter_samples`], so the "nothing to report is nothing sent"
/// rule can be tested without an `AppState`.
pub(crate) fn build_meter_samples(
    nodes: &[String],
    peak_of: impl Fn(&str) -> f32,
    xruns: &std::collections::HashMap<String, u32>,
) -> BTreeMap<String, MeterSample> {
    nodes
        .iter()
        .filter_map(|name| {
            // Both fields are omitted at zero, which is what keeps a quiet house at
            // an empty frame: an untapped node's 0.0 is "no signal" rather than a
            // measurement (and every output reports it), and a node that has never
            // dropped a cycle has nothing to say either. **Absent therefore means
            // zero**, and the client must read it that way — that is how a level
            // decaying to silence and an xrun counter at rest are expressed.
            let peak = Some(peak_of(name)).filter(|p| *p > 0.0);
            let sample = MeterSample { peak, xruns: xruns.get(name).copied().filter(|x| *x > 0) };
            (sample.peak.is_some() || sample.xruns.is_some()).then(|| (name.clone(), sample))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node that has never dropped a cycle reports nothing, so the profiler's
    /// zero-valued entries — which it emits for *every* active node in the graph,
    /// most of which the matrix never shows — cannot pad the frame.
    #[test]
    fn zero_xruns_are_not_reported() {
        let nodes = vec!["airplay-in".to_string()];
        let mut xruns = std::collections::HashMap::new();
        xruns.insert("airplay-in".to_string(), 0);
        assert!(build_meter_samples(&nodes, |_| 0.0, &xruns).is_empty());
    }

    /// The fast lane may only mention nodes the client can place on the matrix it
    /// already has — the profiler's map is graph-wide, so the node list is what
    /// bounds the frame.
    #[test]
    fn the_meters_frame_is_bounded_by_the_last_matrix() {
        let matrix = matrix_with(&["ap2-dev-dusche"], &[]);
        let names = matrix_node_names(&matrix);
        assert!(names.contains(&"ap2-dev-dusche".to_string()));

        let mut xruns = std::collections::HashMap::new();
        xruns.insert("ap2-dev-dusche".to_string(), 3);
        // A busy node the matrix does not show (an internal graph node the profiler
        // reports) stays out.
        xruns.insert("some-internal-node".to_string(), 99);
        let samples = build_meter_samples(&names, |_| 0.0, &xruns);
        assert_eq!(samples.keys().collect::<Vec<_>>(), vec!["ap2-dev-dusche"]);
    }

    use crate::sources::airplay::AIRPLAY_NODE_NAME;

    fn link(source: &str, output: &str) -> RoutingLink {
        RoutingLink { source: source.to_string(), output: output.to_string() }
    }

    #[test]
    fn source_set_of_collects_unique_sorted_sources() {
        let intent = vec![
            link(AIRPLAY_NODE_NAME, "sendspin-dev-kitchen"),
            link("other-source", "sendspin-dev-kitchen"),
            link(AIRPLAY_NODE_NAME, "ap2-dev-dusche"),
        ];
        let kitchen = source_set_of(&intent, "sendspin-dev-kitchen");
        assert!(kitchen.contains(AIRPLAY_NODE_NAME) && kitchen.contains("other-source") && kitchen.len() == 2);
        let dusche = source_set_of(&intent, "ap2-dev-dusche");
        assert_eq!(dusche.len(), 1);
    }

    /// The matrix is what the UI offers *and* what the HA integration turns into
    /// `media_player` entities, so an unadopted device must be absent from it —
    /// including any saved links pointing at it, which stay dormant in the store
    /// rather than being shown (or applied).
    fn matrix_with(adopted: &[&str], intent: &[RoutingLink]) -> RoutingMatrix {
        matrix_with_connected(adopted, intent, &[])
    }

    /// As [`matrix_with`], with `ap2_connected` naming the AP2 outputs whose sender
    /// has a live session (what `streaming` reports).
    fn matrix_with_connected(adopted: &[&str], intent: &[RoutingLink], ap2_connected: &[&str]) -> RoutingMatrix {
        matrix_of(adopted, intent, ap2_connected, &BTreeMap::new())
    }

    /// As [`matrix_with`], with the user's renames (store/outputs.rs).
    fn matrix_with_names(adopted: &[&str], intent: &[RoutingLink], names: &[(&str, &str)]) -> RoutingMatrix {
        let names: BTreeMap<String, String> = names.iter().map(|(n, l)| (n.to_string(), l.to_string())).collect();
        matrix_of(adopted, intent, &[], &names)
    }

    fn matrix_of(
        adopted: &[&str],
        intent: &[RoutingLink],
        ap2_connected: &[&str],
        output_labels: &BTreeMap<String, String>,
    ) -> RoutingMatrix {
        matrix_of_held(adopted, intent, ap2_connected, output_labels, &std::collections::BTreeSet::new())
    }

    /// As [`matrix_of`], with `held` naming the outputs an alignment run holds
    /// exclusively (what `held_for_alignment()` returns in production).
    fn matrix_of_held(
        adopted: &[&str],
        intent: &[RoutingLink],
        ap2_connected: &[&str],
        output_labels: &BTreeMap<String, String>,
        held: &std::collections::BTreeSet<String>,
    ) -> RoutingMatrix {
        let empty_names: std::collections::BTreeSet<String> = adopted.iter().map(|s| s.to_string()).collect();
        let ap2_connected: std::collections::HashSet<String> = ap2_connected.iter().map(|s| s.to_string()).collect();
        build_matrix(
            &RegistryState::default(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &empty_names,
            &std::collections::HashMap::new(),
            output_labels,
            &crate::pw::metering::MeterHub::default(),
            intent,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            // pwsink volumes / mutes
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &ap2_connected,
            &LatencyConfig {
                source_latencies: std::collections::HashMap::new(),
                group_lead_ms: 0,
                sendspin_delays: std::collections::BTreeMap::new(),
                ap2_delays: std::collections::BTreeMap::new(),
                ap2_default_ms: 0,
                pwsink_jitters: std::collections::BTreeMap::new(),
                pwsink_default_ms: 0,
            },
            &std::collections::HashMap::new(),
            held,
        )
    }

    /// As [`matrix_with`], with what a pw-sink host's agent reports about its level —
    /// which is what decides that host's capability, per output and per moment.
    fn matrix_with_pwsink_levels(adopted: &[&str], volumes: &[(&str, f32)], mutes: &[(&str, bool)]) -> RoutingMatrix {
        let empty_names: std::collections::BTreeSet<String> = adopted.iter().map(|s| s.to_string()).collect();
        let volumes: std::collections::HashMap<String, f32> = volumes.iter().map(|(n, v)| (n.to_string(), *v)).collect();
        let mutes: std::collections::HashMap<String, bool> = mutes.iter().map(|(n, m)| (n.to_string(), *m)).collect();
        build_matrix(
            &RegistryState::default(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &empty_names,
            &std::collections::HashMap::new(),
            &BTreeMap::new(),
            &crate::pw::metering::MeterHub::default(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &volumes,
            &mutes,
            &std::collections::HashSet::new(),
            &LatencyConfig {
                source_latencies: std::collections::HashMap::new(),
                group_lead_ms: 0,
                sendspin_delays: std::collections::BTreeMap::new(),
                ap2_delays: std::collections::BTreeMap::new(),
                ap2_default_ms: 0,
                pwsink_jitters: std::collections::BTreeMap::new(),
                pwsink_default_ms: 0,
            },
            &std::collections::HashMap::new(),
            &std::collections::BTreeSet::new(),
        )
    }

    fn caps_of(m: &RoutingMatrix, node: &str) -> Option<LevelCaps> {
        m.outputs.iter().chain(m.sources.iter()).find(|n| n.node_name == node).expect("node in matrix").level_caps
    }

    /// **The bug this field exists to prevent.** An AirPlay 2 receiver whose level has
    /// never been read has `volume: None` — and a knob all the same, in band over RTSP. A
    /// UI that gated its control on "did a level arrive?" hid a working control; one that
    /// gated on the *kind* hid the pw-sink control and offered a dead AP2 one. Only a
    /// per-output capability answers this, and only the daemon can compute it.
    #[test]
    fn an_unknown_level_is_not_an_absent_capability() {
        let m = matrix_with_pwsink_levels(&["ap2-dev-dusche", "sendspin-dev-kitchen"], &[], &[]);
        for node in ["ap2-dev-dusche", "sendspin-dev-kitchen"] {
            let caps = caps_of(&m, node).unwrap_or_else(|| panic!("{node} must report its capability"));
            assert!(caps.volume && caps.mute, "{node}'s knobs are in-band, so they exist before any level is known");
        }
        // ...and the *values* are still honestly absent.
        let ap2 = m.outputs.iter().find(|n| n.node_name == "ap2-dev-dusche").unwrap();
        assert_eq!(ap2.volume, None, "an unread level must not be fabricated");
    }

    /// A pw-sink host's answer is its agent's, so it is per output and per moment: the
    /// same kind reports both capabilities, one, or neither. The `mute`-without-`volume`
    /// case is real — a sink with no device route reports `channel_volumes` through the
    /// node's `Props` with no mute — and it is why these are two booleans.
    #[test]
    fn a_pwsink_hosts_capability_is_whatever_its_agent_reports() {
        let m = matrix_with_pwsink_levels(
            &["pwsink-dev-both", "pwsink-dev-level_only", "pwsink-dev-silent"],
            &[("pwsink-dev-both", 0.4), ("pwsink-dev-level_only", 0.5)],
            &[("pwsink-dev-both", false)],
        );
        assert_eq!(caps_of(&m, "pwsink-dev-both"), Some(LevelCaps { volume: true, mute: true }));
        assert_eq!(caps_of(&m, "pwsink-dev-level_only"), Some(LevelCaps { volume: true, mute: false }));
        // No agent answering at all: not tunable and not mutable *by its own knob* — the
        // alignment relay can still silence it, which is a different question.
        assert_eq!(caps_of(&m, "pwsink-dev-silent"), Some(LevelCaps { volume: false, mute: false }));
    }

    /// Every adopted output says *something* about its knobs, so a consumer never has to
    /// fall back to guessing from the kind — which is the failure mode `OutputKind` and
    /// this field exist to remove.
    #[test]
    fn every_output_kind_reports_a_capability_and_sources_report_none() {
        let adopted: Vec<String> = OutputKind::ALL.iter().map(|k| format!("{}test", k.prefix())).collect();
        let names: Vec<&str> = adopted.iter().map(String::as_str).collect();
        let m = matrix_with_pwsink_levels(&names, &[], &[]);
        assert_eq!(m.outputs.len(), OutputKind::ALL.len());
        for out in &m.outputs {
            assert!(out.level_caps.is_some(), "{} must state its capability", out.node_name);
        }
        // A source's level is PipeWire's own; the field is absent rather than all-false,
        // and absent from the wire entirely.
        let json = serde_json::to_value(&m.outputs[0]).unwrap();
        assert!(json.get("level_caps").is_some(), "an output's capability must reach the wire: {json}");
    }

    /// A rename has to reach the matrix, not just the Outputs page: this listing is
    /// what the routing graph draws, what the group editors label their chips with,
    /// and what the HA integration turns into `media_player` metadata.
    #[test]
    fn a_renamed_output_carries_its_name_into_the_matrix() {
        let m = matrix_with_names(&["ap2-dev-dusche"], &[], &[("ap2-dev-dusche", "Shower")]);
        assert_eq!(m.outputs.len(), 1);
        assert_eq!(m.outputs[0].display_name, "Shower");
        // Without one, the derived name still applies.
        let plain = matrix_with(&["ap2-dev-dusche"], &[]);
        assert_eq!(plain.outputs[0].display_name, "dusche");
    }

    #[test]
    fn unadopted_outputs_and_their_saved_links_stay_out_of_the_matrix() {
        let intent = vec![link(AIRPLAY_NODE_NAME, "ap2-dev-dusche")];
        let m = matrix_with(&[], &intent);
        assert!(m.outputs.is_empty(), "an unadopted output must not be routable");
        assert!(m.links.is_empty(), "its saved link must not show (nor be applied)");
        assert!(m.sources.is_empty(), "and it must not drag its source in on its own");
    }

    #[test]
    fn adopting_an_output_restores_the_routing_it_had() {
        let intent = vec![link(AIRPLAY_NODE_NAME, "ap2-dev-dusche"), link(AIRPLAY_NODE_NAME, "ap2-dev-pioneer")];
        let m = matrix_with(&["ap2-dev-dusche"], &intent);
        assert_eq!(m.outputs.len(), 1);
        // Offline (nothing in the registry) but listed, so its routing is visible.
        assert_eq!(m.outputs[0].node_name, "ap2-dev-dusche");
        assert!(!m.outputs[0].present);
        assert_eq!(m.links, vec![link(AIRPLAY_NODE_NAME, "ap2-dev-dusche")]);
        assert_eq!(m.sources.len(), 1);
    }

    /// `present` (reachable) and `streaming` (session up) are different questions for
    /// the dialed backends, and the matrix must answer both — reporting only presence
    /// is what let the routing graph animate a wire to a pw-sink target that had
    /// never accepted a session, while announcements to it were refused.
    #[test]
    fn dialed_outputs_report_session_state_separately_from_presence() {
        let intent = vec![link(AIRPLAY_NODE_NAME, "ap2-dev-dusche"), link(AIRPLAY_NODE_NAME, "pwsink-dev-david_local")];
        let m = matrix_with_connected(&["ap2-dev-dusche", "pwsink-dev-david_local"], &intent, &["ap2-dev-dusche"]);
        let by_name = |n: &str| m.outputs.iter().find(|o| o.node_name == n).expect("output listed");
        assert_eq!(by_name("ap2-dev-dusche").streaming, Some(true), "a connected AP2 sender is streaming");
        // No sender ever published a status for this target (pw_sink_liveness) — the
        // receiver-initiated handshake hasn't happened, so nothing is carried.
        assert_eq!(by_name("pwsink-dev-david_local").streaming, Some(false));
    }

    /// An alignment hold is exclusive (plan §12.3): the held speakers play nothing
    /// while it runs, so each held row has to say so — and, more importantly, each
    /// row that is *not* held must not. The field is a plain mirror of the hold
    /// registry's snapshot, which is what makes "it clears on release" true by
    /// construction: an empty snapshot cannot produce a held row.
    #[test]
    fn a_held_output_reports_it_and_the_others_do_not() {
        let held: std::collections::BTreeSet<String> = ["ap2-dev-dusche".to_string()].into_iter().collect();
        let adopted = ["ap2-dev-dusche", "sendspin-dev-kitchen"];
        let intent = vec![link(AIRPLAY_NODE_NAME, "ap2-dev-dusche")];
        let m = matrix_of_held(&adopted, &intent, &[], &BTreeMap::new(), &held);
        let by_name = |m: &RoutingMatrix, n: &str| m.outputs.iter().find(|o| o.node_name == n).expect("output listed").held;
        assert!(by_name(&m, "ap2-dev-dusche"), "a held speaker must say why it is silent");
        assert!(!by_name(&m, "sendspin-dev-kitchen"), "an output nobody is aligning is untouched");
        // The hold takes speakers, never inputs — a source is never held even when its
        // name somehow appears in the set.
        assert!(m.sources.iter().all(|s| !s.held));

        // Released (the registry snapshot is empty again): no row claims a hold, and the
        // key is gone from the frame entirely — the badge is driven by its presence, so a
        // stale `held: false` would be indistinguishable but bigger.
        let after = matrix_of_held(&adopted, &intent, &[], &BTreeMap::new(), &std::collections::BTreeSet::new());
        assert!(after.outputs.iter().all(|o| !o.held), "the hold released, so nothing is held");
        let json = serde_json::to_value(&after).unwrap();
        assert!(json["outputs"][0].get("held").is_none(), "an unheld row carries no `held` key: {json}");
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["outputs"][0]["held"], true, "a held row does: {json}");
    }

    /// A source has no session to be up — `streaming` must stay absent rather than
    /// claim `false` (which the UI would render as "not delivering").
    #[test]
    fn sources_carry_no_session_state() {
        let intent = vec![link(AIRPLAY_NODE_NAME, "sendspin-dev-kitchen")];
        let m = matrix_with(&["sendspin-dev-kitchen"], &intent);
        assert_eq!(m.sources.len(), 1);
        assert_eq!(m.sources[0].streaming, None);
        // Nor does a sendspin device: it always has a sender while it's adopted.
        assert_eq!(m.outputs[0].streaming, None);
    }
}
