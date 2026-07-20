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
use sendspin::protocol::messages::StreamPlayerConfig;
use sendspin::server::{Advertisement, ClientEvent, ClientManager, Group};
use sendspin::{DefaultClock, ServerConnection, ServerListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Fixed by the spec for real deployments (matches sendspin-rs's own
/// convention and this daemon's other sendspin-adjacent code).
const SENDSPIN_PATH: &str = "/sendspin";

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
    capture_forward_task: JoinHandle<()>,
}

impl Drop for SendspinServerHandle {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.event_task.abort();
        self.capture_forward_task.abort();
        // _capture's own Drop stops the capture thread; _client_manager's own
        // Drop stops discovery/reconnect loops; _advertisement's own Drop
        // unregisters the mDNS advertisement. The sink node is the shared anchor
        // owned by sync_group.rs — not destroyed here.
    }
}

/// Start a native sendspin server bound to `port`, capturing from the already-
/// existing sink node `sink_node_id` (the shared sync anchor created by
/// sync_group.rs) and running the embedded server (accept inbound + discover/dial
/// devices) pushing captured audio to one shared `Group`. `server_name` is the
/// server's own identity/advertisement label.
///
/// `device_filter`, when `Some`, restricts dialing to devices whose mDNS
/// fullname is in the set — this is what makes a *group*: one anchor + one
/// synchronized `Group` dialing exactly its member devices. `None` dials every
/// discovered device (the manual-output behavior).
///
/// `send_ahead_us`, when `Some`, overrides the group's presentation lead — how
/// far ahead of "now" audio is scheduled (raise it to let slower members, e.g. a
/// RAOP receiver sharing this anchor, play the same instant; see
/// [`sendspin::server::Group::with_send_ahead_us`]). `None` uses the protocol
/// default.
// Each parameter is a distinct shared handle the server needs; a struct wrapper
// wouldn't clarify the one call site (sync_group.rs).
#[allow(clippy::too_many_arguments)]
pub async fn start_server(
    server_name: &str,
    display_name: &str,
    port: u16,
    sink_node_id: u32,
    device_filter: Option<std::collections::HashSet<String>>,
    send_ahead_us: Option<i64>,
    control: crate::sendspin_volume::SharedSendspinControl,
    devices: crate::sendspin_discovery::SharedSendspinDevices,
) -> anyhow::Result<SendspinServerHandle> {
    let node_name = server_name.to_string();

    let (capture_handle, mut pcm_rx) = crate::sendspin_capture::spawn(sink_node_id)
        .map_err(|e| anyhow::anyhow!("failed to start capture for '{node_name}': {e}"))?;

    let listener = ServerListener::bind(("0.0.0.0", port), &node_name, display_name)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind sendspin server on port {port}: {e}"))?
        .path(SENDSPIN_PATH);
    let advertisement = Advertisement::new(&node_name, display_name, port, SENDSPIN_PATH)
        .map_err(|e| anyhow::anyhow!("failed to advertise sendspin server '{node_name}': {e}"))?;

    let base_group = Group::new(Arc::new(DefaultClock::default()));
    let group = Arc::new(Mutex::new(match send_ahead_us {
        Some(us) => base_group.with_send_ahead_us(us),
        None => base_group,
    }));

    let accept_task = spawn_accept_loop(listener, Arc::clone(&group), control.clone());

    let (client_manager, mut events) =
        ClientManager::start_filtered(node_name.clone(), display_name.to_string(), Arc::new(DefaultClock::default()), move |fullname| {
            device_filter.as_ref().is_none_or(|set| set.contains(fullname))
        })
        .map_err(|e| anyhow::anyhow!("failed to start sendspin client discovery for '{node_name}': {e}"))?;
    let event_task = {
        let group = Arc::clone(&group);
        let control = control.clone();
        let devices = devices.clone();
        // The group keys members by client_id (an opaque MAC), but volume
        // control keys by the virtual device node name; remember the mapping so
        // a Disconnected (which only carries client_id) can unregister volume.
        let mut client_to_node: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    ClientEvent::Connected { client_id, fullname, sender, .. } => {
                        // Resolve the discovered device's node name from the
                        // dialed mDNS fullname (client_id is a MAC and won't
                        // match). Register for volume control (applies any
                        // stored per-device volume), then add to the sync group.
                        let node_name = resolve_node_name(&devices, &fullname);
                        client_to_node.insert(client_id.clone(), node_name.clone());
                        control.lock().await.register(node_name, sender.clone()).await;
                        if let Err(e) = group.lock().await.add_member(client_id, sender).await {
                            tracing::warn!("failed to add sendspin group member: {e}");
                        }
                    }
                    // client/state, client/command from the device — no
                    // server-side reaction needed (we push volume, not poll it).
                    ClientEvent::Message { .. } => {}
                    ClientEvent::Disconnected { client_id } => {
                        if let Some(node_name) = client_to_node.remove(&client_id) {
                            control.lock().await.unregister(&node_name);
                        }
                        group.lock().await.remove_member(&client_id);
                    }
                }
            }
        })
    };

    let capture_forward_task = {
        let group = Arc::clone(&group);
        let stream_started = AtomicBool::new(false);
        tokio::spawn(async move {
            while let Some(pcm) = pcm_rx.recv().await {
                let group = group.lock().await;
                if !stream_started.swap(true, Ordering::Relaxed) {
                    group
                        .start_stream(StreamPlayerConfig {
                            codec: "pcm".to_string(),
                            sample_rate: crate::sendspin_capture::SAMPLE_RATE,
                            channels: crate::sendspin_capture::CHANNELS as u8,
                            bit_depth: 16,
                            codec_header: None,
                        })
                        .await;
                }
                group.push_audio(&pcm);
            }
        })
    };

    Ok(SendspinServerHandle {
        _advertisement: advertisement,
        _client_manager: client_manager,
        _capture: capture_handle,
        accept_task,
        event_task,
        capture_forward_task,
    })
}

/// Accept clients that dial in to us (some real Sendspin clients do; see
/// sendspin-rs's `ServerListener` docs).
fn spawn_accept_loop(
    listener: ServerListener,
    group: Arc<Mutex<Group>>,
    control: crate::sendspin_volume::SharedSendspinControl,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((conn, _addr)) => {
                    let client_id = conn.client_id().to_string();
                    // Inbound dial-ins carry no mDNS fullname; derive the node
                    // name from the client's self-reported hello name instead.
                    let node_name = crate::sendspin_discovery::device_node_name(&conn.hello().name);
                    let sender = conn.sender();
                    control.lock().await.register(node_name.clone(), sender.clone()).await;
                    if let Err(e) = group.lock().await.add_member(client_id.clone(), sender).await {
                        tracing::warn!("failed to add inbound sendspin group member: {e}");
                    }
                    tokio::spawn(drain_messages(conn, client_id, node_name, Arc::clone(&group), control.clone()));
                }
                Err(e) => {
                    tracing::warn!("sendspin accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    })
}

/// `ServerConnection::recv_message` must be polled for the connection to make
/// progress at all (it also drives the underlying message loop) — this just
/// keeps that going, then drops the client from the group + volume control when
/// the connection ends (the dial path gets this from `ClientEvent`; inbound
/// connections have no such event, so we clean up here).
async fn drain_messages(
    mut conn: ServerConnection,
    client_id: String,
    node_name: String,
    group: Arc<Mutex<Group>>,
    control: crate::sendspin_volume::SharedSendspinControl,
) {
    while conn.recv_message().await.is_some() {}
    control.lock().await.unregister(&node_name);
    group.lock().await.remove_member(&client_id);
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
