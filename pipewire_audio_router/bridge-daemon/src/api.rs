//! REST API: health check, live PipeWire registry state, and manual link
//! creation

use crate::airplay_clients::AirplayClientStore;
use crate::ap2_discovery::SharedAp2Devices;
use crate::ap2_ptp::SharedAp2Ptp;
use crate::config::{AP2_DEV_PREFIX, PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX};
use crate::locks::LockRecover;
use crate::outputs_store::{OutputState, SharedOutputs};
use crate::pw_thread::{ChangeNotifier, LinkSpec, PwCommand, PwCommandSender, SharedState};
use crate::routing;
use crate::routing_store::SharedRouting;
use crate::rtp_source::{DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_PORT, DEFAULT_RTP_RATE, DEFAULT_RTP_SOURCE_ADDR};
use crate::sendspin_discovery::SharedSendspinDevices;
use crate::settings_store::SharedSettings;
use crate::sources_store::{AirplaySourceConfig, RtpSourceConfig, SourceConfig, SourceEntry, SourceKind, SourcesStore};
use airplay_core::features::Features;
use axum::{
    body::{Body, Bytes},
    extract::{Extension, FromRef, Path, Query, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Runtime config for the AirPlay and RTP sources.
pub type SharedSources = Arc<Mutex<SourcesStore>>;

/// The running AirPlay-receive sources (airplay_source.rs), keyed by source id —
/// each a native embedded RAOP server feeding its own PipeWire source node.
/// Phase 4: multiple concurrent receivers, reconciled against the store.
pub type SharedAirplay = crate::airplay_source::SharedAirplayMap;

/// Shared axum state: the live PipeWire registry snapshot, the routing UI's
/// change-notification channel (routing.rs), and the command sender for runtime
/// module load/unload (pw_thread.rs). Existing handlers extract just the piece
/// they need via `FromRef` — they don't need to know this type grew more fields.
#[derive(Clone)]
pub struct AppState {
    pub pw: SharedState,
    pub changes: ChangeNotifier,
    pub pw_cmd: PwCommandSender,
    pub sources: SharedSources,
    pub airplay: SharedAirplay,
    /// Remembered AirPlay senders (airplay_clients.rs), per-receiver — the
    /// backing store for each source's connection list + ban/priority controls.
    /// Per-source views are taken via `.registry(id)`. Anti-takeover is now
    /// per-receiver too (each running `AirplayHandle` owns its flag), so there is
    /// no process-wide flag here anymore.
    pub airplay_clients: AirplayClientStore,
    /// Per-source now-playing metadata (now_playing.rs) — what each input is
    /// currently playing, from whichever producer can say: the AirPlay receiver's
    /// DMAP callbacks locally, a Pi reporter over `/api/sources/{id}/now_playing`
    /// remotely. Read into the routing socket's `now_playing` frame.
    pub now_playing: crate::now_playing::NowPlayingStore,
    /// On-demand source peak meters (metering.rs); taps live only while a
    /// routing-matrix WS client is connected.
    pub meters: crate::metering::SharedMeters,
    /// Per-node xrun counts from the PipeWire profiler (profiler.rs), written by
    /// the PipeWire thread while profiling is armed and read into the routing
    /// snapshot. Empty when the routing UI is closed.
    pub xruns: crate::profiler::SharedXruns,
    /// Count of open routing-matrix WebSockets. The first arms profiling
    /// (`PwCommand::SetProfiling(true)`), the last disarms it — same "pay only
    /// while watched" gating as the peak meters.
    pub profiler_watchers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Live mDNS-discovered sendspin devices (sendspin_discovery.rs), surfaced
    /// as virtual routing outputs.
    pub sendspin_devices: SharedSendspinDevices,
    /// Live mDNS-discovered AirPlay-2 receivers (ap2_discovery.rs), surfaced as
    /// virtual routing outputs (`ap2-dev-*`). The RAOP-output replacement.
    pub ap2_devices: SharedAp2Devices,
    /// Paired receiver agents (pwsink_agent.rs) — the source of truth for pw-sink
    /// outputs (there is no mDNS registry behind them: `pw_target_discovery` is a
    /// diagnostic that nothing here reads), their volume/mute control channel, and
    /// the pairing queue.
    pub agents: crate::pwsink_agent::SharedAgents,
    /// Live mDNS-discovered Bluetooth→RTP bridges (bt_bridge_discovery.rs).
    /// Unlike the other discoveries these are *senders*, not outputs: they build
    /// no audio path, they annotate an RTP source with which bridge feeds it and
    /// offer that bridge's diagnostics page. Unconfigured ones are offered for
    /// adoption on the Sources tab.
    pub bt_bridges: crate::bt_bridge_discovery::SharedBtBridges,
    /// The daemon's single host-global AirPlay-2 PTP grandmaster (ap2_ptp.rs),
    /// reused by the AP2 tone spike (ap2_spike.rs) so it shares 319/320 rather
    /// than double-binding.
    pub ap2_ptp: SharedAp2Ptp,
    pub sendspin_control: crate::sendspin_volume::SharedSendspinControl,
    pub ap2_control: crate::ap2_volume::SharedAp2Control,
    /// Persistent routing intent (routing_store.rs): links by stable node
    /// name, reconciled onto the live graph so routing survives node reloads
    /// and device disappearance/reappearance.
    pub routing: SharedRouting,
    /// Which discovered outputs the user adopted (outputs_store.rs). Discovery
    /// only *offers* a device; until it's added here it stays out of the routing
    /// matrix, out of Home Assistant and out of the group reconciler.
    pub outputs: SharedOutputs,
    /// Persistent sync/latency tuning (sync_settings.rs): the group presentation
    /// lead + per-sendspin-device static delays.
    pub sync_settings: crate::sync_settings::SharedSyncSettings,
    /// General app settings (settings_store.rs): announce default duck, mDNS
    /// discovery on/off.
    pub settings: SharedSettings,
    /// Runtime mDNS on/off, driven by the discovery flag above.
    pub discovery: crate::discovery_supervisor::DiscoverySupervisor,
    /// Latency-alignment session manager (calibrate.rs) for the alignment panel.
    pub align: crate::calibrate::AlignManager,
    /// Live sync-group layout (sync_group.rs) — used to restart a group's
    /// sendspin stream when a static-delay change needs it to take effect.
    pub groups: crate::sync_group::SharedGroups,
    /// Named music/announcement groups (groups_store.rs) — the MG/AG data model.
    pub groups_config: crate::groups_store::SharedGroupsStore,
    /// Add-on version string (main.rs `addon_version()`), for `/api/status`.
    pub version: String,
    /// Process start instant, for the `/api/status` uptime.
    pub started: std::time::Instant,
}

impl FromRef<AppState> for SharedState {
    fn from_ref(state: &AppState) -> SharedState {
        state.pw.clone()
    }
}

impl FromRef<AppState> for ChangeNotifier {
    fn from_ref(state: &AppState) -> ChangeNotifier {
        state.changes.clone()
    }
}

// Internal wiring, not a public API surface with a stability concern — each
// param is a distinct shared handle `AppState` needs, not something a struct
// wrapper would make clearer to call sites (main.rs's one call site).
#[allow(clippy::too_many_arguments)]
pub fn router(
    pw_state: SharedState,
    changes: ChangeNotifier,
    pw_cmd: PwCommandSender,
    sources: SharedSources,
    airplay: SharedAirplay,
    airplay_clients: AirplayClientStore,
    now_playing: crate::now_playing::NowPlayingStore,
    meters: crate::metering::SharedMeters,
    xruns: crate::profiler::SharedXruns,
    sendspin_devices: SharedSendspinDevices,
    ap2_devices: SharedAp2Devices,
    agents: crate::pwsink_agent::SharedAgents,
    bt_bridges: crate::bt_bridge_discovery::SharedBtBridges,
    ap2_ptp: SharedAp2Ptp,
    routing: SharedRouting,
    outputs: SharedOutputs,
    sendspin_control: crate::sendspin_volume::SharedSendspinControl,
    ap2_control: crate::ap2_volume::SharedAp2Control,
    sync_settings: crate::sync_settings::SharedSyncSettings,
    settings: SharedSettings,
    discovery: crate::discovery_supervisor::DiscoverySupervisor,
    align: crate::calibrate::AlignManager,
    groups: crate::sync_group::SharedGroups,
    groups_config: crate::groups_store::SharedGroupsStore,
    version: String,
    started: std::time::Instant,
    static_dir: PathBuf,
) -> Router {
    let state = AppState {
        pw: pw_state,
        changes,
        pw_cmd,
        sources,
        airplay,
        airplay_clients,
        now_playing,
        meters,
        xruns,
        profiler_watchers: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        sendspin_devices,
        ap2_devices,
        agents,
        bt_bridges,
        ap2_ptp,
        routing,
        outputs,
        sendspin_control,
        ap2_control,
        sync_settings,
        settings,
        discovery,
        align,
        groups,
        groups_config,
        version,
        started,
    };
    Router::new()
        .route("/health", get(health))
        .route("/api/nodes", get(list_nodes))
        .route("/api/links", post(create_link))
        .route("/api/outputs", get(list_outputs))
        // Discovery only *offers* a device; these three are how the user decides.
        // `/discovered` is a static sibling of the `{node_name}` remove route below;
        // matchit prefers the static segment, and registering both is accepted (see
        // `discovered_listing_and_remove_routes_coexist` — a route conflict would
        // panic here at startup, where nothing would catch it).
        .route("/api/outputs/discovered", get(list_discovered_outputs))
        .route("/api/outputs/{node_name}", delete(remove_output))
        .route("/api/outputs/{node_name}/adopt", post(adopt_output))
        .route("/api/outputs/{node_name}/ignore", post(ignore_output))
        // pw-sink only: the removal that also revokes the pairing (plan §8).
        .route("/api/outputs/{node_name}/unpair", post(unpair_output))
        .route("/api/outputs/{node_name}/name", put(set_output_name))
        .route("/api/outputs/{node_name}/latency", put(set_output_latency))
        .route("/api/outputs/{node_name}/ap2-rate", put(set_ap2_rate_mode))
        .route("/api/outputs/{node_name}/sendspin-codec", put(set_sendspin_codec))
        // Per-receiver AirPlay client/policy routes. `{id}` is the source id;
        // each receiver has its own client list + anti-takeover flag.
        .route("/api/sources/{id}/clients", get(list_source_clients))
        .route("/api/sources/{id}/clients/forget", post(forget_source_client))
        .route("/api/sources/{id}/clients/ban", post(ban_source_client))
        .route("/api/sources/{id}/clients/priority", post(set_source_client_priority))
        .route("/api/sources/{id}/clients/disconnect", post(disconnect_source_client))
        .route("/api/sources/{id}/policy", put(set_source_policy))
        // Per-source now-playing metadata (now_playing.rs). Keyed by source NODE
        // NAME, not by source id: that is the key the routing matrix, the routing
        // intent and the HA integration all already share, and it is what the
        // `now_playing` WebSocket frame is keyed by — so a consumer never has to
        // hold both keys. `report` is the self-identifying entry point for a
        // remote producer that knows its RTP port but not our source ids.
        .route("/api/now_playing", get(list_now_playing))
        .route("/api/now_playing/report", post(report_now_playing))
        .route("/api/now_playing/{node_name}", get(get_now_playing).put(put_now_playing).delete(clear_now_playing))
        .route("/api/now_playing/{node_name}/artwork", get(get_now_playing_artwork))
        // Multi-source collection CRUD — the sole source-management API.
        .route("/api/sources", get(list_sources).post(create_source))
        .route("/api/sources/{id}", get(get_source).put(update_source).delete(delete_source))
        .route("/api/sendspin/volumes", get(get_sendspin_volumes))
        .route("/api/sendspin/volume", put(set_sendspin_volume))
        .route("/api/sendspin/mute", put(set_sendspin_mute))
        .route("/api/sendspin/clear", post(clear_sendspin_stream))
        .route("/api/ap2/volume", put(set_ap2_volume))
        .route("/api/ap2/mute", put(set_ap2_mute))
        .route("/api/sendspin/delays", get(get_sendspin_delays))
        .route("/api/sendspin/delay", put(set_sendspin_delay_handler))
        .route("/api/sync/settings", get(get_sync_settings).put(set_sync_settings))
        .route("/api/settings", get(get_settings).put(set_settings))
        .route("/api/status", get(get_status))
        .route("/api/spike/per-device", post(spike_per_device_start).delete(spike_per_device_stop))
        .route("/api/spike/multi-device", post(spike_multi_device_start).delete(spike_per_device_stop))
        .route("/api/spike/overlay", post(spike_overlay_start).delete(spike_overlay_stop))
        .route("/api/spike/ap2", post(spike_ap2_start).delete(spike_ap2_stop))
        .route("/api/spike/pw-sink", post(spike_pwsink_start).delete(spike_pwsink_stop))
        .route("/api/announce", post(ag_announce))
        .route("/api/groups/music", get(list_music_groups).post(create_music_group))
        .route("/api/groups/music/{id}", put(update_music_group).delete(delete_music_group))
        .route("/api/groups/music/{id}/route", post(route_music_group).delete(unroute_music_group))
        .route("/api/groups/announcement", get(list_announcement_groups).post(create_announcement_group))
        .route("/api/groups/announcement/{id}", put(update_announcement_group).delete(delete_announcement_group))
        .route("/api/align/groups", get(align_groups))
        .route("/api/align", get(align_status).delete(align_stop))
        .route("/api/align/start", post(align_start))
        .route("/api/align/select", post(align_select))
        .route("/api/align/volume", post(align_volume))
        .route("/api/routing", get(routing::get_routing))
        .route("/api/routing/link", post(routing::link))
        .route("/api/routing/unlink", post(routing::unlink))
        .route("/api/routing/entity/{node_name}", delete(routing::forget_entity))
        .route("/api/routing/ws", get(routing::routing_ws))
        // The receiver agents' own control plane: the agent dials in here.
        // Pairing decisions are *output* operations (`/adopt`, `/ignore`, `/unpair`)
        // — a host asking to pair is a discovered output, so it is decided where
        // every other output is. This listing is left for diagnostics.
        .route("/api/agent/ws", get(crate::pwsink_agent::agent_ws))
        .route("/api/agents", get(get_agents))
        .route("/api/pwsink/volume", put(set_pwsink_volume))
        .route("/api/pwsink/mute", put(set_pwsink_mute))
        // Everything else (`/`, `/assets/*`, favicon, …) is the built Svelte SPA,
        // read into memory ONCE at startup (below) and served from RAM. This
        // deliberately does NOT use `ServeDir`: the add-on's `/data` lives on a USB
        // stick whose filesystem gets slow (and slower as it fills), and a per-
        // request `tokio::fs` read there — with no read timeout — stalls the
        // blocking pool and the UI won't load. In-RAM serving does one boot-time
        // read, then never touches the disk per request.
        .fallback(static_fallback)
        .layer(Extension(Arc::new(StaticAssets::load(&static_dir))))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// The built web UI, read into memory at startup and served from RAM (see the
/// `fallback` note in `router`). A UI redeploy restarts the daemon, which reloads.
struct StaticAssets {
    /// URL-path (no leading `/`, e.g. `assets/index-abc.js`) → file.
    files: HashMap<String, StaticFile>,
    /// `index.html`, cloned out for the SPA fallback (client-side routes → index).
    index: Option<StaticFile>,
}

#[derive(Clone)]
struct StaticFile {
    body: Bytes, // ref-counted: cheap per-request clone, no re-read
    content_type: &'static str,
}

impl StaticAssets {
    fn load(dir: &FsPath) -> Self {
        let mut files = HashMap::new();
        Self::walk(dir, dir, &mut files);
        let index = files.get("index.html").cloned();
        tracing::info!("web UI: loaded {} file(s) into memory from {}", files.len(), dir.display());
        if index.is_none() {
            tracing::warn!("web UI: no index.html under {} — the UI won't serve", dir.display());
        }
        Self { files, index }
    }

    fn walk(root: &FsPath, cur: &FsPath, files: &mut HashMap<String, StaticFile>) {
        let Ok(rd) = std::fs::read_dir(cur) else { return };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                Self::walk(root, &p, files);
            } else if let (Ok(bytes), Ok(rel)) = (std::fs::read(&p), p.strip_prefix(root)) {
                let key = rel.to_string_lossy().replace('\\', "/");
                let content_type = content_type_for(&key);
                files.insert(key, StaticFile { body: Bytes::from(bytes), content_type });
            }
        }
    }
}

/// Minimal extension→MIME map for the assets Vite emits (no `mime_guess` dep).
fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn static_response(f: &StaticFile, key: &str) -> Response {
    // Content-hashed assets are immutable (cache a year); the entrypoint HTML must
    // never be cached, or a stale index pins old asset URLs after a redeploy.
    let cache = if key.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    Response::builder()
        .header(header::CONTENT_TYPE, f.content_type)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(f.body.clone()))
        .expect("static response builds")
}

