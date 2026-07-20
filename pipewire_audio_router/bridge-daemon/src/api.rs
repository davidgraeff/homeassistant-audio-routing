//! REST API: health check, live PipeWire registry state, and manual link
//! creation

use crate::airplay_source::AirplayHandle;
use crate::airplay_source::DEFAULT_AIRPLAY_LATENCY_MSEC;
use crate::config::{RaopOutputConfig, SENDSPIN_DEV_PREFIX, SENDSPIN_NODE_PREFIX};
use crate::locks::LockRecover;
use crate::outputs_store::OutputsStore;
use crate::pw_thread::{ChangeNotifier, LinkSpec, PwCommand, PwCommandSender, SharedState};
use crate::raop::{raop_module_args, raop_node_name, RAOP_MODULE_NAME, RAOP_NODE_PREFIX};
use crate::routing;
use crate::routing_store::SharedRouting;
use crate::rtp_source::{
    rtp_source_module_args, DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_PORT, DEFAULT_RTP_SOURCE_ADDR,
    RTP_SOURCE_MODULE_NAME, RTP_SOURCE_NODE_NAME,
};
use crate::sendspin_discovery::SharedSendspinDevices;
use crate::sources_store::{RtpSourceConfig, SourcesStore};
use axum::{
    extract::{FromRef, Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tower_http::services::ServeDir;

/// Runtime-managed set of RAOP outputs, shared between the CRUD handlers.
pub type SharedStore = Arc<Mutex<OutputsStore>>;

/// Runtime config for the AirPlay and RTP sources.
pub type SharedSources = Arc<Mutex<SourcesStore>>;

/// The running AirPlay-receive source (airplay_source.rs), if configured —
/// a native embedded RAOP server feeding a PipeWire source node. `tokio` mutex
/// since start/stop `.await`. `None` = disabled.
pub type SharedAirplay = Arc<tokio::sync::Mutex<Option<AirplayHandle>>>;

/// Shared axum state: the live PipeWire registry snapshot, the routing UI's
/// change-notification channel (Section 8, routing.rs), the command sender for
/// runtime module load/unload (pw_thread.rs), and the persistent outputs store.
/// Existing handlers extract just the piece they need via `FromRef` — they
/// don't need to know this type grew more fields.
#[derive(Clone)]
pub struct AppState {
    pub pw: SharedState,
    pub changes: ChangeNotifier,
    pub pw_cmd: PwCommandSender,
    pub store: SharedStore,
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
    pub sendspin_control: crate::sendspin_volume::SharedSendspinControl,
    /// Persistent routing intent (routing_store.rs): links by stable node
    /// name, reconciled onto the live graph so routing survives node reloads
    /// and device disappearance/reappearance.
    pub routing: SharedRouting,
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
    store: SharedStore,
    sources: SharedSources,
    airplay: SharedAirplay,
    airplay_clients: crate::airplay_clients::SharedAirplayClients,
    airplay_prevent_takeover: crate::airplay_source::SharedPreventTakeover,
    meters: crate::metering::SharedMeters,
    sendspin_devices: SharedSendspinDevices,
    routing: SharedRouting,
    sendspin_control: crate::sendspin_volume::SharedSendspinControl,
    static_dir: PathBuf,
) -> Router {
    let state = AppState {
        pw: pw_state,
        changes,
        pw_cmd,
        store,
        sources,
        airplay,
        airplay_clients,
        airplay_prevent_takeover,
        meters,
        sendspin_devices,
        routing,
        sendspin_control,
    };
    Router::new()
        .route("/health", get(health))
        .route("/api/nodes", get(list_nodes))
        .route("/api/links", post(create_link))
        .route("/api/outputs", get(list_outputs).post(add_output))
        .route("/api/outputs/:node_name", delete(remove_output))
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
        .route("/api/media_players", get(list_media_players))
        .route("/api/media_players/:node_id/volume", get(get_volume).post(set_volume))
        .route("/api/media_players/:node_id/announce", post(announce))
        .route("/api/routing", get(routing::get_routing))
        .route("/api/routing/link", post(routing::link))
        .route("/api/routing/unlink", post(routing::unlink))
        .route("/api/routing/entity/:node_name", delete(routing::forget_entity))
        .route("/api/routing/ws", get(routing::routing_ws))
        // Everything else (`/`, `/assets/*`, favicon, …) is the built Svelte
        // SPA (frontend/, served from `static_dir`). `ServeDir` returns
        // `index.html` for `/`.
        .fallback_service(ServeDir::new(static_dir))
        // Cache policy for the static SPA: hashed assets are immutable and cached
        // for a year; `index.html` (and any other entrypoint) is `no-cache` so a
        // new deploy — whose index references freshly-hashed asset names — is
        // always picked up instead of a stale index pinning old asset URLs.
        .layer(middleware::from_fn(spa_cache_control))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// Set `Cache-Control` on the static SPA served by `ServeDir`. Vite emits
/// content-hashed asset filenames (`index-<hash>.js`), so those are safe to
/// cache forever; `index.html` must never be cached, or a browser keeps serving
/// an old index that points at asset URLs a new deploy no longer has. API and
/// health responses are left untouched.
async fn spa_cache_control(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut resp = next.run(req).await;
    if !path.starts_with("/api") && path != "/health" {
        let value = if path.starts_with("/assets/") {
            HeaderValue::from_static("public, max-age=31536000, immutable")
        } else {
            HeaderValue::from_static("no-cache")
        };
        resp.headers_mut().insert(header::CACHE_CONTROL, value);
    }
    resp
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

// ---- Runtime-managed RAOP outputs ----------------------------------------
//
// Hot-reloadable: add/remove loads/unloads one `libpipewire-module-raop-sink`
// into the daemon's own PipeWire context, live, with no restart and no
// disturbance to audio on the other outputs. See docs/decisions.md "Loading
// PipeWire modules at runtime". The outputs store (outputs_store.rs) is the
// persistent source of truth; the live registry is what actually has a node.

/// An output for the Outputs tab. Covers RAOP (AirPlay) receivers and
/// discovered sendspin devices, across all three origins so the UI shows
/// everything the routing matrix does:
/// - **configured**: a manual RAOP store entry — `configured: true`,
///   `ip`/`port`/`encryption` known.
/// - **discovered**: present via mDNS, not in the store (a RAOP receiver or a
///   sendspin device) — `configured: false`, `present: true`, connection
///   details unknown here (`None`).
/// - **offline**: referenced by saved routing intent but not currently in the
///   graph — `present: false` (shown grayed; re-linked when it returns).
#[derive(Serialize)]
struct OutputInfo {
    node_name: String,
    name: String,
    /// `"airplay"` (RAOP) or `"sendspin"` — for the Type column / badge.
    kind: &'static str,
    /// Node/device is live right now.
    present: bool,
    /// Manual store entry (`true`) vs mDNS auto-discovered (`false`).
    configured: bool,
    /// Connection details — known only for configured RAOP entries.
    ip: Option<String>,
    port: Option<u16>,
    encryption: Option<String>,
}

/// Human name from a RAOP node name (`raop-out-living_room` -> `living room`).
fn raop_display_name(node_name: &str) -> String {
    node_name.strip_prefix(RAOP_NODE_PREFIX).unwrap_or(node_name).replace(['_', '-'], " ")
}

#[derive(Serialize)]
struct OutputOpResponse {
    ok: bool,
    message: String,
}

async fn list_outputs(State(state): State<AppState>) -> Json<Vec<OutputInfo>> {
    use std::collections::{BTreeMap, BTreeSet};

    // RAOP node names present in the live graph right now.
    let present: BTreeSet<String> = {
        let pw = state.pw.lock_recover();
        pw.nodes.values().map(|n| n.node_name.clone()).filter(|n| n.starts_with(RAOP_NODE_PREFIX)).collect()
    };
    // Configured (manual) outputs: node_name -> config.
    let configured: BTreeMap<String, RaopOutputConfig> =
        state.store.lock_recover().list().iter().map(|o| (raop_node_name(&o.name), o.clone())).collect();
    // RAOP outputs referenced by saved routing intent (offline ones surface here).
    let intent_outputs: BTreeSet<String> =
        state.routing.lock_recover().referenced_outputs().into_iter().filter(|n| n.starts_with(RAOP_NODE_PREFIX)).collect();

    // Union: present ∪ configured ∪ intent-referenced.
    let mut names: BTreeSet<String> = present.iter().cloned().collect();
    names.extend(configured.keys().cloned());
    names.extend(intent_outputs);

    let mut outputs: Vec<OutputInfo> = names
        .into_iter()
        .map(|node_name| {
            let cfg = configured.get(&node_name);
            OutputInfo {
                kind: "airplay",
                present: present.contains(&node_name),
                configured: cfg.is_some(),
                name: cfg.map(|c| c.name.clone()).unwrap_or_else(|| raop_display_name(&node_name)),
                ip: cfg.map(|c| c.ip.clone()),
                port: cfg.map(|c| c.port),
                encryption: cfg.map(|c| c.encryption.as_pipewire_arg().to_string()),
                node_name,
            }
        })
        .collect();

    // Discovered sendspin devices (present) + any offline ones still referenced
    // by saved routing intent — surfaced here too so users see every routable
    // output, not just RAOP.
    let devices = state.sendspin_devices.lock_recover().clone();
    let mut sendspin_names: BTreeSet<String> = devices.keys().cloned().collect();
    sendspin_names.extend(state.routing.lock_recover().referenced_outputs().into_iter().filter(|n| n.starts_with(SENDSPIN_DEV_PREFIX)));
    for node_name in sendspin_names {
        let present = devices.contains_key(&node_name);
        let name = devices
            .get(&node_name)
            .map(|d| d.display_name.clone())
            .unwrap_or_else(|| node_name.strip_prefix(SENDSPIN_DEV_PREFIX).unwrap_or(&node_name).replace(['_', '-'], " "));
        outputs.push(OutputInfo {
            kind: "sendspin",
            present,
            configured: false, // sendspin devices are always auto-discovered
            name,
            ip: None,
            port: None,
            encryption: None,
            node_name,
        });
    }

    Json(outputs)
}

/// Add a RAOP output and load its module live. Request body is a full output
/// config (`{ "name", "ip", "port"?, "encryption"? }`) — the same shape as one
/// entry of the add-on's `outputs`. The module is loaded first; only on success
/// is the output persisted, so a failed load leaves no stale store entry.
async fn add_output(State(state): State<AppState>, Json(output): Json<RaopOutputConfig>) -> (StatusCode, Json<OutputOpResponse>) {
    let node_name = raop_node_name(&output.name);

    if state.store.lock_recover().contains(&node_name) {
        return (StatusCode::CONFLICT, Json(OutputOpResponse { ok: false, message: format!("output '{}' already exists", output.name) }));
    }

    let args = raop_module_args(&output);
    let (tx, rx) = oneshot::channel();
    if state
        .pw_cmd
        .send(PwCommand::Load { node_name: node_name.clone(), module_name: RAOP_MODULE_NAME.to_string(), args, reply: tx })
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: "PipeWire thread is not running".to_string() }),
        );
    }

    match rx.await {
        Ok(Ok(())) => match state.store.lock_recover().add(output) {
            Ok(()) => (StatusCode::CREATED, Json(OutputOpResponse { ok: true, message: format!("added output '{node_name}'") })),
            // Loaded but not persisted: works this session, wouldn't survive a
            // restart — report it rather than pretend clean success.
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OutputOpResponse { ok: false, message: format!("loaded '{node_name}' but failed to persist it: {e}") }),
            ),
        },
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(OutputOpResponse { ok: false, message: format!("failed to load RAOP module: {e}") })),
        Err(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: "no reply from PipeWire thread".to_string() }))
        }
    }
}

