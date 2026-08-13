use super::*;

pub(crate) async fn get_settings(State(state): State<AppState>) -> Json<SettingsInfo> {
    Json(settings_info(&state))
}

pub(crate) async fn set_settings(State(state): State<AppState>, Json(req): Json<SetSettingsRequest>) -> OpResult {
    // Persist each provided field.
    {
        let mut s = state.settings.lock_recover();
        if let Some(d) = req.default_duck {
            if let Err(e) = s.set_default_duck(d) {
                return Err(ApiError::internal(format!("failed to persist: {e}")));
            }
        }
        if let Some(enabled) = req.discovery_enabled {
            if let Err(e) = s.set_discovery_enabled(enabled) {
                return Err(ApiError::internal(format!("failed to persist: {e}")));
            }
        }
        if let Some(live) = req.sendspin_delay_live {
            if let Err(e) = s.set_sendspin_delay_live(live) {
                return Err(ApiError::internal(format!("failed to persist: {e}")));
            }
        }
        if let Some(expose) = req.expose_outputs_as_media_players {
            if let Err(e) = s.set_expose_outputs_as_media_players(expose) {
                return Err(ApiError::internal(format!("failed to persist: {e}")));
            }
        }
    }
    // Apply the discovery flag live (outside the settings lock). A spawn failure
    // is reported but the flag stays persisted — it'll retry on next boot.
    if let Some(enabled) = req.discovery_enabled {
        if let Err(e) = state.discovery.set_enabled(enabled) {
            return Err(ApiError::internal(format!("failed to apply discovery: {e}")));
        }
    }
    ok("settings saved".to_string())
}
