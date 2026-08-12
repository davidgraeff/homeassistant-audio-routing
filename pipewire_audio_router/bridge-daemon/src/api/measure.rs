use super::*;

// ---- Microphone-assisted alignment (align/measure.rs, plan §11) ----------
//
// The measurement rides *beside* the by-ear session rather than replacing it: it
// needs that session running (the click track has to be playing on every member
// off one clock) and it drives the same live-mute machinery to solo one member at
// a time. `apply` is deliberately a separate step — the user sees the proposed
// deltas and their confidence before anything is written.

use crate::align::measure::{DelayWriter, MeasureDeps, MeasureStatus, Mode, Refusal, RefusalKind, SendAheadContext, Timing};

/// Writes one member's delay knob **through the existing endpoint handlers**.
///
/// Not through `sync_settings`: those handlers own the persist-then-push order,
/// the per-kind clamping, and — the part that must not be duplicated — the scoped
/// `force_device_reconnect` plus its group-wide send-ahead exception (plan §9.3).
/// A second copy of that reasoning is exactly how a calibration write would end up
/// blacking out a whole room.
pub(crate) struct ApiDelayWriter {
    pub(crate) state: AppState,
}

impl DelayWriter for ApiDelayWriter {
    fn write(
        &self,
        node_name: String,
        kind: crate::align::calibrate::MemberKind,
        delay_ms: u16,
    ) -> crate::align::measure::Fut<'_, Result<String, String>> {
        Box::pin(async move {
            let (status, Json(resp)) = match kind {
                crate::align::calibrate::MemberKind::Sendspin => {
                    set_sendspin_delay_handler(State(self.state.clone()), Json(SetSendspinDelayRequest { node_name, delay_ms })).await
                }
                // pw-sink shares AP2's per-output latency endpoint (its playout
                // delay); the handler clamps to `PWSINK_JITTER_MIN_MS`.
                crate::align::calibrate::MemberKind::Airplay2 | crate::align::calibrate::MemberKind::PwSink => {
                    set_output_latency(
                        State(self.state.clone()),
                        Path(node_name),
                        Json(SetOutputLatencyRequest { latency_ms: Some(delay_ms) }),
                    )
                    .await
                }
            };
            if status.is_success() && resp.ok {
                Ok(resp.message)
            } else {
                Err(resp.message)
            }
        })
    }
}

/// Assemble what a run needs from `AppState`, so `align_measure` never sees it.
pub(crate) fn measure_deps(state: &AppState, mode: Mode, chained: bool, link_to: Vec<String>) -> MeasureDeps {
    // Every knob the two member kinds have, as persisted — the value a revert
    // restores and the value the solve adds to.
    // Snapshotted before the sync-settings lock is taken: two locks in one expression
    // is how a lock-order inversion gets written.
    let adopted = crate::store::outputs::adopted_snapshot(&state.outputs);
    let (current_delays, send_ahead, band_splits) = {
        let ss = state.sync_settings.lock_recover();
        let mut delays: HashMap<String, u16> = ss.sendspin_delays().into_iter().collect();
        delays.extend(ss.ap2_latencies());
        // pw-sink members (alignable since W15) render at their stored playout delay
        // **or the default** — never at 0. So the *effective* value is reported for
        // every adopted pw-sink output, not just the overridden ones: absent would read
        // as "currently 0 ms", and the solve would compute its delta from a delay the
        // host is not using (it cannot even be set below `PWSINK_JITTER_MIN_MS`, plan
        // §1.1.2).
        for name in adopted.iter().filter(|n| n.starts_with(PWSINK_DEV_PREFIX)) {
            delays.insert(name.clone(), ss.pwsink_jitter_effective(name));
        }
        // Inputs for the plan §9.2 high-water warning. `floor_ms` is deliberately
        // just the configured group lead: the codec's own decode floor is per-group
        // and per-device here, and over-reporting the floor can only *suppress* a
        // warning about a mark that would not have moved — never invent one.
        let min_buffer_ms =
            state.sendspin_devices.lock_recover().iter().map(|(node_name, d)| (node_name.clone(), d.min_buffer_ms)).collect();
        let ctx = SendAheadContext { floor_ms: ss.group_lead_ms(), unreported_floor_ms: ss.opus_floor_ms(), min_buffer_ms };
        // Each output's own measured band split (plan §10.2). Only the figure travels:
        // the SNR and the age it was stored with are for the API listing, not for the
        // arithmetic, which subtracts one number per member and nothing else.
        let splits = ss.band_splits().into_iter().map(|(node, s)| (node, s.split_ms)).collect();
        (delays, ctx, splits)
    };
    MeasureDeps {
        mode,
        chained,
        link_to,
        session: std::sync::Arc::new(state.align.clone()),
        mic: std::sync::Arc::new(crate::align::measure::LiveMic),
        writer: std::sync::Arc::new(ApiDelayWriter { state: state.clone() }),
        // The provisional delay line a chain applies its per-step delays to (plan
        // §1.1.1). Process-global, like the mic ingest: there is one set of relays.
        relay: std::sync::Arc::new(crate::align::measure::LiveRelay),
        current_delays,
        send_ahead,
        band_splits,
        // The run's forensic transcript (`align/transcript.rs`), in /data beside the
        // other stores. Disabled — and every run therefore unrecorded but unaffected —
        // until `main` points it at a directory.
        transcript: crate::align::transcript::shared(),
        timing: Timing::real(),
    }
}

