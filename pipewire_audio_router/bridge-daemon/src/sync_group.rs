//! Routing-driven **sync grouping** reconciler — sendspin multi-room grouping
//! and per-device AirPlay-2 senders in one model, so speakers routed from the
//! same sources play the same audio off one clock.
//!
//! ## The model
//!
//! Grouping is derived from routing intent, not declared: **every output routed
//! from the same set of sources belongs to one group**. Each group is backed by
//! one real `support.null-audio-sink` — the *sync anchor* (`SYNC_GRP_PREFIX`) —
//! which is the group's shared clock/timeline:
//!
//! - the group's sources are linked **into** the anchor;
//! - a filtered sendspin server (sendspin_server) captures **from** the anchor
//!   and dials exactly the group's sendspin devices, pushing one timestamped
//!   stream so they sync (see sendspin's `Group`);
//! - the group's AP2 receivers are driven by in-process senders (ap2_server.rs)
//!   that capture from the same anchor and stream realtime ALAC with libairptp
//!   PTP timing, so they share the same timeline.
//!
//! Because the anchor is one stable node per source-set, devices can come and
//! go — and the sendspin server / AP2 senders can be restarted when their
//! dialed set changes — without disturbing the anchor or the other members fed
//! from it.
//!
//! ## Reconcile
//!
//! Stateful (owns the running anchors/servers/AP2 senders) and serialized in the
//! single reconciler task (main.rs). On each change it diffs desired groups (from
//! intent + live devices) against running ones: tears down groups that are gone
//! (dropping the server + AP2 senders, destroying the anchor — its links go with
//! it), creates new anchors, and restarts a group's sendspin server / AP2 senders
//! when their dialed set (or the AP2 wire rate) changes.

use crate::config::{AP2_DEV_PREFIX, PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX, SYNC_GRP_PREFIX};
use crate::locks::LockRecover;
use crate::overlay_mixer::OverlayMixer;
use crate::pw_target_discovery::{PwTarget, SharedPwTargets};
use crate::pw_thread::{PwCommand, PwCommandSender, SharedState};
use crate::routing::{self, node_id_for};
use crate::routing_store::{self, RoutingLink, SharedRouting};
use crate::sendspin_discovery::{SendspinDevice, SharedSendspinDevices};
use crate::sendspin_server::{self, SendspinServerHandle};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Ports for group servers' embedded sendspin listeners are allocated upward
/// from here (distinct from any manual-output base so they never collide).
const GROUP_BASE_PORT: u16 = 8930;

/// Control ports for a group's pw-sink AppleMIDI sessions are allocated upward
/// from here in steps of 2 (each session binds control + control+1 data). Well
/// clear of GROUP_BASE_PORT (sendspin) and the AP2 port so the ranges never
/// overlap.
const PWSINK_BASE_PORT: u16 = 6200;

/// Separator joining a sorted source-set into a group key (a control char that
/// can't appear in a node name, so the join is unambiguous).
const KEY_SEP: char = '\u{1f}';

/// A group the current intent + live graph call for.
struct DesiredGroup {
    /// Sources feeding the group (sorted, unique) — linked into the anchor.
    sources: Vec<String>,
    /// PRESENT sendspin device node names (sorted). Identity for "did the dialed
    /// set change?" (the server's dial filter is fixed at start).
    sendspin_node_names: Vec<String>,
    /// PRESENT sendspin devices as `(mDNS fullname, ws URL)` — what the server
    /// supervises. URLs come from the discovery registry (the daemon's single
    /// `_sendspin._tcp` browser), because a browser per server steals the shared mDNS
    /// daemon's one listener per type and silently blinds the others.
    sendspin_members: Vec<(String, String)>,
    /// Send-ahead this group's sendspin stream must use (µs): the user's configured
    /// group lead raised to the largest per-member requirement (`min_buffer_ms` +
    /// that member's static delay), which the spec makes mandatory rather than
    /// advisory. Part of the restart identity — the timeline's send-ahead is fixed at
    /// construction, so a change means a fresh server.
    sendspin_send_ahead_us: i64,
    /// Wire codec for this group's sendspin stream: the per-output choices narrowed
    /// by what the daemon can encode and what EVERY member decodes (one stream, one
    /// format). Part of the server's restart identity, like `ap2_rate` — changing it
    /// means a new `stream/start`, so the server is dropped and recreated.
    sendspin_codec: &'static str,
    /// PRESENT AP2 receivers in this group: (output node_name, resolved IP,
    /// per-output render delay override in ms — `None` = sender default), sorted
    /// by node_name. Identity for "did the receiver set *or its delay* change?"
    /// — a delay edit thus triggers the same drop-and-restart as a membership
    /// change, reconnecting the RTSP session with the new render buffer.
    ap2_members: Vec<(String, std::net::IpAddr, Option<u16>)>,
    /// Negotiated wire/capture rate for this group's AP2 senders (Hz): 48000 iff
    /// every AP2 member's effective rate is 48000, else 44100. Part of the AP2
    /// restart identity, so a rate change (e.g. a 48 kHz downgrade or a UI mode
    /// switch) restarts the senders + re-spawns the capture at the new rate.
    ap2_rate: u32,
    /// PRESENT pw-sink targets (remote PipeWire hosts) in this group, by output
    /// node name (`pwsink-dev-*`), sorted. Identity for "did the target set
    /// change?" — each target's AppleMIDI session is fixed at start, so a
    /// membership change is a drop-and-restart (only the pw-sink senders, never
    /// the shared anchor).
    pwsink_members: Vec<String>,
}

impl DesiredGroup {
    fn new(sources: &BTreeSet<&str>) -> Self {
        Self {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            sendspin_node_names: Vec::new(),
            sendspin_members: Vec::new(),
            sendspin_send_ahead_us: 0,
            sendspin_codec: "pcm",
            ap2_members: Vec::new(),
            ap2_rate: 48_000,
            pwsink_members: Vec::new(),
        }
    }
}

/// A group currently running.
struct RunningGroup {
    anchor_node_name: String,
    anchor_node_id: u32,
    port: u16,
    /// Live sendspin server (dropping it stops capture/dial but leaves the
    /// anchor intact); `None` when the group has no present sendspin devices.
    server: Option<SendspinServerHandle>,
    /// The sendspin device set currently in the group. Bookkeeping for the API/UI
    /// and the alignment wizard — deliberately NOT part of the server's restart
    /// identity (see the reconcile step that maintains it).
    server_devices: Vec<String>,
    /// The `(fullname, url)` set last pushed to the running server. Both a
    /// membership change and a mere *address* change are applied to the running
    /// server (supervise / stop_device), so this is what that diff is taken against.
    server_members: Vec<(String, String)>,
    /// The wire codec the running sendspin server was started with — half of its
    /// restart identity (a codec change needs a fresh `stream/start`).
    server_codec: &'static str,
    /// The send-ahead the running sendspin server was started with (µs) — the other
    /// half of that identity, since the timeline fixes it at construction.
    ///
    /// Compared **one-way**: it is a high-water mark, so only a *higher* requirement
    /// restarts the server. The send-ahead is a floor the spec asks us to clear ("the
    /// maximum per-player send-ahead across grouped players") and it is derived from
    /// membership — so a device leaving lowers it, and honouring that lower value
    /// would reconnect every remaining member to buy 50 ms of latency back. On real
    /// ESPHome firmware a reconnect costs tens of seconds of silence (2026-07-28
    /// hardware test), so a stale-but-larger lead is enormously the cheaper of the
    /// two. Reset to 0 when the server stops, so the next one starts from the real
    /// requirement rather than a departed device's.
    server_send_ahead_us: i64,
    /// Set by [`GroupReconciler::force_server_restart`] to make the next reconcile
    /// restart this group's sendspin server even though its stream config is
    /// unchanged — the static-delay path, where a reconnect is the *point* (current
    /// ESPHome firmware reads `SetStaticDelay` at stream start, not live). A flag
    /// rather than dropping the handle on the spot, so the teardown still goes
    /// through the graceful path.
    force_restart: bool,
    /// Live AP2 senders (ap2_server.rs) for this group; drop = TEARDOWN each
    /// receiver session. `None` when the group has no present AP2 receivers.
    ap2_sender: Option<crate::ap2_server::Ap2ServerHandle>,
    /// AP2 receiver node names the running senders were started for — the restart
    /// identity. NOTE: render delay is deliberately NOT part of this: a delay change
    /// is applied LIVE (ap2_control → SetRenderDelay), never by a reconnect (that
    /// churn could silence a flaky receiver). Only membership/rate changes restart.
    ap2_members: Vec<String>,
    /// The AP2 capture/wire rate (Hz) the running senders were started at — part
    /// of the restart identity alongside `ap2_members`.
    ap2_rate: u32,
    /// Live pw-sink senders (pwsink_server.rs) for this group; drop = tear down
    /// each target's advertised session. `None` when the group has no present
    /// pw-sink targets.
    pwsink_server: Option<crate::pwsink_server::PwSinkServerHandle>,
    /// pw-sink target node names the running senders were started for — the
    /// restart identity (a membership change drops + recreates the senders).
    pwsink_members: Vec<String>,
    /// Control ports assigned to the running pw-sink senders (data = +1); tracked
    /// so port allocation avoids collisions across groups.
    pwsink_ports: Vec<u16>,
}

