//! The near-field walk: per-speaker arrivals measured at the speaker, closed by
//! returning to the first one.
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

/// The headline case: the user walks to each speaker, and what comes back is each
/// speaker's **wire** delay — including plan §2.4.2's inversion, since a
/// sendspin-only group is aligned to its *earliest* member.
#[tokio::test(start_paused = true)]
async fn a_near_field_walk_recovers_the_injected_wire_delays() {
    let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, 0.0);
    let levels = rig.levels.clone();
    let m = manager();
    let started = m.start(rig.deps).await.expect("near field must start");
    assert_eq!(started.mode, Mode::NearField);

    // Deliberately not the member order: the walk order is the user's to choose.
    walk_all(&m, &["b", "a", "c"]).await;
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);

    let p = s.proposal.clone().expect("a proposal");
    assert_eq!(p.reference, "a", "sendspin knobs advance, so the earliest arrival is left alone — not the walk's first speaker");
    assert_eq!(proposed(&s, "a").new_delay_ms, 0);
    assert_eq!(proposed(&s, "b").new_delay_ms, 6);
    assert_eq!(proposed(&s, "c").new_delay_ms, 11);
    assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
    assert!(p.blocked.is_none(), "{:?}", p.blocked);
    assert!(s.can_apply);

    let w = s.walk.clone().expect("a walk");
    assert_eq!(w.purpose, WalkPurpose::Measure);
    assert_eq!(w.next, WalkAction::Done);
    assert_eq!(w.measured, vec!["b".to_string(), "a".to_string(), "c".to_string()], "the walk order is reported as walked");
    assert_eq!(w.anchor.as_deref(), Some("b"), "the closure anchor is where the walk started");
    let c = w.closure.clone().expect("a closure report");
    assert!(c.passed, "{c:?}");
    assert_eq!(c.anchor, "b");
    assert!(c.span_s > 20.0, "the closure has to span the whole walk to be worth anything: {} s", c.span_s);

    // One reading per member plus the closure.
    assert_eq!(s.observations.len(), 4);
    assert_eq!(s.observations.iter().filter(|o| o.node_name == "b").count(), 2);

    // §12.2: the level was applied per arrival, from the session's own map.
    assert_eq!(levels.lock_recover().len(), 3, "every speaker was soloed at its own level");

    // The closure replaces repeatability rather than sitting beside it, and the
    // drift *was* fitted, so there is no "no drift fit" warning.
    assert!(p.checks.repeatability.is_none(), "a walk's repeatability check would be an identity, not evidence");
    assert!(p.checks.closure.is_some());
    assert!(!s.warnings.iter().any(|w| w.kind == WarningKind::NoDriftFit));
    assert!(!s.warnings.iter().any(|w| w.kind == WarningKind::LevelLearningSkipped), "near field has no level phase to skip");

    // The premise the user has to keep is stated, on every run.
    let warn = s.warnings.iter().find(|w| w.kind == WarningKind::NearFieldPathAssumed).expect("the path assumption is stated");
    assert!(warn.message.contains("3 ms"), "{}", warn.message);
    assert!(warn.message.contains("nothing in this measurement can tell that apart"), "{}", warn.message);
    // …and so is what the result is *not* coherent with (plan §1.2, W8b).
    assert!(w.scope_note.contains("not related to any"), "{}", w.scope_note);
    assert!(w.level_note.contains("clipping"), "{}", w.level_note);
}

