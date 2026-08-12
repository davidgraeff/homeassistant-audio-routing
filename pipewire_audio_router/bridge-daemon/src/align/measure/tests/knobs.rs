//! The interval model: which knob each member exposes, and the one target value
//! that satisfies all of them at once.
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

/// **The W14 inversion, and the assertion most likely to regress.** Before
/// §2.4.1 this group was aligned to `b`, the *latest*-arriving member, by
/// delaying `a` and `c` towards it. A sendspin knob is an advance, so the group
/// is aligned to `a`, the *earliest*, and `b` and `c` are advanced to meet it —
/// the same relative geometry, mirrored knobs, and less latency rather than
/// more.
#[test]
fn a_sendspin_only_group_aligns_to_the_earliest_member_and_advances_the_rest() {
    let members = [member("a"), member("b"), member("c")];
    // b arrives 12 ms after a, c 5 ms after a.
    let o = [
        obs("a", 0, 0.0, 300.0, 0.0),
        obs("b", 0, 4.0, 312.0, 0.0),
        obs("c", 0, 8.0, 305.0, 0.0),
        obs("a", 1, 12.0, 300.0, 0.0),
        obs("b", 1, 16.0, 312.0, 0.0),
        obs("c", 1, 20.0, 305.0, 0.0),
    ];
    let p = solve_of(&members, &o, &[]).expect("accepted");
    let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
    // T = min_i(τ_i + a_i) = 0: the earliest intrinsic arrival takes advance 0.
    assert!((p.target_ms - 0.0).abs() < 0.01, "target {}", p.target_ms);
    assert_eq!(by("a").new_delay_ms, 0);
    assert_eq!(by("b").new_delay_ms, 12);
    assert_eq!(by("c").new_delay_ms, 5);
    assert_eq!(p.reference, "a", "the member left at knob zero is the earliest, not the latest");
    assert!(by("a").is_reference && !by("b").is_reference);
    assert_eq!(p.largest_knob_ms, 12);
    assert!((p.spread_ms - 12.0).abs() < 0.01, "spread {}", p.spread_ms);
    // Every knob is an advance, and it says so in words the UI can show.
    assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
    assert!(by("b").effect.contains("advance 12 ms"), "{}", by("b").effect);
    assert!(by("b").effect.contains("earlier"), "{}", by("b").effect);
    assert!(!by("b").effect.contains("delay"), "a sendspin knob must never be called a delay: {}", by("b").effect);
}

/// The same geometry seen through [`choose_target`] alone: no member can be
/// placed later than its own intrinsic arrival, so the intersection's ceiling is
/// the earliest of them and that is where the target lands.
#[test]
fn the_advance_only_intersection_is_capped_by_the_earliest_intrinsic_arrival() {
    let ivs = [
        MemberInterval::new("a".into(), MemberKind::Sendspin, 0, 0.0),
        MemberInterval::new("b".into(), MemberKind::Sendspin, 0, 12.0),
        // Already carrying 30 ms of advance: it *can* be moved later, up to 30 ms
        // past where it arrives now, because lowering the knob gives that back.
        MemberInterval::new("c".into(), MemberKind::Sendspin, 30, 5.0),
    ];
    assert_eq!(ivs[2].base_ms, 35.0, "the intrinsic arrival includes the advance already applied");
    assert_eq!(ivs[2].hi_ms, 35.0);
    assert_eq!(ivs[0].lo_ms, -f64::from(SENDSPIN_ADVANCE_MAX_MS));
    let sol = choose_target(&ivs).expect("feasible");
    assert!((sol.hi_ms - 0.0).abs() < 1e-9, "hi {}", sol.hi_ms);
    assert!((sol.target_ms - 0.0).abs() < 1e-9, "target {}", sol.target_ms);
    assert!((sol.largest_knob_ms - 35.0).abs() < 1e-9, "largest {}", sol.largest_knob_ms);
}

