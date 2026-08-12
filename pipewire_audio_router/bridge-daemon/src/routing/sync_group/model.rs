//! What a group *is*, in the two forms the reconciler compares.
//!
//! [`DesiredGroup`] is what the routing intent implies should exist — the members, the
//! sources feeding them, the codec and timing each needs. [`RunningGroup`] is what
//! actually exists in the graph right now: the anchor sink, the per-device senders,
//! the ports they hold. Reconciliation is the difference between the two, so keeping
//! them side by side is the point of this file.
//!
//! [`IdleSender`] is a sender kept alive with nothing routed into it, which is what
//! makes an announcement to an unrouted output possible without paying a device
//! reconnect first.

use super::*;

/// Connected receiver hosts, `node_name → label`, as
/// `outputs::pwsink::agent::Agents::connected_targets` reports them. A pw-sink target exists
/// because an agent is on the socket (plan §3) — mDNS is not consulted, and cannot
/// be: its node names lack the `_<user>` half that routing intent carries.
pub(crate) type PwsinkHosts = std::collections::BTreeMap<String, String>;

/// Ports for group servers' embedded sendspin listeners are allocated upward
/// from here (distinct from any manual-output base so they never collide).
pub(crate) const GROUP_BASE_PORT: u16 = 8930;

/// Control ports for a group's pw-sink AppleMIDI sessions are allocated upward
/// from here in steps of 2 (each session binds control + control+1 data). Well
/// clear of GROUP_BASE_PORT (sendspin) and the AP2 port so the ranges never
/// overlap.
pub(crate) const PWSINK_BASE_PORT: u16 = 6200;

/// Separator joining a sorted source-set into a group key (a control char that
/// can't appear in a node name, so the join is unambiguous).
pub(crate) const KEY_SEP: char = '\u{1f}';

/// A group the current intent + live graph call for.
pub(crate) struct DesiredGroup {
    /// Sources feeding the group (sorted, unique) — linked into the anchor.
    pub(crate) sources: Vec<String>,
    /// PRESENT sendspin device node names (sorted). Identity for "did the dialed
    /// set change?" (the server's dial filter is fixed at start).
    pub(crate) sendspin_node_names: Vec<String>,
    /// PRESENT sendspin devices as `(mDNS fullname, ws URL)` — what the server
    /// supervises. URLs come from the discovery registry (the daemon's single
    /// `_sendspin._tcp` browser), because a browser per server steals the shared mDNS
    /// daemon's one listener per type and silently blinds the others.
    pub(crate) sendspin_members: Vec<(String, String)>,
    /// Send-ahead this group's sendspin stream must use (µs): the user's configured
    /// group lead raised to the largest per-member requirement (`min_buffer_ms` +
    /// that member's static delay), which the spec makes mandatory rather than
    /// advisory. Part of the restart identity — the timeline's send-ahead is fixed at
    /// construction, so a change means a fresh server.
    pub(crate) sendspin_send_ahead_us: i64,
    /// Wire codec for this group's sendspin stream: the per-output choices narrowed
    /// by what the daemon can encode and what EVERY member decodes (one stream, one
    /// format). Part of the server's restart identity, like `ap2_rate` — changing it
    /// means a new `stream/start`, so the server is dropped and recreated.
    pub(crate) sendspin_codec: &'static str,
    /// PRESENT AP2 receivers in this group: (output node_name, resolved IP,
    /// per-output render delay override in ms — `None` = sender default), sorted
    /// by node_name. Identity for "did the receiver set *or its delay* change?"
    /// — a delay edit thus triggers the same drop-and-restart as a membership
    /// change, reconnecting the RTSP session with the new render buffer.
    pub(crate) ap2_members: Vec<(String, std::net::IpAddr, Option<u16>)>,
    /// Negotiated wire/capture rate for this group's AP2 senders (Hz): 48000 iff
    /// every AP2 member's effective rate is 48000, else 44100. Part of the AP2
    /// restart identity, so a rate change (e.g. a 48 kHz downgrade or a UI mode
    /// switch) restarts the senders + re-spawns the capture at the new rate.
    pub(crate) ap2_rate: u32,
    /// PRESENT pw-sink targets (remote PipeWire hosts) in this group, by output
    /// node name (`pwsink-dev-*`), sorted. Identity for "did the target set
    /// change?" — each target's AppleMIDI session is fixed at start, so a
    /// membership change is a drop-and-restart (only the pw-sink senders, never
    /// the shared anchor).
    pub(crate) pwsink_members: Vec<String>,
}

