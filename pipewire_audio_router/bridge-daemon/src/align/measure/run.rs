//! Driving a measurement: the orchestration between the session, the microphone and
//! the knobs.
//!
//! [`run_measure`] holds the run's shape — bind the session, learn the playback
//! levels, measure each member over as many passes as the checks need, solve, and
//! report. [`run_apply`] writes what was proposed and then *verifies* by measuring
//! again, because a knob a device silently ignored must not be reported as applied.
//! [`measure_member`] is one member's turn, and the one place that pays a device
//! reconnect.
//!
//! Cancellation and the phase machine live here too: every wait is cancellable
//! ([`sleep_cancellable`]), and [`park`] is what a run owes the room when it stops
//! driving it — the delays it borrowed go back whether it finished, refused, or was
//! abandoned.

use super::*;

// ---------------------------------------------------------------- manager

/// W4 seam (plan §7): the per-member calibration level the measurement uses, plus
/// the exact hand-off `align_levels` expects, built and type-checked here so
/// wiring it up is a loop rather than an interface negotiation.
#[derive(Debug, Clone)]
pub struct LevelSeam {
    /// The level each member is actually measured at, keyed by node name. This is
    /// the *only* field the measurement stage consumes, and W4's
    /// `LevelPlan::levels` drops straight into it.
    pub levels: HashMap<String, u8>,
    /// The member model `align_levels::LevelSolver::with_config` takes.
    // The hand-off contract for W4, exercised by
    // `the_level_seam_matches_what_the_level_solver_actually_takes` but not by the
    // pass-through itself — same convention as `align_mic::MicWindow`'s W3 fields.
    #[allow(dead_code)]
    pub specs: Vec<crate::align::levels::LevelMemberSpec>,
    /// The Stage-1 configuration — see [`learn_levels`] for why it must be
    /// sequential.
    #[allow(dead_code)]
    pub config: crate::align::levels::LevelConfig,
    pub note: String,
    /// False while the level-learning phase is unimplemented.
    pub learned: bool,
}

/// The LEARNING state of plan §8, as a pass-through — and the seam for W4.
///
/// **What W4 needs from here.** `align_levels::LevelSolver` *drives* the
/// excitation rather than being told about it, so wiring it is:
///
/// ```ignore
/// let mut solver = LevelSolver::with_config(seam.specs, seam.config)?;
/// let mut step = solver.begin();
/// loop {
///     // `step.excite` says what to play; `step.levels` what to play it at.
///     apply(&step.levels);                                  // session.solo(…, level)
///     let est = gate_and_measure(&step.excite).await?;      // the same gate as below
///     let obs = RoundObservation::from_estimate(step.excite.clone(), &est, mic.status().peak);
///     match solver.observe(obs) {
///         LevelDecision::Continue(next) => step = next,
///         LevelDecision::Converged(plan) => break plan,     // → LevelSeam::levels
///         LevelDecision::Refused(r) => return Err(r),       // plan §7: refuse, do not best-effort
///     }
/// }
/// ```
///
/// The member model is built below and is deliberately *not* invented: every
/// member's burst lands in the **same** estimator channel under the shared click
/// track (plan §2.2), so `LevelConfig::sequential()` is not a preference but a
/// requirement — the parallel mode rejects duplicate channel labels at
/// construction because per-member SNR would be unattributable. There is a unit
/// test that both of those hold against the real API.
///
/// **Why this is still a seam.** Two capabilities are missing, and neither is
/// mine to add:
///
/// * The solver needs one `Excitation::All` round to measure the *aggregate* peak
///   — the clipping half of the constraint (`align_levels` line "if
///   !self.aggregate_ok"). The session's audibility control makes **at most two**
///   members audible (`calibrate::apply_audibility` solos reference + target), so
///   a group of three or more cannot honour that round. It needs W7's per-device
///   `cal_gate`, or a new all-members mode in `align/calibrate.rs`.
/// * AP2 members' level knob is `LevelKnob::SnapshotRestore`, i.e. it requires a
///   pre-session snapshot restored on teardown. That snapshot belongs next to
///   `calibrate::Session::saved_sendspin` (plan §7 says so explicitly), and
///   getting it wrong leaves a receiver stuck at a calibration volume. Sendspin's
///   knob is `LevelKnob::Live` and needs nothing new, so a sendspin-only group is
///   what W4 can light up first.
///
/// Until then every member is measured at the session's single calibration level
/// and the user is told so, rather than being left to wonder why a far speaker was
/// too quiet to measure.
pub(crate) fn learn_levels(session_level: u8, members: &[SessionMember]) -> LevelSeam {
    use crate::align::levels::{LevelConfig, LevelMemberKind, LevelMemberSpec};
    LevelSeam {
        levels: members.iter().map(|m| (m.node_name.clone(), session_level)).collect(),
        specs: members
            .iter()
            // `snapshot_level` is left `None` on purpose: the pre-session level is
            // owned by the session (`saved_sendspin`), which already restores it on
            // teardown, and inventing a value here would fight that.
            .map(|m| LevelMemberSpec::new(m.node_name.clone(), CLICK_A_LABEL, LevelMemberKind::from(m.kind)))
            .collect(),
        config: LevelConfig::sequential(),
        note: format!(
            "level learning (W4) is not wired up: every member is measured at the session's calibration level \
             ({session_level}). A speaker too quiet for the estimator will be refused with its own SNR rather than \
             turned up."
        ),
        learned: false,
    }
}

