use super::*;

// ---- AirPlay-receive source -----------------------------------------------
//
// The AirPlay-receive source is an embedded, native RAOP receiver
// (sources/airplay.rs) — not a subprocess and not a PipeWire module. Its
// enabled/disabled state and knobs are persisted in the sources store
// (sources.rs), which starts empty on a fresh install (no options.json
// seeding) and is then authoritative. Same "runtime, no restart" model as
// /api/outputs, but backed by an in-process receiver rather than a module.

/// Reconcile BOTH source kinds after a `/api/sources` mutation: (un)load RTP
/// modules (Phase 2) and start/stop AirPlay receivers (Phase 4) to match the
/// persisted set. Idempotent, so it's safe to call after every add/update/
/// remove. `list()` returns an owned snapshot so no lock is held across await.
pub(crate) async fn reconcile_sources(state: &AppState) {
    let entries = state.sources.lock_recover().list();
    crate::sources::rtp::reconcile(&entries, &state.pw_cmd, &state.pw).await;
    crate::sources::airplay::reconcile(&state.airplay, &entries, &state.airplay_clients, &state.now_playing).await;
    // A source that no longer exists must not leave a track behind in the
    // listings for the TTL to eventually collect (sources/now_playing.rs).
    let live: Vec<String> = entries.iter().map(|e| e.node_name()).collect();
    state.now_playing.retain_sources(&live);
}

/// One remembered AirPlay sender for the Sources tab. `key` (name if known,
/// else IP) identifies it for a `forget` call. `connected` is live, so it's
/// derived here rather than read from the persisted record (which omits it).
#[derive(Serialize)]
pub(crate) struct AirplayClientInfo {
    pub(crate) key: String,
    pub(crate) name: Option<String>,
    pub(crate) addr: String,
    pub(crate) first_seen: u64,
    pub(crate) last_connected: u64,
    pub(crate) connected: bool,
    pub(crate) banned: bool,
    pub(crate) priority: i32,
}

// --- per-receiver client-registry helpers ----------------------------------
//
// Shared by the legacy singular routes (which target `LEGACY_AIRPLAY_ID`) and
// the per-source `/api/sources/{id}/clients/*` routes. Each operates on that
// source's own client registry (sources/airplay_clients.rs).

pub(crate) fn list_clients_for(state: &AppState, id: &str) -> Vec<AirplayClientInfo> {
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

pub(crate) fn forget_client_for(state: &AppState, id: &str, key: &str) -> (StatusCode, Json<OutputOpResponse>) {
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

pub(crate) fn ban_client_for(state: &AppState, id: &str, key: &str, banned: bool) -> (StatusCode, Json<OutputOpResponse>) {
    if state.airplay_clients.registry(id).set_banned(key, banned) {
        let verb = if banned { "banned" } else { "unbanned" };
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{verb} AirPlay client '{key}'") }))
    } else {
        (StatusCode::NOT_FOUND, Json(OutputOpResponse { ok: false, message: format!("unknown AirPlay client '{key}'") }))
    }
}

pub(crate) fn set_priority_for(state: &AppState, id: &str, key: &str, priority: i32) -> (StatusCode, Json<OutputOpResponse>) {
    if state.airplay_clients.registry(id).set_priority(key, priority) {
        (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set priority {priority} for '{key}'") }))
    } else {
        (StatusCode::NOT_FOUND, Json(OutputOpResponse { ok: false, message: format!("unknown AirPlay client '{key}'") }))
    }
}

/// Force-disconnect a currently-connected client on source `id` by dropping its
/// RTSP connection (the receiver stops its stream shortly after).
pub(crate) async fn disconnect_client_for(state: &AppState, id: &str, key: &str) -> (StatusCode, Json<OutputOpResponse>) {
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

pub(crate) fn policy_message(prevent_takeover: bool) -> &'static str {
    if prevent_takeover {
        "AirPlay: new senders refused while one is streaming"
    } else {
        "AirPlay: new senders may take over the current stream"
    }
}

// --- legacy singular routes (target the legacy AirPlay id) ------------------

#[derive(Deserialize)]
pub(crate) struct ForgetClientRequest {
    pub(crate) key: String,
}

#[derive(Deserialize)]
pub(crate) struct BanClientRequest {
    pub(crate) key: String,
    pub(crate) banned: bool,
}

#[derive(Deserialize)]
pub(crate) struct SetPriorityRequest {
    pub(crate) key: String,
    pub(crate) priority: i32,
}

#[derive(Deserialize)]
pub(crate) struct DisconnectClientRequest {
    pub(crate) key: String,
}

#[derive(Deserialize)]
pub(crate) struct SetAirplayPolicyRequest {
    pub(crate) prevent_takeover: bool,
}

// --- per-source routes (Phase 4): /api/sources/{id}/clients/* + /policy -----

pub(crate) async fn list_source_clients(State(state): State<AppState>, Path(id): Path<String>) -> Json<Vec<AirplayClientInfo>> {
    Json(list_clients_for(&state, &id))
}

pub(crate) async fn forget_source_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ForgetClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    forget_client_for(&state, &id, &req.key)
}

pub(crate) async fn ban_source_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<BanClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    ban_client_for(&state, &id, &req.key, req.banned)
}

pub(crate) async fn set_source_client_priority(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetPriorityRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    set_priority_for(&state, &id, &req.key, req.priority)
}

pub(crate) async fn disconnect_source_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DisconnectClientRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    disconnect_client_for(&state, &id, &req.key).await
}

/// Toggle one AirPlay source's anti-takeover policy: persist it into that
/// source's config and live-update its running receiver's flag (no restart).
pub(crate) async fn set_source_policy(
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
