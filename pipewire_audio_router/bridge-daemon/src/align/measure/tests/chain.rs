//! Multi-position chaining: one run per listening spot, joined through the members
//! they share.
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

/// Linking a walk to a previously aligned set is plan §1.2's cross-session case and
/// it is **not** implemented. Refusing is the honest answer: a run that claimed to
/// link and did not would leave the user believing in coherence that is not there.
#[tokio::test(start_paused = true)]
async fn linking_to_an_earlier_run_is_refused_rather_than_ignored() {
    let mut rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::NearField, 0.0);
    rig.deps.link_to = vec!["sendspin-dev-hall".to_string()];
    let m = manager();
    let r = m.start(rig.deps).await.expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::ModeUnsupported);
    assert!(r.message.contains("sendspin-dev-hall"), "{}", r.message);
    assert!(r.message.contains("in one run"), "{}", r.message);
    assert_eq!(m.status().phase, Phase::Idle, "a refused start must leave no run behind");
}

/// The headline case: two rooms, one microphone, and a result that is right in *both*
/// places — recovered from arrivals injected separately per position.
#[tokio::test(start_paused = true)]
async fn a_two_step_chain_recovers_the_arrivals_injected_at_both_positions() {
    let rig = chain_rig(&["a", "b", "c", "d"]);
    let (writer, relay, arrivals) = (rig.writer.clone(), rig.relay.clone(), rig.arrivals.clone());
    let m = manager();
    m.start(rig.deps).await.expect("a chained run must start");

    // Position 1, in the living room: 'b' arrives 5 ms after 'a', so 'a' is delayed to
    // meet it — the latest arrival is the target (plan §1.1).
    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
    let s = wait_chain(&m).await;
    assert_eq!(applied(&s, "a"), 5, "the earlier speaker is delayed to the later one");
    assert_eq!(applied(&s, "b"), 0);
    assert_eq!(chain_of(&s).steps[0].confidence, OverlapConfidence::Origin);
    assert_eq!(chain_of(&s).remaining, vec!["c".to_string(), "d".to_string()]);

    // Position 2, in the kitchen: 'a' and 'b' are still audible and are the overlaps.
    // With the 5 ms 'a' is carrying they arrive together here at 25 ms, and the new
    // speakers arrive earlier, so nothing already aligned has to move.
    at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("c", 10.0), ("d", 13.0)], &["c", "d"], &["a", "b"]).await;
    let s = wait_chain(&m).await;
    let chain = chain_of(&s);
    assert_eq!(chain.next, ChainAction::Finish, "every speaker is aligned somewhere: {}", chain.prompt);
    assert_eq!(chain.steps.len(), 2);
    let two = &chain.steps[1];
    assert_eq!(two.confidence, OverlapConfidence::Checked);
    assert!(two.disagreement_ms.is_some_and(|d| d < 1.0), "the two overlaps agree here: {two:?}");
    assert!(two.delta_ms.abs() < 0.6, "nothing new arrives later than the aligned set, so Δ is 0: {}", two.delta_ms);
    assert_eq!(applied(&s, "c"), 15);
    assert_eq!(applied(&s, "d"), 12);
    assert_eq!(applied(&s, "a"), 5, "an aligned member only moves when Δ says so");
    assert_eq!(chain.floor_ms, 0.0, "the floor has not ratcheted yet — 'b' is still at zero");

    // Finishing renormalises globally and proposes the one write.
    m.finish().expect("finish accepted");
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
    assert!(writer.writes.lock_recover().is_empty(), "a chain writes nothing until `apply` (plan §1.1.1)");
    assert!(relay.applied_ms("c") > 0.0, "the proposal the user is listening to is the delay line");

    // Both positions' geometry is reproduced by the knobs: an advance of `x` plays
    // that much earlier, so a member that needed 5 ms *more* delay than another ends
    // with 5 ms *less* advance.
    assert_eq!(proposed(&s, "a").new_delay_ms, 10);
    assert_eq!(proposed(&s, "b").new_delay_ms, 15);
    assert_eq!(proposed(&s, "c").new_delay_ms, 0);
    assert_eq!(proposed(&s, "d").new_delay_ms, 3);
    let p = s.proposal.clone().expect("a proposal");
    assert_eq!(p.reference, "c", "the member that needed the most delay ends at knob zero (§2.4.2)");
    assert!(p.members.iter().all(|x| x.polarity == KnobPolarity::Advance));
    assert!(p.blocked.is_none(), "{:?}", p.blocked);
    assert!(s.can_apply);
    // Position 1's 5 ms between 'a' and 'b' survives, and so does position 2's 3 ms.
    assert_eq!(proposed(&s, "b").new_delay_ms - proposed(&s, "a").new_delay_ms, 5);
    assert_eq!(proposed(&s, "d").new_delay_ms - proposed(&s, "c").new_delay_ms, 3);

    // What it does *not* promise is stated, on every chained run.
    let scope = s.warnings.iter().find(|w| w.kind == WarningKind::ChainScope).expect("the doorway caveat is not optional");
    assert!(scope.message.contains("doorway"), "{}", scope.message);
    assert!(scope.message.contains("indirectly"), "{}", scope.message);
    assert!(chain_of(&s).error.bounded, "both joints were checked by two overlaps");
}