/// A refusal is never a 500: every one of them is a state the user can act on.
pub(crate) fn refusal_status(kind: RefusalKind) -> StatusCode {
    match kind {
        // "you have to do something first" / "something is already running".
        // `WalkOutOfOrder` belongs here for the same reason: the request is
        // well-formed, the run is simply not at that step (plan §11 — a refusal is a
        // state the user can act on, never a 500 and never a malformed-input error).
        RefusalKind::NoSession | RefusalKind::MicMissing | RefusalKind::Internal | RefusalKind::WalkOutOfOrder => StatusCode::CONFLICT,
        // Chaining's out-of-order cases are the same shape: the request is well-formed,
        // the chain is simply not at that step, or the position it describes is not one
        // the chain can link (a missing overlap is "do this differently", not a fault).
        RefusalKind::ChainOutOfOrder | RefusalKind::OverlapMissing => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    }
}

pub(crate) fn refused(r: Refusal) -> (StatusCode, Json<Refusal>) {
    tracing::info!("alignment measurement refused: {:?} — {}", r.kind, r.message);
    (refusal_status(r.kind), Json(r))
}

#[derive(Deserialize)]
pub(crate) struct MeasureStartRequest {
    /// `"sweet_spot"` or `"near_field"`. The two make different promises about *where*
    /// the group is aligned, so it is explicit and never inferred:
    ///
    /// - `sweet_spot`: the phone stays put and the daemon measures every member
    ///   itself, twice. Aligns the position it was measured from.
    /// - `near_field`: the daemon parks and waits for the user to walk. One
    ///   `POST /api/align/measure/arrival` per speaker while standing at it, then
    ///   `POST /api/align/measure/close` back at the first one. Aligns the wiring, so
    ///   it holds everywhere.
    #[serde(default = "default_measure_mode")]
    pub(crate) mode: Mode,
    /// Run the multi-position mode as a **chain** (plan §1.1): align a locally-audible
    /// set, reposition, align the next through overlaps.
    ///
    /// `false` (the default) is the single-position case — which is a chain with one step,
    /// and behaves exactly as it did before W12. `true` parks the run in `positioning` and
    /// expects one `POST /api/align/measure/position` per listening spot, then
    /// `POST /api/align/measure/finish`. Ignored for `near_field`, which has its own
    /// acquisition and needs no overlaps at all.
    #[serde(default)]
    pub(crate) chain: bool,
    /// Speakers aligned in an **earlier** run that this one should be made coherent
    /// with (plan §12.1's "link, or keep independent?"). Not implemented — send it and
    /// the run is refused with `mode_unsupported`, which is deliberate: a run that
    /// claimed to link and did not would be worse than one that refuses. Chaining
    /// *within* one run is `chain` above.
    #[serde(default)]
    pub(crate) link_to: Vec<String>,
}

pub(crate) fn default_measure_mode() -> Mode {
    Mode::SweetSpot
}

/// `POST /api/align/equivalence` — the relay-vs-device equivalence experiment (W21).
///
/// The deferred-write scheme (plan §1.1.1) assumes a relay-side delay of *d* and a
/// device-side knob of *d* produce the same shift. This measures it: one speaker, six
/// bracketed readings, three real writes. It reports the **scale** and the **sign** with
/// an explicit resolution bound and applies no correction of its own — a discrepancy is
/// a finding for a human, not something to silently absorb.
#[derive(Deserialize)]
pub(crate) struct EquivalenceStartRequest {
    /// Override the member. Omit — the choice is a property of the transport, and
    /// `plan_equivalence` picks the one where the sign can actually be confirmed.
    #[serde(default)]
    pub(crate) node_name: Option<String>,
}

