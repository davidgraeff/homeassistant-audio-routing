//! The lock gate: when a mic window counts as a stable measurement, and what it
//! reports when it does not.
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

#[test]
fn the_gate_locks_only_after_enough_stable_periods() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    for p in 1..GATE_MIN_PERIODS {
        let step = gate.observe(&clean(p as u64 * 2000, p, 0.2));
        assert!(!step.locked, "locked after only {p} periods");
        assert_eq!(step.progress.waiting_for, Some(GateReason::Acquiring));
    }
    let step = gate.observe(&clean(8000, GATE_MIN_PERIODS, 0.2));
    assert!(step.locked, "{:?}", step.progress);
    assert!(!step.restart && step.failed.is_none());
}

#[test]
fn the_gate_passes_the_estimators_refusal_through_verbatim() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    let mut s = clean(2000, GATE_MIN_PERIODS, 0.2);
    s.quality = rejected(RejectReason::LowSnr, "the a tone is only 6.0 dB above the room's noise floor");
    let step = gate.observe(&s);
    assert!(!step.locked);
    assert_eq!(step.progress.waiting_for, Some(GateReason::Estimator));
    assert!(step.progress.message.contains("6.0 dB"), "the estimator's own sentence must survive: {}", step.progress.message);
    // …and it is still the message the user gets when the gate finally gives up.
    s.elapsed = GATE_TIMEOUT_SETTLE;
    let step = gate.observe(&s);
    let failed = step.failed.expect("the timeout must fail the gate");
    assert_eq!(failed.kind, RefusalKind::Estimator);
    assert_eq!(failed.estimator_reason, Some(RejectReason::LowSnr));
    assert!(failed.message.contains("noise floor"), "{}", failed.message);
}

#[test]
fn a_gap_or_a_clip_restarts_the_window_instead_of_measuring_it() {
    for (label, mutate) in [("gap", 0), ("clip", 1)] {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        gate.observe(&clean(2000, 1, 0.2));
        gate.observe(&clean(4000, 2, 0.2));
        let mut s = clean(6000, GATE_MIN_PERIODS, 0.2);
        if mutate == 0 {
            s.gap = true;
        } else {
            s.clipped = true;
        }
        let step = gate.observe(&s);
        assert!(step.restart, "{label} must discard the accumulated window");
        assert!(!step.locked);
        // The count restarts: a fresh set of periods is required afterwards.
        let step = gate.observe(&clean(8000, GATE_MIN_PERIODS, 0.2));
        assert!(step.locked, "a clean window after the {label} must be usable again");
    }
}

#[test]
fn a_disconnect_and_a_reconnect_are_reported_differently() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    let mut s = clean(1000, GATE_MIN_PERIODS, 0.2);
    s.connected = false;
    let step = gate.observe(&s);
    assert!(step.restart && !step.locked);
    assert_eq!(step.progress.waiting_for, Some(GateReason::MicDisconnected));
    s.elapsed = GATE_TIMEOUT_SETTLE;
    assert_eq!(gate.observe(&s).failed.map(|f| f.kind), Some(RefusalKind::MicLost));

    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    let mut s = clean(1000, GATE_MIN_PERIODS, 0.2);
    s.reconnected = true;
    let step = gate.observe(&s);
    assert_eq!(step.progress.waiting_for, Some(GateReason::MicReconnected));
    assert!(step.progress.message.contains("timing reference"), "{}", step.progress.message);
}

#[test]
fn an_unstable_amplitude_blocks_the_lock_even_when_the_estimator_is_happy() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    gate.observe(&clean(2000, 1, 0.20));
    gate.observe(&clean(4000, 2, 0.21));
    // +6 dB in one period: the other speaker's mute had not settled.
    let step = gate.observe(&clean(6000, GATE_MIN_PERIODS, 0.42));
    assert!(step.restart, "a 6 dB jump must not be measured");
    assert_eq!(step.progress.waiting_for, Some(GateReason::UnstableAmplitude));
    assert!(step.progress.message.contains("dB"));
}

#[test]
fn a_monotonic_decay_is_reported_as_the_aec_signature() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    // −0.9 dB per period, three periods: inside the 3 dB spread tolerance, so
    // only the *direction* gives it away (plan §4.2).
    gate.observe(&clean(2000, 1, 0.300));
    gate.observe(&clean(4000, 2, 0.270));
    let step = gate.observe(&clean(6000, 3, 0.243));
    assert_eq!(step.progress.waiting_for, Some(GateReason::AecSuspected));
    assert!(gate.aec_suspected());
    assert!(step.restart);
    assert!(amplitude_spread_db(&[0.300, 0.270, 0.243]).unwrap() < GATE_AMP_TOL_DB, "the spread alone must not catch this");
}

