//! REST API: health check, live PipeWire registry state, and manual link
//! creation

use crate::airplay_source::AirplayHandle;
use crate::airplay_source::DEFAULT_AIRPLAY_LATENCY_MSEC;
use crate::ap2_discovery::SharedAp2Devices;
use airplay_core::features::Features;
use crate::ap2_ptp::SharedAp2Ptp;
use crate::config::{AP2_DEV_PREFIX, SENDSPIN_DEV_PREFIX, SENDSPIN_NODE_PREFIX};
use crate::locks::LockRecover;
use crate::pw_thread::{ChangeNotifier, LinkSpec, PwCommand, PwCommandSender, SharedState};
use crate::routing;
use crate::routing_store::SharedRouting;
use crate::rtp_source::{
    rtp_source_module_args, DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_PORT, DEFAULT_RTP_SOURCE_ADDR,
    RTP_SOURCE_MODULE_NAME, RTP_SOURCE_NODE_NAME,
};
use crate::sendspin_discovery::SharedSendspinDevices;
use crate::settings_store::SharedSettings;
use crate::sources_store::{RtpSourceConfig, SourcesStore};
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

/// The running AirPlay-receive source (airplay_source.rs), if configured —
/// a native embedded RAOP server feeding a PipeWire source node. `tokio` mutex
/// since start/stop `.await`. `None` = disabled.
pub type SharedAirplay = Arc<tokio::sync::Mutex<Option<AirplayHandle>>>;

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
    /// Remembered AirPlay senders (airplay_clients.rs) — the Sources-tab
    /// connection list and the base for later priority/ban controls.
    pub airplay_clients: crate::airplay_clients::SharedAirplayClients,
    /// Live anti-takeover flag, shared with the running receiver's session gate
    /// so the API can toggle it without a restart.
    pub airplay_prevent_takeover: crate::airplay_source::SharedPreventTakeover,
    /// On-demand source peak meters (metering.rs); taps live only while a
    /// routing-matrix WS client is connected.
    pub meters: crate::metering::SharedMeters,
    /// Live mDNS-discovered sendspin devices (sendspin_discovery.rs), surfaced
    /// as virtual routing outputs.
    pub sendspin_devices: SharedSendspinDevices,
    /// Live mDNS-discovered AirPlay-2 receivers (ap2_discovery.rs), surfaced as
    /// virtual routing outputs (`ap2-dev-*`). The RAOP-output replacement.
    pub ap2_devices: SharedAp2Devices,
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
    /// Persistent sync/latency tuning (sync_settings.rs): the group presentation
    /// lead + per-sendspin-device static delays.
    pub sync_settings: crate::sync_settings::SharedSyncSettings,
    /// General app settings (settings_store.rs): announce default duck, mDNS
    /// discovery on/off.
    pub settings: SharedSettings,
    /// Runtime mDNS on/off, driven by the discovery flag above.
    pub discovery: crate::discovery_supervisor::DiscoverySupervisor,
    /// Latency-alignment session manager (calibrate.rs) for the Align page.
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
    airplay_clients: crate::airplay_clients::SharedAirplayClients,
    airplay_prevent_takeover: crate::airplay_source::SharedPreventTakeover,
    meters: crate::metering::SharedMeters,
    sendspin_devices: SharedSendspinDevices,
    ap2_devices: SharedAp2Devices,
    ap2_ptp: SharedAp2Ptp,
    routing: SharedRouting,
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
        airplay_prevent_takeover,
        meters,
        sendspin_devices,
        ap2_devices,
        ap2_ptp,
        routing,
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
        .route("/api/outputs/{node_name}/latency", put(set_output_latency))
        .route("/api/outputs/{node_name}/ap2-rate", put(set_ap2_rate_mode))
        .route("/api/source/airplay", get(get_airplay_source).put(set_airplay_source).delete(delete_airplay_source))
        .route("/api/source/airplay/clients", get(list_airplay_clients))
        .route("/api/source/airplay/clients/forget", post(forget_airplay_client))
        .route("/api/source/airplay/clients/ban", post(ban_airplay_client))
        .route("/api/source/airplay/clients/priority", post(set_airplay_client_priority))
        .route("/api/source/airplay/clients/disconnect", post(disconnect_airplay_client))
        .route("/api/source/airplay/policy", put(set_airplay_policy))
        .route("/api/source/rtp", get(get_rtp_source).put(set_rtp_source).delete(delete_rtp_source))
        .route("/api/sendspin/volumes", get(get_sendspin_volumes))
        .route("/api/sendspin/volume", put(set_sendspin_volume))
        .route("/api/sendspin/mute", put(set_sendspin_mute))
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
        .route("/api/media_players", get(list_media_players))
        .route("/api/media_players/{node_id}/volume", get(get_volume).post(set_volume))
        .route("/api/media_players/{node_id}/announce", post(announce))
        .route("/api/routing", get(routing::get_routing))
        .route("/api/routing/link", post(routing::link))
        .route("/api/routing/unlink", post(routing::unlink))
        .route("/api/routing/entity/{node_name}", delete(routing::forget_entity))
        .route("/api/routing/ws", get(routing::routing_ws))
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
    let cache = if key.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
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

/// An output for the Outputs tab. Covers discovered sendspin devices and
/// AirPlay-2 receivers, in both origins the UI shows:
/// - **discovered**: present via mDNS — `present: true`, `configured: false`.
/// - **offline**: referenced by saved routing intent but not currently
///   discovered — `present: false` (shown grayed; re-linked when it returns).
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
struct OutputInfo {
    node_name: String,
    name: String,
    /// `"sendspin"` or `"airplay2"` — for the Type column / badge.
    kind: &'static str,
    /// Node/device is live right now.
    present: bool,
    /// Always `false` now that every output is mDNS auto-discovered (kept for
    /// the API shape / a possible future manually-added output kind).
    configured: bool,
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
}

