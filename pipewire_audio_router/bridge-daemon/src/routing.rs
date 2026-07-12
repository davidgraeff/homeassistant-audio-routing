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
use crate::pw_config_gen::RAOP_NODE_PREFIX;
use crate::pw_thread::{PortInfo, RegistryState, SharedState};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Clone)]
pub struct RoutingNode {
    node_id: u32,
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

/// Every non-monitor output-direction port on `source_node_id` paired with
/// the same-channel input-direction port on `output_node_id`, as full
/// `"node:port"` strings ready for `pw-link`/`pw-link -d`. Empty if either
/// node doesn't exist or no channel suffixes match on both sides.
fn matched_port_pairs(state: &SharedState, source_node_id: u32, output_node_id: u32) -> Vec<(String, String)> {
    let state = state.lock().unwrap();
    let Some(source_name) = state.nodes.get(&source_node_id).map(|n| n.node_name.clone()) else {
        return Vec::new();
    };
    let Some(output_name) = state.nodes.get(&output_node_id).map(|n| n.node_name.clone()) else {
        return Vec::new();
    };

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
                .map(|op| (format!("{source_name}:{}", sp.port_name), format!("{output_name}:{}", op.port_name)))
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
    Json(build_matrix(&pw.lock().unwrap()))
}

pub async fn link(State(pw): State<SharedState>, Json(req): Json<LinkPairRequest>) -> Json<LinkOpResponse> {
    let pairs = matched_port_pairs(&pw, req.source_node_id, req.output_node_id);
    if pairs.is_empty() {
        return Json(LinkOpResponse {
            ok: false,
            message: "no matching source/output node or no matching channel ports".to_string(),
        });
    }
    let mut ok = true;
    let mut messages = Vec::with_capacity(pairs.len());
    for (from, to) in &pairs {
        let output = tokio::process::Command::new("pw-link").arg(from).arg(to).output().await;
        match output {
            Ok(o) if o.status.success() => messages.push(format!("linked {from} -> {to}")),
            // Idempotent, same reasoning as api.rs's create_link: a UI
            // double-click or a retried request must not read as failure.
            Ok(o) if String::from_utf8_lossy(&o.stderr).contains("File exists") => {
                messages.push(format!("{from} -> {to} already linked"))
            }
            Ok(o) => {
                ok = false;
                messages.push(format!("{from} -> {to} failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
            }
            Err(e) => {
                ok = false;
                messages.push(format!("failed to run pw-link: {e}"));
            }
        }
    }
    Json(LinkOpResponse { ok, message: messages.join("; ") })
}

pub async fn unlink(State(pw): State<SharedState>, Json(req): Json<LinkPairRequest>) -> Json<LinkOpResponse> {
    let pairs = matched_port_pairs(&pw, req.source_node_id, req.output_node_id);
    if pairs.is_empty() {
        return Json(LinkOpResponse {
            ok: false,
            message: "no matching source/output node or no matching channel ports".to_string(),
        });
    }
    let mut messages = Vec::with_capacity(pairs.len());
    for (from, to) in &pairs {
        // Always report success for -d: whether the link existed a moment
        // ago (race with the registry snapshot the UI acted on) or was
        // already gone, the end state the caller wants — "not linked" —
        // holds either way.
        let output = tokio::process::Command::new("pw-link").arg("-d").arg(from).arg(to).output().await;
        match output {
            Ok(_) => messages.push(format!("unlinked {from} -> {to}")),
            Err(e) => messages.push(format!("failed to run pw-link -d: {e}")),
        }
    }
    Json(LinkOpResponse { ok: true, message: messages.join("; ") })
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
    let matrix = build_matrix(&pw.lock().unwrap());
    let json = serde_json::to_string(&matrix).unwrap_or_else(|_| "{}".to_string());
    socket.send(Message::Text(json)).await
}