/// What the user's "I am here now" calls turn into — the one channel both
/// user-driven acquisitions are parked on.
///
/// The daemon cannot see where the phone is (auto-detecting the nearest speaker would
/// need per-speaker excitation, which is W7 and does not exist), so both a near-field
/// walk and a multi-position chain are driven by these and by nothing else. One channel
/// rather than two, so the "validate under the state lock, then mark busy" rule that
/// makes a double-tap impossible exists once.
#[derive(Debug, Clone)]
pub(crate) enum RunCommand {
    /// Near field: solo this speaker at this level, gate, and take its reading.
    Arrival { node_name: String, level: Option<u8> },
    /// Near field: take the closure reading at the walk's first speaker.
    Close,
    /// Chaining: measure this position — these speakers, linked to the already-aligned
    /// set through these overlaps (plan §1.1).
    Position { members: Vec<String>, overlaps: Vec<String> },
    /// Chaining: every held speaker is aligned; renormalise globally and propose.
    Finish,
}

/// Why a member's measurement stopped short.
pub(crate) enum StepError {
    /// Give up on the whole run.
    Refuse(Refusal),
    /// The grid moved (the capture reconnected): discard the set and start over.
    RestartSet(Refusal),
}

/// Park the state machine on a terminal state, and silence the room.
///
/// Skipped entirely when the run's own cancel flag is set: that flag is what
/// `abandon` raises, and each run owns a fresh one, so a run that was abandoned
/// (or superseded by a newer one) must not write its late verdict over the state
/// the user is now looking at — nor silence a session that state has moved on to.
pub(crate) async fn finish(deps: &MeasureDeps, inner: &Arc<Mutex<Inner>>, cancel: &AtomicBool, outcome: Result<Phase, Refusal>) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    park(inner, cancel, outcome);
    // Every state this lands on is one the user reads rather than listens to
    // (`Proposed`, `Done`, `Refused`), so nothing should still be audible — the hold
    // stays, the click track keeps looping off the one clock `apply` needs, and every
    // member is muted. Awaited here rather than spawned so that a client which
    // abandons or re-selects the moment it sees the terminal phase cannot race us into
    // re-silencing a session it has just taken over.
    if let Err(e) = deps.session.silence().await {
        // Not a refusal: the run's verdict is already recorded, and a session that
        // cannot be silenced is one that has usually gone away by itself.
        tracing::info!("alignment: could not silence the parked group: {e}");
        inner.lock_recover().record(transcript::Event::new("silence_failed", format!("could not silence the parked group: {e}")));
    }
}

/// The state half of [`finish`], separate so the state lock is released before the
/// silencing await. Keeps its own cancel check: it is the one that guards the state.
pub(crate) fn park(inner: &Arc<Mutex<Inner>>, cancel: &AtomicBool, outcome: Result<Phase, Refusal>) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let mut g = inner.lock_recover();
    g.running = false;
    g.gate = None;
    // The walk itself stays visible — its closure numbers are part of the verdict —
    // but nothing more can be posted to it.
    g.cmd_tx = None;
    if let Some(w) = g.walk.as_mut() {
        w.next = WalkAction::Done;
        w.reading = None;
    }
    // Same for the chain: its per-position numbers are the verdict and stay readable.
    if let Some(c) = g.chain.as_mut() {
        c.next = ChainAction::Done;
        c.measuring = None;
    }
    match outcome {
        Ok(phase) => {
            g.phase = phase;
            if phase == Phase::Done {
                g.message = "aligned and verified".to_string();
            }
            g.record(transcript::Event::new("run_finished", g.message.clone()).detail(&serde_json::json!({ "phase": phase })));
        }
        Err(refusal) => {
            g.record(refusal_event(&refusal));
            g.record(
                transcript::Event::new("run_finished", refusal.message.clone()).detail(&serde_json::json!({ "phase": Phase::Refused })),
            );
            // A run that ends without a write owes its provisional delays back: the
            // proposal it was standing behind is gone, so leaving the lines applied would
            // silently misalign normal playback (plan §1.1.1). A *successful* run keeps
            // them — the user is listening to the proposal, and `run_apply` drops them as
            // the real knobs take over.
            g.clear_provisional();
            g.message = refusal.message.clone();
            g.refusal = Some(refusal);
            g.phase = Phase::Refused;
        }
    }
    g.bump();
}