/// A standalone per-device sender for an *ungrouped* (idle) sendspin device, kept
/// alive so the device is always reachable — an announcement, or a volume command,
/// never pays a cold dial. It owns its own silent `null-audio-sink` (nothing routed
/// in → its monitor is silence) whose monitor the overlay mixer turns into the
/// announcement. Superseded by the device's group sender the moment it's routed
/// into a group.
///
/// It runs under [`sendspin_server::StreamPolicy::WhenAnnounced`]: the connection
/// stays up (dialed as `Discovery`, so it doesn't claim the device against another
/// server) but carries **no audio** until an announcement is headed for the device.
/// It used to `stream/start` on connect and push silence forever — see that enum
/// for why that was both a claim on the device and ~1.5 Mbit/s per idle speaker.
struct IdleSender {
    sink_node_name: String,
    sink_node_id: u32,
    port: u16,
    /// Torn down via `shutdown().await` when the device is grouped or leaves, so
    /// the connection is really gone before its group server dials the same
    /// device — not merely dropped.
    server: SendspinServerHandle,
}

/// An **on-demand** sender for an *unrouted* output, opened so an announcement can
/// reach it, then torn down again.
///
/// Sendspin devices get a permanently-running [`IdleSender`], but the two dialed
/// backends can't be held open like that:
///
/// - an **AirPlay-2** receiver accepts only ONE session at a time, so a permanent
///   one would block the household's phones from AirPlaying to it (and keep AVRs
///   parked on their AirPlay input);
/// - a **pw-sink** session is an advertised mDNS service plus bound ports, and
///   stock `module-rtp-session` in discover mode connects to *every* advertised
///   session it sees — a permanent advert per idle target would keep every
///   receiver on the LAN attached to sessions it has no reason to be in.
///
/// So an idle output of either kind gets a transport only while it's actually being
/// announced to — same shape as `IdleSender` otherwise (its own silent
/// `null-audio-sink`, whose monitor the overlay mixer turns into the announcement)
/// — plus a lease: it lingers a while after the clip so back-to-back announcements
/// skip the connect, then goes away.
struct AnnounceSession {
    sink_node_name: String,
    sink_node_id: u32,
    /// The live sender; drop = tear its session down.
    transport: AnnounceSessionTransport,
    /// Tear the session down at/after this instant. Extended while a clip is
    /// queued or playing on the output, and on every new announcement to it.
    expires_at: Instant,
    /// The lease length used for each extension (see [`ANNOUNCE_LINGER`], never
    /// shorter than an AP2 receiver's render delay plus a tail).
    linger: Duration,
}

/// The per-backend sender behind an [`AnnounceSession`].
enum AnnounceSessionTransport {
    /// AirPlay-2: drop = TEARDOWN the receiver's RTSP session.
    Ap2(crate::ap2_server::Ap2ServerHandle),
    /// pw-sink: drop = `BY` + withdraw the mDNS advert (the handle is held only for
    /// that, never read — hence the underscore). `control_port` is tracked so port
    /// allocation across groups and sessions never collides.
    PwSink { _server: crate::pwsink_server::PwSinkServerHandle, control_port: u16 },
}

#[derive(Default)]
pub struct GroupReconciler {
    /// Keyed by the group's source-set (sorted sources joined by `KEY_SEP`).
    running: HashMap<String, RunningGroup>,
    /// Standalone senders for ungrouped devices (per-device mode only), keyed by
    /// device node name.
    idle_senders: HashMap<String, IdleSender>,
    /// On-demand announce sessions for unrouted AP2 receivers / pw-sink targets,
    /// keyed by output node name (`ap2-dev-*` / `pwsink-dev-*`).
    announce_sessions: HashMap<String, AnnounceSession>,
    /// Something this pass wanted to do didn't take (a sender failed to start, an
    /// anchor didn't appear, a device had no resolved URL yet) and only a *retry*
    /// will fix it. The reconcile task is change-driven with no periodic tick, so
    /// without this a transient failure left a group silent until an unrelated
    /// event happened along — see [`Self::retry_wanted`].
    retry_wanted: bool,
}

/// Distinctive sink-name prefix for an idle device's private sink. Deliberately
/// not `sendspin-dev-`/`ap2-dev-`/`sync-grp-` so routing never treats it as an
/// output or anchor.
const IDLE_SINK_PREFIX: &str = "idle-dev-";

/// How long an on-demand announce session stays up after its clip stops playing.
/// Long enough that a burst of announcements (or a retry) reuses the warm session
/// instead of paying the connect again; short enough that an AP2 receiver's single
/// AirPlay session — and a pw-sink advert the LAN's receivers would otherwise
/// attach to — is handed back promptly. Also bounds the session's life: the mixer's
/// stall watchdog guarantees a clip always stops, so `clip length + this` is the
/// worst case.
const ANNOUNCE_LINGER: Duration = Duration::from_secs(30);

/// Tail added on top of an AP2 receiver's render delay when clamping the lease, so
/// audio already buffered on the receiver renders before TEARDOWN cuts it off.
const ANNOUNCE_TAIL: Duration = Duration::from_secs(2);

/// What will carry an announcement to an output, from
/// [`GroupReconciler::ensure_announce_transport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceTransport {
    /// A group (or idle) per-device sender is already streaming this output — the
    /// clip is consumed immediately.
    Live,
    /// An on-demand session was just opened; audio starts once the receiver is
    /// connected (AP2: pairing/SETUP; pw-sink: it discovers our advert and
    /// initiates the AppleMIDI handshake) — a few seconds either way.
    Starting,
    /// An on-demand session opened earlier is still up; its lease was extended.
    Warm,
    /// Nothing can carry the clip (reason for the caller to report).
    Unavailable(String),
}

impl AnnounceTransport {
    /// Whether the clip may legitimately sit unconsumed for a while (an on-demand
    /// session still connecting) — picks the mixer's stall grace.
    pub fn is_on_demand(&self) -> bool {
        matches!(self, Self::Starting | Self::Warm)
    }
}

/// The shared handles [`GroupReconciler::ensure_announce_transport`] needs to open a
/// transport on demand. Bundled so the one call site (api.rs's `/api/announce`) can
/// build it once and the signature stays readable.
pub struct AnnounceDeps<'a> {
    pub pw: &'a SharedState,
    pub pw_cmd: &'a PwCommandSender,
    pub routing: &'a SharedRouting,
    pub ap2_devices: &'a crate::ap2_discovery::SharedAp2Devices,
    pub ap2_ptp: &'a crate::ap2_ptp::SharedAp2Ptp,
    pub ap2_control: &'a crate::ap2_volume::SharedAp2Control,
    pub sync_settings: &'a crate::sync_settings::SharedSyncSettings,
    pub pw_targets: &'a SharedPwTargets,
}

/// Why an output with no live per-device sender can't carry an announcement, for
/// the caller to report. The dialed backends (AP2, pw-sink) are handled before this
/// — they have the on-demand path.
fn no_transport_reason(output: &str) -> String {
    if output.starts_with(SENDSPIN_DEV_PREFIX) {
        "sendspin device is offline (no sender running for it)".into()
    } else {
        "output has no per-device sender (only sendspin, AirPlay-2 and PipeWire targets can be announced to individually)".into()
    }
}

/// `n` free pw-sink control ports at/above [`PWSINK_BASE_PORT`], skipping any pair
/// overlapping `taken` (each session binds `control` **and** `control + 1`), so the
/// control/data pairs never collide. Pure, so the stepping is unit-testable.
fn next_free_pwsink_ports(taken: impl IntoIterator<Item = u16>, n: usize) -> Vec<u16> {
    let mut used: HashSet<u16> = HashSet::new();
    for p in taken {
        used.insert(p);
        used.insert(p.saturating_add(1));
    }
    let mut out = Vec::with_capacity(n);
    let mut port = PWSINK_BASE_PORT;
    while out.len() < n && port < u16::MAX - 1 {
        if !used.contains(&port) && !used.contains(&(port + 1)) {
            out.push(port);
            used.insert(port);
            used.insert(port + 1);
        }
        port = port.saturating_add(2);
    }
    out
}

/// Whether `output` is one of the dialed backends that can get a transport opened
/// on demand for an announcement (AP2 receivers, pw-sink targets).
fn supports_on_demand_announce(output: &str) -> bool {
    output.starts_with(AP2_DEV_PREFIX) || output.starts_with(PWSINK_DEV_PREFIX)
}

/// The private silent sink backing an on-demand announce session for `output`.
/// Shares [`IDLE_SINK_PREFIX`] with the sendspin idle sinks (so routing ignores it
/// the same way) and keeps a per-kind marker, so the kinds can't collide.
fn announce_sink_name(output: &str) -> String {
    if let Some(slug) = output.strip_prefix(AP2_DEV_PREFIX) {
        format!("{IDLE_SINK_PREFIX}ap2-{slug}")
    } else if let Some(slug) = output.strip_prefix(PWSINK_DEV_PREFIX) {
        format!("{IDLE_SINK_PREFIX}pwsink-{slug}")
    } else {
        format!("{IDLE_SINK_PREFIX}{output}")
    }
}

/// Shared handle so the alignment API (calibrate.rs) can read the live group
/// layout the reconcile task owns.
pub type SharedGroups = std::sync::Arc<tokio::sync::Mutex<GroupReconciler>>;

/// Read-only view of one running group, for the alignment wizard.
#[derive(Debug, Clone)]
pub struct GroupSnapshot {
    /// Source node names feeding this group (its stable identity).
    pub sources: Vec<String>,
    /// The group's sync-anchor node id — where calibration audio is injected so
    /// every member hears it off the one clock.
    pub anchor_node_id: u32,
    /// Present sendspin device node names in the group.
    pub sendspin_members: Vec<String>,
    /// Present AP2 receiver node names in the group (alignable by muting +
    /// tuning each one's live render delay).
    pub ap2_members: Vec<String>,
}

