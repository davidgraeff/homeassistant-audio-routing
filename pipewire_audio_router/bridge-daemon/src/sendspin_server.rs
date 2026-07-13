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
use crate::pw_thread::{PwCommand, PwCommandSender, SharedState};
use sendspin::protocol::messages::StreamPlayerConfig;
use sendspin::server::{Advertisement, ClientEvent, ClientManager, Group};
use sendspin::{DefaultClock, ServerConnection, ServerListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

/// Fixed by the spec for real deployments (matches sendspin-rs's own
/// convention and this daemon's other sendspin-adjacent code).
const SENDSPIN_PATH: &str = "/sendspin";

/// Everything one running sendspin output owns. Dropping it tears down every
/// task/thread/background resource it started — the in-process equivalent of
/// `Supervisor::stop`.
pub struct SendspinServerHandle {
    _advertisement: Advertisement,
    _client_manager: ClientManager,
    _capture: crate::sendspin_capture::CaptureHandle,
    accept_task: JoinHandle<()>,
    event_task: JoinHandle<()>,
    capture_forward_task: JoinHandle<()>,
    node_id: u32,
    pw_cmd: PwCommandSender,
}

impl Drop for SendspinServerHandle {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.event_task.abort();
        self.capture_forward_task.abort();
        // _capture's own Drop stops the capture thread; _client_manager's own
        // Drop stops discovery/reconnect loops; _advertisement's own Drop
        // unregisters the mDNS advertisement. Destroying the sink node itself
        // needs the resolved node id and goes through pw_thread — fire and
        // forget, matching how removing a RAOP output doesn't block on the
        // unload either.
        let (reply_tx, _reply_rx) = oneshot::channel();
        let _ = self.pw_cmd.send(PwCommand::DestroySinkNode {
            node_id: self.node_id,
            reply: reply_tx,
        });
    }
}

