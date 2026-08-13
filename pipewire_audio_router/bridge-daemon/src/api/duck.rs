use super::*;

// ---- Duck holds (outputs/overlay_mixer.rs) --------------------------------------
//
// A duck hold attenuates an output's music with **no clip of its own** — what
// voice ducking needs, since a voice assistant speaking through its own speaker
// gives the router nothing to play. Deliberately not the announce path: that one
// is built for atomic clips (whole-or-nothing, queue, barge-in, TTL) and it
// *occupies* its targets, which would make a doorbell queue behind someone's
// voice turn instead of playing over the already-ducked music.
//
// The daemon knows nothing about rooms: Home Assistant resolves "which speakers
// are in the room the satellite is in" (its own area registry) and posts output
// names. Holds are leased, so a holder that dies mid-turn cannot leave music
// ducked forever — see `DUCK_HOLD_TTL` and the announce tick.

#[derive(Deserialize)]
pub(crate) struct DuckRequest {
    /// Output node names to duck. Optional if `announcement_group` is given.
    #[serde(default)]
    pub(crate) targets: Vec<String>,
    /// Named announcement group (store/groups.rs) whose targets to duck — the
    /// same addressing `/api/announce` accepts, so an AG can double as "these
    /// speakers" without repeating the list.
    #[serde(default)]
    pub(crate) announcement_group: Option<String>,
    /// Music gain 0.0–1.0 while held. Omitted → the daemon's default duck.
    #[serde(default)]
    pub(crate) level: Option<f32>,
    /// Lease length; omitted → `DUCK_HOLD_TTL` (30 s). Renew inside it.
    #[serde(default)]
    pub(crate) ttl_ms: Option<u64>,
}

/// A duck hold, as the caller needs it back. A *success* body: the status says it
/// happened, so there is no `ok` — see `api::error`.
#[derive(Serialize)]
pub(crate) struct DuckResponse {
    /// The hold's id — pass it to renew/release.
    pub(crate) hold_id: u64,
    /// Outputs this hold covers.
    #[serde(default)]
    pub(crate) ducked: Vec<String>,
    pub(crate) level: Option<f32>,
    pub(crate) message: String,
}

#[derive(Serialize)]
pub(crate) struct DuckHoldView {
    pub(crate) output: String,
    pub(crate) hold_id: u64,
    pub(crate) level: f32,
}

pub(crate) fn duck_ttl(ttl_ms: Option<u64>) -> std::time::Duration {
    ttl_ms.map_or(crate::outputs::overlay_mixer::DUCK_HOLD_TTL, std::time::Duration::from_millis)
}

/// Start a duck hold on a set of outputs. Idempotent in the only sense that
/// matters: holds compose (strongest gain wins), so two callers ducking the same
/// output don't fight, and each releases only its own.
pub(crate) async fn duck_start(State(state): State<AppState>, Json(req): Json<DuckRequest>) -> Result<Json<DuckResponse>, ApiError> {
    let reject = ApiError::bad_request;
    let mut targets = req.targets.clone();
    if let Some(agid) = &req.announcement_group {
        let store = state.groups_config.lock_recover();
        match store.announcement_by_id(agid) {
            Some(ag) if targets.is_empty() => targets = ag.targets.clone(),
            Some(_) => {}
            None => return Err(reject(format!("no announcement group '{agid}'"))),
        }
    }
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return Err(reject("no targets (provide `targets` or `announcement_group`)".into()));
    }
    let level = req.level.unwrap_or_else(|| state.settings.lock_recover().default_duck()).clamp(0.0, 1.0);
    let ttl = duck_ttl(req.ttl_ms);
    let id = crate::outputs::overlay_mixer::OverlayMixer::global().start_duck(&targets, level, ttl);
    // An agent-backed pw-sink host also ducks the audio it plays *itself* — the
    // overlay mix can't reach that. No-op for every other kind.
    for target in &targets {
        crate::announce::sync_agent_duck(target);
    }
    // A duck hold outranks an alignment hold (plan §12.3) — the session gets a
    // structured report through `align_group`, and it is worth a log line at the point
    // of cause too, so "why was my measurement discarded?" is answerable from the log.
    let aligning: Vec<&str> = targets.iter().filter(|t| crate::align::group::registry().is_reserved(t)).map(String::as_str).collect();
    if !aligning.is_empty() {
        tracing::warn!(
            "duck hold {id} lands on speaker(s) currently being aligned [{}] — their measurements will be discarded",
            aligning.join(", ")
        );
    }
    // One line per call, like announce: "why is the kitchen quiet?" is answerable
    // from the log alone.
    tracing::info!(
        "USER ACTION: duck -> {} target(s) [{}] at gain {level:.2}, hold {id}, lease {} ms",
        targets.len(),
        targets.join(", "),
        ttl.as_millis()
    );
    let message = format!("ducking {} target(s) to {level:.2}", targets.len());
    Ok(Json(DuckResponse { hold_id: id, ducked: targets, level: Some(level), message }))
}

/// Extend a hold's lease. 404 when the id is unknown (released or expired) so the
/// caller starts a fresh hold instead of believing it is still ducking.
pub(crate) async fn duck_renew(Path(hold_id): Path<u64>, Json(req): Json<DuckRequest>) -> Result<Json<DuckResponse>, ApiError> {
    let ttl = duck_ttl(req.ttl_ms);
    if crate::outputs::overlay_mixer::OverlayMixer::global().renew_duck(hold_id, ttl) {
        Ok(Json(DuckResponse {
            hold_id,
            ducked: Vec::new(),
            level: None,
            message: format!("renewed hold {hold_id} for {} ms", ttl.as_millis()),
        }))
    } else {
        Err(ApiError::not_found(format!("no duck hold {hold_id} (released or expired) — start a new one")))
    }
}

/// Release a hold now (the normal end of a voice turn).
pub(crate) async fn duck_release(Path(hold_id): Path<u64>) -> Result<Json<DuckResponse>, ApiError> {
    let affected = crate::outputs::overlay_mixer::OverlayMixer::global().release_duck(hold_id);
    let existed = !affected.is_empty();
    for output in &affected {
        crate::announce::sync_agent_duck(output);
    }
    tracing::info!("USER ACTION: unduck -> hold {hold_id}{}", if existed { "" } else { " (already gone)" });
    // Releasing an already-gone hold is success: the caller wanted it not ducking.
    Ok(Json(DuckResponse {
        hold_id,
        ducked: Vec::new(),
        level: None,
        message: if existed { format!("released hold {hold_id}") } else { format!("hold {hold_id} was already gone") },
    }))
}

/// Live holds — for the UI and for answering "why is this output quiet?".
pub(crate) async fn duck_list() -> Json<Vec<DuckHoldView>> {
    Json(
        crate::outputs::overlay_mixer::OverlayMixer::global()
            .duck_holds()
            .into_iter()
            .map(|(output, hold_id, level)| DuckHoldView { output, hold_id, level })
            .collect(),
    )
}