/// The closure measurement doing the job it exists for: a mic clock running fast
/// makes every arrival creep, one pass cannot see it (plan §5.3), and without the
/// closure the creep would be written into the speakers as delay.
#[tokio::test(start_paused = true)]
async fn the_closure_measurement_recovers_the_drift_and_takes_it_back_out() {
    // 0.1 ms of phase per second of capture = 100 ppm, the realistic phone figure
    // §5.4.1 exercises.
    let drift = 0.1;
    let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, drift);
    let m = manager();
    m.start(rig.deps).await.expect("started");
    walk_all(&m, &["a", "b", "c"]).await;
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);

    let c = s.walk.clone().expect("a walk").closure.expect("a closure");
    // The closure error is the drift accumulated over the walk it actually took.
    let expected = drift * c.span_s;
    assert!((c.error_ms - expected).abs() < 0.5, "closure {} ms, expected about {expected:.2} ms over {} s", c.error_ms, c.span_s);
    assert!((c.drift_ppm - 100.0).abs() < 20.0, "implied drift {} ppm", c.drift_ppm);
    assert!(c.passed, "100 ppm is exactly what this is supposed to accept: {c:?}");
    let p = s.proposal.clone().expect("a proposal");
    assert!((p.drift_ppm - 100.0).abs() < 20.0, "the solve's own fit must agree: {} ppm", p.drift_ppm);

    // And the arrivals come back as the *wire* delays, not as wire + creep.
    assert_eq!(proposed(&s, "a").new_delay_ms, 0);
    assert_eq!(proposed(&s, "b").new_delay_ms, 6);
    assert_eq!(proposed(&s, "c").new_delay_ms, 11);

    // The correction is distributed by *when* each member was measured: later in
    // the walk, more correction. Without it 'c' would have been out by more than
    // the knob's own 1 ms granularity, which is what makes the closure worth
    // walking back for rather than assuming the clocks agree.
    let (ca, cb, cc) = (proposed(&s, "a"), proposed(&s, "b"), proposed(&s, "c"));
    assert!(cb.drift_correction_ms > 0.0 && cc.drift_correction_ms > cb.drift_correction_ms, "b {cb:?} c {cc:?}");
    assert!(cc.drift_correction_ms > 1.0, "an uncorrected walk would have been wrong by {} ms", cc.drift_correction_ms);
    assert!(cc.drift_correction_ms < c.error_ms, "no member can be corrected by more than the whole closure error");
    // 'a' is the anchor, so it was read at both ends of the walk and its correction
    // is quoted where the fit places it — between the two visits, not at either.
    assert!(
        cb.drift_correction_ms < ca.drift_correction_ms && ca.drift_correction_ms < cc.drift_correction_ms,
        "the anchor sits between its two visits: b {} a {} c {}",
        cb.drift_correction_ms,
        ca.drift_correction_ms,
        cc.drift_correction_ms
    );
}

/// The refusal that matters: if the two readings of the anchor disagree by more
/// than any clock can explain, something *moved*, and since the correction was
/// applied to every member the whole walk goes — not one reading.
#[tokio::test(start_paused = true)]
async fn an_implausible_closure_refuses_the_whole_walk() {
    let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, 0.0);
    let arrivals = rig.arrivals.clone();
    let m = manager();
    m.start(rig.deps).await.expect("started");

    for name in ["a", "b", "c"] {
        wait_ready(&m, WalkAction::Arrival).await;
        m.arrival(name.to_string(), None).expect("accepted");
    }
    wait_ready(&m, WalkAction::Close).await;
    // Between the last speaker and the walk back, 'a' moves 40 ms — a speaker
    // dragged across a room, or the phone held at arm's length this time.
    arrivals.lock_recover().insert("a".to_string(), 40.0);
    m.close().expect("accepted");

    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Refused, "{}", s.message);
    let r = s.refusal.clone().expect("a refusal");
    assert_eq!(r.kind, RefusalKind::ClosureError);
    assert_eq!(r.member.as_deref(), Some("a"));
    assert!(r.message.contains("whole walk is discarded"), "{}", r.message);
    assert!(r.message.contains("ppm"), "the implied rate is what makes the verdict: {}", r.message);
    assert!(!s.can_apply, "nothing may be written from an unclosed walk");

    // The numbers stay visible next to the refusal (plan §10).
    let c = s.walk.clone().expect("a walk").closure.expect("a closure");
    assert!(!c.passed);
    assert!(c.error_ms.abs() > 30.0, "closure {} ms", c.error_ms);
    assert!(c.drift_ppm.abs() > MAX_CLOSURE_DRIFT_PPM, "{} ppm", c.drift_ppm);
    let p = s.proposal.clone().expect("the proposal is still reported, blocked");
    assert_eq!(p.blocked.map(|b| b.kind), Some(RefusalKind::ClosureError));
    assert!(p.checks.closure.is_some_and(|c| !c.passed));
}