/// Serve the SPA from RAM. Exact file if present; a missing `/assets/*` is a real
/// 404; any other unknown path falls back to `index.html` (client-side routing).
async fn static_fallback(Extension(assets): Extension<Arc<StaticAssets>>, uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let key = if raw.is_empty() { "index.html" } else { raw };
    if let Some(f) = assets.files.get(key) {
        return static_response(f, key);
    }
    if key.starts_with("assets/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match &assets.index {
        Some(f) => static_response(f, "index.html"),
        None => (StatusCode::NOT_FOUND, "web UI not built").into_response(),
    }
}

#[derive(Serialize)]
struct NodesResponse {
    nodes: Vec<crate::pw_thread::NodeInfo>,
    ports: Vec<crate::pw_thread::PortInfo>,
}

async fn list_nodes(State(pw_state): State<SharedState>) -> Json<NodesResponse> {
    let state = pw_state.lock_recover();
    Json(NodesResponse { nodes: state.nodes.values().cloned().collect(), ports: state.ports.values().cloned().collect() })
}

/// Links two ports by their exact PipeWire port names (e.g.
/// `"airplay-in:output_FL"`), one call per channel — the
/// caller (eventually the routing UI / HA integration, for now this
/// project's own test scripts) is responsible for pairing FL/FR etc.
///
/// Created natively via `Core::create_object` on the PipeWire thread (see
/// pw_thread.rs) — the port names are resolved to object ids against the live
/// registry here, then handed over as a `CreateLinks` command.
#[derive(Deserialize)]
struct CreateLinkRequest {
    from_port: String,
    to_port: String,
}

#[derive(Serialize)]
struct CreateLinkResponse {
    ok: bool,
    message: String,
}

/// Resolves a full `"node.name:port.name"` string to its `(node_id, port_id)`
/// in the live registry, or `None` if either isn't present. Splits on the last
/// `:` so a node name containing `:` still resolves (port names never do).
fn resolve_port(pw: &SharedState, full_name: &str) -> Option<(u32, u32)> {
    let (node_name, port_name) = full_name.rsplit_once(':')?;
    let state = pw.lock_recover();
    let node_id = state.nodes.values().find(|n| n.node_name == node_name).map(|n| n.node_id)?;
    let port_id = state.ports.values().find(|p| p.node_id == node_id && p.port_name == port_name).map(|p| p.port_id)?;
    Some((node_id, port_id))
}

async fn create_link(State(app): State<AppState>, Json(req): Json<CreateLinkRequest>) -> (StatusCode, Json<CreateLinkResponse>) {
    let Some((out_node, out_port)) = resolve_port(&app.pw, &req.from_port) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(CreateLinkResponse { ok: false, message: format!("unknown output port: {}", req.from_port) }),
        );
    };
    let Some((in_node, in_port)) = resolve_port(&app.pw, &req.to_port) else {
        return (StatusCode::BAD_REQUEST, Json(CreateLinkResponse { ok: false, message: format!("unknown input port: {}", req.to_port) }));
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = PwCommand::CreateLinks { specs: vec![LinkSpec { out_node, out_port, in_node, in_port }], reply: reply_tx };
    if app.pw_cmd.send(cmd).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(CreateLinkResponse { ok: false, message: "pipewire thread unavailable".to_string() }),
        );
    }
    match reply_rx.await {
        Ok(Ok(message)) => (StatusCode::OK, Json(CreateLinkResponse { ok: true, message })),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, Json(CreateLinkResponse { ok: false, message })),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(CreateLinkResponse { ok: false, message: "pipewire thread dropped the request".to_string() }),
        ),
    }
}

// ---- Outputs listing ------------------------------------------------------
//
// Every output is now a *virtual*, auto-discovered device (sendspin `sendspin-dev-*`
// or AirPlay-2 `ap2-dev-*`) — there is no manual output store and no runtime
// module load/unload for outputs (the AirPlay-1/RAOP output path was removed).

/// An output for the Outputs tab, in both origins the UI shows:
/// - **discovered**: present via mDNS — `present: true`, `configured: false`.
/// - **offline**: adopted (or referenced by saved routing intent) but not
///   currently discovered — `present: false` (shown grayed; re-linked when it
///   returns).
///
/// The same shape serves both listings: `GET /api/outputs` (adopted only — the
/// system's actual outputs, which is what the routing matrix, the group editors
/// and the HA integration mean by "output") and `GET /api/outputs/discovered`
/// (everything found but not adopted). `state` says which bucket an entry is in.
/// Decoded AirPlay-2 capability flags (from the `features` TXT bitmask), surfaced
/// in `/api/outputs` for the Diagnostics capability card. `raw` is the canonical
/// `0xLOWER,0xUPPER` string for copy/paste + cross-referencing.
#[derive(Serialize)]
struct Ap2FeaturesInfo {
    raw: String,
    /// bit 41 — PTP timing supported.
    ptp: bool,
    /// bit 40 — buffered-audio mode supported (implies PTP is mandatory in that mode).
    buffered_audio: bool,
    /// bit 48 — HomeKit transient pairing (how we connect, PIN 3939).
    transient_pairing: bool,
}

#[derive(Serialize)]
pub(crate) struct OutputInfo {
    node_name: String,
    name: String,
    /// Is `name` the user's own (a rename), rather than the one discovery
    /// reported? Only then is there anything for a UI's "use the discovered
    /// name again" control to clear, so it can offer that honestly instead of
    /// showing a button that does nothing.
    renamed: bool,
    /// `"sendspin"` or `"airplay2"` — for the Type column / badge.
    kind: &'static str,
    /// Node/device is live right now.
    present: bool,
    /// Always `false` now that every output is mDNS auto-discovered (kept for
    /// the API shape / a possible future manually-added output kind).
    configured: bool,
    /// The user's verdict on this discovered device (outputs_store.rs):
    /// `"adopted"` (a real output), `"discovered"` (found, awaiting a decision)
    /// or `"ignored"` (dismissed). Only `"adopted"` outputs are routable and
    /// exposed to Home Assistant.
    state: &'static str,
    /// Connection details (from the mDNS-resolved address).
    ip: Option<String>,
    port: Option<u16>,
    encryption: Option<String>,
    /// Per-output latency override in ms; `None` = the type's built-in default
    /// (1500 ms). For AirPlay-2 it's the render delay (ap2_server.rs). Not
    /// meaningful for sendspin (uses a separate static-delay knob).
    latency_ms: Option<u16>,
    /// AirPlay-2 only: PTP-lock health. `Some(true)` = the receiver is currently
    /// returning gPTP to our grandmaster (heard recently); `Some(false)` = registered
    /// but not exchanging gPTP; `None` = not an AP2 output (or PTP not started). NOTE:
    /// since we stream realtime ALAC (type 96), a single receiver renders fine WITHOUT
    /// an active lock (it free-runs off the PT=87 anchors) — a lock only matters for
    /// multi-room drift. So `false` is only alarming when `ptp_relevant` is true; the
    /// UI badge keys off both.
    #[serde(skip_serializing_if = "Option::is_none")]
    ptp_locked: Option<bool>,
    /// AirPlay-2 only: seconds since the last gPTP packet from the receiver (lock age);
    /// `None` if never seen / not AP2. Small = healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    ptp_lock_age_s: Option<u64>,
    /// AirPlay-2 only: does the receiver *advertise* PTP support (features bit 41)?
    /// `None` if not AP2 or features weren't seen. A device that doesn't advertise PTP
    /// will never lock, so the UI shouldn't alarm about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    ptp_supported: Option<bool>,
    /// AirPlay-2 only: is a live PTP lock actually *relevant* for this output right now?
    /// True only when the receiver is present AND shares its source-set with ≥1 other
    /// present AP2 receiver (a multi-room group, where drift is audible). A lone AP2
    /// output plays realtime fine unlocked, so the UI shows an unlocked-but-single-room
    /// device as neutral, not alarming.
    #[serde(skip_serializing_if = "Option::is_none")]
    ptp_relevant: Option<bool>,
    /// AirPlay-2 only: decoded capability flags from the `features` TXT, for the
    /// Diagnostics card. `None` if not AP2 or features weren't seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    ap2_features: Option<Ap2FeaturesInfo>,
    /// AirPlay-2 only: wire sample-rate mode — `"auto"` (negotiate 48 kHz, fall back
    /// to 44.1 kHz) or `"fixed_44100"`. `None` for non-AP2 outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    ap2_rate_mode: Option<&'static str>,
    /// AirPlay-2 only: the effective wire rate in Hz the output will use (48000 or
    /// 44100), reflecting the mode + learned capability. `None` for non-AP2.
    #[serde(skip_serializing_if = "Option::is_none")]
    ap2_rate: Option<u32>,
    /// AirPlay-2 only: device-authoritative volume 0.0–1.0 — READ from the receiver
    /// (or last set by the user), or `None` when unknown (receiver didn't report and
    /// the user hasn't set it). Show unknown honestly (no level / 0), never a
    /// fabricated 100 %. We never impose a volume on connect.
    #[serde(skip_serializing_if = "Option::is_none")]
    ap2_volume: Option<f32>,
    /// AirPlay-2 only: mute state (`true` = muted). `None` for non-AP2.
    #[serde(skip_serializing_if = "Option::is_none")]
    ap2_muted: Option<bool>,
    /// sendspin only: the stored wire-codec choice — `"auto"` (Opus when usable,
    /// else PCM), or a pinned `"pcm"`/`"opus"`/`"flac"`. `None` for other kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    sendspin_codec: Option<&'static str>,
    /// sendspin only: the codec the stream will actually use — the choice narrowed by
    /// what the daemon can encode and what the device advertised. Differs from
    /// `sendspin_codec` whenever the choice isn't currently usable.
    #[serde(skip_serializing_if = "Option::is_none")]
    sendspin_codec_active: Option<&'static str>,
    /// sendspin only: every codec the picker offers, with whether it can be selected
    /// and — when it can't — why not. Drives the greyed-out entries in the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    sendspin_codec_options: Option<Vec<CodecOption>>,
    /// sendspin only: the buffer this device asks us to keep queued (`min_buffer_ms`
    /// from its `client/state`), in ms. `None` until it has connected and reported one.
    /// **It can change with the wire codec** — a player may raise it for "codec init,
    /// decode warmup" — which is why a codec change makes the UI re-read this.
    #[serde(skip_serializing_if = "Option::is_none")]
    sendspin_min_buffer_ms: Option<u32>,
    /// sendspin only: the startup lead it would like (`required_lead_time_ms`).
    /// Surfaced for diagnostics; the spec says to extend toward it only for buffered
    /// sources, and this is a live stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    sendspin_required_lead_ms: Option<u32>,
    /// sendspin only: the send-ahead its stream actually uses (ms) — the configured
    /// group lead raised to the largest member requirement.
    #[serde(skip_serializing_if = "Option::is_none")]
    sendspin_send_ahead_ms: Option<u32>,
    /// pw-sink only: has this host's agent been paired? `Some(false)` = it is asking
    /// to be, which is what a *discovered* pw-sink output is; pairing it is the "Add"
    /// for this kind. `None` = not a pw-sink output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pwsink_paired: Option<bool>,
    /// pw-sink only: the code to check against the one the host's own agent logged,
    /// before pairing it. `Some` only while the host is waiting to be paired —
    /// approving a request you cannot identify is how you would hand your audio to a
    /// stranger on the network, so the card shows it next to the button.
    #[serde(skip_serializing_if = "Option::is_none")]
    pwsink_pair_code: Option<String>,
    /// pw-sink only: is a remote `module-rtp-session` receiver actually connected
    /// and being streamed to right now (the AppleMIDI handshake completed)?
    /// `Some(false)` = discovered + routed but the receiver hasn't connected yet;
    /// `None` = not a pw-sink output. Distinct from `present` (mDNS visibility).
    #[serde(skip_serializing_if = "Option::is_none")]
    pwsink_streaming: Option<bool>,
    /// pw-sink only: the host's master volume as *reported by its agent*, cubic
    /// 0.0-1.0. `None` when no agent is connected — deliberately not a
    /// fabricated 100 % (same rule as `ap2_volume`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pwsink_volume: Option<f32>,
    /// pw-sink only: the host's mute state, as reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pwsink_muted: Option<bool>,
    /// pw-sink only: which sink on the host plays our stream (display only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pwsink_sink_name: Option<String>,
    /// pw-sink only: whether foreign streams on that host are currently ducked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pwsink_ducked: Option<bool>,
}

/// One entry in a sendspin output's codec picker.
#[derive(Serialize)]
struct CodecOption {
    /// `"auto"`, `"pcm"`, `"opus"`, `"flac"`.
    codec: &'static str,
    /// Selectable? False ⇒ the UI greys it out (and rejects it if posted anyway).
    available: bool,
    /// Why it isn't selectable — shown as the option's tooltip.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct OutputOpResponse {
    ok: bool,
    message: String,
}

/// Build a sendspin output's codec picker: the stored choice, what it resolves to
/// right now, and per-codec availability.
///
/// Two independent reasons a codec can be unavailable, and the UI needs to say
/// which: **we can't encode it yet** (no encoder in the daemon — currently
/// everything but PCM) or **the device didn't advertise it** at our wire format.
/// `Auto` is always selectable; it just resolves to the best usable option.
fn sendspin_codec_info(
    node_name: &str,
    device_codecs: &[String],
    settings: &crate::sync_settings::SyncSettings,
) -> (&'static str, &'static str, Vec<CodecOption>) {
    let mode = settings.sendspin_codec(node_name);
    let active = crate::sendspin_server::resolve_codec(mode, std::iter::once(&device_codecs.to_vec()));
    let mut options = vec![CodecOption { codec: "auto", available: true, reason: None }];
    for codec in crate::sendspin_server::OFFERED_CODECS {
        let encodable = crate::sendspin_server::can_encode(codec);
        let supported = crate::sendspin_server::device_supports(device_codecs, codec);
        let reason = match (encodable, supported) {
            (true, true) => None,
            (false, _) => Some(format!("the add-on can't encode {codec} yet")),
            (true, false) if device_codecs.is_empty() => {
                Some("not known yet — the device hasn't connected, so it hasn't told us what it decodes".to_string())
            }
            (true, false) => Some(format!("this device doesn't advertise {codec} at 48 kHz/16-bit/stereo")),
        };
        options.push(CodecOption { codec, available: reason.is_none(), reason });
    }
    (mode.as_str(), active, options)
}

/// Every output node name we know of *besides* what mDNS currently sees: the
/// adopted set (so an adopted device that's off the network stays listed as
/// offline) plus anything still referenced by saved routing intent or by a
/// music/announcement group. The latter two matter because adoption starts
/// empty on an upgrade — a device the user had routed must still be findable on
/// the Outputs page (under "Discovered", offline if need be) so its saved
/// routing can be picked back up or cleaned away, rather than becoming
/// invisible state.
fn remembered_output_names(state: &AppState) -> std::collections::BTreeSet<String> {
    let mut names = state.outputs.lock_recover().adopted().clone();
    names.extend(state.routing.lock_recover().referenced_outputs());
    let groups = state.groups_config.lock_recover();
    for g in groups.music() {
        names.extend(g.members.iter().cloned());
    }
    for g in groups.announcement() {
        names.extend(g.targets.iter().cloned());
    }
    names
}