pub(crate) async fn equivalence_start(
    State(state): State<AppState>,
    Json(req): Json<EquivalenceStartRequest>,
) -> Result<Json<crate::align::measure::EquivalenceStatus>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: start the relay-vs-device equivalence experiment ({:?})", req.node_name);
    let deps = crate::align::measure::EquivalenceDeps {
        // The same deps a measurement run uses — including the provisional delay line the
        // relay arm drives, so there is one handle on it rather than two that could
        // differ. `mode`/`chained`/`link_to` are unused here.
        base: measure_deps(&state, Mode::SweetSpot, false, Vec::new()),
        member: req.node_name,
    };
    crate::align::measure::equivalence().start(deps).await.map(Json).map_err(refused)
}

/// `GET /api/align/equivalence` — the experiment's state, including both arms' numbers,
/// the resolution bound and what it cannot tell you.
pub(crate) async fn equivalence_status() -> Json<crate::align::measure::EquivalenceStatus> {
    Json(crate::align::measure::equivalence().status())
}

/// `DELETE /api/align/equivalence` — abandon. Restore still runs.
pub(crate) async fn equivalence_abandon() -> Json<crate::align::measure::EquivalenceStatus> {
    Json(crate::align::measure::equivalence().abandon())
}

/// `POST /api/align/measure/start` — begin the run.
pub(crate) async fn measure_start(
    State(state): State<AppState>,
    Json(req): Json<MeasureStartRequest>,
) -> Result<Json<MeasureStatus>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: start microphone-assisted alignment measurement ({:?}, chain={})", req.mode, req.chain);
    crate::align::measure::shared().start(measure_deps(&state, req.mode, req.chain, req.link_to)).await.map(Json).map_err(refused)
}

#[derive(Deserialize)]
pub(crate) struct MeasurePositionRequest {
    /// The speakers to align at this position — the ones the user can hear clearly from
    /// where they are standing.
    pub(crate) members: Vec<String>,
    /// Speakers **already** aligned at an earlier position that are still audible here.
    /// These are what tie the two regions together (plan §1.1).
    ///
    /// Empty is correct for the first position and refused for every later one. **Two is
    /// what you want:** the shift a step derives from its overlaps is applied as a common
    /// delay to every speaker aligned so far and anchors everything measured afterwards,
    /// so with one overlap nothing checks it. One is accepted — a user may genuinely have
    /// only one shared speaker — and reported as reduced confidence.
    #[serde(default)]
    pub(crate) overlaps: Vec<String>,
}

/// `POST /api/align/measure/position` — a chain's "these are the speakers I can hear
/// from where I am now" (plan §1.1).
///
/// The call that makes multi-position chaining work, and the reason it exists rather than
/// the daemon working it out: nothing in a capture says which speakers are *locally
/// audible*, so the user says it. The run then measures those speakers plus the overlaps,
/// applies the resulting delays **provisionally** in the relay (nothing is written, no
/// speaker reconnects), and parks for the next position.
///
/// Refused — never 500 — when the run is not chained, is busy, names a speaker it is not
/// holding, re-aligns one that is already aligned, offers an overlap that is not, or omits
/// the overlap a later position needs.
pub(crate) async fn measure_position(Json(req): Json<MeasurePositionRequest>) -> Result<Json<MeasureStatus>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: align position [{}] through overlap(s) [{}]", req.members.join(", "), req.overlaps.join(", "));
    crate::align::measure::shared().position(req.members, req.overlaps).map(Json).map_err(refused)
}

/// `POST /api/align/measure/finish` — "every speaker is aligned at some position".
///
/// Where plan §1.1's **global renormalisation** happens: every step could only ever *add*
/// delay, so the floor ratchets upward across an apartment, and this takes it back out —
/// a common shift, so all relative alignment survives it — before proposing the one write.
/// Refused while any held speaker is still unaligned.
pub(crate) async fn measure_finish() -> Result<Json<MeasureStatus>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: finish the multi-position chain");
    crate::align::measure::shared().finish().map(Json).map_err(refused)
}

#[derive(Deserialize)]
pub(crate) struct MeasureArrivalRequest {
    /// The speaker the user is standing at, by node name.
    pub(crate) node_name: String,
    /// Playback level (0–100) to measure it at. Omit to use the level the session last
    /// applied to this speaker — i.e. whatever the user settled on with
    /// `POST /api/align/audible` while standing there (plan §12.2).
    #[serde(default)]
    pub(crate) level: Option<u8>,
}

/// `POST /api/align/measure/arrival` — near field's "I am at this speaker now".
///
/// The one call that makes near field work, and the reason it exists rather than the
/// daemon working it out: nothing in a mixed capture says *which* speaker the phone is
/// closest to, and per-speaker excitation (which could) is a separate work package. So
/// the user points, and the run solos, levels, gates and measures that member.
///
/// Refused — never 500 — when the run is not a walk, is busy, has already measured
/// this speaker, or has never heard of it.
pub(crate) async fn measure_arrival(Json(req): Json<MeasureArrivalRequest>) -> Result<Json<MeasureStatus>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: near-field arrival at '{}'", req.node_name);
    crate::align::measure::shared().arrival(req.node_name, req.level).map(Json).map_err(refused)
}