impl GroupReconciler {
    /// Force the sendspin server of the group containing `sendspin_node_name` to
    /// restart on the next reconcile. The devices reconnect and re-apply their
    /// static delay on connect — the only way current ESPHome firmware picks up a
    /// delay change (it reads `SetStaticDelay` at stream start, not live). Returns
    /// true if a group was found; the caller must nudge a reconcile
    /// (ChangeNotifier) afterwards.
    ///
    /// Sets a flag instead of dropping the handle here: membership is no longer part
    /// of the restart identity, so clearing the remembered device set would not
    /// restart anything — and the teardown belongs on the reconcile path, which can
    /// `await` the graceful shutdown (`stream/end` before the socket goes).
    pub fn force_server_restart(&mut self, sendspin_node_name: &str) -> bool {
        for g in self.running.values_mut() {
            if g.server_devices.iter().any(|d| d == sendspin_node_name) {
                g.force_restart = true;
                return true;
            }
        }
        false
    }

    /// Gracefully tear down every sendspin server — group senders and idle senders
    /// alike — so each device gets a `stream/end` before the process goes away.
    ///
    /// Without this the add-on's SIGTERM just kills the sockets under devices that
    /// have an active stream, and the 2026-07-28 hardware test says that is expensive:
    /// after an abrupt teardown a Voice PE stayed silent for **tens of seconds** once
    /// the new daemon reconnected it, while AP2 receivers on the same anchor were back
    /// in ~5 s. The old comment here — "sendspin group servers tear down with the
    /// process" — was true and was the problem.
    ///
    /// Bounded: each `shutdown()` carries its own `GRACEFUL_END` timeout, and they run
    /// concurrently, so the total wait is the slowest single one rather than their sum.
    pub async fn shutdown_sendspin(&mut self) {
        let mut handles: Vec<SendspinServerHandle> = Vec::new();
        for g in self.running.values_mut() {
            if let Some(h) = g.server.take() {
                handles.push(h);
                g.server_devices.clear();
                g.server_members.clear();
                g.server_send_ahead_us = 0;
            }
        }
        // Idle senders are connected too (that is their whole point), and an idle
        // sender mid-announcement has an active stream like any other.
        for (_dev, s) in std::mem::take(&mut self.idle_senders) {
            handles.push(s.server);
        }
        if handles.is_empty() {
            return;
        }
        tracing::info!("graceful shutdown: stream/end + close for {} sendspin server(s)", handles.len());
        let tasks: Vec<_> = handles.into_iter().map(|h| tokio::spawn(async move { h.shutdown().await })).collect();
        for t in tasks {
            let _ = t.await;
        }
    }

    /// Gracefully tear down every running group's pw-sink senders so each remote
    /// `module-rtp-session` receiver gets a clean AppleMIDI `BY` (+ advert
    /// withdraw) and drops its session immediately, rather than holding a stale
    /// session until a timeout after the daemon restarts. Called from the process
    /// shutdown path (main.rs): the async reconcile task's own `Drop` is not
    /// guaranteed to run on exit, so the teardown is triggered explicitly here.
    /// Best-effort and synchronous — `PwSinkServerHandle`/`AppleMidiSender` `Drop`
    /// sends the `BY` inline before returning.
    pub fn shutdown_pwsink(&mut self) {
        let groups = self.running.values().filter(|g| g.pwsink_server.is_some()).count();
        // On-demand announce sessions advertise the same way, so they need the same
        // clean BY (their sinks go with the process — nothing else references them).
        let announce: Vec<String> = self
            .announce_sessions
            .iter()
            .filter(|(_, s)| matches!(s.transport, AnnounceSessionTransport::PwSink { .. }))
            .map(|(o, _)| o.clone())
            .collect();
        if groups == 0 && announce.is_empty() {
            return;
        }
        // Log BEFORE dropping: the container's stop-grace can SIGKILL us partway
        // through the drop, so the tail of a post-drop log may never flush. The
        // BY itself still escapes — `AppleMidiSender::Drop` withdraws the advert
        // and sends BY *first*, before the worker-thread joins that a SIGKILL cuts.
        tracing::info!(
            "graceful shutdown: withdrawing advert + sending BY for {groups} pw-sink group session(s) and {} on-demand announce session(s)",
            announce.len()
        );
        for g in self.running.values_mut() {
            g.pwsink_server = None; // drop → advert withdraw + BY to peers
            g.pwsink_members.clear();
            g.pwsink_ports.clear();
        }
        for output in announce {
            self.announce_sessions.remove(&output); // drop → advert withdraw + BY
        }
    }

    /// Is a per-device sender currently streaming `output` (so an overlay dropped on
    /// it is consumed right away)? Covers every backend: a group's sendspin server /
    /// AP2 senders / pw-sink senders, plus a sendspin device's idle sender.
    ///
    /// For the two **dialed** backends, group membership is not the answer — it lists
    /// what the group *dialed*, including a receiver still connecting or one whose
    /// session failed, neither of which consumes an overlay. So:
    /// - **AP2**: `ap2_connected` (from `Ap2Control::connected`) = its sender
    ///   registered a command channel, i.e. the session is up.
    /// - **pw-sink**: `PwSinkLiveness` `established` = a receiver completed the
    ///   AppleMIDI handshake to our advertised session (it is receiver-initiated, so
    ///   an advertised session with nobody attached carries nothing).
    fn has_live_sender(&self, output: &str, ap2_connected: &HashSet<String>) -> bool {
        if output.starts_with(AP2_DEV_PREFIX) {
            return ap2_connected.contains(output);
        }
        if output.starts_with(PWSINK_DEV_PREFIX) {
            return crate::pw_sink_liveness::PwSinkLiveness::global().get(output).is_some_and(|s| s.established);
        }
        if self.idle_senders.contains_key(output) {
            return true;
        }
        self.running.values().any(|g| g.server_devices.iter().any(|d| d == output))
    }

    /// Make sure *something* will carry an announcement to `output`, and say what.
    ///
    /// An output only hears an announcement while a per-device sender is reading
    /// its overlay slot (sendspin_server / ap2_server / pwsink_server all call
    /// `OverlayMixer::mix_into`). Sendspin devices always have one — grouped or via
    /// their idle sender — but the **dialed** backends only have a sender while they
    /// are routed, so an unrouted AP2 receiver / pw-sink target had no transport at
    /// all and announcements to it were silently dropped. For those this opens an
    /// on-demand session ([`AnnounceSession`]); otherwise it reports honestly so the
    /// caller can tell the user instead of claiming "playing".
    ///
    /// Call this **before** starting the clip: it publishes the session's wire rate
    /// to the mixer, which `OverlayMixer::start` needs to rate-match the clip.
    pub async fn ensure_announce_transport(&mut self, output: &str, deps: &AnnounceDeps<'_>) -> AnnounceTransport {
        let ap2_connected = deps.ap2_control.lock().await.connected();
        let live = self.has_live_sender(output, &ap2_connected);
        // An on-demand session already up: extend its lease and reuse it (whether or
        // not it has finished connecting).
        if let Some(s) = self.announce_sessions.get_mut(output) {
            s.expires_at = Instant::now() + s.linger;
            return if live { AnnounceTransport::Live } else { AnnounceTransport::Warm };
        }
        if live {
            return AnnounceTransport::Live;
        }
        if !supports_on_demand_announce(output) {
            return AnnounceTransport::Unavailable(no_transport_reason(output));
        }

        // Only for an endpoint with NO wired input. A routed one belongs to the group
        // reconciler, which owns (and retries) its session; a second AP2 session would
        // collide (a receiver accepts one), and a second pw-sink advert would give the
        // receiver two sessions to attach to.
        let intent = routing_store::snapshot(deps.routing);
        if !routing::source_set_of(&intent, output).is_empty() {
            return AnnounceTransport::Unavailable(if output.starts_with(AP2_DEV_PREFIX) {
                "routed, but its AirPlay-2 sender isn't streaming (receiver unreachable, or still connecting)".into()
            } else {
                "routed, but no receiver has connected to its session yet (its module-rtp-session must initiate the handshake)".into()
            });
        }

        if output.starts_with(AP2_DEV_PREFIX) {
            self.open_ap2_announce_session(output, deps).await
        } else {
            self.open_pwsink_announce_session(output, deps).await
        }
    }

    /// Open an on-demand AP2 session for an unrouted receiver (see
    /// [`AnnounceSession`]). The lease outlasts the receiver's render buffer so a
    /// TEARDOWN can't cut off audio it hasn't rendered yet.
    async fn open_ap2_announce_session(&mut self, output: &str, deps: &AnnounceDeps<'_>) -> AnnounceTransport {
        let device = deps.ap2_devices.lock_recover().get(output).cloned();
        let Some(device) = device else {
            return AnnounceTransport::Unavailable("unknown AirPlay-2 receiver".into());
        };
        if !device.present {
            return AnnounceTransport::Unavailable("receiver is offline".into());
        }
        let Some(addr) = device.addr else {
            return AnnounceTransport::Unavailable("receiver has no resolved address yet".into());
        };
        // Receivers are registered as PTP peers at discovery; make sure the
        // host-global grandmaster is up so PT=87 anchors carry its clock id.
        let clock_id = match deps.ap2_ptp.ensure_started() {
            Ok(id) => id,
            Err(e) => return AnnounceTransport::Unavailable(format!("AP2 PTP grandmaster unavailable: {e}")),
        };
        let (rate, delay) = {
            let ss = deps.sync_settings.lock_recover();
            (ss.ap2_group_rate([output]), ss.ap2_latency(output))
        };

        let (sink_node_name, sink_node_id) = match self.ensure_announce_sink(output, deps).await {
            Ok(v) => v,
            Err(e) => return AnnounceTransport::Unavailable(e),
        };

        // Publish the wire rate BEFORE the clip starts: `OverlayMixer::start`
        // resamples the 48 kHz clip to the output's rate, and ap2_server only
        // publishes it once the receiver has connected — too late for a clip queued
        // now, which would then play back at the wrong pitch on a 44.1 kHz receiver.
        OverlayMixer::global().set_output_rate(output, rate);

        let server = match crate::ap2_server::start(
            vec![(output.to_string(), addr.ip(), delay)],
            sink_node_id,
            clock_id,
            deps.ap2_control.clone(),
            rate,
            deps.sync_settings.clone(),
        ) {
            Ok(handle) => handle,
            Err(e) => {
                self.abandon_announce_sink(output, sink_node_id, deps.pw_cmd).await;
                return AnnounceTransport::Unavailable(format!("failed to start an on-demand AirPlay-2 session: {e}"));
            }
        };

        let render_delay_ms = u64::from(delay.unwrap_or(crate::ap2_server::AP2_RENDER_DELAY_MS as u16));
        let linger = ANNOUNCE_LINGER.max(Duration::from_millis(render_delay_ms) + ANNOUNCE_TAIL);
        tracing::info!(
            "on-demand AP2 announce session for '{output}' ({}) opening @ {rate} Hz (sink '{sink_node_name}', lease {}s)",
            addr.ip(),
            linger.as_secs()
        );
        self.announce_sessions.insert(
            output.to_string(),
            AnnounceSession {
                sink_node_name,
                sink_node_id,
                transport: AnnounceSessionTransport::Ap2(server),
                expires_at: Instant::now() + linger,
                linger,
            },
        );
        AnnounceTransport::Starting
    }