#[derive(Serialize)]
struct OutputOpResponse {
    ok: bool,
    message: String,
}

async fn list_outputs(State(state): State<AppState>) -> Json<Vec<OutputInfo>> {
    use std::collections::BTreeSet;

    let mut outputs: Vec<OutputInfo> = Vec::new();

    // Discovered sendspin devices (present) + any offline ones still referenced
    // by saved routing intent — so users see every routable output.
    let devices = state.sendspin_devices.lock_recover().clone();
    let mut sendspin_names: BTreeSet<String> = devices.keys().cloned().collect();
    sendspin_names.extend(state.routing.lock_recover().referenced_outputs().into_iter().filter(|n| n.starts_with(SENDSPIN_DEV_PREFIX)));
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
        outputs.push(OutputInfo {
            kind: "sendspin",
            present,
            configured: false, // sendspin devices are always auto-discovered
            name,
            ip: addr.map(|a| a.ip().to_string()),
            port: addr.map(|a| a.port()),
            encryption: Some("None".to_string()),
            latency_ms: None,
            ptp_locked: None,   // sendspin has no PTP
            ptp_lock_age_s: None,
            ptp_supported: None,
            ptp_relevant: None,
            ap2_features: None,
            ap2_rate_mode: None,
            ap2_rate: None,
            ap2_volume: None,
            ap2_muted: None,
            node_name,
        });
    }

    // Discovered AirPlay-2 receivers (present) + offline ones still referenced by
    // saved routing intent. These are the RAOP-output replacement; like sendspin
    // devices they're virtual (no PipeWire node) and always auto-discovered.
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
    ap2_names.extend(state.routing.lock_recover().referenced_outputs().into_iter().filter(|n| n.starts_with(AP2_DEV_PREFIX)));
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
        let ptp_locked = if present {
            Some(ptp_age.is_some_and(|age| age <= std::time::Duration::from_secs(5)))
        } else {
            None
        };
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
            Some(
                !my.is_empty()
                    && ap2_present_nodes
                        .iter()
                        .any(|o| o != &node_name && crate::routing::source_set_of(&ap2_intent, o) == my),
            )
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
            name,
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
            node_name,
        });
    }

    Json(outputs)
}

// ---- AirPlay-receive source -----------------------------------------------
//
// The AirPlay-receive source is an embedded, native RAOP receiver
// (airplay_source.rs) — not a subprocess and not a PipeWire module. Its
// enabled/disabled state and knobs are persisted in the sources store
// (sources_store.rs), which starts empty on a fresh install (no options.json
// seeding) and is then authoritative. Same "runtime, no restart" model as
// /api/outputs, but backed by an in-process receiver rather than a module.

#[derive(Serialize)]
struct AirplaySourceInfo {
    /// `None` when the source is disabled.
    name: Option<String>,
    /// Whether the embedded AirPlay receiver is running right now.
    running: bool,
    /// Producer jitter-buffer target in ms. Higher = fewer stutters, more latency.
    latency_msec: u32,
    /// Whether the auth-setup encryption mode is advertised (`et=0,4`), letting
    /// encryption-requiring senders connect (default off = unencrypted only).
    auth_setup: bool,
    /// Whether a new sender is refused while one is already streaming
    /// (anti-takeover). Toggled via `PUT /api/source/airplay/policy`.
    prevent_takeover: bool,
}

#[derive(Deserialize)]
struct SetAirplaySourceRequest {
    /// The advertised AirPlay name; an empty string disables the source.
    name: String,
    /// Jitter buffer target in ms; omitted by older clients, so it defaults.
    #[serde(default = "default_airplay_source_latency_msec")]
    latency_msec: u32,
    /// Advertise the auth-setup encryption mode; omitted by older clients.
    #[serde(default)]
    auth_setup: bool,
}

fn default_airplay_source_latency_msec() -> u32 {
    DEFAULT_AIRPLAY_LATENCY_MSEC
}

async fn get_airplay_source(State(state): State<AppState>) -> Json<AirplaySourceInfo> {
    let (name, latency_msec, auth_setup, prevent_takeover) = {
        let s = state.sources.lock_recover();
        (
            s.airplay_source_name().map(str::to_string),
            s.airplay_latency_msec(),
            s.airplay_auth_setup(),
            s.airplay_prevent_takeover(),
        )
    };
    let running = state.airplay.lock().await.is_some();
    Json(AirplaySourceInfo { name, running, latency_msec, auth_setup, prevent_takeover })
}

/// Stop the current AirPlay receiver (if any). Caller holds no locks.
async fn stop_airplay(state: &AppState) {
    if let Some(handle) = state.airplay.lock().await.take() {
        handle.stop().await;
    }
}