/// **The trick the whole feature rests on** (plan §1.1): when a new speaker arrives
/// later than the already-aligned set, the set gains Δ — and Δ goes to *every* member
/// of it, not just to the overlap that was measured, because a common delay preserves
/// that set's internal alignment.
#[tokio::test(start_paused = true)]
async fn a_delta_moves_the_whole_aligned_set_and_not_just_the_overlap() {
    // 'e' is aligned at position 1 and is **not** audible at position 2, so it is
    // never measured there: it can only move if Δ was propagated to the whole set.
    let rig = chain_rig(&["a", "b", "e", "c", "d"]);
    let arrivals = rig.arrivals.clone();
    let m = manager();
    m.start(rig.deps).await.expect("started");

    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0), ("e", 2.0)], &["a", "b", "e"], &[]).await;
    let s = wait_chain(&m).await;
    assert_eq!((applied(&s, "a"), applied(&s, "b"), applied(&s, "e")), (5, 0, 3));

    // At position 2 the overlaps arrive together at 15 ms while the new speakers
    // arrive at 30 and 25: the target is 30, so the aligned set is 15 ms early.
    at_position(&m, &arrivals, &[("a", 10.0), ("b", 15.0), ("c", 30.0), ("d", 25.0)], &["c", "d"], &["a", "b"]).await;
    let s = wait_chain(&m).await;
    let two = &chain_of(&s).steps[1];
    assert!((two.delta_ms - 15.0).abs() < 0.6, "Δ should be 15 ms: {}", two.delta_ms);
    assert_eq!(applied(&s, "a"), 20, "the measured overlap gained Δ");
    assert_eq!(applied(&s, "b"), 15, "so did the other overlap");
    assert_eq!(applied(&s, "e"), 18, "and so did the member that was not even audible here — that is the trick");
    assert_eq!(applied(&s, "c"), 0);
    assert_eq!(applied(&s, "d"), 5);
    // Position 1's internal alignment is untouched by the common shift.
    assert_eq!(applied(&s, "a") - applied(&s, "b"), 5);
    assert_eq!(applied(&s, "e") - applied(&s, "b"), 3);
    assert!((chain_of(&s).floor_ms - 0.0).abs() < 1e-9, "'c' is the new floor at 0 ms");
    assert!(two.note.contains("common delay"), "{}", two.note);

    // And the floor that ratcheted is taken back out by the global renormalisation, so
    // nothing is left carrying delay for nothing (plan §1.1).
    m.finish().expect("finish accepted");
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
    let p = s.proposal.clone().expect("a proposal");
    assert_eq!(p.members.iter().map(|x| x.new_delay_ms).min(), Some(0), "the renormalisation puts the smallest knob back at zero");
    assert_eq!(proposed(&s, "a").new_delay_ms, 0);
    assert_eq!(proposed(&s, "b").new_delay_ms, 5);
    assert_eq!(proposed(&s, "e").new_delay_ms, 2);
    assert_eq!(proposed(&s, "c").new_delay_ms, 20);
    assert_eq!(proposed(&s, "d").new_delay_ms, 15);
    assert!((p.spread_ms - 20.0).abs() < 0.6, "the ratchet the renormalisation removed: {} ms", p.spread_ms);
}