/// `POST /api/align/measure/close` — near field's closure reading.
///
/// "I have walked back to the speaker I started at." The difference between its two
/// readings is the mic-vs-audio clock drift accumulated over the whole walk, which is
/// the only thing that makes a one-pass walk trustworthy (plan §5.3). Refused until
/// every member has been visited.
pub(crate) async fn measure_close() -> Result<Json<MeasureStatus>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: close the near-field walk");
    crate::align::measure::shared().close().map(Json).map_err(refused)
}

/// `GET /api/align/measure` — phase, per-member state, SNR, uncertainties and
/// refusal reasons. Poll-only for now; pushing it belongs on the alignment
/// panel's existing subscription (plan §11), which is W6's frontend work.
pub(crate) async fn measure_status() -> Json<MeasureStatus> {
    Json(crate::align::measure::shared().status())
}

/// `GET /api/align/mic/signal` — plan §12's per-channel SNR readout: is the level
/// good enough to measure?
///
/// Separate from `/api/align/mic` on purpose. That endpoint's `peak` is a decaying
/// *broadband* peak, and against an 8 ms burst once per second it is only a
/// "the mic is alive" indicator — it cannot say whether a measurement would
/// succeed. This runs the estimator over the recent capture and grades the weaker
/// tone, which is what actually decides it. Session-independent and side-effect
/// free, so it can be polled while the user is still turning a volume knob.
pub(crate) async fn mic_signal() -> Json<crate::align::measure::SignalCheck> {
    Json(crate::align::measure::signal_check(crate::align::estimator::PATTERN_SECS * 1000.0))
}

/// `POST /api/align/measure/apply` — write the solved delays, then settle and
/// verify. Never automatic: a blocked proposal is refused here with the reason.
///
/// The mode is read off the **run** rather than taken from the request, because how
/// the arrivals were acquired decides how the write can be checked: a near-field
/// proposal is verified by walking again (a residual measured from one spot would be
/// each speaker's distance to that spot, not the write), and a chain's is checked at the
/// last position only. `apply` asserts this itself — it overwrites both `mode` and
/// `chained` from the run's own state — so the two cannot disagree; this just keeps the
/// log honest.
pub(crate) async fn measure_apply(State(state): State<AppState>) -> Result<Json<MeasureStatus>, (StatusCode, Json<Refusal>)> {
    let mode = crate::align::measure::shared().status().mode;
    tracing::info!("USER ACTION: apply measured alignment delays ({mode:?})");
    crate::align::measure::shared().apply(measure_deps(&state, mode, false, Vec::new())).await.map(Json).map_err(refused)
}

/// `POST /api/align/measure/revert` — restore the start-of-session delay
/// snapshot (plan §9.4).
pub(crate) async fn measure_revert(State(state): State<AppState>) -> Result<Json<MeasureStatus>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: revert alignment delays to the pre-measurement snapshot");
    let writer = ApiDelayWriter { state };
    crate::align::measure::shared().revert(&writer).await.map(Json).map_err(refused)
}

/// `DELETE /api/align/measure` — abandon, leaving delays untouched.
pub(crate) async fn measure_abandon() -> Json<MeasureStatus> {
    tracing::info!("USER ACTION: abandon the alignment measurement");
    // Awaited rather than spawned: abandoning also silences the group (the hold and the
    // session stay), and the reply must not come back before the room is quiet — a
    // client that immediately re-selects two speakers by ear would otherwise race it.
    Json(crate::align::measure::shared().abandon().await)
}

// ---- band-split calibration (plan §10.2) ---------------------------------

#[derive(Deserialize)]
pub(crate) struct SplitCalibrateRequest {
    /// The speaker to calibrate. The user must be standing at it, phone in hand.
    pub(crate) node_name: String,
    /// Playback level (0–100). Omit to use the level the session last applied to this
    /// speaker — i.e. whatever `POST /api/align/audible` settled on while the user was
    /// standing there watching `/api/align/mic/signal` (plan §12.2).
    #[serde(default)]
    pub(crate) level: Option<u8>,
}

/// One stored band-split calibration, as the listing reports it.
#[derive(Serialize)]
pub(crate) struct BandSplitEntry {
    pub(crate) node_name: String,
    pub(crate) split_ms: f64,
    pub(crate) std_error_ms: f64,
    pub(crate) peak_snr_db: f64,
    /// Unix seconds. A speaker keeps its node name when it is replaced, so the age of
    /// the claim is part of the claim.
    pub(crate) measured_at: u64,
}