async fn set_airplay_source(
    State(state): State<AppState>,
    Json(req): Json<SetAirplaySourceRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // NOTE: this RESTARTS the 'airplay-in' receiver (stops + re-starts the shairplay
    // server + producer node), which cascades to a full sync-group rebuild. Logged as
    // a USER ACTION so such a restart in the logs is attributable to a human, not a bug.
    tracing::info!("USER ACTION: set AirPlay source (name={:?}) — restarts the 'airplay-in' receiver", req.name);
    // Persist first (normalizes empty -> None), then reconcile the receiver.
    let (stored, latency, auth_setup) = {
        let mut sources = state.sources.lock_recover();
        if let Err(e) = sources.set_airplay_source_name(Some(req.name.clone())) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
        }
        if let Err(e) = sources.set_airplay_latency_msec(req.latency_msec) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
        }
        if let Err(e) = sources.set_airplay_auth_setup(req.auth_setup) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
        }
        (sources.airplay_source_name().map(str::to_string), sources.airplay_latency_msec(), sources.airplay_auth_setup())
    };

    // Tear down any existing receiver, then (re)start for the new name.
    stop_airplay(&state).await;
    match stored {
        Some(name) => match crate::airplay_source::start(
            name.clone(),
            latency,
            auth_setup,
            state.airplay_clients.clone(),
            state.airplay_prevent_takeover.clone(),
        )
        .await
        {
            Ok(handle) => {
                *state.airplay.lock().await = Some(handle);
                (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("AirPlay source set to '{name}'") }))
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(OutputOpResponse { ok: false, message: format!("saved '{name}' but failed to start it: {e}") }),
            ),
        },
        None => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: "AirPlay source disabled".to_string() })),
    }
}

async fn delete_airplay_source(State(state): State<AppState>) -> (StatusCode, Json<OutputOpResponse>) {
    tracing::info!("USER ACTION: delete/disable AirPlay source — stops the 'airplay-in' receiver");
    if let Err(e) = state.sources.lock_recover().set_airplay_source_name(None) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
    }
    stop_airplay(&state).await;
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: "AirPlay source disabled".to_string() }))
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

async fn list_airplay_clients(State(state): State<AppState>) -> Json<Vec<AirplayClientInfo>> {
    let clients = state
        .airplay_clients
        .lock_recover()
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
        .collect();
    Json(clients)
}

#[derive(Deserialize)]
struct ForgetClientRequest {
    key: String,
}

async fn forget_airplay_client(
    State(state): State<AppState>,
    Json(req): Json<ForgetClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let forgotten = state.airplay_clients.lock_recover().forget(&req.key);
    if forgotten {
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("forgot AirPlay client '{}'", req.key) }))
    } else {
        // Not found, or still connected (a live client can't be forgotten).
        (
            StatusCode::CONFLICT,
            Json(OutputOpResponse {
                ok: false,
                message: format!("could not forget '{}' (unknown or still connected)", req.key),
            }),
        )
    }
}

#[derive(Deserialize)]
struct BanClientRequest {
    key: String,
    banned: bool,
}

/// Ban/unban a remembered client. A ban is enforced at the next session start
/// (`authorize_session`); it does not evict a client that's already streaming.
async fn ban_airplay_client(
    State(state): State<AppState>,
    Json(req): Json<BanClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let ok = state.airplay_clients.lock_recover().set_banned(&req.key, req.banned);
    if ok {
        let verb = if req.banned { "banned" } else { "unbanned" };
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{verb} AirPlay client '{}'", req.key) }))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(OutputOpResponse { ok: false, message: format!("unknown AirPlay client '{}'", req.key) }),
        )
    }
}

#[derive(Deserialize)]
struct SetPriorityRequest {
    key: String,
    priority: i32,
}

/// Set a client's takeover priority. A connecting client with a strictly higher
/// priority than the current one takes the session over (see `authorize_session`).
async fn set_airplay_client_priority(
    State(state): State<AppState>,
    Json(req): Json<SetPriorityRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let ok = state.airplay_clients.lock_recover().set_priority(&req.key, req.priority);
    if ok {
        (
            StatusCode::OK,
            Json(OutputOpResponse { ok: true, message: format!("set priority {} for '{}'", req.priority, req.key) }),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(OutputOpResponse { ok: false, message: format!("unknown AirPlay client '{}'", req.key) }),
        )
    }
}

#[derive(Deserialize)]
struct DisconnectClientRequest {
    key: String,
}

/// Force-disconnect a currently-connected client by dropping its RTSP
/// connection (the receiver stops its stream shortly after).
async fn disconnect_airplay_client(
    State(state): State<AppState>,
    Json(req): Json<DisconnectClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Resolve the key to the live peer IP the RAOP server keys connections on.
    let addr = state.airplay_clients.lock_recover().connected_addr(&req.key);
    let Some(addr) = addr else {
        return (
            StatusCode::CONFLICT,
            Json(OutputOpResponse { ok: false, message: format!("'{}' is not currently connected", req.key) }),
        );
    };
    match &*state.airplay.lock().await {
        Some(handle) => {
            handle.disconnect_client(&addr);
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("disconnecting AirPlay client '{}'", req.key) }))
        }
        None => (
            StatusCode::CONFLICT,
            Json(OutputOpResponse { ok: false, message: "AirPlay source is not running".to_string() }),
        ),
    }
}

#[derive(Deserialize)]
struct SetAirplayPolicyRequest {
    prevent_takeover: bool,
}

/// Toggle the anti-takeover policy. Updates the live flag the receiver's session
/// gate reads (no restart, so the current stream is undisturbed) and persists it.
async fn set_airplay_policy(
    State(state): State<AppState>,
    Json(req): Json<SetAirplayPolicyRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if let Err(e) = state.sources.lock_recover().set_airplay_prevent_takeover(req.prevent_takeover) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
    }
    state.airplay_prevent_takeover.store(req.prevent_takeover, std::sync::atomic::Ordering::Relaxed);
    let msg = if req.prevent_takeover {
        "AirPlay: new senders refused while one is streaming"
    } else {
        "AirPlay: new senders may take over the current stream"
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: msg.to_string() }))
}