#[test]
fn existing_knob_values_are_folded_in_and_the_largest_one_is_kept_as_small_as_possible() {
    // Plan §9.2, generalised by §2.4.2: a common shift changes no relative
    // timing, so the *largest* knob should be as small as possible.
    let members = [member("a"), member("b")];
    // Both arrive together *because* a already carries 40 ms of advance and b
    // 10 ms; the measurement therefore says "leave the difference alone, but give
    // the common 10 ms back".
    let o = [obs("a", 0, 0.0, 500.0, 0.0), obs("b", 0, 4.0, 500.0, 0.0), obs("a", 1, 8.0, 500.0, 0.0), obs("b", 1, 12.0, 500.0, 0.0)];
    let p = solve_of(&members, &o, &[("a", 40), ("b", 10)]).expect("accepted");
    let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
    assert_eq!(by("a").new_delay_ms, 30);
    assert_eq!(by("b").new_delay_ms, 0);
    assert_eq!(by("a").added_ms, -10);
    assert_eq!(p.largest_knob_ms, 30);
    // The group ends up playing 10 ms *later* than it does now, because b's
    // advance was the only thing holding it early.
    assert!((p.target_ms - 10.0).abs() < 0.01, "target {}", p.target_ms);
    assert!(by("a").effect.contains("later"), "lowering an advance plays later: {}", by("a").effect);
}

#[test]
fn a_spread_near_half_a_period_is_refused_rather_than_wrapped() {
    let members = [member("a"), member("b")];
    let o = [obs("a", 0, 0.0, 0.0, 0.0), obs("b", 0, 4.0, 900.0, 0.0), obs("a", 1, 8.0, 0.0, 0.0), obs("b", 1, 12.0, 900.0, 0.0)];
    let r = solve_of(&members, &o, &[]).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::AmbiguousSpread);
    assert!(r.message.contains("wrap"), "{}", r.message);
}

#[test]
fn a_member_that_was_never_measured_is_not_silently_left_out() {
    let members = [member("a"), member("b"), member("ghost")];
    let o = [obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 4.0, 110.0, 0.0)];
    let r = solve_of(&members, &o, &[]).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::Internal);
    assert_eq!(r.member.as_deref(), Some("ghost"));
}

/// A mixed group meets **in the middle**, which the old reference-member solver
/// could not express: the sendspin member is advanced and the AP2 member delayed
/// at the same time, and the target that minimises the larger of the two knobs
/// sits halfway between their intrinsic arrivals.
#[test]
fn a_mixed_group_converges_from_both_directions_at_once() {
    let members = [SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }, member("s")];
    // The AP2 member arrives first but already carries 1500 ms of render delay,
    // so its *intrinsic* arrival is 1500 ms earlier than the sendspin member's.
    let o = [
        obs("ap2-dev-x", 0, 0.0, 300.0, 0.0),
        obs("s", 0, 4.0, 900.0, 0.0),
        obs("ap2-dev-x", 1, 8.0, 300.0, 0.0),
        obs("s", 1, 12.0, 900.0, 0.0),
    ];
    let p = solve_of(&members, &o, &[("ap2-dev-x", 1500)]).expect("feasible from both sides");
    let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
    assert_eq!(by("ap2-dev-x").polarity, KnobPolarity::Delay);
    assert_eq!(by("s").polarity, KnobPolarity::Advance);
    // Intrinsic arrivals: ap2 at −1500, s at +600. Halfway is −450, and both
    // knobs then hold 1050 ms — the smallest possible largest knob.
    assert!((p.target_ms + 450.0).abs() < 0.01, "target {}", p.target_ms);
    assert_eq!(by("ap2-dev-x").new_delay_ms, 1050);
    assert_eq!(by("s").new_delay_ms, 1050);
    assert_eq!(p.largest_knob_ms, 1050);
    // Any other target inside the interval makes one of the two knobs bigger.
    let ivs = [
        MemberInterval::new("ap2-dev-x".into(), MemberKind::Airplay2, 1500, 0.0),
        MemberInterval::new("s".into(), MemberKind::Sendspin, 0, 600.0),
    ];
    let sol = choose_target(&ivs).expect("feasible");
    for probe in [sol.lo_ms, sol.hi_ms, sol.target_ms - 100.0, sol.target_ms + 100.0] {
        let worst = ivs.iter().map(|iv| iv.knob_for(probe)).fold(0.0, f64::max);
        assert!(worst >= sol.largest_knob_ms - 1e-9, "target {probe} beats the chosen one: {worst} < {}", sol.largest_knob_ms);
    }
    // And the write is described in each member's own direction.
    assert!(by("s").effect.contains("advance 1050 ms"), "{}", by("s").effect);
    assert!(by("ap2-dev-x").effect.contains("delay 1050 ms"), "{}", by("ap2-dev-x").effect);
}