#[derive(Serialize)]
pub(crate) struct BandSplitList {
    pub(crate) calibrations: Vec<BandSplitEntry>,
    /// The cross-band tolerance applied between two **uncalibrated** members, in ms.
    pub(crate) tolerance_ms: f64,
    /// The tolerance applied when *both* members of a pair are calibrated — tighter,
    /// because the legitimate hardware difference has been subtracted.
    pub(crate) calibrated_tolerance_ms: f64,
    /// Largest split that will be stored as a crossover (plan §10.2).
    pub(crate) max_plausible_ms: f64,
}

/// `GET /api/align/measure/split` — the stored per-output band splits and the
/// tolerances they buy.
pub(crate) async fn measure_splits(State(state): State<AppState>) -> Json<BandSplitList> {
    let calibrations = state
        .sync_settings
        .lock_recover()
        .band_splits()
        .into_iter()
        .map(|(node_name, s)| BandSplitEntry {
            node_name,
            split_ms: s.split_ms,
            std_error_ms: s.std_error_ms,
            peak_snr_db: s.peak_snr_db,
            measured_at: s.measured_at,
        })
        .collect();
    Json(BandSplitList {
        calibrations,
        tolerance_ms: crate::align::measure::TRANSITIVITY_TOL_MS,
        calibrated_tolerance_ms: crate::align::measure::CALIBRATED_TRANSITIVITY_TOL_MS,
        max_plausible_ms: crate::align::measure::MAX_PLAUSIBLE_SPLIT_MS,
    })
}

/// `POST /api/align/measure/split` — measure one speaker's own crossover band split
/// at close range and persist it (plan §10.2).
///
/// The answer to a mixed-model group failing the cross-band check for its *hardware*:
/// a crossover split is a fixed property of the speaker, so it is measured once, at
/// arm's length where reflections are negligible, and subtracted from every future
/// run's reading of that speaker. Takes about fifteen seconds — one solo, one gate,
/// one reading — and refuses while a measurement run is live.
pub(crate) async fn measure_split_calibrate(
    State(state): State<AppState>,
    Json(req): Json<SplitCalibrateRequest>,
) -> Result<Json<crate::align::measure::SplitCalibration>, (StatusCode, Json<Refusal>)> {
    tracing::info!("USER ACTION: calibrate '{}''s band split at close range", req.node_name);
    let deps = measure_deps(&state, Mode::NearField, false, Vec::new());
    let cal = crate::align::measure::shared().calibrate_split(deps, req.node_name, req.level).await.map_err(refused)?;
    // Persisted here rather than in `align/measure.rs` for the same reason the delay
    // writes are (plan §9.3): the store belongs to the API layer, and the measurement
    // module never sees `AppState`.
    let stored = crate::routing::sync_settings::BandSplit {
        split_ms: cal.split_ms,
        std_error_ms: cal.std_error_ms,
        peak_snr_db: cal.peak_snr_db,
        measured_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    };
    if let Err(e) = state.sync_settings.lock_recover().set_band_split(&cal.node_name, Some(stored)) {
        return Err(refused(Refusal::new(RefusalKind::Internal, format!("measured {:.2} ms but could not store it: {e}", cal.split_ms))));
    }
    tracing::info!("alignment: stored '{}' band split {:.2} ms", cal.node_name, cal.split_ms);
    Ok(Json(cal))
}

/// `DELETE /api/align/measure/split/{node_name}` — forget one speaker's calibration,
/// which puts it back on the wider uncalibrated tolerance.
pub(crate) async fn measure_split_clear(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
) -> (StatusCode, Json<OutputOpResponse>) {
    tracing::info!("USER ACTION: clear '{node_name}''s band-split calibration");
    match state.sync_settings.lock_recover().set_band_split(&node_name, None) {
        Ok(()) => (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("'{node_name}' is uncalibrated again") })),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("could not clear it: {e}") })),
    }
}

// ---- run transcripts (plan §11) ------------------------------------------

#[derive(Serialize)]
pub(crate) struct MeasureLogList {
    /// Newest first.
    pub(crate) runs: Vec<crate::align::transcript::RunSummary>,
    /// How many runs are kept before the oldest is dropped.
    pub(crate) retained: usize,
    /// Where they live, when transcripts are enabled at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) directory: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct MeasureLogQuery {
    /// A run id from the listing, or `latest` for the most recent one. Omitted ⇒ the
    /// listing.
    #[serde(default)]
    pub(crate) run: Option<String>,
}