/// Two overlaps that disagree by more than plausible geometry **refuse the step**
/// (plan §1.1) — and the chain survives it, because the positions already aligned are
/// still good and still carry their delays.
#[tokio::test(start_paused = true)]
async fn overlaps_that_disagree_refuse_the_step_and_leave_the_chain_alive() {
    let rig = chain_rig(&["a", "b", "c"]);
    let arrivals = rig.arrivals.clone();
    let m = manager();
    m.start(rig.deps).await.expect("started");
    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
    wait_chain(&m).await;

    // 'a' carries 5 ms, so the overlaps read 15 and 25 here: 10 ms apart, which is
    // more than one room's geometry explains.
    at_position(&m, &arrivals, &[("a", 10.0), ("b", 25.0), ("c", 8.0)], &["c"], &["a", "b"]).await;
    let s = wait_for(&m, "the step to be refused", |s| s.chain.as_ref().is_some_and(|c| c.refusal.is_some())).await;
    let chain = chain_of(&s);
    let r = chain.refusal.clone().expect("a step refusal");
    assert_eq!(r.kind, RefusalKind::OverlapDisagreement);
    // The refusal has to set the expectation, not just report a number: two overlaps
    // are *not* expected to agree exactly, and the tolerance is a plausibility check.
    assert!(r.message.contains("not* expected to read identically"), "{}", r.message);
    assert!(r.message.contains("a few ms is normal"), "{}", r.message);
    assert!(r.message.contains("every* speaker aligned so far"), "the stake has to be in the sentence: {}", r.message);
    assert_eq!(chain.steps.len(), 1, "the bad position was not recorded");
    assert!(!chain.aligned.contains(&"c".to_string()));
    assert_eq!(applied(&s, "a"), 5, "position 1 is untouched");
    assert_eq!(s.phase, Phase::Positioning, "the chain is still waiting, not refused");
    assert!(s.refusal.is_none(), "a step refusal is not the run's refusal");

    // Standing somewhere the two overlaps *are* plausible, the same position works.
    at_position(&m, &arrivals, &[("a", 15.0), ("b", 20.0), ("c", 8.0)], &["c"], &["a", "b"]).await;
    let s = wait_chain(&m).await;
    assert_eq!(chain_of(&s).steps.len(), 2, "{}", chain_of(&s).prompt);
    assert!(chain_of(&s).refusal.is_none(), "an accepted position clears the last refusal");
    assert_eq!(applied(&s, "c"), 12, "the overlaps read 20 ms here, so 'c' at 8 ms is delayed by 12");
}

/// One overlap is **possible** — a user may genuinely have only one shared speaker —
/// and it is reported as what it is: a joint nothing checks, and a chain whose total
/// error cannot be bounded (plan §1.1).
#[tokio::test(start_paused = true)]
async fn a_single_overlap_step_is_accepted_and_reported_as_reduced_confidence() {
    let rig = chain_rig(&["a", "b", "c"]);
    let arrivals = rig.arrivals.clone();
    let m = manager();
    m.start(rig.deps).await.expect("started");
    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
    wait_chain(&m).await;
    at_position(&m, &arrivals, &[("a", 20.0), ("c", 10.0)], &["c"], &["a"]).await;
    let s = wait_chain(&m).await;

    let chain = chain_of(&s);
    let two = &chain.steps[1];
    assert_eq!(two.confidence, OverlapConfidence::Single);
    assert_eq!(two.disagreement_ms, None, "one overlap has nothing to disagree with");
    assert_eq!(two.joint_error_ms, None, "and therefore no error estimate at all");
    assert!(two.note.contains("weakest"), "{}", two.note);
    assert_eq!(applied(&s, "c"), 15, "'a' reads 25 ms here with its 5 ms, so 'c' at 10 is delayed by 15");

    // The honest answer about the total is *no* total, not a partial one.
    assert!(!chain.error.bounded);
    assert_eq!(chain.error.joint_ms, None);
    assert!(chain.error.message.contains("cannot be bounded"), "{}", chain.error.message);
    assert!(chain.error.message.contains("single overlap"), "{}", chain.error.message);
    let warn = s.warnings.iter().find(|w| w.kind == WarningKind::OneOverlap).expect("the user has to be told which step was weaker");
    assert!(warn.message.contains("nothing to check it against"), "{}", warn.message);

    m.finish().expect("finish accepted");
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
    // …and it is still on the proposal, where the user decides whether to write.
    assert!(s.proposal.as_ref().expect("a proposal").warnings.iter().any(|w| w.kind == WarningKind::OneOverlap));
    assert!(!chain_of(&s).error.bounded);
}