/// Build the full output listing — every kind, every known device, each tagged
/// with its adoption `state`. The two endpoints below are filtered views of this:
/// `/api/outputs` keeps the adopted ones, `/api/outputs/discovered` the rest.
async fn collect_outputs(state: &AppState) -> Vec<OutputInfo> {
    use std::collections::BTreeSet;

    let mut outputs: Vec<OutputInfo> = Vec::new();
    let remembered = remembered_output_names(state);
    let adoption = |n: &str| state.outputs.lock_recover().state(n).as_str();

    // Discovered sendspin devices (present) + any offline ones we remember — so
    // users see every output the system knows about.
    let devices = state.sendspin_devices.lock_recover().clone();
    let mut sendspin_names: BTreeSet<String> = devices.keys().cloned().collect();
    sendspin_names.extend(remembered.iter().filter(|n| n.starts_with(SENDSPIN_DEV_PREFIX)).cloned());
    for node_name in sendspin_names {
        let dev = devices.get(&node_name);
        let present = dev.is_some();
        let name = dev
            .map(|d| d.display_name.clone())
            .unwrap_or_else(|| node_name.strip_prefix(SENDSPIN_DEV_PREFIX).unwrap_or(&node_name).replace(['_', '-'], " "));
        // IP/Port come from the mDNS-resolved server address (`None` until an
        // IPv4 address resolves). Sendspin has no transport encryption, so the
        // column is a constant "None" rather than an absent value.
        let addr = dev.and_then(|d| d.addr);
        let device_codecs = dev.map(|d| d.supported_codecs.clone()).unwrap_or_default();
        let (codec_mode, codec_active, codec_options) =
            sendspin_codec_info(&node_name, &device_codecs, &state.sync_settings.lock_recover());
        // What this device asked for, and the send-ahead its stream ends up with — the
        // same computation sync_group feeds the server, so the UI can't disagree with
        // the audio path.
        let (min_buffer_ms, required_lead_ms) = dev.map(|d| (d.min_buffer_ms, d.required_lead_time_ms)).unwrap_or((None, None));
        let send_ahead_ms = {
            let ss = state.sync_settings.lock_recover();
            let static_delay = ss.sendspin_delays().get(&node_name).copied().unwrap_or(0);
            let us = crate::sendspin_server::required_send_ahead_us(
                ss.group_lead_us(),
                codec_active,
                std::iter::once((min_buffer_ms, static_delay)),
            );
            (us / 1000) as u32
        };
        outputs.push(OutputInfo {
            kind: "sendspin",
            present,
            configured: false, // sendspin devices are always auto-discovered
            state: adoption(&node_name),
            name,
            // Set by the rename pass at the end of collect_outputs.
            renamed: false,
            ip: addr.map(|a| a.ip().to_string()),
            port: addr.map(|a| a.port()),
            encryption: Some("None".to_string()),
            latency_ms: None,
            ptp_locked: None, // sendspin has no PTP
            ptp_lock_age_s: None,
            ptp_supported: None,
            ptp_relevant: None,
            ap2_features: None,
            ap2_rate_mode: None,
            ap2_rate: None,
            ap2_volume: None,
            ap2_muted: None,
            sendspin_codec: Some(codec_mode),
            sendspin_codec_active: Some(codec_active),
            sendspin_codec_options: Some(codec_options),
            sendspin_min_buffer_ms: min_buffer_ms,
            sendspin_required_lead_ms: required_lead_ms,
            sendspin_send_ahead_ms: Some(send_ahead_ms),
            pwsink_paired: None,
            pwsink_pair_code: None,
            pwsink_streaming: None,
            pwsink_volume: None,
            pwsink_muted: None,
            pwsink_sink_name: None,
            pwsink_ducked: None,
            node_name,
        });
    }

    // Discovered AirPlay-2 receivers (present) + remembered offline ones. These
    // are the RAOP-output replacement; like sendspin devices they're virtual (no
    // PipeWire node) and always auto-discovered.
    let ap2_devices = state.ap2_devices.lock_recover().clone();
    // Per-output AP2 render-delay overrides (sync_settings.rs), keyed by node name
    // — the per-output latency field (`latency_ms`).
    let ap2_latencies = state.sync_settings.lock_recover().ap2_latencies();
    // Routing intent snapshot + the set of present AP2 receivers, so we can tell
    // whether a live PTP lock is *relevant* for each output: it only matters when
    // ≥2 present AP2 receivers share a source-set (a multi-room group that would
    // audibly drift without a shared clock). A lone AP2 output renders realtime
    // fine unlocked.
    let ap2_intent = crate::routing_store::snapshot(&state.routing);
    let ap2_present_nodes: Vec<String> = ap2_devices.keys().cloned().collect();
    // Device-authoritative volume/mute snapshot (read from the receiver on connect,
    // or set by the user); volume is absent when unknown → reported as `None`.
    let (ap2_vols, ap2_mutes) = {
        let c = state.ap2_control.lock().await;
        (c.volumes(), c.mutes())
    };
    let mut ap2_names: BTreeSet<String> = ap2_devices.keys().cloned().collect();
    ap2_names.extend(remembered.iter().filter(|n| n.starts_with(AP2_DEV_PREFIX)).cloned());
    for node_name in ap2_names {
        let dev = ap2_devices.get(&node_name);
        let present = dev.is_some();
        let name = dev
            .map(|d| d.display_name.clone())
            .unwrap_or_else(|| node_name.strip_prefix(AP2_DEV_PREFIX).unwrap_or(&node_name).replace(['_', '-'], " "));
        let addr = dev.and_then(|d| d.addr);
        // PTP-lock health: has libairptp heard gPTP from this receiver recently? A
        // locked receiver sends Delay_Req at a ~130ms cadence, so 5s is a generous
        // "still locked" window. If a present, routed receiver isn't locked, its stream
        // renders silence — surface that as degraded in the UI.
        let ptp_age = addr.and_then(|a| state.ap2_ptp.peer_lock_age(&a.ip().to_string()));
        let ptp_locked = if present { Some(ptp_age.is_some_and(|age| age <= std::time::Duration::from_secs(5))) } else { None };
        // Decoded capabilities (features TXT bit 41 = PTP, 40 = buffered, 48 = transient).
        let features = dev.and_then(|d| d.features).map(Features::from_raw);
        let ptp_supported = features.map(|f| f.supports_ptp());
        let ap2_features = features.map(|f| Ap2FeaturesInfo {
            raw: f.to_txt_value(),
            ptp: f.supports_ptp(),
            buffered_audio: f.supports_buffered_audio(),
            transient_pairing: f.supports_transient_pairing(),
        });
        // Relevant = present AND in a ≥2-member AP2 group (shares a non-empty
        // source-set with another present AP2 receiver). Only then does an unlocked
        // receiver risk audible multi-room drift.
        let ptp_relevant = if present {
            let my = crate::routing::source_set_of(&ap2_intent, &node_name);
            Some(!my.is_empty() && ap2_present_nodes.iter().any(|o| o != &node_name && crate::routing::source_set_of(&ap2_intent, o) == my))
        } else {
            None
        };
        // Rate mode (user choice) + the effective wire rate it resolves to.
        let (rate_mode, rate) = {
            let ss = state.sync_settings.lock_recover();
            let mode = match ss.ap2_rate_mode(&node_name) {
                crate::sync_settings::Ap2RateMode::Auto => "auto",
                crate::sync_settings::Ap2RateMode::Fixed44100 => "fixed_44100",
            };
            (mode, ss.ap2_effective_rate(&node_name))
        };
        outputs.push(OutputInfo {
            kind: "airplay2",
            present,
            configured: false, // AP2 receivers are always auto-discovered
            state: adoption(&node_name),
            name,
            // Set by the rename pass at the end of collect_outputs.
            renamed: false,
            ip: addr.map(|a| a.ip().to_string()),
            port: addr.map(|a| a.port()),
            // AirPlay 2 always uses HomeKit transient pairing + encryption.
            encryption: Some("HomeKit".to_string()),
            latency_ms: ap2_latencies.get(&node_name).copied(),
            ptp_locked,
            ptp_lock_age_s: ptp_age.map(|a| a.as_secs()),
            ptp_supported,
            ptp_relevant,
            ap2_features,
            ap2_rate_mode: Some(rate_mode),
            ap2_rate: Some(rate),
            ap2_volume: ap2_vols.get(&node_name).copied(),
            ap2_muted: Some(ap2_mutes.get(&node_name).copied().unwrap_or(false)),
            sendspin_codec: None,
            sendspin_codec_active: None,
            sendspin_codec_options: None,
            sendspin_min_buffer_ms: None,
            sendspin_required_lead_ms: None,
            sendspin_send_ahead_ms: None,
            pwsink_paired: None,
            pwsink_pair_code: None,
            pwsink_streaming: None,
            pwsink_volume: None,
            pwsink_muted: None,
            pwsink_sink_name: None,
            pwsink_ducked: None,
            node_name,
        });
    }

    // pw-sink hosts = **receiver agents** (docs/receiver-agent-plan.md §3). A host is
    // here because a helper on it dialled in, not because something answered an mDNS
    // browse — which is why `present` means "the agent is connected" and
    // `pwsink_streaming` still means "a receiver completed the AppleMIDI handshake"
    // (pw_sink_liveness.rs).
    //
    // A host that has only *asked* to pair is listed too, as a `discovered` output
    // carrying its pairing code: pairing is this kind's "Add" (`adopt_output`), so
    // one section and one button serve every output kind. Remembered names are listed
    // as well, so an unpaired host with routing entries doesn't silently vanish.
    let (agent_hosts, agent_rows) = {
        let agents = state.agents.lock().await;
        (agents.hosts(), agents.snapshot())
    };
    let mut pwsink_names: BTreeSet<String> = agent_hosts.iter().map(|h| h.node_name.clone()).collect();
    pwsink_names.extend(remembered.iter().filter(|n| n.starts_with(PWSINK_DEV_PREFIX)).cloned());
    for node_name in pwsink_names {
        let host = agent_hosts.iter().find(|h| h.node_name == node_name);
        let connected = host.map(|h| h.connected).unwrap_or(false);
        let paired = host.map(|h| h.paired).unwrap_or(false);
        let name = host
            .map(|h| h.label.clone())
            .unwrap_or_else(|| node_name.strip_prefix(PWSINK_DEV_PREFIX).unwrap_or(&node_name).replace(['_', '-'], " "));
        let host_state = agent_rows.iter().find(|row| row.node_name == node_name).and_then(|row| row.state.clone());
        let streaming = crate::pw_sink_liveness::PwSinkLiveness::global().get(&node_name).map(|s| s.established);
        outputs.push(OutputInfo {
            kind: "pwsink",
            present: connected,
            configured: false, // a pairing, not a hand-written config
            state: adoption(&node_name),
            name,
            // Set by the rename pass at the end of collect_outputs.
            renamed: false,
            ip: None, // the agent dials us; we never need its address
            port: None,
            encryption: Some("None".to_string()), // L16 RTP is unencrypted
            latency_ms: None,
            ptp_locked: None,
            ptp_lock_age_s: None,
            ptp_supported: None,
            ptp_relevant: None,
            ap2_features: None,
            ap2_rate_mode: None,
            ap2_rate: None,
            ap2_volume: None,
            ap2_muted: None,
            sendspin_codec: None,
            sendspin_codec_active: None,
            sendspin_codec_options: None,
            sendspin_min_buffer_ms: None,
            sendspin_required_lead_ms: None,
            sendspin_send_ahead_ms: None,
            pwsink_paired: Some(paired),
            pwsink_pair_code: host.and_then(|h| h.pair_code.clone()),
            pwsink_streaming: if connected { Some(streaming.unwrap_or(false)) } else { None },
            // Host-reported, never fabricated: the user's own desktop owns these
            // values and the agent pushes changes back (plan §9.4).
            pwsink_volume: host_state.as_ref().and_then(|s| s.volume),
            pwsink_muted: host_state.as_ref().and_then(|s| s.muted),
            pwsink_sink_name: host_state.as_ref().and_then(|s| s.sink_name.clone()),
            pwsink_ducked: host_state.as_ref().map(|s| s.ducked),
            node_name,
        });
    }

    // The user's own name for an output wins over whatever discovery reported —
    // applied once here rather than per kind above, since every kind derives its
    // name differently but overrides the same way.
    let renamed = crate::outputs_store::names_snapshot(&state.outputs);
    for o in &mut outputs {
        if let Some(name) = renamed.get(&o.node_name) {
            o.name = name.clone();
            o.renamed = true;
        }
    }

    outputs
}

/// An output's user-facing name for a message: the user's own name if they gave
/// one, else the name derived from the node name. Discovery's mDNS name isn't
/// consulted — this is for toasts, where the store lookup is cheap and reaching
/// into three discovery maps isn't.
fn output_label(state: &AppState, node_name: &str) -> String {
    state.outputs.lock_recover().name(node_name).map(str::to_string).unwrap_or_else(|| routing::output_display_name(node_name))
}

/// Both listings from one pass: `(adopted, offered)`.
///
/// Split out of the two handlers below so the routing WebSocket can push these
/// listings when they change (routing.rs) instead of the UI polling for them. One
/// `collect_outputs` call serves both halves, which is what the handlers did
/// individually anyway.
pub(crate) async fn outputs_listings(state: &AppState) -> (Vec<OutputInfo>, Vec<OutputInfo>) {
    let outputs = collect_outputs(state).await;
    outputs.into_iter().partition(|o| o.state == OutputState::Adopted.as_str())
}

/// The system's outputs: adopted only. Everything downstream — the routing
/// matrix's counterpart listing, the group editors, the alignment panel, the HA
/// integration's per-output metadata — means *these* by "output". A discovered
/// device the user hasn't added yet is deliberately absent.
async fn list_outputs(State(state): State<AppState>) -> Json<Vec<OutputInfo>> {
    Json(outputs_listings(&state).await.0)
}

/// Devices discovery has *offered* but the user hasn't added: `state` is
/// `"discovered"` or `"ignored"`. Both are returned in one listing so the
/// Outputs page's "show ignored" checkbox is a client-side filter rather than a
/// refetch. Carries the same connection details, codec picker and test-playback
/// eligibility as an adopted output, because identifying a device ("which
/// speaker is `ap2-dev-living-2`?") is exactly what you need before deciding.
async fn list_discovered_outputs(State(state): State<AppState>) -> Json<Vec<OutputInfo>> {
    Json(outputs_listings(&state).await.1)
}

/// Drop every trace of an output from the *intent* stores: saved routing links
/// and music/announcement group membership. Shared by remove and ignore — both
/// mean "this is not one of my outputs", and leaving a stale link or group
/// member behind would silently re-route it if it were added again later.
/// Returns a human-readable note about what was cleaned, for the toast.
fn forget_output_intent(state: &AppState, node_name: &str) -> String {
    let mut notes: Vec<String> = Vec::new();
    let links = state.routing.lock_recover().referenced_outputs().contains(node_name);
    if links {
        if let Err(e) = state.routing.lock_recover().remove_entity(node_name) {
            tracing::warn!("removing routing intent for '{node_name}': {e}");
        } else {
            notes.push("routing".to_string());
        }
    }
    match state.groups_config.lock_recover().remove_output(node_name) {
        Ok(true) => notes.push("group membership".to_string()),
        Ok(false) => {}
        Err(e) => tracing::warn!("removing '{node_name}' from groups: {e}"),
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!(" (also cleared its {})", notes.join(" and "))
    }
}

/// Add a discovered device: from here on it's routable, tunable and — with the
/// `expose_outputs_as_media_players` setting on — an HA `media_player`. Any
/// routing it had from before (an upgrade, or a previous adoption) is still in
/// the store and starts applying again on the next reconcile, which is why this
/// nudges the change notifier.
///
/// For a receiver host this is also the **pairing** step: a pw-sink output that is
/// waiting to be paired gets its token minted here first (plan §8). One button, one
/// intention — a human ran the agent on that host and a human is clicking Add here,
/// so asking twice would only be ceremony. Pairing that fails leaves the output
/// unadopted rather than adopting a host the daemon cannot drive.
async fn adopt_output(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
    tracing::info!("USER ACTION: add output '{}'", node_name);
    let mut paired_note = String::new();
    if node_name.starts_with(PWSINK_DEV_PREFIX) {
        let mut agents = state.agents.lock().await;
        match agents.identity_for_node(&node_name) {
            Some(identity) => {
                // Already paired (a re-add after Remove) → nothing to mint.
                if !agents.is_paired(&identity) {
                    match agents.approve(&identity) {
                        Ok(agent) => paired_note = format!(" (paired '{}')", agent.label),
                        Err(e) => return Json(OutputOpResponse { ok: false, message: format!("failed to pair '{node_name}': {e}") }),
                    }
                }
            }
            None => {
                return Json(OutputOpResponse {
                    ok: false,
                    message: format!("no receiver agent is asking to pair as '{node_name}' — is it still running?"),
                })
            }
        }
    }
    if let Err(e) = state.outputs.lock_recover().adopt(&node_name) {
        return Json(OutputOpResponse { ok: false, message: format!("failed to add '{node_name}': {e}") });
    }
    let _ = state.changes.send(());
    Json(OutputOpResponse { ok: true, message: format!("added '{}'{paired_note}", output_label(&state, &node_name)) })
}

/// Unpair a receiver host: revoke its token, forget its routing and group
/// membership, and un-adopt it — one action, because "remove this output" and "stop
/// trusting this host" are not two intentions a user has separately.
///
/// Its agent keeps dialling in, so the host comes back under Discovered as pairable,
/// exactly like an un-added speaker that is still on the network. Ignore it there if
/// it should stay out of the way.
async fn unpair_output(State(state): State<AppState>, Path(node_name): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
    tracing::info!("USER ACTION: unpair output '{}'", node_name);
    let label = output_label(&state, &node_name);
    {
        // Idempotent, and deliberately not fussy about finding a pairing to revoke:
        // an output can outlive its pairing (a lost `agents.json`, a host whose agent
        // was reinstalled), and that card's only removal button is this one. Refusing
        // would leave it stuck on the page with nothing that could clear it.
        let mut agents = state.agents.lock().await;
        match agents.identity_for_node(&node_name) {
            Some(identity) => {
                if let Err(e) = agents.unpair(&identity) {
                    tracing::warn!("revoking the pairing of '{node_name}': {e}");
                }
            }
            None => tracing::info!("'{node_name}' had no pairing to revoke; removing it anyway"),
        }
    }
    let cleaned = forget_output_intent(&state, &node_name);
    if let Err(e) = state.outputs.lock_recover().reset(&node_name) {
        // The pairing is already revoked, so report the leftover rather than
        // claiming the host is still an output.
        tracing::warn!("unpaired '{node_name}' but could not reset its adoption: {e}");
    }
    let _ = state.changes.send(());
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("unpaired '{label}'{cleaned}") }))
}

/// Dismiss a discovered device: hidden from the Outputs page unless "show
/// ignored" is ticked. The stronger form of remove — so it also clears any
/// routing/group references it had.
async fn ignore_output(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
    tracing::info!("USER ACTION: ignore output '{}'", node_name);
    let cleaned = forget_output_intent(&state, &node_name);
    if let Err(e) = state.outputs.lock_recover().ignore(&node_name) {
        return Json(OutputOpResponse { ok: false, message: format!("failed to ignore '{node_name}': {e}") });
    }
    let _ = state.changes.send(());
    Json(OutputOpResponse { ok: true, message: format!("ignoring '{}'{cleaned}", output_label(&state, &node_name)) })
}

