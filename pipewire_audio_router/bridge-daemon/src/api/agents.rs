use super::*;

// ---- Receiver agents (outputs/pwsink/agent.rs) -----------------------------------
//
// A pw-sink output is a receiver agent (docs/receiver-agent.md §3). The pairing
// *decisions* are output operations — `/api/outputs/{n}/adopt` pairs, `/unpair`
// revokes, `/ignore` hides — because a host asking to pair is a discovered output and
// nothing is gained by giving it a second vocabulary. What is left here is the
// listing (diagnostics). The host's own master volume and mute are set through the
// output — `PUT /api/outputs/{n}/volume` and `/mute` (api/level.rs) — like every other
// kind's; what is specific to a host is that there is nothing to "save for later": it
// owns the value and reports it back, so an unreachable host is an error rather than a
// stored intent (§9.4).

pub(crate) async fn get_agents(State(state): State<AppState>) -> Json<Vec<crate::outputs::pwsink::agent::AgentInfo>> {
    Json(state.agents.lock().await.snapshot())
}