// ---- RTP source (Bluetooth bridge firmware target) ------------------------
//
// A single source, but — unlike the AirPlay source — a native PipeWire module,
// not a subprocess. So it's loaded/unloaded via the PipeWire thread
// (PwCommand::Load/Unload, keyed by RTP_SOURCE_NODE_NAME), rather than through
// the process supervisor. Enable/disable and
// re-point the port live, no restart. Once loaded, its node shows up in the
// routing matrix automatically (routing.rs classifies it as a source).

#[derive(Serialize)]
struct RtpSourceInfo {
    /// Whether the source is enabled in the store.
    enabled: bool,
    /// UDP port it listens on (the stored value, or the default when disabled).
    port: u16,
    /// Receiver-side jitter buffer target in ms (stored value, or default when
    /// disabled). Higher = more dropout tolerance on a weak link, more latency.
    latency_msec: u32,
    /// `source.ip`: `0.0.0.0` = unicast, or a multicast group so several
    /// receivers can share one firmware stream. Stored value, or default.
    source_addr: String,
    /// `sess.ignore-ssrc`: `true` accepts any sender on the port, `false` locks
    /// onto the first SSRC and rejects the rest ("Only one client"). Stored
    /// value, or default. Needs a stable-SSRC firmware to use `false`.
    ignore_ssrc: bool,
    /// Whether the `bt-bridge-rtp` node is actually present in the live
    /// registry right now — the module analogue of the AirPlay source's
    /// `running`. Can lag briefly after enabling, or be `false` if libpipewire
    /// refused the load.
    loaded: bool,
}

#[derive(Deserialize)]
struct SetRtpSourceRequest {
    /// UDP port to listen on; must match the firmware's configured target port.
    #[serde(default = "default_rtp_source_port")]
    port: u16,
    /// Jitter buffer target in ms; omitted by older clients, so it defaults.
    #[serde(default = "default_rtp_source_latency_msec")]
    latency_msec: u32,
    /// `source.ip` to bind: `0.0.0.0` unicast, or a multicast group. Older
    /// clients omit it, so it defaults to unicast.
    #[serde(default = "default_rtp_source_addr")]
    source_addr: String,
    /// `sess.ignore-ssrc`: `true` accept any sender, `false` lock onto the first
    /// SSRC ("Only one client"). Older clients omit it, so it defaults to `true`.
    #[serde(default = "default_rtp_source_ignore_ssrc")]
    ignore_ssrc: bool,
}

fn default_rtp_source_port() -> u16 {
    DEFAULT_RTP_PORT
}

fn default_rtp_source_latency_msec() -> u32 {
    DEFAULT_RTP_LATENCY_MSEC
}

fn default_rtp_source_addr() -> String {
    DEFAULT_RTP_SOURCE_ADDR.to_string()
}

fn default_rtp_source_ignore_ssrc() -> bool {
    DEFAULT_RTP_IGNORE_SSRC
}

/// Whether the RTP source node is present in the live registry right now.
fn rtp_source_loaded(pw: &SharedState) -> bool {
    pw.lock_recover().nodes.values().any(|n| n.node_name == RTP_SOURCE_NODE_NAME)
}

/// (Re)load the rtp-source module on `port`. Unloads any existing instance
/// first so a re-enable or a port change is a clean reload — `Load` errors if
/// a module is already registered under the node name. Unload is idempotent, so
/// its result is intentionally ignored.
async fn reload_rtp_source(
    pw_cmd: &PwCommandSender,
    port: u16,
    latency_msec: u32,
    source_addr: &str,
    ignore_ssrc: bool,
) -> Result<(), String> {
    let (tx, rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::Unload { node_name: RTP_SOURCE_NODE_NAME.to_string(), reply: tx }).is_err() {
        return Err("PipeWire thread is not running".to_string());
    }
    let _ = rx.await;

    let args = rtp_source_module_args(port, latency_msec, source_addr, ignore_ssrc);
    let (tx, rx) = oneshot::channel();
    if pw_cmd
        .send(PwCommand::Load {
            node_name: RTP_SOURCE_NODE_NAME.to_string(),
            module_name: RTP_SOURCE_MODULE_NAME.to_string(),
            args,
            reply: tx,
        })
        .is_err()
    {
        return Err("PipeWire thread is not running".to_string());
    }
    match rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("no reply from PipeWire thread".to_string()),
    }
}

async fn get_rtp_source(State(state): State<AppState>) -> Json<RtpSourceInfo> {
    let cfg = state.sources.lock_recover().rtp_source();
    Json(RtpSourceInfo {
        enabled: cfg.is_some(),
        port: cfg.as_ref().map(|c| c.port).unwrap_or(DEFAULT_RTP_PORT),
        latency_msec: cfg.as_ref().map(|c| c.latency_msec).unwrap_or(DEFAULT_RTP_LATENCY_MSEC),
        source_addr: cfg.as_ref().map(|c| c.source_addr.clone()).unwrap_or_else(|| DEFAULT_RTP_SOURCE_ADDR.to_string()),
        ignore_ssrc: cfg.map(|c| c.ignore_ssrc).unwrap_or(DEFAULT_RTP_IGNORE_SSRC),
        loaded: rtp_source_loaded(&state.pw),
    })
}

