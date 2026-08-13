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
        if let Some(enabled) = req.presets_enabled {
            if let Err(e) = s.set_presets_enabled(enabled) {
                return Err(ApiError::internal(format!("failed to persist: {e}")));
            }
        }
        if let Some(expose) = req.expose_outputs_as_media_players {
            if let Err(e) = s.set_expose_outputs_as_media_players(expose) {
                return Err(ApiError::internal(format!("failed to persist: {e}")));
            }
        }
    }
    // Switching presets *off* puts the house back on `Default` (plan §6.1).
    // Otherwise a party grouping would stay in force with nothing left in the UI
    // that names it, and no way to get out of it. Done outside the settings lock,
    // and only when it would actually change something.
    if req.presets_enabled == Some(false) {
        let active = state.groups_config.lock_recover().active_preset().to_string();
        if active != crate::store::groups::DEFAULT_PRESET_ID {
            tracing::info!("presets switched off while '{active}' was active — returning to the default grouping");
            apply_preset(&state, crate::store::groups::DEFAULT_PRESET_ID)?;
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
