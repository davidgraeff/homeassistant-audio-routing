//! The per-output band-split calibration, and what it does to §10.2's cross-band
//! check.
//!
//! The failure this exists for is a real one (2026-08-12): a Home Assistant Voice PE
//! and an ESPHome satellite, microphone and both speakers close together in one room,
//! refused with *"the two test tones disagree by 3.45 ms"* against a 3.0 ms limit.
//! Transitivity blocks the write, so a legitimate two-model setup could not be
//! aligned at all. The tolerance is **not** the fix — it is the only exposure this
//! design has to §5.6's reflection bias — so the hardware difference is measured and
//! subtracted instead.

use super::super::*;
use super::harness::*;

fn splits(pairs: &[(&str, f64)]) -> BandSplits {
    pairs.iter().map(|(n, v)| ((*n).to_string(), *v)).collect()
}

/// The reported numbers, reproduced: two different speaker models whose crossovers
/// disagree by 3.45 ms fail while uncalibrated, and pass once each one's own split is
/// subtracted. Nothing about the tolerance changed.
#[test]
fn a_calibrated_split_removes_a_hardware_disagreement_that_would_have_blocked_the_write() {
    let members = [member("voice-pe"), member("satellite")];
    // Same arrival difference in both bands; the models simply split differently.
    let o = [
        obs("voice-pe", 0, 0.0, 300.0, 0.30),
        obs("satellite", 0, 4.0, 306.0, 3.75),
        obs("voice-pe", 1, 8.0, 300.0, 0.30),
        obs("satellite", 1, 12.0, 306.0, 3.75),
    ];
    let uncalibrated = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS, no_band_splits());
    assert!((uncalibrated.worst_ms - 3.45).abs() < 1e-9, "worst {}", uncalibrated.worst_ms);
    assert!(!uncalibrated.passed, "this is the run that failed on hardware");
    assert!(!uncalibrated.all_calibrated);
    // And it really did block the write, which is why it had to be fixed properly.
    let blocked = solve_of(&members, &o, &[]).expect("a proposal is still produced");
    assert_eq!(blocked.blocked.as_ref().map(|b| b.kind), Some(RefusalKind::Transitivity));

    // Each speaker's own split, measured at close range and stored per output.
    let cal = splits(&[("voice-pe", 0.30), ("satellite", 3.75)]);
    let calibrated = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS, &cal);
    assert!(calibrated.passed, "residual {} ms", calibrated.worst_ms);
    assert!(calibrated.worst_ms < 1e-9, "the whole disagreement was the hardware: {}", calibrated.worst_ms);
    assert!(calibrated.all_calibrated);
    // Calibrated pairs are held to the *tighter* limit — the confound is gone, so the
    // check gets sharper rather than looser (plan §5.6.1).
    assert_eq!(calibrated.tolerance_ms, CALIBRATED_TRANSITIVITY_TOL_MS);
    const _: () = assert!(CALIBRATED_TRANSITIVITY_TOL_MS < TRANSITIVITY_TOL_MS, "calibration must sharpen the check, never loosen it");
    assert_eq!(TRANSITIVITY_TOL_MS, 3.0, "the uncalibrated tolerance is deliberately unchanged");
}

/// The other half of the same claim: subtracting a crossover must not make the check
/// blind to the thing it exists to catch. A reflection-sized residual on a calibrated
/// pair still fails — and fails at the *tighter* limit, so §5.6's measured 0.89–1.72 ms
/// biases are now inside the net rather than sailing through a 3 ms one.
#[test]
fn a_reflection_sized_residual_still_fails_after_calibration() {
    let cal = splits(&[("a", 1.0), ("b", 1.0)]);
    // §5.6 measured −1.72 ms from a 0.9× reflection at +1 ms. On top of the calibrated
    // crossover it is a residual of that size, and it must not pass.
    let o = [obs("a", 0, 0.0, 300.0, 1.0), obs("b", 0, 4.0, 305.0, 2.72)];
    let t = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS, &cal);
    assert!((t.worst_ms - 1.72).abs() < 1e-9, "worst {}", t.worst_ms);
    assert!(!t.passed, "a reflection-sized residual on calibrated speakers must block");
    // And the advice says what it now *cannot* be, rather than repeating the hardware
    // explanation that has been measured away.
    assert!(t.advice.contains("calibrated"), "{}", t.advice);
    assert!(t.advice.contains("reflection"), "{}", t.advice);
    // Below the old 3 ms limit, i.e. exactly the case that used to pass silently.
    assert!(t.worst_ms < TRANSITIVITY_TOL_MS);
}