/// A refusal as one transcript line, carrying the whole refusal as its detail (the
/// kind, the member, and the estimator's own verdict where it came from there).
pub(crate) fn refusal_event(r: &Refusal) -> transcript::Event {
    match r.member.as_deref() {
        Some(m) => transcript::Event::for_member("refusal", m, r.message.clone()),
        None => transcript::Event::new("refusal", r.message.clone()),
    }
    .detail(r)
}

pub(crate) fn set_phase(inner: &Arc<Mutex<Inner>>, phase: Phase, message: impl Into<String>) {
    let mut g = inner.lock_recover();
    g.phase = phase;
    g.message = message.into();
    // Every phase transition in the run loop goes through here, so this is the
    // transcript's spine: the sequence of these lines *is* what the run did, and the
    // gaps between their timestamps are where it spent its minutes.
    g.record(transcript::Event::new("phase", g.message.clone()).detail(&serde_json::json!({ "phase": phase })));
    g.bump();
}

/// ARMING → LEARNING → MEASURING → SOLVING → (park in) PROPOSED, or
/// ARMING → WALKING ⇄ MEASURING → SOLVING → PROPOSED for near field.
pub(crate) async fn run_measure(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    cmd_rx: Option<tokio::sync::mpsc::UnboundedReceiver<RunCommand>>,
) -> Result<Phase, Refusal> {
    let session = bind(deps, inner, cancel).await?;
    let rate = deps.mic.status().sample_rate;

    // A chain has its own acquisition *and* its own solve (plan §1.1's global
    // renormalisation), so it owns the path all the way to `Proposed` rather than
    // handing observations back to the single-position solve below — the two positions'
    // arrivals are not comparable, which is the premise of the whole mode.
    if deps.chained && !deps.mode.is_walk() {
        let mut rx =
            cmd_rx.ok_or_else(|| Refusal::new(RefusalKind::Internal, "a chained run was started without a way to accept positions"))?;
        return run_chain(deps, inner, cancel, &mut rx, &session, rate).await;
    }

    // Plan §12.2: "near field breaks the two-phase shape". Its level is only
    // meaningful *at* the speaker and the risk there inverts from too-quiet to
    // clipping, so there is no group-wide learning phase to run or to skip — the level
    // is folded into each arrival, which is also what makes near field one pass
    // instead of two.
    let (observations, closure) = if deps.mode.is_walk() {
        let mut rx =
            cmd_rx.ok_or_else(|| Refusal::new(RefusalKind::Internal, "a near-field run was started without a way to accept arrivals"))?;
        inner.lock_recover().warn(Warning::new(
            WarningKind::NearFieldPathAssumed,
            "near field measures the wiring rather than one listening position, and it does so by assuming the phone is *at* each \
             speaker: hold it within a hand's width of the driver. A phone held a metre away adds about 3 ms of travel to that \
             speaker's reading, and nothing in this measurement can tell that apart from the speaker genuinely being 3 ms late.",
        ));
        let (obs, closure) = run_walk(deps, inner, cancel, &mut rx, WalkPurpose::Measure, &session, rate).await?;
        (obs, Some(closure))
    } else {
        (measure_passes(deps, inner, cancel, &session, rate).await?, None)
    };

    set_phase(inner, Phase::Solving, "solving");
    let proposal = solve(&SolveInput {
        timing: deps.timing,
        members: &session.members,
        observations: &observations,
        current_delays: &deps.current_delays,
        send_ahead: &deps.send_ahead,
        band_splits: &deps.band_splits,
        closure,
    })?;
    let blocked = proposal.blocked.clone();
    {
        let mut g = inner.lock_recover();
        for w in &proposal.warnings {
            g.warn(w.clone());
        }
        // The whole proposal, verbatim: the knobs, the checks with each member's
        // measured and calibrated band split, and the refusal that blocks it if one
        // does. This one line is what makes a run reconstructable afterwards.
        g.record(
            transcript::Event::new(
                "proposal",
                match proposal.blocked.as_ref() {
                    None => format!("proposed: reference '{}', spread {:.2} ms", proposal.reference, proposal.spread_ms),
                    Some(b) => format!("proposal blocked ({:?})", b.kind),
                },
            )
            .detail(&proposal),
        );
        g.proposal = Some(proposal);
        g.bump();
    }
    if let Some(blocked) = blocked {
        return Err(blocked);
    }
    set_phase(inner, Phase::Proposed, "measured; review the proposed delays, then apply them");
    Ok(Phase::Proposed)
}

