//! Manual routing UI backend (PLAN.md Section 8): a source×output matrix —
//! REST endpoints to read the matrix and toggle links, plus a WebSocket
//! that pushes a fresh matrix snapshot on every registry change instead of
//! requiring the client to poll.
//!
//! **Source/output classification is a heuristic over live registry
//! state, not a fixed list** — consistent with the rest of this project's
//! "trust the observed graph" approach (api.rs's `/api/media_players`):
//! - **Outputs** are exactly the nodes `/api/media_players` already
//!   recognizes (`RAOP_NODE_PREFIX`/`SENDSPIN_NODE_PREFIX`) — one source of
//!   truth for "what counts as a routable output" shared with the HA
//!   integration.
//! - **Sources** are any node with at least one **non-monitor** output-
//!   direction port. Every sink in this project also exposes `monitor_*`
//!   output ports (for `pw-record` taps) — excluding ports named with that
//!   prefix is what keeps a sink's own monitor tap from being
//!   misclassified as a routable source. Real sources observed so far
//!   (shairport-sync's AirPlay input, ad-hoc test nodes) all expose plain
//!   `output_*`/`capture_*` ports, never `monitor_*`.
//!
//! **Channel pairing**: linking a source to an output pairs ports by their
//! name's final `_`-separated suffix (`output_FL` ~ `send_FL` ~
//! `playback_FL` all pair as `FL`) — matches the convention already used
//! by hand in every test script in `tests/`, generalized here instead of
//! being hardcoded per source/output type.

use crate::api::AppState;
use crate::config::{SENDSPIN_DEV_PREFIX, SENDSPIN_NODE_PREFIX};
use crate::locks::LockRecover;
use crate::outputs_store::OutputsStore;
use crate::raop::{raop_node_name, RAOP_NODE_PREFIX};
use crate::pw_thread::{LinkSpec, PortInfo, PwCommand, PwCommandSender, RegistryState, SharedState};
use crate::routing_store::{self, RoutingLink, SharedRouting};
use crate::sendspin_discovery::SendspinDevice;
use std::collections::BTreeMap;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

#[derive(Serialize, Clone)]
pub struct RoutingNode {
    /// Stable node name — the primary key everything routes on. Survives module
    /// reloads and device disappearance/reappearance; the HA integration and
    /// the persisted routing intent (routing_store.rs) both key off it.
    node_name: String,
    display_name: String,
    /// Whether the node is in the live graph right now. `false` = configured or
    /// previously-routed but currently absent — shown grayed in the UI; its
    /// routing intent is kept and reapplied by reconcile() when it returns.
    present: bool,
    /// Live PipeWire node id when present (needed for per-node ops like
    /// volume). `None` for offline entities.
    node_id: Option<u32>,
    /// Outputs only: a manually-configured store entry (`true`) vs an
    /// mDNS-auto-discovered one (`false`) — drives the "auto-discovered" badge.
    /// Always `true` for sources (they aren't mDNS-discovered here).
    configured: bool,
}

#[derive(Serialize, Clone)]
pub struct RoutingMatrix {
    sources: Vec<RoutingNode>,
    outputs: Vec<RoutingNode>,
    /// Desired routing = persisted intent (routing_store.rs), by stable name.
    /// The UI renders these as the linked cells (including links to a currently
    /// offline endpoint, shown grayed); reconcile() makes the live graph match
    /// for pairs whose endpoints are both present.
    links: Vec<RoutingLink>,
}

fn is_output_node(node_name: &str) -> bool {
    node_name.starts_with(RAOP_NODE_PREFIX) || node_name.starts_with(SENDSPIN_NODE_PREFIX)
}

fn output_display_name(node_name: &str) -> String {
    for prefix in [RAOP_NODE_PREFIX, SENDSPIN_NODE_PREFIX, SENDSPIN_DEV_PREFIX] {
        if let Some(rest) = node_name.strip_prefix(prefix) {
            return rest.replace(['_', '-'], " ");
        }
    }
    node_name.to_string()
}

fn channel_suffix(port_name: &str) -> &str {
    port_name.rsplit('_').next().unwrap_or(port_name)
}

