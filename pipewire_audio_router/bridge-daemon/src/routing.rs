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
//!   output ports (monitor/capture taps) — excluding ports named with that
//!   prefix is what keeps a sink's own monitor tap from being
//!   misclassified as a routable source. Real sources observed so far
//!   (the native AirPlay input `airplay-in`, ad-hoc test nodes) all expose
//!   plain `output_*`/`capture_*` ports, never `monitor_*`.
//!
//! **Channel pairing**: linking a source to an output pairs ports by their
//! name's final `_`-separated suffix (`output_FL` ~ `send_FL` ~
//! `playback_FL` all pair as `FL`) — matches the convention already used
//! by hand in every test script in `tests/`, generalized here instead of
//! being hardcoded per source/output type.

use crate::airplay_source::AIRPLAY_NODE_NAME;
use crate::api::AppState;
use crate::config::{SENDSPIN_DEV_PREFIX, SENDSPIN_NODE_PREFIX};
use crate::locks::LockRecover;
use crate::outputs_store::OutputsStore;
use crate::pw_thread::{LinkSpec, PortInfo, PwCommand, PwCommandSender, RegistryState, SharedState};
use crate::raop::{raop_node_name, RAOP_NODE_PREFIX};
use crate::routing_store::{self, RoutingLink, SharedRouting};
use crate::rtp_source::RTP_SOURCE_NODE_NAME;
use crate::sendspin_discovery::SendspinDevice;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// Recent peak level (0.0–1.0) for the UI meter. Sources only (metered
    /// on-demand while the matrix is watched); `0.0` for outputs and unmetered.
    peak: f32,
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
    airplay_display: Option<&str>,
    meters: &crate::metering::MeterHub,
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
            let has_real_source_port =
                reg.ports.values().any(|p| p.node_id == node.node_id && p.direction == "out" && !p.port_name.starts_with("monitor_"));
            if has_real_source_port {
                let e = present_sources.entry(node.node_name.clone()).or_insert(node.node_id);
                *e = (*e).max(node.node_id);
            }
        }
    }

    // Configured RAOP outputs: node_name -> friendly display name.
    let configured: BTreeMap<String, String> = store.list().iter().map(|o| (raop_node_name(&o.name), o.name.clone())).collect();

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
            let device = devices.get(&name);
            let display_name = configured
                .get(&name)
                .cloned()
                .or_else(|| device.map(|d| d.display_name.clone()))
                .unwrap_or_else(|| output_display_name(&name));
            RoutingNode {
                // Present if live in the graph, or a discovered sendspin device
                // the liveness task still deems online. An offline device stays
                // listed (grayed) until liveness removes it — an mDNS blip no
                // longer makes it vanish.
                present: node_id.is_some() || device.is_some_and(|d| d.present),
                node_id,
                // Devices and offline entries are never manually configured.
                configured: configured.contains_key(&name),
                display_name,
                node_name: name,
                peak: 0.0, // outputs aren't metered
            }
        })
        .collect();

    let mut sources: Vec<RoutingNode> = source_names
        .into_iter()
        .map(|name| {
            let node_id = present_sources.get(&name).copied();
            // The AirPlay source's node is named `airplay-in`; show the
            // user-facing service name instead when we know it.
            let display_name = match airplay_display {
                Some(ap) if name == AIRPLAY_NODE_NAME => ap.to_string(),
                _ => name.clone(),
            };
            let peak = meters.peak(&name);
            RoutingNode { present: node_id.is_some(), node_id, configured: true, display_name, node_name: name, peak }
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
    let airplay = state.sources.lock_recover().airplay_source_name().map(str::to_string);
    let reg = state.pw.lock_recover();
    let store = state.store.lock_recover();
    build_matrix(&reg, &store, &devices, airplay.as_deref(), &state.meters, &intent)
}

/// Present source nodes as `(node_name, node_id)` — the set the meter hub taps
/// while the matrix is being watched.
fn present_source_meters(matrix: &RoutingMatrix) -> Vec<(String, u32)> {
    matrix.sources.iter().filter_map(|s| s.node_id.map(|id| (s.node_name.clone(), id))).collect()
}

/// Every non-monitor output-direction port on `source_node_id` paired with the
/// same-channel input-direction port on `output_node_id`, as `LinkSpec`s
/// (resolved to PipeWire object ids) ready for a `CreateLinks` command. Empty
/// if either node doesn't exist or no channel suffixes match on both sides.
fn matched_port_specs(state: &SharedState, source_node_id: u32, output_node_id: u32) -> Vec<LinkSpec> {
    matched_port_specs_impl(state, source_node_id, output_node_id, false)
}

/// Like `matched_port_specs`, but takes the source side's **monitor** output
/// ports (`monitor_FL`/`monitor_FR`) instead of its normal outputs. Used to
/// wire a null-sink *anchor*'s monitor into a RAOP output (rtp_raop_anchor.rs):
/// a null sink's only outputs are its monitor ports, which the normal matcher
/// deliberately excludes.
fn matched_monitor_port_specs(state: &SharedState, source_node_id: u32, output_node_id: u32) -> Vec<LinkSpec> {
    matched_port_specs_impl(state, source_node_id, output_node_id, true)
}

/// `want_monitor` selects which of the source node's output ports to match:
/// `false` = normal outputs (excluding `monitor_*`), `true` = only `monitor_*`.
fn matched_port_specs_impl(state: &SharedState, source_node_id: u32, output_node_id: u32, want_monitor: bool) -> Vec<LinkSpec> {
    let state = state.lock_recover();
    if !state.nodes.contains_key(&source_node_id) || !state.nodes.contains_key(&output_node_id) {
        return Vec::new();
    }

    let source_ports: Vec<&PortInfo> = state
        .ports
        .values()
        .filter(|p| p.node_id == source_node_id && p.direction == "out" && p.port_name.starts_with("monitor_") == want_monitor)
        .collect();
    let output_ports: Vec<&PortInfo> = state.ports.values().filter(|p| p.node_id == output_node_id && p.direction == "in").collect();

    source_ports
        .iter()
        .filter_map(|sp| {
            let suffix = channel_suffix(&sp.port_name);
            output_ports.iter().find(|op| channel_suffix(&op.port_name) == suffix).map(|op| LinkSpec {
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
pub(crate) fn node_id_for(state: &RegistryState, node_name: &str) -> Option<u32> {
    state.nodes.values().filter(|n| n.node_name == node_name).map(|n| n.node_id).max()
}

/// Whether `name` is a RAOP output node (`raop-out-*`).
pub(crate) fn is_raop_output(name: &str) -> bool {
    name.starts_with(RAOP_NODE_PREFIX)
}

/// Whether a source must be routed to RAOP outputs through a null-sink anchor
/// rather than linked directly. True for the RTP source: `module-rtp-source` is
/// a `node.network` RateMatch *follower* that can't drive a graph cycle, and a
/// RAOP sink can't either, so a direct link is a driverless cycle that stalls
/// the whole component. Driver-capable sources (the AirPlay receive source)
/// return false and link directly. See rtp_raop_anchor.rs and
/// docs/rtp-source-to-raop-routing.md.
pub(crate) fn source_needs_raop_anchor(name: &str) -> bool {
    name == RTP_SOURCE_NODE_NAME
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

/// Like `ensure_link_by_name`, but links `monitor`'s **monitor** output ports
/// (a null sink's only outputs) into `output`'s inputs. Used to feed a RAOP
/// output from the RTP→RAOP anchor's monitor (rtp_raop_anchor.rs). Idempotent.
pub async fn ensure_monitor_link_by_name(pw: &SharedState, pw_cmd: &PwCommandSender, monitor: &str, output: &str) {
    let ids = {
        let st = pw.lock_recover();
        node_id_for(&st, monitor).zip(node_id_for(&st, output))
    };
    let Some((monitor_id, output_id)) = ids else { return };
    let specs = matched_monitor_port_specs(pw, monitor_id, output_id);
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
        // RTP-source → RAOP routes go through a null-sink anchor
        // (rtp_raop_anchor.rs), not a direct link — a direct link is a
        // driverless cycle that stalls the graph. Skip them here.
        if source_needs_raop_anchor(&link.source) && is_raop_output(&link.output) {
            continue;
        }
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
    // Nudge the reconcilers (routing/groups/anchor) so intent changes take
    // effect promptly, not only on the next PipeWire registry event.
    let _ = state.changes.send(());
    // RTP-source → RAOP goes through the null-sink anchor (rtp_raop_anchor.rs),
    // never a direct link (which stalls). The anchor reconciler, woken by the
    // notification above, builds the path.
    if source_needs_raop_anchor(&req.source) && is_raop_output(&req.output) {
        return Json(LinkOpResponse { ok: true, message: "saved; routing via RTP→RAOP anchor".to_string() });
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
    // Wake the reconcilers: for an RTP→RAOP route the live links live on the
    // anchor (not a direct source→output link), so the anchor reconciler must
    // run to tear them (and the anchor) down.
    let _ = state.changes.send(());
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
        st.links.values().filter(|l| l.output_node == source_id && l.input_node == output_id).map(|l| l.link_id).collect()
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
    // A watching client turns on source peak metering (turned off when the last
    // client leaves — see MeterHub). reconcile_sources on each registry change
    // keeps the tapped set matching the present sources.
    state.meters.watch();

    // Peak levels change continuously, but the registry `changes` channel only
    // fires on graph changes — so also push a fresh snapshot on a timer while
    // watched, giving the UI a live meter. This cost is only paid while a
    // client is connected.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let matrix = build_snapshot(&state);
    state.meters.reconcile_sources(&present_source_meters(&matrix));
    if send_matrix(&mut socket, &matrix).await.is_err() {
        state.meters.unwatch();
        return;
    }
    loop {
        tokio::select! {
            changed = changes.recv() => {
                match changed {
                    // On any graph change, rebuild + re-tap the current sources.
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let matrix = build_snapshot(&state);
                        state.meters.reconcile_sources(&present_source_meters(&matrix));
                        if send_matrix(&mut socket, &matrix).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // Live peak refresh (levels move without graph changes).
            _ = tick.tick() => {
                let matrix = build_snapshot(&state);
                if send_matrix(&mut socket, &matrix).await.is_err() {
                    break;
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
    state.meters.unwatch();
}

async fn send_matrix(socket: &mut WebSocket, matrix: &RoutingMatrix) -> Result<(), axum::Error> {
    let json = serde_json::to_string(matrix).unwrap_or_else(|_| "{}".to_string());
    socket.send(Message::Text(json)).await
}