/// Plan §1.2: a walk is **one capture**. A reconnect restarts `align_mic`'s frame
/// counter, so nothing measured before it can be compared with anything after —
/// the walk starts again from its first speaker, and the user is told so instead of
/// two frames being silently mixed.
#[tokio::test(start_paused = true)]
async fn a_mic_reconnect_mid_walk_restarts_the_walk_instead_of_mixing_frames() {
    let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, 0.0);
    let (reconnect_at, mic) = (rig.reconnect_at.clone(), rig.mic.clone());
    let m = manager();
    m.start(rig.deps).await.expect("started");

    for name in ["a", "b"] {
        wait_ready(&m, WalkAction::Arrival).await;
        m.arrival(name.to_string(), None).expect("accepted");
    }
    let s = wait_ready(&m, WalkAction::Arrival).await;
    assert_eq!(s.walk.as_ref().unwrap().measured, vec!["a".to_string(), "b".to_string()]);

    // Schedule the reconnect for 5 s from now: past the 3 s mute guard, so it lands
    // inside the next reading, which is the only place it is detectable.
    reconnect_at.store(mic.frames_now() + u64::from(mic.rate) * 5, Ordering::Relaxed);
    m.arrival("c".to_string(), None).expect("accepted");

    let s =
        wait_for(&m, "the walk to restart", |s| s.walk.as_ref().is_some_and(|w| w.restarts == 1 && w.next == WalkAction::Arrival)).await;
    let w = s.walk.clone().expect("a walk");
    assert!(w.measured.is_empty(), "everything from the old capture must be discarded, not kept: {:?}", w.measured);
    assert_eq!(w.remaining.len(), 3);
    assert_eq!(w.anchor, None, "there is no anchor until the walk starts again");
    assert!(s.observations.is_empty(), "no observation may survive the seam");
    assert!(w.prompt.contains("begins again"), "{}", w.prompt);
    let warn = s.warnings.iter().find(|x| x.kind == WarningKind::MicReconnected).expect("the user has to be told");
    assert!(warn.message.contains("start again from the first speaker"), "{}", warn.message);
    assert!(warn.message.contains("discarded"), "{}", warn.message);

    // And the restarted walk still works: same run, one capture, from scratch.
    walk_all(&m, &["a", "b", "c"]).await;
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
    assert_eq!(proposed(&s, "c").new_delay_ms, 11);
    // Every observation the solve used came from the *same* capture.
    let epochs: Vec<u64> = s.observations.iter().map(|o| o.grid_epoch).collect();
    assert!(epochs.windows(2).all(|w| w[0] == w[1]), "{epochs:?}");
    assert_eq!(epochs.first().copied(), Some(1), "the post-reconnect epoch, not the pre-reconnect one");
}

/// Near field's write is checked by **walking again**, because a residual measured
/// from one spot would be each speaker's distance to that spot rather than the
/// write (see [`WalkPurpose::Verify`]).
#[tokio::test(start_paused = true)]
async fn applying_a_near_field_proposal_verifies_by_walking_again() {
    let rig = Rig::new(&[("early", 0.0), ("late", 7.0)], Mode::NearField, 0.0);
    let m = manager();
    m.start(rig.deps).await.expect("started");
    walk_all(&m, &["early", "late"]).await;
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);

    // Post-write the two arrive together, at the earlier time: what was written is
    // an advance on the late one.
    let after = Rig::new(&[("early", 0.0), ("late", 0.0)], Mode::NearField, 0.0);
    let writer = after.writer.clone();
    m.apply(after.deps).await.expect("apply accepted");

    let s = wait_ready(&m, WalkAction::Arrival).await;
    let w = s.walk.clone().expect("a verification walk");
    assert_eq!(w.purpose, WalkPurpose::Verify, "the check is a walk, not a reading from wherever the phone is");
    assert!(w.prompt.contains("check"), "{}", w.prompt);
    walk_all(&m, &["early", "late"]).await;

    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Done, "{}", s.message);
    assert_eq!(writer.writes.lock_recover().clone(), vec![("late".to_string(), 7)], "only the knob that changed is written");
    let v = s.verification.clone().expect("a verification");
    assert!(v.passed, "residual {} ms", v.residual.worst_ms);
    assert!(v.residual.worst_ms < RESIDUAL_TOL_MS);
}

