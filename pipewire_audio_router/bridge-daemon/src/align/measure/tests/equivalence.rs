//! The relay-vs-device experiment: does a relay-side delay measure the same as the
//! device's own knob?
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

/// The headline case: both arms measured on one speaker, and the answer is a number
/// with an uncertainty rather than a boolean.
#[tokio::test(start_paused = true)]
async fn both_arms_are_measured_and_the_result_is_a_number_with_a_bound() {
    let rig = equiv_rig(&[("ap2-dev-other", MemberKind::Airplay2, 0), ("spk", MemberKind::Sendspin, 0)], "spk", EquivInject::default());
    let (s, writer, relay) = run_equiv(rig).await;
    assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
    assert_eq!(s.steps_done, EQUIV_STEPS);
    let r = report_of(&s);

    // The relay arm: a 20 ms line moved it 20 ms later, and the line reports the
    // sample-exact figure rather than what was asked for.
    assert_eq!(r.relay.applied_ms, 20.0);
    assert!((r.relay.shift_ms - 20.0).abs() < 0.2, "relay arm: {} ms", r.relay.shift_ms);
    assert!(r.relay.writes.is_empty(), "the relay arm must cost no reconnect at all");

    // The device arm: a 20 ms advance moved it 20 ms EARLIER (§2.4.1), which is a
    // negative shift and a positive equivalent delay.
    assert!((r.device.shift_ms + 20.0).abs() < 0.2, "device arm: {} ms", r.device.shift_ms);
    assert!((r.device_equivalent_delay_ms - 20.0).abs() < 0.2);
    assert_eq!(r.polarity_assumed, KnobPolarity::Advance);
    assert_eq!(r.polarity_observed, Some(KnobPolarity::Advance), "the firmware's sign is confirmed, not assumed");

    // …and the comparison is reported as a bounded claim.
    assert_eq!(r.verdict, EquivalenceVerdict::WithinResolution);
    assert!(r.discrepancy_ms.abs() < 0.5, "{:+} ms", r.discrepancy_ms);
    assert!(r.uncertainty_ms > 0.0 && r.uncertainty_ms < 0.5, "1σ = {} ms", r.uncertainty_ms);
    assert!(r.resolution_ms >= EQUIV_MIN_MEANINGFUL_MS, "the claim is never finer than the knob's own granularity");
    assert!(r.headline.contains("no difference beyond"), "{}", r.headline);
    assert!(r.headline.contains("one speaker of one transport"), "the scope travels with the claim: {}", r.headline);
    assert!((r.scale.expect("a scale") - 1.0).abs() < 0.05, "scale {:?}", r.scale);

    // Three writes: from → to → from. The last one leaves the knob where the user
    // had it, so the happy path needs no restoring write.
    assert_eq!(writer.writes.lock_recover().clone(), vec![("spk".into(), 0), ("spk".into(), 20), ("spk".into(), 0)]);
    assert_eq!(r.reconnects, 3);
    assert_eq!(r.plan.member, "spk", "the sendspin member is chosen, and only one member is used");
    assert!(r.plan.why_member.contains("only *advance*"), "{}", r.plan.why_member);
    assert!(r.plan.why_step.contains("one wire-codec frame"), "{}", r.plan.why_step);

    // State restored: no line left applied, knob back where it started.
    let restore = s.restore.expect("a restore report");
    assert!(restore.relay_cleared && restore.failures.is_empty(), "{restore:?}");
    assert!(!restore.knob_rewritten, "the step's own last write already put it back");
    assert_eq!(restore.knob_left_at_ms, Some(0));
    assert_eq!(relay.applied_ms("spk"), 0.0, "the provisional delay is not the user's and must not outlive the run");

    // What it cannot tell you travels with the numbers.
    assert_eq!(r.cannot_tell.len(), 6);
    assert!(r.cannot_tell.iter().any(|c| c.contains("ONE speaker of ONE transport")));
    assert!(r.cannot_tell.iter().any(|c| c.contains("cannot see a *constant* difference")));
    // A clean run has nothing to caveat: the line did what it said, and every write
    // reported that it was reconnecting the speaker.
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    assert!(r.device.writes.len() == 3 && r.device.writes.iter().all(|m| m.contains("reconnecting")), "{:?}", r.device.writes);
}

