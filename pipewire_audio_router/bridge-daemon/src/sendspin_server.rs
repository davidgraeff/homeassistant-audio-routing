//! Embedded sendspin server for one configured output — replaces
//! adapter.py's per-output `SendspinServer` + `PushStream` + `pw-record`
//! subprocess.
//!
//! One instance per `SendspinOutput`: creates the sink node natively
//! (`pw_thread::PwCommand::CreateSinkNode`), captures from it natively
//! (`sendspin_capture`), and runs an embedded `sendspin` server role —
//! `ServerListener` for inbound dial-in plus `ClientManager` for discovering
//! and dialing devices that only run their own embedded server (e.g. Home
//! Assistant Voice PE) — pushing the captured PCM to one shared `Group`.
//! Mirrors the exact composition already built and validated against real
//! hardware in `sendspin-rs`'s own `examples/play_wav.rs`; the only new part
//! here is where the PCM comes from.
//!
//! Discovery is **filtered** to the device set the caller passes
//! (`ClientManager::start with ConnectionReason::Discovery`), so a server only ever dials the devices
//! its group/idle sender owns — we don't compete for devices that aren't ours.
//!
//! ## Claiming a device, and not claiming it
//!
//! The spec allows several servers to be connected to one device and leaves the
//! keep-or-switch decision to the *device*, which weighs each server's
//! `connection_reason` and can send `client/goodbye { another_server }` to the
//! loser. So how we connect is a policy decision, not a formality — see
//! [`StreamPolicy`]: a group server dials `Playback` and streams; an idle sender
//! dials `Discovery` and streams **nothing** until an announcement is actually
//! headed for the device. The old idle behaviour (dial `Playback`, `stream/start`,
//! then push silence forever) both looked like the active server to any
//! keep-or-switch policy and cost ~1.5 Mbit/s per idle device, which also kept
//! the device out of WiFi power-save.

use crate::locks::LockRecover;
use sendspin::protocol::messages::{AudioFormatSpec, ClientHello, ClientSyncState, ConnectionReason, Message, StreamPlayerConfig};
use sendspin::server::{Advertisement, ClientEvent, ClientManager, Group, ServerSender, SharedTimeline, SharesTimeline};
use sendspin::{Clock, DefaultClock, ServerConnection, ServerListener};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

/// When a connected device joins its group — which is what sends `stream/start`
/// and starts it consuming audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPolicy {
    /// On connect, and for as long as it stays connected: a routed group's music
    /// path. Dials announcing [`ConnectionReason::Playback`] — we connected in
    /// order to stream.
    Always,
    /// Only while an announcement is playing on (or queued for) the device — the
    /// *idle* path for an ungrouped device.
    ///
    /// The connection stays warm (so an announcement, or a volume command, never
    /// pays a cold dial), but between announcements it carries **no audio at all**:
    /// the device is not in a group, so it gets no `stream/start` and no chunks.
    /// Two reasons that matters, and they're why this is not the old
    /// stream-silence-forever behaviour:
    ///
    /// 1. **It stops claiming the device.** The spec lets several servers connect
    ///    to one client and leaves the keep-or-switch choice to the *client*, which
    ///    weighs each server's `connection_reason`. An idle sender that announced
    ///    `Playback` and kept pushing (silent) audio looked exactly like the active
    ///    server, which can stop the device switching to one the user asked to
    ///    play. This dials [`ConnectionReason::Discovery`] — the spec's own
    ///    "discovery/announcement" case, which is precisely what an idle sender is.
    /// 2. **It stops the traffic.** The wire format is PCM 48 kHz/16-bit/stereo =
    ///    ~1.5 Mbit/s *per idle device*, continuously, and a device that is
    ///    receiving a stream can't drop into WiFi power-save.
    WhenAnnounced,
}

impl StreamPolicy {
    /// The `connection_reason` a dial under this policy should announce.
    fn connection_reason(self) -> ConnectionReason {
        match self {
            Self::Always => ConnectionReason::Playback,
            Self::WhenAnnounced => ConnectionReason::Discovery,
        }
    }
}

/// How often the arm task re-evaluates which [`StreamPolicy::WhenAnnounced`]
/// devices should be streaming. Sets the delay between "announcement admitted" and
/// `stream/start`, so it stays well inside the mixer's stall grace and is
/// imperceptible next to the send-ahead lead.
const ARM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Devices that are connected but not (yet) in a group: `client_id → (output node
/// name, its sender, when it connected)`. See `spawn_membership_task`.
type SharedPending = Arc<Mutex<HashMap<String, (String, ServerSender, std::time::Instant)>>>;

/// `client_id`s that have sent their initial `client/state` — the spec's gate on
/// sending them any binary audio.
type SharedReady = Arc<Mutex<std::collections::HashSet<String>>>;

/// How long a freshly-connected device may go without sending its initial
/// `client/state` before we stream to it anyway. The spec requires waiting for that
/// message, but firmware that never sends one must not end up permanently silent — so
/// the wait is bounded, and taking it is logged.
const READY_GRACE: Duration = Duration::from_secs(3);

/// Extra time an armed idle device keeps its stream after its announcement
/// finishes, before `stream/end`. The device renders on a send-ahead lead, so
/// ending the stream the instant the clip's last byte is queued would cut off audio
/// still sitting in its buffer.
const ANNOUNCE_DRAIN: Duration = Duration::from_millis(1500);

/// How long a deliberate teardown may spend telling devices their stream ended
/// before it stops waiting and closes anyway ([`SendspinServerHandle::shutdown`],
/// [`SendspinServerHandle::stop_device`]).
///
/// `broadcast_stream_end` awaits the write reaching each member's socket, and a
/// device that vanished mid-stream (powered off, WiFi gone) may not fail that write
/// for as long as the kernel's TCP retransmit budget — minutes. The teardown runs on
/// the reconcile task, which holds the group lock, so an unbounded wait there would
/// freeze all routing. A healthy device acks in well under a millisecond (the writer
/// task is local), so this is generous for the case it serves and cheap for the case
/// it guards against.
const GRACEFUL_END: Duration = Duration::from_millis(300);

/// Fixed by the spec for real deployments (matches sendspin-rs's own
/// convention and this daemon's other sendspin-adjacent code).
const SENDSPIN_PATH: &str = "/sendspin";