/// Remove an output: back to undecided. It stops being routable, loses its HA
/// media_player, and its routing + group membership are forgotten. A device
/// that's still on the network reappears under "Discovered" (where it can be
/// added again or ignored); one that's offline/gone simply disappears.
async fn remove_output(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
    tracing::info!("USER ACTION: remove output '{}'", node_name);
    let cleaned = forget_output_intent(&state, &node_name);
    if let Err(e) = state.outputs.lock_recover().reset(&node_name) {
        return Json(OutputOpResponse { ok: false, message: format!("failed to remove '{node_name}': {e}") });
    }
    // Un-adopting changes what the group reconciler is allowed to drive, so the
    // device's stream/session has to be torn down now, not on the next unrelated
    // registry event.
    let _ = state.changes.send(());
    Json(OutputOpResponse { ok: true, message: format!("removed '{}'{cleaned}", output_label(&state, &node_name)) })
}

// ---- AirPlay-receive source -----------------------------------------------
//
// The AirPlay-receive source is an embedded, native RAOP receiver
// (airplay_source.rs) — not a subprocess and not a PipeWire module. Its
// enabled/disabled state and knobs are persisted in the sources store
// (sources_store.rs), which starts empty on a fresh install (no options.json
// seeding) and is then authoritative. Same "runtime, no restart" model as
// /api/outputs, but backed by an in-process receiver rather than a module.

/// Reconcile BOTH source kinds after a `/api/sources` mutation: (un)load RTP
/// modules (Phase 2) and start/stop AirPlay receivers (Phase 4) to match the
/// persisted set. Idempotent, so it's safe to call after every add/update/
/// remove. `list()` returns an owned snapshot so no lock is held across await.
async fn reconcile_sources(state: &AppState) {
    let entries = state.sources.lock_recover().list();
    crate::rtp_source::reconcile(&entries, &state.pw_cmd, &state.pw).await;
    crate::airplay_source::reconcile(&state.airplay, &entries, &state.airplay_clients, &state.now_playing).await;
    // A source that no longer exists must not leave a track behind in the
    // listings for the TTL to eventually collect (now_playing.rs).
    let live: Vec<String> = entries.iter().map(|e| e.node_name()).collect();
    state.now_playing.retain_sources(&live);
}

/// One remembered AirPlay sender for the Sources tab. `key` (name if known,
/// else IP) identifies it for a `forget` call. `connected` is live, so it's
/// derived here rather than read from the persisted record (which omits it).
#[derive(Serialize)]
struct AirplayClientInfo {
    key: String,
    name: Option<String>,
    addr: String,
    first_seen: u64,
    last_connected: u64,
    connected: bool,
    banned: bool,
    priority: i32,
}

// --- per-receiver client-registry helpers ----------------------------------
//
// Shared by the legacy singular routes (which target `LEGACY_AIRPLAY_ID`) and
// the per-source `/api/sources/{id}/clients/*` routes. Each operates on that
// source's own client registry (airplay_clients.rs).

fn list_clients_for(state: &AppState, id: &str) -> Vec<AirplayClientInfo> {
    state
        .airplay_clients
        .registry(id)
        .list()
        .into_iter()
        .map(|c| AirplayClientInfo {
            key: c.key().to_string(),
            name: c.name,
            addr: c.addr,
            first_seen: c.first_seen,
            last_connected: c.last_connected,
            connected: c.connected,
            banned: c.banned,
            priority: c.priority,
        })
        .collect()
}

fn forget_client_for(state: &AppState, id: &str, key: &str) -> (StatusCode, Json<OutputOpResponse>) {
    if state.airplay_clients.registry(id).forget(key) {
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("forgot AirPlay client '{key}'") }))
    } else {
        // Not found, or still connected (a live client can't be forgotten).
        (
            StatusCode::CONFLICT,
            Json(OutputOpResponse { ok: false, message: format!("could not forget '{key}' (unknown or still connected)") }),
        )
    }
}

fn ban_client_for(state: &AppState, id: &str, key: &str, banned: bool) -> (StatusCode, Json<OutputOpResponse>) {
    if state.airplay_clients.registry(id).set_banned(key, banned) {
        let verb = if banned { "banned" } else { "unbanned" };
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{verb} AirPlay client '{key}'") }))
    } else {
        (StatusCode::NOT_FOUND, Json(OutputOpResponse { ok: false, message: format!("unknown AirPlay client '{key}'") }))
    }
}

fn set_priority_for(state: &AppState, id: &str, key: &str, priority: i32) -> (StatusCode, Json<OutputOpResponse>) {
    if state.airplay_clients.registry(id).set_priority(key, priority) {
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set priority {priority} for '{key}'") }))
    } else {
        (StatusCode::NOT_FOUND, Json(OutputOpResponse { ok: false, message: format!("unknown AirPlay client '{key}'") }))
    }
}

/// Force-disconnect a currently-connected client on source `id` by dropping its
/// RTSP connection (the receiver stops its stream shortly after).
async fn disconnect_client_for(state: &AppState, id: &str, key: &str) -> (StatusCode, Json<OutputOpResponse>) {
    // Resolve the key to the live peer IP the RAOP server keys connections on.
    let Some(addr) = state.airplay_clients.registry(id).connected_addr(key) else {
        return (StatusCode::CONFLICT, Json(OutputOpResponse { ok: false, message: format!("'{key}' is not currently connected") }));
    };
    match state.airplay.lock().await.get(id) {
        Some(handle) => {
            handle.disconnect_client(&addr);
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("disconnecting AirPlay client '{key}'") }))
        }
        None => (StatusCode::CONFLICT, Json(OutputOpResponse { ok: false, message: format!("AirPlay source '{id}' is not running") })),
    }
}

fn policy_message(prevent_takeover: bool) -> &'static str {
    if prevent_takeover {
        "AirPlay: new senders refused while one is streaming"
    } else {
        "AirPlay: new senders may take over the current stream"
    }
}

// --- legacy singular routes (target the legacy AirPlay id) ------------------

#[derive(Deserialize)]
struct ForgetClientRequest {
    key: String,
}

#[derive(Deserialize)]
struct BanClientRequest {
    key: String,
    banned: bool,
}

#[derive(Deserialize)]
struct SetPriorityRequest {
    key: String,
    priority: i32,
}

#[derive(Deserialize)]
struct DisconnectClientRequest {
    key: String,
}

#[derive(Deserialize)]
struct SetAirplayPolicyRequest {
    prevent_takeover: bool,
}

// --- per-source routes (Phase 4): /api/sources/{id}/clients/* + /policy -----

async fn list_source_clients(State(state): State<AppState>, Path(id): Path<String>) -> Json<Vec<AirplayClientInfo>> {
    Json(list_clients_for(&state, &id))
}

async fn forget_source_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ForgetClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    forget_client_for(&state, &id, &req.key)
}

async fn ban_source_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<BanClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    ban_client_for(&state, &id, &req.key, req.banned)
}

async fn set_source_client_priority(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetPriorityRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    set_priority_for(&state, &id, &req.key, req.priority)
}

async fn disconnect_source_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DisconnectClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    disconnect_client_for(&state, &id, &req.key).await
}

/// Toggle one AirPlay source's anti-takeover policy: persist it into that
/// source's config and live-update its running receiver's flag (no restart).
async fn set_source_policy(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetAirplayPolicyRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Read the current AirPlay config for this id, flip prevent_takeover, save.
    let entry = state.sources.lock_recover().get(&id);
    let Some(entry) = entry else {
        return (StatusCode::NOT_FOUND, Json(OutputOpResponse { ok: false, message: format!("no source '{id}'") }));
    };
    let SourceConfig::Airplay(mut cfg) = entry.config else {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("source '{id}' is not an AirPlay source") }));
    };
    cfg.prevent_takeover = req.prevent_takeover;
    if let Err(e) = state.sources.lock_recover().update(&id, None, Some(SourceConfig::Airplay(cfg))) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
    }
    if let Some(handle) = state.airplay.lock().await.get(&id) {
        handle.set_prevent_takeover(req.prevent_takeover);
    }
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: policy_message(req.prevent_takeover).to_string() }))
}

// ---- Now-playing metadata (now_playing.rs) --------------------------------
//
// Reads are the cold-path companion to the `now_playing` WebSocket frame (a
// consumer that just connected, or `curl`). Writes are how a *remote* producer
// reports — the local AirPlay receiver writes straight into the store through its
// own reporter handle and never comes through here.

/// `GET /api/now_playing` — every source with something to say, keyed by node
/// name. Same payload as the socket's `now_playing` frame.
async fn list_now_playing(State(state): State<AppState>) -> Json<NowPlayingListResponse> {
    Json(NowPlayingListResponse { sources: state.now_playing.snapshot() })
}

/// `GET /api/now_playing/{node_name}` — one source, or 404 when nothing is known.
async fn get_now_playing(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
) -> Result<Json<crate::now_playing::NowPlaying>, SourceError> {
    state
        .now_playing
        .get(&node_name)
        .map(Json)
        .ok_or_else(|| source_err(StatusCode::NOT_FOUND, format!("nothing playing on '{node_name}'")))
}

/// `PUT /api/now_playing/{node_name}` — merge metadata into a source by node name.
///
/// Only *configured* sources are accepted. That is the one check worth making:
/// this endpoint is unauthenticated like the rest of the API, and without it a
/// caller could invent listings for nodes that do not exist, which would then
/// appear in the socket frame and in Home Assistant.
async fn put_now_playing(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(update): Json<crate::now_playing::MetadataUpdate>,
) -> Result<Json<OutputOpResponse>, SourceError> {
    if update.is_empty() {
        return Err(source_err(StatusCode::BAD_REQUEST, "no metadata fields supplied".to_string()));
    }
    let known = state.sources.lock_recover().list().iter().any(|e| e.node_name() == node_name);
    if !known {
        return Err(source_err(StatusCode::NOT_FOUND, format!("no source with node name '{node_name}'")));
    }
    state.now_playing.reporter(node_name.clone()).update(update);
    Ok(Json(OutputOpResponse { ok: true, message: format!("now playing updated for '{node_name}'") }))
}

/// `DELETE /api/now_playing/{node_name}` — the session ended.
///
/// A producer that goes away should say so rather than leave the TTL to collect
/// it, so Home Assistant's media card collapses instead of freezing on the last
/// track. Idempotent: clearing something already gone is a success.
async fn clear_now_playing(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
    state.now_playing.reporter(node_name.clone()).clear();
    Json(OutputOpResponse { ok: true, message: format!("now playing cleared for '{node_name}'") })
}

/// `GET /api/now_playing/{node_name}/artwork` — the embedded cover art bytes.
///
/// The `?rev=` in the published path is a cache-buster, not a selector: there is
/// only ever one current image per source, so the query is deliberately ignored
/// here and the *current* art is returned.
async fn get_now_playing_artwork(State(state): State<AppState>, Path(node_name): Path<String>) -> Response {
    match state.now_playing.artwork(&node_name) {
        Some((bytes, mime, rev)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime),
                // Immutable *for this rev*: the path changes when the picture does,
                // so a consumer (and HA's image proxy) can cache hard.
                (header::CACHE_CONTROL, "public, max-age=3600, immutable".to_string()),
                (header::ETAG, format!("\"{node_name}-{rev}\"")),
            ],
            Body::from(bytes.to_vec()),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no artwork").into_response(),
    }
}

/// `POST /api/now_playing/report` — the self-identifying entry point for a remote
/// producer (the Pi bridge's reporter, plan §3.4/WP3).
///
/// A reporter on the Pi knows its own RTP port; it does not know — and must not
/// have to learn — the source ids this daemon assigned. So it says "I am the
/// sender on port 46000" and the port is resolved against the source store, which
/// is the same key `bt_bridge_discovery` already matches an advert to a source by.
/// A port matching no configured source is a `404`, not an error worth retrying:
/// the bridge may simply be set up before its source was added here.
async fn report_now_playing(
    State(state): State<AppState>,
    Json(req): Json<NowPlayingReportRequest>,
) -> Result<Json<OutputOpResponse>, SourceError> {
    let node_name = {
        let sources = state.sources.lock_recover();
        sources.list().iter().find(|e| matches!(&e.config, SourceConfig::Rtp(r) if r.port == req.rtp_port)).map(|e| e.node_name())
    };
    let Some(node_name) = node_name else {
        return Err(source_err(StatusCode::NOT_FOUND, format!("no RTP source is configured on port {}", req.rtp_port)));
    };
    let reporter = state.now_playing.reporter(node_name.clone());
    // An empty body is how a reporter says "nothing is playing any more" without
    // needing a second endpoint — its BlueZ player object simply went away.
    if req.metadata.is_empty() {
        reporter.clear();
        return Ok(Json(OutputOpResponse { ok: true, message: format!("now playing cleared for '{node_name}'") }));
    }
    reporter.update(req.metadata);
    Ok(Json(OutputOpResponse { ok: true, message: format!("now playing updated for '{node_name}'") }))
}

#[derive(Serialize)]
struct NowPlayingListResponse {
    sources: std::collections::BTreeMap<String, crate::now_playing::NowPlaying>,
}

#[derive(Deserialize)]
struct NowPlayingReportRequest {
    /// The UDP port this reporter's `module-rtp-sink` transmits to — the key that
    /// identifies which of our sources it is feeding.
    rtp_port: u16,
    /// The metadata itself, flattened, so the body reads as one object rather than
    /// a wrapper: `{"rtp_port": 46000, "title": "…", "artist": "…"}`.
    #[serde(flatten)]
    metadata: crate::now_playing::MetadataUpdate,
}

// ---- RTP source (Bluetooth bridge firmware target) ------------------------
//
// An RTP source is a native PipeWire module, not a subprocess, so it is
// loaded/unloaded via the PipeWire thread (PwCommand::Load/Unload, keyed by the
// entry's own node name) rather than through the process supervisor. Re-point it
// live, no restart. Once loaded its node shows up in the routing matrix
// automatically (routing.rs classifies it as a source).

// ---- Multi-source collection CRUD (Phase 3) ------------------------------
//
// The generalized, keyed replacement for the two singular `/api/source/*` routes
// this daemon used to expose: a collection of AirPlay + RTP input sources, each with its own
// stable id / node name (sources_store.rs). These handlers only mutate the
// STORE — actually loading/unloading the PipeWire module (RTP) or starting/
// stopping the embedded receiver (AirPlay) is done by the per-kind reconcilers
// wired from main.rs (Phases 2 & 4). After each mutation we nudge the change
// notifier so watchers refresh.

/// Nested response shape for a single source. Distinct from the *flat* stored
/// [`SourceEntry`] (`{id,label,kind,<config...>}`): here the kind-specific
/// config is nested under `airplay`/`rtp` (exactly one non-null), plus the
/// derived `node_name` and the live `present` flag. Matches the frontend's
/// `SourceView` (Phase 5) exactly.
#[derive(Serialize, Debug, PartialEq, Eq)]
struct SourceView {
    id: String,
    label: String,
    kind: SourceKind,
    /// A node named `node_name` exists in the live PipeWire registry right now
    /// (the source is actually loaded/running).
    present: bool,
    node_name: String,
    /// The AirPlay config when `kind == airplay`, else `null`.
    airplay: Option<AirplaySourceConfig>,
    /// The RTP config when `kind == rtp`, else `null`.
    rtp: Option<RtpSourceConfig>,
    /// The discovered Bluetooth bridge that feeds this RTP source, when exactly
    /// one advertises this port (and group). `null` for AirPlay sources, when no
    /// bridge advertises, or when two do — see `bt_bridge_discovery::match_bridge`.
    bridge: Option<BridgeView>,
}

/// A discovered Bluetooth→RTP bridge, as the UI sees it. Used both nested on an
/// RTP source (`bridge`) and standalone in `discovered_bridges`.
#[derive(Serialize, Debug, PartialEq, Eq)]
struct BridgeView {
    /// mDNS instance fullname — the stable identity the UI keys on.
    fullname: String,
    /// Human label from the advert (the bridge's Bluetooth/host name).
    display_name: String,
    /// mDNS hostname, for display when the address is uninformative.
    hostname: String,
    /// Resolved address, `null` until mDNS resolves one.
    addr: Option<String>,
    /// UDP port this bridge sends RTP to (what a source must listen on).
    rtp_port: u16,
    /// Its RTP destination: the HA host's address, or a multicast group.
    rtp_dest: String,
    rate: u32,
    channels: u16,
    /// Diagnostics page URL, `null` while the address is unresolved.
    diag_url: Option<String>,
    /// The diagnostics app answered on the last probe. **The UI must only offer
    /// the link when this is true** — the advert is installed by
    /// `setup_pi_bridge.py` and outlives any particular app run.
    diag_ok: bool,
}

