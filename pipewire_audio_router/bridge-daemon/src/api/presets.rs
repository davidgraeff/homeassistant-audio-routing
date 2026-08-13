//! **Music-group presets** — the grouping of the whole house as a named,
//! switchable thing (docs/music-group-presets-plan.md).
//!
//! The store holds the data (store/groups.rs); this module is the CRUD around it
//! plus the one interesting operation, **activation**, which is where a preset
//! stops being data and moves speakers:
//!
//! 1. the store makes the preset active and hands back a plan — one entry per
//!    group *with members*, holding the source it should play;
//! 2. this applies the plan through the same `route_members` the Source dropdown
//!    uses, in one pass, and sends `changes` **once**.
//!
//! One pass matters. N calls from the UI would take the house through states no
//! preset describes — a member briefly in two groups, or a sender torn down and
//! rebuilt because an intermediate grouping split a source set.
//!
//! A plan entry with no source means *leave these members' links alone*: a preset
//! does not touch what it does not mention, so activating one that says nothing
//! about the bathroom leaves the bathroom playing.

use super::*;

#[derive(Deserialize)]
pub(crate) struct CreatePresetRequest {
    pub(crate) name: String,
    /// Preset to copy the grouping from; omitted = start empty. The UI sends the
    /// preset being edited, because a variant is nearly always "the current
    /// grouping, but…".
    #[serde(default)]
    pub(crate) copy_from: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct UpdatePresetRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
}

/// Every preset plus which one is in force — one call, because a chip bar that
/// cannot mark the active preset is a lie about what is playing.
#[derive(Serialize)]
pub(crate) struct PresetsInfo {
    pub(crate) active: String,
    pub(crate) presets: Vec<crate::store::groups::Preset>,
}

pub(crate) fn presets_info(state: &AppState) -> PresetsInfo {
    let g = state.groups_config.lock_recover();
    PresetsInfo { active: g.active_preset().to_string(), presets: g.presets().to_vec() }
}

pub(crate) async fn list_presets(State(state): State<AppState>) -> Json<PresetsInfo> {
    Json(presets_info(&state))
}

pub(crate) async fn create_preset(
    State(state): State<AppState>,
    Json(req): Json<CreatePresetRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.groups_config.lock_recover().create_preset(&req.name, req.copy_from.as_deref()) {
        Ok(p) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "preset": p }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

pub(crate) async fn update_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdatePresetRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(name) = req.name else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": "nothing to update" })));
    };
    match state.groups_config.lock_recover().rename_preset(&id, &name) {
        Ok(p) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "preset": p }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "message": e.to_string() }))),
    }
}

/// Delete a preset. Deleting the one in force leaves `Default` active, so the
/// grouping the house is on is always a preset that exists — and that new active
/// preset is applied here, exactly as an explicit activation would.
pub(crate) async fn delete_preset(State(state): State<AppState>, Path(id): Path<String>) -> OpResult {
    tracing::info!("USER ACTION: delete preset '{}'", id);
    let was_active = state.groups_config.lock_recover().active_preset() == id;
    if let Err(e) = state.groups_config.lock_recover().delete_preset(&id) {
        return Err(ApiError::bad_request(e.to_string()));
    }
    if was_active {
        apply_preset(&state, crate::store::groups::DEFAULT_PRESET_ID)?;
        return ok(format!("deleted preset '{id}' and activated the fallback"));
    }
    ok(format!("deleted preset '{id}'"))
}

pub(crate) async fn activate_preset(State(state): State<AppState>, Path(id): Path<String>) -> OpResult {
    tracing::info!("USER ACTION: activate preset '{}'", id);
    let groups = apply_preset(&state, &id)?;
    ok(format!("activated preset '{id}' ({groups} group(s) routed)"))
}

/// Make `id` active and route what it says. Returns how many groups it routed.
///
/// Shared with the "work with presets" switch (api/settings.rs), which turns the
/// house back to `Default` when it is switched off — leaving a non-Default
/// grouping in force with no UI showing it would be a puzzle nobody could solve.
pub(crate) fn apply_preset(state: &AppState, id: &str) -> Result<usize, ApiError> {
    let plan = state.groups_config.lock_recover().activate_preset(id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let mut routed = 0;
    for (members, source) in &plan {
        match source {
            // "Leave as is" — the members keep whatever is feeding them. Silence
            // would be the other reading, and it is the wrong one: a preset that
            // says nothing about a room should not stop its music.
            None => tracing::debug!("preset '{id}': {} member(s) left on their current source", members.len()),
            Some(source) => {
                route_members(state, members, source)?;
                routed += 1;
            }
        }
    }
    // One notification for the whole switch: the reconciler then sees a single
    // transition instead of one per group (plan §4.3).
    let _ = state.changes.send(());
    Ok(routed)
}
