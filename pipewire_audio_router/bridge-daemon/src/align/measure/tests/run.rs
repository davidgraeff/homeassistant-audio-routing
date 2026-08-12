//! Whole-run orchestration: measure, apply, verify, revert, and what each failure
//! mode reports.
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

/// The whole machine over synthetic audio: ARMING → LEARNING → MEASURING →
/// SOLVING → PROPOSED, with a real estimator behind a fake transport.
#[tokio::test(start_paused = true)]
async fn a_full_run_proposes_delays_that_match_the_injected_arrivals() {
    let (deps, _w, _a, _c) = deps_for(&[("early", 0.0), ("late", 9.0)]);
    let inner = Arc::new(Mutex::new(Inner::idle()));
    inner.lock_recover().members = vec![
        MemberProgress {
            node_name: "early".into(),
            kind: MemberKind::Sendspin,
            level: 50,
            current_delay_ms: 0,
            passes_done: 0,
            last: None,
            note: None,
        },
        MemberProgress {
            node_name: "late".into(),
            kind: MemberKind::Sendspin,
            level: 50,
            current_delay_ms: 0,
            passes_done: 0,
            last: None,
            note: None,
        },
    ];
    let cancel = AtomicBool::new(false);
    let phase = run_measure(&deps, &inner, &cancel, None).await.expect("the run must reach a proposal");
    assert_eq!(phase, Phase::Proposed);
    let status = inner.lock_recover().status();
    let p = status.proposal.expect("a proposal");
    assert_eq!(p.reference, "early", "sendspin knobs advance, so the earliest member is the one left alone");
    let late = p.members.iter().find(|m| m.node_name == "late").unwrap();
    assert_eq!(late.new_delay_ms, 9, "9 ms of injected offset must come back as 9 ms of advance");
    assert_eq!(late.polarity, KnobPolarity::Advance);
    assert_eq!(p.members.iter().find(|m| m.node_name == "early").unwrap().new_delay_ms, 0);
    assert!(p.blocked.is_none(), "{:?}", p.blocked);
    assert!(p.checks.transitivity.passed);
    // Every member measured twice, alternating.
    assert_eq!(status.observations.len(), 4);
    assert_eq!(status.observations[0].node_name, "early");
    assert_eq!(status.observations[2].node_name, "late", "pass 2 runs the list backwards");
    // The W4 seam is reported, not hidden.
    assert!(status.warnings.iter().any(|w| w.kind == WarningKind::LevelLearningSkipped));
}

#[tokio::test(start_paused = true)]
async fn losing_the_session_mid_run_says_which_one_went_away() {
    let (deps, _w, active, _c) = deps_for(&[("a", 0.0), ("b", 5.0)]);
    let inner = Arc::new(Mutex::new(Inner::idle()));
    inner.lock_recover().sources = vec!["src".to_string()];
    active.store(false, Ordering::Relaxed);
    let cancel = AtomicBool::new(false);
    let r = run_measure(&deps, &inner, &cancel, None).await.expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::SessionLost);
    assert!(r.message.contains("microphone is still connected"), "{}", r.message);
}

#[tokio::test(start_paused = true)]
async fn losing_the_microphone_mid_run_says_which_one_went_away() {
    let (deps, _w, _a, connected) = deps_for(&[("a", 0.0), ("b", 5.0)]);
    let inner = Arc::new(Mutex::new(Inner::idle()));
    connected.store(false, Ordering::Relaxed);
    let cancel = AtomicBool::new(false);
    let r = run_measure(&deps, &inner, &cancel, None).await.expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::MicLost);
    assert!(r.message.contains("session is still running"), "{}", r.message);
}

#[tokio::test(start_paused = true)]
async fn abandoning_stops_the_run_without_writing_anything() {
    let (deps, writer, _a, _c) = deps_for(&[("a", 0.0), ("b", 5.0)]);
    let inner = Arc::new(Mutex::new(Inner::idle()));
    let cancel = AtomicBool::new(true);
    let r = run_measure(&deps, &inner, &cancel, None).await.expect_err("must stop");
    assert_eq!(r.kind, RefusalKind::Cancelled);
    assert!(writer.writes.lock_recover().is_empty());
}