impl BridgeView {
    fn of(b: &crate::bt_bridge_discovery::BtBridge) -> Self {
        Self {
            fullname: b.fullname.clone(),
            display_name: b.display_name.clone(),
            hostname: b.hostname.trim_end_matches('.').to_string(),
            addr: b.addr.map(|a| a.to_string()),
            rtp_port: b.stream.rtp_port,
            rtp_dest: b.stream.rtp_dest.clone(),
            rate: b.stream.rate,
            channels: b.stream.channels,
            diag_url: b.diag_url(),
            diag_ok: b.diag_ok(),
        }
    }
}

/// Pure conversion: flat stored [`SourceEntry`] + a live `present` flag → the
/// nested [`SourceView`] wire shape. Kept side-effect-free so it is unit-tested
/// directly (see tests below).
///
/// `bridges` is the discovered-bridge set to match an RTP source against; pass an
/// empty slice to skip (AirPlay sources never match).
fn source_view(entry: &SourceEntry, present: bool, bridges: &[crate::bt_bridge_discovery::BtBridge]) -> SourceView {
    let (airplay, rtp) = match &entry.config {
        SourceConfig::Airplay(a) => (Some(a.clone()), None),
        SourceConfig::Rtp(r) => (None, Some(r.clone())),
    };
    let bridge =
        rtp.as_ref().and_then(|r| crate::bt_bridge_discovery::match_bridge(bridges.iter(), r.port, &r.source_addr)).map(BridgeView::of);
    SourceView {
        id: entry.id.clone(),
        label: entry.label.clone(),
        kind: entry.kind(),
        present,
        node_name: entry.node_name(),
        airplay,
        rtp,
        bridge,
    }
}

/// Whether a node with `node_name` is present in the live registry right now.
fn node_present(pw: &SharedState, node_name: &str) -> bool {
    pw.lock_recover().nodes.values().any(|n| n.node_name == node_name)
}

/// The set of node names currently present in the live registry — snapshotted
/// once so a list of sources can be resolved without re-locking per entry.
fn present_node_names(pw: &SharedState) -> std::collections::HashSet<String> {
    pw.lock_recover().nodes.values().map(|n| n.node_name.clone()).collect()
}

/// A default RTP config (all knobs at their `DEFAULT_RTP_*`), used when a
/// `POST` omits the `rtp` object. `RtpSourceConfig` has no `Default` impl, so
/// this spells it out from the shared constants.
fn default_rtp_config() -> RtpSourceConfig {
    RtpSourceConfig {
        port: DEFAULT_RTP_PORT,
        latency_msec: DEFAULT_RTP_LATENCY_MSEC,
        source_addr: DEFAULT_RTP_SOURCE_ADDR.to_string(),
        ignore_ssrc: DEFAULT_RTP_IGNORE_SSRC,
        rate: DEFAULT_RTP_RATE,
    }
}

#[derive(Serialize)]
struct SourcesListResponse {
    sources: Vec<SourceView>,
    /// Discovered Bluetooth bridges that no configured RTP source is listening
    /// for — offered on the Sources tab for one-click adoption, with their
    /// advertised port/rate prefilled so the two ends cannot disagree by typo.
    /// A bridge already matched by a source appears there (`sources[].bridge`)
    /// and is omitted here, so the list is exactly "what is missing".
    discovered_bridges: Vec<BridgeView>,
}

/// `POST /api/sources` body. `kind` selects which config object is honored; the
/// matching `airplay`/`rtp` object carries partial fields (every field has a
/// serde default), and may be omitted entirely to accept all defaults.
#[derive(Deserialize)]
struct CreateSourceRequest {
    label: String,
    kind: SourceKind,
    #[serde(default)]
    airplay: Option<AirplaySourceConfig>,
    #[serde(default)]
    rtp: Option<RtpSourceConfig>,
}

/// `PUT /api/sources/{id}` body. All fields optional: `label` renames, and an
/// `airplay`/`rtp` object replaces the config (must match the source's
/// immutable kind — the store rejects a mismatch). Omitting both config objects
/// is a label-only update.
#[derive(Deserialize)]
struct UpdateSourceRequest {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    airplay: Option<AirplaySourceConfig>,
    #[serde(default)]
    rtp: Option<RtpSourceConfig>,
}

/// A source-CRUD error as a `{ok:false, message}` body + status code. Both the
/// success and error arms implement `IntoResponse`, so handlers return
/// `Result<_, SourceError>`.
type SourceError = (StatusCode, Json<OutputOpResponse>);

fn source_err(code: StatusCode, message: String) -> SourceError {
    (code, Json(OutputOpResponse { ok: false, message }))
}

async fn list_sources(State(state): State<AppState>) -> Json<SourcesListResponse> {
    // Probe stale diagnostics pages first, so `bridge.diag_ok` in this very
    // response reflects a fresh check. Only when someone is looking (this
    // handler), rate-limited by `PROBE_TTL` — no background polling of Pi Zeros.
    crate::bt_bridge_discovery::refresh_probes(&state.bt_bridges).await;

    let entries = state.sources.lock_recover().list();
    let present = present_node_names(&state.pw);
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    let sources: Vec<SourceView> = entries.iter().map(|e| source_view(e, present.contains(&e.node_name()), &bridges)).collect();
    let discovered_bridges = unmatched_bridges(&bridges, &sources);
    Json(SourcesListResponse { sources, discovered_bridges })
}

/// The bridges not already claimed by a source in `sources`, as views.
///
/// Pure so the "already configured disappears from the offer list" rule is
/// unit-tested: a bridge visible in both lists would invite the user to add a
/// duplicate source on a port that is already taken.
fn unmatched_bridges(bridges: &[crate::bt_bridge_discovery::BtBridge], sources: &[SourceView]) -> Vec<BridgeView> {
    let claimed: std::collections::HashSet<&str> = sources.iter().filter_map(|s| s.bridge.as_ref()).map(|b| b.fullname.as_str()).collect();
    bridges.iter().filter(|b| !claimed.contains(b.fullname.as_str())).map(BridgeView::of).collect()
}

async fn create_source(
    State(state): State<AppState>,
    Json(req): Json<CreateSourceRequest>,
) -> Result<(StatusCode, Json<SourceView>), SourceError> {
    let config = match req.kind {
        SourceKind::Airplay => SourceConfig::Airplay(req.airplay.unwrap_or_default()),
        SourceKind::Rtp => SourceConfig::Rtp(req.rtp.unwrap_or_else(default_rtp_config)),
    };
    let entry = {
        let mut store = state.sources.lock_recover();
        // add() validates (e.g. RTP port collisions) — surface that as a 400.
        store.add(req.label, config).map_err(|e| source_err(StatusCode::BAD_REQUEST, e.to_string()))?
    };
    // Load/start the new source now, then nudge downstream (routing/groups).
    reconcile_sources(&state).await;
    let _ = state.changes.send(());
    let present = node_present(&state.pw, &entry.node_name());
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    Ok((StatusCode::CREATED, Json(source_view(&entry, present, &bridges))))
}

async fn get_source(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<SourceView>, SourceError> {
    let entry =
        state.sources.lock_recover().get(&id).ok_or_else(|| source_err(StatusCode::NOT_FOUND, format!("no source with id '{id}'")))?;
    let present = node_present(&state.pw, &entry.node_name());
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    Ok(Json(source_view(&entry, present, &bridges)))
}

async fn update_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateSourceRequest>,
) -> Result<Json<SourceView>, SourceError> {
    // Kind is immutable, so it's derived from whichever config object is present
    // (the store rejects a config whose kind differs from the stored entry's).
    let config = match (req.airplay, req.rtp) {
        (Some(a), _) => Some(SourceConfig::Airplay(a)),
        (None, Some(r)) => Some(SourceConfig::Rtp(r)),
        (None, None) => None,
    };
    let entry = {
        let mut store = state.sources.lock_recover();
        store.update(&id, req.label, config).map_err(|e| {
            let msg = e.to_string();
            // "no source with id" → 404; validation (kind change, port clash) → 400.
            let code = if msg.contains("no source with id") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
            source_err(code, msg)
        })?
    };
    // Apply the config change to the running source, then nudge downstream.
    reconcile_sources(&state).await;
    let _ = state.changes.send(());
    let present = node_present(&state.pw, &entry.node_name());
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    Ok(Json(source_view(&entry, present, &bridges)))
}

async fn delete_source(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
    // Bind the result so the std MutexGuard drops HERE — a match scrutinee holds
    // its temporaries for the whole match, which would keep the guard alive
    // across the `.await` below and make the handler future `!Send`.
    let removed = state.sources.lock_recover().remove(&id);
    match removed {
        Ok(true) => {
            // Unload/stop the removed source now, then nudge downstream.
            reconcile_sources(&state).await;
            let _ = state.changes.send(());
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("removed source '{id}'") }))
        }
        Ok(false) => (StatusCode::NOT_FOUND, Json(OutputOpResponse { ok: false, message: format!("no source with id '{id}'") })),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") })),
    }
}

// ---- Sendspin per-device volume ------------------------------------------
//
// Sendspin devices are virtual outputs fed by a shared group sink, so there's
// no PipeWire node volume to drive. Volume is carried in-band over the sendspin
// protocol to the specific device; see
// sendspin_volume.rs. `GET` returns the desired volume per device node name
// (sparse — absent means the default); `PUT` sets one device.

#[derive(Deserialize)]
struct SetSendspinVolumeRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    node_name: String,
    /// Target volume, 0–100.
    volume: u8,
}

async fn get_sendspin_volumes(State(state): State<AppState>) -> Json<std::collections::HashMap<String, u8>> {
    Json(state.sendspin_control.lock().await.volumes())
}

async fn set_sendspin_volume(
    State(state): State<AppState>,
    Json(req): Json<SetSendspinVolumeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Two statements: the control guard must drop before the send is awaited
    // (see sendspin_volume::PendingCommands).
    let pending = state.sendspin_control.lock().await.set_volume(&req.node_name, req.volume);
    let reached = pending.apply().await;
    let message = if reached {
        format!("set '{}' to {}%", req.node_name, req.volume.min(100))
    } else {
        // Stored; will apply when the device (re)connects.
        format!("saved {}% for '{}' (device not connected)", req.volume.min(100), req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
struct SetSendspinMuteRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    node_name: String,
    /// Target mute state.
    muted: bool,
}

async fn set_sendspin_mute(State(state): State<AppState>, Json(req): Json<SetSendspinMuteRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    let pending = state.sendspin_control.lock().await.set_muted(&req.node_name, req.muted);
    let reached = pending.apply().await;
    let verb = if req.muted { "muted" } else { "unmuted" };
    let message = if reached {
        format!("{verb} '{}'", req.node_name)
    } else {
        format!("saved {verb} for '{}' (device not connected)", req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
struct ClearSendspinRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    node_name: String,
}

/// Ask one sendspin device to discard buffered-but-unplayed audio and re-anchor,
/// without ending its stream (`stream/clear`).
///
/// The recovery action for the failure mode where a device is demonstrably being
/// *sent* audio and renders nothing — measured on 2026-08-03, when three of four
/// devices went silent while the daemon, the graph and the clock sync were all
/// healthy (docs/sendspin-open-items.md). Until this existed the only lever was
/// restarting the whole add-on, which fixed it but destroyed the evidence and
/// interrupted every other output.
///
/// Cheaper and more surgical than the alternatives: a per-device *reconnect* (nudge
/// its static delay) costs a full re-dial and a fresh clock filter for that device,
/// and a group restart costs it for everyone. This is one frame.
///
/// A disconnected device is reported honestly rather than treated as success — there
/// is nothing to clear, and its next stream starts fresh anyway.
async fn clear_sendspin_stream(
    State(state): State<AppState>,
    Json(req): Json<ClearSendspinRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Two statements, never one: holding the control guard across the await would
    // block every other device's commands behind this device's socket.
    let pending = state.sendspin_control.lock().await.clear_stream(&req.node_name);
    let reached = pending.apply().await;
    let display = output_label(&state, &req.node_name);
    if reached {
        tracing::info!("USER ACTION: sendspin stream/clear -> '{}' (buffers discarded, re-anchoring)", req.node_name);
    }
    let message = if reached {
        format!("cleared '{display}' — it will re-anchor on the next audio")
    } else {
        format!("'{display}' has no live connection, so there is nothing to clear")
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: reached, message }))
}

// ---- AirPlay-2 per-device volume/mute ------------------------------------
//
// AP2 receivers are virtual outputs (`ap2-dev-…`) like sendspin: no PipeWire
// node volume. Volume is carried in-band as an RTSP SET_PARAMETER the sender
// pushes to the receiver (ap2_volume.rs → ap2_server.rs); mute is volume 0.
// Volume is 0.0–1.0 (matches the receiver's dB mapping and the matrix field).
// There's no receiver→daemon feedback yet, so the UI shows the last-set level.

#[derive(Deserialize)]
struct SetAp2VolumeRequest {
    /// Virtual device node name, e.g. `ap2-dev-yamaha`.
    node_name: String,
    /// Target volume, 0.0–1.0.
    volume: f32,
}

async fn set_ap2_volume(State(state): State<AppState>, Json(req): Json<SetAp2VolumeRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    let reached = state.ap2_control.lock().await.set_volume(&req.node_name, req.volume).await;
    let pct = (req.volume.clamp(0.0, 1.0) * 100.0).round() as u8;
    let message = if reached {
        format!("set '{}' to {}%", req.node_name, pct)
    } else {
        format!("saved {}% for '{}' (not streaming)", pct, req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
struct SetAp2MuteRequest {
    node_name: String,
    muted: bool,
}

async fn set_ap2_mute(State(state): State<AppState>, Json(req): Json<SetAp2MuteRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    let reached = state.ap2_control.lock().await.set_muted(&req.node_name, req.muted).await;
    let verb = if req.muted { "muted" } else { "unmuted" };
    let message =
        if reached { format!("{verb} '{}'", req.node_name) } else { format!("saved {verb} for '{}' (not streaming)", req.node_name) };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

// ---- Receiver agents (pwsink_agent.rs) -----------------------------------
//
// A pw-sink output is a receiver agent (docs/receiver-agent-plan.md §3). The pairing
// *decisions* are output operations — `/api/outputs/{n}/adopt` pairs, `/unpair`
// revokes, `/ignore` hides — because a host asking to pair is a discovered output and
// nothing is gained by giving it a second vocabulary. What is left here is the
// listing (diagnostics) and the per-host volume/mute the agent applies on the
// receiver's *own* master out. Unlike sendspin/AP2 volume there is nothing to "save
// for later": the host owns the value and reports it back, so an unreachable host is
// an error rather than a stored intent (§9.4).

async fn get_agents(State(state): State<AppState>) -> Json<Vec<crate::pwsink_agent::AgentInfo>> {
    Json(state.agents.lock().await.snapshot())
}

#[derive(Deserialize)]
struct SetPwsinkVolumeRequest {
    node_name: String,
    /// Cubic 0.0-1.0, same scale as HA's `volume_level` and `wpctl`.
    volume: f32,
}

async fn set_pwsink_volume(State(state): State<AppState>, Json(req): Json<SetPwsinkVolumeRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    let delivered = state.agents.lock().await.set_volume(&req.node_name, req.volume);
    if delivered {
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{}' to {:.0}%", req.node_name, req.volume * 100.0) }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(OutputOpResponse { ok: false, message: format!("no agent connected for '{}'", req.node_name) }),
        )
    }
}

#[derive(Deserialize)]
struct SetPwsinkMuteRequest {
    node_name: String,
    muted: bool,
}

async fn set_pwsink_mute(State(state): State<AppState>, Json(req): Json<SetPwsinkMuteRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    let delivered = state.agents.lock().await.set_mute(&req.node_name, req.muted);
    let verb = if req.muted { "muted" } else { "unmuted" };
    if delivered {
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{verb} '{}'", req.node_name) }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(OutputOpResponse { ok: false, message: format!("no agent connected for '{}'", req.node_name) }),
        )
    }
}

// ---- Sync tuning: group lead + per-device static delay -------------------
//
// The user-facing latency dials for group sync (sync_settings.rs). The group
// lead is one daemon-wide value (raise it so the slowest member still plays in
// time; lower it for a snappier start). The per-sendspin-device static delay
// trims one speaker that's consistently early/late. The AP2 per-output
// counterpart is its render delay (`latency_ms`, below), applied live.

#[derive(Serialize)]
struct SyncSettingsInfo {
    /// Group presentation lead in ms (sendspin `send_ahead`) as configured here.
    group_lead_ms: u32,
    /// The largest buffering requirement across present sendspin devices
    /// (`min_buffer_ms` + that device's static delay), in ms. The daemon raises every
    /// group's send-ahead to at least this — the spec makes it mandatory, not advisory
    /// — so configuring less than this has no effect. 0 when no device has reported one
    /// (a device only reports after it connects, and it may report a *different* value
    /// per wire codec, since decode warmup differs).
    group_lead_floor_ms: u32,
    /// What the daemon actually uses: `max(group_lead_ms, group_lead_floor_ms)`.
    group_lead_effective_ms: u32,
    /// Which device(s) set the floor, for a UI that has to explain why the value it
    /// shows is higher than the one the user typed.
    group_lead_floor_sources: Vec<LeadFloorSource>,
}

/// One device's contribution to the send-ahead floor.
#[derive(Serialize)]
struct LeadFloorSource {
    node_name: String,
    name: String,
    /// The codec it's streaming — its requirement changes with this.
    codec: &'static str,
    /// What the device itself asked for (excluding its static delay), if it reported
    /// anything. `None` for firmware that never sends `min_buffer_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    min_buffer_ms: Option<u32>,
    /// The add-on's own minimum for that codec, used when the device is silent.
    codec_minimum_ms: u32,
    /// Its static delay, which the server adds on top per the spec.
    static_delay_ms: u16,
    /// Its effective per-player send-ahead — the larger of the two, plus the delay.
    required_ms: u32,
    /// `"reported"` (the device asked for it) or `"codec-minimum"` (it didn't, so the
    /// add-on's floor for its codec applies). Lets the UI explain the number honestly.
    reason: &'static str,
}

/// The send-ahead floor across present sendspin devices, plus the per-device detail
/// behind it. Mirrors what `sync_group` feeds each group, so the UI shows the same
/// number the audio path uses.
fn lead_floor(state: &AppState) -> (u32, Vec<LeadFloorSource>) {
    let devices = state.sendspin_devices.lock_recover().clone();
    let ss = state.sync_settings.lock_recover();
    let delays = ss.sendspin_delays();
    let mut sources: Vec<LeadFloorSource> = devices
        .iter()
        .filter(|(_, d)| d.present)
        .map(|(node_name, d)| {
            let static_delay_ms = delays.get(node_name).copied().unwrap_or(0);
            let codec = crate::sendspin_server::resolve_codec(ss.sendspin_codec(node_name), std::iter::once(&d.supported_codecs));
            // Same rule the audio path uses: what the device asked for, else our floor
            // for its codec — and the device's static delay on top either way.
            let codec_minimum_ms = (crate::sendspin_codec::min_send_ahead_us(codec) / 1000) as u32;
            let (base_ms, reason) = match d.min_buffer_ms {
                Some(m) => (m, "reported"),
                None => (codec_minimum_ms, "codec-minimum"),
            };
            LeadFloorSource {
                node_name: node_name.clone(),
                name: d.display_name.clone(),
                codec,
                min_buffer_ms: d.min_buffer_ms,
                codec_minimum_ms,
                static_delay_ms,
                required_ms: base_ms + u32::from(static_delay_ms),
                reason,
            }
        })
        .filter(|s| s.required_ms > 0)
        .collect();
    // Largest first: the head is the one that actually sets the floor.
    sources.sort_by_key(|s| std::cmp::Reverse(s.required_ms));
    (sources.first().map(|s| s.required_ms).unwrap_or(0), sources)
}

#[derive(Deserialize)]
struct SetSyncSettingsRequest {
    group_lead_ms: u32,
}

async fn get_sync_settings(State(state): State<AppState>) -> Json<SyncSettingsInfo> {
    let configured = state.sync_settings.lock_recover().group_lead_ms();
    let (floor, sources) = lead_floor(&state);
    Json(SyncSettingsInfo {
        group_lead_ms: configured,
        group_lead_floor_ms: floor,
        group_lead_effective_ms: configured.max(floor),
        group_lead_floor_sources: sources,
    })
}

async fn set_sync_settings(State(state): State<AppState>, Json(req): Json<SetSyncSettingsRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    if let Err(e) = state.sync_settings.lock_recover().set_group_lead_ms(req.group_lead_ms) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
    }
    // Nudge the reconciler; it re-reads the lead each tick and restarts group
    // servers so the new value takes effect promptly.
    let _ = state.changes.send(());
    // The daemon raises the lead to the device-reported floor regardless, so a value
    // below it is stored but not used — say so rather than letting the UI imply it took.
    let (floor, sources) = lead_floor(&state);
    let message = match sources.first() {
        Some(top) if floor > req.group_lead_ms && top.reason == "reported" => format!(
            "group lead set to {} ms, but {} ms is used — '{}' asks for that much buffer with {}",
            req.group_lead_ms, floor, top.name, top.codec
        ),
        Some(top) if floor > req.group_lead_ms => format!(
            "group lead set to {} ms, but {} ms is used — {} needs that much head start to decode in time on '{}'",
            req.group_lead_ms, floor, top.codec, top.name
        ),
        _ => format!("group lead set to {} ms", req.group_lead_ms),
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

/// General app settings (settings_store.rs) — the Settings page's General
/// section. Group lead lives on `/api/sync/settings` (it's sync-specific).
#[derive(Serialize)]
struct SettingsInfo {
    default_duck: f32,
    discovery_enabled: bool,
    sendspin_delay_live: bool,
    expose_outputs_as_media_players: bool,
}

/// Partial update: every field is optional so the UI can PATCH one knob at a time.
#[derive(Deserialize)]
struct SetSettingsRequest {
    #[serde(default)]
    default_duck: Option<f32>,
    #[serde(default)]
    discovery_enabled: Option<bool>,
    #[serde(default)]
    sendspin_delay_live: Option<bool>,
    #[serde(default)]
    expose_outputs_as_media_players: Option<bool>,
}

fn settings_info(state: &AppState) -> SettingsInfo {
    let s = state.settings.lock_recover();
    SettingsInfo {
        default_duck: s.default_duck(),
        discovery_enabled: s.discovery_enabled(),
        sendspin_delay_live: s.sendspin_delay_live(),
        expose_outputs_as_media_players: s.expose_outputs_as_media_players(),
    }
}

async fn get_settings(State(state): State<AppState>) -> Json<SettingsInfo> {
    Json(settings_info(&state))
}

async fn set_settings(State(state): State<AppState>, Json(req): Json<SetSettingsRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    // Persist each provided field.
    {
        let mut s = state.settings.lock_recover();
        if let Some(d) = req.default_duck {
            if let Err(e) = s.set_default_duck(d) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }),
                );
            }
        }
        if let Some(enabled) = req.discovery_enabled {
            if let Err(e) = s.set_discovery_enabled(enabled) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }),
                );
            }
        }
        if let Some(live) = req.sendspin_delay_live {
            if let Err(e) = s.set_sendspin_delay_live(live) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }),
                );
            }
        }
        if let Some(expose) = req.expose_outputs_as_media_players {
            if let Err(e) = s.set_expose_outputs_as_media_players(expose) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }),
                );
            }
        }
    }
    // Apply the discovery flag live (outside the settings lock). A spawn failure
    // is reported but the flag stays persisted — it'll retry on next boot.
    if let Some(enabled) = req.discovery_enabled {
        if let Err(e) = state.discovery.set_enabled(enabled) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OutputOpResponse { ok: false, message: format!("failed to apply discovery: {e}") }),
            );
        }
    }
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: "settings saved".to_string() }))
}