/// Enable the RTP source (or change its port). Persists first, then reconciles
/// the module — same "saved even if the load fails" contract as the AirPlay
/// source, so a transient PipeWire failure never silently drops the setting.
async fn set_rtp_source(State(state): State<AppState>, Json(req): Json<SetRtpSourceRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    if let Err(e) = state.sources.lock_recover().set_rtp_source(Some(RtpSourceConfig {
        port: req.port,
        latency_msec: req.latency_msec,
        source_addr: req.source_addr.clone(),
        ignore_ssrc: req.ignore_ssrc,
    })) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
    }
    match reload_rtp_source(&state.pw_cmd, req.port, req.latency_msec, &req.source_addr, req.ignore_ssrc).await {
        Ok(()) => (
            StatusCode::OK,
            Json(OutputOpResponse {
                ok: true,
                message: format!(
                    "RTP source enabled on {}:{} ({} ms jitter buffer, {})",
                    req.source_addr,
                    req.port,
                    req.latency_msec,
                    if req.ignore_ssrc { "any sender" } else { "single sender" },
                ),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(OutputOpResponse { ok: false, message: format!("saved port {} but failed to start it: {e}", req.port) }),
        ),
    }
}

async fn delete_rtp_source(State(state): State<AppState>) -> (StatusCode, Json<OutputOpResponse>) {
    if let Err(e) = state.sources.lock_recover().set_rtp_source(None) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
    }
    let (tx, rx) = oneshot::channel();
    if state.pw_cmd.send(PwCommand::Unload { node_name: RTP_SOURCE_NODE_NAME.to_string(), reply: tx }).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: "PipeWire thread is not running".to_string() }),
        );
    }
    let _ = rx.await;
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: "RTP source disabled".to_string() }))
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
    let reached = state.sendspin_control.lock().await.set_volume(&req.node_name, req.volume).await;
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

async fn set_sendspin_mute(
    State(state): State<AppState>,
    Json(req): Json<SetSendspinMuteRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let reached = state.sendspin_control.lock().await.set_muted(&req.node_name, req.muted).await;
    let verb = if req.muted { "muted" } else { "unmuted" };
    let message = if reached {
        format!("{verb} '{}'", req.node_name)
    } else {
        format!("saved {verb} for '{}' (device not connected)", req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
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

async fn set_ap2_volume(
    State(state): State<AppState>,
    Json(req): Json<SetAp2VolumeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
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

async fn set_ap2_mute(
    State(state): State<AppState>,
    Json(req): Json<SetAp2MuteRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let reached = state.ap2_control.lock().await.set_muted(&req.node_name, req.muted).await;
    let verb = if req.muted { "muted" } else { "unmuted" };
    let message = if reached {
        format!("{verb} '{}'", req.node_name)
    } else {
        format!("saved {verb} for '{}' (not streaming)", req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
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
    /// Group presentation lead in ms (sendspin `send_ahead`).
    group_lead_ms: u32,
}

#[derive(Deserialize)]
struct SetSyncSettingsRequest {
    group_lead_ms: u32,
}

async fn get_sync_settings(State(state): State<AppState>) -> Json<SyncSettingsInfo> {
    Json(SyncSettingsInfo { group_lead_ms: state.sync_settings.lock_recover().group_lead_ms() })
}

async fn set_sync_settings(
    State(state): State<AppState>,
    Json(req): Json<SetSyncSettingsRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if let Err(e) = state.sync_settings.lock_recover().set_group_lead_ms(req.group_lead_ms) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
    }
    // Nudge the reconciler; it re-reads the lead each tick and restarts group
    // servers so the new value takes effect promptly.
    let _ = state.changes.send(());
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("group lead set to {} ms", req.group_lead_ms) }))
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
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
            }
        }
        if let Some(enabled) = req.discovery_enabled {
            if let Err(e) = s.set_discovery_enabled(enabled) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
            }
        }
        if let Some(live) = req.sendspin_delay_live {
            if let Err(e) = s.set_sendspin_delay_live(live) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
            }
        }
        if let Some(expose) = req.expose_outputs_as_media_players {
            if let Err(e) = s.set_expose_outputs_as_media_players(expose) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
            }
        }
    }
    // Apply the discovery flag live (outside the settings lock). A spawn failure
    // is reported but the flag stays persisted — it'll retry on next boot.
    if let Some(enabled) = req.discovery_enabled {
        if let Err(e) = state.discovery.set_enabled(enabled) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to apply discovery: {e}") }));
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

async fn spike_ap2_start(
    State(state): State<AppState>,
    Json(req): Json<Ap2SpikeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Resolve targets: explicit IPs, else every present discovered AP2 receiver.
    let targets: Vec<(String, std::net::IpAddr)> = if !req.ips.is_empty() {
        req.ips
            .iter()
            .filter_map(|s| s.parse::<std::net::IpAddr>().ok().map(|ip| (s.clone(), ip)))
            .collect()
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
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("decode test clip: {e}") })),
        }
    } else {
        (req.mode.as_deref() == Some("live"), None)
    };

    match crate::ap2_spike::start(targets, &state.ap2_ptp, freq, secs, delay, live, rate, file_wav).await {
        Ok(info) => (
            StatusCode::OK,
            Json(OutputOpResponse { ok: true, message: format!("{} — {}", info.message, info.targets.join(", ")) }),
        ),
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })),
    }
}

async fn spike_ap2_stop() -> (StatusCode, Json<OutputOpResponse>) {
    let msg = crate::ap2_spike::stop().await;
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: msg }))
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
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("overlay on '{device}': {}", if stopped { "stopped" } else { "none active" }) }))
        }
        None => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: "missing ?device=".to_string() })),
    }
}

/// Announcement-group announce (announce.rs): play a clip to a set of sendspin
/// outputs with per-device duck+overlay and scheduler policy (queue/barge/TTL).
/// Requires the target devices to be on per-device senders. The node-based path
/// remains on `/api/media_players/:id/announce`.
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
    #[serde(default)]
    wyoming: Option<WyomingAnnounceRequest>,
    /// Use the built-in test-announcement clip (no url/wyoming needed).
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