/// The out-of-order cases, all of which are states the user can act on — so they
/// are refusals with a sentence, never a 500 and never silence (plan §11).
#[tokio::test(start_paused = true)]
async fn the_walk_refuses_calls_that_do_not_match_where_it_is() {
    let rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::NearField, 0.0);
    let m = manager();
    m.start(rig.deps).await.expect("started");
    wait_ready(&m, WalkAction::Arrival).await;

    // A speaker that is not in the group.
    let r = m.arrival("ghost".to_string(), None).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
    assert_eq!(r.member.as_deref(), Some("ghost"));
    // Closing before anything has been walked.
    let r = m.close().expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
    assert!(r.message.contains("nothing to close"), "{}", r.message);

    m.arrival("a".to_string(), None).expect("accepted");
    // A second tap while the reading is in flight.
    let r = m.arrival("b".to_string(), None).expect_err("must refuse");
    assert!(r.message.contains("busy"), "{}", r.message);

    wait_ready(&m, WalkAction::Arrival).await;
    // The same speaker twice — the only legitimate repeat is the closure.
    let r = m.arrival("a".to_string(), None).expect_err("must refuse");
    assert!(r.message.contains("already been measured"), "{}", r.message);
    assert!(r.message.contains("closure reading at 'a'"), "{}", r.message);

    m.arrival("b".to_string(), None).expect("accepted");
    wait_ready(&m, WalkAction::Close).await;
    // An arrival when only the closure is left names where to go.
    let r = m.arrival("b".to_string(), None).expect_err("must refuse");
    assert!(r.message.contains("closure reading at 'a'"), "{}", r.message);

    // Abandoning mid-walk stops the run and writes nothing.
    let after = m.abandon().await;
    assert_eq!(after.phase, Phase::Idle);
    assert!(after.walk.is_none(), "abandoning clears the walk with the run");
    let r = m.close().expect_err("an abandoned walk takes nothing");
    assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
}

/// A multi-position run takes no arrivals, and says why rather than accepting one
/// and doing nothing with it.
#[tokio::test(start_paused = true)]
async fn a_multi_position_run_is_not_a_walk_and_says_so() {
    let m = manager();
    let r = m.arrival("a".to_string(), None).expect_err("idle refuses");
    assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);

    let rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::SweetSpot, 0.0);
    m.start(rig.deps).await.expect("started");
    let r = m.arrival("a".to_string(), None).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
    assert!(r.message.contains("measures every member itself"), "{}", r.message);

    // …and the multi-position path is otherwise exactly what it was: two passes,
    // a real repeatability check, no walk state, and the level-phase seam reported.
    let s = wait_for(&m, "the multi-position run to propose", |s| matches!(s.phase, Phase::Proposed | Phase::Refused)).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
    assert!(s.walk.is_none(), "a multi-position run has no walk");
    assert_eq!(s.observations.len(), 4, "two passes over two members");
    let p = s.proposal.expect("a proposal");
    assert!(p.checks.repeatability.is_some(), "two passes are checkable, and that check must not have been dropped");
    assert!(p.checks.closure.is_none());
    assert!(s.warnings.iter().any(|w| w.kind == WarningKind::LevelLearningSkipped));
    assert!(!s.warnings.iter().any(|w| w.kind == WarningKind::NearFieldPathAssumed));
}

/// The tolerance is a **rate** bound, not a magnitude one, and that is the whole
/// discrimination: a speaker that moved shows up as a large error over a short
/// walk, while clock drift is bounded in ppm however long the walk was.
#[test]
fn the_closure_tolerance_is_a_drift_rate_and_not_a_fixed_number_of_milliseconds() {
    let t = Timing::real();
    // A 40 s walk: 200 ppm buys 8 ms, so 5 ms of creep is credible…
    let ok = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 20.0, 305.0), &t);
    assert!((ok.span_s - 40.0).abs() < 1e-9, "span {}", ok.span_s);
    assert!((ok.tolerance_ms - 8.0).abs() < 1e-9, "tolerance {}", ok.tolerance_ms);
    assert!((ok.drift_ppm - 125.0).abs() < 1e-6, "ppm {}", ok.drift_ppm);
    assert!(ok.passed);
    // …and 12 ms over the same 40 s is not, because no pair of clocks does 300 ppm.
    let bad = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 20.0, 312.0), &t);
    assert!(!bad.passed, "{bad:?}");
    assert!(bad.drift_ppm > MAX_CLOSURE_DRIFT_PPM);
    // The *same* 12 ms over a five-minute walk is credible, and the check says so
    // rather than pretending to a resolution it does not have.
    let slow = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 150.0, 312.0), &t);
    assert!(slow.passed, "{slow:?}");
    assert!(slow.tolerance_ms > 12.0);
    // Sign is kept: which way the clocks ran decides which way every member moves.
    let back = closure_report(&walk_obs("a", 0.0, 305.0), &walk_obs("a", 20.0, 300.0), &t);
    assert!((back.error_ms + 5.0).abs() < 1e-9, "error {}", back.error_ms);
    assert!(back.passed && back.drift_ppm < 0.0);
    // A short walk still gets the floor rather than a sub-millisecond verdict the
    // measurement cannot support.
    let quick = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 2.0, 302.0), &t);
    assert!((quick.tolerance_ms - MIN_CLOSURE_TOL_MS).abs() < 1e-9, "tolerance {}", quick.tolerance_ms);
    assert!(quick.passed);
    // Two readings with no time between them have no baseline at all.
    let flat = closure_report(&walk_obs("a", 5.0, 300.0), &walk_obs("a", 5.0, 300.0), &t);
    assert!(!flat.passed, "a closure with no span cannot establish anything");
    assert!(flat.drift_ppm.is_infinite());
}