    /// Open an on-demand pw-sink session for an unrouted remote PipeWire host: bind a
    /// control/data port pair and advertise `pwrouter-<slug>` for the target's
    /// `module-rtp-session` to attach to, fed from a private silent sink.
    ///
    /// The handshake is **receiver-initiated**, so the clip waits until the target
    /// notices the advert and connects — the overlay isn't consumed before then, so it
    /// still plays whole, a second or two late. Same caveat as a routed pw-sink
    /// session: stock `module-rtp-session` in discover mode attaches to *every*
    /// advertised session, so with 2+ pw-sink targets on one LAN an announcement aimed
    /// at one can be heard by the others (the deferred session-scoping decision, see
    /// docs/pipewire-sink-roadmap.md §4).
    async fn open_pwsink_announce_session(&mut self, output: &str, deps: &AnnounceDeps<'_>) -> AnnounceTransport {
        let target = deps.pw_targets.lock_recover().get(output).cloned();
        let Some(target) = target else {
            return AnnounceTransport::Unavailable("unknown PipeWire target".into());
        };
        if !target.present {
            return AnnounceTransport::Unavailable("target is not on the network (no mDNS advert)".into());
        }
        let Some(control_port) = self.alloc_pwsink_ports(1).first().copied() else {
            return AnnounceTransport::Unavailable("no free control port for a pw-sink session".into());
        };

        let (sink_node_name, sink_node_id) = match self.ensure_announce_sink(output, deps).await {
            Ok(v) => v,
            Err(e) => return AnnounceTransport::Unavailable(e),
        };

        let member = crate::pwsink_server::PwSinkMember { node_name: output.to_string(), control_port };
        let server = match crate::pwsink_server::start(vec![member], sink_node_id) {
            Ok(handle) => handle,
            Err(e) => {
                self.abandon_announce_sink(output, sink_node_id, deps.pw_cmd).await;
                return AnnounceTransport::Unavailable(format!("failed to start an on-demand pw-sink session: {e}"));
            }
        };

        tracing::info!(
            "on-demand pw-sink announce session for '{output}' advertising on control port {control_port} (sink '{sink_node_name}', lease {}s)",
            ANNOUNCE_LINGER.as_secs()
        );
        self.announce_sessions.insert(
            output.to_string(),
            AnnounceSession {
                sink_node_name,
                sink_node_id,
                transport: AnnounceSessionTransport::PwSink { _server: server, control_port },
                expires_at: Instant::now() + ANNOUNCE_LINGER,
                linger: ANNOUNCE_LINGER,
            },
        );
        AnnounceTransport::Starting
    }

    /// The private silent sink an on-demand session captures from: nothing is routed
    /// in, so its monitor is silence and the overlay mixer supplies the whole
    /// announcement. Reuses the node if one with this name already exists (a previous
    /// session's sink can outlive the daemon — they're created `object.linger`).
    /// Returns `(node_name, node_id)` or the reason to report.
    async fn ensure_announce_sink(&self, output: &str, deps: &AnnounceDeps<'_>) -> Result<(String, u32), String> {
        let sink_node_name = announce_sink_name(output);
        if let Some(id) = node_id_for(&deps.pw.lock_recover(), &sink_node_name) {
            return Ok((sink_node_name, id));
        }
        let (tx, rx) = oneshot::channel();
        if deps.pw_cmd.send(PwCommand::CreateSinkNode { node_name: sink_node_name.clone(), reply: tx }).is_err() {
            return Err("PipeWire thread unavailable".into());
        }
        match rx.await {
            Ok(Ok(())) => {}
            _ => return Err(format!("failed to create announce sink '{sink_node_name}'")),
        }
        match wait_for_node(deps.pw, &sink_node_name).await {
            Some(id) => Ok((sink_node_name, id)),
            None => Err(format!("announce sink '{sink_node_name}' did not appear in the graph")),
        }
    }

    /// Undo [`Self::ensure_announce_sink`] when the sender failed to start, so a
    /// retry doesn't inherit a stray sink (and the overlay rate doesn't stick).
    async fn abandon_announce_sink(&self, output: &str, sink_node_id: u32, pw_cmd: &PwCommandSender) {
        OverlayMixer::global().clear_output_rate(output);
        let (tx, rx) = oneshot::channel();
        if pw_cmd.send(PwCommand::DestroySinkNode { node_id: sink_node_id, reply: tx }).is_ok() {
            let _ = rx.await;
        }
    }

    /// Expire on-demand announce sessions whose lease has run out. Driven from a
    /// slow ticker in main.rs; a session with a clip still queued or playing keeps
    /// having its lease extended (the mixer's stall watchdog bounds that, so a
    /// receiver that never connects can't hold its session open forever).
    pub async fn poll_announce_sessions(&mut self, pw_cmd: &PwCommandSender) {
        if self.announce_sessions.is_empty() {
            return;
        }
        let mixer = OverlayMixer::global();
        // A clip that's playing on the output, or still queued for it, keeps the
        // lease rolling — the queued case has no overlay slot yet, so the mixer
        // alone would let the session go before the clip's turn came.
        let in_flight = crate::announce::AnnounceCoordinator::global().outputs_in_flight();
        let now = Instant::now();
        let mut expired: Vec<String> = Vec::new();
        for (output, s) in self.announce_sessions.iter_mut() {
            if mixer.is_active(output) || in_flight.contains(output) {
                s.expires_at = now + s.linger;
            } else if now >= s.expires_at {
                expired.push(output.clone());
            }
        }
        for output in expired {
            self.drop_announce_session(&output, pw_cmd, "lease expired").await;
        }
    }

    /// Tear down one on-demand announce session — AP2: TEARDOWN the receiver's RTSP
    /// session; pw-sink: `BY` + withdraw the advert (both on handle drop) — then
    /// destroy its private sink.
    async fn drop_announce_session(&mut self, output: &str, pw_cmd: &PwCommandSender, why: &str) {
        let Some(s) = self.announce_sessions.remove(output) else { return };
        tracing::info!("on-demand announce session for '{output}' torn down ({why}); removing sink '{}'", s.sink_node_name);
        drop(s.transport); // AP2: TEARDOWN + capture close; pw-sink: BY + advert withdraw
        OverlayMixer::global().clear_output_rate(output);
        let (tx, rx) = oneshot::channel();
        if pw_cmd.send(PwCommand::DestroySinkNode { node_id: s.sink_node_id, reply: tx }).is_ok() {
            let _ = rx.await;
        }
    }

    /// Graceful process-exit teardown for every AP2 session — group senders **and**
    /// on-demand announce sessions.
    ///
    /// A receiver accepts one AirPlay session at a time and holds a session we never
    /// closed until it times out: that is what makes the next start's first connect
    /// fail (the "cold/stale session" retry in `connect_one`) and what leaves the
    /// receiver's AirPlay input busy for phones in between. Dropping the handles only
    /// *signals* their tasks, and on process exit nothing polls those again — so this
    /// awaits them (each bounded inside `Ap2ServerHandle::shutdown`, all concurrently
    /// so an unreachable receiver doesn't serialize the rest).
    ///
    /// Called explicitly from main.rs's shutdown path, like `shutdown_pwsink`,
    /// because the reconcile task's own `Drop` isn't guaranteed to run on exit.
    pub async fn shutdown_ap2(&mut self) {
        let mut handles: Vec<crate::ap2_server::Ap2ServerHandle> = Vec::new();
        for g in self.running.values_mut() {
            if let Some(h) = g.ap2_sender.take() {
                handles.push(h);
                g.ap2_members.clear();
            }
        }
        // On-demand AP2 announce sessions too (pw-sink ones are handled by
        // `shutdown_pwsink`, whose BY is synchronous). Their private sinks go with the
        // process — nothing else references them — so only the RTSP session matters.
        let ap2_announce: Vec<String> = self
            .announce_sessions
            .iter()
            .filter(|(_, s)| matches!(s.transport, AnnounceSessionTransport::Ap2(_)))
            .map(|(o, _)| o.clone())
            .collect();
        for output in ap2_announce {
            tracing::debug!("graceful shutdown: tearing down on-demand announce session for '{output}'");
            if let Some(s) = self.announce_sessions.remove(&output) {
                if let AnnounceSessionTransport::Ap2(h) = s.transport {
                    handles.push(h);
                }
            }
        }
        if handles.is_empty() {
            return;
        }
        tracing::info!("graceful shutdown: TEARDOWN for {} AirPlay-2 session group(s)", handles.len());
        // Concurrently, via tasks (no `futures` dependency): each `shutdown()` carries
        // its own timeout, so the whole wait is bounded by the slowest single one
        // rather than their sum.
        let joins: Vec<tokio::task::JoinHandle<()>> = handles.into_iter().map(|h| tokio::spawn(h.shutdown())).collect();
        for j in joins {
            let _ = j.await;
        }
    }

