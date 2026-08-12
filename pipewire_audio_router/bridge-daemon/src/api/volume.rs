use super::*;

// ---- Sendspin per-device volume ------------------------------------------
//
// Sendspin devices are virtual outputs fed by a shared group sink, so there's
// no PipeWire node volume to drive. Volume is carried in-band over the sendspin
// protocol to the specific device; see
// outputs/sendspin/volume.rs. `GET` returns the desired volume per device node name
// (sparse — absent means the default); `PUT` sets one device.

#[derive(Deserialize)]
pub(crate) struct SetSendspinVolumeRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    pub(crate) node_name: String,
    /// Target volume, 0–100.
    pub(crate) volume: u8,
}

pub(crate) async fn get_sendspin_volumes(State(state): State<AppState>) -> Json<std::collections::HashMap<String, u8>> {
    Json(state.sendspin_control.lock().await.volumes())
}

pub(crate) async fn set_sendspin_volume(
    State(state): State<AppState>,
    Json(req): Json<SetSendspinVolumeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Two statements: the control guard must drop before the send is awaited
    // (see outputs::sendspin::volume::PendingCommands).
    let pending = state.sendspin_control.lock().await.set_volume(&req.node_name, req.volume);
    let reached = pending.apply().await;
    let message = if reached {
        format!("set '{}' to {}%", req.node_name, req.volume.min(100))
    } else {
        // Stored; will apply when the device (re)connects.
        format!("saved {}% for '{}' (device not connected)", req.volume.min(100), req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
pub(crate) struct SetSendspinMuteRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    pub(crate) node_name: String,
    /// Target mute state.
    pub(crate) muted: bool,
}

pub(crate) async fn set_sendspin_mute(
    State(state): State<AppState>,
    Json(req): Json<SetSendspinMuteRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let pending = state.sendspin_control.lock().await.set_muted(&req.node_name, req.muted);
    let reached = pending.apply().await;
    let verb = if req.muted { "muted" } else { "unmuted" };
    let message = if reached {
        format!("{verb} '{}'", req.node_name)
    } else {
        format!("saved {verb} for '{}' (device not connected)", req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
pub(crate) struct ClearSendspinRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    pub(crate) node_name: String,
}

/// Ask one sendspin device to discard buffered-but-unplayed audio and re-anchor,
/// without ending its stream (`stream/clear`).
///
/// The recovery action for the failure mode where a device is demonstrably being
/// *sent* audio and renders nothing — measured on 2026-08-03, when three of four
/// devices went silent while the daemon, the graph and the clock sync were all
/// healthy (docs/sendspin-open-items.md). Until this existed the only lever was
/// restarting the whole add-on, which fixed it but destroyed the evidence and
/// interrupted every other output.
///
/// Cheaper and more surgical than the alternatives: a per-device *reconnect* (nudge
/// its static delay) costs a full re-dial and a fresh clock filter for that device,
/// and a group restart costs it for everyone. This is one frame.
///
/// A disconnected device is reported honestly rather than treated as success — there
/// is nothing to clear, and its next stream starts fresh anyway.
pub(crate) async fn clear_sendspin_stream(
    State(state): State<AppState>,
    Json(req): Json<ClearSendspinRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Two statements, never one: holding the control guard across the await would
    // block every other device's commands behind this device's socket.
    let pending = state.sendspin_control.lock().await.clear_stream(&req.node_name);
    let reached = pending.apply().await;
    let display = output_label(&state, &req.node_name);
    if reached {
        tracing::info!("USER ACTION: sendspin stream/clear -> '{}' (buffers discarded, re-anchoring)", req.node_name);
    }
    let message = if reached {
        format!("cleared '{display}' — it will re-anchor on the next audio")
    } else {
        format!("'{display}' has no live connection, so there is nothing to clear")
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: reached, message }))
}

// ---- AirPlay-2 per-device volume/mute ------------------------------------
//
// AP2 receivers are virtual outputs (`ap2-dev-…`) like sendspin: no PipeWire
// node volume. Volume is carried in-band as an RTSP SET_PARAMETER the sender
// pushes to the receiver (outputs/ap2/volume.rs → outputs/ap2/server.rs); mute is volume 0.
// Volume is 0.0–1.0 (matches the receiver's dB mapping and the matrix field).
// There's no receiver→daemon feedback yet, so the UI shows the last-set level.

#[derive(Deserialize)]
pub(crate) struct SetAp2VolumeRequest {
    /// Virtual device node name, e.g. `ap2-dev-yamaha`.
    pub(crate) node_name: String,
    /// Target volume, 0.0–1.0.
    pub(crate) volume: f32,
}

pub(crate) async fn set_ap2_volume(
    State(state): State<AppState>,
    Json(req): Json<SetAp2VolumeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let reached = state.ap2_control.lock().await.set_volume(&req.node_name, req.volume).await;
    let pct = (req.volume.clamp(0.0, 1.0) * 100.0).round() as u8;
    let message = if reached {
        format!("set '{}' to {}%", req.node_name, pct)
    } else {
        format!("saved {}% for '{}' (not streaming)", pct, req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
pub(crate) struct SetAp2MuteRequest {
    pub(crate) node_name: String,
    pub(crate) muted: bool,
}

pub(crate) async fn set_ap2_mute(
    State(state): State<AppState>,
    Json(req): Json<SetAp2MuteRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let reached = state.ap2_control.lock().await.set_muted(&req.node_name, req.muted).await;
    let verb = if req.muted { "muted" } else { "unmuted" };
    let message =
        if reached { format!("{verb} '{}'", req.node_name) } else { format!("saved {verb} for '{}' (not streaming)", req.node_name) };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}