/// The multi-position measurement stage: the run steps the member list itself,
/// [`MEASURE_PASSES`] times, alternating direction (plan §6.1).
///
/// Unchanged by W8a — near field goes through [`run_walk`] instead — and kept apart
/// from [`run_measure`] only so the two acquisition strategies read as the
/// alternatives they are.
pub(crate) async fn measure_passes(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    session: &SessionSnapshot,
    rate: u32,
) -> Result<Vec<MemberObservation>, Refusal> {
    set_phase(inner, Phase::Learning, "learning playback levels");
    let plan = learn_levels(session.level, &session.members);
    {
        let mut g = inner.lock_recover();
        if !plan.learned {
            g.warn(Warning::new(WarningKind::LevelLearningSkipped, plan.note.clone()));
        }
        for m in &mut g.members {
            m.level = plan.levels.get(&m.node_name).copied().unwrap_or(session.level);
        }
        g.bump();
    }

    let mut epoch = 0u64;
    let mut restarts = 0u32;
    loop {
        {
            let mut g = inner.lock_recover();
            g.observations.clear();
            g.bump();
        }
        match measure_set(deps, inner, cancel, &session.members, &plan.levels, session.level, "", epoch, rate).await {
            Ok(observations) => return Ok(observations),
            Err(StepError::Refuse(r)) => return Err(r),
            Err(StepError::RestartSet(r)) => {
                if restarts >= MAX_SET_RESTARTS {
                    return Err(r);
                }
                restarts += 1;
                epoch += 1;
                let mut g = inner.lock_recover();
                g.record(
                    transcript::Event::new("set_restart", r.message.clone())
                        .detail(&serde_json::json!({ "attempt": restarts, "limit": MAX_SET_RESTARTS, "grid_epoch": epoch, "refusal": r })),
                );
                g.warn(Warning::new(WarningKind::MicReconnected, r.message.clone()));
                for m in &mut g.members {
                    m.passes_done = 0;
                    m.last = None;
                }
                g.bump();
            }
        }
    }
}

/// One set of members, measured [`MEASURE_PASSES`] times with the pass order
/// **alternating** (plan §6.1), inside one grid epoch.
///
/// The unit both stationary acquisitions are built from: a single-position run measures
/// the whole group this way, and a chain measures **one position's** members plus its
/// overlaps this way. A capture reconnect is returned as [`StepError::RestartSet`]
/// rather than retried here, because what a new frame costs differs — a set can simply
/// be retaken, while for a chain it is a position the *user* has to stand at again
/// (plan §1.2).
#[allow(clippy::too_many_arguments)] // one set's worth of context; a struct would only move the list
pub(crate) async fn measure_set(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    members: &[SessionMember],
    levels: &HashMap<String, u8>,
    default_level: u8,
    label: &str,
    epoch: u64,
    rate: u32,
) -> Result<Vec<MemberObservation>, StepError> {
    let mut observations: Vec<MemberObservation> = Vec::new();
    for pass in 0..MEASURE_PASSES {
        // Alternate the order so a mic-clock drift averages out across members
        // instead of accumulating down the list (plan §6.1).
        let mut order: Vec<&SessionMember> = members.iter().collect();
        if pass % 2 == 1 {
            order.reverse();
        }
        for member in order {
            set_phase(inner, Phase::Measuring, format!("{label}measuring '{}' (pass {}/{})", member.node_name, pass + 1, MEASURE_PASSES));
            let level = levels.get(&member.node_name).copied().unwrap_or(default_level);
            let cfg = GateConfig::mute_settle(&deps.timing);
            let o = measure_member(deps, inner, cancel, member, level, cfg, pass, epoch, rate).await?;
            let mut g = inner.lock_recover();
            if let Some(p) = g.members.iter_mut().find(|m| m.node_name == member.node_name) {
                p.passes_done += 1;
                p.last = Some(o.m.clone());
            }
            g.observations.push(o.clone());
            g.bump();
            drop(g);
            observations.push(o);
        }
    }
    Ok(observations)
}

/// Wait for the user's next "I am here", while keeping an eye on everything that can
/// make the wait pointless.
///
/// [`bind`] is polled throughout rather than only when a command arrives: a walk
/// between floors takes minutes, and finding out at the *next* speaker that the
/// session timed out three minutes ago wastes the walk. A microphone that disconnects
/// while parked is fatal for the same reason it is fatal mid-reading — the capture is
/// the timing reference (plan §1.2) — and [`bind`] says so in those words.
///
/// Shared by the walk and the chain: both park waiting for a person to move, and
/// [`Timing::walk_arrival_timeout`] is the same budget either way. `did` names what the
/// user did not do, so the timeout reads correctly for both.
pub(crate) async fn next_command(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RunCommand>,
    did: &str,
) -> Result<RunCommand, Refusal> {
    let deadline = Instant::now() + deps.timing.walk_arrival_timeout;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
        }
        bind(deps, inner, cancel).await?;
        let now = Instant::now();
        if now >= deadline {
            return Err(Refusal::new(
                RefusalKind::WalkTimeout,
                format!(
                    "nobody {did} for {} minutes, so the run gave up rather than holding these speakers indefinitely. \
                     Start the measurement again — the alignment session is still yours.",
                    deps.timing.walk_arrival_timeout.as_secs() / 60
                ),
            ));
        }
        tokio::select! {
            command = rx.recv() => return match command {
                Some(c) => Ok(c),
                // The sender lives in `Inner`, which `abandon` (and a fresh `start`)
                // replaces — so a closed channel is exactly "this run is over".
                None => Err(Refusal::new(RefusalKind::Cancelled, "abandoned")),
            },
            _ = tokio::time::sleep(deps.timing.poll.min(deadline - now)) => {}
        }
    }
}