/// Plan §1.1.1: the real knobs are written **once**, after the last position. A
/// per-position write would spend the run's wall clock waiting for speakers to come
/// back (§2.3: tens of seconds each).
#[tokio::test(start_paused = true)]
async fn a_multi_step_chain_writes_exactly_one_wave_and_drops_the_delay_lines() {
    let rig = chain_rig(&["a", "b", "c"]);
    let (relay, arrivals) = (rig.relay.clone(), rig.arrivals.clone());
    let m = manager();
    m.start(rig.deps).await.expect("started");
    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
    wait_chain(&m).await;
    at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("c", 10.0)], &["c"], &["a", "b"]).await;
    wait_chain(&m).await;
    m.finish().expect("finish accepted");
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
    assert!(relay.applied_ms("a") > 0.0, "the alignment is in the line until `apply`");

    // Post-write the whole set arrives together — the knobs now carry what the lines
    // were standing in for, so the fake capture is told to render them level.
    let after = chain_rig(&["a", "b", "c"]);
    *after.arrivals.lock_recover() = [("a", 0.0), ("b", 0.0), ("c", 0.0)].iter().map(|(n, v)| ((*n).to_string(), *v)).collect();
    let writer = after.writer.clone();
    m.apply(after.deps).await.expect("apply accepted");
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Done, "{}", s.message);

    // One wave: every knob written at most once, and only the ones that changed.
    let writes = writer.writes.lock_recover().clone();
    for (name, _) in &writes {
        assert_eq!(writer.count(name), 1, "'{name}' was written more than once: {writes:?}");
    }
    assert!(writes.len() <= 3, "a three-speaker chain cannot need more than three writes: {writes:?}");
    // 'c' needed the most provisional delay, so the renormalisation leaves it at
    // advance 0 — which is where it already was, and writing it would reconnect a
    // speaker for nothing (plan §2.3).
    assert_eq!(proposed(&s, "c").new_delay_ms, 0);
    assert_eq!(writer.count("c"), 0, "an unchanged knob is not written: {writes:?}");
    assert_eq!(writer.last("a"), Some(10));
    assert_eq!(writer.last("b"), Some(15));
    // And the provisional lines are gone, or every chained member would be delayed
    // twice (plan §1.1.1: the line is a stand-in for the knob, not an addition).
    assert_eq!(relay.applied_ms("a"), 0.0);
    assert!(chain_of(&s).provisional.is_empty());

    // The verification says what it covered: the *last* position only (§10.4's rule,
    // applied to a chain).
    let v = s.verification.clone().expect("a verification");
    assert!(v.passed, "residual {} ms", v.residual.worst_ms);
    let note = v.scope_note.clone().expect("a chain must not imply it re-checked every position");
    assert!(note.contains("last of 2 position(s)"), "{note}");
    assert!(note.contains("walking the chain again"), "{note}");
}

