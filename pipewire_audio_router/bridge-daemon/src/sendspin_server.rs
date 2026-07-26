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
//! Unfiltered discovery (`ClientManager::start`, not `start_filtered`) is
//! deliberate, not an oversight — see docs/decisions.md: aiosendspin's
//! `SendspinServer.start_server()` already defaults `discover_clients=True`,
//! so today's Python adapter already discovers and dials every such device
//! on the network, once per configured output/process. This just reproduces
//! that in Rust; whatever already arbitrates "which server's connection does
//! a device keep" continues to do so unchanged.

use crate::locks::LockRecover;
use sendspin::protocol::messages::{Message, StreamPlayerConfig};
use sendspin::server::{Advertisement, ClientEvent, ClientManager, Group, SharedTimeline};
use sendspin::{Clock, DefaultClock, ServerConnection, ServerListener};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

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
    _client_manager: ClientManager,
    _capture: crate::sendspin_capture::CaptureHandle,
    accept_task: JoinHandle<()>,
    event_task: JoinHandle<()>,
    /// The capture→wire relay runs on its own dedicated (RT-scheduled) OS
    /// thread, not a tokio task — see `set_relay_realtime_priority`. It stops
    /// when `_capture`'s Drop closes the PCM channel (its `blocking_recv`
    /// returns `None`), so it needs no explicit abort; the handle is held only
    /// to keep it named. Dropping it detaches — the thread exits on its own.
    _relay_thread: std::thread::JoinHandle<()>,
}