/// S3 spike (per_device_spike.rs): stand up one per-device PipeWire node +
/// single-member sendspin sender for `device`, optionally fed from `source`.
#[derive(Deserialize)]
struct SpikeStartRequest {
    /// The discovered sendspin device's node name (`sendspin-dev-…`).
    device: String,
    /// Source node to link into the per-device sink (its audio path). Optional:
    /// without it the node is created but silent until something is routed in.
    #[serde(default)]
    source: Option<String>,
}

#[derive(Serialize)]
struct SpikeStartResponse {
    ok: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    spike: Option<crate::per_device_spike::SpikeInfo>,
}

async fn spike_per_device_start(
    State(state): State<AppState>,
    Json(req): Json<SpikeStartRequest>,
) -> (StatusCode, Json<SpikeStartResponse>) {
    let send_ahead_us = state.sync_settings.lock_recover().group_lead_us();
    match crate::per_device_spike::start(
        &req.device,
        req.source.as_deref(),
        &state.pw,
        &state.pw_cmd,
        &state.changes,
        &state.routing,
        &state.sendspin_devices,
        &state.sendspin_control,
        send_ahead_us,
    )
    .await
    {
        Ok(info) => (StatusCode::OK, Json(SpikeStartResponse { ok: true, message: info.message.clone(), spike: Some(info) })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(SpikeStartResponse { ok: false, message: e, spike: None })),
    }
}

async fn spike_per_device_stop(State(state): State<AppState>) -> (StatusCode, Json<OutputOpResponse>) {
    match crate::per_device_spike::stop(&state.pw_cmd, &state.changes, &state.routing).await {
        Ok(msg) => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: msg })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })),
    }
}

/// AirPlay-2 synchronized **test tone** spike (ap2_spike.rs). Streams a sine tone
/// to the target receivers via the PROVEN file path (`start_streaming`), bypassing
/// the live-capture producer, to check that AP2 + PTP multi-room works on the Pi.
#[derive(Deserialize)]
struct Ap2SpikeRequest {
    /// Explicit receiver IPs. If empty, all present discovered AP2 receivers.
    #[serde(default)]
    ips: Vec<String>,
    /// Tone frequency in Hz (default 440).
    #[serde(default)]
    freq: Option<f32>,
    /// Tone duration in seconds (default 60).
    #[serde(default)]
    seconds: Option<f32>,
    /// Render delay in ms (default `ap2_server::AP2_RENDER_DELAY_MS`).
    #[serde(default)]
    render_delay_ms: Option<u32>,
    /// Streaming path to exercise: `"file"` (default; `start_streaming`, known-good)
    /// or `"live"` (`start_streaming_live` + `LiveAudioDecoder`, the live-output path
    /// fed a clean synthetic tone) — the bisection knob for the live-path silence.
    #[serde(default)]
    mode: Option<String>,
    /// Wire sample rate in Hz: 44100 (default) or 48000 — to test whether the
    /// receivers accept 48 kHz realtime ALAC (drives the ALAC cookie + SETUP
    /// `audioFormat` bit). Anything ≥ 48000 is treated as 48000.
    #[serde(default)]
    rate: Option<u32>,
    /// Source clip: `"tone"` (default, generated sine) or `"voice"` (the embedded
    /// `test-announcement.mp3`, decoded + resampled to `rate`). A voice makes a
    /// wrong playback rate obvious to the ear. `"voice"` forces file mode.
    #[serde(default)]
    clip: Option<String>,
}

async fn spike_ap2_start(State(state): State<AppState>, Json(req): Json<Ap2SpikeRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    // Resolve targets: explicit IPs, else every present discovered AP2 receiver.
    let targets: Vec<(String, std::net::IpAddr)> = if !req.ips.is_empty() {
        req.ips.iter().filter_map(|s| s.parse::<std::net::IpAddr>().ok().map(|ip| (s.clone(), ip))).collect()
    } else {
        state
            .ap2_devices
            .lock_recover()
            .values()
            .filter(|d| d.present)
            .filter_map(|d| d.addr.map(|a| (d.display_name.clone(), a.ip())))
            .collect()
    };
    if targets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse { ok: false, message: "no target receivers (none discovered/present and no valid ips given)".into() }),
        );
    }

    let freq = req.freq.unwrap_or(440.0);
    let secs = req.seconds.unwrap_or(60.0);
    let delay = req.render_delay_ms.unwrap_or(crate::ap2_server::AP2_RENDER_DELAY_MS);
    let rate = if req.rate.unwrap_or(44_100) >= 48_000 { 48_000 } else { 44_100 };
    // "voice" = play the embedded test clip (decoded to WAV; the spike's file path
    // then resamples it to `rate`). A voice reveals a wrong playback rate by ear.
    // Forces file mode (live is the synthetic sine only).
    let voice = req.clip.as_deref() == Some("voice");
    let (live, file_wav) = if voice {
        match crate::decode::decode_bytes_to_wav(include_bytes!("../assets/test-announcement.mp3"), "mp3").await {
            Ok(wav) => (false, Some(wav)),
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("decode test clip: {e}") }))
            }
        }
    } else {
        (req.mode.as_deref() == Some("live"), None)
    };

    match crate::ap2_spike::start(targets, &state.ap2_ptp, freq, secs, delay, live, rate, file_wav).await {
        Ok(info) => {
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{} — {}", info.message, info.targets.join(", ")) }))
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })),
    }
}

async fn spike_ap2_stop() -> (StatusCode, Json<OutputOpResponse>) {
    let msg = crate::ap2_spike::stop().await;
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: msg }))
}

/// pw-sink transport spike (pw_sink_spike.rs). Streams a self-driving test tone
/// to a remote PipeWire host via native rtp-sink + rtp-sap — the real-LAN A/B
/// oracle for the pw-sink output backend. No user interaction; the remote host
/// (running rtp-sap in discover mode) auto-creates a source and plays it.
#[derive(Deserialize)]
struct PwSinkSpikeRequest {
    /// Target remote host, unicast IPv4 (required — rtp-sink unicasts to it).
    target_ip: String,
    /// Tone frequency in Hz (default 440).
    #[serde(default)]
    freq: Option<f32>,
    /// Optional LAN interface name to pin egress/advert to (default `end0` on
    /// the HA host — avoids host-network multi-iface fan-out).
    #[serde(default)]
    ifname: Option<String>,
}

async fn spike_pwsink_start(State(state): State<AppState>, Json(req): Json<PwSinkSpikeRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    if req.target_ip.parse::<std::net::IpAddr>().is_err() {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("invalid target_ip '{}'", req.target_ip) }));
    }
    let freq = req.freq.unwrap_or(440.0);
    let ifname = req.ifname.as_deref().or(Some("end0"));
    match crate::pw_sink_spike::start(&state.pw, &state.pw_cmd, &req.target_ip, freq, ifname).await {
        Ok(info) => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: info.message })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })),
    }
}

async fn spike_pwsink_stop() -> (StatusCode, Json<OutputOpResponse>) {
    crate::pw_sink_spike::stop().await;
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: "pw-sink spike stopped".into() }))
}

/// Multi-device shared-timeline spike (S1): one anchor + one timeline driving one
/// sender per device. Teardown reuses `spike_per_device_stop` (same slot).
#[derive(Deserialize)]
struct SpikeMultiRequest {
    /// Two or more discovered sendspin device node names.
    devices: Vec<String>,
    #[serde(default)]
    source: Option<String>,
}

async fn spike_multi_device_start(
    State(state): State<AppState>,
    Json(req): Json<SpikeMultiRequest>,
) -> (StatusCode, Json<SpikeStartResponse>) {
    let send_ahead_us = state.sync_settings.lock_recover().group_lead_us();
    match crate::per_device_spike::start_multi(
        &req.devices,
        req.source.as_deref(),
        &state.pw,
        &state.pw_cmd,
        &state.changes,
        &state.routing,
        &state.sendspin_devices,
        &state.sendspin_control,
        send_ahead_us,
    )
    .await
    {
        Ok(info) => (StatusCode::OK, Json(SpikeStartResponse { ok: true, message: info.message.clone(), spike: Some(info) })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(SpikeStartResponse { ok: false, message: e, spike: None })),
    }
}

/// Overlay spike (overlay_mixer.rs): inject a test-tone announcement overlay on
/// one output. Audible on any sendspin device with a running per-device sender —
/// i.e. any discovered device (grouped, or via its always-on idle sender).
#[derive(Deserialize)]
struct OverlayStartRequest {
    /// The sendspin device's output node name (`sendspin-dev-…`).
    device: String,
    #[serde(default)]
    seconds: Option<f32>,
    #[serde(default)]
    freq: Option<f32>,
    /// Music duck gain while the overlay plays (0–1); default 0.25.
    #[serde(default)]
    duck: Option<f32>,
}

async fn spike_overlay_start(Json(req): Json<OverlayStartRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seconds = req.seconds.unwrap_or(6.0);
    let freq = req.freq.unwrap_or(660.0);
    let duck = req.duck.unwrap_or(0.25);
    let pcm = crate::overlay_mixer::test_tone(seconds, freq, 0.3);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::overlay_mixer::OverlayMixer::global().start(&req.device, id, pcm, duck);
    (
        StatusCode::OK,
        Json(OutputOpResponse {
            ok: true,
            message: format!(
                "overlay {freq}Hz for {seconds}s on '{}' (duck {duck}); audible only if that device is on per-device senders",
                req.device
            ),
        }),
    )
}

async fn spike_overlay_stop(Query(q): Query<std::collections::HashMap<String, String>>) -> (StatusCode, Json<OutputOpResponse>) {
    match q.get("device") {
        Some(device) => {
            let stopped = crate::overlay_mixer::OverlayMixer::global().stop(device).is_some();
            (
                StatusCode::OK,
                Json(OutputOpResponse {
                    ok: true,
                    message: format!("overlay on '{device}': {}", if stopped { "stopped" } else { "none active" }),
                }),
            )
        }
        None => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: "missing ?device=".to_string() })),
    }
}

/// Announcement-group announce (announce.rs): play a clip to a set of per-device
/// outputs with per-device duck+overlay and scheduler policy (queue/barge/TTL).
///
/// Each target needs a per-device sender to *consume* its overlay, so the handler
/// first ensures one exists — including opening an **on-demand AP2 session** for a
/// receiver with nothing routed into it (sync_group.rs) — and reports any target
/// nothing can carry instead of dropping the clip silently.
#[derive(Deserialize)]
struct AgAnnounceRequest {
    /// Target output node names (`sendspin-dev-…`). Optional if
    /// `announcement_group` is given (its targets are used).
    #[serde(default)]
    targets: Vec<String>,
    /// Named announcement group (groups_store.rs) to resolve targets/priority/duck.
    #[serde(default)]
    announcement_group: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// Use the built-in test-announcement clip (no url needed).
    #[serde(default)]
    test: bool,
    /// Use the built-in calibration tone (the `calibrate.rs` click track) as a
    /// quick "is this speaker alive and correctly wired" check.
    #[serde(default)]
    tone: bool,
    #[serde(default)]
    priority: i32,
    /// "queue" (default) or "reject" when the targets are busy.
    #[serde(default)]
    on_busy: Option<String>,
    #[serde(default)]
    barge_in: bool,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    duck: Option<f32>,
}