/// Acquire the announce audio as 48k/S16/stereo PCM from one of test/tone/url/wyoming.
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
    match (&req.url, &req.wyoming) {
        (Some(url), None) => {
            let path = std::env::temp_dir().join("ag-announce-fetch");
            let _ = tokio::fs::remove_file(&path).await;
            fetch_to_file(url, &path).await.map_err(|e| format!("fetch: {e}"))?;
            let pcm = crate::decode::decode_file_to_pcm_48k_stereo(&path).await.map_err(|e| format!("decode: {e}"));
            let _ = tokio::fs::remove_file(&path).await;
            pcm
        }
        (None, Some(w)) => {
            let wav = crate::wyoming::synthesize_to_wav(&w.host, w.port, &w.text, w.voice.as_deref())
                .await
                .map_err(|e| format!("wyoming: {e}"))?;
            let (rate, ch, pcm) = crate::wav::read_pcm16(&wav).ok_or("wyoming did not return a PCM WAV")?;
            Ok(crate::resample::to_48k_stereo_s16le(pcm, rate, ch))
        }
        _ => Err("provide exactly one of: test, tone, url, wyoming".to_string()),
    }
}

async fn ag_announce(State(state): State<AppState>, Json(req): Json<AgAnnounceRequest>) -> (StatusCode, Json<AgAnnounceResponse>) {
    let reject = |msg: String| (StatusCode::BAD_REQUEST, Json(AgAnnounceResponse { ok: false, admission: "rejected".into(), position: None, reason: Some(msg.clone()), message: msg }));

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

    let pcm = match acquire_announce_pcm(&req).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return reject("empty audio".into()),
        Err(e) => return reject(e),
    };

    let target_count = targets.len();
    let admission = crate::announce::AnnounceCoordinator::global().announce(targets, pcm, duck, priority, on_busy, req.barge_in, req.ttl_ms);
    use crate::announce_arbiter::Admission;
    let (label, position, reason, ok) = match admission {
        Admission::Playing => ("playing", None, None, true),
        Admission::Queued { position } => ("queued", Some(position), None, true),
        Admission::Rejected(r) => ("rejected", None, Some(format!("{r:?}")), false),
    };
    (
        StatusCode::OK,
        Json(AgAnnounceResponse {
            ok,
            admission: label.to_string(),
            position,
            reason,
            message: format!("announce to {target_count} target(s): {label}"),
        }),
    )
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

async fn create_music_group(State(state): State<AppState>, Json(req): Json<CreateMusicGroupRequest>) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().create_music(&req.name, req.members) {
        Ok(mg) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": mg }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

async fn update_music_group(State(state): State<AppState>, Path(id): Path<String>, Json(req): Json<UpdateMusicGroupRequest>) -> (StatusCode, Json<serde_json::Value>) {
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
async fn route_music_group(State(state): State<AppState>, Path(id): Path<String>, Json(req): Json<RouteGroupRequest>) -> (StatusCode, Json<OutputOpResponse>) {
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
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist: {e}") }));
            }
        }
    }
    let _ = state.changes.send(());
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("routed '{id}' ({} member(s)) from '{}'", members.len(), req.source) }))
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

