use super::*;

// ---- Sync tuning: group lead + per-device static delay -------------------
//
// The user-facing latency dials for group sync (routing/sync_settings.rs). The group
// lead is one daemon-wide value (raise it so the slowest member still plays in
// time; lower it for a snappier start). The per-sendspin-device static delay
// trims one speaker that's consistently early/late. The AP2 per-output
// counterpart is its render delay (`latency_ms`, below), applied live.

#[derive(Serialize)]
pub(crate) struct SyncSettingsInfo {
    /// Group presentation lead in ms (sendspin `send_ahead`) as configured here.
    pub(crate) group_lead_ms: u32,
    /// The largest buffering requirement across present sendspin devices
    /// (`min_buffer_ms` + that device's static delay), in ms. The daemon raises every
    /// group's send-ahead to at least this — the spec makes it mandatory, not advisory
    /// — so configuring less than this has no effect. 0 when no device has reported one
    /// (a device only reports after it connects, and it may report a *different* value
    /// per wire codec, since decode warmup differs).
    pub(crate) group_lead_floor_ms: u32,
    /// What the daemon **would** start a group at now: `max(group_lead_ms,
    /// group_lead_floor_ms)`. Not necessarily what is playing — see
    /// [`Self::group_lead_running_ms`].
    pub(crate) group_lead_effective_ms: u32,
    /// The largest send-ahead any running sendspin server is **actually** streaming at,
    /// in ms; `None` when no group has a live server.
    ///
    /// Reported because it and `group_lead_effective_ms` genuinely diverge, and the UI
    /// showing only the computed one was actively misleading: on 2026-08-12 this instance
    /// reported `effective 130 ms` while the relay logged `lead 899..921 ms` — a value
    /// left over from an earlier experiment, kept because the send-ahead is a high-water
    /// mark. Anyone reading the UI concluded the low value was in force. It was not, and
    /// there was no way to tell from here.
    pub(crate) group_lead_running_ms: Option<u32>,
    /// The same per group, since groups can legitimately differ (each computes its own
    /// requirement from its own members) and one number for all of them would be the
    /// same kind of half-truth.
    pub(crate) group_lead_running: Vec<RunningLead>,
    /// Which device(s) set the floor, for a UI that has to explain why the value it
    /// shows is higher than the one the user typed.
    pub(crate) group_lead_floor_sources: Vec<LeadFloorSource>,
    /// Decode+network headroom imposed on an **Opus** stream, in ms — the one term of
    /// a group's lead that is neither the user's choice nor a device's request, and the
    /// reason an Opus group cannot go below it however low the group lead is set.
    /// Tunable because the shipped 250 ms is a guess (outputs/sendspin/codec.rs).
    pub(crate) opus_floor_ms: u32,
    /// The lowest value `opus_floor_ms` accepts: the Opus block size, since nothing can
    /// be sent before a whole block exists. Sent so the UI can bound its own input
    /// instead of duplicating the arithmetic.
    pub(crate) opus_floor_min_ms: u32,
}

/// What one running group's sendspin server is streaming at right now.
#[derive(Serialize)]
pub(crate) struct RunningLead {
    /// The group's sync-anchor node name (`sync-grp-…`) — the only stable name a group
    /// has, and the one its relay log lines carry.
    pub(crate) anchor: String,
    pub(crate) lead_ms: u32,
}

/// One device's contribution to the send-ahead floor.
#[derive(Serialize)]
pub(crate) struct LeadFloorSource {
    pub(crate) node_name: String,
    pub(crate) name: String,
    /// The codec it's streaming — its requirement changes with this.
    pub(crate) codec: &'static str,
    /// What the device itself asked for (excluding its static delay), if it reported
    /// anything. `None` for firmware that never sends `min_buffer_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) min_buffer_ms: Option<u32>,
    /// The add-on's own minimum for that codec, used when the device is silent.
    pub(crate) codec_minimum_ms: u32,
    /// Its static delay, which the server adds on top per the spec.
    pub(crate) static_delay_ms: u16,
    /// Its effective per-player send-ahead — the larger of the two, plus the delay.
    pub(crate) required_ms: u32,
    /// `"reported"` (the device asked for it) or `"codec-minimum"` (it didn't, so the
    /// add-on's floor for its codec applies). Lets the UI explain the number honestly.
    pub(crate) reason: &'static str,
}

/// The send-ahead floor across present sendspin devices, plus the per-device detail
/// behind it. Mirrors what `sync_group` feeds each group, so the UI shows the same
/// number the audio path uses.
pub(crate) fn lead_floor(state: &AppState) -> (u32, Vec<LeadFloorSource>) {
    let devices = state.sendspin_devices.lock_recover().clone();
    let ss = state.sync_settings.lock_recover();
    let delays = ss.sendspin_delays();
    let opus_floor_ms = ss.opus_floor_ms();
    let mut sources: Vec<LeadFloorSource> = devices
        .iter()
        .filter(|(_, d)| d.present)
        .map(|(node_name, d)| {
            let static_delay_ms = delays.get(node_name).copied().unwrap_or(0);
            let codec = crate::outputs::sendspin::server::resolve_codec(ss.sendspin_codec(node_name), std::iter::once(&d.supported_codecs));
            // Same rule the audio path uses: what the device asked for, else our floor
            // for its codec — and the device's static delay on top either way.
            let codec_minimum_ms = (crate::outputs::sendspin::codec::min_send_ahead_us(codec, opus_floor_ms) / 1000) as u32;
            let (base_ms, reason) = match d.min_buffer_ms {
                Some(m) => (m, "reported"),
                None => (codec_minimum_ms, "codec-minimum"),
            };
            LeadFloorSource {
                node_name: node_name.clone(),
                name: d.display_name.clone(),
                codec,
                min_buffer_ms: d.min_buffer_ms,
                codec_minimum_ms,
                static_delay_ms,
                required_ms: base_ms + u32::from(static_delay_ms),
                reason,
            }
        })
        .filter(|s| s.required_ms > 0)
        .collect();
    // Largest first: the head is the one that actually sets the floor.
    sources.sort_by_key(|s| std::cmp::Reverse(s.required_ms));
    (sources.first().map(|s| s.required_ms).unwrap_or(0), sources)
}