#[derive(Serialize)]
struct AgAnnounceResponse {
    ok: bool,
    /// "playing" | "queued" | "rejected".
    admission: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    message: String,
}

/// Acquire the announce audio as 48k/S16/stereo PCM from one of test/tone/url.
async fn acquire_announce_pcm(req: &AgAnnounceRequest) -> Result<Vec<u8>, String> {
    if req.test {
        let wav = crate::decode::decode_bytes_to_wav(include_bytes!("../assets/test-announcement.mp3"), "mp3")
            .await
            .map_err(|e| format!("decode test clip: {e}"))?;
        let (rate, ch, pcm) = crate::wav::read_pcm16(&wav).ok_or("test clip not a PCM WAV")?;
        return Ok(crate::resample::to_48k_stereo_s16le(pcm, rate, ch));
    }
    if req.tone {
        // The calibration click (calibrate.rs) — already 16-bit PCM WAV, so no
        // decode step; just standardize to the announce mix format.
        let wav = crate::calibrate::click_wav();
        let (rate, ch, pcm) = crate::wav::read_pcm16(&wav).ok_or("tone clip not a PCM WAV")?;
        return Ok(crate::resample::to_48k_stereo_s16le(pcm, rate, ch));
    }
    match &req.url {
        Some(url) => {
            let path = std::env::temp_dir().join("ag-announce-fetch");
            let _ = tokio::fs::remove_file(&path).await;
            fetch_to_file(url, &path).await.map_err(|e| format!("fetch: {e}"))?;
            let pcm = crate::decode::decode_file_to_pcm_48k_stereo(&path).await.map_err(|e| format!("decode: {e}"));
            let _ = tokio::fs::remove_file(&path).await;
            pcm
        }
        None => Err("provide exactly one of: test, tone, url".to_string()),
    }
}

async fn ag_announce(State(state): State<AppState>, Json(req): Json<AgAnnounceRequest>) -> (StatusCode, Json<AgAnnounceResponse>) {
    let reject = |msg: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(AgAnnounceResponse { ok: false, admission: "rejected".into(), position: None, reason: Some(msg.clone()), message: msg }),
        )
    };

    // Resolve effective targets/priority/duck — optionally from a named
    // announcement group (explicit `targets`/`duck` in the request still win).
    // Done before the await below so the store guard doesn't span it.
    let (targets, priority, ag_duck) = {
        let mut targets = req.targets.clone();
        let mut priority = req.priority;
        let mut ag_duck = None;
        if let Some(agid) = &req.announcement_group {
            let store = state.groups_config.lock_recover();
            match store.announcement_by_id(agid) {
                Some(ag) => {
                    if targets.is_empty() {
                        targets = ag.targets.clone();
                    }
                    priority = ag.priority;
                    ag_duck = Some(ag.duck);
                }
                None => return reject(format!("no announcement group '{agid}'")),
            }
        }
        (targets, priority, ag_duck)
    };
    if targets.is_empty() {
        return reject("no targets (provide `targets` or `announcement_group`)".into());
    }
    let duck = req.duck.or(ag_duck).unwrap_or_else(|| state.settings.lock_recover().default_duck());
    let on_busy = match req.on_busy.as_deref() {
        Some("reject") => crate::announce_arbiter::OnBusy::Reject,
        _ => crate::announce_arbiter::OnBusy::Queue,
    };

    // Make sure each target actually has a sender that will *consume* the overlay.
    // An announcement is only audible while a per-device relay reads its output's
    // overlay slot, and an unrouted AP2 receiver / pw-sink target has no sender at
    // all — so this opens an on-demand session for it. Done before the (possibly
    // slow) clip fetch/decode so the connect overlaps it. Targets that nothing can
    // carry are dropped from the announcement and reported, rather than silently
    // swallowing the clip and answering "playing".
    use crate::sync_group::{AnnounceDeps, AnnounceTransport};
    let mut transports: Vec<(String, AnnounceTransport)> = Vec::with_capacity(targets.len());
    {
        let deps = AnnounceDeps {
            pw: &state.pw,
            pw_cmd: &state.pw_cmd,
            routing: &state.routing,
            outputs: &state.outputs,
            ap2_devices: &state.ap2_devices,
            ap2_ptp: &state.ap2_ptp,
            ap2_control: &state.ap2_control,
            sync_settings: &state.sync_settings,
            agents: &state.agents,
        };
        let mut groups = state.groups.lock().await;
        for target in &targets {
            let t = groups.ensure_announce_transport(target, &deps).await;
            transports.push((target.clone(), t));
        }
    }
    let skipped: Vec<String> = transports
        .iter()
        .filter_map(|(t, s)| match s {
            AnnounceTransport::Unavailable(why) => Some(format!("{} ({why})", output_label(&state, t))),
            _ => None,
        })
        .collect();
    let starting: Vec<String> =
        transports.iter().filter(|(_, s)| matches!(s, AnnounceTransport::Starting)).map(|(t, _)| output_label(&state, t)).collect();
    // A clip may sit unconsumed while an on-demand session pairs up; give the
    // mixer's stall watchdog a matching grace so it isn't reaped mid-connect.
    let grace = if transports.iter().any(|(_, s)| s.is_on_demand()) {
        crate::overlay_mixer::OVERLAY_ONDEMAND_GRACE
    } else {
        crate::overlay_mixer::OVERLAY_STALL_GRACE
    };
    let targets: Vec<String> =
        transports.into_iter().filter(|(_, s)| !matches!(s, AnnounceTransport::Unavailable(_))).map(|(t, _)| t).collect();
    if targets.is_empty() {
        return reject(format!("no target can play an announcement right now: {}", skipped.join("; ")));
    }

    let pcm = match acquire_announce_pcm(&req).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return reject("empty audio".into()),
        Err(e) => return reject(e),
    };

    let target_count = targets.len();
    // Display names captured before `targets` is moved into the coordinator, for
    // the log line below.
    let played: Vec<String> = targets.iter().map(|t| output_label(&state, t)).collect();
    let admission =
        crate::announce::AnnounceCoordinator::global().announce(targets, pcm, duck, priority, on_busy, req.barge_in, req.ttl_ms, grace);
    use crate::announce_arbiter::Admission;
    let (label, position, reason, ok) = match admission {
        Admission::Playing => ("playing", None, None, true),
        Admission::Queued { position } => ("queued", Some(position), None, true),
        Admission::Rejected(r) => ("rejected", None, Some(format!("{r:?}")), false),
    };
    // Log the whole decision, once, at INFO. An announcement makes an admission
    // decision and can silently drop targets, and until this line existed the
    // daemon logged *nothing at all* for one — an 8 h window of the live log
    // contained zero announce entries, so "the tone didn't play" could not be
    // answered from the log and had to be reconstructed with tcpdump. Everything a
    // future "it didn't play" report needs is here: what was asked for, what was
    // admitted, what is still connecting, and what was dropped and why.
    tracing::info!(
        "USER ACTION: announce -> {} target(s) [{}]: {label}{}{}",
        target_count,
        played.join(", "),
        if starting.is_empty() { String::new() } else { format!(" (on-demand: {})", starting.join(", ")) },
        if skipped.is_empty() { String::new() } else { format!(" SKIPPED: {}", skipped.join("; ")) },
    );

    let mut message = format!("announce to {target_count} target(s): {label}");
    if !starting.is_empty() {
        // Honest about the wait: the endpoint has to connect first (AP2: pair +
        // SETUP + its render buffer; pw-sink: discover our advert and handshake).
        message.push_str(&format!(" — opening an on-demand session for {} (audio starts in a few seconds)", starting.join(", ")));
    }
    if !skipped.is_empty() {
        message.push_str(&format!("; skipped {}", skipped.join("; ")));
    }
    (StatusCode::OK, Json(AgAnnounceResponse { ok, admission: label.to_string(), position, reason, message }))
}

// ---- Named groups (groups_store.rs) -------------------------------------

#[derive(Deserialize)]
struct CreateMusicGroupRequest {
    name: String,
    #[serde(default)]
    members: Vec<String>,
}
#[derive(Deserialize)]
struct UpdateMusicGroupRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    members: Option<Vec<String>>,
}
#[derive(Deserialize)]
struct CreateAnnouncementGroupRequest {
    name: String,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    duck: Option<f32>,
}
#[derive(Deserialize)]
struct UpdateAnnouncementGroupRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    targets: Option<Vec<String>>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    duck: Option<f32>,
}

async fn list_music_groups(State(state): State<AppState>) -> Json<Vec<crate::groups_store::MusicGroup>> {
    Json(state.groups_config.lock_recover().music().to_vec())
}

async fn create_music_group(
    State(state): State<AppState>,
    Json(req): Json<CreateMusicGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().create_music(&req.name, req.members) {
        Ok(mg) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": mg }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

async fn update_music_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMusicGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().update_music(&id, req.name, req.members) {
        Ok(mg) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": mg }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

async fn delete_music_group(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
    match state.groups_config.lock_recover().delete_music(&id) {
        Ok(()) => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("deleted music group '{id}'") })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e.to_string() })),
    }
}

#[derive(Deserialize)]
struct RouteGroupRequest {
    source: String,
}

/// Route a source to a whole music group: the group's members are (re)linked from
/// `source` (replacing any prior source per member), so they play it in sync. The
/// group is the routable unit; individual member re-routing is left to the raw
/// matrix. Reuses the per-output routing store + reconciler (no special-casing).
async fn route_music_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RouteGroupRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    tracing::info!("USER ACTION: route music group '{}' (routing graph)", id);
    let members = {
        let g = state.groups_config.lock_recover();
        match g.music().iter().find(|m| m.id == id) {
            Some(m) => m.members.clone(),
            None => return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("no music group '{id}'") })),
        }
    };
    if members.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: "music group has no members".into() }));
    }
    let snapshot = crate::routing_store::snapshot(&state.routing);
    {
        let mut store = state.routing.lock_recover();
        for member in &members {
            for l in snapshot.iter().filter(|l| &l.output == member && l.source != req.source) {
                let _ = store.remove(&l.source, member);
            }
            if let Err(e) = store.add(&req.source, member) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }),
                );
            }
        }
    }
    let _ = state.changes.send(());
    (
        StatusCode::OK,
        Json(OutputOpResponse { ok: true, message: format!("routed '{id}' ({} member(s)) from '{}'", members.len(), req.source) }),
    )
}

/// Un-route a whole music group: remove all links feeding its members.
async fn unroute_music_group(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
    tracing::info!("USER ACTION: unroute music group '{}' (routing graph)", id);
    let members = {
        let g = state.groups_config.lock_recover();
        match g.music().iter().find(|m| m.id == id) {
            Some(m) => m.members.clone(),
            None => return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("no music group '{id}'") })),
        }
    };
    let snapshot = crate::routing_store::snapshot(&state.routing);
    {
        let mut store = state.routing.lock_recover();
        for l in snapshot.iter().filter(|l| members.contains(&l.output)) {
            let _ = store.remove(&l.source, &l.output);
        }
    }
    let _ = state.changes.send(());
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("un-routed music group '{id}'") }))
}

async fn list_announcement_groups(State(state): State<AppState>) -> Json<Vec<crate::groups_store::AnnouncementGroup>> {
    Json(state.groups_config.lock_recover().announcement().to_vec())
}

async fn create_announcement_group(
    State(state): State<AppState>,
    Json(req): Json<CreateAnnouncementGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let duck = req.duck.unwrap_or_else(|| state.settings.lock_recover().default_duck());
    match state.groups_config.lock_recover().create_announcement(&req.name, req.targets, req.priority, duck) {
        Ok(ag) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": ag }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

async fn update_announcement_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAnnouncementGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().update_announcement(&id, req.name, req.targets, req.priority, req.duck) {
        Ok(ag) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": ag }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

async fn delete_announcement_group(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
    match state.groups_config.lock_recover().delete_announcement(&id) {
        Ok(()) => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("deleted announcement group '{id}'") })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e.to_string() })),
    }
}

/// Diagnostics snapshot for the Diagnostics page's status header.
#[derive(Serialize)]
struct StatusInfo {
    version: String,
    uptime_secs: u64,
    discovery_enabled: bool,
    /// Live PipeWire graph node count (0 while the graph is empty/unconnected).
    pipewire_nodes: usize,
    /// mDNS-discovered AirPlay-2 receivers currently tracked.
    ap2_receivers: usize,
    /// mDNS-discovered sendspin devices currently tracked.
    sendspin_devices: usize,
    /// Persisted routing links (by stable name).
    routes: usize,
    /// Host capability / weak-system assessment (CPU, RAM, RT scheduling).
    host: crate::host_assessment::HostAssessment,
}

async fn get_status(State(state): State<AppState>) -> Json<StatusInfo> {
    let pipewire_nodes = state.pw.lock_recover().nodes.len();
    let ap2_receivers = state.ap2_devices.lock_recover().len();
    let sendspin_devices = state.sendspin_devices.lock_recover().len();
    let routes = state.routing.lock_recover().links().count();
    Json(StatusInfo {
        version: state.version.clone(),
        uptime_secs: state.started.elapsed().as_secs(),
        discovery_enabled: state.discovery.is_running(),
        pipewire_nodes,
        ap2_receivers,
        sendspin_devices,
        routes,
        host: crate::host_assessment::assess(),
    })
}

// ---- Latency alignment (calibrate.rs) -----------------------------------

/// Alignable groups (a source-set with its present members), for the picker.
async fn align_groups(State(state): State<AppState>) -> Json<Vec<crate::calibrate::AlignGroup>> {
    Json(state.align.groups().await)
}

/// Current calibration state (active session or not).
async fn align_status(State(state): State<AppState>) -> Json<crate::calibrate::AlignState> {
    Json(state.align.status().await)
}

#[derive(Deserialize)]
struct AlignStartRequest {
    /// Source node names identifying the group to align (its stable identity).
    sources: Vec<String>,
}

async fn align_start(
    State(state): State<AppState>,
    Json(req): Json<AlignStartRequest>,
) -> Result<Json<crate::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    state.align.start(req.sources).await.map(Json).map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
}

#[derive(Deserialize)]
struct AlignSelectRequest {
    /// Member kept audible as the fixed reference.
    reference: String,
    /// Member being tuned (audible alongside the reference).
    target: String,
}

async fn align_select(
    State(state): State<AppState>,
    Json(req): Json<AlignSelectRequest>,
) -> Result<Json<crate::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    state
        .align
        .select(req.reference, req.target)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
}

#[derive(Deserialize)]
struct AlignVolumeRequest {
    /// Audible-member playback level, 0–100.
    volume: u8,
}

async fn align_volume(
    State(state): State<AppState>,
    Json(req): Json<AlignVolumeRequest>,
) -> Result<Json<crate::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    state
        .align
        .set_level(req.volume)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
}

/// Stop the session, restoring every member's volume.
async fn align_stop(State(state): State<AppState>) -> Json<crate::calibrate::AlignState> {
    Json(state.align.stop().await)
}

#[derive(Deserialize)]
struct SetSendspinDelayRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    node_name: String,
    /// Static delay in ms (0–5000); `0` clears it.
    delay_ms: u16,
}

async fn get_sendspin_delays(State(state): State<AppState>) -> Json<std::collections::HashMap<String, u16>> {
    Json(state.sendspin_control.lock().await.delays())
}