/// §2.4.2's real mixed-group failure, and the reason the solver refuses instead
/// of best-effort: a sendspin member that is already the earliest can only be
/// moved *earlier*, and an AP2 member that is already the latest only *later*, so
/// the two achievable ranges diverge and nothing can be written.
#[test]
fn a_mixed_group_that_can_only_diverge_is_refused_and_names_both_members() {
    let members = [member("s"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
    let o = [
        obs("s", 0, 0.0, 300.0, 0.0),
        obs("ap2-dev-x", 0, 4.0, 320.0, 0.0),
        obs("s", 1, 8.0, 300.0, 0.0),
        obs("ap2-dev-x", 1, 12.0, 320.0, 0.0),
    ];
    let r = solve_of(&members, &o, &[]).expect_err("must refuse");
    assert_eq!(r.kind, RefusalKind::KnobRange);
    // Both names, because changing the wrong speaker is the failure mode here.
    assert!(r.message.contains("'s'"), "{}", r.message);
    assert!(r.message.contains("'ap2-dev-x'"), "{}", r.message);
    // …and *why*: each member's direction, and how far apart they stay.
    assert!(r.message.contains("advance"), "{}", r.message);
    assert!(r.message.contains("delay"), "{}", r.message);
    assert!(r.message.contains("20 ms"), "the shortfall must be quantified: {}", r.message);
    assert!(!r.message.contains("would need"), "this is not a one-member knob overflow: {}", r.message);
    assert_eq!(r.member.as_deref(), Some("s"), "the binding ceiling is the sendspin member's own arrival");
}

/// pw-sink cannot be placed arbitrarily early: its playout delay is floored at
/// three packet times, and that floor — not the arrivals — is what pins the
/// target here.
#[test]
fn the_pw_sink_floor_can_be_what_constrains_the_target() {
    let k = knob_of(MemberKind::PwSink);
    assert_eq!(k.polarity, KnobPolarity::Delay);
    assert_eq!(k.min_ms, crate::routing::sync_settings::PWSINK_JITTER_MIN_MS);
    assert!(k.min_ms > 0, "the floor is the whole reason pw-sink is modelled apart from AP2");

    let floor = f64::from(k.min_ms);
    let ivs = [
        // Already at the floor, arriving first: it cannot be placed any earlier
        // than `base + floor`, which is exactly where it arrives now.
        MemberInterval::new("pwsink-dev-x".into(), MemberKind::PwSink, k.min_ms, 0.0),
        MemberInterval::new("s".into(), MemberKind::Sendspin, 0, 10.0),
    ];
    assert!((ivs[0].base_ms + floor).abs() < 1e-9, "base {}", ivs[0].base_ms);
    let sol = choose_target(&ivs).expect("feasible");
    // Unconstrained, the two arms would cross at (10 + −15)/2 = −2.5 ms and both
    // knobs would hold 12.5 ms. The floor forbids anything before 0, so the
    // target is clamped there and the pw-sink member stays at its minimum.
    assert!((sol.lo_ms - 0.0).abs() < 1e-9, "lo {}", sol.lo_ms);
    assert!((sol.target_ms - 0.0).abs() < 1e-9, "target {}", sol.target_ms);
    assert!((ivs[0].knob_for(sol.target_ms) - floor).abs() < 1e-9);
    assert!((ivs[1].knob_for(sol.target_ms) - 10.0).abs() < 1e-9);
    assert!((sol.largest_knob_ms - floor).abs() < 1e-9, "the floor is the largest knob: {}", sol.largest_knob_ms);
}

/// §9.2's check, now driven by **advances** (§2.4.2's "new consequence"): a
/// sendspin device plays its static delay early, so the group's lead has to
/// cover it, and raising it reconfigures every member's stream.
#[test]
fn an_advance_that_crosses_the_groups_send_ahead_high_water_mark_warns() {
    let members = [member("a"), member("b")];
    let o = [obs("a", 0, 0.0, 300.0, 0.0), obs("b", 0, 4.0, 340.0, 0.0), obs("a", 1, 8.0, 300.0, 0.0), obs("b", 1, 12.0, 340.0, 0.0)];
    let current: HashMap<String, u16> = HashMap::new();
    let ctx = SendAheadContext {
        floor_ms: 100,
        unreported_floor_ms: 0,
        min_buffer_ms: [("a".to_string(), Some(80)), ("b".to_string(), Some(80))].into_iter().collect(),
    };
    let p = solve(&SolveInput {
        timing: Timing::real(),
        members: &members,
        observations: &o,
        current_delays: &current,
        send_ahead: &ctx,
        closure: None,
    })
    .expect("accepted");
    // b arrives 40 ms later, so b takes a 40 ms *advance* ⇒ 80+40 = 120 > the
    // 100 ms floor ⇒ the group's stream is reconfigured, not one connection.
    let b = p.members.iter().find(|m| m.node_name == "b").unwrap();
    assert_eq!((b.polarity, b.new_delay_ms), (KnobPolarity::Advance, 40));
    assert!(p.warnings.iter().any(|w| w.kind == WarningKind::SendAheadHighWater), "{:?}", p.warnings);
    assert!(p.warnings.iter().any(|w| w.message.contains("120 ms")));
    assert!(p.warnings.iter().any(|w| w.message.contains("advance")), "the warning must name what causes it: {:?}", p.warnings);
    // The same solve with plenty of floor does not warn.
    let ctx = SendAheadContext { floor_ms: 500, ..ctx };
    let p = solve(&SolveInput {
        timing: Timing::real(),
        members: &members,
        observations: &o,
        current_delays: &current,
        send_ahead: &ctx,
        closure: None,
    })
    .expect("accepted");
    assert!(!p.warnings.iter().any(|w| w.kind == WarningKind::SendAheadHighWater));
}

/// The other half of the same rule, and the delay-only case: an AP2 group still
/// aligns to its **latest** member (§9.1 was right for delay knobs), and a delay
/// knob — which happens inside that member's own sender — must never be counted
/// into the sendspin group's lead, however large it is.
#[test]
fn an_ap2_only_group_delays_towards_the_latest_and_never_lifts_the_lead() {
    let ap2 = |n: &str| SessionMember { node_name: n.to_string(), kind: MemberKind::Airplay2 };
    let members = [ap2("ap2-dev-early"), ap2("ap2-dev-late")];
    let o = [
        obs("ap2-dev-early", 0, 0.0, 300.0, 0.0),
        obs("ap2-dev-late", 0, 4.0, 900.0, 0.0),
        obs("ap2-dev-early", 1, 8.0, 300.0, 0.0),
        obs("ap2-dev-late", 1, 12.0, 900.0, 0.0),
    ];
    let current: HashMap<String, u16> = HashMap::new();
    // Deliberately over-broad: the context claims a `min_buffer_ms` for both AP2
    // members, so if the solve fed *delays* into the mark this would warn.
    let ctx = SendAheadContext {
        floor_ms: 100,
        unreported_floor_ms: 0,
        min_buffer_ms: [("ap2-dev-early".to_string(), Some(80)), ("ap2-dev-late".to_string(), Some(80))].into_iter().collect(),
    };
    let p = solve(&SolveInput {
        timing: Timing::real(),
        members: &members,
        observations: &o,
        current_delays: &current,
        send_ahead: &ctx,
        closure: None,
    })
    .expect("accepted");
    assert_eq!(p.members.iter().find(|m| m.node_name == "ap2-dev-early").unwrap().new_delay_ms, 600);
    assert_eq!(p.members.iter().find(|m| m.node_name == "ap2-dev-late").unwrap().new_delay_ms, 0);
    assert_eq!(p.reference, "ap2-dev-late", "with delay knobs only, the latest member is still the one left alone");
    assert!(!p.warnings.iter().any(|w| w.kind == WarningKind::SendAheadHighWater), "{:?}", p.warnings);
}

#[test]
fn only_advances_feed_the_send_ahead_mark() {
    let ctx =
        SendAheadContext { floor_ms: 100, unreported_floor_ms: 0, min_buffer_ms: [("s".to_string(), Some(80))].into_iter().collect() };
    let map = |pairs: &[(&str, u16)]| -> HashMap<String, u16> { pairs.iter().map(|(n, v)| ((*n).to_string(), *v)).collect() };
    // A sendspin advance is added to the lead, because the device plays that early.
    assert_eq!(ctx.mark_ms(&map(&[("s", 300)])), 380);
    // A member the context knows nothing about contributes nothing, however big.
    assert_eq!(ctx.mark_ms(&map(&[("s", 0), ("ap2-dev-x", 2000)])), 100);
}

#[test]
fn linearising_puts_the_earliest_member_at_zero_and_wraps_the_short_way() {
    let offsets: HashMap<String, f64> = [("a".to_string(), 1990.0), ("b".to_string(), 5.0)].into_iter().collect();
    let order = vec!["a".to_string(), "b".to_string()];
    let out = linearise(&offsets, &order, 2000.0);
    // b is 15 ms *after* a, not 1985 ms before it.
    assert_eq!(out[0].0, "a");
    assert!((out[0].1 - 0.0).abs() < 1e-9);
    assert!((out[1].1 - 15.0).abs() < 1e-9, "{:?}", out);
}