/// Plan §1.2: a *position* is one continuous capture. A reconnect inside one discards
/// that position's readings — nothing is ever solved across the seam — while the
/// positions already aligned survive, because what carries a chain from one position
/// to the next is the delay each speaker holds, and the next position re-measures its
/// overlaps in the new frame.
#[tokio::test(start_paused = true)]
async fn a_mic_reconnect_voids_the_position_in_flight_without_mixing_frames() {
    let rig = chain_rig(&["a", "b", "c"]);
    let (reconnect_at, mic, arrivals) = (rig.reconnect_at.clone(), rig.mic.clone(), rig.arrivals.clone());
    let m = manager();
    m.start(rig.deps).await.expect("started");
    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
    let s = wait_chain(&m).await;
    let before = s.observations.len();
    assert_eq!(before, 4, "two members, two passes");

    // 5 s from now: past the 3 s mute guard, so it lands inside the next reading —
    // the only place a reconnect is detectable.
    reconnect_at.store(mic.frames_now() + u64::from(mic.rate) * 5, Ordering::Relaxed);
    at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("c", 10.0)], &["c"], &["a", "b"]).await;

    let s = wait_for(&m, "the position to restart", |s| s.chain.as_ref().is_some_and(|c| c.restarts >= 1)).await;
    let warn = s.warnings.iter().find(|w| w.kind == WarningKind::MicReconnected).expect("the user has to be told");
    assert!(warn.message.contains("discarded"), "{}", warn.message);
    assert!(warn.message.contains("earlier positions are not affected"), "{}", warn.message);
    assert!(warn.message.contains("re-measures its overlaps"), "the reason they survive has to be in the sentence: {}", warn.message);

    let s = wait_chain(&m).await;
    let chain = chain_of(&s);
    assert_eq!(chain.steps.len(), 2, "the position was retaken, not lost: {}", chain.prompt);
    assert_eq!(applied(&s, "a"), 5, "position 1 came through untouched");
    assert_eq!(applied(&s, "c"), 15);
    // Nothing from the discarded attempt survived into the readings the step solved.
    assert_eq!(s.observations.len(), before + 6, "3 members × 2 passes for position 2, and nothing left over");
    // The two positions are in **different** frames, and that is fine precisely
    // because no arrival is ever compared across them.
    assert_eq!(chain.steps[0].grid_epoch, 0);
    assert_eq!(chain.steps[1].grid_epoch, 1, "the post-reconnect epoch");
    // Every reading a step solved shares that step's epoch — the seam the epoch check
    // in `arrivals_of` refuses is never reached.
    for step in &chain.steps {
        let names: Vec<&String> = step.members.iter().chain(step.overlaps.iter().map(|o| &o.node_name)).collect();
        let mine: Vec<&MemberObservation> =
            s.observations.iter().filter(|o| names.contains(&&o.node_name) && o.grid_epoch == step.grid_epoch).collect();
        assert_eq!(mine.len(), names.len() * MEASURE_PASSES, "step {} is missing readings in its own epoch", step.index);
    }
}

/// §2.4.2, through the chain: a sendspin-only run is aligned to the member that needed
/// the **most** delay — its earliest arrival — which takes advance 0 while everyone
/// else is advanced to meet it. The renormalisation is the interval solver, not a
/// subtraction, and this is the case most likely to regress.
#[test]
fn a_sendspin_only_chain_aligns_to_its_earliest_member_after_renormalising() {
    let members = [member("a"), member("b"), member("c")];
    // A floor that has ratcheted a long way up, which is exactly what §1.1 warns
    // about: every step could only add.
    let provisional: HashMap<String, f64> =
        [("a".to_string(), 100.0), ("b".to_string(), 105.0), ("c".to_string(), 120.0)].into_iter().collect();
    let p = solve_chain(&ChainSolveInput {
        timing: Timing::real(),
        members: &members,
        provisional: &provisional,
        current_delays: &HashMap::new(),
        send_ahead: &SendAheadContext::default(),
        steps: &[],
        observations: &[],
    })
    .expect("accepted");
    let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
    // 'c' needed the most delay, i.e. it arrived earliest: advance 0, and the others
    // are advanced to meet it.
    assert_eq!(by("c").new_delay_ms, 0);
    assert_eq!(by("b").new_delay_ms, 15);
    assert_eq!(by("a").new_delay_ms, 20);
    assert_eq!(p.reference, "c");
    assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
    // The point of the renormalisation: nobody carries the 100 ms floor the chain
    // accumulated, and the relative geometry is untouched.
    assert_eq!(p.largest_knob_ms, 20);
    assert_eq!(p.members.iter().map(|m| m.new_delay_ms).min(), Some(0));
    assert!((p.spread_ms - 20.0).abs() < 1e-9, "the ratchet that was removed: {}", p.spread_ms);
    assert_eq!(by("a").new_delay_ms - by("b").new_delay_ms, 5, "'a' needed 5 ms less delay than 'b', so it ends 5 ms less advanced");
}

