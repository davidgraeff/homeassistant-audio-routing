use super::*;

// ---- Named groups (store/groups.rs) -------------------------------------

#[derive(Deserialize)]
pub(crate) struct CreateMusicGroupRequest {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) members: Vec<String>,
    /// Which preset the members go into; omitted = the active one. The UI sends
    /// the preset it is editing, which may not be the one in force.
    #[serde(default)]
    pub(crate) preset: Option<String>,
}
#[derive(Deserialize)]
pub(crate) struct UpdateMusicGroupRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) members: Option<Vec<String>>,
    /// Which preset the membership change applies to; omitted = the active one.
    /// A rename ignores it — the name is the group's identity, shared by every
    /// preset (store/groups.rs).
    #[serde(default)]
    pub(crate) preset: Option<String>,
}
#[derive(Deserialize)]
pub(crate) struct CreateAnnouncementGroupRequest {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) targets: Vec<String>,
    #[serde(default)]
    pub(crate) priority: i32,
    #[serde(default)]
    pub(crate) duck: Option<f32>,
}
#[derive(Deserialize)]
pub(crate) struct UpdateAnnouncementGroupRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) targets: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) priority: Option<i32>,
    #[serde(default)]
    pub(crate) duck: Option<f32>,
}

pub(crate) async fn list_music_groups(State(state): State<AppState>) -> Json<Vec<crate::store::groups::MusicGroup>> {
    Json(state.groups_config.lock_recover().music().to_vec())
}