/// The interesting failure: the knob moves the speaker by a *different amount* than
/// the delay line. It must come out as both numbers and a factor — and nothing may
/// quietly divide by it.
#[tokio::test(start_paused = true)]
async fn a_scale_disagreement_is_reported_with_both_numbers_and_never_applied() {
    let inject = EquivInject { device_per_ms: -1.15, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
    let (s, writer, _relay) = run_equiv(rig).await;
    assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
    let r = report_of(&s);
    assert_eq!(r.verdict, EquivalenceVerdict::ScaleDisagrees);
    assert!((r.relay.shift_ms - 20.0).abs() < 0.2, "the relay arm is the reference: {} ms", r.relay.shift_ms);
    assert!((r.device_equivalent_delay_ms - 23.0).abs() < 0.3, "device arm: {} ms", r.device_equivalent_delay_ms);
    assert!((r.discrepancy_ms - 3.0).abs() < 0.3, "{:+} ms", r.discrepancy_ms);
    assert!(r.discrepancy_ms.abs() > r.resolution_ms, "3 ms must clear the resolution, or this test proves nothing");
    assert!((r.scale.expect("a scale") - 1.15).abs() < 0.02, "scale {:?}", r.scale);
    // Both numbers, in the sentence a user reads.
    assert!(r.headline.contains("NOT interchangeable"), "{}", r.headline);
    assert!(r.headline.contains("20.0") && r.headline.contains("23."), "{}", r.headline);
    // The correction is stated and disowned in the same breath.
    assert!(r.implied_correction.contains("NOT applied"), "{}", r.implied_correction);
    assert!(r.implied_correction.contains("0.870"), "the factor is spelled out: {}", r.implied_correction);
    // Nothing was written except the experiment's own three steps: a discrepancy is a
    // finding, not a repair.
    assert_eq!(writer.writes.lock_recover().len(), 3);
    assert_eq!(s.restore.expect("a restore").knob_left_at_ms, Some(0));
}

/// The finding §2.4.1 says would be far more serious than an offset: the firmware
/// disagreeing with the polarity the solver assumes.
#[tokio::test(start_paused = true)]
async fn a_knob_that_moves_the_sound_the_wrong_way_is_called_a_sign_inversion() {
    // +1.0: raising the knob makes the speaker play LATER, i.e. it is a delay, not
    // the advance the solver models.
    let inject = EquivInject { device_per_ms: 1.0, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
    let (s, _writer, _relay) = run_equiv(rig).await;
    assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
    let r = report_of(&s);
    assert_eq!(r.verdict, EquivalenceVerdict::SignInverted);
    assert_eq!(r.polarity_assumed, KnobPolarity::Advance);
    assert_eq!(r.polarity_observed, Some(KnobPolarity::Delay), "the measurement, not the assumption, decides");
    assert!((r.device.shift_ms - 20.0).abs() < 0.2, "{} ms", r.device.shift_ms);
    // Expressed as a delay-equivalent it is *negative*, which is what makes every
    // proposal for this kind inverted rather than merely offset.
    assert!((r.device_equivalent_delay_ms + 20.0).abs() < 0.2);
    assert!((r.discrepancy_ms + 40.0).abs() < 0.5, "{:+} ms", r.discrepancy_ms);
    assert!(r.headline.contains("WRONG WAY") && r.headline.contains("inverted"), "{}", r.headline);
    assert!(r.headline.contains("do not write"), "{}", r.headline);
    assert!(r.implied_correction.contains("knob_of"), "the fix is a code change, not a factor: {}", r.implied_correction);
    // The relay arm is still reported, so the reader can see which half moved.
    assert!((r.relay.shift_ms - 20.0).abs() < 0.2);
}

/// Plan §1.1.2 item 3, tested: a reconnect shifts this speaker by a constant that a
/// one-reconnect comparison would have charged to the knob. The two-point device arm
/// cancels it — and therefore cannot measure a constant at all, which the report
/// says out loud.
#[tokio::test(start_paused = true)]
async fn a_constant_reconnect_offset_cancels_and_is_reported_as_epsilon() {
    let inject = EquivInject { reconnect_eps_ms: 5.0, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
    let (s, _writer, _relay) = run_equiv(rig).await;
    assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
    let r = report_of(&s);

    // The naive experiment §1.1.1 budgeted — "relay N, no reconnect" against
    // "device N, after one reconnect" — would have been 5 ms out, and here is the
    // evidence: the same speaker at the same knob value, either side of one write.
    assert!(
        (r.device.baseline_before_ms - r.relay.baseline_after_ms).abs() > 4.0,
        "the reconnect really did move it: {} → {}",
        r.relay.baseline_after_ms,
        r.device.baseline_before_ms
    );
    assert!((r.reconnect_epsilon_ms - 5.0).abs() < 0.3, "ε = {} ms", r.reconnect_epsilon_ms);

    // …and the bracketed difference is unmoved by it.
    assert_eq!(r.verdict, EquivalenceVerdict::WithinResolution);
    assert!(r.discrepancy_ms.abs() < 0.5, "{:+} ms", r.discrepancy_ms);
    assert!(r.reconnect_variation_ms < 0.5, "two identical reconnects landed the same: {} ms", r.reconnect_variation_ms);
    assert!(
        r.cannot_tell.iter().any(|c| c.contains("cancels any constant")),
        "the price of item 3 is that a constant is invisible, and that has to be stated"
    );
}

/// The correction §1.1.2 item 3 did *not* budget for: two reconnects are tens of
/// seconds apart, and a 100 ppm phone clock creeps millimetres of a millisecond per
/// second — 6 ms over a device arm, against a 20 ms step. Bracketing each arm
/// removes it; without the brackets this measurement would report a 30 % scale error
/// that does not exist.
#[tokio::test(start_paused = true)]
async fn clock_drift_across_the_reconnects_is_bracketed_out() {
    let inject = EquivInject { drift_ms_per_s: 0.1, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
    let (s, _writer, _relay) = run_equiv(rig).await;
    assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
    let r = report_of(&s);

    // The drift is real and large enough to matter: the device arm's two identical
    // readings are milliseconds apart, and it is measured as ~100 ppm on the arm that
    // has no reconnects in it.
    assert!(r.relay.span_s > 15.0, "the relay bracket has to span something: {} s", r.relay.span_s);
    assert!((r.relay.drift_ppm - 100.0).abs() < 20.0, "{} ppm", r.relay.drift_ppm);
    assert!(
        r.device.baseline_disagreement_ms.abs() > 2.0,
        "the device arm's baselines must disagree, or the bracket is not being tested: {} ms",
        r.device.baseline_disagreement_ms
    );
    // What the drift is *not* is a scale error, and the brackets are why.
    assert_eq!(r.verdict, EquivalenceVerdict::WithinResolution, "{}", r.headline);
    assert!(r.discrepancy_ms.abs() < 0.6, "{:+} ms", r.discrepancy_ms);
    assert!(r.reconnect_variation_ms < 1.0, "drift explained the disagreement: {} ms", r.reconnect_variation_ms);
}

/// A knob nothing honours is its own finding, and it is not "equivalent".
#[tokio::test(start_paused = true)]
async fn a_knob_the_device_ignores_is_named_rather_than_averaged_away() {
    let inject = EquivInject { device_per_ms: 0.0, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
    let (s, _writer, _relay) = run_equiv(rig).await;
    let r = report_of(&s);
    assert_eq!(r.verdict, EquivalenceVerdict::KnobHadNoEffect);
    assert_eq!(r.polarity_observed, None, "no shift means no direction — that is not a sign, it is an absence");
    assert!(r.headline.contains("did not act on it"), "{}", r.headline);
    assert!(r.implied_correction.contains("no factor turns zero into a delay"), "{}", r.implied_correction);
    // The relay arm still worked, which is what makes this a device finding.
    assert!((r.relay.shift_ms - 20.0).abs() < 0.2);
}

/// The other half's failure, and it must not be charged to the device: if the delay
/// line produces no shift, the *provisional* half of the deferred-write scheme is
/// broken and the knob is beside the point.
#[tokio::test(start_paused = true)]
async fn a_delay_line_that_does_nothing_is_blamed_on_the_line_not_the_knob() {
    let inject = EquivInject { relay_per_ms: 0.0, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
    let (s, _writer, _relay) = run_equiv(rig).await;
    let r = report_of(&s);
    assert_eq!(r.verdict, EquivalenceVerdict::RelayLineHadNoEffect);
    assert!(r.relay.shift_ms.abs() < 0.5, "{} ms", r.relay.shift_ms);
    // The knob was fine, and is still reported — but no factor is offered, because
    // there is nothing to divide by.
    assert!((r.device_equivalent_delay_ms - 20.0).abs() < 0.3);
    assert_eq!(r.scale, None, "a NaN here would break the status serialisation, never mind the arithmetic");
    assert!(r.headline.contains("*delay line* is what failed"), "{}", r.headline);
    assert!(r.implied_correction.contains("fix the delay line"), "{}", r.implied_correction);
    // And the whole status still serialises, which is the reason `scale` is optional.
    serde_json::to_string(&s).expect("the status must serialise whatever the verdict");
}

/// A gate that never locks must refuse, not guess. Here the speaker is wedged from
/// the start (plan §2.3.2), so the very first reading fails — before a single
/// reconnect has been spent, which is why the relay arm goes first.
///
/// Only the gate *timeouts* are shortened: the test has to synthesise every sample
/// of the wait, and 45 s of silence at 48 kHz proves nothing that 20 s does not.
#[tokio::test(start_paused = true)]
async fn a_gate_that_never_locks_refuses_before_a_reconnect_is_spent() {
    let inject = EquivInject { silent_after_writes: Some(0), short_timeouts: true, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
    let (s, writer, relay) = run_equiv(rig).await;
    assert_eq!(s.phase, EquivPhase::Refused);
    let refusal = s.refusal.expect("a refusal");
    assert_eq!(refusal.kind, RefusalKind::GateTimeout);
    assert_eq!(refusal.member.as_deref(), Some("spk"));
    assert!(refusal.message.contains("no tone from this speaker"), "{}", refusal.message);
    assert!(s.report.is_none(), "a refused experiment must not report a verdict");
    assert!(writer.writes.lock_recover().is_empty(), "the relay arm is first precisely so a failure costs no reconnect");
    assert_eq!(relay.applied_ms("spk"), 0.0);
    let restore = s.restore.expect("a restore report even on refusal");
    assert!(restore.relay_cleared && restore.failures.is_empty());
    assert_eq!(restore.knob_left_at_ms, None, "no knob was ever written");
}

/// The same, but after the device arm has already written the step: the knob is at a
/// value the user did not choose, so the refusal has to pay for one more reconnect
/// to put it back.
#[tokio::test(start_paused = true)]
async fn a_refusal_after_the_step_was_written_puts_the_knob_back() {
    // Silent once two writes have landed, i.e. exactly when the stepped value is on
    // the device.
    let inject = EquivInject { silent_after_writes: Some(2), short_timeouts: true, ..EquivInject::default() };
    let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 7)], "spk", inject);
    let (s, writer, relay) = run_equiv(rig).await;
    assert_eq!(s.phase, EquivPhase::Refused);
    assert_eq!(s.refusal.expect("a refusal").kind, RefusalKind::GateTimeout);
    assert!(s.report.is_none());
    let restore = s.restore.expect("a restore report");
    assert!(restore.knob_rewritten, "the run stopped with the step applied, so it owes a write back");
    assert_eq!(restore.knob_left_at_ms, Some(7), "back to the value the user had, not to zero");
    assert!(restore.failures.is_empty(), "{restore:?}");
    assert!(restore.message.contains("one more reconnect"), "the extra cost is stated: {}", restore.message);
    // 7 → 27 → (refused) → 7.
    assert_eq!(writer.writes.lock_recover().clone(), vec![("spk".into(), 7), ("spk".into(), 27), ("spk".into(), 7)]);
    assert_eq!(relay.applied_ms("spk"), 0.0);
}

/// Abandoning is not a licence to leave a speaker 20 ms out: the run still finishes
/// its restore, and the status says where the knob ended up.
#[tokio::test(start_paused = true)]
async fn abandoning_still_puts_the_borrowed_delay_back() {
    let rig = equiv_rig(&[("ap2-dev-other", MemberKind::Airplay2, 0), ("spk", MemberKind::Sendspin, 12)], "spk", EquivInject::default());
    let EquivRig { deps, relay, writer } = rig;
    let m = EquivalenceManager { st: EquivState::new() };
    m.start(deps).await.expect("the experiment must start");

    // Wait until the stepped value is actually on the device, so what is being tested
    // is a cancellation that owes a write back.
    for _ in 0..40_000u32 {
        if writer.last("spk") == Some(32) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(writer.last("spk"), Some(32), "the run never reached the stepped write");
    let cancelling = m.abandon();
    assert!(cancelling.message.contains("putting the borrowed delay back"), "{}", cancelling.message);

    let s = loop {
        let s = m.status();
        if s.phase == EquivPhase::Refused && s.restore.is_some() {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(s.refusal.expect("a refusal").kind, RefusalKind::Cancelled);
    assert!(s.report.is_none(), "an abandoned experiment has no verdict");
    let restore = s.restore.expect("a restore report");
    assert!(restore.knob_rewritten && restore.failures.is_empty(), "{restore:?}");
    assert_eq!(restore.knob_left_at_ms, Some(12));
    assert_eq!(writer.last("spk"), Some(12), "the user's own value is what the speaker is left at");
    assert!(restore.relay_cleared);
    assert_eq!(relay.applied_ms("spk"), 0.0);
    assert!(s.message.contains("abandoned;"), "{}", s.message);
}

/// The step is one wire-codec frame, and that is what set it — asserted against the
/// codec rather than restated, so a codec change cannot quietly reintroduce
/// §1.1.2 item 2's window-phase confound.
#[test]
fn the_step_is_exactly_one_wire_codec_frame() {
    assert_eq!(EQUIV_STEP_MS, 20);
    assert_eq!(usize::from(EQUIV_STEP_MS) * 48, crate::outputs::sendspin::codec::OPUS_FRAME_FRAMES);
    // And it dwarfs the estimator by the margins §5.4.1 measured.
    assert!(f64::from(EQUIV_STEP_MS) > 100.0 * 0.14, "100× the worst accepted delta error");
    assert!(f64::from(EQUIV_STEP_MS) > 10.0 * REPEATABILITY_TOL_MS);
}

#[test]
fn the_member_is_chosen_by_transport_and_the_choice_is_explained() {
    let members =
        equiv_members(&[("ap2-dev-x", MemberKind::Airplay2), ("pwsink-dev-y", MemberKind::PwSink), ("spk", MemberKind::Sendspin)]);
    let current: HashMap<String, u16> = HashMap::new();
    let ctx = SendAheadContext::default();
    let p = plan_equivalence(&members, &current, &ctx, None).expect("a plan");
    assert_eq!(p.member, "spk", "sendspin first: it is the only advance, so it is the only place the sign can be confirmed");
    assert_eq!(p.delta_ms, i32::from(EQUIV_STEP_MS));
    assert_eq!((p.from_ms, p.to_ms), (0, EQUIV_STEP_MS));
    assert!(p.why_member.contains("2 other member(s) were not used"), "{}", p.why_member);
    assert!(p.why_member.contains("property of the transport"), "{}", p.why_member);

    // No sendspin member: AP2 next, and its live-push caveat is stated.
    let members = equiv_members(&[("pwsink-dev-y", MemberKind::PwSink), ("ap2-dev-x", MemberKind::Airplay2)]);
    let p = plan_equivalence(&members, &current, &ctx, None).expect("a plan");
    assert_eq!(p.member, "ap2-dev-x");
    assert!(p.why_member.contains("pushes it *live*"), "{}", p.why_member);

    // pw-sink last, and its baseline is the floor rather than zero.
    let members = equiv_members(&[("pwsink-dev-y", MemberKind::PwSink)]);
    let current: HashMap<String, u16> = [("pwsink-dev-y".to_string(), 0u16)].into_iter().collect();
    let p = plan_equivalence(&members, &current, &ctx, None).expect("a plan");
    assert_eq!(p.from_ms, crate::routing::sync_settings::PWSINK_JITTER_MIN_MS, "a pw-sink knob cannot sit below its floor");
    assert_eq!(p.to_ms, crate::routing::sync_settings::PWSINK_JITTER_MIN_MS + EQUIV_STEP_MS);
    assert_eq!(p.stored_ms, 0, "…but what the restore writes back is what was stored, not the floor it had to start from");

    // An explicit override is honoured; an unknown name is refused rather than
    // silently replaced by the automatic choice.
    let members = equiv_members(&[("spk", MemberKind::Sendspin), ("ap2-dev-x", MemberKind::Airplay2)]);
    let current: HashMap<String, u16> = HashMap::new();
    assert_eq!(plan_equivalence(&members, &current, &ctx, Some("ap2-dev-x")).expect("a plan").member, "ap2-dev-x");
    assert!(plan_equivalence(&members, &current, &ctx, Some("nope")).is_err());
    assert!(plan_equivalence(&[], &current, &ctx, None).is_err());
}

/// Plan §9.2 is an upper bound on the step, and it is checked against the real
/// numbers: a step that lifted the group's send-ahead high-water mark would silence
/// every speaker in the group to measure one of them. It refuses rather than
/// shrinking the step, because a smaller step gives up the codec-frame property.
#[test]
fn a_step_that_would_lift_the_send_ahead_mark_is_refused_not_shrunk() {
    let members = equiv_members(&[("spk", MemberKind::Sendspin)]);
    let current: HashMap<String, u16> = [("spk".to_string(), 0u16)].into_iter().collect();
    // The mark is the max over members of `min_buffer + advance`, floored by the
    // group lead. With 200 ms of lead and a 190 ms buffer there are 10 ms of room —
    // not 20.
    let tight = SendAheadContext {
        floor_ms: 200,
        unreported_floor_ms: 40,
        min_buffer_ms: [("spk".to_string(), Some(190u32))].into_iter().collect(),
    };
    let r = plan_equivalence(&members, &current, &tight, None).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::KnobRange);
    assert!(r.message.contains("high-water mark from 200 ms to 210 ms"), "{}", r.message);
    assert!(r.message.contains("silence the whole group"), "{}", r.message);

    // 40 ms of room: the same step now fits, and nothing is refused.
    let roomy = SendAheadContext { min_buffer_ms: [("spk".to_string(), Some(160u32))].into_iter().collect(), ..tight.clone() };
    let p = plan_equivalence(&members, &current, &roomy, None).expect("a plan");
    assert_eq!(p.to_ms, EQUIV_STEP_MS);

    // A delay-polarity knob never feeds that mark (§1.1.2's asymmetry), so the same
    // tight group is fine on an AP2 member.
    let ap2 = equiv_members(&[("ap2-dev-x", MemberKind::Airplay2)]);
    assert!(plan_equivalence(&ap2, &HashMap::new(), &tight, None).is_ok());
}

#[test]
fn a_knob_with_no_headroom_steps_downwards_instead_of_refusing() {
    // An AP2 member already at its ceiling: the step goes the other way, and the
    // comparison normalises the sign back (see `equiv_compare`).
    let members = equiv_members(&[("ap2-dev-x", MemberKind::Airplay2)]);
    let max = crate::outputs::ap2::server::AP2_RENDER_DELAY_MAX_MS;
    let current: HashMap<String, u16> = [("ap2-dev-x".to_string(), max)].into_iter().collect();
    let p = plan_equivalence(&members, &current, &SendAheadContext::default(), None).expect("a plan");
    assert_eq!((p.from_ms, p.to_ms, p.delta_ms), (max, max - EQUIV_STEP_MS, -i32::from(EQUIV_STEP_MS)));
}