    /// Snapshot every running group (anchor + members) for the alignment API.
    pub fn snapshot(&self) -> Vec<GroupSnapshot> {
        self.running
            .iter()
            .map(|(key, g)| GroupSnapshot {
                sources: key.split(KEY_SEP).map(str::to_string).collect(),
                anchor_node_id: g.anchor_node_id,
                sendspin_members: g.server_devices.clone(),
                ap2_members: g.ap2_members.clone(),
            })
            .collect()
    }
}

/// Stable, deterministic short id for a group key (no rng/time — those aren't
/// available and would break determinism; `DefaultHasher` has fixed keys).
fn group_hash(key: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The group key for a sorted source-set.
fn source_key(sources: &BTreeSet<&str>) -> String {
    sources.iter().copied().collect::<Vec<_>>().join(&KEY_SEP.to_string())
}

/// Compute the groups the current intent + live devices call for.
///
/// A group is materialized (gets an anchor) as soon as it has a live consumer
/// that needs one: a present sendspin device or a present AP2 receiver. Members
/// are grouped by their exact source-set, so a sendspin device and an AP2
/// receiver fed from the same sources land in one group and share its clock.
fn compute_desired(
    intent: &[RoutingLink],
    devices: &BTreeMap<String, SendspinDevice>,
    ap2_devices: &BTreeMap<String, crate::ap2_discovery::Ap2Device>,
    ap2_latencies: &BTreeMap<String, u16>,
    pw_targets: &BTreeMap<String, PwTarget>,
) -> BTreeMap<String, DesiredGroup> {
    let mut groups: BTreeMap<String, DesiredGroup> = BTreeMap::new();

    // Present sendspin devices → members of their source-set's group.
    for (dev_node, dev) in devices {
        let sources = routing::source_set_of(intent, dev_node);
        if sources.is_empty() {
            continue;
        }
        let g = groups.entry(source_key(&sources)).or_insert_with(|| DesiredGroup::new(&sources));
        g.sendspin_node_names.push(dev_node.clone());
        // A device with no resolved URL yet can't be dialed; it joins on a later
        // reconcile once mDNS has resolved it (the reconciler is nudged then).
        if let Some(url) = &dev.url {
            g.sendspin_members.push((dev.fullname.clone(), url.clone()));
        }
    }

    // Present AP2 receivers (with a resolved address) → members of their group.
    // Mirrors the sendspin loop; the audio path is built in reconcile step (e).
    for (dev_node, dev) in ap2_devices {
        let Some(addr) = dev.addr else { continue };
        let sources = routing::source_set_of(intent, dev_node);
        if sources.is_empty() {
            continue;
        }
        let g = groups.entry(source_key(&sources)).or_insert_with(|| DesiredGroup::new(&sources));
        g.ap2_members.push((dev_node.clone(), addr.ip(), ap2_latencies.get(dev_node).copied()));
    }

    // Present pw-sink targets (remote PipeWire hosts) → members of their group.
    // The audio path (per-target AppleMIDI sender) is built in reconcile step (e).
    for (node, tgt) in pw_targets {
        if !tgt.present {
            continue;
        }
        let sources = routing::source_set_of(intent, node);
        if sources.is_empty() {
            continue;
        }
        let g = groups.entry(source_key(&sources)).or_insert_with(|| DesiredGroup::new(&sources));
        g.pwsink_members.push(node.clone());
    }

    for g in groups.values_mut() {
        g.sendspin_node_names.sort();
        g.sendspin_members.sort();
        g.ap2_members.sort();
        g.pwsink_members.sort();
    }
    groups
}

impl GroupReconciler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Did the last [`Self::reconcile`] leave work undone that only a retry can
    /// finish? The reconcile task uses this to wake itself again after a short
    /// delay instead of waiting for the next unrelated change (main.rs).
    pub fn retry_wanted(&self) -> bool {
        self.retry_wanted
    }

    /// Lowest free port at/above the base not used by a running group or idle
    /// sender. `extra` lets a caller reserve ports it's about to assign in the
    /// same reconcile pass (before they land in `running`/`idle_senders`).
    fn alloc_port(&self, extra: &HashSet<u16>) -> u16 {
        let mut used: HashSet<u16> = self.running.values().map(|g| g.port).collect();
        used.extend(self.idle_senders.values().map(|s| s.port));
        used.extend(extra.iter().copied());
        let mut port = GROUP_BASE_PORT;
        while used.contains(&port) {
            port += 1;
        }
        port
    }

    /// Allocate `n` control ports for a group's pw-sink AppleMIDI sessions (each
    /// session also binds `control + 1` as its data port), avoiding every port a
    /// running group's pw-sink senders already hold. Ports step by 2 from
    /// PWSINK_BASE_PORT so the control/data pairs never overlap.
    fn alloc_pwsink_ports(&self, n: usize) -> Vec<u16> {
        // Both a group's senders and an on-demand announce session bind out of this
        // range, so both must be in the taken set.
        let group_ports = self.running.values().flat_map(|g| g.pwsink_ports.iter().copied());
        let announce_ports = self.announce_sessions.values().filter_map(|s| match &s.transport {
            AnnounceSessionTransport::PwSink { control_port, .. } => Some(*control_port),
            AnnounceSessionTransport::Ap2(_) => None,
        });
        next_free_pwsink_ports(group_ports.chain(announce_ports), n)
    }

    /// `send_ahead_us` is the group presentation lead from the sync settings
    /// (sync_settings.rs), applied to every group's sendspin server.
    pub async fn reconcile(
        &mut self,
        pw: &SharedState,
        pw_cmd: &PwCommandSender,
        routing: &SharedRouting,
        devices: &SharedSendspinDevices,
        control: &crate::sendspin_volume::SharedSendspinControl,
        send_ahead_us: i64,
        ap2_devices: &crate::ap2_discovery::SharedAp2Devices,
        ap2_ptp: &crate::ap2_ptp::SharedAp2Ptp,
        sync_settings: &crate::sync_settings::SharedSyncSettings,
        ap2_control: &crate::ap2_volume::SharedAp2Control,
        pw_targets: &SharedPwTargets,
    ) {
        // Re-earned every pass: whatever failed last time either succeeds now or
        // sets this again.
        self.retry_wanted = false;
        let intent = routing_store::snapshot(routing);
        let devices_map = devices.lock_recover().clone();
        let ap2_map = ap2_devices.lock_recover().clone();
        let ap2_latencies = sync_settings.lock_recover().ap2_latencies();
        let pw_targets_map = pw_targets.lock_recover().clone();
        let mut desired = compute_desired(&intent, &devices_map, &ap2_map, &ap2_latencies, &pw_targets_map);

        // Resolve each group's AP2 capture/wire rate from the per-output rate mode
        // + learned capability cache (48000 iff every member's effective rate is
        // 48000, else 44100). Done here (not in compute_desired) so the rate logic
        // stays with the settings store.
        {
            let ss = sync_settings.lock_recover();
            for d in desired.values_mut() {
                if !d.ap2_members.is_empty() {
                    d.ap2_rate = ss.ap2_group_rate(d.ap2_members.iter().map(|(n, _, _)| n.as_str()));
                }
                // Sendspin wire codec: one stream serves the whole group, so the
                // per-output choices are narrowed to something EVERY member decodes
                // (and the daemon can encode). A conflict resolves to PCM, which
                // every player must handle.
                if !d.sendspin_node_names.is_empty() {
                    let member_codecs: Vec<Vec<String>> = d
                        .sendspin_node_names
                        .iter()
                        .map(|n| devices_map.get(n).map(|dev| dev.supported_codecs.clone()).unwrap_or_default())
                        .collect();
                    // The group's mode is the least-permissive member choice: an
                    // explicit pick anywhere wins over Auto, and PCM wins over the
                    // rest (a member pinned to PCM must not be sent Opus).
                    let mode = d
                        .sendspin_node_names
                        .iter()
                        .map(|n| ss.sendspin_codec(n))
                        .min_by_key(|m| match m {
                            crate::sync_settings::SendspinCodec::Pcm => 0,
                            crate::sync_settings::SendspinCodec::Flac => 1,
                            crate::sync_settings::SendspinCodec::Opus => 2,
                            crate::sync_settings::SendspinCodec::Auto => 3,
                        })
                        .unwrap_or_default();
                    d.sendspin_codec = sendspin_server::resolve_codec(mode, member_codecs.iter());
                    // Send-ahead floor, resolved AFTER the codec because a compressed
                    // stream needs more decode headroom than PCM: whichever is larger of
                    // what each member reported and its codec's own minimum, plus that
                    // member's static delay.
                    let delays = ss.sendspin_delays();
                    d.sendspin_send_ahead_us = sendspin_server::required_send_ahead_us(
                        send_ahead_us,
                        d.sendspin_codec,
                        d.sendspin_node_names.iter().map(|n| {
                            (devices_map.get(n).and_then(|dev| dev.min_buffer_ms), delays.get(n).copied().unwrap_or(0))
                        }),
                    );
                }
            }
        }

        // 1. Tear down groups no longer desired (server first, then the anchor —
        //    destroying the anchor node takes its source/monitor links with it).
        let stale: Vec<String> = self.running.keys().filter(|k| !desired.contains_key(*k)).cloned().collect();
        for key in stale {
            if let Some(rg) = self.running.remove(&key) {
                tracing::info!(
                    "tearing down sync group {} ({} sendspin, {} ap2)",
                    rg.anchor_node_name,
                    rg.server_devices.len(),
                    rg.ap2_members.len()
                );
                if let Some(server) = rg.server {
                    // `stream/end` to each member before its socket goes, same as a
                    // config-change restart — an unrouted speaker should be told the
                    // stream is over, not left to notice a dead connection.
                    server.shutdown().await;
                }
                drop(rg.ap2_sender); // signals AP2 senders to TEARDOWN their receivers
                drop(rg.pwsink_server); // tears down each pw-sink target session (BY + advert)
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::DestroySinkNode { node_id: rg.anchor_node_id, reply: tx }).is_ok() {
                    let _ = rx.await;
                }
            }
        }

        // 1b. Idle-sender teardown. Every discovered device that isn't in a group
        //     keeps a standalone sender (so it's always reachable — e.g.
        //     announcements to an idle speaker). Drop the sender of any device that
        //     is now grouped or gone, BEFORE the group servers below dial, so a
        //     newly-grouped device isn't dialed by both its idle sender and its
        //     group at once.
        let grouped: HashSet<String> = desired.values().flat_map(|d| d.sendspin_node_names.iter().cloned()).collect();
        let want_idle: HashSet<String> = devices_map.keys().filter(|d| !grouped.contains(*d)).cloned().collect();
        let drop_idle: Vec<String> = self.idle_senders.keys().filter(|d| !want_idle.contains(*d)).cloned().collect();
        for dev in drop_idle {
            if let Some(s) = self.idle_senders.remove(&dev) {
                tracing::info!("idle sender '{}' torn down (device grouped or gone)", s.sink_node_name);
                // Awaited, not dropped: this runs immediately before the group
                // server below dials the same device, so the old connection must be
                // gone first — and if an announcement was playing through this
                // sender, the device is told the stream ended.
                s.server.shutdown().await;
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::DestroySinkNode { node_id: s.sink_node_id, reply: tx }).is_ok() {
                    let _ = rx.await;
                }
            }
        }

        // 1c. On-demand announce-session teardown. An AP2 receiver accepts ONE
        //     session and a pw-sink receiver would see two adverts, so an on-demand
        //     session must go BEFORE the group senders below dial/advertise for the
        //     same endpoint. Also drops sessions whose device/target went away (the
        //     handle would be streaming into the void).
        let grouped: HashSet<&str> = desired
            .values()
            .flat_map(|d| d.ap2_members.iter().map(|(n, _, _)| n.as_str()).chain(d.pwsink_members.iter().map(String::as_str)))
            .collect();
        let drop_announce: Vec<(String, &str)> = self
            .announce_sessions
            .keys()
            .filter_map(|o| {
                let still_there = if o.starts_with(AP2_DEV_PREFIX) {
                    ap2_map.get(o).is_some_and(|d| d.present && d.addr.is_some())
                } else {
                    pw_targets_map.get(o).is_some_and(|t| t.present)
                };
                if grouped.contains(o.as_str()) {
                    Some((o.clone(), "endpoint is now routed — its group sender takes over"))
                } else if !still_there {
                    Some((o.clone(), "endpoint went offline"))
                } else {
                    None
                }
            })
            .collect();
        for (output, why) in drop_announce {
            self.drop_announce_session(&output, pw_cmd, why).await;
        }

        // 2. Create/steer each desired group.
        for (key, d) in &desired {
            // a. Ensure the anchor sink exists (create + wait, within this call,
            //    so the wiring below finds it and we don't re-create next tick).
            if !self.running.contains_key(key) {
                let anchor_node_name = format!("{SYNC_GRP_PREFIX}{}", group_hash(key));
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::CreateSinkNode { node_name: anchor_node_name.clone(), reply: tx }).is_err() {
                    continue;
                }
                match rx.await {
                    Ok(Ok(())) => {}
                    _ => {
                        tracing::warn!("failed to create sync anchor '{anchor_node_name}' — retrying shortly");
                        self.retry_wanted = true;
                        continue;
                    }
                }
                let Some(anchor_node_id) = wait_for_node(pw, &anchor_node_name).await else {
                    tracing::warn!("sync anchor '{anchor_node_name}' did not appear in the graph in time — retrying shortly");
                    self.retry_wanted = true;
                    continue;
                };
                let port = self.alloc_port(&HashSet::new());
                tracing::info!("created sync anchor '{anchor_node_name}' (id {anchor_node_id}) for source(s) {:?}", d.sources);
                self.running.insert(
                    key.clone(),
                    RunningGroup {
                        anchor_node_name,
                        anchor_node_id,
                        port,
                        server: None,
                        server_devices: Vec::new(),
                        server_members: Vec::new(),
                        server_codec: "pcm",
                        server_send_ahead_us: 0,
                        force_restart: false,
                        ap2_sender: None,
                        ap2_members: Vec::new(),
                        ap2_rate: 48_000,
                        pwsink_server: None,
                        pwsink_members: Vec::new(),
                        pwsink_ports: Vec::new(),
                    },
                );
            }

            // Snapshot what we need so no borrow of `self.running` is held across
            // an await (the async link/server calls below).
            let (
                anchor_name,
                anchor_id,
                port,
                prev_members,
                prev_codec,
                prev_lead,
                prev_force,
                have_server,
                prev_ap2,
                prev_ap2_rate,
                prev_pwsink,
            ) = {
                let rg = self.running.get(key).expect("just inserted");
                (
                    rg.anchor_node_name.clone(),
                    rg.anchor_node_id,
                    rg.port,
                    rg.server_members.clone(),
                    rg.server_codec,
                    rg.server_send_ahead_us,
                    rg.force_restart,
                    rg.server.is_some(),
                    rg.ap2_members.clone(),
                    rg.ap2_rate,
                    rg.pwsink_members.clone(),
                )
            };

            // b. Wire each source into the anchor (idempotent).
            for source in &d.sources {
                routing::ensure_link_by_name(pw, pw_cmd, source, &anchor_name).await;
            }

            // c. The group's sendspin server. Each device is its own single-member
            //    sender sharing one timeline off the anchor capture, so a device can
            //    be ducked/overlaid independently while staying in sync (see
            //    sendspin_server).
            //
            //    Its restart identity is the **stream config** — the codec and the
            //    send-ahead, i.e. what `stream/start` carries and what the shared
            //    timeline fixes at construction. Membership is deliberately NOT part
            //    of it: `ClientManager::supervise` adds a device to a running server
            //    and the membership task gives any newly-connected client its own
            //    `Group` on the live timeline, so a join needs nothing torn down.
            //    Restarting for a join instead cost every *existing* member a full
            //    reconnect — 813 ms of it measured end-to-end, and every device's
            //    stream re-anchored (docs/sendspin-group-churn-plan.md §2b, H1).
            //
            //    A restart, when the config really did change, is still only the
            //    server — never the anchor — so AP2/RAOP outputs fed from the same
            //    anchor don't blip.
            let action = sendspin_server_action(SendspinServerState {
                routed: !d.sendspin_node_names.is_empty(),
                have_server,
                // Codec: any change. Send-ahead: only a RAISE — see
                // `server_send_ahead_us`, it is a high-water mark.
                config_changed: d.sendspin_codec != prev_codec || d.sendspin_send_ahead_us > prev_lead,
                force_restart: prev_force,
            });
            let restart = action == ServerAction::Start;
            if matches!(action, ServerAction::Start | ServerAction::Stop) {
                // Graceful and awaited: the members are told their stream ended
                // instead of having the socket pulled from under them, and the
                // listener is really gone before the new server binds the same port
                // below (see `SendspinServerHandle::shutdown`).
                if let Some(server) = self.running.get_mut(key).and_then(|rg| rg.server.take()) {
                    tracing::info!(
                        "sync group '{anchor_name}': stopping sendspin server ({}) — every member reconnects, and real firmware answers a reconnect with seconds of silence",
                        if action == ServerAction::Stop {
                            "no sendspin devices routed here any more".to_string()
                        } else if prev_force {
                            "a static-delay change needs a reconnect".to_string()
                        } else if d.sendspin_codec != prev_codec {
                            format!("wire codec {prev_codec} -> {}", d.sendspin_codec)
                        } else {
                            format!(
                                "a member needs a longer send-ahead than the running {} ms ({} ms)",
                                prev_lead / 1000,
                                d.sendspin_send_ahead_us / 1000
                            )
                        }
                    );
                    server.shutdown().await;
                }
                if let Some(rg) = self.running.get_mut(key) {
                    rg.server_devices = Vec::new();
                    rg.server_members = Vec::new();
                    rg.force_restart = false;
                    rg.server_send_ahead_us = 0; // the high-water mark dies with its server
                }
            }
            if restart {
                match sendspin_server::start_server_per_device(
                    &anchor_name,
                    &group_display(d),
                    port,
                    anchor_id,
                    d.sendspin_members.clone(),
                    d.sendspin_send_ahead_us,
                    control.clone(),
                    devices.clone(),
                    sendspin_server::StreamPolicy::Always,
                    d.sendspin_codec,
                )
                .await
                {
                    Ok(handle) => {
                        tracing::info!(
                            "sync group '{anchor_name}': per-device senders on port {port} dialing {} device(s), codec {}, send-ahead {} ms{}",
                            d.sendspin_members.len(),
                            d.sendspin_codec,
                            d.sendspin_send_ahead_us / 1000,
                            if d.sendspin_send_ahead_us > send_ahead_us {
                                // Say which rule raised it: a member's own request, our
                                // codec floor, and/or its static delay (which the device
                                // subtracts from every timestamp, so the server must send
                                // that much further ahead or its chunks land in the past).
                                format!(
                                    " (raised from the configured {} ms to cover a member's buffer requirement + its static delay)",
                                    send_ahead_us / 1000
                                )
                            } else {
                                String::new()
                            }
                        );
                        if let Some(rg) = self.running.get_mut(key) {
                            rg.server = Some(handle);
                            rg.server_codec = d.sendspin_codec;
                            rg.server_send_ahead_us = d.sendspin_send_ahead_us;
                        }
                    }
                    Err(e) => {
                        // Nothing else will nudge us: the reconcile task is
                        // change-driven, so without an explicit retry this group
                        // would have no sendspin output until an unrelated event
                        // happened along.
                        tracing::warn!("sync group '{anchor_name}': failed to start sendspin server: {e} — retrying shortly");
                        self.retry_wanted = true;
                    }
                }
            }

            // c2. Membership on the RUNNING server — the part that used to cost a
            //     restart. Three cases, all applied in place:
            //
            //     * a device joined → `supervise` dials it; the membership task puts
            //       it in its own group on the live timeline when it reports
            //       `client/state`, and its groupmates never notice;
            //     * a device left → `stop_device` ends *its* stream and stops *its*
            //       supervisor, gracefully;
            //     * a device re-resolved at a new address → `supervise` is idempotent
            //       per fullname, so an unchanged URL costs nothing and a changed one
            //       redials. (This is what the per-server mDNS browser used to do,
            //       before it turned out to be stealing every other server's
            //       subscription.)
            if !restart && prev_members != d.sendspin_members {
                let departed: Vec<String> = prev_members
                    .iter()
                    .map(|(fullname, _)| fullname.clone())
                    .filter(|fullname| !d.sendspin_members.iter().any(|(f, _)| f == fullname))
                    .collect();
                let arrived = d.sendspin_members.iter().filter(|(f, _)| !prev_members.iter().any(|(pf, _)| pf == f)).count();
                if let Some(server) = self.running.get(key).and_then(|rg| rg.server.as_ref()) {
                    for fullname in &departed {
                        server.stop_device(fullname).await;
                    }
                    server.supervise(&d.sendspin_members);
                    tracing::info!(
                        "sync group '{anchor_name}': sendspin membership now {} device(s) (+{arrived}/-{}) — no restart, the stream keeps running",
                        d.sendspin_members.len(),
                        departed.len()
                    );
                }
            }

            // Bookkeeping for the API/UI and the alignment wizard. Tracked whether or
            // not anything restarted, so a join that the running server absorbed is
            // still visible to `groups_snapshot`.
            if let Some(rg) = self.running.get_mut(key) {
                if rg.server.is_some() {
                    rg.server_devices = d.sendspin_node_names.clone();
                    rg.server_members = d.sendspin_members.clone();
                }
            }

            // d. (Re)start AP2 senders when the receiver set changes. Like sendspin,
            //    each per-device Connection is fixed at start, so a change means
            //    drop-and-recreate — only the senders, never the shared anchor.
            // Identity is the receiver SET + the negotiated group rate — a
            // membership change or a rate change (UI mode switch / cached 48→44.1
            // downgrade) restarts the senders. Render delay is intentionally NOT in
            // the identity: it's retuned live (ap2_control → SetRenderDelay), so a
            // delay edit never reconnects (that churn could silence a flaky receiver).
            let ap2_identity: Vec<String> =
                d.ap2_members.iter().map(|(n, _, _)| n.clone()).collect();
            if ap2_identity != prev_ap2 || d.ap2_rate != prev_ap2_rate {
                if let Some(rg) = self.running.get_mut(key) {
                    rg.ap2_sender = None; // drop → TEARDOWN old receiver sessions
                    rg.ap2_members = Vec::new();
                }
                if !d.ap2_members.is_empty() {
                    // Receivers are already PTP peers of the host-global grandmaster
                    // (registered at discovery); ensure it's up and get its clock id.
                    match ap2_ptp.ensure_started() {
                        Ok(clock_id) => match crate::ap2_server::start(d.ap2_members.clone(), anchor_id, clock_id, ap2_control.clone(), d.ap2_rate, sync_settings.clone()) {
                            Ok(handle) => {
                                tracing::info!(
                                    "sync group '{anchor_name}': AP2 senders streaming to {} receiver(s) @ {} Hz",
                                    d.ap2_members.len(), d.ap2_rate
                                );
                                if let Some(rg) = self.running.get_mut(key) {
                                    rg.ap2_sender = Some(handle);
                                    rg.ap2_members = ap2_identity;
                                    rg.ap2_rate = d.ap2_rate;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("sync group '{anchor_name}': failed to start AP2 senders: {e} — retrying shortly");
                                self.retry_wanted = true;
                            }
                        },
                        Err(e) => {
                            tracing::warn!("sync group '{anchor_name}': AP2 PTP grandmaster unavailable: {e} — retrying shortly");
                            self.retry_wanted = true;
                        }
                    }
                }
            }

            // e. (Re)start pw-sink senders when the target set changes. Each
            //    target's AppleMIDI session (advert + bound ports) is fixed at
            //    start, so a membership change is a drop-and-recreate — only the
            //    pw-sink senders, never the shared anchor (so co-routed sendspin/AP2
            //    outputs fed from the same anchor never blip). Fresh control ports
            //    are allocated per restart; the receiver reconnects to the new
            //    advertised session.
            if d.pwsink_members != prev_pwsink {
                if let Some(rg) = self.running.get_mut(key) {
                    rg.pwsink_server = None; // drop → tear down old target sessions
                    rg.pwsink_members = Vec::new();
                    rg.pwsink_ports = Vec::new();
                }
                if !d.pwsink_members.is_empty() {
                    let ports = self.alloc_pwsink_ports(d.pwsink_members.len());
                    let members: Vec<crate::pwsink_server::PwSinkMember> = d
                        .pwsink_members
                        .iter()
                        .zip(ports.iter())
                        .map(|(node_name, port)| crate::pwsink_server::PwSinkMember {
                            node_name: node_name.clone(),
                            control_port: *port,
                        })
                        .collect();
                    match crate::pwsink_server::start(members, anchor_id) {
                        Ok(handle) => {
                            tracing::info!(
                                "sync group '{anchor_name}': pw-sink senders advertising {} target session(s)",
                                d.pwsink_members.len()
                            );
                            if let Some(rg) = self.running.get_mut(key) {
                                rg.pwsink_server = Some(handle);
                                rg.pwsink_members = d.pwsink_members.clone();
                                rg.pwsink_ports = ports;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("sync group '{anchor_name}': failed to start pw-sink senders: {e} — retrying shortly");
                            self.retry_wanted = true;
                        }
                    }
                }
            }
        }

        // 3. Idle-sender creation (per-device mode): stand up a standalone sender
        //    for every ungrouped device that doesn't have one, so it's always
        //    reachable. Its own silent sink → it streams silence until the overlay
        //    mixer injects an announcement, then falls back to silence.
        for dev in &want_idle {
            if self.idle_senders.contains_key(dev) {
                continue;
            }
            let Some(fullname) = devices_map.get(dev).map(|d| d.fullname.clone()) else {
                continue;
            };
            // Single-device stream, so the codec is just this device's choice
            // narrowed by what it advertised (and what we can encode).
            let (idle_codec, idle_lead_us) = {
                let ss = sync_settings.lock_recover();
                let caps = devices_map.get(dev).map(|d| d.supported_codecs.clone()).unwrap_or_default();
                let codec = sendspin_server::resolve_codec(ss.sendspin_codec(dev), std::iter::once(&caps));
                let lead = sendspin_server::required_send_ahead_us(
                    send_ahead_us,
                    codec,
                    std::iter::once((
                        devices_map.get(dev).and_then(|d| d.min_buffer_ms),
                        ss.sendspin_delays().get(dev).copied().unwrap_or(0),
                    )),
                );
                (codec, lead)
            };
            let suffix = dev.strip_prefix(crate::config::SENDSPIN_DEV_PREFIX).unwrap_or(dev);
            let sink_node_name = format!("{IDLE_SINK_PREFIX}{suffix}");
            let (tx, rx) = oneshot::channel();
            if pw_cmd.send(PwCommand::CreateSinkNode { node_name: sink_node_name.clone(), reply: tx }).is_err() {
                continue;
            }
            match rx.await {
                Ok(Ok(())) => {}
                _ => {
                    tracing::warn!("idle sender: failed to create sink '{sink_node_name}' — retrying shortly");
                    self.retry_wanted = true;
                    continue;
                }
            }
            let Some(sink_node_id) = wait_for_node(pw, &sink_node_name).await else {
                tracing::warn!("idle sender: sink '{sink_node_name}' did not appear — retrying shortly");
                self.retry_wanted = true;
                continue;
            };
            let port = self.alloc_port(&HashSet::new());
            // One member, from the registry — the URL is what the single daemon-wide
            // browser resolved (an idle sender doesn't browse either).
            let Some(idle_url) = devices_map.get(dev).and_then(|d| d.url.clone()) else {
                tracing::debug!("idle sender for '{dev}': no resolved URL yet; retrying shortly");
                self.retry_wanted = true;
                continue;
            };
            let members = vec![(fullname, idle_url)];
            match sendspin_server::start_server_per_device(
                &sink_node_name,
                &format!("idle: {}", routing::output_display_name(dev)),
                port,
                sink_node_id,
                members,
                idle_lead_us,
                control.clone(),
                devices.clone(),
                // Idle: stay connected (warm + controllable) but stream nothing
                // until an announcement is actually headed for this device.
                sendspin_server::StreamPolicy::WhenAnnounced,
                idle_codec,
            )
            .await
            {
                Ok(server) => {
                    tracing::info!("idle sender for '{dev}' up on port {port} (silence until announced)");
                    self.idle_senders.insert(dev.clone(), IdleSender { sink_node_name, sink_node_id, port, server });
                }
                Err(e) => {
                    tracing::warn!("idle sender for '{dev}': failed to start: {e} — retrying shortly");
                    self.retry_wanted = true;
                    let (t, r) = oneshot::channel();
                    if pw_cmd.send(PwCommand::DestroySinkNode { node_id: sink_node_id, reply: t }).is_ok() {
                        let _ = r.await;
                    }
                }
            }
        }
    }
}

