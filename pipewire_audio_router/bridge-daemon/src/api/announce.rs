use super::*;

/// Announcement-group announce (announce/mod.rs): play a clip to a set of per-device
/// outputs with per-device duck+overlay and scheduler policy (queue/barge/TTL).
///
/// Each target needs a per-device sender to *consume* its overlay, so the handler
/// first ensures one exists — including opening an **on-demand AP2 session** for a
/// receiver with nothing routed into it (routing/sync_group/mod.rs) — and reports any target
/// nothing can carry instead of dropping the clip silently.
#[derive(Deserialize)]
pub(crate) struct AgAnnounceRequest {
    /// Target output node names (`sendspin-dev-…`). Optional if
    /// `announcement_group` is given (its targets are used).
    #[serde(default)]
    pub(crate) targets: Vec<String>,
    /// Named announcement group (store/groups.rs) to resolve targets/priority/duck.
    #[serde(default)]
    pub(crate) announcement_group: Option<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
    /// Use the built-in test-announcement clip (no url needed).
    #[serde(default)]
    pub(crate) test: bool,
    /// Use the built-in calibration tone (the `align/calibrate/mod.rs` click track) as a
    /// quick "is this speaker alive and correctly wired" check.
    #[serde(default)]
    pub(crate) tone: bool,
    #[serde(default)]
    pub(crate) priority: i32,
    /// "queue" (default) or "reject" when the targets are busy.
    #[serde(default)]
    pub(crate) on_busy: Option<String>,
    #[serde(default)]
    pub(crate) barge_in: bool,
    #[serde(default)]
    pub(crate) ttl_ms: Option<u64>,
    #[serde(default)]
    pub(crate) duck: Option<f32>,
}

#[derive(Serialize)]
pub(crate) struct AgAnnounceResponse {
    /// "playing" | "queued" | "rejected" — the *outcome*, on a 200. A rejection is the
    /// arbiter's decision, not a failed request, so it is a value here rather than a
    /// status (see `api::error` for where the line is).
    pub(crate) admission: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) position: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    pub(crate) message: String,
}

/// Acquire the announce audio as 48k/S16/stereo PCM from one of test/tone/url.
pub(crate) async fn acquire_announce_pcm(req: &AgAnnounceRequest) -> Result<Vec<u8>, String> {
    if req.test {
        let wav = crate::audio::decode::decode_bytes_to_wav(include_bytes!("../../assets/test-announcement.mp3"), "mp3")
            .await
            .map_err(|e| format!("decode test clip: {e}"))?;
        let (rate, ch, pcm) = crate::audio::wav::read_pcm16(&wav).ok_or("test clip not a PCM WAV")?;
        return Ok(crate::audio::resample::to_48k_stereo_s16le(pcm, rate, ch));
    }
    if req.tone {
        // The calibration click (align/calibrate/mod.rs) — already 16-bit PCM WAV, so no
        // decode step; just standardize to the announce mix format.
        let wav = crate::align::calibrate::click_wav();
        let (rate, ch, pcm) = crate::audio::wav::read_pcm16(&wav).ok_or("tone clip not a PCM WAV")?;
        return Ok(crate::audio::resample::to_48k_stereo_s16le(pcm, rate, ch));
    }
    match &req.url {
        Some(url) => {
            let path = std::env::temp_dir().join("ag-announce-fetch");
            let _ = tokio::fs::remove_file(&path).await;
            fetch_to_file(url, &path).await.map_err(|e| format!("fetch: {e}"))?;
            let pcm = crate::audio::decode::decode_file_to_pcm_48k_stereo(&path).await.map_err(|e| format!("decode: {e}"));
            let _ = tokio::fs::remove_file(&path).await;
            pcm
        }
        None => Err("provide exactly one of: test, tone, url".to_string()),
    }
}