/// WRITING → SETTLING → VERIFYING → DONE.
pub(crate) async fn run_apply(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    proposal: &Proposal,
    cmd_rx: Option<tokio::sync::mpsc::UnboundedReceiver<RunCommand>>,
) -> Result<Phase, Refusal> {
    let session = bind(deps, inner, cancel).await?;
    let pattern_ms = deps.timing.pattern_ms;
    let rate = deps.mic.status().sample_rate;

    // One reconnect wave: every write is issued back to back, and a member whose
    // delay is unchanged is not written at all — writing it would reconnect a
    // device for nothing (plan §2.3: tens of seconds each).
    set_phase(inner, Phase::Writing, "writing the solved delays");
    let mut wrote = 0usize;
    for m in &proposal.members {
        if m.new_delay_ms == m.current_delay_ms {
            continue;
        }
        match deps.writer.write(m.node_name.clone(), m.kind, m.new_delay_ms).await {
            Ok(msg) => {
                wrote += 1;
                let mut g = inner.lock_recover();
                // The endpoint's own reply, **verbatim**: it is what says whether the
                // device was reconnected to pick the value up, and that is the sentence
                // anyone reconstructing a run needs (plan §2.3).
                g.record(transcript::Event::for_member("write", &m.node_name, msg.clone()).detail(&serde_json::json!({
                    "kind": m.kind,
                    "from_ms": m.current_delay_ms,
                    "to_ms": m.new_delay_ms,
                    "reply": msg,
                })));
                g.mark_written(&m.node_name);
                drop(g);
                tracing::info!("alignment write: {msg}");
            }
            Err(e) => {
                inner
                    .lock_recover()
                    .record(transcript::Event::for_member("write_failed", &m.node_name, e.clone()).detail(
                        &serde_json::json!({ "kind": m.kind, "from_ms": m.current_delay_ms, "to_ms": m.new_delay_ms, "error": e }),
                    ));
                return Err(Refusal::for_member(
                    RefusalKind::WriteFailed,
                    &m.node_name,
                    format!("writing '{}''s delay failed: {e}. Use revert to restore the delays from before this session.", m.node_name),
                ));
            }
        }
    }
    // The real knobs now carry what the delay lines were standing in for, so the lines
    // have to go — otherwise every chained member would be delayed twice (plan §1.1.1:
    // the provisional delay is a *stand-in* for the knob, not an addition to it). Done
    // immediately after the write wave rather than before it, so nothing is briefly
    // un-delayed while the writes are being issued; the reconnect-length gate below
    // absorbs the transient either way.
    let cleared = inner.lock_recover().clear_provisional();
    if cleared > 0 {
        tracing::info!("alignment chain: dropped {cleared} provisional delay line(s); the written knobs carry them now");
    }

    if wrote == 0 {
        set_phase(inner, Phase::Verifying, "nothing to write — the group was already aligned; verifying");
    } else {
        set_phase(inner, Phase::Settling, format!("settling: {wrote} device(s) reconnect to pick their new delay up"));
        sleep_cancellable(deps.timing.settle_grace, deps.timing.poll, cancel).await?;
    }

    set_phase(inner, Phase::Verifying, "verifying");
    // A chain can only be checked where the phone is, which is the **last** position.
    // Its own set — the position's speakers and its overlaps, which that step's Δ put in
    // step with them — is the one set that is genuinely aligned here; every other
    // position was aligned somewhere else, and measuring it from here would report a
    // correct chain as broken for exactly the reason §10.4 gives for a walk.
    let chain_scope: Option<(Vec<String>, usize)> = inner.lock_recover().chain.as_ref().and_then(|c| {
        c.steps.last().map(|last| {
            let mut set = last.members.clone();
            set.extend(last.overlaps.iter().map(|o| o.node_name.clone()));
            (set, c.steps.len())
        })
    });
    let observations = if deps.mode.is_walk() {
        // A near-field write can only be checked from where it was measured — at the
        // speakers. See [`WalkPurpose::Verify`]: a stationary residual would measure
        // the phone's distance to each speaker and fail every time.
        let mut rx = cmd_rx
            .ok_or_else(|| Refusal::new(RefusalKind::Internal, "a near-field verification was started without a way to accept arrivals"))?;
        let (observations, closure) = run_walk(deps, inner, cancel, &mut rx, WalkPurpose::Verify, &session, rate).await?;
        if !closure.passed {
            return Err(Refusal::for_member(
                RefusalKind::ClosureError,
                &closure.anchor,
                format!(
                    "the check walk did not close: '{}' read {:.2} ms differently at the end than at the start (limit {:.1} ms over \
                     {:.0} s). The written delays may well be right, but this walk cannot confirm them — nothing was changed by \
                     the check itself, so revert if you want the previous delays back, or walk the check again.",
                    closure.anchor, closure.error_ms, closure.tolerance_ms, closure.span_s
                ),
            ));
        }
        observations
    } else {
        let mut observations = Vec::new();
        for pass in 0..VERIFY_PASSES {
            let mut order: Vec<&SessionMember> = session.members.iter().collect();
            if let Some((set, _)) = chain_scope.as_ref() {
                order.retain(|m| set.contains(&m.node_name));
            }
            if pass % 2 == 1 {
                order.reverse();
            }
            for member in order {
                set_phase(inner, Phase::Verifying, format!("verifying '{}'", member.node_name));
                let level =
                    inner.lock_recover().members.iter().find(|m| m.node_name == member.node_name).map(|m| m.level).unwrap_or(session.level);
                // The reconnect-length gate: a written device is silent for tens of
                // seconds before it renders again (plan §2.3).
                let cfg = GateConfig::reconnect(&deps.timing);
                match measure_member(deps, inner, cancel, member, level, cfg, pass, u64::MAX, rate).await {
                    Ok(o) => observations.push(o),
                    Err(StepError::Refuse(r)) | Err(StepError::RestartSet(r)) => return Err(r),
                }
            }
        }
        observations
    };

    // The reference has to be inside the set that was actually re-measured, so a chain
    // measures its residual against whichever of the last position's speakers ended with
    // the smallest knob — the same "everyone was moved towards this one" rule the
    // single-position solve uses, restricted to what the phone can hear from here.
    let reference = match chain_scope.as_ref() {
        None => proposal.reference.clone(),
        Some((set, _)) => proposal
            .members
            .iter()
            .filter(|m| set.contains(&m.node_name))
            .min_by(|a, b| a.new_delay_ms.cmp(&b.new_delay_ms).then_with(|| a.node_name.cmp(&b.node_name)))
            .map(|m| m.node_name.clone())
            .unwrap_or_else(|| proposal.reference.clone()),
    };
    let residual = residual(&observations, &reference, pattern_ms, RESIDUAL_TOL_MS);
    let trans = transitivity(&observations, &deps.timing, TRANSITIVITY_TOL_MS, &deps.band_splits);
    let passed = residual.passed && trans.passed;
    let verification = Verification {
        residual: residual.clone(),
        transitivity: trans.clone(),
        merged_peak: MergedPeakCheck::seam(),
        observations,
        passed,
        scope_note: chain_scope.as_ref().map(|(set, positions)| {
            format!(
                "this checked the last of {positions} position(s) only — the {} speaker(s) that position aligned, measured against '{}' \
                 from where the phone is now. The earlier positions were aligned at *their* spots, so a reading of them from here would \
                 be their distance to this spot rather than the write, and it would fail however correct the chain is. Re-checking them \
                 means walking the chain again.",
                set.len(),
                reference
            )
        }),
    };
    {
        let mut g = inner.lock_recover();
        g.record(
            transcript::Event::new(
                "verification",
                format!(
                    "residual {:.2} ms (limit {:.1}), cross-band {:.2} ms (limit {:.1}): {}",
                    residual.worst_ms,
                    residual.tolerance_ms,
                    trans.worst_ms,
                    trans.tolerance_ms,
                    match passed {
                        true => "passed",
                        false => "failed",
                    }
                ),
            )
            .detail(&verification),
        );
        g.verification = Some(verification);
        g.bump();
    }
    if !trans.passed {
        let (a, b) = trans.worst_pair.clone().unwrap_or_default();
        return Err(Refusal::new(
            RefusalKind::Transitivity,
            format!(
                "after writing, the two test tones disagree by {:.2} ms about '{a}' vs '{b}' (limit {:.1} ms), so the \
                 delays that were written cannot be trusted — revert and measure again. {}",
                trans.worst_ms, trans.tolerance_ms, trans.advice
            ),
        ));
    }
    if !residual.passed {
        let who = residual.worst_member.clone().unwrap_or_default();
        return Err(Refusal::for_member(
            RefusalKind::ResidualTooLarge,
            &who,
            format!(
                "after writing and settling, '{who}' still arrives {:.2} ms away from the reference (limit {:.1} ms). \
                 The delay may not have taken effect yet, or the first measurement was wrong — revert and measure again.",
                residual.worst_ms, residual.tolerance_ms
            ),
        ));
    }
    Ok(Phase::Done)
}