/// Remove a RAOP output by node name — works for all three origins:
/// - **configured**: unload its module (if loaded) + drop the store entry.
/// - **discovered**: unload it; it'll be re-discovered when the device next
///   announces (mDNS), per the intended UX.
/// - **offline**: nothing to unload; just forget its saved routing.
///
/// In every case the output's routing intent is dropped so it doesn't linger
/// as an offline phantom. Unload is idempotent — the caller's intent ("gone")
/// always holds afterward.
async fn remove_output(State(state): State<AppState>, Path(node_name): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
    let configured = state.store.lock_recover().contains(&node_name);
    let present = { state.pw.lock_recover().nodes.values().any(|n| n.node_name == node_name) };
    let has_intent = state.routing.lock_recover().referenced_outputs().contains(&node_name);
    if !configured && !present && !has_intent {
        return (StatusCode::NOT_FOUND, Json(OutputOpResponse { ok: false, message: format!("no such output: {node_name}") }));
    }

    // Unload the live module if it has a node right now (discovered or
    // configured-and-loaded). Offline entries skip this.
    if present {
        let (tx, rx) = oneshot::channel();
        if state.pw_cmd.send(PwCommand::Unload { node_name: node_name.clone(), reply: tx }).is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OutputOpResponse { ok: false, message: "PipeWire thread is not running".to_string() }),
            );
        }
        if let Ok(Err(e)) = rx.await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OutputOpResponse { ok: false, message: format!("failed to unload module: {e}") }),
            );
        }
    }

    if let Err(e) = state.store.lock_recover().remove(&node_name) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: format!("failed to persist removal: {e}") }),
        );
    }
    if let Err(e) = state.routing.lock_recover().remove_entity(&node_name) {
        tracing::warn!("removed output '{node_name}' but failed to drop its routing intent: {e}");
    }
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("removed output '{node_name}'") }))
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
// (PwCommand::Load/Unload, keyed by RTP_SOURCE_NODE_NAME), exactly like a RAOP
// sink, rather than through the process supervisor. Enable/disable and
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
// Sendspin devices are virtual outputs fed by a shared group sink, so (unlike
// AirPlay's raop-sink node volume) there's no PipeWire volume to drive. Volume
// is carried in-band over the sendspin protocol to the specific device; see
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