/// Best-effort real-time scheduling for the capture→wire relay thread.
///
/// The relay must never queue behind the daemon's general-purpose async work
/// (HTTP API, mDNS, discovery, other groups) — that starvation is the primary
/// sendspin stutter cause (RC1 in docs/audio-jitter-analysis.md). Running the
/// relay on its own dedicated OS thread already takes it off the shared tokio
/// runtime; this additionally elevates it to `SCHED_FIFO` so it preempts the
/// normal-priority worker pool the instant a captured chunk is ready. Priority
/// 40 sits below the AP2 sender's 50 and PipeWire's own RT threads. Without
/// `CAP_SYS_NICE` (e.g. a dev box) it logs and continues at normal priority —
/// exactly like the AP2 sender's `set_realtime_priority`.
#[cfg(target_os = "linux")]
fn set_relay_realtime_priority() {
    // SAFETY: sched_setscheduler on the current thread (pid 0) with a valid
    // sched_param; no aliasing, no ownership transfer.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 40;
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
            tracing::info!("sendspin relay: real-time priority set (SCHED_FIFO, priority 40)");
        } else {
            tracing::debug!("sendspin relay: could not set RT priority (need CAP_SYS_NICE); running at normal priority");
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn set_relay_realtime_priority() {}

/// Everything one running sendspin server owns. Dropping it tears down every
/// task/thread/background resource it started — the in-process equivalent of
/// `Supervisor::stop`.
///
/// It does **not** own the sink node it captures from: that's the shared sync
/// anchor created and destroyed by sync_group.rs, whose lifetime is independent
/// of this server (the server can be restarted — e.g. when the dialed-device set
/// changes — without disturbing the anchor or the RAOP outputs also fed from
/// it). Only the capture/discovery/advertise/accept resources are torn down here.
pub struct SendspinServerHandle {
    _advertisement: Advertisement,
    client_manager: ClientManager,
    _capture: crate::sendspin_capture::CaptureHandle,
    /// The live per-device groups (client_id → its single-member group on the
    /// shared timeline), shared with the accept loop, the event loop, the
    /// membership task and the relay. Held here so a *deliberate* teardown can
    /// send `stream/end` before the socket goes — see [`Self::shutdown`].
    groups: Arc<Mutex<HashMap<String, Group<SharesTimeline>>>>,
    /// client_id → device node name, so a caller who knows the device can find
    /// its group (the reverse of what the relay uses it for).
    client_to_node: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Discovery registry, to map an mDNS fullname back to a device node name in
    /// [`Self::stop_device`].
    devices: crate::sendspin_discovery::SharedSendspinDevices,
    /// `Option` so a deliberate shutdown can `take()` each task, abort it and
    /// **await** it — that await is what guarantees the listener's socket is
    /// closed (and its port free) before the caller binds a new server on it.
    accept_task: Option<JoinHandle<()>>,
    event_task: Option<JoinHandle<()>>,
    /// Arms/disarms `StreamPolicy::WhenAnnounced` devices around announcements;
    /// `None` under `StreamPolicy::Always` (members join on connect and stay).
    arm_task: Option<JoinHandle<()>>,
    /// The capture→wire relay runs on its own dedicated (RT-scheduled) OS
    /// thread, not a tokio task — see `set_relay_realtime_priority`. It stops
    /// when `_capture`'s Drop closes the PCM channel (its `blocking_recv`
    /// returns `None`), so it needs no explicit abort; the handle is held only
    /// to keep it named. Dropping it detaches — the thread exits on its own.
    _relay_thread: std::thread::JoinHandle<()>,
}

impl SendspinServerHandle {
    /// Re-assert which devices this server keeps connected, with their current URLs.
    ///
    /// Called on every reconcile pass: `supervise` is idempotent, so an unchanged URL
    /// costs nothing, while a device that re-resolved at a new address redirects its
    /// supervisor without restarting the server (a membership change is what restarts
    /// it). This is the replacement for the per-server mDNS browser that used to keep
    /// itself up to date — and that broke every other server's subscription.
    pub fn supervise(&self, members: &[(String, String)]) {
        for (fullname, url) in members {
            self.client_manager.supervise(fullname, url);
        }
    }

    /// Take one device out of the stream deliberately: `stream/end` first, so the
    /// player knows its stream is over and can idle cleanly, then `stop_client`,
    /// which closes the connection and ends its reconnect loop.
    ///
    /// This is what a member *leaving* a group costs now. Previously the only way to
    /// stop supervising one device was to drop the whole server — which yanked every
    /// other member's socket mid-stream as collateral.
    ///
    /// The `Disconnected` event that follows does the rest of the bookkeeping
    /// (`pending`/`ready`/`client_to_node` and the volume registration), so this only
    /// has to remove the group — before the await, so the relay stops fanning audio
    /// at it immediately.
    pub async fn stop_device(&self, fullname: &str) {
        let node_name = resolve_node_name(&self.devices, fullname);
        let client_id = self.client_to_node.lock().unwrap().iter().find(|(_, n)| **n == node_name).map(|(id, _)| id.clone());
        if let Some(client_id) = client_id {
            let group = self.groups.lock_recover().remove(&client_id);
            if let Some(group) = group {
                // `broadcast_stream_end`, not `end_stream`: this group SHARES the
                // server's timeline, so resetting the anchor would desync the members
                // that are staying.
                if tokio::time::timeout(GRACEFUL_END, group.broadcast_stream_end()).await.is_err() {
                    tracing::warn!("sendspin '{node_name}': stream/end did not reach it within {GRACEFUL_END:?}; closing anyway");
                }
            }
        }
        tracing::info!("sendspin '{node_name}': left the group — stream ended and supervision stopped");
        self.client_manager.stop_client(fullname);
    }

    /// Tear the whole server down deliberately, in the order the devices need:
    /// `stream/end` to everyone, then close every connection, then wait for the
    /// listener to actually be gone.
    ///
    /// Two reasons this exists rather than just dropping the handle:
    ///
    /// 1. **The devices are told.** `Drop` aborts the supervisor tasks, so each
    ///    player's socket dies *while it has an active stream* and it has to work that
    ///    out for itself — the suspected cause of the multi-second silence after a
    ///    group restart (docs/sendspin-group-churn-plan.md H6).
    /// 2. **The port is really free.** `abort()` only requests cancellation; the bound
    ///    listener lives inside the accept task's future until the runtime drops it.
    ///    Awaiting the aborted handle is what makes rebinding the same port on the
    ///    very next line safe (`SO_REUSEADDR` does not permit two live listeners).
    ///
    /// Residual: the close is a TCP close, not a WebSocket Close frame — the
    /// connection is owned by the supervisor task, which has no "close politely"
    /// directive. The `stream/end` is the part the player acts on.
    pub async fn shutdown(mut self) {
        // Take the groups out first: the relay fans audio to whatever is in this map,
        // so emptying it stops the stream before we announce that it ended.
        let groups: Vec<Group<SharesTimeline>> = std::mem::take(&mut *self.groups.lock_recover()).into_values().collect();
        let ending = async {
            for group in &groups {
                group.broadcast_stream_end().await;
            }
        };
        if tokio::time::timeout(GRACEFUL_END, ending).await.is_err() {
            tracing::warn!("sendspin server: stream/end did not reach every member within {GRACEFUL_END:?}; closing anyway");
        }
        for fullname in self.client_manager.supervised() {
            self.client_manager.stop_client(&fullname);
        }
        for task in [self.accept_task.take(), self.event_task.take(), self.arm_task.take()].into_iter().flatten() {
            task.abort();
            let _ = task.await;
        }
        // Dropping `self` here releases the capture, the advertisement and the
        // ClientManager (whose own Drop aborts any supervisor that outlived its Stop).
    }
}

impl Drop for SendspinServerHandle {
    /// The fallback path: everything stops, but nothing is announced to the devices
    /// and nothing is awaited. Prefer [`Self::shutdown`] wherever the teardown is
    /// deliberate.
    fn drop(&mut self) {
        for t in [&self.accept_task, &self.event_task, &self.arm_task].into_iter().flatten() {
            t.abort();
        }
        // `_capture`'s own Drop stops the capture thread, which drops the PCM
        // sender and closes the relay's channel → the relay thread exits.
        // `client_manager`'s Drop stops every reconnect loop; `_advertisement`'s
        // Drop unregisters mDNS. The sink node is the shared anchor owned by
        // sync_group.rs — not destroyed here.
    }
}

/// A device-reported (volume, muted) carried by an inbound client message, if
/// any. Devices report their own level/mute in a `client/state` player update —
/// the device→server half of volume/mute sync (a user turning the physical knob
/// or muting). Either field may be `None` (only some fields present); a non-state
/// message yields `None` for the whole thing.
fn reported_player_state(message: &Message) -> Option<(Option<u8>, Option<bool>)> {
    match message {
        Message::ClientState(state) => state.player.as_ref().map(|p| (p.volume, p.muted)),
        _ => None,
    }
}

/// Whether a message is the device's `client/state`, and the operational state it
/// reports (`Some(Synchronized)` = ready to play; `Some(ExternalSource)` = its output
/// is in use by something else and it will not participate; `None` = it reported no
/// top-level state).
///
/// Two things hang off this. The spec says a player reports itself available "only
/// after it has established clock synchronization", and — the part we were getting
/// wrong — **"The server MUST NOT send binary data to a client before that client has
/// sent its initial `client/state`."** A member added to its group the instant it
/// connects gets `stream/start` plus audio before it has said a word, and a client is
/// entitled to reject binary frames when it has no active stream; from then on the
/// stream is never established and the audio is silently discarded. So membership
/// waits for this message. (It also explains why announcements to an *idle* device
/// always worked: an idle sender only joins its group when a clip arrives — long after
/// the device settled — while a group server joined immediately.)
fn reported_client_state(message: &Message) -> Option<Option<ClientSyncState>> {
    match message {
        Message::ClientState(state) => Some(state.state),
        _ => None,
    }
}

/// The buffering a device asks for, from its `client/state` player object:
/// `(min_buffer_ms, required_lead_time_ms)`. `None` for a non-state message or one
/// that reports neither.
///
/// The spec makes these the server's obligation, not a hint we may ignore:
/// "servers must schedule timestamps so each player's queued audio duration stays at
/// or above its `min_buffer_ms`", and for a group "use a common send-ahead equal to
/// the maximum per-player send-ahead across grouped players … recompute when players
/// join, leave, or update their timing parameters". A player is explicitly allowed to
/// raise them for "codec init, decode warmup" — i.e. the value can differ per codec,
/// which is exactly the case a fixed group lead gets wrong.
fn reported_timing(message: &Message) -> Option<(Option<u32>, Option<u32>)> {
    match message {
        Message::ClientState(state) => {
            state.player.as_ref().map(|p| (p.min_buffer_ms, p.required_lead_time_ms)).filter(|(a, b)| a.is_some() || b.is_some())
        }
        _ => None,
    }
}

/// Apply a device-reported player state to the shared control (device→UI sync).
async fn apply_reported_state(
    control: &crate::sendspin_volume::SharedSendspinControl,
    node_name: &str,
    volume: Option<u8>,
    muted: Option<bool>,
) {
    if volume.is_none() && muted.is_none() {
        return;
    }
    let mut c = control.lock().await;
    if let Some(v) = volume {
        c.note_reported_volume(node_name, v);
    }
    if let Some(m) = muted {
        c.note_reported_mute(node_name, m);
    }
}

/// Per-interval relay counters for the compressed wire path (see the relay loop).
///
/// The two numbers that matter for a stutter report: is the timeline **continuous**
/// (every chunk exactly one block-duration after the last — a gap or a repeat is a
/// re-blocking bug) and does it stay **ahead of now** (the send-ahead lead; if that
/// decays toward zero we're producing slower than real time and the player starves).
#[derive(Default)]
struct RelayStats {
    blocks: u64,
    /// Chunks discarded because a device's write backlog was full, per device node
    /// name. A device that stops draining its socket gets nothing while every
    /// whole-group number stays perfect, so without this the log can show a healthy
    /// stream at the exact moment one speaker is receiving none of it — which is what
    /// "the group plays but that one speaker is silent" looks like from here.
    dropped_by_device: HashMap<String, u64>,
    /// Capture chunks that yielded more than one block (expected when the codec's
    /// block size doesn't divide the capture quantum — 20 ms Opus vs ~21.3 ms).
    bursts: u64,
    gap_min_us: i64,
    gap_max_us: i64,
    lead_min_us: i64,
    lead_max_us: i64,
    packet_max: usize,
    prev_ts: Option<i64>,
}

impl RelayStats {
    fn note_block(&mut self, ts: i64, now_us: i64, largest_packet: usize) {
        if let Some(prev) = self.prev_ts {
            let gap = ts - prev;
            if self.blocks == 1 {
                self.gap_min_us = gap;
                self.gap_max_us = gap;
            } else {
                self.gap_min_us = self.gap_min_us.min(gap);
                self.gap_max_us = self.gap_max_us.max(gap);
            }
        }
        let lead = ts - now_us;
        if self.blocks == 0 {
            self.lead_min_us = lead;
            self.lead_max_us = lead;
        } else {
            self.lead_min_us = self.lead_min_us.min(lead);
            self.lead_max_us = self.lead_max_us.max(lead);
        }
        self.prev_ts = Some(ts);
        self.blocks += 1;
        self.packet_max = self.packet_max.max(largest_packet);
    }

    /// Did any device lose audio this interval? Worth logging even for PCM, where
    /// the block stats below are otherwise skipped.
    fn any_dropped(&self) -> bool {
        !self.dropped_by_device.is_empty()
    }

    fn log(&self, server: &str, codec: &str, elapsed: Duration) {
        tracing::info!(
            "sendspin relay '{server}' [{codec}]: {} blocks in {:.1}s ({:.1}/s), ts gap {}..{} µs, lead {}..{} ms, largest packet {} B, {} multi-block chunk(s)",
            self.blocks,
            elapsed.as_secs_f64(),
            self.blocks as f64 / elapsed.as_secs_f64(),
            self.gap_min_us,
            self.gap_max_us,
            self.lead_min_us / 1000,
            self.lead_max_us / 1000,
            self.packet_max,
            self.bursts,
        );
        if self.any_dropped() {
            let mut per_device: Vec<String> = self.dropped_by_device.iter().map(|(node, n)| format!("{node} x{n}")).collect();
            per_device.sort();
            tracing::warn!(
                "sendspin relay '{server}': audio DISCARDED because the device's write backlog was full — {}. That device is not draining its socket, so it is receiving none of this stream while the numbers above stay healthy.",
                per_device.join(", ")
            );
        }
    }
}

/// Record a device's requested buffering on its registry entry, returning whether
/// that changed anything (the caller then nudges a reconcile, which recomputes the
/// group's send-ahead and restarts its server with the new value).
///
/// `client/state` repeats these values on every update, so only a *change* logs or
/// triggers work — this must never become per-message spam.
fn record_timing_request(
    node_name: &str,
    min_buffer_ms: Option<u32>,
    required_lead_ms: Option<u32>,
    send_ahead_us: i64,
    devices: &crate::sendspin_discovery::SharedSendspinDevices,
) -> bool {
    let mut changed = false;
    match devices.lock_recover().get_mut(node_name) {
        Some(dev) => {
            if dev.min_buffer_ms != min_buffer_ms || dev.required_lead_time_ms != required_lead_ms {
                dev.min_buffer_ms = min_buffer_ms;
                dev.required_lead_time_ms = required_lead_ms;
                changed = true;
            }
        }
        None => return false, // not discovered yet; it'll report again
    }
    if !changed {
        return false;
    }
    let lead_ms = send_ahead_us / 1000;
    if min_buffer_ms.is_some_and(|m| i64::from(m) > lead_ms) {
        // Not fatal: the reconcile this triggers raises the group's send-ahead to
        // cover it. Logged because it means the *configured* lead is below what the
        // hardware needs, which is worth seeing once.
        tracing::info!(
            "sendspin '{node_name}': needs min_buffer_ms={min_buffer_ms:?} (required_lead_time_ms={required_lead_ms:?}); our send-ahead is {lead_ms} ms and will be raised to cover it"
        );
    } else {
        tracing::info!(
            "sendspin '{node_name}': timing request min_buffer_ms={min_buffer_ms:?} required_lead_time_ms={required_lead_ms:?} (send-ahead {lead_ms} ms)"
        );
    }
    true
}

/// The send-ahead a sendspin stream must use: the user's configured lead, raised to
/// the largest per-member requirement.
///
/// Per the spec, each player's own send-ahead is its `min_buffer_ms` **plus its
/// `static_delay_ms`** ("Do not include `static_delay_ms` in these values; the server
/// applies `static_delay_ms` separately"), and a group uses "the maximum per-player
/// send-ahead across grouped players". `required_lead_time_ms` is deliberately NOT
/// folded in: the spec says to extend toward it "only when doing so adds no latency,
/// i.e. for buffered sources but not live streams", and this is a live stream.
///
/// `members` yields `(min_buffer_ms, static_delay_ms)` per member. The configured lead
/// stays a floor of its own, so a user who wants more headroom than the hardware asks
/// for still gets it.
pub fn required_send_ahead_us(configured_us: i64, codec: &str, members: impl IntoIterator<Item = (Option<u32>, u16)>) -> i64 {
    // A device that reports nothing still needs enough lead to decode a compressed
    // stream, so the codec's own floor applies to it — see `min_send_ahead_us`.
    let codec_floor = crate::sendspin_codec::min_send_ahead_us(codec);
    members
        .into_iter()
        .map(|(min_buffer_ms, static_delay_ms)| match min_buffer_ms {
            // Reported: authoritative, plus this member's static delay.
            Some(m) => (i64::from(m) + i64::from(static_delay_ms)) * 1000,
            // Not reported: our codec floor, still plus its static delay (the device
            // plays that much earlier, so we must send that much further ahead).
            None => codec_floor + i64::from(static_delay_ms) * 1000,
        })
        .fold(configured_us, i64::max)
}

/// Map a dialed mDNS fullname to the discovered device's virtual node name.
/// Prefers the exact discovery-registry entry (so it matches whatever
/// display-name rule discovery used); falls back to deriving from the mDNS
/// instance label when the device isn't in the registry yet.
fn resolve_node_name(devices: &crate::sendspin_discovery::SharedSendspinDevices, fullname: &str) -> String {
    if let Some(node_name) = devices.lock_recover().iter().find(|(_, d)| d.fullname == fullname).map(|(node_name, _)| node_name.clone()) {
        return node_name;
    }
    let label = fullname.split("._sendspin._tcp").next().unwrap_or(fullname);
    crate::sendspin_discovery::device_node_name(label)
}

/// Advertise a sendspin server on the process-wide shared, LAN-restricted mDNS
/// daemon ([`crate::discovery_supervisor::shared_advertise_daemon`]), falling
/// back to a private per-advertisement daemon if that's unavailable.
fn advertise(node_name: &str, display_name: &str, port: u16) -> Result<Advertisement, sendspin::error::Error> {
    match crate::discovery_supervisor::shared_advertise_daemon() {
        Some(daemon) => Advertisement::with_daemon(daemon, node_name, display_name, port, SENDSPIN_PATH),
        None => Advertisement::new(node_name, display_name, port, SENDSPIN_PATH),
    }
}

/// Like [`start_server`], but instead of one `Group` fanning identical frames to
/// all members, it gives **each device its own single-member `Group`**, and all
/// those groups **share one [`SharedTimeline`]**. One capture (the single PCM
/// source) drives them: each chunk is stamped **once** on the shared timeline and
/// delivered to every group via `push_at`, so chunk-N carries an identical
/// timestamp to every device — the O-B per-device-sender model (each device is
/// independently addressable, e.g. for per-device duck/overlay, while staying
/// sample-accurately coincident). This is spike S1's subject; see
/// docs/spike-results-and-status.md.
///
/// Unlike a per-device *null-sink* (one sink per device — the S3 spike, which
/// showed dropouts because the null-sink isn't a steady clock driver), here the
/// senders are fed from one steady anchor monitor, which is the recommended shape
/// for a synchronized music group.
#[allow(clippy::too_many_arguments)]
pub async fn start_server_per_device(
    server_name: &str,
    display_name: &str,
    port: u16,
    sink_node_id: u32,
    // The devices this server keeps connected: `(mDNS fullname, ws URL)` from the
    // discovery registry. Handed in rather than discovered here — see the module docs:
    // one browse per daemon, because mdns-sd's per-type listener is single-slot and a
    // second browse steals it.
    members: Vec<(String, String)>,
    send_ahead_us: i64,
    control: crate::sendspin_volume::SharedSendspinControl,
    devices: crate::sendspin_discovery::SharedSendspinDevices,
    policy: StreamPolicy,
    codec: &str,
) -> anyhow::Result<SendspinServerHandle> {
    let node_name = server_name.to_string();

    let (capture_handle, mut pcm_rx) =
        crate::sendspin_capture::spawn(sink_node_id).map_err(|e| anyhow::anyhow!("failed to start capture for '{node_name}': {e}"))?;

    // One clock shared by the timeline and the dial manager, so the timestamps
    // stamped here are in the same domain as the `server/time` replies members
    // trust. (On Linux even distinct DefaultClocks share CLOCK_MONOTONIC_RAW,
    // but sharing the Arc is the portable guarantee.)
    let clock: Arc<dyn Clock> = Arc::new(DefaultClock::default());

    // One `ServerRole` describes this server's identity, clock and — the part that
    // matters for an idle sender — the `connection_reason` its dials announce. Both
    // halves of the server (the inbound listener and the outbound dial manager) are
    // built from it, so they can't disagree.
    let role = sendspin::server::ServerRole::new(node_name.clone(), display_name)
        .clock(Arc::clone(&clock))
        .connection_reason(policy.connection_reason());
    let listener = role
        .bind(("0.0.0.0", port))
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind sendspin server on port {port}: {e}"))?
        .path(SENDSPIN_PATH);
    let advertisement =
        advertise(&node_name, display_name, port).map_err(|e| anyhow::anyhow!("failed to advertise sendspin server '{node_name}': {e}"))?;

    // The single shared timeline. Config is set once up front so every member
    // gets `stream/start` when it joins and the timeline is never re-anchored
    // mid-stream (a per-member re-anchor would desync the others).
    let timeline = Arc::new(SharedTimeline::new(Arc::clone(&clock)).with_send_ahead_us(send_ahead_us));
    // One config for the whole stream (every member of this server's timeline), so
    // the codec was resolved across all its devices by the caller — see
    // `resolve_codec`. `codec_header` stays None for PCM; a compressed codec that
    // needs one (e.g. FLAC's STREAMINFO) must set it when the encoder lands.
    timeline.set_config(StreamPlayerConfig {
        codec: codec.to_string(),
        // FLAC needs its stream header up front (base64); PCM/Opus carry their
        // parameters in the format fields below, which the client turns into a
        // synthetic header itself. Set before any member joins, so every
        // `stream/start` carries it.
        sample_rate: crate::sendspin_capture::SAMPLE_RATE,
        channels: crate::sendspin_capture::CHANNELS as u8,
        bit_depth: 16,
        codec_header: crate::sendspin_codec::codec_header_base64(codec),
    });

    // One single-member Group per device, all sharing `timeline`. Keyed by the
    // opaque client_id; `client_to_node` maps that to the device's output node
    // name so the capture loop can look up its overlay (announcement) state.
    let groups: Arc<Mutex<HashMap<String, Group<SharesTimeline>>>> = Arc::new(Mutex::new(HashMap::new()));
    let client_to_node: Arc<std::sync::Mutex<HashMap<String, String>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    // Devices that are connected but NOT (yet) in a group, by client_id → (node name,
    // its sender, when it connected). Under `WhenAnnounced` a device waits here until a
    // clip needs it; under `Always` it waits only until it has sent its initial
    // `client/state` (the spec's MUST — see `reported_client_state`), which the
    // membership task below checks. `ready` is the set that have sent it.
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let ready: SharedReady = Arc::new(Mutex::new(std::collections::HashSet::new()));

    let accept_task = spawn_accept_loop_per_device(
        listener,
        Arc::clone(&groups),
        Arc::clone(&client_to_node),
        Arc::clone(&timeline),
        control.clone(),
        policy,
        Arc::clone(&pending),
        Arc::clone(&ready),
        codec.to_string(),
        devices.clone(),
        send_ahead_us,
    );

    // No browser of our own: the daemon's single `sendspin_discovery` browse owns the
    // `_sendspin._tcp` subscription, and we drive the dial loop from the URLs it
    // resolved. A browser per server is what silently un-subscribed every *other*
    // server — and the registry itself — because mdns-sd keeps one listener per
    // service type and the newest browse overwrites it.
    let (client_manager, mut events) = sendspin::server::ClientManager::start_without_discovery(&role);
    for (fullname, url) in &members {
        client_manager.supervise(fullname, url);
    }
    tracing::info!(
        "sendspin server '{node_name}': supervising {} device(s) from the discovery registry",
        members.len()
    );

    let event_task = {
        let groups = Arc::clone(&groups);
        let client_to_node = Arc::clone(&client_to_node);
        let control = control.clone();
        let devices = devices.clone();
        let pending_conn = Arc::clone(&pending);
        let ready_conn = Arc::clone(&ready);
        let groups_conn = Arc::clone(&groups);
        let codec_for_report = codec.to_string();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    ClientEvent::Connected { client_id, fullname, hello, sender, .. } => {
                        let dev_node_name = resolve_node_name(&devices, &fullname);
                        tracing::info!("sendspin per-device: '{dev_node_name}' connected (client {client_id}, policy {policy:?})");
                        // Check the device really decodes what we're about to send, and
                        // log what it offers — the only way to know whether a
                        // compressed codec is even an option on this hardware.
                        if report_format_support(&dev_node_name, &hello, &codec_for_report, &devices) {
                            // First sight of what this device decodes → let the
                            // reconciler re-resolve its group's wire codec.
                            control.lock().await.notify_reconcile();
                        }
                        client_to_node.lock().unwrap().insert(client_id.clone(), dev_node_name.clone());
                        // Register under the lock, push stored volume/mute/delay after
                        // releasing it: this loop is serial, so awaiting a stalled
                        // device's socket here would stall every other device's events.
                        let pending = control.lock().await.register(dev_node_name.clone(), sender.clone());
                        pending.apply().await;
                        ready_conn.lock_recover().remove(&client_id);
                        groups_conn.lock_recover().remove(&client_id);
                        pending_conn
                            .lock_recover()
                            .insert(client_id, (dev_node_name, sender, std::time::Instant::now()));
                    }
                    ClientEvent::Message { client_id, message } => {
                        // Device→UI volume/mute sync: a device reporting its own
                        // (physically-changed) level/mute updates the stored state
                        // so the UI reflects it. Resolve client_id→node first, then
                        // drop the std guard before the async control lock.
                        if let Some((volume, muted)) = reported_player_state(&message) {
                            let dev_node = client_to_node.lock().unwrap().get(&client_id).cloned();
                            if let Some(dev_node) = dev_node {
                                apply_reported_state(&control, &dev_node, volume, muted).await;
                            }
                        }
                        if let Some(sync_state) = reported_client_state(&message) {
                            let dev_node = client_to_node.lock().unwrap().get(&client_id).cloned();
                            let node = dev_node.unwrap_or_else(|| client_id.clone());
                            if ready_conn.lock_recover().insert(client_id.clone()) {
                                tracing::info!(
                                    "sendspin '{node}': reported client/state ({}) — it may now be streamed to",
                                    match sync_state {
                                        Some(ClientSyncState::Synchronized) => "synchronized",
                                        Some(ClientSyncState::ExternalSource) => "external source — its output is in use elsewhere",
                                        None => "no operational state given",
                                    }
                                );
                            }
                        }
                        if let Some((min_buffer_ms, required_lead_ms)) = reported_timing(&message) {
                            let dev_node = client_to_node.lock().unwrap().get(&client_id).cloned();
                            if let Some(dev_node) = dev_node {
                                if record_timing_request(&dev_node, min_buffer_ms, required_lead_ms, send_ahead_us, &devices) {
                                    // The group's send-ahead is derived from these, so a
                                    // change has to reach the reconciler.
                                    control.lock().await.notify_reconcile();
                                }
                            }
                        }
                    }
                    ClientEvent::Disconnected { client_id } => {
                        pending_conn.lock_recover().remove(&client_id);
                        ready_conn.lock_recover().remove(&client_id);
                        // Bind the removal to a statement so the (non-Send) std
                        // MutexGuard drops before the await below.
                        let removed = client_to_node.lock().unwrap().remove(&client_id);
                        if let Some(ref dev_node_name) = removed {
                            tracing::info!(
                                "sendspin per-device: '{dev_node_name}' disconnected (client {client_id}) — awaiting ClientManager re-dial"
                            );
                            control.lock().await.unregister(dev_node_name);
                        } else {
                            tracing::info!("sendspin per-device: client {client_id} disconnected (unmapped)");
                        }
                        groups.lock_recover().remove(&client_id);
                    }
                }
            }
        })
    };

    // The capture→wire relay: stamp each captured chunk once on the shared
    // timeline and fan it out to every device's group. This is the timing-
    // critical path — it runs on a DEDICATED, RT-scheduled OS thread rather
    // than a tokio task so that general-purpose async work can never preempt it
    // (RC1). Everything it calls is synchronous and non-blocking:
    // `timeline.stamp`, the (std-`Mutex`) `groups`/`client_to_node` locks, the
    // overlay `mix`, and `Group::push_at` (which enqueues to each member's
    // writer task without awaiting). `blocking_recv` drains the capture channel
    // and returns `None` when capture stops, ending the thread.
    let relay_thread = {
        let groups = Arc::clone(&groups);
        let client_to_node = Arc::clone(&client_to_node);
        let timeline = Arc::clone(&timeline);
        let relay_codec = codec.to_string();
        let relay_node_name = node_name.clone();
        let clock_relay = Arc::clone(&clock);
        std::thread::Builder::new()
            .name("sendspin-relay".into())
            .spawn(move || {
                set_relay_realtime_priority();
                let mixer = crate::overlay_mixer::OverlayMixer::global();
                // Reused across chunks AND across devices within a chunk so the
                // per-device overlay mix allocates at most once (only relevant
                // while an announcement is overlaying; the plain-music path never
                // touches it). push_at copies into its own wire frame
                // synchronously, so one buffer is safe to reuse for every device.
                let mut mix_buf: Vec<u8> = Vec::new();
                // Wire codec (sendspin_codec.rs). PCM is a passthrough; a compressed
                // codec needs fixed-size blocks, so captured quanta are re-cut here —
                // once for the whole group, since one stream carries one format, which
                // is what keeps the members sample-coincident.
                let block_frames = crate::sendspin_codec::Encoder::block_frames(&relay_codec);
                let mut blocker = crate::sendspin_codec::Reblocker::new(block_frames);
                // A predictive codec's decoded output lags its input (Opus lookahead),
                // and sendspin has no pre-skip field to declare that — so shift our
                // timestamps back by it, or every chunk is heard that much after the
                // instant it asked for. Zero for PCM/FLAC. Queried once here, not per
                // block: it's a property of the codec + format, not of a device.
                let codec_delay_us = crate::sendspin_codec::codec_delay_us(&relay_codec);
                if codec_delay_us != 0 {
                    tracing::info!("sendspin relay: compensating {relay_codec} encoder delay of {codec_delay_us} µs");
                }
                // One encoder per member: Opus/FLAC are predictive, and a device being
                // announced to gets different audio from its groupmates, so a shared
                // encoder would put a discontinuity in everyone's stream. Created
                // lazily (a new member is a new stream) and pruned with membership.
                let mut encoders: HashMap<String, crate::sendspin_codec::Encoder> = HashMap::new();
                // Rate-limited stats for the compressed path, so a "codec X stutters"
                // report is answerable from the log instead of a deploy cycle: does the
                // timeline stay continuous and ahead of now, and how big are the
                // packets? Only for a compressed codec (PCM is the unchanged path) and
                // only every STATS_INTERVAL, so this is never per-chunk log spam.
                const STATS_INTERVAL: Duration = Duration::from_secs(10);
                let mut stats = RelayStats::default();
                let mut stats_since = std::time::Instant::now();
                while let Some(pcm) = pcm_rx.blocking_recv() {
                    blocker.push(&pcm);
                    let mut emitted_this_chunk = 0usize;
                    while let Some(len) = blocker.ready() {
                        // Stamp ONCE per emitted block, then fan that identical ts to
                        // every device's group — the shared-timeline sync guarantee. A
                        // device with an active announcement overlay gets
                        // duck(music)+overlay instead of the plain block; its
                        // groupmates get plain music. The locks are held only for this
                        // brief synchronous fan-out (M5) and drop each iteration.
                        let ts = timeline.stamp(len) - codec_delay_us;
                        let block = blocker.block(len);
                        let mut largest_packet = 0usize;
                        let groups = groups.lock_recover();
                        let c2n = client_to_node.lock().unwrap();
                        for (client_id, group) in groups.iter() {
                            let overlaid = c2n.get(client_id).is_some_and(|node| mixer.mix_into(node, block, &mut mix_buf));
                            let src: &[u8] = if overlaid { &mix_buf } else { block };
                            let encoder = encoders.entry(client_id.clone()).or_insert_with(|| {
                                crate::sendspin_codec::Encoder::new(&relay_codec).unwrap_or(crate::sendspin_codec::Encoder::Pcm)
                            });
                            // A failed encode drops just this chunk for this device —
                            // same as a full backlog — rather than sending garbage.
                            if let Some(payload) = encoder.encode(src) {
                                largest_packet = largest_packet.max(payload.len());
                                if group.push_at(ts, payload).dropped > 0 {
                                    let node = c2n.get(client_id).cloned().unwrap_or_else(|| client_id.clone());
                                    *stats.dropped_by_device.entry(node).or_default() += 1;
                                }
                            }
                        }
                        // Prune encoders whose member left, so a long-lived server
                        // doesn't accumulate codec state (cheap, and only when the
                        // membership actually changed).
                        if encoders.len() != groups.len() {
                            encoders.retain(|id, _| groups.contains_key(id));
                        }
                        drop(c2n);
                        drop(groups);
                        blocker.consume(len);
                        emitted_this_chunk += 1;
                        stats.note_block(ts, clock_relay.now_micros(), largest_packet);
                    }
                    if emitted_this_chunk > 1 {
                        stats.bursts += 1;
                    }
                    if (block_frames.is_some() || stats.any_dropped()) && stats_since.elapsed() >= STATS_INTERVAL {
                        stats.log(&relay_node_name, &relay_codec, stats_since.elapsed());
                        stats = RelayStats::default();
                        stats_since = std::time::Instant::now();
                    }
                    // Finished overlays are drained by the AnnounceCoordinator's
                    // poll loop (main.rs), not here.
                }
                tracing::debug!("sendspin relay thread exiting");
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn sendspin relay thread for '{node_name}': {e}"))?
    };

    // Arm/disarm idle devices around announcements (WhenAnnounced only).
    // Decides which connected devices are in a group (→ stream/start + audio).
    let arm_task = Some(spawn_membership_task(
        policy,
        Arc::clone(&pending),
        Arc::clone(&ready),
        Arc::clone(&groups),
        Arc::clone(&timeline),
    ));

    Ok(SendspinServerHandle {
        _advertisement: advertisement,
        client_manager,
        _capture: capture_handle,
        groups: Arc::clone(&groups),
        client_to_node: Arc::clone(&client_to_node),
        devices: devices.clone(),
        accept_task: Some(accept_task),
        event_task: Some(event_task),
        arm_task,
        _relay_thread: relay_thread,
    })
}

/// Log a connected device's advertised formats and warn if our wire format isn't
/// among them.
///
/// The protocol negotiates: a client lists the discrete `{codec, channels,
/// sample_rate, bit_depth}` combinations it can decode. This server streams one
/// fixed format (the capture format — PCM 48 kHz/16-bit/stereo, chosen so the whole
/// path is resample-free), so there is nothing to pick yet; what there *is* to do is
/// stop assuming. A device that doesn't list it would be receiving audio it can't
/// decode, which is worth a loud line rather than silent silence. The logged list is
/// also the answer to "could we use a compressed codec here?" — it says exactly what
/// this hardware would accept (`PlayerV1Support::pick_format` is the crate-side
/// helper a multi-codec path would use).
fn report_format_support(
    node_name: &str,
    hello: &ClientHello,
    codec: &str,
    devices: &crate::sendspin_discovery::SharedSendspinDevices,
) -> bool {
    let wire = wire_format_for(codec);
    let Some(support) = &hello.player_v1_support else {
        tracing::warn!("sendspin '{node_name}': no player@v1 capabilities in its client/hello; streaming our default format blind");
        return false;
    };
    // Codecs usable *at our rate/depth* — a codec the device only decodes at 44.1k
    // is no use to us. Stored on the device so the API can offer exactly these in
    // the per-output codec picker.
    let usable: Vec<String> = OFFERED_CODECS.iter().filter(|c| support.supports(&wire_format_for(c))).map(|c| c.to_string()).collect();
    let offered: Vec<String> =
        support.supported_formats.iter().map(|f| format!("{}/{}Hz/{}bit/{}ch", f.codec, f.sample_rate, f.bit_depth, f.channels)).collect();
    if support.supports(&wire) {
        tracing::info!(
            "sendspin '{node_name}': streaming {}/{}Hz/{}bit/{}ch; it also accepts [{}] at our format (full list: {})",
            wire.codec,
            wire.sample_rate,
            wire.bit_depth,
            wire.channels,
            usable.join(", "),
            offered.join(", ")
        );
    } else {
        tracing::warn!(
            "sendspin '{node_name}': does NOT advertise our {}/{}Hz/{}bit/{}ch stream — it may not decode what we send (offers: {})",
            wire.codec,
            wire.sample_rate,
            wire.bit_depth,
            wire.channels,
            offered.join(", ")
        );
    }
    // Record on the device so the API's codec picker offers exactly these, and report
    // whether that's new information (the caller then nudges a reconcile so the group
    // can upgrade off its conservative PCM guess).
    //
    // Keyed by `node_name` — the registry key the caller already resolved from the
    // stable mDNS identity. Do NOT match on `hello.name`: a client's self-reported
    // name is not the discovery display name (a Voice PE says "Home Assistant Voice
    // 093ca8" where mDNS advertises "home-assistant-voice-093ca8"), so a name compare
    // silently matched nothing and every device stayed "codecs not known yet" even
    // though it had just told us it decodes Opus and FLAC.
    let mut changed = false;
    match devices.lock_recover().get_mut(node_name) {
        Some(dev) if dev.supported_codecs != usable => {
            dev.supported_codecs = usable;
            changed = true;
        }
        Some(_) => {}
        // A device that dialed in before mDNS resolved it isn't in the registry yet;
        // its capabilities land on the next connect. Not silent, because "nothing was
        // recorded" is exactly the failure this comment exists for.
        None => tracing::debug!("sendspin '{node_name}': not in the discovery registry yet; codec capabilities not recorded"),
    }
    changed
}

/// The wire format this server streams in `codec`, at the capture rate/depth so
/// nothing resamples.
fn wire_format_for(codec: &str) -> AudioFormatSpec {
    AudioFormatSpec {
        codec: codec.to_string(),
        channels: crate::sendspin_capture::CHANNELS as u8,
        sample_rate: crate::sendspin_capture::SAMPLE_RATE,
        bit_depth: 16,
    }
}

/// Codecs this daemon can **encode** right now, best-first — see sendspin_codec.rs
/// (Opus via vendored libopus, FLAC via pure-Rust flacenc, PCM passthrough). The spec
/// requires a server to support all three, and this is what the UI's codec picker
/// greys out against.
pub const ENCODABLE_CODECS: &[&str] = &["opus", "flac", "pcm"];

/// Every codec the UI offers, best-first — the union of what the protocol names
/// and what a device might support. Availability per output is computed from
/// [`ENCODABLE_CODECS`] ∩ the device's advertised codecs.
pub const OFFERED_CODECS: &[&str] = &["opus", "flac", "pcm"];

/// Preference order for [`crate::sync_settings::SendspinCodec::Auto`]: Opus when
/// everything in the way supports it (≈10× less WiFi airtime than PCM), else PCM.
/// FLAC is deliberately not auto-selected — it's a deliberate lossless choice, and
/// picking it silently over Opus would trade a lot of bandwidth for inaudibility.
const AUTO_CODEC_PREFERENCE: &[&str] = &["opus", "pcm"];

/// Can this daemon encode `codec`?
pub fn can_encode(codec: &str) -> bool {
    ENCODABLE_CODECS.iter().any(|c| c.eq_ignore_ascii_case(codec))
}

/// Is `codec` usable for `device_codecs` (what the device advertised at our wire
/// format)? An empty list means "we haven't connected yet, so we don't know" — only
/// PCM is assumed then, since it's the one format every player must handle.
pub fn device_supports(device_codecs: &[String], codec: &str) -> bool {
    if device_codecs.is_empty() {
        return codec.eq_ignore_ascii_case("pcm");
    }
    device_codecs.iter().any(|c| c.eq_ignore_ascii_case(codec))
}

/// The codec a stream should actually use, given the user's per-output `mode` and
/// what each member device advertised. `device_codecs` is one entry per member (a
/// group's stream carries ONE format, so a codec has to work for all of them).
///
/// Falls back to PCM whenever the choice isn't usable — an explicitly-picked codec
/// we can't encode, or one a member can't decode — because a stream nothing can
/// decode is worse than a lossless one nobody asked for.
///
/// **A member whose capabilities we don't know yet (an empty list) is skipped, not
/// treated as PCM-only.** Capabilities are learned from the device's own
/// `client/hello`, so every device is "unknown" until it has connected once —
/// and counting unknown as PCM-only meant routing one more speaker into a live Opus
/// group dropped the *whole group* to PCM, restarted it, then restarted it again the
/// moment the newcomer said it decodes Opus (measured: two restarts 914 ms apart, see
/// docs/sendspin-group-churn-plan.md §2b). Assuming the group's codec instead costs at
/// most one restart, and only for hardware that really can't decode it:
/// `report_format_support` nudges a reconcile as soon as the device's `client/hello`
/// proves otherwise. Per-device *display* of what a codec is worth stays honest —
/// that's [`device_supports`], which still answers "unknown ⇒ only PCM is assured".
pub fn resolve_codec<'a>(
    mode: crate::sync_settings::SendspinCodec,
    device_codecs: impl IntoIterator<Item = &'a Vec<String>> + Clone,
) -> &'static str {
    let usable =
        |codec: &str| can_encode(codec) && device_codecs.clone().into_iter().filter(|d| !d.is_empty()).all(|d| device_supports(d, codec));
    match mode.explicit_codec() {
        Some(codec) => OFFERED_CODECS.iter().copied().find(|c| *c == codec && usable(c)).unwrap_or("pcm"),
        None => AUTO_CODEC_PREFERENCE.iter().copied().find(|c| usable(c)).unwrap_or("pcm"),
    }
}