/// `GET /api/align/measure/log` — the persisted run transcripts (plan §11).
///
/// Two shapes behind one path, because the client wants exactly one of them:
/// without `?run=` it lists the retained runs newest-first; with `?run=<id>` (or
/// `?run=latest`) it returns that run as **one document**, which is the form that can
/// be read days later without the UI.
pub(crate) async fn measure_log(Query(q): Query<MeasureLogQuery>) -> Result<Response, (StatusCode, Json<Refusal>)> {
    let store = crate::align::transcript::shared();
    let Some(want) = q.run else {
        return Ok(Json(MeasureLogList {
            runs: store.list(),
            retained: crate::align::transcript::MAX_RUNS,
            directory: store.dir().map(|d| d.display().to_string()),
        })
        .into_response());
    };
    let id = match want.as_str() {
        "latest" => store.list().first().map(|r| r.id.clone()),
        other => Some(other.to_string()),
    };
    match id.and_then(|id| store.document(&id)) {
        Some(doc) => Ok(Json(doc).into_response()),
        None => Err(refused(Refusal::new(
            RefusalKind::Internal,
            format!("there is no stored transcript for '{want}' — only the last {} runs are kept", crate::align::transcript::MAX_RUNS),
        ))),
    }
}

#[derive(Deserialize)]
pub(crate) struct SetSendspinDelayRequest {
    /// Virtual device node name, e.g. `sendspin-dev-voice_pe_kitchen`.
    pub(crate) node_name: String,
    /// Static delay in ms (0–5000); `0` clears it.
    pub(crate) delay_ms: u16,
}

pub(crate) async fn get_sendspin_delays(State(state): State<AppState>) -> Json<std::collections::HashMap<String, u16>> {
    Json(state.sendspin_control.lock().await.delays())
}

pub(crate) async fn set_sendspin_delay_handler(
    State(state): State<AppState>,
    Json(req): Json<SetSendspinDelayRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    // Persist first (a calibrated offset must survive restarts), then push live.
    if let Err(e) = state.sync_settings.lock_recover().set_sendspin_delay(&req.node_name, req.delay_ms) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist delay: {e}") }));
    }
    let pending = state.sendspin_control.lock().await.set_delay(&req.node_name, req.delay_ms);
    let reached = pending.apply().await;
    let ms = req.delay_ms.min(5000);

    // Current ESPHome firmware reads the static delay only at stream start, so the
    // live push above doesn't shift the running stream — the device has to reconnect
    // for it to take. Scoped to THIS device's connection on the next reconcile
    // (`force_device_reconnect`): its groupmates' streams are unaffected by its delay,
    // and restarting the group's server for them cost every speaker in the room tens
    // of seconds of silence (docs/old/sendspin-group-churn-plan.md §4.10). The one
    // genuinely group-wide case — a delay large enough to raise the group's send-ahead
    // high-water mark — is picked up by the reconciler's ordinary stream-config check.
    // Skipped entirely when `sendspin_delay_live` is on (firmware that honors a live
    // SetStaticDelay).
    let live = state.settings.lock_recover().sendspin_delay_live();
    let mut reconnecting = false;
    if !live {
        reconnecting = state.groups.lock().await.force_device_reconnect(&req.node_name);
        if reconnecting {
            let _ = state.changes.send(());
        }
    }

    let message = if !reached {
        format!("saved {ms} ms for '{}' (device not connected)", req.node_name)
    } else if reconnecting {
        format!("set '{}' static delay to {ms} ms (reconnecting just this speaker to apply)", req.node_name)
    } else {
        format!("set '{}' static delay to {ms} ms", req.node_name)
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
pub(crate) struct SetOutputNameRequest {
    /// The user's name for this output; `null`/omitted drops the override so the
    /// output goes back to the name discovery reports.
    #[serde(default)]
    pub(crate) name: Option<String>,
}

/// Rename an output (persisted in store/outputs.rs, keyed by node name).
///
/// A device's own mDNS name is often useless in a house (`ap2-dev-living-2`, or
/// four speakers all called "Yamaha"), so the name shown everywhere — Outputs,
/// the routing graph, group chips, the HA `media_player` — is the user's if they
/// set one. The store trims and enforces the minimum length; nothing needs to
/// restart, so this just nudges the change notifier and every matrix subscriber
/// picks the new name up on the next frame.
pub(crate) async fn set_output_name(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetOutputNameRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    tracing::info!("USER ACTION: rename output '{}' to {:?}", node_name, req.name);
    if let Err(e) = state.outputs.lock_recover().set_name(&node_name, req.name.as_deref()) {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e.to_string() }));
    }
    let _ = state.changes.send(());
    let message = match req.name {
        Some(_) => format!("renamed to '{}'", output_label(&state, &node_name)),
        None => format!("'{}' uses its discovered name again", output_label(&state, &node_name)),
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message }))
}

#[derive(Deserialize)]
pub(crate) struct SetOutputLatencyRequest {
    /// Receiver latency in ms; `null`/omitted resets to the type's default.
    pub(crate) latency_ms: Option<u16>,
}

