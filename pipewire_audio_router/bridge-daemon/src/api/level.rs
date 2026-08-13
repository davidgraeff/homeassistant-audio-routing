//! One output's **level, mute, delay and recovery** — addressed by node name in the
//! path, dispatched to whatever transport that output actually has.
//!
//! ## Why these are per-output rather than per-kind
//!
//! Every output kind carries its level over its own transport — sendspin in-band over
//! its protocol, AirPlay 2 as an RTSP `SET_PARAMETER`, a pw-sink host through its
//! receiver agent — so there used to be one endpoint per kind, and they did not even
//! agree on a scale (sendspin 0–100, AP2 and pw-sink 0.0–1.0). Both consumers therefore
//! carried a dispatcher keyed off the node-name prefix, and one of them documented the
//! bug that caused: a `pwsink-dev-*` name sent to `PUT /api/sendspin/volume` was
//! **stored as a desired value for a device that will never connect** and answered
//! `200 {ok: true}`, so the click looked accepted and the next pushed frame put the old
//! value back — the "mute flips back on its own" symptom.
//!
//! That is a property of the API shape, not a slip: a per-kind endpoint cannot tell an
//! out-of-kind name from a device that is merely offline, because both are "not
//! connected". Addressing the *output* and dispatching on
//! [`crate::util::node_names::OutputKind`] makes it unrepresentable — a name whose kind
//! has no such knob is a `400` naming the kind, and an unknown name is a `404`.
//!
//! ## One scale
//!
//! `0.0`–`1.0` everywhere, matching Home Assistant's `volume_level` and `wpctl`. The
//! conversions live here, once: sendspin's protocol wants 0–100, a pw-sink host wants
//! the cubic 0.0–1.0 its own lever uses, AP2 wants 0.0–1.0.

use super::*;
use crate::util::node_names::OutputKind;

/// A level on the wire: `0.0`–`1.0`, clamped rather than rejected — a slider that
/// overshoots by a rounding step should not fail.
#[derive(Deserialize)]
pub(crate) struct SetVolumeRequest {
    pub(crate) volume: f32,
}

#[derive(Deserialize)]
pub(crate) struct SetMuteRequest {
    pub(crate) muted: bool,
}

/// The timing knob, in milliseconds. Its **polarity is per kind** and is reported by
/// `GET /api/outputs`, not encoded in this path: for sendspin it is an *advance* (the
/// device plays that much earlier), for AP2 and pw-sink a delay.
#[derive(Deserialize)]
pub(crate) struct SetDelayRequest {
    /// Omitted or `null` puts the output back on its default: no advance for sendspin,
    /// the sender's own render delay for AP2, the receiving module's own buffer for
    /// pw-sink.
    #[serde(default)]
    pub(crate) delay_ms: Option<u16>,
}

fn clamp_unit(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// `0.0`–`1.0` → sendspin's 0–100.
fn to_percent(v: f32) -> u8 {
    (clamp_unit(v) * 100.0).round() as u8
}

fn bad_request(message: String) -> (StatusCode, Json<OutputOpResponse>) {
    (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message }))
}

/// The one refusal every handler here shares: a node name that is not an output kind
/// this daemon drives at all.
fn kind_of(node_name: &str) -> Result<OutputKind, (StatusCode, Json<OutputOpResponse>)> {
    OutputKind::of(node_name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(OutputOpResponse {
                ok: false,
                message: format!("'{node_name}' is not an output this daemon drives (sendspin, AirPlay 2 or a PipeWire host)"),
            }),
        )
    })
}

/// `PUT /api/outputs/{node_name}/volume` — set one output's level, `0.0`–`1.0`.
pub(crate) async fn set_output_volume(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetVolumeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let kind = match kind_of(&node_name) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let display = output_label(&state, &node_name);
    let volume = clamp_unit(req.volume);
    match kind {
        OutputKind::Sendspin => {
            // Two statements: the control guard must drop before the send is awaited
            // (see outputs::sendspin::volume::PendingCommands).
            let pending = state.sendspin_control.lock().await.set_volume(&node_name, to_percent(volume));
            let reached = pending.apply().await;
            let message = if reached {
                format!("set '{display}' to {}%", to_percent(volume))
            } else {
                // Stored, and *said* to be stored: this is the one honest form of the
                // answer that used to hide an out-of-kind write.
                format!("saved {}% for '{display}' (device not connected)", to_percent(volume))
            };
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
        }
        OutputKind::Airplay2 => {
            state.ap2_control.lock().await.set_volume(&node_name, volume);
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{display}' to {:.0}%", volume * 100.0) }))
        }
        OutputKind::PwSink => {
            // A host owns its level and reports it back, so there is nothing to store:
            // an unreachable host is an error, never a queued intent.
            if state.agents.lock().await.set_volume(&node_name, volume) {
                (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{display}' to {:.0}%", volume * 100.0) }))
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(OutputOpResponse { ok: false, message: format!("no agent connected for '{display}'") }),
                )
            }
        }
    }
}