/// Mixed calibration is the ordinary case while a user works through their speakers,
/// and it must not hold an uncalibrated speaker to the sharp limit — its crossover has
/// not been subtracted, so that difference is still legitimate.
#[test]
fn a_pair_with_only_one_side_calibrated_keeps_the_wider_tolerance() {
    let cal = splits(&[("known", 0.0)]);
    let o = [obs("known", 0, 0.0, 300.0, 0.0), obs("unknown", 0, 4.0, 305.0, 2.0)];
    let t = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS, &cal);
    assert_eq!(t.tolerance_ms, TRANSITIVITY_TOL_MS, "one calibrated side is not enough to tighten anything");
    assert!(t.passed);
    assert!(!t.all_calibrated);
    // Both members are reported either way, so which one is uncalibrated is visible.
    let known = t.splits.iter().find(|s| s.node_name == "known").unwrap();
    assert_eq!(known.calibrated_ms, Some(0.0));
    let unknown = t.splits.iter().find(|s| s.node_name == "unknown").unwrap();
    assert_eq!(unknown.calibrated_ms, None);
    assert!((unknown.residual_ms - 2.0).abs() < 1e-9, "an uncalibrated member's residual is its raw split");
}

/// With mixed calibration the largest *raw* disagreement and the *decisive* pair can
/// be different pairs, and the report has to describe the one that decided the verdict.
#[test]
fn the_reported_pair_is_the_one_that_decided_the_verdict() {
    // a/b are calibrated and disagree by 2 ms — past the 1.5 ms calibrated limit.
    // b/c disagree by 3 ms, which is *larger* but exactly on the uncalibrated limit.
    let cal = splits(&[("a", 0.0), ("b", 0.0)]);
    let o = [obs("a", 0, 0.0, 300.0, 0.0), obs("b", 0, 4.0, 305.0, 2.0), obs("c", 0, 8.0, 310.0, -1.0)];
    let t = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS, &cal);
    assert!(!t.passed);
    let pair = t.worst_pair.clone().expect("a decisive pair");
    assert!(pair == ("a".into(), "b".into()) || pair == ("b".into(), "a".into()), "{pair:?}");
    assert_eq!(t.tolerance_ms, CALIBRATED_TRANSITIVITY_TOL_MS, "the limit reported is the one applied to that pair");
    assert!((t.worst_ms - 2.0).abs() < 1e-9, "worst {}", t.worst_ms);
}

/// An uncalibrated failure must not just say "a reflection or a crossover" and leave
/// the user with no way to tell. It has to name the decisive experiment: a crossover
/// split does not change with geometry and a reflection-induced one does.
#[test]
fn an_uncalibrated_failure_names_the_decisive_test() {
    let members = [member("voice-pe"), member("satellite")];
    let o = [
        obs("voice-pe", 0, 0.0, 300.0, 0.30),
        obs("satellite", 0, 4.0, 306.0, 3.75),
        obs("voice-pe", 1, 8.0, 300.0, 0.30),
        obs("satellite", 1, 12.0, 306.0, 3.75),
    ];
    let p = solve_of(&members, &o, &[]).expect("a proposal is still produced");
    let refusal = p.blocked.expect("blocked on transitivity");
    assert_eq!(refusal.kind, RefusalKind::Transitivity);
    let m = refusal.message.to_lowercase();
    assert!(m.contains("different speaker models"), "{}", refusal.message);
    assert!(m.contains("different position"), "the decisive test has to be named: {}", refusal.message);
    assert!(m.contains("crossover") && m.contains("reflection"), "{}", refusal.message);
    // And it points at how to remove the confound rather than at how to loosen it.
    assert!(m.contains("/api/align/measure/split"), "{}", refusal.message);
    assert!(!m.contains("raise the tolerance"), "{}", refusal.message);
    // Both members are named as uncalibrated, so the user knows what to calibrate.
    assert!(m.contains("voice-pe") && m.contains("satellite"), "{}", refusal.message);
}

/// The calibrated split is reported wherever it is applied — otherwise a wrong
/// calibration silently corrects every future measurement and nothing shows it.
#[test]
fn every_member_reports_its_measured_and_residual_split() {
    let cal = splits(&[("a", 1.5)]);
    let o = [obs("a", 0, 0.0, 300.0, 2.0), obs("b", 0, 4.0, 305.0, 0.5)];
    let t = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS, &cal);
    assert_eq!(t.splits.len(), 2);
    let a = t.splits.iter().find(|s| s.node_name == "a").unwrap();
    assert!((a.measured_ms - 2.0).abs() < 1e-9);
    assert_eq!(a.calibrated_ms, Some(1.5));
    assert!((a.residual_ms - 0.5).abs() < 1e-9);
    // Serialised, because this is the distribution plan §5.6.1 says W22 must read off a
    // real run — it is evidence, and evidence that never leaves the daemon is none.
    let json = serde_json::to_value(&t).expect("serialises");
    assert!(json["splits"][0]["measured_ms"].is_number());
    assert!(json["all_calibrated"].is_boolean());
}