/// Set an output's per-output playout delay (ms), persisted per node name in
/// routing/sync_settings.rs. `latency_ms: null` clears the override, back to the kind's
/// default. Two kinds have such a knob, and both land here because they are the
/// same decision — "this speaker plays too early/late, shift it":
///
/// * **AirPlay 2** — the **render delay**: no PipeWire module to reload, the
///   value is applied *live* to the running stream (the PT=87 anchor offset the
///   streamer reads per packet), and reused as the initial delay on the next
///   (membership/rate) reconnect.
/// * **pw-sink** — the remote receiver's **jitter buffer** (`sess.latency.msec`),
///   pushed to that host's agent, which reloads its `module-rtp-session` with the
///   new value. That reload is a sub-second gap in *that* target's audio only —
///   there is no other lever on this path, so the gap is the price of the knob.
///
/// Only bounds are enforced, and only where the transport demands them (the AP2
/// receiver's negotiated buffer; the module's refusal to run a buffer below its
/// ptime). A *low* value is otherwise allowed straight through even though it
/// risks dropouts: finding that threshold by ear is exactly what this knob is for,
/// and the UI marks the low end red rather than forbidding it.
pub(crate) async fn set_output_latency(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetOutputLatencyRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if node_name.starts_with(PWSINK_DEV_PREFIX) {
        return set_pwsink_jitter(state, node_name, req.latency_ms).await;
    }
    if !node_name.starts_with(AP2_DEV_PREFIX) {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse {
                ok: false,
                message: format!("'{node_name}' has no playout-delay knob (AirPlay 2 and PipeWire hosts do)"),
            }),
        );
    }
    let clamped = req.latency_ms.map(|ms| ms.min(crate::outputs::ap2::server::AP2_RENDER_DELAY_MAX_MS));
    if let Err(e) = state.sync_settings.lock_recover().set_ap2_latency(&node_name, clamped) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: format!("failed to persist latency: {e}") }),
        );
    }
    // Apply live to the streaming session (no-op if not currently streaming —
    // the persisted value then applies on the next connect).
    let effective = clamped.unwrap_or(crate::outputs::ap2::server::AP2_RENDER_DELAY_MS as u16);
    state.ap2_control.lock().await.set_render_delay(&node_name, effective);
    // `latency_ms` is on the routing matrix, and the matrix is only pushed when
    // something says it changed — this used to reach the graph on the next 250 ms
    // meter tick, which no longer carries it (routing/mod.rs `Frame::Meters`).
    let _ = state.changes.send(());
    let latency_label = match clamped {
        Some(ms) => format!("{ms} ms"),
        None => "default".to_string(),
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{node_name}' render delay to {latency_label} (live)") }))
}

/// The pw-sink half of [`set_output_latency`]: persist the playout delay and push
/// it to the host's agent.
///
/// Clamped into [`PWSINK_JITTER_MIN_MS`]..=[`PWSINK_JITTER_MAX_MS`] and rounded up
/// to a multiple of the sender's packet time ([`crate::outputs::pwsink::applemidi::PACKET_MS`]).
/// Both bounds come from the receiving module rather than from taste: it refuses a
/// buffer below `rtp.ptime` outright, and warns when the buffer is not an integer
/// multiple of it.
///
/// A disconnected host is **not** an error: the value is stored and its agent
/// picks it up in the `welcome` of its next connect, which is how every other
/// setting for an absent device behaves here.
pub(crate) async fn set_pwsink_jitter(state: AppState, node_name: String, requested: Option<u16>) -> (StatusCode, Json<OutputOpResponse>) {
    use crate::routing::sync_settings::{PWSINK_JITTER_MAX_MS, PWSINK_JITTER_MIN_MS};

    let packet_ms = crate::outputs::pwsink::applemidi::PACKET_MS as u16;
    let clamped = requested.map(|ms| {
        let bounded = ms.clamp(PWSINK_JITTER_MIN_MS, PWSINK_JITTER_MAX_MS);
        // Round *up* to a whole number of packets: rounding down could re-cross the
        // minimum, and the module wants an exact multiple either way.
        bounded.next_multiple_of(packet_ms).min(PWSINK_JITTER_MAX_MS)
    });
    if let Err(e) = state.sync_settings.lock_recover().set_pwsink_jitter(&node_name, clamped) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: format!("failed to persist the playout delay: {e}") }),
        );
    }
    let effective = state.sync_settings.lock_recover().pwsink_jitter_effective(&node_name);
    // Push it now. `retune` reloads the receiver on that host — a brief gap in its
    // audio, and only its own.
    let pushed = state.agents.lock().await.retune(&node_name, effective);
    let _ = state.changes.send(());
    let label = match clamped {
        Some(ms) => format!("{ms} ms"),
        None => format!("default ({effective} ms)"),
    };
    let how = if pushed { "applied now" } else { "stored; the host is not connected, so it applies when it reconnects" };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{node_name}' playout delay to {label} ({how})") }))
}