/// Decides, continuously, which connected devices are **in** their group — which is
/// what sends `stream/start` and starts audio flowing to them.
///
/// Two rules, and the first applies to every policy:
///
/// 1. **Never before the device has spoken.** The spec: "The server MUST NOT send
///    binary data to a client before that client has sent its initial `client/state`."
///    Joining on connect raced `stream/start` + audio ahead of that message, and a
///    client may reject binary frames while it has no active stream — after which
///    nothing we send is ever played, though our side looks perfectly healthy.
///    [`READY_GRACE`] bounds the wait so firmware that never reports state still gets
///    audio (the previous behaviour) instead of going permanently silent.
/// 2. **Under [`StreamPolicy::WhenAnnounced`], only while a clip needs it** — an idle
///    device stays connected but carries no audio, and leaves its group again after
///    [`ANNOUNCE_DRAIN`] so it can return to WiFi power-save.
///
/// Polling rather than event-driven: the trigger state lives in process-global
/// singletons the RT relay also reads, and an [`ARM_POLL_INTERVAL`] tick is cheap and
/// imperceptible next to the send-ahead lead. A clip is never consumed while its device
/// is un-armed (`mix_into` only runs for group members), so nothing of it is lost.
fn spawn_membership_task(
    policy: StreamPolicy,
    pending: SharedPending,
    ready: SharedReady,
    groups: Arc<Mutex<HashMap<String, Group<SharesTimeline>>>>,
    timeline: Arc<SharedTimeline>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // client_id → when its announcement stopped, for the drain delay.
        let mut finished_at: HashMap<String, std::time::Instant> = HashMap::new();
        let mut ticker = tokio::time::interval(ARM_POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let mixer = crate::overlay_mixer::OverlayMixer::global();
            let in_flight = crate::announce::AnnounceCoordinator::global().outputs_in_flight();
            let wanted = |node: &str| match policy {
                StreamPolicy::Always => true,
                StreamPolicy::WhenAnnounced => mixer.is_active(node) || in_flight.contains(node),
            };

            // Promote: connected, has spoken (or the grace expired), and wanted.
            let candidates: Vec<(String, String, ServerSender, bool)> = {
                let ready = ready.lock_recover();
                pending
                    .lock_recover()
                    .iter()
                    .map(|(id, (node, tx, since))| {
                        (id.clone(), node.clone(), tx.clone(), ready.contains(id) || since.elapsed() >= READY_GRACE)
                    })
                    .collect()
            };
            for (client_id, node, sender, may_stream) in candidates {
                if !may_stream || !wanted(&node) || groups.lock_recover().contains_key(&client_id) {
                    continue;
                }
                let spoke = ready.lock_recover().contains(&client_id);
                tracing::info!(
                    "sendspin '{node}': starting its stream{}",
                    if spoke { "" } else { " (no client/state within the grace — streaming anyway)" }
                );
                let group = Group::with_timeline(Arc::clone(&timeline));
                if let Err(e) = group.add_member(client_id.clone(), sender).await {
                    tracing::warn!("sendspin '{node}': failed to start its stream: {e}");
                    continue;
                }
                finished_at.remove(&client_id);
                groups.lock_recover().insert(client_id, group);
            }

            // Demote (idle path only): the clip finished, so hand the device back after
            // the drain — audio already queued on it must still render.
            if policy == StreamPolicy::WhenAnnounced {
                let armed: Vec<(String, String)> = {
                    let pending = pending.lock_recover();
                    groups
                        .lock_recover()
                        .keys()
                        .filter_map(|id| pending.get(id).map(|(node, _, _)| (id.clone(), node.clone())))
                        .collect()
                };
                for (client_id, node) in armed {
                    if wanted(&node) {
                        finished_at.remove(&client_id);
                        continue;
                    }
                    let since = *finished_at.entry(client_id.clone()).or_insert_with(std::time::Instant::now);
                    if since.elapsed() < ANNOUNCE_DRAIN {
                        continue;
                    }
                    let group = groups.lock_recover().remove(&client_id);
                    if let Some(group) = group {
                        tracing::info!("sendspin '{node}': announcement done — ending its stream (idle, no audio)");
                        // `broadcast_stream_end`, not `end_stream`: this group SHARES the
                        // server's timeline, so it must only send the message — resetting
                        // the shared anchor would disturb groups still streaming off it.
                        group.broadcast_stream_end().await;
                    }
                    finished_at.remove(&client_id);
                }
            }
        }
    })
}