/// Start a native sendspin server on `node_name`/`port`: create its sink node,
/// wait for it to appear in the registry snapshot, capture from it, and run the
/// embedded server (accept inbound + discover/dial devices) pushing captured
/// audio to one shared `Group`.
///
/// `device_filter`, when `Some`, restricts dialing to devices whose mDNS
/// fullname is in the set — this is what makes a *group*: one sink + one
/// synchronized `Group` dialing exactly its member devices. `None` dials every
/// discovered device (the manual-output behavior).
pub async fn start_server(
    node_name: &str,
    display_name: &str,
    port: u16,
    device_filter: Option<std::collections::HashSet<String>>,
    pw_state: SharedState,
    pw_cmd: PwCommandSender,
) -> anyhow::Result<SendspinServerHandle> {
    let node_name = node_name.to_string();

    let (reply_tx, reply_rx) = oneshot::channel();
    pw_cmd
        .send(PwCommand::CreateSinkNode {
            node_name: node_name.clone(),
            reply: reply_tx,
        })
        .map_err(|_| anyhow::anyhow!("pipewire thread is gone"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("pipewire thread dropped the reply"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let node_id = wait_for_node_id(&pw_state, &node_name)
        .await
        .ok_or_else(|| anyhow::anyhow!("sink node '{node_name}' never appeared in the registry"))?;
    // Guards the node we just created: if anything below fails and returns
    // early via `?`, `SendspinServerHandle` (whose `Drop` normally sends
    // `DestroySinkNode`) never gets constructed, which would otherwise leak
    // this node in the graph forever (confirmed live: a failed bind left an
    // orphaned `support.null-audio-sink` node with no owner). Disarmed only
    // once the handle is actually built.
    let node_guard = SinkNodeGuard::new(node_id, pw_cmd.clone());

    let (capture_handle, mut pcm_rx) = crate::sendspin_capture::spawn(node_id)
        .map_err(|e| anyhow::anyhow!("failed to start capture for '{node_name}': {e}"))?;

    let listener = ServerListener::bind(("0.0.0.0", port), &node_name, display_name)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind sendspin server on port {port}: {e}"))?
        .path(SENDSPIN_PATH);
    let advertisement = Advertisement::new(&node_name, display_name, port, SENDSPIN_PATH)
        .map_err(|e| anyhow::anyhow!("failed to advertise sendspin server '{node_name}': {e}"))?;

    let group = Arc::new(Mutex::new(Group::new(Arc::new(DefaultClock::default()))));

    let accept_task = spawn_accept_loop(listener, Arc::clone(&group));

    let (client_manager, mut events) = ClientManager::start_filtered(
        node_name.clone(),
        display_name.to_string(),
        Arc::new(DefaultClock::default()),
        move |fullname| device_filter.as_ref().map_or(true, |set| set.contains(fullname)),
    )
    .map_err(|e| anyhow::anyhow!("failed to start sendspin client discovery for '{node_name}': {e}"))?;
    let event_task = {
        let group = Arc::clone(&group);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    ClientEvent::Connected {
                        client_id, sender, ..
                    } => {
                        if let Err(e) = group.lock().await.add_member(client_id, sender).await {
                            tracing::warn!("failed to add sendspin group member: {e}");
                        }
                    }
                    // client/state, client/command — no group-level reaction
                    // needed yet (volume/mute persistence per client is a
                    // later refinement, not required for parity with today).
                    ClientEvent::Message { .. } => {}
                    ClientEvent::Disconnected { client_id } => {
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
                let mut group = group.lock().await;
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
                group.push_audio(&pcm).await;
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
        node_id: node_guard.disarm(),
        pw_cmd,
    })
}

/// Owns a just-created sink node's cleanup until something else takes over.
/// Sends `DestroySinkNode` on drop unless `disarm()` was called first — see
/// its construction site in `start()` for why this exists.
struct SinkNodeGuard {
    node_id: u32,
    pw_cmd: PwCommandSender,
    armed: bool,
}

impl SinkNodeGuard {
    fn new(node_id: u32, pw_cmd: PwCommandSender) -> Self {
        Self { node_id, pw_cmd, armed: true }
    }

    /// Hands ownership of the node's lifecycle to the caller (the eventual
    /// `SendspinServerHandle`) without destroying it.
    fn disarm(mut self) -> u32 {
        self.armed = false;
        self.node_id
    }
}

impl Drop for SinkNodeGuard {
    fn drop(&mut self) {
        if self.armed {
            let (reply_tx, _reply_rx) = oneshot::channel();
            let _ = self.pw_cmd.send(PwCommand::DestroySinkNode {
                node_id: self.node_id,
                reply: reply_tx,
            });
        }
    }
}

/// Accept clients that dial in to us (some real Sendspin clients do; see
/// sendspin-rs's `ServerListener` docs).
fn spawn_accept_loop(listener: ServerListener, group: Arc<Mutex<Group>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((conn, _addr)) => {
                    let client_id = conn.client_id().to_string();
                    let sender = conn.sender();
                    tokio::spawn(drain_messages(conn));
                    if let Err(e) = group.lock().await.add_member(client_id, sender).await {
                        tracing::warn!("failed to add inbound sendspin group member: {e}");
                    }
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
/// keeps that going. No group-level reaction needed yet.
async fn drain_messages(mut conn: ServerConnection) {
    while conn.recv_message().await.is_some() {}
}

/// Polls the shared registry snapshot for a node named `node_name`. Needed
/// because `CreateSinkNode`'s reply confirms the *request* succeeded, not
/// that the node has shown up in the registry snapshot yet — mirrors the
/// wait-for-node-to-appear pattern this repo's own e2e test scripts already
/// use for the exact same reason.
///
/// Picks the highest-numbered (newest) match rather than an arbitrary one:
/// node names aren't unique at the PipeWire level, and a same-named node can
/// briefly outlive its owner (`object.linger`) or, historically, leak on a
/// failed startup — see `SinkNodeGuard`. Registry ids only increase within a
/// session, so "highest id" reliably means "most recently created" here.
async fn wait_for_node_id(pw_state: &SharedState, node_name: &str) -> Option<u32> {
    for _ in 0..50 {
        {
            let state = pw_state.lock_recover();
            if let Some(id) = state.nodes.iter().filter(|(_, n)| n.node_name == node_name).map(|(&id, _)| id).max() {
                return Some(id);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}