/// One output the custom HA integration (Section 6/9) turns into a
/// `media_player` entity: derived from the live registry, not from the
/// add-on's static config, so it only lists outputs PipeWire actually
/// created (a misconfigured RAOP device that never loaded still won't
/// appear — matches this project's "trust the observed state" approach
/// throughout).
#[derive(Serialize)]
struct MediaPlayerInfo {
    node_id: u32,
    node_name: String,
    /// "playing" if any link currently feeds this node, "idle" otherwise
    /// (pw_thread.rs's `node_has_incoming_link`) — there is no richer
    /// PipeWire-native concept of "paused" for a passive routing sink;
    /// see PLAN.md Section 6 for why this entity's state model is
    /// necessarily simpler than a real playback device's.
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
            .filter(|n| n.node_name.starts_with(RAOP_NODE_PREFIX) || n.node_name.starts_with(SENDSPIN_NODE_PREFIX))
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

/// TTS/voice-response ducked announce stream (PLAN.md Section 5.6). Ducks
/// every source currently linked into this sink by setting their **node**
/// volume (not link volume — PipeWire Links have no Props/gain stage at
/// all, only a Format param; disproven empirically in
/// spikes/05-tts-ducking-mechanism.md), plays the announce clip into the
/// sink natively via a `pw::stream` (player.rs), then restores every ducked
/// source to its original volume, always — even if playback fails — so an
/// announce error can never leave music stuck at duck volume.
///
/// The announce audio itself comes from exactly one of two mutually
/// exclusive sources — **additive**, not a v1→v2 migration (Phase 3.5):
/// - `url` (**v1**, unchanged): a rendered TTS clip fetched over HTTP
///   (LAN-local, e.g. HA's own `tts` integration), decoded to WAV via
///   `decode.rs` (pure-Rust `symphonia` — pw-cat/libsndfile can't decode
///   compressed formats like mp3 itself, and shelling out to `ffmpeg` for
///   this used to pull ~250-300MB of unrelated GPU/video-transcoding
///   system dependencies into the runtime image for no benefit here).
/// - `wyoming` (**v2**, new): synthesized directly against a local
///   Wyoming TTS server (e.g. Piper, see wyoming.rs), skipping the
///   render-to-file-then-HTTP-fetch round trip for lower first-audible-
///   word latency. Whichever caller wants this picks it per call — HA's
///   `tts.speak` keeps using `url` exactly as before; nothing is forced
///   to switch.
#[derive(Deserialize)]
struct AnnounceRequest {
    url: Option<String>,
    wyoming: Option<WyomingAnnounceRequest>,
    /// 0.0-1.0, the level surviving sources are ducked to while the
    /// announce plays. Defaults to a level that keeps music audibly
    /// present but subordinate, matching Section 5.6's "ducked, not
    /// silenced" design.
    #[serde(default = "default_duck_volume")]
    duck_volume: f32,
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

fn default_duck_volume() -> f32 {
    0.25
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
    State(pw_state): State<SharedState>,
    Path(node_id): Path<u32>,
    Json(req): Json<AnnounceRequest>,
) -> (StatusCode, Json<AnnounceResponse>) {
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
        // No ffmpeg decode step needed here — we build the WAV ourselves
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
            let _ = crate::volume::set_volume(src_id, req.duck_volume).await;
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