#[tokio::test(start_paused = true)]
async fn apply_writes_only_what_changed_and_verifies_the_result() {
    // The proposal from a real run, then the write + settle + verify half. The
    // fake mic renders each member at its *post-write* arrival, so the residual
    // is what a correctly applied delay would produce.
    let (deps, writer, _a, _c) = deps_for(&[("early", 0.0), ("late", 7.0)]);
    let inner = Arc::new(Mutex::new(Inner::idle()));
    let cancel = AtomicBool::new(false);
    run_measure(&deps, &inner, &cancel, None).await.expect("proposal");
    let proposal = inner.lock_recover().proposal.clone().expect("a proposal");
    assert_eq!(proposal.reference, "early");

    // Post-write the two members arrive together — and they do so at the *earlier*
    // arrival, because what was written is an advance on the late one.
    let (mut deps2, writer2, _a2, _c2) = deps_for(&[("early", 0.0), ("late", 0.0)]);
    deps2.writer = writer2.clone();
    let phase = run_apply(&deps2, &inner, &cancel, &proposal, None).await.expect("verified");
    assert_eq!(phase, Phase::Done);
    let writes = writer2.writes.lock_recover().clone();
    assert_eq!(writes, vec![("late".to_string(), 7)], "only the member whose knob changed is written");
    assert!(writer.writes.lock_recover().is_empty());
    let v = inner.lock_recover().verification.clone().expect("a verification");
    assert!(v.passed, "residual {} ms", v.residual.worst_ms);
    assert!(v.residual.worst_ms < RESIDUAL_TOL_MS);
    assert_eq!(v.merged_peak.state, "not_implemented");
}

#[tokio::test(start_paused = true)]
async fn a_write_that_did_not_take_fails_verification_instead_of_claiming_success() {
    let (deps, _w, _a, _c) = deps_for(&[("early", 0.0), ("late", 7.0)]);
    let inner = Arc::new(Mutex::new(Inner::idle()));
    let cancel = AtomicBool::new(false);
    run_measure(&deps, &inner, &cancel, None).await.expect("proposal");
    let proposal = inner.lock_recover().proposal.clone().expect("a proposal");
    // The delay never took: the members still arrive 7 ms apart.
    let (deps2, _w2, _a2, _c2) = deps_for(&[("early", 0.0), ("late", 7.0)]);
    let r = run_apply(&deps2, &inner, &cancel, &proposal, None).await.expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::ResidualTooLarge);
    let v = inner.lock_recover().verification.clone().expect("the numbers are still reported");
    assert!(!v.passed);
}

#[test]
fn the_phase_machine_only_starts_from_a_terminal_state() {
    for p in [Phase::Idle, Phase::Done, Phase::Refused, Phase::Proposed] {
        assert!(p.is_terminal(), "{p:?}");
    }
    for p in [
        Phase::Arming,
        Phase::Learning,
        // Parked waiting for the user is *not* terminal: the run is alive and
        // holding the group, so a second `start` must refuse rather than quietly
        // abandoning a walk in progress — or, for a chain, stranding an apartment's
        // worth of provisional delays with nothing left that knows about them.
        Phase::Walking,
        Phase::Positioning,
        Phase::Measuring,
        Phase::Solving,
        Phase::Writing,
        Phase::Settling,
        Phase::Verifying,
    ] {
        assert!(!p.is_terminal(), "{p:?}");
    }
}

#[test]
fn the_send_ahead_mark_takes_the_largest_member_requirement() {
    let ctx = SendAheadContext {
        floor_ms: 50,
        unreported_floor_ms: 250,
        min_buffer_ms: [("a".to_string(), Some(100)), ("b".to_string(), None)].into_iter().collect(),
    };
    let none: HashMap<String, u16> = HashMap::new();
    assert_eq!(ctx.mark_ms(&none), 250, "an unreported member still needs the codec floor");
    let delays: HashMap<String, u16> = [("a".to_string(), 400u16)].into_iter().collect();
    assert_eq!(ctx.mark_ms(&delays), 500, "a member's delay is part of its send-ahead");
}