/// Plan §12.3's whole reason for existing: a barge-in announcement changes the
/// level on the member being measured, and *without* this the amplitude-stability
/// check catches it and blames the user's hand. The cause must win, and it must be
/// the reason the gate reports when it eventually gives up.
#[test]
fn interference_is_blamed_on_the_announcement_not_the_user() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    let reason = "an urgent announcement (#7) played on 'sendspin-dev-kitchen' during the measurement".to_string();

    // A level jump big enough that the amplitude check *would* fire — but the
    // announcement that caused it is known, so that is what gets reported.
    gate.observe(&clean(2000, 1, 0.30));
    let mut s = clean(4000, 2, 0.60);
    s.interference = Some(reason.clone());
    let step = gate.observe(&s);
    assert!(step.restart, "the window spanning an announcement must be discarded");
    assert_eq!(step.progress.waiting_for, Some(GateReason::Interference));
    assert_eq!(step.progress.message, reason, "the cause must be quoted verbatim");
    assert!(!step.progress.message.contains("hold it still"));

    // And the timeout inherits it, so the refusal names the doorbell rather than
    // degrading into a generic gate timeout.
    let mut late = clean(60_000, 0, 0.60);
    late.interference = Some(reason.clone());
    let failed = gate.observe(&late).failed.expect("the gate must give up eventually");
    assert_eq!(failed.kind, RefusalKind::Interference);
    assert!(failed.message.contains("announcement"), "{}", failed.message);
}

#[test]
fn silence_says_no_tone_arrived_rather_than_unstable() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    let step = gate.observe(&clean(2000, 1, 0.0));
    assert_eq!(step.progress.waiting_for, Some(GateReason::Silent));
    assert!(step.progress.message.contains("no tone"), "{}", step.progress.message);
}

/// A speaker whose stream keeps breaking up must be diagnosed as a stream
/// fault, not as a moving phone. Hardware-observed on 2026-08-11: a wedged
/// sendspin device renders intermittently until it is reconnected, and the
/// amplitude-spread check would otherwise blame the user's hand.
#[test]
fn an_intermittent_stream_is_named_as_a_stream_fault_not_a_moving_phone() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    // Heard, lost, heard, lost: the signature of a discontinuous stream.
    assert_eq!(gate.observe(&clean(2000, 1, 0.30)).progress.waiting_for, Some(GateReason::Acquiring));
    let first = gate.observe(&clean(4000, 2, 0.0));
    assert_eq!(first.progress.waiting_for, Some(GateReason::Silent), "one dropout can still be a mute settling");
    assert_eq!(gate.observe(&clean(6000, 1, 0.30)).progress.waiting_for, Some(GateReason::Acquiring));
    let step = gate.observe(&clean(8000, 2, 0.0));
    assert_eq!(step.progress.waiting_for, Some(GateReason::Intermittent));
    // The advice must point at the speaker, and name the remedy that works.
    assert!(step.progress.message.contains("not continuous"), "{}", step.progress.message);
    assert!(step.progress.message.contains("static delay"), "{}", step.progress.message);
    assert!(!step.progress.message.contains("hold it still"), "{}", step.progress.message);
}

/// The counterpart: a speaker that never made a sound must stay `Silent`, so
/// "muted or still reconnecting" is not mislabelled as a broken stream.
#[test]
fn a_speaker_that_never_sounded_stays_silent_not_intermittent() {
    let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
    for i in 0..4 {
        let step = gate.observe(&clean(2000 * (i + 1), 1, 0.0));
        assert_eq!(step.progress.waiting_for, Some(GateReason::Silent), "period {i}");
    }
}

#[test]
fn the_reconnect_gate_waits_tens_of_seconds_not_five() {
    // Plan §2.3 is the single most important constraint on the orchestration:
    // a reconnecting sendspin device takes tens of seconds to render again.
    assert!(GATE_TIMEOUT_RECONNECT >= Duration::from_secs(120));
    let mut gate = Gate::new(GateConfig::reconnect(&Timing::real()));
    let mut s = clean(60_000, 0, 0.0);
    s.peak = 0.0;
    assert!(gate.observe(&s).failed.is_none(), "a minute of silence after a write is expected, not a failure");
}