pub(crate) async fn ag_announce(
    State(state): State<AppState>,
    Json(req): Json<AgAnnounceRequest>,
) -> Result<Json<AgAnnounceResponse>, ApiError> {
    let reject = ApiError::bad_request;

    // Resolve effective targets/priority/duck — optionally from a named
    // announcement group (explicit `targets`/`duck` in the request still win).
    // Done before the await below so the store guard doesn't span it.
    let (targets, priority, ag_duck) = {
        let mut targets = req.targets.clone();
        let mut priority = req.priority;
        let mut ag_duck = None;
        if let Some(agid) = &req.announcement_group {
            let store = state.groups_config.lock_recover();
            match store.announcement_by_id(agid) {
                Some(ag) => {
                    if targets.is_empty() {
                        targets = ag.targets.clone();
                    }
                    priority = ag.priority;
                    ag_duck = Some(ag.duck);
                }
                None => return Err(reject(format!("no announcement group '{agid}'"))),
            }
        }
        (targets, priority, ag_duck)
    };
    if targets.is_empty() {
        return Err(reject("no targets (provide `targets` or `announcement_group`)".into()));
    }
    let duck = req.duck.or(ag_duck).unwrap_or_else(|| state.settings.lock_recover().default_duck());
    let on_busy = match req.on_busy.as_deref() {
        Some("reject") => crate::announce::arbiter::OnBusy::Reject,
        _ => crate::announce::arbiter::OnBusy::Queue,
    };

    // Make sure each target actually has a sender that will *consume* the overlay.
    // An announcement is only audible while a per-device relay reads its output's
    // overlay slot, and an unrouted AP2 receiver / pw-sink target has no sender at
    // all — so this opens an on-demand session for it. Done before the (possibly
    // slow) clip fetch/decode so the connect overlaps it. Targets that nothing can
    // carry are dropped from the announcement and reported, rather than silently
    // swallowing the clip and answering "playing".
    use crate::routing::sync_group::{AnnounceDeps, AnnounceTransport};
    let mut transports: Vec<(String, AnnounceTransport)> = Vec::with_capacity(targets.len());
    {
        let deps = AnnounceDeps {
            pw: &state.pw,
            pw_cmd: &state.pw_cmd,
            routing: &state.routing,
            outputs: &state.outputs,
            ap2_devices: &state.ap2_devices,
            ap2_ptp: &state.ap2_ptp,
            ap2_control: &state.ap2_control,
            sync_settings: &state.sync_settings,
            agents: &state.agents,
        };
        let mut groups = state.groups.lock().await;
        for target in &targets {
            let t = groups.ensure_announce_transport(target, &deps).await;
            transports.push((target.clone(), t));
        }
    }
    let skipped: Vec<String> = transports
        .iter()
        .filter_map(|(t, s)| match s {
            AnnounceTransport::Unavailable(why) => Some(format!("{} ({why})", output_label(&state, t))),
            _ => None,
        })
        .collect();
    let starting: Vec<String> =
        transports.iter().filter(|(_, s)| matches!(s, AnnounceTransport::Starting)).map(|(t, _)| output_label(&state, t)).collect();
    // A clip may sit unconsumed while an on-demand session pairs up; give the
    // mixer's stall watchdog a matching grace so it isn't reaped mid-connect.
    let grace = if transports.iter().any(|(_, s)| s.is_on_demand()) {
        crate::outputs::overlay_mixer::OVERLAY_ONDEMAND_GRACE
    } else {
        crate::outputs::overlay_mixer::OVERLAY_STALL_GRACE
    };
    let targets: Vec<String> =
        transports.into_iter().filter(|(_, s)| !matches!(s, AnnounceTransport::Unavailable(_))).map(|(t, _)| t).collect();
    if targets.is_empty() {
        return Err(reject(format!("no target can play an announcement right now: {}", skipped.join("; "))));
    }

    let pcm = match acquire_announce_pcm(&req).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return Err(reject("empty audio".into())),
        Err(e) => return Err(reject(e)),
    };

    let target_count = targets.len();
    // Display names captured before `targets` is moved into the coordinator, for
    // the log line below.
    let played: Vec<String> = targets.iter().map(|t| output_label(&state, t)).collect();
    let admission =
        crate::announce::AnnounceCoordinator::global().announce(targets, pcm, duck, priority, on_busy, req.barge_in, req.ttl_ms, grace);
    use crate::announce::arbiter::Admission;
    // `admission` is the outcome, and it is *not* a failure: the request was carried
    // out, the arbiter decided. So this stays a 200 with a machine-readable verdict —
    // which is what the `ok` flag it replaces was reaching for.
    let (label, position, reason) = match admission {
        Admission::Playing => ("playing", None, None),
        Admission::Queued { position } => ("queued", Some(position), None),
        Admission::Rejected(r) => ("rejected", None, Some(format!("{r:?}"))),
    };
    // Log the whole decision, once, at INFO. An announcement makes an admission
    // decision and can silently drop targets, and until this line existed the
    // daemon logged *nothing at all* for one — an 8 h window of the live log
    // contained zero announce entries, so "the tone didn't play" could not be
    // answered from the log and had to be reconstructed with tcpdump. Everything a
    // future "it didn't play" report needs is here: what was asked for, what was
    // admitted, what is still connecting, and what was dropped and why.
    tracing::info!(
        "USER ACTION: announce -> {} target(s) [{}]: {label}{}{}",
        target_count,
        played.join(", "),
        if starting.is_empty() { String::new() } else { format!(" (on-demand: {})", starting.join(", ")) },
        if skipped.is_empty() { String::new() } else { format!(" SKIPPED: {}", skipped.join("; ")) },
    );

    let mut message = format!("announce to {target_count} target(s): {label}");
    if !starting.is_empty() {
        // Honest about the wait: the endpoint has to connect first (AP2: pair +
        // SETUP + its render buffer; pw-sink: discover our advert and handshake).
        message.push_str(&format!(" — opening an on-demand session for {} (audio starts in a few seconds)", starting.join(", ")));
    }
    if !skipped.is_empty() {
        message.push_str(&format!("; skipped {}", skipped.join("; ")));
    }
    Ok(Json(AgAnnounceResponse { admission: label.to_string(), position, reason, message }))
}
