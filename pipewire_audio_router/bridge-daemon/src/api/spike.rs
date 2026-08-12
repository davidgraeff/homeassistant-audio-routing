use super::*;

/// S3 spike (spike/per_device.rs): stand up one per-device PipeWire node +
/// single-member sendspin sender for `device`, optionally fed from `source`.
#[derive(Deserialize)]
pub(crate) struct SpikeStartRequest {
    /// The discovered sendspin device's node name (`sendspin-dev-…`).
    pub(crate) device: String,
    /// Source node to link into the per-device sink (its audio path). Optional:
    /// without it the node is created but silent until something is routed in.
    #[serde(default)]
    pub(crate) source: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SpikeStartResponse {
    pub(crate) ok: bool,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) spike: Option<crate::spike::per_device::SpikeInfo>,
}

pub(crate) async fn spike_per_device_start(
    State(state): State<AppState>,
    Json(req): Json<SpikeStartRequest>,
) -> (StatusCode, Json<SpikeStartResponse>) {
    let send_ahead_us = state.sync_settings.lock_recover().group_lead_us();
    match crate::spike::per_device::start(
        &req.device,
        req.source.as_deref(),
        &state.pw,
        &state.pw_cmd,
        &state.changes,
        &state.routing,
        &state.sendspin_devices,
        &state.sendspin_control,
        send_ahead_us,
    )
    .await
    {
        Ok(info) => (StatusCode::OK, Json(SpikeStartResponse { ok: true, message: info.message.clone(), spike: Some(info) })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(SpikeStartResponse { ok: false, message: e, spike: None })),
    }
}

pub(crate) async fn spike_per_device_stop(State(state): State<AppState>) -> (StatusCode, Json<OutputOpResponse>) {
    match crate::spike::per_device::stop(&state.pw_cmd, &state.changes, &state.routing).await {
        Ok(msg) => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: msg })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })),
    }
}

/// AirPlay-2 synchronized **test tone** spike (spike/ap2.rs). Streams a sine tone
/// to the target receivers via the PROVEN file path (`start_streaming`), bypassing
/// the live-capture producer, to check that AP2 + PTP multi-room works on the Pi.
#[derive(Deserialize)]
pub(crate) struct Ap2SpikeRequest {
    /// Explicit receiver IPs. If empty, all present discovered AP2 receivers.
    #[serde(default)]
    pub(crate) ips: Vec<String>,
    /// Tone frequency in Hz (default 440).
    #[serde(default)]
    pub(crate) freq: Option<f32>,
    /// Tone duration in seconds (default 60).
    #[serde(default)]
    pub(crate) seconds: Option<f32>,
    /// Render delay in ms (default `outputs::ap2::server::AP2_RENDER_DELAY_MS`).
    #[serde(default)]
    pub(crate) render_delay_ms: Option<u32>,
    /// Streaming path to exercise: `"file"` (default; `start_streaming`, known-good)
    /// or `"live"` (`start_streaming_live` + `LiveAudioDecoder`, the live-output path
    /// fed a clean synthetic tone) — the bisection knob for the live-path silence.
    #[serde(default)]
    pub(crate) mode: Option<String>,
    /// Wire sample rate in Hz: 44100 (default) or 48000 — to test whether the
    /// receivers accept 48 kHz realtime ALAC (drives the ALAC cookie + SETUP
    /// `audioFormat` bit). Anything ≥ 48000 is treated as 48000.
    #[serde(default)]
    pub(crate) rate: Option<u32>,
    /// Source clip: `"tone"` (default, generated sine) or `"voice"` (the embedded
    /// `test-announcement.mp3`, decoded + resampled to `rate`). A voice makes a
    /// wrong playback rate obvious to the ear. `"voice"` forces file mode.
    #[serde(default)]
    pub(crate) clip: Option<String>,
}