/// The renormalisation cannot be "subtract the minimum" once the polarities are mixed
/// (plan §2.4.2), so it is the interval solver — and the solver's answer is a common
/// shift chosen inside the intersection, which lands both knobs at the same value here.
#[test]
fn a_mixed_polarity_chain_is_renormalised_by_the_interval_solver() {
    let members = [member("spk"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
    let provisional: HashMap<String, f64> = [("spk".to_string(), 20.0), ("ap2-dev-x".to_string(), 0.0)].into_iter().collect();
    // The sendspin member needs headroom to be moved *later*: reducing an advance is
    // the only way to do it, so it has to have one to give up.
    let current: HashMap<String, u16> = [("spk".to_string(), 30u16)].into_iter().collect();
    let p = solve_chain(&ChainSolveInput {
        timing: Timing::real(),
        members: &members,
        provisional: &provisional,
        current_delays: &current,
        send_ahead: &SendAheadContext::default(),
        steps: &[],
        observations: &[],
    })
    .expect("accepted");
    let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
    assert_eq!(by("spk").new_delay_ms, 5, "the advance is reduced by 25, which delays it by 25");
    assert_eq!(by("ap2-dev-x").new_delay_ms, 5, "and the AP2 member is delayed by 5 — the difference is the 20 ms asked for");
    assert_eq!(p.largest_knob_ms, 5, "the target chosen is the one that keeps the largest knob smallest (§9.2)");

    // Without that headroom the chain is measurable but **not writable**, and the
    // refusal names both members rather than writing a best effort (§2.4.2).
    let r = solve_chain(&ChainSolveInput {
        timing: Timing::real(),
        members: &members,
        provisional: &provisional,
        current_delays: &HashMap::new(),
        send_ahead: &SendAheadContext::default(),
        steps: &[],
        observations: &[],
    })
    .expect_err("an advance-only member cannot be pushed later");
    assert_eq!(r.kind, RefusalKind::KnobRange);
    assert!(r.message.contains("spk") && r.message.contains("ap2-dev-x"), "{}", r.message);
}

/// §1.1's algebra on its own, with no capture, no relay and no runtime — including the
/// two cases the orchestration cannot express as cleanly: three overlaps, and an
/// aligned set that is already the latest thing at the new position.
#[test]
fn the_chain_step_algebra_takes_the_latest_arrival_as_the_target() {
    let aligned: HashMap<String, f64> = [("x".to_string(), 4.0), ("y".to_string(), 0.0), ("z".to_string(), 7.0)].into_iter().collect();
    // The aligned set reads 10 and 12 here (mean 11); the new member arrives at 30.
    let arrivals = [("new".to_string(), 30.0), ("x".to_string(), 10.0), ("y".to_string(), 12.0)];
    let overlaps = ["x".to_string(), "y".to_string()];
    let s = chain_step(&ChainStepInput { aligned: &aligned, arrivals: &arrivals, overlaps: &overlaps, tolerance_ms: 8.0 })
        .expect("2 ms apart is plausible");
    assert_eq!(s.target_ms, 30.0);
    assert_eq!(s.anchor_ms, Some(11.0));
    assert_eq!(s.delta_ms, 19.0);
    assert_eq!(s.provisional.get("new"), Some(&0.0), "the latest arrival needs no delay");
    assert_eq!(s.provisional.get("x"), Some(&23.0), "4 + Δ");
    assert_eq!(s.provisional.get("y"), Some(&19.0), "0 + Δ");
    assert_eq!(s.provisional.get("z"), Some(&26.0), "7 + Δ — never measured here, moved anyway");
    assert_eq!(s.disagreement_ms, Some(2.0));
    assert_eq!(s.joint_error_ms, Some(1.0), "the anchor is the mean, so it can be out by half the disagreement");
    assert_eq!(s.confidence, OverlapConfidence::Checked);

    // The other direction: the aligned set is already the latest thing here, so it
    // does not move at all and the new members are delayed to it.
    let arrivals = [("new".to_string(), 5.0), ("x".to_string(), 20.0), ("y".to_string(), 20.0)];
    let s =
        chain_step(&ChainStepInput { aligned: &aligned, arrivals: &arrivals, overlaps: &overlaps, tolerance_ms: 8.0 }).expect("accepted");
    assert_eq!(s.delta_ms, 0.0);
    assert_eq!(s.provisional.get("new"), Some(&15.0));
    assert_eq!(s.provisional.get("z"), None, "with no Δ the aligned set is not touched at all");

    // A later position with no overlap has nothing tying it to anything.
    let arrivals = [("new".to_string(), 5.0)];
    let r =
        chain_step(&ChainStepInput { aligned: &aligned, arrivals: &arrivals, overlaps: &[], tolerance_ms: 8.0 }).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::OverlapMissing);
    assert!(r.message.contains("mutually meaningless"), "{}", r.message);
}

/// The chain's error statement, which is the whole honesty budget of the feature.
#[test]
fn the_accumulated_error_is_the_joints_and_is_withheld_when_a_joint_is_unmeasurable() {
    let step = |index: usize, confidence: OverlapConfidence, joint: Option<f64>| ChainStep {
        index,
        members: vec![format!("m{index}")],
        overlaps: match confidence {
            OverlapConfidence::Origin => Vec::new(),
            _ => vec![ChainOverlap { node_name: "ov".into(), arrival_ms: 0.0, applied_ms: 0.0 }],
        },
        confidence,
        disagreement_ms: joint.map(|j| j * 2.0),
        worst_pair: None,
        tolerance_ms: OVERLAP_AGREEMENT_TOL_MS,
        anchor_ms: None,
        delta_ms: 0.0,
        target_ms: 0.0,
        spread_ms: 0.0,
        drift_ppm: 0.0,
        joint_error_ms: joint,
        grid_epoch: 0,
        checks: Checks {
            transitivity: transitivity(&[], &Timing::real(), TRANSITIVITY_TOL_MS),
            repeatability: None,
            merged_peak: MergedPeakCheck::seam(),
            closure: None,
        },
        note: String::new(),
    };
    // One position is not a chain: no joints, so nothing accumulates.
    let one = chain_error(&[step(1, OverlapConfidence::Origin, None)]);
    assert!(one.bounded && one.joint_ms == Some(0.0));
    assert!(one.message.contains("no joints"), "{}", one.message);

    // Two checked joints compose additively — the shifts are applied one on top of
    // the other, so the worst case is the sum.
    let checked = chain_error(&[
        step(1, OverlapConfidence::Origin, None),
        step(2, OverlapConfidence::Checked, Some(1.5)),
        step(3, OverlapConfidence::Checked, Some(2.0)),
    ]);
    assert!(checked.bounded);
    assert_eq!(checked.joint_ms, Some(3.5));
    assert!(checked.message.contains("3.5 ms"), "{}", checked.message);
    // …and it still says what it is *not* measuring.
    assert!(checked.message.contains("§5.6"), "{}", checked.message);
    assert!(checked.message.contains("between* regions"), "{}", checked.message);

    // One unchecked joint anywhere and there is no total, rather than a total with a
    // hole in it.
    let single = chain_error(&[
        step(1, OverlapConfidence::Origin, None),
        step(2, OverlapConfidence::Checked, Some(1.5)),
        step(3, OverlapConfidence::Single, None),
    ]);
    assert!(!single.bounded);
    assert_eq!(single.joint_ms, None);
    assert!(single.message.contains("position 3"), "{}", single.message);
    assert!(single.message.contains("would be worse than none"), "{}", single.message);
}

/// The out-of-order cases, all of which are states the user can act on — so they are
/// refusals with a sentence, never a 500 and never silence (plan §11).
#[tokio::test(start_paused = true)]
async fn the_chain_refuses_calls_that_do_not_match_where_it_is() {
    let m = manager();
    let r = m.position(vec!["a".into()], Vec::new()).expect_err("idle refuses");
    assert_eq!(r.kind, RefusalKind::ChainOutOfOrder);

    let rig = chain_rig(&["a", "b", "c"]);
    let (arrivals, relay, writer) = (rig.arrivals.clone(), rig.relay.clone(), rig.writer.clone());
    m.start(rig.deps).await.expect("started");
    wait_chain(&m).await;

    // A speaker the run is not holding.
    let r = m.position(vec!["ghost".into(), "a".into()], Vec::new()).expect_err("must refuse");
    assert_eq!(r.member.as_deref(), Some("ghost"));
    // An overlap at the first position, where nothing is aligned yet.
    let r = m.position(vec!["a".into(), "b".into()], vec!["c".into()]).expect_err("must refuse");
    assert!(r.message.contains("first position"), "{}", r.message);
    // One speaker on its own has nothing to be aligned *to*.
    let r = m.position(vec!["a".into()], Vec::new()).expect_err("must refuse");
    assert!(r.message.contains("at least two speakers"), "{}", r.message);
    // Finishing with speakers still unaligned.
    let r = m.finish().expect_err("must refuse");
    assert!(r.message.contains("not been aligned"), "{}", r.message);
    // A near-field call on a chained run points at the right endpoint.
    let r = m.arrival("a".into(), None).expect_err("must refuse");
    assert!(r.message.contains("/api/align/measure/position"), "{}", r.message);

    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
    // A second tap while the position is being measured.
    let r = m.position(vec!["c".into()], vec!["a".into()]).expect_err("must refuse");
    assert!(r.message.contains("busy"), "{}", r.message);
    wait_chain(&m).await;

    // Re-aligning a speaker that is already aligned, and using one that is not as an
    // overlap: the two halves of the same mistake, each named correctly.
    let r = m.position(vec!["a".into(), "c".into()], vec!["b".into()]).expect_err("must refuse");
    assert_eq!(r.member.as_deref(), Some("a"));
    assert!(r.message.contains("Offer it as an *overlap*"), "{}", r.message);
    let r = m.position(vec!["c".into()], vec!["c".into()]).expect_err("must refuse");
    assert!(r.message.contains("named twice"), "{}", r.message);

    // Abandoning mid-chain gives the provisional delays back and writes nothing.
    assert!(relay.applied_ms("a") > 0.0);
    let after = m.abandon();
    assert_eq!(after.phase, Phase::Idle);
    assert!(after.chain.is_none(), "abandoning clears the chain with the run");
    assert_eq!(relay.applied_ms("a"), 0.0, "a closed tab must not leave a delay line applied (plan §1.1.1)");
    assert!(after.message.contains("provisional"), "{}", after.message);
    assert!(writer.writes.lock_recover().is_empty());
}

/// A run that is not chained takes no positions, and says why rather than accepting
/// one and doing nothing with it.
#[tokio::test(start_paused = true)]
async fn an_unchained_run_is_not_a_chain_and_says_so() {
    let rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::SweetSpot, 0.0);
    let m = manager();
    m.start(rig.deps).await.expect("started");
    let r = m.position(vec!["a".into(), "b".into()], Vec::new()).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::ChainOutOfOrder);
    assert!(r.message.contains("`chain: true`"), "{}", r.message);
    let s = wait_terminal(&m).await;
    assert_eq!(s.phase, Phase::Proposed, "the ordinary path is untouched: {}", s.message);
    assert!(s.chain.is_none(), "an unchained run has no chain state");

    // And a near-field walk is refused for its own reason: it needs no overlaps at all.
    let walk = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::NearField, 0.0);
    m.abandon();
    m.start(walk.deps).await.expect("started");
    let r = m.position(vec!["a".into(), "b".into()], Vec::new()).expect_err("must refuse");
    assert!(r.message.contains("one continuous capture"), "{}", r.message);
}