/// `PUT /api/outputs/{node_name}/mute`.
pub(crate) async fn set_output_mute(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetMuteRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let kind = match kind_of(&node_name) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let display = output_label(&state, &node_name);
    let verb = if req.muted { "muted" } else { "unmuted" };
    match kind {
        OutputKind::Sendspin => {
            let pending = state.sendspin_control.lock().await.set_muted(&node_name, req.muted);
            let reached = pending.apply().await;
            let message =
                if reached { format!("{verb} '{display}'") } else { format!("saved {verb} for '{display}' (device not connected)") };
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
        }
        OutputKind::Airplay2 => {
            state.ap2_control.lock().await.set_muted(&node_name, req.muted);
            (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{verb} '{display}'") }))
        }
        OutputKind::PwSink => {
            if state.agents.lock().await.set_mute(&node_name, req.muted) {
                (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("{verb} '{display}'") }))
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(OutputOpResponse { ok: false, message: format!("no agent connected for '{display}'") }),
                )
            }
        }
    }
}

/// `POST /api/outputs/{node_name}/resync` — ask one output to recover.
///
/// One intent, three mechanisms, and the intent is what a caller has: *this output is
/// being sent audio and is not rendering it properly, do the cheapest thing that fixes
/// that.* For sendspin it is a `stream/clear` (discard buffered audio, re-anchor, one
/// frame); for AirPlay 2 it is a fresh RTSP session with its PTP peer re-armed; a
/// pw-sink host has no such lever, and says so rather than pretending.
pub(crate) async fn resync_output(State(state): State<AppState>, Path(node_name): Path<String>) -> (StatusCode, Json<OutputOpResponse>) {
    let kind = match kind_of(&node_name) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let display = output_label(&state, &node_name);
    match kind {
        OutputKind::Sendspin => {
            // Two statements, never one: holding the control guard across the await would
            // block every other device's commands behind this device's socket.
            let pending = state.sendspin_control.lock().await.clear_stream(&node_name);
            let reached = pending.apply().await;
            if reached {
                tracing::info!("USER ACTION: resync '{node_name}' (sendspin stream/clear — buffers discarded, re-anchoring)");
            }
            let message = if reached {
                format!("cleared '{display}' — it will re-anchor on the next audio")
            } else {
                format!("'{display}' has no live connection, so there is nothing to clear")
            };
            (StatusCode::OK, Json(OutputOpResponse { ok: reached, message }))
        }
        OutputKind::Airplay2 => {
            let queued = state.ap2_control.lock().await.reconnect(&node_name, "you asked for a resync");
            if queued {
                tracing::info!("USER ACTION: resync '{node_name}' (AP2 — releasing its session and building a fresh one)");
            }
            let message = if queued {
                format!("rebuilding '{display}''s session — it should be back in a few seconds")
            } else {
                format!("'{display}' has no live sender, so there is no session to rebuild")
            };
            (StatusCode::OK, Json(OutputOpResponse { ok: queued, message }))
        }
        OutputKind::PwSink => bad_request(format!(
            "'{display}' is a PipeWire host: it has no resync lever of its own — its receiver reloads when its playout \
             delay changes (PUT /api/outputs/{node_name}/delay)"
        )),
    }
}

/// `PUT /api/outputs/{node_name}/delay` — the output's timing knob, in ms.
///
/// **The polarity differs by kind and the cost differs with it**, which is why the
/// response says what happened rather than the path saying which knob it is:
///
/// * **sendspin**: a static *advance* (the device subtracts it from every timestamp, so a
///   larger value plays *earlier*), persisted, and it costs that device a reconnect —
///   tens of seconds of silence — because the firmware reads it at stream start;
/// * **AirPlay 2**: a render delay, applied live on the running session;
/// * **pw-sink**: the receiver's playout buffer, floored at three packet times, pushed to
///   that host's agent live.
pub(crate) async fn set_output_delay(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetDelayRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    let kind = match kind_of(&node_name) {
        Ok(k) => k,
        Err(e) => return e,
    };
    match kind {
        // A sendspin advance has no "default" other than none at all, so an omitted
        // value clears it — which is exactly what 0 means to the device.
        OutputKind::Sendspin => set_sendspin_delay(state, node_name, req.delay_ms.unwrap_or(0)).await,
        OutputKind::Airplay2 => set_ap2_render_delay(state, node_name, req.delay_ms).await,
        OutputKind::PwSink => set_pwsink_jitter(state, node_name, req.delay_ms).await,
    }
}