/// Build the matrix from live registry + configured outputs + persisted intent.
///
/// Every routable entity is included even when it isn't in the graph right
/// now: an output that's configured or has saved routing, and any source
/// referenced by intent, appear as `present: false` (grayed in the UI) so its
/// routing survives disappearance and is reapplied on return. Outputs also
/// carry `configured` (store entry vs mDNS auto-discovered) for the badge.
fn build_matrix(
    reg: &RegistryState,
    store: &OutputsStore,
    devices: &BTreeMap<String, SendspinDevice>,
    intent: &[RoutingLink],
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
            let has_real_source_port = reg
                .ports
                .values()
                .any(|p| p.node_id == node.node_id && p.direction == "out" && !p.port_name.starts_with("monitor_"));
            if has_real_source_port {
                let e = present_sources.entry(node.node_name.clone()).or_insert(node.node_id);
                *e = (*e).max(node.node_id);
            }
        }
    }

    // Configured RAOP outputs: node_name -> friendly display name.
    let configured: BTreeMap<String, String> =
        store.list().iter().map(|o| (raop_node_name(&o.name), o.name.clone())).collect();

    // Discovered sendspin devices are virtual outputs (present, auto, no live
    // node id — audio reaches them via a group sink, sendspin_group.rs).

    // Union of every output/source name to show: present ∪ configured ∪
    // discovered devices ∪ intent.
    let mut output_names: BTreeSet<String> = present_outputs.keys().cloned().collect();
    output_names.extend(configured.keys().cloned());
    output_names.extend(devices.keys().cloned());
    output_names.extend(intent.iter().map(|l| l.output.clone()));

    let mut source_names: BTreeSet<String> = present_sources.keys().cloned().collect();
    source_names.extend(intent.iter().map(|l| l.source.clone()));

    let mut outputs: Vec<RoutingNode> = output_names
        .into_iter()
        .map(|name| {
            let node_id = present_outputs.get(&name).copied();
            let is_device = devices.contains_key(&name);
            let display_name = configured
                .get(&name)
                .cloned()
                .or_else(|| devices.get(&name).map(|d| d.display_name.clone()))
                .unwrap_or_else(|| output_display_name(&name));
            RoutingNode {
                // A device is "present" when it's in the live discovery registry.
                present: node_id.is_some() || is_device,
                node_id,
                // Devices and offline entries are never manually configured.
                configured: configured.contains_key(&name),
                display_name,
                node_name: name,
            }
        })
        .collect();

    let mut sources: Vec<RoutingNode> = source_names
        .into_iter()
        .map(|name| {
            let node_id = present_sources.get(&name).copied();
            RoutingNode { present: node_id.is_some(), node_id, configured: true, display_name: name.clone(), node_name: name }
        })
        .collect();

    outputs.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    sources.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    let mut links = intent.to_vec();
    links.sort();

    RoutingMatrix { sources, outputs, links }
}

/// Snapshot the matrix from shared state (locks registry + outputs store +
/// routing intent). Lock order is fixed here — routing first (released before
/// the others), then registry, then store — to stay deadlock-free.
fn build_snapshot(state: &AppState) -> RoutingMatrix {
    let intent = routing_store::snapshot(&state.routing);
    let devices = state.sendspin_devices.lock_recover().clone();
    let reg = state.pw.lock_recover();
    let store = state.store.lock_recover();
    build_matrix(&reg, &store, &devices, &intent)
}

