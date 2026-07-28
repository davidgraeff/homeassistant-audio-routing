//! Manual routing UI backend: a source×output matrix —
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

use crate::api::AppState;
use crate::config::{AP2_DEV_PREFIX, PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX, SENDSPIN_NODE_PREFIX};
use crate::locks::LockRecover;
use crate::pw_thread::{LinkSpec, PortInfo, PwCommand, PwCommandSender, RegistryState, SharedState};
use crate::routing_store::{self, RoutingLink, SharedRouting};
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
    /// Current volume (0.0–1.0) for outputs whose volume the daemon tracks
    /// out-of-band. Presently sendspin devices only — their in-band volume
    /// (sendspin_volume.rs) is pushed here so the UI slider syncs live over this
    /// WebSocket (including a physical volume change the device reports). `None`
    /// for sources/offline entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<f32>,
    /// Current mute state for outputs whose mute the daemon tracks out-of-band
    /// (sendspin + AP2) — pushed live over this WebSocket like `volume`. `None`
    /// for sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    muted: Option<bool>,
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
    node_name.starts_with(SENDSPIN_NODE_PREFIX)
}

pub(crate) fn output_display_name(node_name: &str) -> String {
    for prefix in [SENDSPIN_NODE_PREFIX, SENDSPIN_DEV_PREFIX, AP2_DEV_PREFIX, PWSINK_DEV_PREFIX] {
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
    devices: &BTreeMap<String, SendspinDevice>,
    ap2_devices: &BTreeMap<String, crate::ap2_discovery::Ap2Device>,
    pw_targets: &BTreeMap<String, crate::pw_target_discovery::PwTarget>,
    source_labels: &std::collections::HashMap<String, String>,
    meters: &crate::metering::MeterHub,
    intent: &[RoutingLink],
    sendspin_volumes: &std::collections::HashMap<String, u8>,
    sendspin_mutes: &std::collections::HashMap<String, bool>,
    ap2_volumes: &std::collections::HashMap<String, f32>,
    ap2_mutes: &std::collections::HashMap<String, bool>,
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

    // Every output is now virtual + auto-discovered (sendspin + AP2 devices) —
    // audio reaches them via a group sink (sync_group.rs), not a live node here.

    // Union of every output/source name to show: present ∪ discovered devices ∪
    // intent.
    let mut output_names: BTreeSet<String> = present_outputs.keys().cloned().collect();
    output_names.extend(devices.keys().cloned());
    output_names.extend(ap2_devices.keys().cloned());
    output_names.extend(pw_targets.keys().cloned());
    output_names.extend(intent.iter().map(|l| l.output.clone()));

    let mut source_names: BTreeSet<String> = present_sources.keys().cloned().collect();
    source_names.extend(intent.iter().map(|l| l.source.clone()));

    let mut outputs: Vec<RoutingNode> = output_names
        .into_iter()
        .map(|name| {
            let node_id = present_outputs.get(&name).copied();
            let device = devices.get(&name);
            let ap2 = ap2_devices.get(&name);
            let pwt = pw_targets.get(&name);
            let display_name = device
                .map(|d| d.display_name.clone())
                .or_else(|| ap2.map(|d| d.display_name.clone()))
                .or_else(|| pwt.map(|t| t.display_name.clone()))
                .unwrap_or_else(|| output_display_name(&name));
            RoutingNode {
                // Present if live in the graph, or a discovered sendspin/AP2/pw-sink
                // endpoint the liveness task still deems online. An offline endpoint
                // stays listed (grayed) until liveness removes it — an mDNS blip no
                // longer makes it vanish.
                present: node_id.is_some()
                    || device.is_some_and(|d| d.present)
                    || ap2.is_some_and(|d| d.present)
                    || pwt.is_some_and(|t| t.present),
                node_id,
                // Every output is now auto-discovered (sendspin + AP2); nothing is
                // manually configured anymore (the RAOP store is gone).
                configured: false,
                display_name,
                // Virtual outputs (sendspin + AP2) carry their in-band volume/mute
                // here so the UI syncs live over the routing WS.
                volume: if name.starts_with(SENDSPIN_DEV_PREFIX) {
                    Some(sendspin_volumes.get(&name).map_or(1.0, |v| *v as f32 / 100.0))
                } else if name.starts_with(AP2_DEV_PREFIX) {
                    // AP2 volume is device-authoritative: `None` (unknown) when we
                    // haven't read it from the receiver and the user hasn't set it —
                    // the UI then shows no level rather than a fabricated 100 %.
                    ap2_volumes.get(&name).copied()
                } else {
                    None
                },
                muted: if name.starts_with(SENDSPIN_DEV_PREFIX) {
                    Some(sendspin_mutes.get(&name).copied().unwrap_or(false))
                } else if name.starts_with(AP2_DEV_PREFIX) {
                    Some(ap2_mutes.get(&name).copied().unwrap_or(false))
                } else {
                    None
                },
                node_name: name,
                peak: 0.0, // outputs aren't metered
            }
        })
        .collect();

    let mut sources: Vec<RoutingNode> = source_names
        .into_iter()
        .map(|name| {
            let node_id = present_sources.get(&name).copied();
            // Show the source's user-facing label (from the source store) when we
            // know it; otherwise fall back to the raw node name. Works for every
            // source instance, not just a fixed-name AirPlay/RTP source.
            let display_name = source_labels.get(&name).cloned().unwrap_or_else(|| name.clone());
            let peak = meters.peak(&name);
            RoutingNode { present: node_id.is_some(), node_id, configured: true, display_name, node_name: name, peak, volume: None, muted: None }
        })
        .collect();

    outputs.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    sources.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    let mut links = intent.to_vec();
    links.sort();

    RoutingMatrix { sources, outputs, links }
}

/// Snapshot the matrix from shared state (locks registry + routing intent).
/// Lock order is fixed here — routing first (released before the others), then
/// registry — to stay deadlock-free.
async fn build_snapshot(state: &AppState) -> RoutingMatrix {
    // Snapshot the sendspin volumes up front, before any sync (std::sync::Mutex)
    // locks are taken — the control is an async mutex and its guard must not be
    // held across the sync section below (and never across an await).
    let (sendspin_volumes, sendspin_mutes) = {
        let c = state.sendspin_control.lock().await;
        (c.volumes(), c.mutes())
    };
    let (ap2_volumes, ap2_mutes) = {
        let c = state.ap2_control.lock().await;
        (c.volumes(), c.mutes())
    };
    let intent = routing_store::snapshot(&state.routing);
    let devices = state.sendspin_devices.lock_recover().clone();
    let ap2_devices = state.ap2_devices.lock_recover().clone();
    // Per-source display label, keyed by node name, for every configured source
    // instance (AirPlay or RTP) — not just a fixed-name one.
    let pw_targets = state.pw_targets.lock_recover().clone();
    let source_labels: std::collections::HashMap<String, String> = {
        let sources = state.sources.lock_recover();
        sources.list().into_iter().map(|entry| (entry.node_name(), entry.label.clone())).collect()
    };
    let reg = state.pw.lock_recover();
    build_matrix(&reg, &devices, &ap2_devices, &pw_targets, &source_labels, &state.meters, &intent, &sendspin_volumes, &sendspin_mutes, &ap2_volumes, &ap2_mutes)
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

/// Like [`ensure_link_by_name`] but links the `source` node's **monitor** output
/// ports (a sink's `monitor_*` tap) into `output`'s inputs. Used to fan a group
/// anchor's monitor into a follower sink (e.g. an rtp-sink for the pw-sink
/// backend) — the anchor is the steady QUANT-1024 driver, the follower pulls
/// from its monitor at its own rate. Idempotency is handled by PipeWire (a
/// duplicate link is rejected); callers re-invoke freely.
pub async fn ensure_monitor_link_by_name(pw: &SharedState, pw_cmd: &PwCommandSender, source: &str, output: &str) {
    let ids = {
        let st = pw.lock_recover();
        node_id_for(&st, source).zip(node_id_for(&st, output))
    };
    let Some((source_id, output_id)) = ids else { return };
    let specs = matched_port_specs_impl(pw, source_id, output_id, true);
    if specs.is_empty() {
        return;
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::CreateLinks { specs, reply: reply_tx }).is_err() {
        return;
    }
    let _ = reply_rx.await;
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

/// All live `(node_id, node_name)` whose name contains `substr`, newest first.
/// Used to find reactively-created nodes with auto-generated names (e.g.
/// `module-rtp-session`'s `rtp_session.<sess>.<host>.local.ipv4` sink, which
/// only appears once a receiver connects — pw_sink).
pub fn nodes_matching(state: &SharedState, substr: &str) -> Vec<(u32, String)> {
    let st = state.lock_recover();
    let mut out: Vec<(u32, String)> = st
        .nodes
        .values()
        .filter(|n| n.node_name.contains(substr))
        .map(|n| (n.node_id, n.node_name.clone()))
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

/// The set of sources feeding `output` in the intent (unique, sorted). Shared
/// with sync_group.rs, which keys sync groups by this source-set.
pub(crate) fn source_set_of<'a>(intent: &'a [RoutingLink], output: &str) -> std::collections::BTreeSet<&'a str> {
    intent.iter().filter(|l| l.output == output).map(|l| l.source.as_str()).collect()
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
///
/// After the RAOP output path was removed, every routable output is *virtual*
/// (sendspin/AP2 devices with no live PipeWire node), so `node_id_for(output)`
/// returns `None` and this loop no-ops for them — their audio path is built by
/// sync_group.rs from a group anchor, not by a direct link here. The loop is
/// kept (rather than deleted) so a future real-node output would still be
/// direct-linked, and it stays a cheap no-op for the current output kinds.
pub async fn reconcile(pw: &SharedState, pw_cmd: &PwCommandSender, routing: &SharedRouting) {
    let intent = routing_store::snapshot(routing);
    for link in &intent {
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
    Json(build_snapshot(&state).await)
}

pub async fn link(State(state): State<AppState>, Json(req): Json<LinkPairRequest>) -> Json<LinkOpResponse> {
    // Human-initiated (routing-graph / API) — logged distinctly so a person's
    // actions can be told apart from stack-driven churn (reconcile, discovery,
    // source cycling) when reading logs. Grep `USER ACTION`.
    tracing::info!("USER ACTION: link '{}' → '{}' (routing graph)", req.source, req.output);
    // Persist the desired route first; it's the source of truth and reconcile()
    // (re)applies it whenever both endpoints are present.
    if let Err(e) = state.routing.lock_recover().add(&req.source, &req.output) {
        return Json(LinkOpResponse { ok: false, message: format!("failed to persist routing intent: {e}") });
    }
    // Nudge the reconcilers (routing/groups/anchor) so intent changes take
    // effect promptly, not only on the next PipeWire registry event.
    let _ = state.changes.send(());
    // Every output is now virtual (sendspin/AP2): its audio path is built by
    // sync_group.rs from a group anchor, not a direct link here. If the output
    // has no live PipeWire node (the normal case), there's nothing to link now —
    // the reconcilers woken above build the path.
    let ids = {
        let st = state.pw.lock_recover();
        node_id_for(&st, &req.source).zip(node_id_for(&st, &req.output))
    };
    let Some((source_id, output_id)) = ids else {
        return Json(LinkOpResponse { ok: true, message: "saved; routing via sync group".to_string() });
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
    // Human-initiated (routing-graph "remove link") — logged distinctly (grep
    // `USER ACTION`) so it's not confused with stack-driven teardown.
    tracing::info!("USER ACTION: unlink '{}' → '{}' (routing graph)", req.source, req.output);
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
    tracing::info!("USER ACTION: forget entity '{}' (routing graph)", node_name);
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

    let matrix = build_snapshot(&state).await;
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
                        let matrix = build_snapshot(&state).await;
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
                let matrix = build_snapshot(&state).await;
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
    socket.send(Message::Text(json.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay_source::AIRPLAY_NODE_NAME;

    fn link(source: &str, output: &str) -> RoutingLink {
        RoutingLink { source: source.to_string(), output: output.to_string() }
    }

    #[test]
    fn source_set_of_collects_unique_sorted_sources() {
        let intent = vec![
            link(AIRPLAY_NODE_NAME, "sendspin-dev-kitchen"),
            link("other-source", "sendspin-dev-kitchen"),
            link(AIRPLAY_NODE_NAME, "ap2-dev-dusche"),
        ];
        let kitchen = source_set_of(&intent, "sendspin-dev-kitchen");
        assert!(kitchen.contains(AIRPLAY_NODE_NAME) && kitchen.contains("other-source") && kitchen.len() == 2);
        let dusche = source_set_of(&intent, "ap2-dev-dusche");
        assert_eq!(dusche.len(), 1);
    }
}