pub(crate) async fn create_music_group(
    State(state): State<AppState>,
    Json(req): Json<CreateMusicGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().create_music(&req.name, req.members, req.preset.as_deref()) {
        Ok(mg) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": mg }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

pub(crate) async fn update_music_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMusicGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().update_music(&id, req.name, req.members, req.preset.as_deref()) {
        Ok(mg) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": mg }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

pub(crate) async fn delete_music_group(State(state): State<AppState>, Path(id): Path<String>) -> OpResult {
    match state.groups_config.lock_recover().delete_music(&id) {
        Ok(()) => ok(format!("deleted music group '{id}'")),
        Err(e) => Err(ApiError::bad_request(e.to_string())),
    }
}

#[derive(Deserialize)]
pub(crate) struct RouteGroupRequest {
    pub(crate) source: String,
}

/// Put `members` exclusively on `source`: link each one to it and drop every other
/// source feeding it. The one place group-level routing is expressed — the Source
/// dropdown, the card's wire, HA's `select_source` and a preset activation all come
/// through here, so they cannot drift apart. Caller sends `changes` once it is done
/// (an activation routes several groups and wants one transition).
pub(crate) fn route_members(state: &AppState, members: &[String], source: &str) -> Result<(), ApiError> {
    let snapshot = crate::store::routing::snapshot(&state.routing);
    let mut store = state.routing.lock_recover();
    for member in members {
        for l in snapshot.iter().filter(|l| &l.output == member && l.source != source) {
            let _ = store.remove(&l.source, member);
        }
        if let Err(e) = store.add(source, member) {
            return Err(ApiError::internal(format!("failed to persist: {e}")));
        }
    }
    Ok(())
}

/// Drop every link feeding `members`.
pub(crate) fn unroute_members(state: &AppState, members: &[String]) {
    let snapshot = crate::store::routing::snapshot(&state.routing);
    let mut store = state.routing.lock_recover();
    for l in snapshot.iter().filter(|l| members.contains(&l.output)) {
        let _ = store.remove(&l.source, &l.output);
    }
}

/// The members of a music group in the *active* preset, or a 400 naming it.
fn members_of(state: &AppState, id: &str) -> Result<Vec<String>, ApiError> {
    state
        .groups_config
        .lock_recover()
        .music()
        .into_iter()
        .find(|m| m.id == id)
        .map(|m| m.members)
        .ok_or_else(|| ApiError::bad_request(format!("no music group '{id}'")))
}

/// Route a source to a whole music group: the group's members are (re)linked from
/// `source` (replacing any prior source per member), so they play it in sync. The
/// group is the routable unit; individual member re-routing is left to the raw
/// matrix. Reuses the per-output routing store + reconciler (no special-casing).
///
/// Also **records** the choice in the active preset (store/groups.rs
/// `note_source`), so a preset remembers what its groups play and activating it
/// later restores the music and not only the grouping.
pub(crate) async fn route_music_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RouteGroupRequest>,
) -> OpResult {
    tracing::info!("USER ACTION: route music group '{}' (routing graph)", id);
    let members = members_of(&state, &id)?;
    if members.is_empty() {
        return Err(ApiError::bad_request("music group has no members"));
    }
    route_members(&state, &members, &req.source)?;
    if let Err(e) = state.groups_config.lock_recover().note_source(&id, Some(&req.source)) {
        // The routing landed; only the preset's memory of it didn't.
        tracing::warn!("could not record the source of '{id}' in the active preset: {e}");
    }
    let _ = state.changes.send(());
    ok(format!("routed '{id}' ({} member(s)) from '{}'", members.len(), req.source))
}

/// Un-route a whole music group: remove all links feeding its members.
pub(crate) async fn unroute_music_group(State(state): State<AppState>, Path(id): Path<String>) -> OpResult {
    tracing::info!("USER ACTION: unroute music group '{}' (routing graph)", id);
    let members = members_of(&state, &id)?;
    unroute_members(&state, &members);
    if let Err(e) = state.groups_config.lock_recover().note_source(&id, None) {
        tracing::warn!("could not record the silence of '{id}' in the active preset: {e}");
    }
    let _ = state.changes.send(());
    ok(format!("un-routed music group '{id}'"))
}

pub(crate) async fn list_announcement_groups(State(state): State<AppState>) -> Json<Vec<crate::store::groups::AnnouncementGroup>> {
    Json(state.groups_config.lock_recover().announcement().to_vec())
}

pub(crate) async fn create_announcement_group(
    State(state): State<AppState>,
    Json(req): Json<CreateAnnouncementGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let duck = req.duck.unwrap_or_else(|| state.settings.lock_recover().default_duck());
    match state.groups_config.lock_recover().create_announcement(&req.name, req.targets, req.priority, duck) {
        Ok(ag) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": ag }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

pub(crate) async fn update_announcement_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAnnouncementGroupRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().update_announcement(&id, req.name, req.targets, req.priority, req.duck) {
        Ok(ag) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "group": ag }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

pub(crate) async fn delete_announcement_group(State(state): State<AppState>, Path(id): Path<String>) -> OpResult {
    match state.groups_config.lock_recover().delete_announcement(&id) {
        Ok(()) => ok(format!("deleted announcement group '{id}'")),
        Err(e) => Err(ApiError::bad_request(e.to_string())),
    }
}

/// Diagnostics snapshot for the Diagnostics page's status header.
#[derive(Serialize)]
pub(crate) struct StatusInfo {
    pub(crate) version: String,
    pub(crate) uptime_secs: u64,
    pub(crate) discovery_enabled: bool,
    /// Live PipeWire graph node count (0 while the graph is empty/unconnected).
    pub(crate) pipewire_nodes: usize,
    /// mDNS-discovered AirPlay-2 receivers currently tracked.
    pub(crate) ap2_receivers: usize,
    /// mDNS-discovered sendspin devices currently tracked.
    pub(crate) sendspin_devices: usize,
    /// Persisted routing links (by stable name).
    pub(crate) routes: usize,
    /// Host capability / weak-system assessment (CPU, RAM, RT scheduling).
    pub(crate) host: crate::util::host_assessment::HostAssessment,
}

pub(crate) async fn get_status(State(state): State<AppState>) -> Json<StatusInfo> {
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
        host: crate::util::host_assessment::assess(),
    })
}