#[derive(serde::Deserialize)]
pub(crate) struct SetAp2RateModeRequest {
    /// `"auto"` (negotiate 48 kHz, fall back to 44.1 kHz) or `"fixed_44100"`.
    pub(crate) mode: String,
}

/// Set an AP2 output's wire-rate mode (persisted in routing/sync_settings.rs) and nudge the
/// reconciler so the group re-negotiates + restarts at the new rate. Choosing `auto`
/// also clears any learned 44.1k cap so 48 kHz is re-probed.
/// Per-sendspin-output wire codec (`{"codec": "auto"|"pcm"|"opus"|"flac"}`).
///
/// Rejects a codec that isn't currently selectable — the daemon can't encode it, or
/// the device didn't advertise it — with the same reason the picker shows, instead of
/// storing a choice that would silently fall back to PCM. The stream carries one
/// format for a whole group, so the change takes effect by restarting that group's
/// sendspin server (the codec is part of its restart identity).
#[derive(Deserialize)]
pub(crate) struct SetSendspinCodecRequest {
    pub(crate) codec: String,
}

pub(crate) async fn set_sendspin_codec(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetSendspinCodecRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if !node_name.starts_with(SENDSPIN_DEV_PREFIX) {
        return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("'{node_name}' is not a sendspin output") }));
    }
    let Some(codec) = crate::routing::sync_settings::SendspinCodec::parse(&req.codec) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse { ok: false, message: format!("unknown codec '{}' (use auto, pcm, opus or flac)", req.codec) }),
        );
    };
    // An explicit pick must be usable right now; `auto` always is.
    if let Some(name) = codec.explicit_codec() {
        let device_codecs = state.sendspin_devices.lock_recover().get(&node_name).map(|d| d.supported_codecs.clone()).unwrap_or_default();
        let (_, _, options) = sendspin_codec_info(&node_name, &device_codecs, &state.sync_settings.lock_recover());
        if let Some(opt) = options.iter().find(|o| o.codec == name) {
            if !opt.available {
                let why = opt.reason.clone().unwrap_or_else(|| "not available".into());
                return (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: format!("{name} is not available: {why}") }));
            }
        }
    }
    if let Err(e) = state.sync_settings.lock_recover().set_sendspin_codec(&node_name, codec) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(OutputOpResponse { ok: false, message: format!("failed to persist codec: {e}") }));
    }
    // Codec is part of the sendspin server's restart identity → the group restarts
    // and sends a fresh stream/start with the new format.
    let _ = state.changes.send(());
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("codec for '{node_name}' set to {}", codec.as_str()) }))
}

pub(crate) async fn set_ap2_rate_mode(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(req): Json<SetAp2RateModeRequest>,
) -> (StatusCode, Json<OutputOpResponse>) {
    if !node_name.starts_with(AP2_DEV_PREFIX) {
        return (
            StatusCode::BAD_REQUEST,
            Json(OutputOpResponse { ok: false, message: format!("'{node_name}' is not an AirPlay-2 output") }),
        );
    }
    let mode = match req.mode.as_str() {
        "auto" => crate::routing::sync_settings::Ap2RateMode::Auto,
        "fixed_44100" | "fixed44100" | "44100" => crate::routing::sync_settings::Ap2RateMode::Fixed44100,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(OutputOpResponse { ok: false, message: format!("unknown rate mode '{other}' (use 'auto' or 'fixed_44100')") }),
            );
        }
    };
    if let Err(e) = state.sync_settings.lock_recover().set_ap2_rate_mode(&node_name, mode) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OutputOpResponse { ok: false, message: format!("failed to persist rate mode: {e}") }),
        );
    }
    // Rate is part of the AP2 restart identity → the group re-negotiates + restarts.
    let _ = state.changes.send(());
    let label = match mode {
        crate::routing::sync_settings::Ap2RateMode::Auto => "auto (negotiate 48 kHz)",
        crate::routing::sync_settings::Ap2RateMode::Fixed44100 => "fixed 44.1 kHz",
    };
    (StatusCode::OK, Json(OutputOpResponse { ok: true, message: format!("set '{node_name}' sample rate to {label} (applies shortly)") }))
}

pub(crate) async fn fetch_to_file(url: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let response = reqwest::get(url).await?.error_for_status()?;
    let bytes = response.bytes().await?;
    tokio::fs::write(path, &bytes).await?;
    Ok(())
}