pub(crate) async fn spike_ap2_start(
    State(state): State<AppState>,
    Json(req): Json<Ap2SpikeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Resolve targets: explicit IPs, else every present discovered AP2 receiver.
    let targets: Vec<(String, std::net::IpAddr)> = if !req.ips.is_empty() {
        req.ips.iter().filter_map(|s| s.parse::<std::net::IpAddr>().ok().map(|ip| (s.clone(), ip))).collect()
    } else {
        state
            .ap2_devices
            .lock_recover()
            .values()
            .filter(|d| d.present)
            .filter_map(|d| d.addr.map(|a| (d.display_name.clone(), a.ip())))
            .collect()
    };
    if targets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse { ok: false, message: "no target receivers (none discovered/present and no valid ips given)".into() }),
        );
    }

    let freq = req.freq.unwrap_or(440.0);
    let secs = req.seconds.unwrap_or(60.0);
    let delay = req.render_delay_ms.unwrap_or(crate::outputs::ap2::server::AP2_RENDER_DELAY_MS);
    let rate = if req.rate.unwrap_or(44_100) >= 48_000 { 48_000 } else { 44_100 };
    // "voice" = play the embedded test clip (decoded to WAV; the spike's file path
    // then resamples it to `rate`). A voice reveals a wrong playback rate by ear.
    // Forces file mode (live is the synthetic sine only).
    let voice = req.clip.as_deref() == Some("voice");
    let (live, file_wav) = if voice {
        match crate::audio::decode::decode_bytes_to_wav(include_bytes!("../../assets/test-announcement.mp3"), "mp3").await {
            Ok(wav) => (false, Some(wav)),
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("decode test clip: {e}") }))
            }
        }
    } else {
        (req.mode.as_deref() == Some("live"), None)
    };

    match crate::spike::ap2::start(targets, &state.ap2_ptp, freq, secs, delay, live, rate, file_wav).await {
        Ok(info) => {
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{} — {}", info.message, info.targets.join(", ")) }))
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })),
    }
}

pub(crate) async fn spike_ap2_stop() -> (StatusCode, Json<OutputOpResponse>) {
    let msg = crate::spike::ap2::stop().await;
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: msg }))
}

/// pw-sink transport spike (spike/pwsink.rs). Streams a self-driving test tone
/// to a remote PipeWire host via native rtp-sink + rtp-sap — the real-LAN A/B
/// oracle for the pw-sink output backend. No user interaction; the remote host
/// (running rtp-sap in discover mode) auto-creates a source and plays it.
#[derive(Deserialize)]
pub(crate) struct PwSinkSpikeRequest {
    /// Target remote host, unicast IPv4 (required — rtp-sink unicasts to it).
    pub(crate) target_ip: String,
    /// Tone frequency in Hz (default 440).
    #[serde(default)]
    pub(crate) freq: Option<f32>,
    /// Optional LAN interface name to pin egress/advert to (default `end0` on
    /// the HA host — avoids host-network multi-iface fan-out).
    #[serde(default)]
    pub(crate) ifname: Option<String>,
}

pub(crate) async fn spike_pwsink_start(
    State(state): State<AppState>,
    Json(req): Json<PwSinkSpikeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if req.target_ip.parse::<std::net::IpAddr>().is_err() {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("invalid target_ip '{}'", req.target_ip) }));
    }
    let freq = req.freq.unwrap_or(440.0);
    let ifname = req.ifname.as_deref().or(Some("end0"));
    match crate::spike::pwsink::start(&state.pw, &state.pw_cmd, &req.target_ip, freq, ifname).await {
        Ok(info) => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: info.message })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })),
    }
}

pub(crate) async fn spike_pwsink_stop() -> (StatusCode, Json<OutputOpResponse>) {
    crate::spike::pwsink::stop().await;
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: "pw-sink spike stopped".into() }))
}