/// Every non-monitor output-direction port on `source_node_id` paired with the
/// same-channel input-direction port on `output_node_id`, as `LinkSpec`s
/// (resolved to PipeWire object ids) ready for a `CreateLinks` command. Empty
/// if either node doesn't exist or no channel suffixes match on both sides.
fn matched_port_specs(state: &SharedState, source_node_id: u32, output_node_id: u32) -> Vec<LinkSpec> {
    let state = state.lock_recover();
    if !state.nodes.contains_key(&source_node_id) || !state.nodes.contains_key(&output_node_id) {
        return Vec::new();
    }

    let source_ports: Vec<&PortInfo> = state
        .ports
        .values()
        .filter(|p| p.node_id == source_node_id && p.direction == "out" && !p.port_name.starts_with("monitor_"))
        .collect();
    let output_ports: Vec<&PortInfo> = state.ports.values().filter(|p| p.node_id == output_node_id && p.direction == "in").collect();

    source_ports
        .iter()
        .filter_map(|sp| {
            let suffix = channel_suffix(&sp.port_name);
            output_ports
                .iter()
                .find(|op| channel_suffix(&op.port_name) == suffix)
                .map(|op| LinkSpec {
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
fn node_id_for(state: &RegistryState, node_name: &str) -> Option<u32> {
    state.nodes.values().filter(|n| n.node_name == node_name).map(|n| n.node_id).max()
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

/// Reapply persisted routing intent (routing_store.rs) to the live graph: for
/// every stored `(source, output)` link whose *both* nodes are currently
/// present, ensure the matched-channel PipeWire links exist. Idempotent —
/// `CreateLinks` skips ports already linked — so this is safe to call on every
/// registry change. **Additive only**: it never removes links, so a manual
/// unlink (which also drops the intent) stays gone and this can't fight the
/// user. Intent whose endpoint is absent is simply left pending until the node
/// (re)appears and a later call links it.
pub async fn reconcile(pw: &SharedState, pw_cmd: &PwCommandSender, routing: &SharedRouting) {
    for link in routing_store::snapshot(routing) {
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
    Json(build_snapshot(&state))
}

pub async fn link(State(state): State<AppState>, Json(req): Json<LinkPairRequest>) -> Json<LinkOpResponse> {
    // Persist the desired route first; it's the source of truth and reconcile()
    // (re)applies it whenever both endpoints are present.
    if let Err(e) = state.routing.lock_recover().add(&req.source, &req.output) {
        return Json(LinkOpResponse { ok: false, message: format!("failed to persist routing intent: {e}") });
    }
    // Apply live now if both ends are present; otherwise it stays pending.
    let ids = {
        let st = state.pw.lock_recover();
        node_id_for(&st, &req.source).zip(node_id_for(&st, &req.output))
    };
    let Some((source_id, output_id)) = ids else {
        return Json(LinkOpResponse { ok: true, message: "saved; will apply when both endpoints are present".to_string() });
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
    // Drop the intent first so reconcile() won't re-create what we remove.
    // Works for offline pairs too — this is purely by name.
    if let Err(e) = state.routing.lock_recover().remove(&req.source, &req.output) {
        return Json(LinkOpResponse { ok: false, message: format!("failed to persist routing intent: {e}") });
    }
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
        st.links
            .values()
            .filter(|l| l.output_node == source_id && l.input_node == output_id)
            .map(|l| l.link_id)
            .collect()
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
    match state.routing.lock_recover().remove_entity(&node_name) {
        Ok(()) => Json(LinkOpResponse { ok: true, message: format!("forgot routing for '{node_name}'") }),
        Err(e) => Json(LinkOpResponse { ok: false, message: format!("failed to forget '{node_name}': {e}") }),
    }
}

pub async fn routing_ws(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut changes = state.changes.subscribe();
    if send_snapshot(&mut socket, &state).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            changed = changes.recv() => {
                match changed {
                    Ok(()) => {
                        if send_snapshot(&mut socket, &state).await.is_err() {
                            break;
                        }
                    }
                    // A slow client missed some pings — one fresh snapshot
                    // catches it up completely, no need to replay history.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if send_snapshot(&mut socket, &state).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // This UI never sends messages; we only poll the socket here
            // to notice the client disconnecting (recv() returning None).
            incoming = socket.recv() => {
                if incoming.is_none() {
                    break;
                }
            }
        }
    }
}

async fn send_snapshot(socket: &mut WebSocket, state: &AppState) -> Result<(), axum::Error> {
    let matrix = build_snapshot(state);
    let json = serde_json::to_string(&matrix).unwrap_or_else(|_| "{}".to_string());
    socket.send(Message::Text(json)).await
}