async fn create_announcement_group(State(state): State<AppState>, Json(req): Json<CreateAnnouncementGroupRequest>) -> (StatusCode, Json<serde_json::Value>) {
    let duck = req.duck.unwrap_or_else(|| state.settings.lock_recover().default_duck());
    match state.groups_config.lock_recover().create_announcement(&req.name, req.targets, req.priority, duck) {
        Ok(ag) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": ag }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

async fn update_announcement_group(State(state): State<AppState>, Path(id): Path<String>, Json(req): Json<UpdateAnnouncementGroupRequest>) -> (StatusCode, Json<serde_json::Value>) {
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
    state
        .align
        .start(req.sources)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
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
    let reached = state.sendspin_control.lock().await.set_delay(&req.node_name, req.delay_ms).await;
    let ms = req.delay_ms.min(5000);

    // Current ESPHome firmware reads the static delay only at stream start, so a
    // live push doesn't shift the running stream — restart the device's group
    // stream (drop + recreate its sendspin server on the next reconcile) so it
    // reconnects and re-applies the delay. Skipped when `sendspin_delay_live` is
    // on (firmware that honors a live SetStaticDelay).
    let live = state.settings.lock_recover().sendspin_delay_live();
    let mut restarted = false;
    if !live {
        restarted = state.groups.lock().await.force_server_restart(&req.node_name);
        if restarted {
            let _ = state.changes.send(());
        }
    }

    let message = if !reached {
        format!("saved {ms} ms for '{}' (device not connected)", req.node_name)
    } else if restarted {
        format!("set '{}' static delay to {ms} ms (restarting stream to apply)", req.node_name)
    } else {
        format!("set '{}' static delay to {ms} ms", req.node_name)
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
/// reconnect. The input is clamped to the render-delay window so it fits the
/// receiver's negotiated buffer.
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
    let clamped = req
        .latency_ms
        .map(|ms| ms.clamp(crate::ap2_server::AP2_RENDER_DELAY_MIN_MS, crate::ap2_server::AP2_RENDER_DELAY_MAX_MS));
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
    let latency_label = match clamped {
        Some(ms) => format!("{ms} ms"),
        None => "default".to_string(),
    };
    (
        StatusCode::OK,
        Json(OutputOpResponse { ok: true, message: format!("set '{node_name}' render delay to {latency_label} (live)") }),
    )
}

#[derive(serde::Deserialize)]
struct SetAp2RateModeRequest {
    /// `"auto"` (negotiate 48 kHz, fall back to 44.1 kHz) or `"fixed_44100"`.
    mode: String,
}

/// Set an AP2 output's wire-rate mode (persisted in sync_settings.rs) and nudge the
/// reconciler so the group re-negotiates + restarts at the new rate. Choosing `auto`
/// also clears any learned 44.1k cap so 48 kHz is re-probed.
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
    (
        StatusCode::OK,
        Json(OutputOpResponse { ok: true, message: format!("set '{node_name}' sample rate to {label} (applies shortly)") }),
    )
}

/// One output the custom HA integration turns into a
/// `media_player` entity: derived from the live registry, not from static
/// config, so it only lists sinks PipeWire actually created — matching this
/// project's "trust the observed state" approach throughout. (Virtual outputs —
/// sendspin/AP2 devices — are exposed to HA separately, via `/api/outputs`.)
#[derive(Serialize)]
struct MediaPlayerInfo {
    node_id: u32,
    node_name: String,
    /// "playing" if any link currently feeds this node, "idle" otherwise
    /// (pw_thread.rs's `node_has_incoming_link`) — there is no richer
    /// PipeWire-native concept of "paused" for a passive routing sink, so
    /// this entity's state model is necessarily simpler than a real
    /// playback device's.
    state: &'static str,
    /// Included inline (read natively from the node's Props param, volume.rs)
    /// rather than requiring the HA integration to make a second request
    /// per output on every poll — `None` if the node has no volume control.
    volume: Option<f32>,
}

async fn list_media_players(State(pw_state): State<SharedState>) -> Json<Vec<MediaPlayerInfo>> {
    // Snapshot and release the lock before the async volume reads below —
    // std::sync::MutexGuard isn't safe to hold across an .await point.
    let candidates: Vec<(u32, String, bool)> = {
        let state = pw_state.lock_recover();
        state
            .nodes
            .values()
            .filter(|n| n.node_name.starts_with(SENDSPIN_NODE_PREFIX))
            .map(|n| (n.node_id, n.node_name.clone(), state.node_has_incoming_link(n.node_id)))
            .collect()
    };

    // Sequential, not concurrent: candidate counts are small (a handful of
    // rooms), and this avoids pulling in a join_all dependency for it.
    let mut players = Vec::with_capacity(candidates.len());
    for (node_id, node_name, playing) in candidates {
        let volume = crate::volume::get_volume(node_id).await.ok().flatten();
        players.push(MediaPlayerInfo { node_id, node_name, state: if playing { "playing" } else { "idle" }, volume });
    }

    Json(players)
}

#[derive(Serialize)]
struct VolumeResponse {
    volume: Option<f32>,
    message: Option<String>,
}

async fn get_volume(Path(node_id): Path<u32>) -> (StatusCode, Json<VolumeResponse>) {
    match crate::volume::get_volume(node_id).await {
        Ok(Some(volume)) => (StatusCode::OK, Json(VolumeResponse { volume: Some(volume), message: None })),
        Ok(None) => (StatusCode::OK, Json(VolumeResponse { volume: None, message: Some(format!("node {node_id} has no volume control")) })),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(VolumeResponse { volume: None, message: Some(e) })),
    }
}

#[derive(Deserialize)]
struct SetVolumeRequest {
    /// 0.0-1.0, matching `wpctl`'s own scale (1.0 = 100%) and HA's
    /// `MediaPlayerEntity.volume_level`.
    volume: f32,
}

async fn set_volume(Path(node_id): Path<u32>, Json(req): Json<SetVolumeRequest>) -> (StatusCode, Json<VolumeResponse>) {
    match crate::volume::set_volume(node_id, req.volume).await {
        Ok(()) => (StatusCode::OK, Json(VolumeResponse { volume: Some(req.volume), message: None })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(VolumeResponse { volume: None, message: Some(e) })),
    }
}

/// TTS/voice-response ducked announce stream. Ducks
/// every source currently linked into this sink by setting their **node**
/// volume (not link volume — PipeWire Links have no Props/gain stage at
/// all, only a Format param; disproven empirically in
/// spikes/05-tts-ducking-mechanism.md), plays the announce clip into the
/// sink natively via a `pw::stream` (player.rs), then restores every ducked
/// source to its original volume, always — even if playback fails — so an
/// announce error can never leave music stuck at duck volume.
///
/// The announce audio itself comes from exactly one of two mutually
/// exclusive sources; the caller picks one per call:
/// - `url`: a rendered TTS clip fetched over HTTP (LAN-local, e.g. HA's own
///   `tts` integration), decoded to WAV via `decode.rs`.
/// - `wyoming`: synthesized directly against a local Wyoming TTS server
///   (e.g. Piper, see wyoming.rs), skipping the render-to-file-then-HTTP-
///   fetch round trip for lower first-audible-word latency.
#[derive(Deserialize)]
struct AnnounceRequest {
    url: Option<String>,
    wyoming: Option<WyomingAnnounceRequest>,
    /// 0.0-1.0, the level surviving sources are ducked to while the
    /// announce plays. Omitted → the global default (settings_store.rs),
    /// which keeps music audibly present but subordinate, matching Section
    /// 5.6's "ducked, not silenced" design.
    #[serde(default)]
    duck_volume: Option<f32>,
}

#[derive(Deserialize)]
struct WyomingAnnounceRequest {
    host: String,
    #[serde(default = "default_wyoming_port")]
    port: u16,
    text: String,
    /// Optional Piper multi-speaker voice name; omit for the server's
    /// default voice.
    voice: Option<String>,
}

fn default_wyoming_port() -> u16 {
    10200
}

#[derive(Serialize)]
struct AnnounceResponse {
    ok: bool,
    message: String,
}

async fn announce(
    State(state): State<AppState>,
    Path(node_id): Path<u32>,
    Json(req): Json<AnnounceRequest>,
) -> (StatusCode, Json<AnnounceResponse>) {
    let pw_state = &state.pw;
    // Fall back to the configured global default when the caller omits a level.
    let duck_volume = req.duck_volume.unwrap_or_else(|| state.settings.lock_recover().default_duck());
    let (target_name, source_node_ids): (String, Vec<u32>) = {
        let state = pw_state.lock_recover();
        match state.nodes.get(&node_id) {
            Some(target) => {
                // A stereo source contributes two Link objects (FL + FR)
                // into the sink, both sharing the same output_node — dedupe
                // by node id here, or a node gets ducked/restored twice:
                // the second "duck" fetches the volume the first duck call
                // already set (mistaking it for the original), and the
                // second "restore" then clobbers the correct restore with
                // that wrong cached value, leaving the source stuck ducked.
                let mut sources: Vec<u32> = state.links.values().filter(|l| l.input_node == node_id).map(|l| l.output_node).collect();
                sources.sort_unstable();
                sources.dedup();
                (target.node_name.clone(), sources)
            }
            None => return (StatusCode::NOT_FOUND, Json(AnnounceResponse { ok: false, message: format!("no such node: {node_id}") })),
        }
    };

    let (url, wyoming_req) = match (&req.url, &req.wyoming) {
        (Some(url), None) => (Some(url), None),
        (None, Some(w)) => (None, Some(w)),
        (Some(_), Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AnnounceResponse { ok: false, message: "exactly one of `url` or `wyoming` must be given, not both".to_string() }),
            )
        }
        (None, None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AnnounceResponse { ok: false, message: "one of `url` or `wyoming` is required".to_string() }),
            )
        }
    };

    let fetch_path = std::env::temp_dir().join(format!("announce-{node_id}-fetch"));
    let wav_path = std::env::temp_dir().join(format!("announce-{node_id}.wav"));
    // Always clear stale paths from a prior announce before writing new
    // ones — see tests/test_addon_phase3_multi_output.sh's comments on
    // cliraop's mkfifo gotcha for why a leftover file at a reused path is
    // a real, previously-hit failure mode, not just defensive paranoia.
    let _ = tokio::fs::remove_file(&fetch_path).await;
    let _ = tokio::fs::remove_file(&wav_path).await;

    if let Some(url) = url {
        if let Err(e) = fetch_to_file(url, &fetch_path).await {
            return (
                StatusCode::BAD_GATEWAY,
                Json(AnnounceResponse { ok: false, message: format!("failed to fetch announce audio: {e}") }),
            );
        }

        let decode_result = crate::decode::decode_file_to_wav(&fetch_path).await;
        let _ = tokio::fs::remove_file(&fetch_path).await;
        match decode_result {
            Ok(wav_bytes) => {
                if let Err(e) = tokio::fs::write(&wav_path, &wav_bytes).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(AnnounceResponse { ok: false, message: format!("failed to write decoded audio: {e}") }),
                    );
                }
            }
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AnnounceResponse { ok: false, message: format!("failed to decode announce audio: {e}") }),
                )
            }
        }
    } else if let Some(w) = wyoming_req {
        // No decode step needed here — we build the WAV ourselves
        // from the exact PCM format Wyoming reports (wyoming.rs).
        match crate::wyoming::synthesize_to_wav(&w.host, w.port, &w.text, w.voice.as_deref()).await {
            Ok(wav_bytes) => {
                if let Err(e) = tokio::fs::write(&wav_path, &wav_bytes).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(AnnounceResponse { ok: false, message: format!("failed to write synthesized audio: {e}") }),
                    );
                }
            }
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, Json(AnnounceResponse { ok: false, message: format!("wyoming synthesis failed: {e}") }))
            }
        }
    }

    let mut original_volumes = Vec::with_capacity(source_node_ids.len());
    for &src_id in &source_node_ids {
        if let Ok(Some(vol)) = crate::volume::get_volume(src_id).await {
            original_volumes.push((src_id, vol));
            let _ = crate::volume::set_volume(src_id, duck_volume).await;
        }
    }

    // Play the WAV natively (pw::stream on a blocking thread — player.rs),
    // replacing a `pw-cat --playback` subprocess. `play_wav_to_target` blocks
    // until the clip has drained, matching pw-cat's blocking behaviour.
    let wav_bytes = tokio::fs::read(&wav_path).await;
    let _ = tokio::fs::remove_file(&wav_path).await;
    let play_result = match wav_bytes {
        Ok(bytes) => tokio::task::spawn_blocking(move || crate::player::play_wav_to_target(node_id, &bytes))
            .await
            .unwrap_or_else(|e| Err(format!("playback task panicked: {e}"))),
        Err(e) => Err(format!("could not read announce audio back: {e}")),
    };

    // Restore unconditionally, after playback returns — a failed announce must
    // never leave music stuck at duck volume.
    for (src_id, vol) in &original_volumes {
        let _ = crate::volume::set_volume(*src_id, *vol).await;
    }

    match play_result {
        Ok(()) => (
            StatusCode::OK,
            Json(AnnounceResponse {
                ok: true,
                message: format!("announced on {target_name}, ducked {} source(s)", original_volumes.len()),
            }),
        ),
        Err(e) => (StatusCode::BAD_REQUEST, Json(AnnounceResponse { ok: false, message: e })),
    }
}

async fn fetch_to_file(url: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let response = reqwest::get(url).await?.error_for_status()?;
    let bytes = response.bytes().await?;
    tokio::fs::write(path, &bytes).await?;
    Ok(())
}
