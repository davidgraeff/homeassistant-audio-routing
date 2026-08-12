use super::*;

// ---- Receiver agents (outputs/pwsink/agent.rs) -----------------------------------
//
// A pw-sink output is a receiver agent (docs/receiver-agent.md §3). The pairing
// *decisions* are output operations — `/api/outputs/{n}/adopt` pairs, `/unpair`
// revokes, `/ignore` hides — because a host asking to pair is a discovered output and
// nothing is gained by giving it a second vocabulary. What is left here is the
// listing (diagnostics) and the per-host volume/mute the agent applies on the
// receiver's *own* master out. Unlike sendspin/AP2 volume there is nothing to "save
// for later": the host owns the value and reports it back, so an unreachable host is
// an error rather than a stored intent (§9.4).

pub(crate) async fn get_agents(State(state): State<AppState>) -> Json<Vec<crate::outputs::pwsink::agent::AgentInfo>> {
    Json(state.agents.lock().await.snapshot())
}

#[derive(Deserialize)]
pub(crate) struct SetPwsinkVolumeRequest {
    pub(crate) node_name: String,
    /// Cubic 0.0-1.0, same scale as HA's `volume_level` and `wpctl`.
    pub(crate) volume: f32,
}

pub(crate) async fn set_pwsink_volume(
    State(state): State<AppState>,
    Json(req): Json<SetPwsinkVolumeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
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
pub(crate) struct SetPwsinkMuteRequest {
    pub(crate) node_name: String,
    pub(crate) muted: bool,
}

pub(crate) async fn set_pwsink_mute(
    State(state): State<AppState>,
    Json(req): Json<SetPwsinkMuteRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
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