impl Drop for SendspinServerHandle {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.event_task.abort();
        // `_capture`'s own Drop stops the capture thread, which drops the PCM
        // sender and closes the relay's channel → the relay thread exits.
        // `_client_manager`'s Drop stops discovery/reconnect; `_advertisement`'s
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
/// delivered to every group via `push_encoded`, so chunk-N carries an identical
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
    device_filter: std::collections::HashSet<String>,
    send_ahead_us: i64,
    control: crate::sendspin_volume::SharedSendspinControl,
    devices: crate::sendspin_discovery::SharedSendspinDevices,
) -> anyhow::Result<SendspinServerHandle> {
    let node_name = server_name.to_string();

    let (capture_handle, mut pcm_rx) = crate::sendspin_capture::spawn(sink_node_id)
        .map_err(|e| anyhow::anyhow!("failed to start capture for '{node_name}': {e}"))?;

    // One clock shared by the timeline and the dial manager, so the timestamps
    // stamped here are in the same domain as the `server/time` replies members
    // trust. (On Linux even distinct DefaultClocks share CLOCK_MONOTONIC_RAW,
    // but sharing the Arc is the portable guarantee.)
    let clock: Arc<dyn Clock> = Arc::new(DefaultClock::default());

    let listener = ServerListener::bind(("0.0.0.0", port), &node_name, display_name)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind sendspin server on port {port}: {e}"))?
        .path(SENDSPIN_PATH);
    let advertisement = advertise(&node_name, display_name, port)
        .map_err(|e| anyhow::anyhow!("failed to advertise sendspin server '{node_name}': {e}"))?;

    // The single shared timeline. Config is set once up front so every member
    // gets `stream/start` when it joins and the timeline is never re-anchored
    // mid-stream (a per-member re-anchor would desync the others).
    let timeline = Arc::new(SharedTimeline::new(Arc::clone(&clock)).with_send_ahead_us(send_ahead_us));
    timeline.set_config(StreamPlayerConfig {
        codec: "pcm".to_string(),
        sample_rate: crate::sendspin_capture::SAMPLE_RATE,
        channels: crate::sendspin_capture::CHANNELS as u8,
        bit_depth: 16,
        codec_header: None,
    });

    // One single-member Group per device, all sharing `timeline`. Keyed by the
    // opaque client_id; `client_to_node` maps that to the device's output node
    // name so the capture loop can look up its overlay (announcement) state.
    let groups: Arc<Mutex<HashMap<String, Group>>> = Arc::new(Mutex::new(HashMap::new()));
    let client_to_node: Arc<std::sync::Mutex<HashMap<String, String>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    let accept_task = spawn_accept_loop_per_device(
        listener,
        Arc::clone(&groups),
        Arc::clone(&client_to_node),
        Arc::clone(&timeline),
        control.clone(),
    );

    // Browse for dial-in clients on the shared, LAN-restricted mDNS daemon so
    // the client discovery doesn't spin its own all-interfaces daemon (which,
    // under host-networking, re-storms the multicast across the Docker veths
    // when a discovered device's address doesn't resolve).
    let (client_manager, mut events) = ClientManager::start_filtered_with_daemon(
        node_name.clone(),
        display_name.to_string(),
        Arc::clone(&clock),
        move |fullname| device_filter.contains(fullname),
        crate::discovery_supervisor::shared_advertise_daemon(),
    )
    .map_err(|e| anyhow::anyhow!("failed to start sendspin client discovery for '{node_name}': {e}"))?;

    let event_task = {
        let groups = Arc::clone(&groups);
        let client_to_node = Arc::clone(&client_to_node);
        let timeline = Arc::clone(&timeline);
        let control = control.clone();
        let devices = devices.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    ClientEvent::Connected { client_id, fullname, sender, .. } => {
                        let dev_node_name = resolve_node_name(&devices, &fullname);
                        tracing::info!("sendspin per-device: '{dev_node_name}' connected (client {client_id}) — (re)adding to its group");
                        client_to_node.lock().unwrap().insert(client_id.clone(), dev_node_name.clone());
                        control.lock().await.register(dev_node_name, sender.clone()).await;
                        // Its own group on the shared timeline. add_member sends
                        // stream/start (config is already set), no re-anchor. Replacing
                        // any prior group for this client_id (a reconnect) drops the
                        // stale one so the fresh sender starts receiving audio.
                        let group = Group::with_timeline(Arc::clone(&timeline));
                        if let Err(e) = group.add_member(client_id.clone(), sender).await {
                            tracing::warn!("failed to add per-device member: {e}");
                        }
                        groups.lock_recover().insert(client_id, group);
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
                    }
                    ClientEvent::Disconnected { client_id } => {
                        // Bind the removal to a statement so the (non-Send) std
                        // MutexGuard drops before the await below.
                        let removed = client_to_node.lock().unwrap().remove(&client_id);
                        if let Some(ref dev_node_name) = removed {
                            tracing::info!("sendspin per-device: '{dev_node_name}' disconnected (client {client_id}) — awaiting ClientManager re-dial");
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
    // overlay `mix`, and `Group::push_encoded` (which enqueues to each member's
    // writer task without awaiting). `blocking_recv` drains the capture channel
    // and returns `None` when capture stops, ending the thread.
    let relay_thread = {
        let groups = Arc::clone(&groups);
        let client_to_node = Arc::clone(&client_to_node);
        let timeline = Arc::clone(&timeline);
        std::thread::Builder::new()
            .name("sendspin-relay".into())
            .spawn(move || {
                set_relay_realtime_priority();
                let mixer = crate::overlay_mixer::OverlayMixer::global();
                // Reused across chunks AND across devices within a chunk so the
                // per-device overlay mix allocates at most once (only relevant
                // while an announcement is overlaying; the plain-music path never
                // touches it). push_encoded copies into its own wire frame
                // synchronously, so one buffer is safe to reuse for every device.
                let mut mix_buf: Vec<u8> = Vec::new();
                while let Some(pcm) = pcm_rx.blocking_recv() {
                    // Stamp ONCE per chunk, then fan that identical ts to every
                    // device's group — the shared-timeline sync guarantee. A
                    // device with an active announcement overlay gets
                    // duck(music)+overlay instead of the plain chunk; its
                    // groupmates get plain music. The two locks are held only
                    // for this brief synchronous fan-out (M5): no encode/mix
                    // work happens before the locks are taken, and the guards
                    // drop at the end of each iteration.
                    let ts = timeline.stamp(pcm.len());
                    let groups = groups.lock_recover();
                    let c2n = client_to_node.lock().unwrap();
                    for (client_id, group) in groups.iter() {
                        let overlaid = c2n
                            .get(client_id)
                            .is_some_and(|node| mixer.mix_into(node, &pcm, &mut mix_buf));
                        if overlaid {
                            group.push_encoded(ts, &mix_buf);
                        } else {
                            group.push_encoded(ts, &pcm);
                        }
                    }
                    // Finished overlays are drained by the AnnounceCoordinator's
                    // poll loop (main.rs), not here.
                }
                tracing::debug!("sendspin relay thread exiting");
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn sendspin relay thread for '{node_name}': {e}"))?
    };

    Ok(SendspinServerHandle {
        _advertisement: advertisement,
        _client_manager: client_manager,
        _capture: capture_handle,
        accept_task,
        event_task,
        _relay_thread: relay_thread,
    })
}

/// Accept loop for the per-device topology: each inbound client gets its own
/// single-member group on the shared timeline (mirrors `spawn_accept_loop`).
fn spawn_accept_loop_per_device(
    listener: ServerListener,
    groups: Arc<Mutex<HashMap<String, Group>>>,
    client_to_node: Arc<Mutex<HashMap<String, String>>>,
    timeline: Arc<SharedTimeline>,
    control: crate::sendspin_volume::SharedSendspinControl,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((conn, _addr)) => {
                    let client_id = conn.client_id().to_string();
                    let node_name = crate::sendspin_discovery::device_node_name(&conn.hello().name);
                    let sender = conn.sender();
                    control.lock().await.register(node_name.clone(), sender.clone()).await;
                    client_to_node.lock().unwrap().insert(client_id.clone(), node_name.clone());
                    let group = Group::with_timeline(Arc::clone(&timeline));
                    if let Err(e) = group.add_member(client_id.clone(), sender).await {
                        tracing::warn!("failed to add inbound per-device member: {e}");
                    }
                    groups.lock_recover().insert(client_id.clone(), group);
                    tokio::spawn(drain_messages_per_device(
                        conn,
                        client_id,
                        node_name,
                        Arc::clone(&groups),
                        Arc::clone(&client_to_node),
                        control.clone(),
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
    groups: Arc<Mutex<HashMap<String, Group>>>,
    client_to_node: Arc<Mutex<HashMap<String, String>>>,
    control: crate::sendspin_volume::SharedSendspinControl,
) {
    while let Some(message) = conn.recv_message().await {
        // Device→UI volume/mute sync (same as the dial-out path): reflect a
        // physically-changed volume/mute the device reports back into the UI.
        if let Some((volume, muted)) = reported_player_state(&message) {
            apply_reported_state(&control, &node_name, volume, muted).await;
        }
    }
    control.lock().await.unregister(&node_name);
    client_to_node.lock().unwrap().remove(&client_id);
    groups.lock_recover().remove(&client_id);
}