/// What a reconcile pass should do with one group's sendspin server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerAction {
    /// Nothing routed here and nothing running.
    Idle,
    /// Keep the running server exactly as it is. Membership and address changes
    /// land here — they're applied to the live server instead.
    KeepRunning,
    /// (Re)start it, tearing down any existing one first.
    Start,
    /// Tear it down: no sendspin device is routed to this group any more.
    Stop,
}

/// The inputs that decide it.
struct SendspinServerState {
    /// Is any sendspin device routed to this group at all? (Not "dialable" — a
    /// device whose URL hasn't resolved yet still wants the server up, and gets
    /// supervised the moment it resolves.)
    routed: bool,
    have_server: bool,
    /// Did the **stream config** change — the codec or the send-ahead? That is the
    /// server's whole restart identity, because it's what `stream/start` carries and
    /// what the shared timeline fixes at construction.
    config_changed: bool,
    /// A caller asked for a reconnect for its own reasons
    /// ([`GroupReconciler::force_server_restart`], the static-delay path).
    force_restart: bool,
}

/// Decide it. Extracted from `reconcile` so the rule is testable without a live
/// PipeWire graph — and because "which changes restart a group" is exactly the
/// thing that regressed: membership used to be part of the identity, so routing one
/// more speaker into a live group made every other member reconnect
/// (docs/sendspin-group-churn-plan.md §2b).
fn sendspin_server_action(s: SendspinServerState) -> ServerAction {
    match (s.routed, s.have_server) {
        (false, false) => ServerAction::Idle,
        (false, true) => ServerAction::Stop,
        (true, false) => ServerAction::Start,
        (true, true) if s.config_changed || s.force_restart => ServerAction::Start,
        (true, true) => ServerAction::KeepRunning,
    }
}