/// The session/mic binding (plan §11: the measurement needs both).
pub(crate) async fn bind(deps: &MeasureDeps, inner: &Arc<Mutex<Inner>>, cancel: &AtomicBool) -> Result<SessionSnapshot, Refusal> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
    }
    let session = deps.session.snapshot().await;
    if !session.active {
        return Err(Refusal::new(
            RefusalKind::SessionLost,
            "the alignment session stopped, so nothing is playing to measure — the microphone is still connected. \
             Start the session again.",
        ));
    }
    let expected = inner.lock_recover().sources.clone();
    if !expected.is_empty() && !same_set(&expected, &session.sources) {
        return Err(Refusal::new(
            RefusalKind::SessionChanged,
            format!("the alignment session moved to a different group ({:?}), so this measurement no longer applies", session.sources),
        ));
    }
    let mic = deps.mic.status();
    if !mic.connected {
        return Err(Refusal::new(
            RefusalKind::MicLost,
            "the microphone capture disconnected — the alignment session is still running, but there is nothing to \
             measure with. Reopen the capture on the phone.",
        ));
    }
    Ok(session)
}

pub(crate) fn same_set(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

pub(crate) async fn sleep_cancellable(d: Duration, poll: Duration, cancel: &AtomicBool) -> Result<(), Refusal> {
    let deadline = Instant::now() + d;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
        }
        tokio::time::sleep(poll.min(deadline - now)).await;
    }
}