async fn set_sendspin_delay_handler(
    State(state): State<AppState>,
    Json(req): Json<SetSendspinDelayRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Persist first (a calibrated offset must survive restarts), then push live.
    if let Err(e) = state.sync_settings.lock_recover().set_sendspin_delay(&req.node_name, req.delay_ms) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist delay: {e}") }));
    }
    let pending = state.sendspin_control.lock().await.set_delay(&req.node_name, req.delay_ms);
    let reached = pending.apply().await;
    let ms = req.delay_ms.min(5000);

    // Current ESPHome firmware reads the static delay only at stream start, so the
    // live push above doesn't shift the running stream — the device has to reconnect
    // for it to take. Scoped to THIS device's connection on the next reconcile
    // (`force_device_reconnect`): its groupmates' streams are unaffected by its delay,
    // and restarting the group's server for them cost every speaker in the room tens
    // of seconds of silence (docs/old/sendspin-group-churn-plan.md §4.10). The one
    // genuinely group-wide case — a delay large enough to raise the group's send-ahead
    // high-water mark — is picked up by the reconciler's ordinary stream-config check.
    // Skipped entirely when `sendspin_delay_live` is on (firmware that honors a live
    // SetStaticDelay).
    let live = state.settings.lock_recover().sendspin_delay_live();
    let mut reconnecting = false;
    if !live {
        reconnecting = state.groups.lock().await.force_device_reconnect(&req.node_name);
        if reconnecting {
            let _ = state.changes.send(());
        }
    }

    let message = if !reached {
        format!("saved {ms} ms for '{}' (device not connected)", req.node_name)
    } else if reconnecting {
        format!("set '{}' static delay to {ms} ms (reconnecting just this speaker to apply)", req.node_name)
    } else {
        format!("set '{}' static delay to {ms} ms", req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
struct SetOutputNameRequest {
    /// The user's name for this output; `null`/omitted drops the override so the
    /// output goes back to the name discovery reports.
    #[serde(default)]
    name: Option<String>,
}

/// Rename an output (persisted in outputs_store.rs, keyed by node name).
///
/// A device's own mDNS name is often useless in a house (`ap2-dev-living-2`, or
/// four speakers all called "Yamaha"), so the name shown everywhere — Outputs,
/// the routing graph, group chips, the HA `media_player` — is the user's if they
/// set one. The store trims and enforces the minimum length; nothing needs to
/// restart, so this just nudges the change notifier and every matrix subscriber
/// picks the new name up on the next frame.
async fn set_output_name(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetOutputNameRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    tracing::info!("USER ACTION: rename output '{}' to {:?}", node_name, req.name);
    if let Err(e) = state.outputs.lock_recover().set_name(&node_name, req.name.as_deref()) {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e.to_string() }));
    }
    let _ = state.changes.send(());
    let message = match req.name {
        Some(_) => format!("renamed to '{}'", output_label(&state, &node_name)),
        None => format!("'{}' uses its discovered name again", output_label(&state, &node_name)),
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
struct SetOutputLatencyRequest {
    /// Receiver latency in ms; `null`/omitted resets to the type's default.
    latency_ms: Option<u16>,
}

/// Set an AirPlay-2 output's per-output **render delay** (ms), persisted per node
/// name in sync_settings.rs. `latency_ms: null` clears the override (back to the
/// sender's default). This is the only per-output latency knob now that the RAOP
/// output path is gone.
///
/// There's no PipeWire module to reload: the value is applied **live** to the
/// running stream (the PT=87 anchor offset the streamer reads per packet) with
/// no reconnect, and reused as the initial delay on the next (membership/rate)
/// reconnect. Only the upper bound is enforced (the receiver's negotiated
/// buffer); a low delay is allowed through even though it risks dropouts, since
/// finding that threshold is exactly what this knob is for.
async fn set_output_latency(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetOutputLatencyRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if !node_name.starts_with(AP2_DEV_PREFIX) {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse { ok: false, message: format!("'{node_name}' is not an AirPlay-2 output") }),
        );
    }
    let clamped = req.latency_ms.map(|ms| ms.min(crate::ap2_server::AP2_RENDER_DELAY_MAX_MS));
    if let Err(e) = state.sync_settings.lock_recover().set_ap2_latency(&node_name, clamped) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: format!("failed to persist latency: {e}") }),
        );
    }
    // Apply live to the streaming session (no-op if not currently streaming —
    // the persisted value then applies on the next connect).
    let effective = clamped.unwrap_or(crate::ap2_server::AP2_RENDER_DELAY_MS as u16);
    state.ap2_control.lock().await.set_render_delay(&node_name, effective).await;
    // `latency_ms` is on the routing matrix, and the matrix is only pushed when
    // something says it changed — this used to reach the graph on the next 250 ms
    // meter tick, which no longer carries it (routing.rs `Frame::Meters`).
    let _ = state.changes.send(());
    let latency_label = match clamped {
        Some(ms) => format!("{ms} ms"),
        None => "default".to_string(),
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{node_name}' render delay to {latency_label} (live)") }))
}

#[derive(serde::Deserialize)]
struct SetAp2RateModeRequest {
    /// `"auto"` (negotiate 48 kHz, fall back to 44.1 kHz) or `"fixed_44100"`.
    mode: String,
}

/// Set an AP2 output's wire-rate mode (persisted in sync_settings.rs) and nudge the
/// reconciler so the group re-negotiates + restarts at the new rate. Choosing `auto`
/// also clears any learned 44.1k cap so 48 kHz is re-probed.
/// Per-sendspin-output wire codec (`{"codec": "auto"|"pcm"|"opus"|"flac"}`).
///
/// Rejects a codec that isn't currently selectable — the daemon can't encode it, or
/// the device didn't advertise it — with the same reason the picker shows, instead of
/// storing a choice that would silently fall back to PCM. The stream carries one
/// format for a whole group, so the change takes effect by restarting that group's
/// sendspin server (the codec is part of its restart identity).
#[derive(Deserialize)]
struct SetSendspinCodecRequest {
    codec: String,
}

async fn set_sendspin_codec(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetSendspinCodecRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if !node_name.starts_with(SENDSPIN_DEV_PREFIX) {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("'{node_name}' is not a sendspin output") }));
    }
    let Some(codec) = crate::sync_settings::SendspinCodec::parse(&req.codec) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse { ok: false, message: format!("unknown codec '{}' (use auto, pcm, opus or flac)", req.codec) }),
        );
    };
    // An explicit pick must be usable right now; `auto` always is.
    if let Some(name) = codec.explicit_codec() {
        let device_codecs = state.sendspin_devices.lock_recover().get(&node_name).map(|d| d.supported_codecs.clone()).unwrap_or_default();
        let (_, _, options) = sendspin_codec_info(&node_name, &device_codecs, &state.sync_settings.lock_recover());
        if let Some(opt) = options.iter().find(|o| o.codec == name) {
            if !opt.available {
                let why = opt.reason.clone().unwrap_or_else(|| "not available".into());
                return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("{name} is not available: {why}") }));
            }
        }
    }
    if let Err(e) = state.sync_settings.lock_recover().set_sendspin_codec(&node_name, codec) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist codec: {e}") }));
    }
    // Codec is part of the sendspin server's restart identity → the group restarts
    // and sends a fresh stream/start with the new format.
    let _ = state.changes.send(());
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("codec for '{node_name}' set to {}", codec.as_str()) }))
}

async fn set_ap2_rate_mode(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetAp2RateModeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if !node_name.starts_with(AP2_DEV_PREFIX) {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse { ok: false, message: format!("'{node_name}' is not an AirPlay-2 output") }),
        );
    }
    let mode = match req.mode.as_str() {
        "auto" => crate::sync_settings::Ap2RateMode::Auto,
        "fixed_44100" | "fixed44100" | "44100" => crate::sync_settings::Ap2RateMode::Fixed44100,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(OutputOpResponse { ok: false, message: format!("unknown rate mode '{other}' (use 'auto' or 'fixed_44100')") }),
            );
        }
    };
    if let Err(e) = state.sync_settings.lock_recover().set_ap2_rate_mode(&node_name, mode) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: format!("failed to persist rate mode: {e}") }),
        );
    }
    // Rate is part of the AP2 restart identity → the group re-negotiates + restarts.
    let _ = state.changes.send(());
    let label = match mode {
        crate::sync_settings::Ap2RateMode::Auto => "auto (negotiate 48 kHz)",
        crate::sync_settings::Ap2RateMode::Fixed44100 => "fixed 44.1 kHz",
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{node_name}' sample rate to {label} (applies shortly)") }))
}

async fn fetch_to_file(url: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let response = reqwest::get(url).await?.error_for_status()?;
    let bytes = response.bytes().await?;
    tokio::fs::write(path, &bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay_source::DEFAULT_AIRPLAY_LATENCY_MSEC;
    use crate::sources_store::LEGACY_RTP_ID;

    /// `/api/outputs/discovered` is a *static* sibling of `/api/outputs/{node_name}`
    /// (the remove route) — same segment count. axum panics on genuinely conflicting
    /// routes at build time, i.e. at daemon startup where nothing would catch it, so
    /// pin the pairing down here: registering both must be accepted, and the static
    /// path must win for the listing.
    #[test]
    fn discovered_listing_and_remove_routes_coexist() {
        let app: Router = Router::new()
            .route("/api/outputs", get(|| async { "all" }))
            .route("/api/outputs/discovered", get(|| async { "offered" }))
            .route("/api/outputs/{node_name}", delete(|| async { "removed" }))
            .route("/api/outputs/{node_name}/adopt", post(|| async { "added" }));
        // Building it is the assertion (a conflict would have panicked above).
        drop(app);
    }

    /// `/api/now_playing/report` is a *static* sibling of
    /// `/api/now_playing/{node_name}` — the same trap as the discovered/remove
    /// pairing above, and a conflict would panic at daemon startup. It also pins
    /// the precedence: a source could in principle be named `report`.
    #[test]
    fn now_playing_report_and_node_routes_coexist() {
        let app: Router = Router::new()
            .route("/api/now_playing", get(|| async { "all" }))
            .route("/api/now_playing/report", post(|| async { "reported" }))
            .route("/api/now_playing/{node_name}", get(|| async { "one" }).put(|| async { "updated" }))
            .route("/api/now_playing/{node_name}/artwork", get(|| async { "art" }));
        drop(app);
    }

    /// The wire shape a remote reporter sends: its port alongside the metadata
    /// fields *flattened*, not nested. The Pi's reporter is written against this.
    #[test]
    fn a_report_body_is_the_port_plus_flattened_metadata() {
        let req: NowPlayingReportRequest =
            serde_json::from_str(r#"{"rtp_port":46000,"title":"Song","artist":"Artist","state":"playing","position_ms":1234}"#)
                .expect("parses");
        assert_eq!(req.rtp_port, 46000);
        assert_eq!(req.metadata.title.as_deref(), Some("Song"));
        assert_eq!(req.metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(req.metadata.state, Some(crate::now_playing::PlaybackState::Playing));
        assert_eq!(req.metadata.position_ms, Some(1234));
    }

    /// A bare port with no fields is how a reporter says "nothing is playing" —
    /// `report_now_playing` turns that into a clear, so it must parse, and
    /// `is_empty()` must recognize it.
    #[test]
    fn a_report_body_may_carry_no_metadata_at_all() {
        let req: NowPlayingReportRequest = serde_json::from_str(r#"{"rtp_port":46001}"#).expect("parses");
        assert!(req.metadata.is_empty());
    }

    fn airplay_entry() -> SourceEntry {
        SourceEntry {
            id: "kitchen-airplay".to_string(),
            label: "Kitchen AirPlay".to_string(),
            config: SourceConfig::Airplay(AirplaySourceConfig { latency_msec: 100, auth_setup: false, prevent_takeover: true, port: 5000 }),
        }
    }

    fn rtp_entry() -> SourceEntry {
        SourceEntry {
            id: "garage-bridge".to_string(),
            label: "Garage Bridge".to_string(),
            config: SourceConfig::Rtp(RtpSourceConfig {
                port: 47000,
                latency_msec: 200,
                source_addr: "0.0.0.0".to_string(),
                ignore_ssrc: true,
                rate: 48000,
            }),
        }
    }

    #[test]
    fn source_view_airplay_shape() {
        let view = source_view(&airplay_entry(), true, &[]);
        assert_eq!(view.id, "kitchen-airplay");
        assert_eq!(view.kind, SourceKind::Airplay);
        assert!(view.present); // passed through verbatim
        assert_eq!(view.node_name, "airplay-in-kitchen-airplay");
        assert!(view.airplay.is_some());
        assert!(view.rtp.is_none()); // exactly one config populated

        // Exact JSON: nested `airplay` object (flat 4 knobs), `rtp` null.
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["kind"], "airplay");
        assert_eq!(json["present"], true);
        assert_eq!(json["node_name"], "airplay-in-kitchen-airplay");
        assert_eq!(json["rtp"], serde_json::Value::Null);
        assert_eq!(json["airplay"]["latency_msec"], 100);
        assert_eq!(json["airplay"]["auth_setup"], false);
        assert_eq!(json["airplay"]["prevent_takeover"], true);
        assert_eq!(json["airplay"]["port"], 5000);
        // The nested config must NOT carry the `kind` tag (that's flat-shape only).
        assert!(json["airplay"].get("kind").is_none());
    }

    #[test]
    fn source_view_rtp_shape() {
        let view = source_view(&rtp_entry(), false, &[]);
        assert_eq!(view.kind, SourceKind::Rtp);
        assert!(!view.present);
        assert_eq!(view.node_name, "rtp-in-garage-bridge");
        assert!(view.airplay.is_none());
        assert!(view.rtp.is_some());

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["kind"], "rtp");
        assert_eq!(json["present"], false);
        assert_eq!(json["airplay"], serde_json::Value::Null);
        assert_eq!(json["rtp"]["port"], 47000);
        assert_eq!(json["rtp"]["latency_msec"], 200);
        assert_eq!(json["rtp"]["source_addr"], "0.0.0.0");
        assert_eq!(json["rtp"]["ignore_ssrc"], true);
        assert_eq!(json["rtp"]["rate"], 48000);
        // No discovered bridges passed in → no diagnostics offer.
        assert_eq!(json["bridge"], serde_json::Value::Null);
    }

    /// A discovered bridge for `rtp_entry()`'s port (47000, unicast `0.0.0.0`).
    fn discovered_bridge(name: &str, port: u16, dest: &str, diag_ok: bool) -> crate::bt_bridge_discovery::BtBridge {
        crate::bt_bridge_discovery::BtBridge {
            fullname: format!("{name}._pwrouter-btbridge._tcp.local."),
            display_name: name.to_string(),
            hostname: "bridge.local.".into(),
            addr: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 178, 78))),
            diag_port: 8080,
            diag_path: "/".into(),
            stream: crate::bt_bridge_discovery::BridgeStream {
                rtp_port: port,
                rtp_dest: dest.into(),
                rate: 48_000,
                channels: 2,
                format: "S16LE".into(),
            },
            probe: Some((std::time::Instant::now(), diag_ok)),
        }
    }

    #[test]
    fn source_view_attaches_a_matching_bridge() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "239.255.42.42", true)];
        let view = source_view(&rtp_entry(), true, &bridges);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["bridge"]["display_name"], "Bathroom");
        assert_eq!(json["bridge"]["rtp_port"], 47000);
        assert_eq!(json["bridge"]["hostname"], "bridge.local", "trailing mDNS dot trimmed for display");
        assert_eq!(json["bridge"]["diag_url"], "http://192.168.178.78:8080/");
        assert_eq!(json["bridge"]["diag_ok"], true);
    }

    #[test]
    fn source_view_reports_a_bridge_whose_page_is_down() {
        // Still matched (so the UI can name the sender) but not linkable: the
        // advert is installed by setup_pi_bridge.py and outlives the app.
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", false)];
        let view = source_view(&rtp_entry(), true, &bridges);
        assert_eq!(serde_json::to_value(&view).unwrap()["bridge"]["diag_ok"], false);
    }

    #[test]
    fn source_view_ignores_a_bridge_on_another_port() {
        let bridges = vec![discovered_bridge("Elsewhere", 46000, "0.0.0.0", true)];
        assert!(source_view(&rtp_entry(), true, &bridges).bridge.is_none());
    }

    #[test]
    fn airplay_sources_never_get_a_bridge() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", true)];
        assert!(source_view(&airplay_entry(), true, &bridges).bridge.is_none());
    }

    #[test]
    fn adopted_bridges_drop_out_of_the_discovered_offer() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", true), discovered_bridge("Garage", 46000, "0.0.0.0", true)];
        // rtp_entry() listens on 47000, so it claims "Bathroom".
        let sources = vec![source_view(&rtp_entry(), true, &bridges)];
        let offered = unmatched_bridges(&bridges, &sources);
        assert_eq!(
            offered.iter().map(|b| b.display_name.as_str()).collect::<Vec<_>>(),
            ["Garage"],
            "an already-configured bridge must not be offered again on a taken port"
        );
    }

    #[test]
    fn every_bridge_is_offered_when_nothing_is_configured() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", true)];
        assert_eq!(unmatched_bridges(&bridges, &[]).len(), 1);
    }

    #[test]
    fn source_view_uses_legacy_node_name() {
        // Legacy ids collapse to the bare node names so routing links resolve.
        let mut e = rtp_entry();
        e.id = LEGACY_RTP_ID.to_string();
        assert_eq!(source_view(&e, true, &[]).node_name, "bt-bridge-rtp");
    }

    #[test]
    fn create_request_config_defaults() {
        // `airplay`/`rtp` omitted → full defaults; a partial object fills the rest.
        let full_default: CreateSourceRequest = serde_json::from_str(r#"{"label":"X","kind":"airplay"}"#).unwrap();
        assert!(full_default.airplay.is_none()); // handler applies default
        assert_eq!(AirplaySourceConfig::default().latency_msec, DEFAULT_AIRPLAY_LATENCY_MSEC);

        let partial: CreateSourceRequest = serde_json::from_str(r#"{"label":"X","kind":"rtp","rtp":{"port":46000}}"#).unwrap();
        let rtp = partial.rtp.unwrap();
        assert_eq!(rtp.port, 46000);
        assert_eq!(rtp.latency_msec, DEFAULT_RTP_LATENCY_MSEC); // filled by serde default
        assert_eq!(rtp.rate, DEFAULT_RTP_RATE);

        // The omitted-object fallback the handler uses.
        assert_eq!(default_rtp_config().port, DEFAULT_RTP_PORT);
    }
}