/// The claim [`ClosureReport`] rests on: feeding the closure observation to
/// [`fit_drift`] *is* the drift fit — the pooled slope reduces to
/// `error / span`, and each member's correction is proportional to when in the
/// walk it was measured.
#[test]
fn the_closure_observation_is_what_makes_the_drift_fit_possible() {
    // A walk: a at period 0, b at 5, c at 10, then back to a at 15. The mic clock
    // runs 0.4 ms fast per period, so every reading creeps.
    let slope = 0.4;
    let o = [
        walk_obs("a", 0.0, 300.0),
        walk_obs("b", 5.0, 306.0 + 5.0 * slope),
        walk_obs("c", 10.0, 311.0 + 10.0 * slope),
        walk_obs("a", 15.0, 300.0 + 15.0 * slope),
    ];
    // One pass with no closure: nothing has two readings, so there is no slope and
    // the creep would be written into the speakers (plan §5.3).
    let unclosed = fit_drift(&o[..3], 2000.0, |x| x.m.phase_a_ms);
    assert!(!unclosed.fitted, "a single pass has no time baseline");

    let fit = fit_drift(&o, 2000.0, |x| x.m.phase_a_ms);
    assert!(fit.fitted, "the closure is the second reading the fit needs");
    assert!((fit.slope_ms_per_period - slope).abs() < 1e-9, "slope {}", fit.slope_ms_per_period);
    // …and the offsets are the true wire delays, creep removed.
    let base = fit.offsets["a"];
    assert!((fit.offsets["b"] - base - 6.0).abs() < 1e-9, "b {}", fit.offsets["b"] - base);
    assert!((fit.offsets["c"] - base - 11.0).abs() < 1e-9, "c {}", fit.offsets["c"] - base);
    // The closure error and the slope are two views of one number.
    let c = closure_report(&o[0], &o[3], &Timing::real());
    assert!((c.error_ms - slope * 15.0).abs() < 1e-9, "error {}", c.error_ms);
    assert!((fit.slope_ms_per_period - c.error_ms / c.span_periods).abs() < 1e-12);
}

/// Near-field arrivals go through the same §2.4.2 solver as everything else, which
/// is the point — near field changes how arrivals are *acquired*, not what is done
/// with them.
#[test]
fn a_walks_arrivals_feed_the_interval_solver_unchanged() {
    let members = [member("a"), member("b"), member("c")];
    // Walk order a, b, c, closing back on a. No drift.
    let o = [walk_obs("a", 0.0, 300.0), walk_obs("b", 5.0, 312.0), walk_obs("c", 10.0, 305.0), walk_obs("a", 15.0, 300.0)];
    let current: HashMap<String, u16> = HashMap::new();
    let ctx = SendAheadContext::default();
    let closure = closure_report(&o[0], &o[3], &Timing::real());
    assert!(closure.passed);
    let p = solve(&SolveInput {
        timing: Timing::real(),
        members: &members,
        observations: &o,
        current_delays: &current,
        send_ahead: &ctx,
        band_splits: no_band_splits(),
        closure: Some(closure),
    })
    .expect("accepted");
    // A sendspin-only group aligns to its EARLIEST member (§2.4.1's inversion),
    // whatever order the user walked in.
    assert_eq!(p.reference, "a");
    let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
    assert_eq!((by("a").new_delay_ms, by("b").new_delay_ms, by("c").new_delay_ms), (0, 12, 5));
    assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
    assert!(p.blocked.is_none());
    assert!(p.checks.closure.is_some_and(|c| c.passed));
    assert!(p.checks.repeatability.is_none());

    // A mixed group is still refused when the two ranges cannot meet — near field
    // does not change the knobs, only where they were measured from.
    let mixed = [member("s"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
    let o = [walk_obs("s", 0.0, 300.0), walk_obs("ap2-dev-x", 5.0, 320.0), walk_obs("s", 10.0, 300.0)];
    let closure = closure_report(&o[0], &o[2], &Timing::real());
    let r = solve(&SolveInput {
        timing: Timing::real(),
        members: &mixed,
        observations: &o,
        current_delays: &current,
        send_ahead: &ctx,
        band_splits: no_band_splits(),
        closure: Some(closure),
    })
    .expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::KnobRange);
}