impl DesiredGroup {
    pub(crate) fn new(sources: &BTreeSet<&str>) -> Self {
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
pub(crate) struct RunningGroup {
    pub(crate) anchor_node_name: String,
    pub(crate) anchor_node_id: u32,
    pub(crate) port: u16,
    /// Live sendspin server (dropping it stops capture/dial but leaves the
    /// anchor intact); `None` when the group has no present sendspin devices.
    pub(crate) server: Option<SendspinServerHandle>,
    /// The sendspin device set currently in the group. Bookkeeping for the API/UI
    /// and the alignment wizard — deliberately NOT part of the server's restart
    /// identity (see the reconcile step that maintains it).
    pub(crate) server_devices: Vec<String>,
    /// The `(fullname, url)` set last pushed to the running server. Both a
    /// membership change and a mere *address* change are applied to the running
    /// server (supervise / stop_device), so this is what that diff is taken against.
    pub(crate) server_members: Vec<(String, String)>,
    /// The wire codec the running sendspin server was started with — half of its
    /// restart identity (a codec change needs a fresh `stream/start`).
    pub(crate) server_codec: &'static str,
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
    pub(crate) server_send_ahead_us: i64,
    /// Device node names whose *own* connection the next reconcile must recycle,
    /// set by [`GroupReconciler::force_device_reconnect`] — the static-delay path,
    /// where a reconnect is the *point* (current ESPHome firmware reads
    /// `SetStaticDelay` at stream start, not live).
    ///
    /// A **set of devices**, not a whole-group flag, because a device's static delay
    /// is a property of its own per-device sender: the other members' timestamps,
    /// codec and send-ahead are all unchanged, so there is nothing for them to
    /// re-arm. Restarting the group's single server to apply one speaker's
    /// calibration dropped every member — 219 ms of daemon work, but each
    /// reconnected speaker then went silent for tens of seconds (firmware-side), so
    /// a one-device tweak caused a group-wide outage
    /// (docs/old/sendspin-group-churn-plan.md §4.10, the same shape as the membership
    /// bug §4.1 fixed).
    ///
    /// The one case that genuinely IS group-wide — a delay big enough to raise the
    /// group's send-ahead high-water mark — is not handled here at all: it shows up
    /// as a *stream config* change (see [`sendspin_config_changed`]) and takes the
    /// ordinary restart path, which is correct.
    ///
    /// A set consumed by the reconcile pass rather than a teardown on the spot, so
    /// the graceful `stream/end` runs on the reconcile task like every other
    /// teardown.
    pub(crate) force_device_reconnect: BTreeSet<String>,
    /// Live AP2 senders (outputs/ap2/server.rs) for this group; drop = TEARDOWN each
    /// receiver session. `None` when the group has no present AP2 receivers.
    pub(crate) ap2_sender: Option<crate::outputs::ap2::server::Ap2ServerHandle>,
    /// AP2 receiver node names the running senders were started for — the restart
    /// identity. NOTE: render delay is deliberately NOT part of this: a delay change
    /// is applied LIVE (ap2_control → SetRenderDelay), never by a reconnect (that
    /// churn could silence a flaky receiver). Only membership/rate changes restart.
    pub(crate) ap2_members: Vec<String>,
    /// The AP2 capture/wire rate (Hz) the running senders were started at — part
    /// of the restart identity alongside `ap2_members`.
    pub(crate) ap2_rate: u32,
    /// Live pw-sink senders (outputs/pwsink/server.rs) for this group; drop = tear down
    /// each target's advertised session. `None` when the group has no present
    /// pw-sink targets.
    pub(crate) pwsink_server: Option<crate::outputs::pwsink::server::PwSinkServerHandle>,
    /// pw-sink target node names the running senders were started for — the
    /// restart identity (a membership change drops + recreates the senders).
    pub(crate) pwsink_members: Vec<String>,
    /// Control ports assigned to the running pw-sink senders (data = +1); tracked
    /// so port allocation avoids collisions across groups.
    pub(crate) pwsink_ports: Vec<u16>,
}

/// A standalone per-device sender for an *ungrouped* (idle) sendspin device, kept
/// alive so the device is always reachable — an announcement, or a volume command,
/// never pays a cold dial. It owns its own silent `null-audio-sink` (nothing routed
/// in → its monitor is silence) whose monitor the overlay mixer turns into the
/// announcement. Superseded by the device's group sender the moment it's routed
/// into a group.
///
/// It runs under [`sendspin::server::StreamPolicy::WhenAnnounced`]: the connection
/// stays up (dialed as `Discovery`, so it doesn't claim the device against another
/// server) but carries **no audio** until an announcement is headed for the device.
/// It used to `stream/start` on connect and push silence forever — see that enum
/// for why that was both a claim on the device and ~1.5 Mbit/s per idle speaker.
pub(crate) struct IdleSender {
    pub(crate) sink_node_name: String,
    pub(crate) sink_node_id: u32,
    pub(crate) port: u16,
    /// Torn down via `shutdown().await` when the device is grouped or leaves, so
    /// the connection is really gone before its group server dials the same
    /// device — not merely dropped.
    pub(crate) server: SendspinServerHandle,
}

/// Distinctive sink-name prefix for an idle device's private sink. Deliberately
/// not `sendspin-dev-`/`ap2-dev-`/`sync-grp-` so routing never treats it as an
/// output or anchor.
pub(crate) const IDLE_SINK_PREFIX: &str = "idle-dev-";

/// How long an on-demand announce session stays up after its clip stops playing.
/// Long enough that a burst of announcements (or a retry) reuses the warm session
/// instead of paying the connect again; short enough that an AP2 receiver's single
/// AirPlay session — and a pw-sink advert the LAN's receivers would otherwise
/// attach to — is handed back promptly. Also bounds the session's life: the mixer's
/// stall watchdog guarantees a clip always stops, so `clip length + this` is the
/// worst case.
pub(crate) const ANNOUNCE_LINGER: Duration = Duration::from_secs(30);

/// Tail added on top of an AP2 receiver's render delay when clamping the lease, so
/// audio already buffered on the receiver renders before TEARDOWN cuts it off.
pub(crate) const ANNOUNCE_TAIL: Duration = Duration::from_secs(2);

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

/// Shared handle so the alignment API (align/calibrate/mod.rs) can read the live group
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

/// A short human label for a group's embedded server / logs.
pub(crate) fn group_display(d: &DesiredGroup) -> String {
    let names: Vec<String> = d.sendspin_node_names.iter().map(|n| routing::output_display_name(n)).collect();
    format!("group: {}", names.join(", "))
}