/// Accept loop for the per-device topology: each inbound client gets its own
/// single-member group on the shared timeline (mirrors `spawn_accept_loop`).
#[allow(clippy::too_many_arguments)]
fn spawn_accept_loop_per_device(
    listener: ServerListener,
    groups: Arc<Mutex<HashMap<String, Group<SharesTimeline>>>>,
    client_to_node: Arc<Mutex<HashMap<String, String>>>,
    // Group membership (and therefore `stream/start`) is decided by the membership
    // task, not here — an accepted connection is only parked.
    _timeline: Arc<SharedTimeline>,
    control: crate::sendspin_volume::SharedSendspinControl,
    _policy: StreamPolicy,
    pending_members: SharedPending,
    ready_members: SharedReady,
    codec_accept: String,
    devices_accept: crate::sendspin_discovery::SharedSendspinDevices,
    send_ahead_us: i64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((conn, _addr)) => {
                    let client_id = conn.client_id().to_string();
                    let node_name = crate::sendspin_discovery::device_node_name(&conn.hello().name);
                    let sender = conn.sender();
                    if report_format_support(&node_name, conn.hello(), &codec_accept, &devices_accept) {
                        control.lock().await.notify_reconcile();
                    }
                    // Lock released before the push — a stalled joiner must not
                    // block accepting the next client.
                    let pending = control.lock().await.register(node_name.clone(), sender.clone());
                    pending.apply().await;
                    client_to_node.lock().unwrap().insert(client_id.clone(), node_name.clone());
                    // Same rule as a dialed device: park it and let the membership task
                    // decide. An idle server must not start streaming just because the
                    // device dialed *us*, and no server may send binary data before the
                    // device's initial `client/state`.
                    ready_members.lock_recover().remove(&client_id);
                    groups.lock_recover().remove(&client_id);
                    pending_members.lock_recover().insert(
                        client_id.clone(),
                        (node_name.clone(), sender, std::time::Instant::now()),
                    );
                    tokio::spawn(drain_messages_per_device(
                        conn,
                        client_id,
                        node_name,
                        Arc::clone(&groups),
                        Arc::clone(&client_to_node),
                        control.clone(),
                        Arc::clone(&pending_members),
                        Arc::clone(&ready_members),
                        send_ahead_us,
                        devices_accept.clone(),
                    ));
                }
                Err(e) => {
                    tracing::warn!("sendspin accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    })
}

async fn drain_messages_per_device(
    mut conn: ServerConnection,
    client_id: String,
    node_name: String,
    groups: Arc<Mutex<HashMap<String, Group<SharesTimeline>>>>,
    client_to_node: Arc<Mutex<HashMap<String, String>>>,
    control: crate::sendspin_volume::SharedSendspinControl,
    pending_members: SharedPending,
    ready_members: SharedReady,
    send_ahead_us: i64,
    devices_drain: crate::sendspin_discovery::SharedSendspinDevices,
) {
    while let Some(message) = conn.recv_message().await {
        // Device→UI volume/mute sync (same as the dial-out path): reflect a
        // physically-changed volume/mute the device reports back into the UI.
        if let Some((volume, muted)) = reported_player_state(&message) {
            apply_reported_state(&control, &node_name, volume, muted).await;
        }
        if reported_client_state(&message).is_some() && ready_members.lock_recover().insert(client_id.clone()) {
            tracing::info!("sendspin '{node_name}': reported client/state — it may now be streamed to");
        }
        if let Some((min_buffer_ms, required_lead_ms)) = reported_timing(&message) {
            if record_timing_request(&node_name, min_buffer_ms, required_lead_ms, send_ahead_us, &devices_drain) {
                control.lock().await.notify_reconcile();
            }
        }
    }
    control.lock().await.unregister(&node_name);
    client_to_node.lock().unwrap().remove(&client_id);
    groups.lock_recover().remove(&client_id);
    pending_members.lock_recover().remove(&client_id);
    ready_members.lock_recover().remove(&client_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_settings::SendspinCodec;

    fn codecs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn auto_prefers_opus_when_the_device_decodes_it() {
        // All three are encodable now (sendspin_codec.rs), as the spec requires.
        assert!(can_encode("opus") && can_encode("flac") && can_encode("pcm"));
        let full = codecs(&["pcm", "flac", "opus"]);
        assert_eq!(resolve_codec(SendspinCodec::Auto, std::iter::once(&full)), "opus");
        // Auto never picks FLAC — lossless is a deliberate choice, not a default.
        let flac_only = codecs(&["pcm", "flac"]);
        assert_eq!(resolve_codec(SendspinCodec::Auto, std::iter::once(&flac_only)), "pcm");
        let pcm_only = codecs(&["pcm"]);
        assert_eq!(resolve_codec(SendspinCodec::Auto, std::iter::once(&pcm_only)), "pcm");
    }

    #[test]
    fn an_explicit_pick_is_honoured_and_falls_back_to_pcm_when_unusable() {
        let device = codecs(&["pcm", "opus"]);
        assert_eq!(resolve_codec(SendspinCodec::Opus, std::iter::once(&device)), "opus");
        assert_eq!(resolve_codec(SendspinCodec::Pcm, std::iter::once(&device)), "pcm");
        // The device doesn't advertise FLAC ⇒ PCM, never a stream it can't decode.
        assert_eq!(resolve_codec(SendspinCodec::Flac, std::iter::once(&device)), "pcm");
        let flac_capable = codecs(&["pcm", "flac"]);
        assert_eq!(resolve_codec(SendspinCodec::Flac, std::iter::once(&flac_capable)), "flac");
    }

    #[test]
    fn a_codec_must_work_for_every_member_of_the_group() {
        // One stream serves the whole group, so a member that lacks the codec vetoes
        // it — this is what stops a mixed group getting undecodable audio.
        let members = [codecs(&["pcm", "opus"]), codecs(&["pcm"])];
        assert_eq!(resolve_codec(SendspinCodec::Auto, members.iter()), "pcm");
        assert!(!device_supports(&members[1], "opus"));
        // Both capable ⇒ the group gets the compressed stream.
        let both = [codecs(&["pcm", "opus"]), codecs(&["pcm", "opus", "flac"])];
        assert_eq!(resolve_codec(SendspinCodec::Auto, both.iter()), "opus");
    }

    /// A `client/hello` shaped like a real Voice PE's: its self-reported `name` is
    /// the friendly one, which is NOT the mDNS/discovery display name.
    fn voice_pe_hello(formats: &[(&str, u8)]) -> ClientHello {
        use sendspin::protocol::messages::{AudioFormatSpec, PlayerV1Support};
        ClientHello {
            client_id: "20:F8:3B:09:3C:A8".into(),
            name: "Home Assistant Voice 093ca8".into(),
            version: 1,
            supported_roles: vec!["player@v1".into()],
            device_info: None,
            player_v1_support: Some(PlayerV1Support {
                supported_formats: formats
                    .iter()
                    .map(|(codec, ch)| AudioFormatSpec {
                        codec: (*codec).to_string(),
                        channels: *ch,
                        sample_rate: crate::sendspin_capture::SAMPLE_RATE,
                        bit_depth: 16,
                    })
                    .collect(),
                buffer_capacity: 16,
                supported_commands: vec![],
            }),
            artwork_v1_support: None,
            visualizer_v1_support: None,
        }
    }

    fn registry(node_name: &str, display_name: &str) -> crate::sendspin_discovery::SharedSendspinDevices {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            node_name.to_string(),
            crate::sendspin_discovery::SendspinDevice {
                fullname: format!("{display_name}._sendspin._tcp.local."),
                display_name: display_name.to_string(),
                addr: None,
                present: true,
                url: Some("ws://192.0.2.1:8928/sendspin".to_string()),
                supported_codecs: Vec::new(),
                min_buffer_ms: None,
                required_lead_time_ms: None,
            },
        );
        Arc::new(Mutex::new(map))
    }

    #[test]
    fn capabilities_are_recorded_even_though_the_client_name_differs_from_the_discovery_name() {
        // The regression: matching the registry on `hello.name` found nothing, so a
        // device that had just advertised Opus + FLAC still showed up as
        // "codecs not known yet" and every group stayed on PCM.
        let node = "sendspin-dev-home_assistant_voice_093ca8";
        let devices = registry(node, "home-assistant-voice-093ca8");
        let hello = voice_pe_hello(&[("flac", 2), ("flac", 1), ("opus", 2), ("opus", 1), ("pcm", 2), ("pcm", 1)]);
        assert_ne!(hello.name, devices.lock_recover()[node].display_name, "the premise of this test");

        assert!(report_format_support(node, &hello, "pcm", &devices), "first sight of the capabilities is a change");
        assert_eq!(devices.lock_recover()[node].supported_codecs, vec!["opus", "flac", "pcm"]);
        // …and the group can now actually pick Opus.
        let caps = devices.lock_recover()[node].supported_codecs.clone();
        assert_eq!(resolve_codec(SendspinCodec::Auto, std::iter::once(&caps)), "opus");
        // A reconnect with the same capabilities must NOT re-trigger a reconcile
        // (that would restart the group's stream for nothing).
        assert!(!report_format_support(node, &hello, "opus", &devices));
    }

    #[test]
    fn send_ahead_is_raised_to_the_largest_member_requirement() {
        let configured = 250_000; // 250 ms, the default group lead
                                  // Nothing reported yet (no device has connected) → the configured lead stands.
        assert_eq!(required_send_ahead_us(configured, "pcm", [(None, 0)]), 250_000);
        // A member asking for less than the configured lead never lowers it — the user
        // may want more headroom than the hardware demands.
        assert_eq!(required_send_ahead_us(configured, "pcm", [(Some(100), 0)]), 250_000);
        // A member asking for more raises it, and the spec has the server add that
        // member's static delay on top (players exclude it from their own figure).
        assert_eq!(required_send_ahead_us(configured, "pcm", [(Some(400), 0)]), 400_000);
        assert_eq!(required_send_ahead_us(configured, "pcm", [(Some(400), 60)]), 460_000);
        // A group takes the MAXIMUM across members, so the neediest one is covered.
        assert_eq!(required_send_ahead_us(configured, "pcm", [(Some(300), 0), (Some(500), 20), (None, 0)]), 520_000);
    }

    #[test]
    fn a_silent_device_still_gets_its_codecs_floor() {
        // The real-hardware case: these speakers report NO min_buffer_ms, so before this
        // a user could run Opus at a 100 ms lead — which stutters.
        let configured = 100_000;
        assert_eq!(required_send_ahead_us(configured, "pcm", [(None, 0)]), 100_000, "PCM imposes nothing");
        assert_eq!(required_send_ahead_us(configured, "flac", [(None, 0)]), 100_000, "FLAC is proven at this lead");
        assert_eq!(required_send_ahead_us(configured, "opus", [(None, 0)]), 250_000, "Opus needs decode headroom");
        // The device's own static delay still adds on top, since it plays that early.
        assert_eq!(required_send_ahead_us(configured, "opus", [(None, 40)]), 290_000);
        // A device that DOES report wins over the codec floor, in both directions —
        // it knows its hardware better than our table does.
        assert_eq!(required_send_ahead_us(configured, "opus", [(Some(400), 0)]), 400_000);
        assert_eq!(required_send_ahead_us(configured, "opus", [(Some(120), 0)]), 120_000.max(configured));
    }

    #[test]
    fn only_formats_at_our_wire_format_count_as_supported() {
        let node = "sendspin-dev-x";
        let devices = registry(node, "x");
        // Mono-only Opus and a PCM stereo entry: the mono Opus is no use to a stereo
        // stream, so it must not make Opus selectable.
        let hello = voice_pe_hello(&[("opus", 1), ("pcm", 2)]);
        assert!(report_format_support(node, &hello, "pcm", &devices));
        assert_eq!(devices.lock_recover()[node].supported_codecs, vec!["pcm"]);
    }

    #[test]
    fn a_device_we_have_never_connected_to_is_only_pcm_assured() {
        // Empty = "hasn't told us yet" (capabilities come from client/hello, not
        // mDNS). For *display* that means only PCM is assured...
        let unknown: Vec<String> = Vec::new();
        assert!(device_supports(&unknown, "pcm"));
        assert!(!device_supports(&unknown, "opus"));
    }

    #[test]
    fn an_unknown_member_does_not_drag_the_group_off_its_codec() {
        // ...but for *stream selection* an unknown member imposes nothing: routing one
        // more speaker into a live Opus group must not downgrade everyone to PCM and
        // restart the group twice while the newcomer's client/hello is still in flight
        // (docs/sendspin-group-churn-plan.md §2b, H2).
        let unknown: Vec<String> = Vec::new();
        let capable = codecs(&["pcm", "opus"]);
        assert_eq!(resolve_codec(SendspinCodec::Auto, [&capable, &unknown].into_iter()), "opus");
        // A member that HAS spoken and lacks the codec still vetoes it — the
        // conservative rule is only relaxed for genuine ignorance.
        let pcm_only = codecs(&["pcm"]);
        assert_eq!(resolve_codec(SendspinCodec::Auto, [&capable, &pcm_only].into_iter()), "pcm");
        // A group of nothing but unknowns still gets the preferred codec, and
        // `report_format_support` corrects it on the first connect if that was wrong.
        assert_eq!(resolve_codec(SendspinCodec::Auto, std::iter::once(&unknown)), "opus");
        // An explicit pick is unaffected by ignorance in either direction.
        assert_eq!(resolve_codec(SendspinCodec::Pcm, std::iter::once(&unknown)), "pcm");
        assert_eq!(resolve_codec(SendspinCodec::Flac, std::iter::once(&unknown)), "flac");
    }
}
