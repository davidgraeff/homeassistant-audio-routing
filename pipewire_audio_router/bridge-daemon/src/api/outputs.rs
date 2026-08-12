use super::*;
use crate::outputs::listing::{output_label, outputs_listings, OutputInfo};

#[derive(Serialize)]
pub(crate) struct OutputOpResponse {
    pub(crate) ok: bool,
    pub(crate) message: String,
}

/// Build a sendspin output's codec picker: the stored choice, what it resolves to
/// right now, and per-codec availability.
///
/// Two independent reasons a codec can be unavailable, and the UI needs to say
/// which: **we can't encode it yet** (no encoder in the daemon — currently
/// The system's outputs: adopted only. Everything downstream — the routing
/// matrix's counterpart listing, the group editors, the alignment panel, the HA
/// integration's per-output metadata — means *these* by "output". A discovered
/// device the user hasn't added yet is deliberately absent.
pub(crate) async fn list_outputs(State(state): State<AppState>) -> Json<Vec<OutputInfo>> {
    Json(outputs_listings(&state).await.0)
}

/// Devices discovery has *offered* but the user hasn't added: `state` is
/// `"discovered"` or `"ignored"`. Both are returned in one listing so the
/// Outputs page's "show ignored" checkbox is a client-side filter rather than a
/// refetch. Carries the same connection details, codec picker and test-playback
/// eligibility as an adopted output, because identifying a device ("which
/// speaker is `ap2-dev-living-2`?") is exactly what you need before deciding.
pub(crate) async fn list_discovered_outputs(State(state): State<AppState>) -> Json<Vec<OutputInfo>> {
    Json(outputs_listings(&state).await.1)
}

/// Drop every trace of an output from the *intent* stores: saved routing links
/// and music/announcement group membership. Shared by remove and ignore — both
/// mean "this is not one of my outputs", and leaving a stale link or group
/// member behind would silently re-route it if it were added again later.
/// Returns a human-readable note about what was cleaned, for the toast.
pub(crate) fn forget_output_intent(state: &AppState, node_name: &str) -> String {
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
pub(crate) async fn adopt_output(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
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
pub(crate) async fn unpair_output(State(state): State<AppState>, Path(node_name): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
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
pub(crate) async fn ignore_output(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
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
pub(crate) async fn remove_output(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
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