#[derive(Deserialize)]
pub(crate) struct SetSyncSettingsRequest {
    pub(crate) group_lead_ms: u32,
    /// Optional so a UI can change the group lead alone (and so an older client keeps
    /// working); absent leaves the Opus floor as it is.
    #[serde(default)]
    pub(crate) opus_floor_ms: Option<u32>,
}

pub(crate) async fn get_sync_settings(State(state): State<AppState>) -> Json<SyncSettingsInfo> {
    let (configured, opus_floor_ms) = {
        let ss = state.sync_settings.lock_recover();
        (ss.group_lead_ms(), ss.opus_floor_ms())
    };
    let (floor, sources) = lead_floor(&state);
    let running: Vec<RunningLead> =
        state.groups.lock().await.running_sendspin_leads().into_iter().map(|(anchor, lead_ms)| RunningLead { anchor, lead_ms }).collect();
    Json(SyncSettingsInfo {
        group_lead_ms: configured,
        group_lead_floor_ms: floor,
        group_lead_effective_ms: configured.max(floor),
        group_lead_running_ms: running.iter().map(|r| r.lead_ms).max(),
        group_lead_running: running,
        group_lead_floor_sources: sources,
        opus_floor_ms,
        opus_floor_min_ms: crate::outputs::sendspin::codec::opus_floor_lower_bound_ms("opus"),
    })
}

pub(crate) async fn set_sync_settings(State(state): State<AppState>, Json(req): Json<SetSyncSettingsRequest>) -> OpResult {
    {
        let mut ss = state.sync_settings.lock_recover();
        if let Err(e) = ss.set_group_lead_ms(req.group_lead_ms) {
            return Err(ApiError::internal(format!("failed to persist: {e}")));
        }
        if let Some(ms) = req.opus_floor_ms {
            if let Err(e) = ss.set_opus_floor_ms(ms) {
                return Err(ApiError::internal(format!("failed to persist: {e}")));
            }
        }
    }
    // Let this pass apply a LOWER send-ahead as well as a higher one. A send-ahead
    // normally only ratchets up (it is a high-water mark, so an incidental drop doesn't
    // reconnect every speaker for a few ms of latency) — but a user who just typed a
    // smaller number is asking for exactly that reconnect, and without this the only
    // ways down were restarting the add-on or nudging a speaker's static delay up and
    // back to force a group restart.
    let rearmed = state.groups.lock().await.rearm_lead();
    // Nudge the reconciler; it re-reads the lead each tick and restarts group
    // servers so the new value takes effect promptly.
    let _ = state.changes.send(());
    // The daemon raises the lead to the device-reported floor regardless, so a value
    // below it is stored but not used — say so rather than letting the UI imply it took.
    let (floor, sources) = lead_floor(&state);
    let effective = req.group_lead_ms.max(floor);
    let mut message = match sources.first() {
        Some(top) if floor > req.group_lead_ms && top.reason == "reported" => format!(
            "group lead set to {} ms, but {} ms is used — '{}' asks for that much buffer with {}",
            req.group_lead_ms, floor, top.name, top.codec
        ),
        Some(top) if floor > req.group_lead_ms => format!(
            "group lead set to {} ms, but {} ms is used — {} needs that much head start to decode in time on '{}'",
            req.group_lead_ms, floor, top.codec, top.name
        ),
        _ => format!("group lead set to {} ms", req.group_lead_ms),
    };
    // Only mention the cost when there is one: a group whose requirement is unchanged
    // stays exactly as it is, re-arm or no re-arm.
    let lowering = state.groups.lock().await.running_sendspin_leads().values().any(|running| *running != effective);
    if rearmed > 0 && lowering {
        message.push_str(&format!(
            " — {rearmed} group(s) restart to apply it, so each speaker reconnects (a few seconds of silence per speaker)"
        ));
    }
    ok(message)
}

/// General app settings (store/settings.rs) — the Settings page's General
/// section. Group lead lives on `/api/sync/settings` (it's sync-specific).
#[derive(Serialize)]
pub(crate) struct SettingsInfo {
    pub(crate) default_duck: f32,
    pub(crate) discovery_enabled: bool,
    pub(crate) sendspin_delay_live: bool,
    pub(crate) presets_enabled: bool,
    pub(crate) expose_outputs_as_media_players: bool,
}

/// Partial update: every field is optional so the UI can PATCH one knob at a time.
#[derive(Deserialize)]
pub(crate) struct SetSettingsRequest {
    #[serde(default)]
    pub(crate) default_duck: Option<f32>,
    #[serde(default)]
    pub(crate) discovery_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) sendspin_delay_live: Option<bool>,
    #[serde(default)]
    pub(crate) presets_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) expose_outputs_as_media_players: Option<bool>,
}

pub(crate) fn settings_info(state: &AppState) -> SettingsInfo {
    let s = state.settings.lock_recover();
    SettingsInfo {
        default_duck: s.default_duck(),
        discovery_enabled: s.discovery_enabled(),
        sendspin_delay_live: s.sendspin_delay_live(),
        presets_enabled: s.presets_enabled(),
        expose_outputs_as_media_players: s.expose_outputs_as_media_players(),
    }
}