/// The split is computed in exactly one place, so the constant that gets stored and
/// the number it is later subtracted from cannot be derived two different ways.
#[test]
fn the_split_is_one_expression_and_wraps_symmetrically() {
    let timing = Timing::real();
    let o = obs("a", 0, 0.0, 300.0, 1.25);
    assert!((member_split_ms(&o.m, &timing) - 1.25).abs() < 1e-9);
    // A split measured across the pattern wrap is a small number, not a 2 s one.
    let wrapped = obs("a", 0, 0.0, 1999.0, -0.5);
    assert!((member_split_ms(&wrapped.m, &timing) + 0.5).abs() < 1e-9, "{}", member_split_ms(&wrapped.m, &timing));
}

/// End to end: a speaker whose crossover really does delay the low band is measured,
/// and the figure that comes back is the split it was built with.
#[tokio::test(start_paused = true)]
async fn calibrating_a_speaker_recovers_its_injected_crossover_split() {
    let rig = Rig::new(&[("a", 0.0), ("b", 4.0)], Mode::NearField, 0.0);
    rig.band_shift_ms.lock_recover().insert("a".to_string(), 2.0);
    let m = manager();
    let cal = m.calibrate_split(rig.deps, "a".to_string(), Some(30)).await.expect("a close-range reading");
    assert_eq!(cal.node_name, "a");
    assert!((cal.split_ms - 2.0).abs() < 0.2, "the injected crossover must come back: {}", cal.split_ms);
    assert!(cal.peak_snr_db > 0.0);
    assert!(cal.std_error_ms < 1.0);
    assert_eq!(cal.level, 30, "it is measured at the level the user settled on standing there");
    // The number arrives with what it cannot establish attached to it.
    assert!(cal.note.contains("close range"));
    assert!(cal.note.contains("yours to keep"));
}

/// Calibration is a *measurement*, so it is refused rather than stored when the number
/// cannot be a crossover: §5.6 measured a reflection the estimator locked onto at
/// +5.2 ms and called it excellent, and a phone held a metre away looks the same.
/// Storing that would silently corrupt every later run of that speaker.
#[tokio::test(start_paused = true)]
async fn an_implausible_split_is_refused_rather_than_stored() {
    let rig = Rig::new(&[("a", 0.0), ("b", 4.0)], Mode::NearField, 0.0);
    let too_much = MAX_PLAUSIBLE_SPLIT_MS + 2.0;
    rig.band_shift_ms.lock_recover().insert("a".to_string(), too_much);
    let m = manager();
    let r = m.calibrate_split(rig.deps, "a".to_string(), None).await.expect_err("must refuse rather than store it");
    assert_eq!(r.member.as_deref(), Some("a"));
    assert!(r.message.contains("too large to be a crossover"), "{}", r.message);
    assert!(r.message.contains("reflection"), "{}", r.message);
    assert!(r.message.contains("Nothing was stored"), "{}", r.message);
}

/// Both operations drive the same session's audibility, so they cannot overlap.
#[tokio::test(start_paused = true)]
async fn a_calibration_refuses_while_a_run_is_live() {
    let rig = Rig::new(&[("a", 0.0), ("b", 4.0)], Mode::SweetSpot, 0.0);
    let second = Rig::new(&[("a", 0.0), ("b", 4.0)], Mode::SweetSpot, 0.0);
    let m = manager();
    m.start(rig.deps).await.expect("starts");
    let r = m.calibrate_split(second.deps, "a".to_string(), None).await.expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::Internal);
    assert!(r.message.contains("abandon"), "{}", r.message);
    m.abandon().await;
}

/// A calibration needs the session that is holding the speaker — there is nothing to
/// measure otherwise, and refusing says so instead of timing out.
#[tokio::test(start_paused = true)]
async fn calibrating_a_speaker_the_session_is_not_holding_refuses() {
    let rig = Rig::new(&[("a", 0.0), ("b", 4.0)], Mode::NearField, 0.0);
    let m = manager();
    let r = m.calibrate_split(rig.deps, "somewhere-else".to_string(), None).await.expect_err("must refuse");
    assert!(r.message.contains("not a member"), "{}", r.message);
    assert_eq!(r.member.as_deref(), Some("somewhere-else"));
}
