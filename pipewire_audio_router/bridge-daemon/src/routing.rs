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
use crate::config::SENDSPIN_NODE_PREFIX;
use crate::locks::LockRecover;
use crate::raop::RAOP_NODE_PREFIX;
use crate::pw_thread::{LinkSpec, PortInfo, PwCommand, RegistryState, SharedState};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

#[derive(Serialize, Clone)]
pub struct RoutingNode {
    node_id: u32,
    /// Stable PipeWire node name (e.g. `"raop-out-kitchen"`,
    /// `"shairport-sync"`). Unlike `node_id`, this survives a module
    /// reload — the HA integration keys routing off this and re-resolves
    /// it to a live `node_id` per link/unlink call, so an automation never
    /// has to persist an ephemeral id. Additive field; existing web-client
    /// consumers that only read `node_id`/`display_name` are unaffected.
    node_name: String,
    display_name: String,
}

#[derive(Serialize, Clone)]
pub struct RoutingMatrix {
    sources: Vec<RoutingNode>,
    outputs: Vec<RoutingNode>,
    /// `(source_node_id, output_node_id)` pairs currently linked — "linked"
    /// means at least one channel is connected between them, matching the
    /// same node-level simplification `RegistryState::node_has_incoming_link`
    /// already uses elsewhere in this daemon.
    links: Vec<(u32, u32)>,
}

fn is_output_node(node_name: &str) -> bool {
    node_name.starts_with(RAOP_NODE_PREFIX) || node_name.starts_with(SENDSPIN_NODE_PREFIX)
}

fn output_display_name(node_name: &str) -> String {
    for prefix in [RAOP_NODE_PREFIX, SENDSPIN_NODE_PREFIX] {
        if let Some(rest) = node_name.strip_prefix(prefix) {
            return rest.replace(['_', '-'], " ");
        }
    }
    node_name.to_string()
}

fn channel_suffix(port_name: &str) -> &str {
    port_name.rsplit('_').next().unwrap_or(port_name)
}

fn build_matrix(state: &RegistryState) -> RoutingMatrix {
    let mut outputs = Vec::new();
    let mut sources = Vec::new();

    for node in state.nodes.values() {
        if is_output_node(&node.node_name) {
            outputs.push(RoutingNode {
                node_id: node.node_id,
                node_name: node.node_name.clone(),
                display_name: output_display_name(&node.node_name),
            });
            continue;
        }
        let has_real_source_port = state
            .ports
            .values()
            .any(|p| p.node_id == node.node_id && p.direction == "out" && !p.port_name.starts_with("monitor_"));
        if has_real_source_port {
            sources.push(RoutingNode {
                node_id: node.node_id,
                node_name: node.node_name.clone(),
                display_name: node.node_name.clone(),
            });
        }
    }

    let output_ids: std::collections::HashSet<u32> = outputs.iter().map(|o| o.node_id).collect();
    let source_ids: std::collections::HashSet<u32> = sources.iter().map(|s| s.node_id).collect();
    let mut links: Vec<(u32, u32)> = state
        .links
        .values()
        .filter(|l| source_ids.contains(&l.output_node) && output_ids.contains(&l.input_node))
        .map(|l| (l.output_node, l.input_node))
        .collect();
    links.sort_unstable();
    links.dedup();

    outputs.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    sources.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    RoutingMatrix { sources, outputs, links }
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

#[derive(Deserialize)]
pub struct LinkPairRequest {
    source_node_id: u32,
    output_node_id: u32,
}

#[derive(Serialize)]
pub struct LinkOpResponse {
    ok: bool,
    message: String,
}

pub async fn get_routing(State(pw): State<SharedState>) -> Json<RoutingMatrix> {
    Json(build_matrix(&pw.lock_recover()))
}

pub async fn link(State(state): State<AppState>, Json(req): Json<LinkPairRequest>) -> Json<LinkOpResponse> {
    let specs = matched_port_specs(&state.pw, req.source_node_id, req.output_node_id);
    if specs.is_empty() {
        return Json(LinkOpResponse {
            ok: false,
            message: "no matching source/output node or no matching channel ports".to_string(),
        });
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
    // Node-level unlink (matching the matrix's node-level "linked" semantics):
    // remove every link feeding this output from this source. Resolve the ids
    // from the current snapshot; destroy is idempotent, so any that raced away
    // before we act are harmless — the desired "not linked" end state holds.
    let link_ids: Vec<u32> = {
        let st = state.pw.lock_recover();
        st.links
            .values()
            .filter(|l| l.output_node == req.source_node_id && l.input_node == req.output_node_id)
            .map(|l| l.link_id)
            .collect()
    };
    if link_ids.is_empty() {
        return Json(LinkOpResponse { ok: true, message: "no links to remove".to_string() });
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

pub async fn routing_ws(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut changes = state.changes.subscribe();
    if send_snapshot(&mut socket, &state.pw).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            changed = changes.recv() => {
                match changed {
                    Ok(()) => {
                        if send_snapshot(&mut socket, &state.pw).await.is_err() {
                            break;
                        }
                    }
                    // A slow client missed some pings — one fresh snapshot
                    // catches it up completely, no need to replay history.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if send_snapshot(&mut socket, &state.pw).await.is_err() {
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

async fn send_snapshot(socket: &mut WebSocket, pw: &SharedState) -> Result<(), axum::Error> {
    let matrix = build_matrix(&pw.lock_recover());
    let json = serde_json::to_string(&matrix).unwrap_or_else(|_| "{}".to_string());
    socket.send(Message::Text(json)).await
}
