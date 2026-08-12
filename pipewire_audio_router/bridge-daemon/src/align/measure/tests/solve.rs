//! Turning observations into a proposal: drift fitting, transitivity, repeatability,
//! and the residual check.
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

#[test]
fn observations_from_two_captures_are_never_compared() {
    let members = [member("a"), member("b")];
    let mut o = vec![obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 4.0, 110.0, 0.0)];
    o[1].grid_epoch = 1;
    let r = solve_of(&members, &o, &[]).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::MicReconnected);
}

#[test]
fn a_common_clock_drift_is_fitted_out_of_the_offsets() {
    // 100 ppm on a 2 s pattern = 0.2 ms of phase per period. Measured
    // alternately (a,b then b,a) the raw phases move, but the *offsets* must
    // not: this is what stops a phone's clock from being written into the
    // speakers as a delay.
    let members = [member("a"), member("b")];
    let drift = 0.2;
    let truth_b = 6.0;
    let o = [
        obs("a", 0, 0.0, 300.0, 0.0),
        obs("b", 0, 5.0, 300.0 + truth_b + 5.0 * drift, 0.0),
        obs("b", 1, 10.0, 300.0 + truth_b + 10.0 * drift, 0.0),
        obs("a", 1, 15.0, 300.0 + 15.0 * drift, 0.0),
    ];
    let fit = fit_drift(&o, 2000.0, |o| o.m.phase_a_ms);
    assert!((fit.slope_ms_per_period - drift).abs() < 1e-6, "slope {}", fit.slope_ms_per_period);
    assert!((fit.drift_ppm(2000.0) - 100.0).abs() < 0.01, "ppm {}", fit.drift_ppm(2000.0));
    let p = solve_of(&members, &o, &[]).expect("accepted");
    let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
    assert_eq!(p.reference, "a", "a arrives first, so it is the one left at knob zero");
    assert_eq!(by("b").new_delay_ms, 6, "the drift must not become a knob value");
    assert_eq!(by("a").new_delay_ms, 0);
    assert!((p.drift_ppm - 100.0).abs() < 0.01);
}

#[test]
fn a_single_pass_reports_that_no_drift_could_be_fitted() {
    let members = [member("a"), member("b")];
    let o = [obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 4.0, 103.0, 0.0)];
    let p = solve_of(&members, &o, &[]).expect("accepted");
    assert!(!p.warnings.is_empty());
    assert!(p.warnings.iter().any(|w| w.kind == WarningKind::NoDriftFit));
    assert!(p.checks.repeatability.is_none(), "one pass cannot be repeatable");
}

#[test]
fn a_per_speaker_band_bias_blocks_the_write() {
    // Plan §5.6's blind spot, made visible: one speaker's arrival is pulled by
    // an early reflection, which biases 1.5 kHz and 3 kHz differently. Every
    // other metric still looks excellent — this is the only check that can see
    // it, and it must BLOCK, not warn (plan §10.2).
    let members = [member("a"), member("b"), member("c")];
    let o = [
        obs("a", 0, 0.0, 300.0, 0.0),
        obs("b", 0, 4.0, 306.0, 4.5), // biased speaker
        obs("c", 0, 8.0, 303.0, 0.0),
        obs("a", 1, 12.0, 300.0, 0.0),
        obs("b", 1, 16.0, 306.0, 4.5),
        obs("c", 1, 20.0, 303.0, 0.0),
    ];
    let p = solve_of(&members, &o, &[]).expect("a proposal is still produced");
    assert!(!p.checks.transitivity.passed);
    assert!((p.checks.transitivity.worst_ms - 4.5).abs() < 0.01, "worst {}", p.checks.transitivity.worst_ms);
    let blocked = p.blocked.expect("a transitivity failure must block the write");
    assert_eq!(blocked.kind, RefusalKind::Transitivity);
    assert!(blocked.message.contains("nothing is written"), "{}", blocked.message);
    // The numbers stay visible next to the refusal (plan §10).
    assert_eq!(p.members.len(), 3);
}

#[test]
fn a_bias_shared_by_every_speaker_does_not_block() {
    // §5.6: "a per-speaker bias breaks transitivity; a bias shared by all
    // speakers does not" — and a shared bias is also harmless, because every
    // quantity consumed here is a difference.
    let members = [member("a"), member("b")];
    let o = [obs("a", 0, 0.0, 300.0, 6.0), obs("b", 0, 4.0, 309.0, 6.0), obs("a", 1, 8.0, 300.0, 6.0), obs("b", 1, 12.0, 309.0, 6.0)];
    let p = solve_of(&members, &o, &[]).expect("accepted");
    assert!(p.checks.transitivity.passed, "worst {}", p.checks.transitivity.worst_ms);
    assert!(p.blocked.is_none());
    // b arrives 9 ms later, so b is the one advanced (§2.4.1's inversion).
    assert_eq!(p.members.iter().find(|m| m.node_name == "b").unwrap().new_delay_ms, 9);
    assert_eq!(p.members.iter().find(|m| m.node_name == "a").unwrap().new_delay_ms, 0);
}

#[test]
fn transitivity_arithmetic_is_the_cross_band_difference() {
    // Directly, without the solve around it: the residual of a triangle closed
    // with edges from two different bands is |split_i − split_j|.
    let o = [obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 1.0, 100.0, 1.0), obs("c", 0, 2.0, 100.0, -1.0)];
    let t = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS, no_band_splits());
    assert!((t.worst_ms - 2.0).abs() < 1e-9, "worst {}", t.worst_ms);
    let pair = t.worst_pair.expect("a worst pair");
    assert!(pair == ("b".into(), "c".into()) || pair == ("c".into(), "b".into()));
    assert!(t.passed, "2 ms is inside the crossover-confound tolerance");
    assert!(t.caveat.contains("crossover"));
}

#[test]
fn a_member_that_moved_between_passes_blocks_on_repeatability() {
    let members = [member("a"), member("b")];
    let o = [
        obs("a", 0, 0.0, 300.0, 0.0),
        obs("b", 0, 4.0, 305.0, 0.0),
        obs("b", 1, 8.0, 305.0, 0.0),
        obs("a", 1, 12.0, 307.0, 0.0), // moved 7 ms between passes
    ];
    let p = solve_of(&members, &o, &[]).expect("a proposal is still produced");
    let rep = p.checks.repeatability.expect("two passes are checkable");
    assert!(!rep.passed, "worst {}", rep.worst_ms);
    assert_eq!(p.blocked.map(|b| b.kind), Some(RefusalKind::Repeatability));
}

#[test]
fn the_residual_check_measures_against_the_chosen_reference() {
    let o = [obs("a", 0, 0.0, 300.0, 0.0), obs("b", 0, 4.0, 300.4, 0.0), obs("c", 0, 8.0, 299.6, 0.0)];
    let r = residual(&o, "a", 2000.0, RESIDUAL_TOL_MS);
    assert!(r.passed, "worst {}", r.worst_ms);
    let o = [obs("a", 0, 0.0, 300.0, 0.0), obs("b", 0, 4.0, 309.0, 0.0)];
    let r = residual(&o, "a", 2000.0, RESIDUAL_TOL_MS);
    assert!(!r.passed);
    assert_eq!(r.worst_member.as_deref(), Some("b"));
    // An unknown reference is a failure, never a pass by default.
    assert!(!residual(&o, "nobody", 2000.0, RESIDUAL_TOL_MS).passed);
}
