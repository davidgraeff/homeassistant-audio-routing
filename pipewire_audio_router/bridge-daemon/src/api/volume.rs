use super::*;

// ---- Desired per-device sendspin state, as listings ------------------------
//
// The *writes* live on the output (`api/level.rs`): `PUT /api/outputs/{node}/volume`,
// `/mute` and `/delay` dispatch on the node's kind, so nothing here has to know a
// transport. What is left is the two maps of **desired** values the daemon keeps for
// devices that may be offline — sparse, keyed by node name, absent meaning "the
// default" — which are per-kind facts rather than per-output ones: only sendspin stores
// intent for a device it cannot reach.

pub(crate) async fn get_sendspin_volumes(State(state): State<AppState>) -> Json<std::collections::HashMap<String, u8>> {
    Json(state.sendspin_control.lock().await.volumes())
}