/// A chain over a pw-sink member: the mode admits every member kind, and the floor
/// that cannot be written below is the solver's problem rather than the chain's
/// (plan §1.1.2 item 4).
#[tokio::test(start_paused = true)]
async fn a_chain_can_include_a_pw_sink_member() {
    let rig = chain_rig_of(&[("a", MemberKind::Sendspin), ("b", MemberKind::Sendspin), ("pwsink-dev-host", MemberKind::PwSink)]);
    let arrivals = rig.arrivals.clone();
    let m = manager();
    m.start(rig.deps).await.expect("started");
    at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
    wait_chain(&m).await;
    at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("pwsink-dev-host", 10.0)], &["pwsink-dev-host"], &["a", "b"]).await;
    wait_chain(&m).await;
    m.finish().expect("finish accepted");
    let s = wait_terminal(&m).await;

    // The pw-sink member's playout delay cannot go below three packet times, and that
    // floor is what pins the whole group's target — a refusal or a shifted target, but
    // never a value nobody can write.
    match s.phase {
        Phase::Proposed => {
            let p = s.proposal.clone().expect("a proposal");
            let sink = proposed(&s, "pwsink-dev-host");
            assert_eq!(sink.polarity, KnobPolarity::Delay);
            assert!(sink.new_delay_ms >= crate::routing::sync_settings::PWSINK_JITTER_MIN_MS, "{sink:?}");
            assert!(p.blocked.is_none(), "{:?}", p.blocked);
        }
        Phase::Refused => {
            let r = s.refusal.clone().expect("a refusal");
            assert_eq!(r.kind, RefusalKind::KnobRange, "{}", r.message);
        }
        other => panic!("unexpected phase {other:?}: {}", s.message),
    }
}