#[test]
fn the_level_seam_matches_what_the_level_solver_actually_takes() {
    use crate::align::levels::{LevelConfig, LevelSolver, RampMode};
    let members = [member("a"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
    let seam = learn_levels(42, &members);
    assert!(!seam.learned);
    assert_eq!(seam.levels.get("a"), Some(&42));
    assert_eq!(seam.levels.len(), 2);
    assert!(seam.note.contains("W4"));
    // Stage 1 shares one estimator channel across every member (plan §2.2), so
    // the solve *must* be sequential — the parallel mode refuses exactly this
    // member model, and that refusal is the thing worth pinning down before W4
    // is wired up rather than after.
    assert_eq!(seam.config.mode, RampMode::Sequential);
    assert!(LevelSolver::with_config(seam.specs.clone(), seam.config.clone()).is_ok());
    let parallel = LevelConfig { mode: RampMode::Parallel, ..seam.config.clone() };
    let err = LevelSolver::with_config(seam.specs.clone(), parallel).err().expect("one channel for two members");
    assert!(err.contains("one measurement channel per member"), "{err}");
    // AP2's knob needs a snapshot/restore the session does not have yet — the
    // documented half of why this is still a seam.
    assert_eq!(seam.specs[1].kind.knob(), crate::align::levels::LevelKnob::SnapshotRestore);
    assert!(seam.specs.iter().all(|s| s.snapshot_level.is_none()));
}

#[tokio::test]
async fn revert_restores_only_the_members_that_were_actually_written() {
    // Plan §9.4 is one click back — but a revert that rewrites every member's
    // delay would reconnect devices that were never touched, and each reconnect
    // is tens of seconds of silence (plan §2.3).
    let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
    {
        let mut g = m.inner.lock_recover();
        g.snapshot =
            [("a".to_string(), (MemberKind::Sendspin, 12u16)), ("b".to_string(), (MemberKind::Sendspin, 0u16))].into_iter().collect();
        g.written = vec!["a".to_string()];
    }
    let w = FakeWriter::default();
    let s = m.revert(&w).await.expect("restores");
    assert_eq!(w.writes.lock_recover().clone(), vec![("a".to_string(), 12)]);
    assert!(!s.can_revert, "there is nothing left to restore");
    assert_eq!(s.revert_scope, None, "nothing pending, so nothing to scope");
    assert!(m.revert(&w).await.is_err(), "a second revert must refuse rather than reconnect the group again");
    assert_eq!(w.writes.lock_recover().len(), 1);
}

/// The write survives `abandon`, so the *pointer to it* has to as well — a page
/// reload is otherwise the end of the only route back from a destructive change
/// (plan §9.4).
#[test]
fn abandoning_after_a_write_keeps_the_revert_snapshot_and_its_scope() {
    let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
    {
        let mut g = m.inner.lock_recover();
        g.sources = vec!["src-a".to_string(), "src-b".to_string()];
        g.snapshot = [("a".to_string(), (MemberKind::Sendspin, 12u16))].into_iter().collect();
        g.mark_written("a");
    }
    let before = m.status();
    assert_eq!(before.revert_scope.as_deref(), Some(["src-a".to_string(), "src-b".to_string()].as_slice()));

    let s = m.abandon();
    assert!(s.can_revert, "a written delay must stay revertable after abandoning");
    assert_eq!(s.phase, Phase::Idle);
    assert_eq!(s.sources, Vec::<String>::new(), "the run itself is gone");
    assert_eq!(
        s.revert_scope.as_deref(),
        Some(["src-a".to_string(), "src-b".to_string()].as_slice()),
        "the group the pending revert belongs to must outlive the run"
    );
    // Still there on a later poll, i.e. it is state and not a one-shot reply.
    assert_eq!(m.status().revert_scope, s.revert_scope);
}

#[test]
fn a_run_with_nothing_written_has_no_revert_scope() {
    let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
    m.inner.lock_recover().sources = vec!["src".to_string()];
    let s = m.status();
    assert!(!s.can_revert);
    assert_eq!(s.revert_scope, None, "`can_revert` and `revert_scope` must agree");
    assert_eq!(m.abandon().revert_scope, None);
}

/// Plan §11: the status is pushed, not polled. `measure_ws` is a thin wrapper
/// over this notification, so what is worth testing is that the notification
/// actually fires on a state change — and that a reset does not disconnect a
/// subscriber that is already watching.
#[tokio::test]
async fn the_status_notifier_fires_on_a_change_and_survives_a_reset() {
    let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
    let mut rx = m.subscribe();
    assert!(!rx.has_changed().unwrap(), "a fresh subscriber starts level");

    set_phase(&m.inner, Phase::Measuring, "measuring 'a'");
    assert!(rx.has_changed().expect("the notifier must still be alive"));
    rx.changed().await.expect("a change");
    assert_eq!(m.status().phase, Phase::Measuring);

    m.inner.lock_recover().warn(Warning::new(WarningKind::NoDriftFit, "no fit"));
    assert!(rx.has_changed().unwrap(), "a warning is a change too");
    rx.changed().await.unwrap();

    // `abandon` replaces the whole state; the socket watching it must not be cut
    // off by that, or the UI would go silent exactly when the run ended.
    m.abandon();
    assert!(rx.has_changed().expect("abandon must not drop the notifier"));
    rx.changed().await.expect("the reset is itself a change");
    assert_eq!(m.status().phase, Phase::Idle);
}