/// Solo one member, pass the gate, and take the estimate over the window the gate
/// approved.
#[allow(clippy::too_many_arguments)] // one measurement's worth of context; a struct would only move the list
pub(crate) async fn measure_member(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    member: &SessionMember,
    level: u8,
    cfg: GateConfig,
    pass: usize,
    grid_epoch: u64,
    rate: u32,
) -> Result<MemberObservation, StepError> {
    let name = member.node_name.as_str();
    let timing = &deps.timing;
    deps.session
        .solo(name.to_string(), level)
        .await
        .map_err(|e| StepError::Refuse(Refusal::for_member(RefusalKind::SessionLost, name, format!("could not solo '{name}': {e}"))))?;

    let mut est = Estimator::new(estimator_config(rate, timing.pattern_ms))
        .map_err(|e| StepError::Refuse(Refusal::for_member(RefusalKind::Internal, name, e)))?;
    let mic = deps.mic.clone();
    let mut feeder = Feeder::new(mic.as_ref(), rate, timing.pattern_ms);
    let mut gate = Gate::new(cfg).for_member(name);

    // Plan §6.1's guard: a mute lands somewhere inside the stream's send-ahead
    // window, so nothing captured until it has surely landed can be judged.
    sleep_cancellable(timing.mute_guard, timing.poll, cancel).await.map_err(StepError::Refuse)?;
    feeder.arm();
    let started = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StepError::Refuse(Refusal::new(RefusalKind::Cancelled, "abandoned")));
        }
        bind(deps, inner, cancel).await.map_err(StepError::Refuse)?;

        let pulled = feeder.pull(&mut est);
        let elapsed = started.elapsed();

        // Exclusivity is real but deliberately not absolute (plan §12.3): a barge-in
        // announcement and a voice-duck hold both outrank the alignment hold, because
        // nobody wants an alarm suppressed by a calibration. What must not happen is
        // losing *why* — so the cause is carried into the gate, and an entry for
        // another member is kept as a warning rather than dropped on the floor.
        let mut interference: Option<String> = None;
        for i in deps.session.take_interference().await {
            {
                let mut g = inner.lock_recover();
                // Recorded per occurrence, unlike the warning, which de-duplicates by
                // kind: three doorbells during one run is a different story from one.
                g.record(transcript::Event::for_member("interference", i.member.clone(), i.reason.clone()).detail(&i));
                g.warn(Warning::new(WarningKind::Interference, i.reason.clone()));
            }
            if i.member == name {
                interference = Some(i.reason);
            }
        }
        // A period boundary is the gate's tick. A disturbance is judged
        // immediately, because waiting a whole period to notice a disconnect would
        // make every failure two seconds slower to explain.
        let ticks = if pulled.disconnected || pulled.reconnected || pulled.gap || pulled.clipped { 1 } else { pulled.new_periods };
        for _ in 0..ticks {
            // One aggregation per tick: `estimate()` is a read, but it medians and
            // line-fits every retained period, so it is not free enough to call
            // twice.
            let e = est.estimate();
            let sample = GateSample {
                elapsed,
                connected: !pulled.disconnected,
                reconnected: pulled.reconnected,
                gap: pulled.gap,
                clipped: pulled.clipped,
                peak: feeder.last_peak(),
                periods_used: e.channels.iter().map(|c| c.periods_used).min().unwrap_or(0),
                quality: e.quality.clone(),
                interference: interference.take(),
            };
            let step = gate.observe(&sample);
            {
                let mut g = inner.lock_recover();
                g.gate = Some(step.progress.clone());
                // `note` bumps the change notifier, so the gate's progress reaches
                // `measure_ws` in the same push as the note it belongs to.
                g.note(name, Some(step.progress.message.clone()));
            }
            if gate.aec_suspected() {
                inner.lock_recover().warn(Warning::new(
                    WarningKind::AecSuspected,
                    "the tone's level decayed monotonically during a measurement, which is the behavioural signature of \
                     echo cancellation converging (plan §4.2). Treat every number here with suspicion until it is off.",
                ));
            }
            if let Some(failed) = step.failed {
                let mut r = failed;
                r.member = Some(name.to_string());
                inner.lock_recover().record(
                    transcript::Event::for_member("gate_failed", name, r.message.clone())
                        .detail(&serde_json::json!({ "refusal": r, "gate": step.progress })),
                );
                return Err(if r.kind == RefusalKind::MicReconnected { StepError::RestartSet(r) } else { StepError::Refuse(r) });
            }
            if step.restart {
                // The reason is the whole value of recording a restart: "acquiring" for
                // the tenth time and "the tone stopped again" are the same delay to the
                // user and completely different diagnoses.
                inner
                    .lock_recover()
                    .record(transcript::Event::for_member("gate_restart", name, step.progress.message.clone()).detail(&step.progress));
                if pulled.reconnected {
                    return Err(StepError::RestartSet(Refusal::for_member(
                        RefusalKind::MicReconnected,
                        name,
                        "the microphone capture reconnected, which restarts the timing reference every earlier \
                         measurement was on — measuring the group again from the start",
                    )));
                }
                est.reset();
                feeder.arm();
                break;
            }
            if step.locked {
                let (Some(a), Some(b)) = (e.channel(CLICK_A_LABEL), e.channel(CLICK_B_LABEL)) else {
                    return Err(StepError::Refuse(Refusal::for_member(
                        RefusalKind::Internal,
                        name,
                        "the estimator returned no A/B channels",
                    )));
                };
                let o = MemberObservation {
                    node_name: name.to_string(),
                    pass,
                    grid_epoch,
                    period_centre: feeder.period_centre(),
                    m: MemberMeasurement {
                        phase_a_ms: a.phase_ms,
                        phase_b_ms: b.phase_ms,
                        std_error_ms: a.std_error_ms.max(b.std_error_ms),
                        peak_snr_db: a.peak_snr_db.min(b.peak_snr_db),
                        second_peak_ratio: a.second_peak_ratio.min(b.second_peak_ratio),
                        drift_ppm: a.drift_ppm,
                        periods_used: a.periods_used.min(b.periods_used),
                    },
                };
                let split_ms = member_split_ms(&o.m, timing);
                let calibrated = deps.band_splits.get(name).copied();
                let mut g = inner.lock_recover();
                g.record(
                    transcript::Event::for_member(
                        "measurement",
                        name,
                        format!(
                            "accepted pass {} of '{name}': {:.2} ms at 3 kHz, SNR {:.1} dB, cross-band split {split_ms:.2} ms{}",
                            pass + 1,
                            o.m.phase_a_ms,
                            o.m.peak_snr_db,
                            // Wherever a calibration is *applied*, it is said out loud —
                            // a wrong calibration must be visible in the record rather
                            // than silently correcting the numbers beside it.
                            match calibrated {
                                Some(c) => format!(" (calibrated {c:.2} ms, residual {:.2} ms)", split_ms - c),
                                None => String::new(),
                            }
                        ),
                    )
                    .detail(&serde_json::json!({
                        "observation": o,
                        "gate": step.progress,
                        "level": level,
                        "split_ms": split_ms,
                        "band_split_calibration_ms": calibrated,
                        "residual_split_ms": calibrated.map(|c| split_ms - c),
                    })),
                );
                g.note(name, None);
                drop(g);
                return Ok(o);
            }
        }
        tokio::time::sleep(timing.poll).await;
    }
}

/// The estimator's configuration for a given capture rate and pattern: the
/// existing click track's two frequency channels (plan §2.2 — every member emits
/// both), on whatever pattern the anchor is actually looping.
pub(crate) fn estimator_config(rate: u32, pattern_ms: f64) -> EstimatorConfig {
    EstimatorConfig { pattern_secs: pattern_ms / 1000.0, ..EstimatorConfig::click_track(rate) }
}