/// Multi-device shared-timeline spike (S1): one anchor + one timeline driving one
/// sender per device. Teardown reuses `spike_per_device_stop` (same slot).
#[derive(Deserialize)]
pub(crate) struct SpikeMultiRequest {
    /// Two or more discovered sendspin device node names.
    pub(crate) devices: Vec<String>,
    #[serde(default)]
    pub(crate) source: Option<String>,
}

pub(crate) async fn spike_multi_device_start(
    State(state): State<AppState>,
    Json(req): Json<SpikeMultiRequest>,
) -> (StatusCode, Json<SpikeStartResponse>) {
    let send_ahead_us = state.sync_settings.lock_recover().group_lead_us();
    match crate::spike::per_device::start_multi(
        &req.devices,
        req.source.as_deref(),
        &state.pw,
        &state.pw_cmd,
        &state.changes,
        &state.routing,
        &state.sendspin_devices,
        &state.sendspin_control,
        send_ahead_us,
    )
    .await
    {
        Ok(info) => (StatusCode::OK, Json(SpikeStartResponse { ok: true, message: info.message.clone(), spike: Some(info) })),
        Err(e) => (StatusCode::BAD_REQUEST, Json(SpikeStartResponse { ok: false, message: e, spike: None })),
    }
}

/// Overlay spike (outputs/overlay_mixer.rs): inject a test-tone announcement overlay on
/// one output. Audible on any sendspin device with a running per-device sender —
/// i.e. any discovered device (grouped, or via its always-on idle sender).
#[derive(Deserialize)]
pub(crate) struct OverlayStartRequest {
    /// The sendspin device's output node name (`sendspin-dev-…`).
    pub(crate) device: String,
    #[serde(default)]
    pub(crate) seconds: Option<f32>,
    #[serde(default)]
    pub(crate) freq: Option<f32>,
    /// Music duck gain while the overlay plays (0–1); default 0.25.
    #[serde(default)]
    pub(crate) duck: Option<f32>,
}

pub(crate) async fn spike_overlay_start(Json(req): Json<OverlayStartRequest>) -> (StatusCode, Json<OutputOpResponse>) {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seconds = req.seconds.unwrap_or(6.0);
    let freq = req.freq.unwrap_or(660.0);
    let duck = req.duck.unwrap_or(0.25);
    let pcm = crate::outputs::overlay_mixer::test_tone(seconds, freq, 0.3);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::outputs::overlay_mixer::OverlayMixer::global().start(&req.device, id, pcm, duck);
    (
        StatusCode::OK,
        Json(OutputOpResponse {
            ok: true,
            message: format!(
                "overlay {freq}Hz for {seconds}s on '{}' (duck {duck}); audible only if that device is on per-device senders",
                req.device
            ),
        }),
    )
}

pub(crate) async fn spike_overlay_stop(Query(q): Query<std::collections::HashMap<String, String>>) -> (StatusCode, Json<OutputOpResponse>) {
    match q.get("device") {
        Some(device) => {
            let stopped = crate::outputs::overlay_mixer::OverlayMixer::global().stop(device).is_some();
            (
                StatusCode::OK,
                Json(OutputOpResponse {
                    ok: true,
                    message: format!("overlay on '{device}': {}", if stopped { "stopped" } else { "none active" }),
                }),
            )
        }
        None => (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: "missing ?device=".to_string() })),
    }
}

/// Announcement-group announce (announce/mod.rs): play a clip to a set of per-device
/// outputs with per-device duck+overlay and scheduler policy (queue/barge/TTL).
///
/// Each target needs a per-device sender to *consume* its overlay, so the handler
/// first ensures one exists — including opening an **on-demand AP2 session** for a
/// receiver with nothing routed into it (routing/sync_group.rs) — and reports any target
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
    /// Use the built-in calibration tone (the `align/calibrate.rs` click track) as a
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
    pub(crate) ok: bool,
    /// "playing" | "queued" | "rejected".
    pub(crate) admission: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) position: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    pub(crate) message: String,
}