/// A short human label for a group's embedded server / logs.
fn group_display(d: &DesiredGroup) -> String {
    let names: Vec<String> = d.sendspin_node_names.iter().map(|n| routing::output_display_name(n)).collect();
    format!("group: {}", names.join(", "))
}

/// Poll until `node_name` is present in the live registry (or give up). Mirrors
/// sendspin_server's old wait-for-node before linking a freshly-created sink.
async fn wait_for_node(pw: &SharedState, node_name: &str) -> Option<u32> {
    for _ in 0..40 {
        if let Some(id) = node_id_for(&pw.lock_recover(), node_name) {
            return Some(id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A running group with the given members and no live handles — enough to
    /// exercise the "is something streaming this output?" bookkeeping.
    fn running_group(sendspin: &[&str], ap2: &[&str], pwsink: &[&str]) -> RunningGroup {
        RunningGroup {
            anchor_node_name: "sync-grp-test".into(),
            anchor_node_id: 1,
            port: GROUP_BASE_PORT,
            server: None,
            server_devices: sendspin.iter().map(|s| s.to_string()).collect(),
            server_members: Vec::new(),
            server_codec: "pcm",
            server_send_ahead_us: 0,
            force_restart: false,
            ap2_sender: None,
            ap2_members: ap2.iter().map(|s| s.to_string()).collect(),
            ap2_rate: 48_000,
            pwsink_server: None,
            pwsink_members: pwsink.iter().map(|s| s.to_string()).collect(),
            pwsink_ports: Vec::new(),
        }
    }

    /// The rule the 2026-07-28 churn report came down to: adding or removing a
    /// speaker must not restart the group's server, because a restart costs every
    /// *other* member a full reconnect and a re-anchored stream.
    #[test]
    fn membership_alone_does_not_restart_the_sendspin_server() {
        let running = |config_changed, force_restart| {
            sendspin_server_action(SendspinServerState { routed: true, have_server: true, config_changed, force_restart })
        };
        // A join or a departure changes neither the codec nor the send-ahead, so the
        // stream config is unchanged — and the server keeps running.
        assert_eq!(running(false, false), ServerAction::KeepRunning);
        // A codec or send-ahead change is a genuinely different `stream/start`; the
        // shared timeline fixes both at construction, so this one has to restart.
        assert_eq!(running(true, false), ServerAction::Start);
        // ...as does an explicitly forced reconnect (the static-delay path, where the
        // reconnect IS the point — firmware reads SetStaticDelay at stream start).
        assert_eq!(running(false, true), ServerAction::Start);
    }

    #[test]
    fn the_server_follows_whether_anything_is_routed() {
        let state = |routed, have_server| SendspinServerState { routed, have_server, config_changed: false, force_restart: false };
        // First device routed here ⇒ stand a server up.
        assert_eq!(sendspin_server_action(state(true, false)), ServerAction::Start);
        // Last device unrouted ⇒ take it down (and release its port + advert).
        assert_eq!(sendspin_server_action(state(false, true)), ServerAction::Stop);
        // Nothing either way ⇒ nothing to do; a group can be AP2-only.
        assert_eq!(sendspin_server_action(state(false, false)), ServerAction::Idle);
    }

    #[test]
    fn announce_sink_names_are_distinct_from_outputs_and_anchors() {
        for (output, expected) in
            [("ap2-dev-dusche", "idle-dev-ap2-dusche"), ("pwsink-dev-office", "idle-dev-pwsink-office")]
        {
            let name = announce_sink_name(output);
            assert_eq!(name, expected);
            // Routing must never mistake it for an output or a sync anchor.
            assert!(!name.starts_with(AP2_DEV_PREFIX));
            assert!(!name.starts_with(SENDSPIN_DEV_PREFIX));
            assert!(!name.starts_with(PWSINK_DEV_PREFIX));
            assert!(!name.starts_with(SYNC_GRP_PREFIX));
        }
    }

    #[test]
    fn has_live_sender_uses_membership_for_sendspin_and_real_state_for_dialed_backends() {
        let mut r = GroupReconciler::new();
        r.running.insert(
            "src".into(),
            running_group(&["sendspin-dev-kitchen"], &["ap2-dev-dusche", "ap2-dev-pioneer"], &["pwsink-dev-office"]),
        );
        let connected: HashSet<String> = ["ap2-dev-dusche".to_string()].into_iter().collect();
        assert!(r.has_live_sender("sendspin-dev-kitchen", &connected));
        assert!(r.has_live_sender("ap2-dev-dusche", &connected));
        // Routed (a dialed group member) but its session never came up: an overlay
        // dropped on it would go nowhere, so this must NOT read as live.
        assert!(!r.has_live_sender("ap2-dev-pioneer", &connected));
        // Same for pw-sink, whose handshake is receiver-initiated: group membership
        // means we advertise a session, not that anyone attached to it. (The global
        // liveness registry is empty in this test = nobody attached.)
        assert!(!r.has_live_sender("pwsink-dev-office", &connected));
        // Unrouted endpoints have no sender at all — the case that used to make
        // announcements silently disappear.
        assert!(!r.has_live_sender("ap2-dev-bad", &connected));
        assert!(!r.has_live_sender("pwsink-dev-bad", &connected));
    }

    #[test]
    fn only_the_dialed_backends_get_an_on_demand_transport() {
        assert!(supports_on_demand_announce("ap2-dev-dusche"));
        assert!(supports_on_demand_announce("pwsink-dev-office"));
        assert!(!supports_on_demand_announce("sendspin-dev-kitchen"));
        assert!(!supports_on_demand_announce("some-local-sink"));
        // …and the kinds that don't must explain themselves rather than drop the clip.
        for (output, needle) in [("sendspin-dev-kitchen", "offline"), ("some-local-sink", "no per-device sender")] {
            let msg = no_transport_reason(output);
            assert!(msg.contains(needle), "{output}: {msg:?} lacks {needle:?}");
        }
    }

    #[test]
    fn pwsink_ports_step_past_pairs_already_taken() {
        // Nothing taken → consecutive control/data pairs from the base.
        assert_eq!(next_free_pwsink_ports([], 2), vec![PWSINK_BASE_PORT, PWSINK_BASE_PORT + 2]);
        // A group's session holds the first pair (control + data) → step past both.
        assert_eq!(next_free_pwsink_ports([PWSINK_BASE_PORT], 1), vec![PWSINK_BASE_PORT + 2]);
        // An on-demand announce session's port is fed in the same way, so a group
        // starting afterwards can't be handed the port it's already bound to.
        assert_eq!(
            next_free_pwsink_ports([PWSINK_BASE_PORT, PWSINK_BASE_PORT + 2], 2),
            vec![PWSINK_BASE_PORT + 4, PWSINK_BASE_PORT + 6]
        );
    }

    #[test]
    fn on_demand_transports_get_the_longer_stall_grace() {
        assert!(AnnounceTransport::Starting.is_on_demand());
        assert!(AnnounceTransport::Warm.is_on_demand());
        assert!(!AnnounceTransport::Live.is_on_demand());
        assert!(!AnnounceTransport::Unavailable("x".into()).is_on_demand());
    }
}
