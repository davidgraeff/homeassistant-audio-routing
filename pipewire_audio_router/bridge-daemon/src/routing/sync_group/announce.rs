//! Announcement delivery into a group, including to outputs with nothing routed.
//!
//! An announcement needs a path to each target. When a target already has a running
//! sender the overlay mixer handles it; when it does not, one is stood up on demand
//! ([`AnnounceSession`]) and torn down after. [`AnnounceTransport`] is which of the
//! two applies, and [`no_transport_reason`] is the sentence a UI shows when neither
//! does — a target with no possible path should say so, not fail silently.
//!
//! [`dialed_session_established`] is the "is it actually connected yet" question, and
//! it is deliberately three-valued: yes, no, and *not knowable for this transport*.

use super::*;

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
pub(crate) struct AnnounceSession {
    pub(crate) sink_node_name: String,
    pub(crate) sink_node_id: u32,
    /// The live sender; drop = tear its session down.
    pub(crate) transport: AnnounceSessionTransport,
    /// Tear the session down at/after this instant. Extended while a clip is
    /// queued or playing on the output, and on every new announcement to it.
    pub(crate) expires_at: Instant,
    /// The lease length used for each extension (see [`ANNOUNCE_LINGER`], never
    /// shorter than an AP2 receiver's render delay plus a tail).
    pub(crate) linger: Duration,
}

/// The per-backend sender behind an [`AnnounceSession`].
pub(crate) enum AnnounceSessionTransport {
    /// AirPlay-2: drop = TEARDOWN the receiver's RTSP session.
    Ap2(crate::outputs::ap2::server::Ap2ServerHandle),
    /// pw-sink: drop = `BY` + withdraw the mDNS advert (the handle is held only for
    /// that, never read — hence the underscore). `control_port` is tracked so port
    /// allocation across groups and sessions never collides.
    PwSink { _server: crate::outputs::pwsink::server::PwSinkServerHandle, control_port: u16 },
}

impl AnnounceTransport {
    /// Whether the clip may legitimately sit unconsumed for a while (an on-demand
    /// session still connecting) — picks the mixer's stall grace.
    pub fn is_on_demand(&self) -> bool {
        matches!(self, Self::Starting | Self::Warm)
    }
}

/// The shared handles [`GroupReconciler::ensure_announce_transport`] needs to open a
/// transport on demand. Bundled so the one call site (api/announce.rs's `/api/announce`) can
/// build it once and the signature stays readable.
pub struct AnnounceDeps<'a> {
    pub pw: &'a SharedState,
    pub pw_cmd: &'a PwCommandSender,
    pub routing: &'a SharedRouting,
    /// Adoption verdicts (store/outputs.rs) — needed to read routing intent the
    /// same way `reconcile` does, so a *discovered* device's leftover intent
    /// doesn't look like a group that owns its session.
    pub outputs: &'a crate::store::outputs::SharedOutputs,
    pub ap2_devices: &'a crate::outputs::ap2::discovery::SharedAp2Devices,
    pub ap2_ptp: &'a crate::outputs::ap2::ptp::SharedAp2Ptp,
    pub ap2_control: &'a crate::outputs::ap2::volume::SharedAp2Control,
    pub sync_settings: &'a crate::routing::sync_settings::SharedSyncSettings,
    /// Receiver-host registry, for the pw-sink on-demand path: an announcement can
    /// only be opened to a host whose agent is connected to take the session.
    pub agents: &'a crate::outputs::pwsink::agent::SharedAgents,
}

/// Why an output with no live per-device sender can't carry an announcement, for
/// the caller to report. The dialed backends (AP2, pw-sink) are handled before this
/// — they have the on-demand path.
pub(crate) fn no_transport_reason(output: &str) -> String {
    if output.starts_with(SENDSPIN_DEV_PREFIX) {
        // Either it's offline, or it hasn't been added on the Outputs page — an
        // unadopted sendspin speaker gets no idle sender, and there's no on-demand
        // path for one (a fresh sendspin connection takes tens of seconds to start
        // rendering, so a "test tone" over it would arrive far too late to help).
        "sendspin device has no sender running for it — it's offline, or not added on the Outputs page yet".into()
    } else {
        "output has no per-device sender (only sendspin, AirPlay-2 and PipeWire targets can be announced to individually)".into()
    }
}

/// `n` free pw-sink control ports at/above [`PWSINK_BASE_PORT`], skipping any pair
/// overlapping `taken` (each session binds `control` **and** `control + 1`), so the
/// control/data pairs never collide. Pure, so the stepping is unit-testable.
pub(crate) fn next_free_pwsink_ports(taken: impl IntoIterator<Item = u16>, n: usize) -> Vec<u16> {
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
pub(crate) fn supports_on_demand_announce(output: &str) -> bool {
    output.starts_with(AP2_DEV_PREFIX) || output.starts_with(PWSINK_DEV_PREFIX)
}

/// **Is a session to `output` actually up right now?** — i.e. is audio dropped at
/// its end of the graph really being carried to the device?
///
/// Only the two **dialed** backends can answer this, and for them mDNS presence is
/// not the answer:
/// - **AP2**: `ap2_connected` (from `Ap2Control::connected`) = its sender
///   registered a command channel, i.e. the RTSP session is up. A receiver can be
///   reachable (so `present`) while its session is still connecting or has failed.
/// - **pw-sink**: [`PwSinkLiveness`](crate::pw_sink_liveness) `established` = a
///   receiver completed the AppleMIDI handshake to the session we advertise. That
///   handshake is *receiver*-initiated, so an advertised session with nobody
///   attached carries nothing — a target can sit `present` (mDNS) forever without
///   ever connecting.
///
/// `None` = the question doesn't apply: a sendspin device always has a sender
/// reading its overlay while it's adopted (grouped, or via its idle sender), and a
/// plain graph node has no session at all.
///
/// Shared by [`GroupReconciler::has_live_sender`] (which adds the sendspin cases it
/// alone can see) and the routing matrix (routing/mod.rs `RoutingNode::streaming`), so
/// the graph, the Outputs page and the announce arbiter all judge delivery by the
/// same rule.
pub fn dialed_session_established(output: &str, ap2_connected: &HashSet<String>) -> Option<bool> {
    if output.starts_with(AP2_DEV_PREFIX) {
        return Some(ap2_connected.contains(output));
    }
    if output.starts_with(PWSINK_DEV_PREFIX) {
        return Some(crate::outputs::pwsink::sender_liveness::PwSinkLiveness::global().get(output).is_some_and(|s| s.established));
    }
    None
}

/// The private silent sink backing an on-demand announce session for `output`.
/// Shares [`IDLE_SINK_PREFIX`] with the sendspin idle sinks (so routing ignores it
/// the same way) and keeps a per-kind marker, so the kinds can't collide.
pub(crate) fn announce_sink_name(output: &str) -> String {
    if let Some(slug) = output.strip_prefix(AP2_DEV_PREFIX) {
        format!("{IDLE_SINK_PREFIX}ap2-{slug}")
    } else if let Some(slug) = output.strip_prefix(PWSINK_DEV_PREFIX) {
        format!("{IDLE_SINK_PREFIX}pwsink-{slug}")
    } else {
        format!("{IDLE_SINK_PREFIX}{output}")
    }
}
