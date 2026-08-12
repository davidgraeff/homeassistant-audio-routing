//! Tests for playback-level learning and the crosstalk matrix.

use super::*;
use crate::align::estimator::MIN_SECOND_PEAK_RATIO;

// -- a simulated room --------------------------------------------------
//
// Each member has an acoustic gain: the level in dBFS its burst reaches at the
// mic when its knob is at 100. Level scaling follows the assumed taper unless a
// per-member `taper` says otherwise, which is how the taper-learning path gets
// exercised. SNR is the burst's dBFS over the room's noise floor.
struct Room {
    gain_db: HashMap<String, f64>,
    taper: HashMap<String, f64>,
    leak_db: HashMap<(String, String), f64>,
    floor_dbfs: f64,
    channel_of: HashMap<String, String>,
}

impl Room {
    fn new(members: &[(&str, &str, f64)]) -> Self {
        Self {
            gain_db: members.iter().map(|(n, _, g)| ((*n).to_string(), *g)).collect(),
            taper: HashMap::new(),
            leak_db: HashMap::new(),
            floor_dbfs: -70.0,
            channel_of: members.iter().map(|(n, c, _)| ((*n).to_string(), (*c).to_string())).collect(),
        }
    }

    fn with_taper(mut self, member: &str, taper: f64) -> Self {
        self.taper.insert(member.to_string(), taper);
        self
    }

    fn with_leak(mut self, member: &str, channel: &str, db: f64) -> Self {
        self.leak_db.insert((member.to_string(), channel.to_string()), db);
        self
    }

    /// dBFS of `member`'s burst at the mic for a commanded level.
    fn dbfs(&self, member: &str, level: u8) -> f64 {
        let taper = self.taper.get(member).copied().unwrap_or(1.0);
        self.gain_db[member] + level_to_db(level) * taper
    }

    fn observe(&self, step: &RampStep) -> RoundObservation {
        let playing: Vec<&MemberLevel> = match &step.excite {
            Excitation::All => step.levels.iter().collect(),
            Excitation::Solo { node_name } => step.levels.iter().filter(|l| &l.node_name == node_name).collect(),
        };
        let mut per_channel: HashMap<String, f64> = HashMap::new();
        let mut amp = 0.0f64;
        for l in &playing {
            let dbfs = self.dbfs(&l.node_name, l.level);
            amp += 10f64.powf(dbfs / 20.0);
            let own = self.channel_of[&l.node_name].clone();
            let e = per_channel.entry(own.clone()).or_insert(f64::NEG_INFINITY);
            *e = e.max(dbfs);
            for (ch, leak) in self.leak_db.iter().filter(|((m, _), _)| m == &l.node_name).map(|((_, c), v)| (c, v)) {
                let e = per_channel.entry(ch.clone()).or_insert(f64::NEG_INFINITY);
                *e = e.max(dbfs + leak);
            }
        }
        let mut channels: Vec<ChannelReading> = Vec::new();
        let mut labels: Vec<String> = self.channel_of.values().cloned().collect();
        labels.sort();
        labels.dedup();
        for label in labels {
            let dbfs = per_channel.get(&label).copied().unwrap_or(f64::NEG_INFINITY);
            let snr = (dbfs - self.floor_dbfs).max(0.0);
            channels.push(ChannelReading::new(label, snr, 50.0));
        }
        let mic_peak = amp.min(4.0) as f32;
        RoundObservation { excited: step.excite.clone(), channels, clipped: mic_peak >= 1.0, mic_peak }
    }
}

/// Drive the solver against a room until it decides. Returns the decision and
/// the number of rounds observed.
fn run(solver: &mut LevelSolver, room: &Room) -> (LevelDecision, usize) {
    let mut step = solver.begin();
    let budget = solver.round_budget();
    for n in 1..=budget + 2 {
        let obs = room.observe(&step);
        match solver.observe(obs) {
            LevelDecision::Continue(next) => {
                assert!(next.round <= next.round_budget + 1, "step {n} advertised round {} over budget {}", next.round, next.round_budget);
                step = next;
            }
            other => return (other, n),
        }
    }
    panic!("solver never decided within {} rounds", budget + 2);
}

fn specs(members: &[(&str, &str, LevelMemberKind)]) -> Vec<LevelMemberSpec> {
    members.iter().map(|(n, c, k)| LevelMemberSpec::new(*n, *c, *k)).collect()
}

#[test]
fn level_db_helpers_round_trip() {
    assert!((level_to_db(100) - 0.0).abs() < 1e-9);
    assert!((level_to_db(50) + 6.0206).abs() < 1e-3);
    assert_eq!(db_to_level(0.0), 100);
    assert_eq!(db_to_level(-6.0206), 50);
    // Clamped to the usable knob range, not to 0/255.
    assert_eq!(db_to_level(-120.0), MIN_LEVEL);
    assert_eq!(db_to_level(40.0), MAX_LEVEL);
    assert_eq!(ap2_scalar(50), 0.5);
}

#[test]
fn member_kinds_map_to_the_right_knob() {
    assert_eq!(LevelMemberKind::Sendspin.knob(), LevelKnob::Live);
    assert_eq!(LevelMemberKind::Airplay2.knob(), LevelKnob::SnapshotRestore);
    // The floor a kind guarantees by itself — pw-sink's transport carries no level.
    assert_eq!(LevelMemberKind::PwSink.knob(), LevelKnob::None);
    assert!(!LevelMemberKind::PwSink.is_adjustable());
    assert_eq!(LevelMemberKind::from(MemberKind::Sendspin), LevelMemberKind::Sendspin);
    assert_eq!(LevelMemberKind::from(MemberKind::Airplay2), LevelMemberKind::Airplay2);
    assert_eq!(LevelMemberKind::from(MemberKind::PwSink), LevelMemberKind::PwSink);
}

/// W20: the knob is a property of the **output**, not of the kind. A pw-sink host
/// whose receiver agent answers is levellable; the same kind with no agent is not, and
/// `LevelKnob::None` therefore has to stay reachable rather than becoming a legacy arm.
#[test]
fn a_resolved_per_output_knob_overrides_what_the_kind_guarantees() {
    let bare = LevelMemberSpec::new("pwsink-dev-office", "o", LevelMemberKind::PwSink);
    assert_eq!(bare.knob(), LevelKnob::None, "nobody asked the host, so the pessimistic answer stands");

    let with_agent = bare.clone().with_knob(LevelKnob::SnapshotRestore);
    assert_eq!(with_agent.knob(), LevelKnob::SnapshotRestore);
    assert!(with_agent.knob().is_adjustable(), "a pw-sink host with a live agent is levellable");

    // …and the resolution can go the other way too: an agent that dropped is a
    // pw-sink member explicitly resolved back to no knob.
    assert_eq!(bare.clone().with_knob(LevelKnob::None).knob(), LevelKnob::None);
    // The kind itself is untouched by the override — it is still what the member *is*.
    assert_eq!(with_agent.kind, LevelMemberKind::PwSink);
}

/// The override has to reach every decision, not just the report: such a member is
/// ramped, is written to, and owes a restore.
#[test]
fn a_levellable_pwsink_member_is_ramped_written_and_restored() {
    let members = vec![
        LevelMemberSpec::new("sendspin-dev-kitchen", "k", LevelMemberKind::Sendspin),
        // Resolved by the caller that could ask the host, with the level it read.
        LevelMemberSpec::new("pwsink-dev-office", "o", LevelMemberKind::PwSink).with_knob(LevelKnob::SnapshotRestore).with_snapshot(70),
    ];
    let mut solver = LevelSolver::new(members).unwrap();
    let step = solver.begin();
    assert_eq!(step.changed.len(), 2, "both members are writable now: {:?}", step.changed);
    let office = step.changed.iter().find(|c| c.node_name == "pwsink-dev-office").expect("written");
    assert_eq!(office.level, START_LEVEL, "it starts at the cold-start level like any other knob");
    assert_eq!(office.knob, LevelKnob::SnapshotRestore);

    // A room in which it would have been refused as un-levellable before W20: 25 dB
    // short at whatever the host happened to be set to.
    let room = Room::new(&[("sendspin-dev-kitchen", "k", -30.0), ("pwsink-dev-office", "o", -35.0)]);
    let (decision, _) = run(&mut solver, &room);
    let plan = decision.plan().expect("it can be turned up now, so this converges");
    for m in &plan.members {
        assert!(m.peak_snr_db.unwrap() >= plan.target_snr_db, "{} {:?}", m.node_name, m.peak_snr_db);
        assert!(m.advice.is_none(), "nothing for the user to do by hand: {:?}", m.advice);
    }
    // And the restore obligation exists, with the host's own pre-session level in it.
    let restore = solver.restore_plan();
    let office = restore.iter().find(|r| r.node_name == "pwsink-dev-office").expect("a driven level owes a restore");
    assert_eq!(office.knob, LevelKnob::SnapshotRestore);
    assert_eq!(office.level, Some(70));
}

/// The same member with no agent: still refused by name, because that is the honest
/// answer and it is the member that sets the clip ceiling (plan §7).
#[test]
fn a_pwsink_member_with_no_agent_is_still_refused_as_un_levellable() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-kitchen", "k", LevelMemberKind::Sendspin),
        ("pwsink-dev-office", "o", LevelMemberKind::PwSink),
    ]))
    .unwrap();
    let room = Room::new(&[("sendspin-dev-kitchen", "k", -30.0), ("pwsink-dev-office", "o", -60.0)]);
    let (decision, _) = run(&mut solver, &room);
    let r = decision.refusal().expect("should refuse");
    assert_eq!(r.reason, RefusalReason::NoLevelKnob);
    assert_eq!(r.member, "pwsink-dev-office");
}

#[test]
fn converges_from_a_cold_start() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-kitchen", "k", LevelMemberKind::Sendspin),
        ("ap2-dev-bath", "b", LevelMemberKind::Airplay2),
    ]))
    .unwrap();
    // Both need a lift from the cold start (level 30 → about -10.5 dB).
    let room = Room::new(&[("sendspin-dev-kitchen", "k", -30.0), ("ap2-dev-bath", "b", -32.0)]);
    let (decision, rounds) = run(&mut solver, &room);
    let plan = decision.plan().expect("should converge");
    assert!(rounds <= 4, "cold start took {rounds} rounds");
    for m in &plan.members {
        let snr = m.peak_snr_db.expect("read");
        assert!(snr >= plan.target_snr_db, "{} at {snr:.1} dB", m.node_name);
        assert!(m.reached_target);
        assert!(m.margin_db.unwrap() >= 10.0, "{} margin {:?}", m.node_name, m.margin_db);
    }
    assert!(plan.aggregate_peak.unwrap() <= CLIP_BACKOFF_PEAK);
    assert!(plan.crosstalk.verdict.is_usable(), "{:?}", plan.crosstalk.verdict);
}

#[test]
fn a_twenty_db_near_far_spread_is_solved_per_member_not_globally() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-near", "n", LevelMemberKind::Sendspin),
        ("sendspin-dev-far", "f", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    // 20 dB of near/far spread — plan §7's motivating case.
    let room = Room::new(&[("sendspin-dev-near", "n", -18.0), ("sendspin-dev-far", "f", -38.0)]);
    let (decision, rounds) = run(&mut solver, &room);
    let plan = decision.plan().expect("should converge");
    assert!(rounds <= 5, "took {rounds} rounds");
    let near = plan.levels.iter().find(|l| l.node_name == "sendspin-dev-near").unwrap().level;
    let far = plan.levels.iter().find(|l| l.node_name == "sendspin-dev-far").unwrap().level;
    // The whole point: not one global volume.
    assert!(far > near, "far {far} should end up louder than near {near}");
    for m in &plan.members {
        assert!(m.peak_snr_db.unwrap() >= plan.target_snr_db, "{} {:?}", m.node_name, m.peak_snr_db);
    }
}

#[test]
fn a_member_that_cannot_get_loud_enough_is_refused_by_name() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-near", "n", LevelMemberKind::Sendspin),
        ("sendspin-dev-shed", "s", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    // The shed is 55 dB down: even at level 100 its SNR is 15 dB, under target.
    let room = Room::new(&[("sendspin-dev-near", "n", -30.0), ("sendspin-dev-shed", "s", -55.0)]);
    let (decision, _) = run(&mut solver, &room);
    let r = decision.refusal().expect("should refuse");
    assert_eq!(r.reason, RefusalReason::TooQuietAtMaxLevel);
    assert_eq!(r.member, "sendspin-dev-shed");
    assert!(r.message.contains("maximum playback level"), "{}", r.message);
    // The refusal still carries the full table so the UI can show both members.
    assert_eq!(r.members.len(), 2);
}

#[test]
fn the_clip_ceiling_refusal_names_both_the_quiet_and_the_blocking_member() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-onthephone", "n", LevelMemberKind::Sendspin),
        ("sendspin-dev-hall", "h", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    // The near speaker is right on the mic (0 dBFS at level 100) and cannot be
    // trimmed below MIN_LEVEL; the hall speaker needs a lot of level, and by the
    // time it has it the sum clips.
    let room = Room::new(&[("sendspin-dev-onthephone", "n", 26.0), ("sendspin-dev-hall", "h", -50.0)]);
    let (decision, _) = run(&mut solver, &room);
    let r = decision.refusal().expect("should refuse");
    assert_eq!(r.reason, RefusalReason::ClipCeiling, "{}", r.message);
    // The member the user has to act on...
    assert_eq!(r.member, "sendspin-dev-hall");
    // ...and the one actually setting the ceiling. Naming only one of the two
    // would send the user to the wrong speaker.
    assert_eq!(r.blocking_member.as_deref(), Some("sendspin-dev-onthephone"));
    // It got there by trimming the loud member to the floor first, i.e. it
    // exhausted the adjustable side before refusing.
    let hot = r.members.iter().find(|m| m.node_name == "sendspin-dev-onthephone").unwrap();
    assert_eq!(hot.level, MIN_LEVEL);
}

#[test]
fn a_report_only_member_that_is_too_quiet_is_refused_and_told_to_be_raised_by_hand() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-kitchen", "k", LevelMemberKind::Sendspin),
        ("pwsink-dev-office", "o", LevelMemberKind::PwSink),
    ]))
    .unwrap();
    // pw-sink members are played at whatever the device is set to; here that is
    // not enough, and there is no knob to fix it with.
    let room = Room::new(&[("sendspin-dev-kitchen", "k", -30.0), ("pwsink-dev-office", "o", -60.0)]);
    let (decision, _) = run(&mut solver, &room);
    let r = decision.refusal().expect("should refuse");
    assert_eq!(r.reason, RefusalReason::NoLevelKnob);
    assert_eq!(r.member, "pwsink-dev-office");
    assert!(r.message.contains("no level knob"), "{}", r.message);
    // And the solver never pretended it could write a level for it.
    assert!(solver.restore_plan().iter().all(|p| p.node_name != "pwsink-dev-office"));
}

#[test]
fn a_fixed_member_that_clips_constrains_the_solve_and_is_named() {
    let mut solver = LevelSolver::new(specs(&[
        ("pwsink-dev-loud", "l", LevelMemberKind::PwSink),
        ("sendspin-dev-quiet", "q", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    // The pw-sink output alone reaches full scale at the mic. Turning the
    // sendspin member down cannot rescue the round, so the refusal has to point
    // at the member that has no knob.
    let room = Room::new(&[("pwsink-dev-loud", "l", 0.0), ("sendspin-dev-quiet", "q", -40.0)]);
    let (decision, _) = run(&mut solver, &room);
    let r = decision.refusal().expect("should refuse");
    assert_eq!(r.reason, RefusalReason::FixedMemberClips, "{}", r.message);
    assert_eq!(r.member, "pwsink-dev-loud");
    assert!(r.message.contains("no level knob"), "{}", r.message);
}

#[test]
fn a_hot_but_adjustable_member_is_trimmed_instead_of_refused() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-hot", "h", LevelMemberKind::Sendspin),
        ("sendspin-dev-ok", "o", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    // "hot" would clip at the cold-start level; the solve must cut it rather
    // than give up, because it has a live knob.
    let room = Room::new(&[("sendspin-dev-hot", "h", 8.0), ("sendspin-dev-ok", "o", -30.0)]);
    let (decision, _) = run(&mut solver, &room);
    let plan = decision.plan().expect("should converge");
    let hot = plan.levels.iter().find(|l| l.node_name == "sendspin-dev-hot").unwrap().level;
    assert!(hot < START_LEVEL, "hot member should have been trimmed below the cold start, got {hot}");
    assert!(plan.aggregate_peak.unwrap() < 1.0);
}

#[test]
fn the_taper_is_measured_so_a_wrong_assumption_only_costs_rounds() {
    // This device realises 0.5 dB for every dB commanded — a badly wrong taper.
    let mut solver = LevelSolver::new(specs(&[("sendspin-dev-odd", "o", LevelMemberKind::Sendspin)])).unwrap();
    let room = Room::new(&[("sendspin-dev-odd", "o", -40.0)]).with_taper("sendspin-dev-odd", 0.5);
    let (decision, rounds) = run(&mut solver, &room);
    let plan = decision.plan().expect("should still converge");
    assert!(plan.members[0].peak_snr_db.unwrap() >= plan.target_snr_db);
    assert!(plan.members[0].measured_taper < 0.9, "taper should have been learned, got {}", plan.members[0].measured_taper);
    assert!(rounds <= solver.round_budget());
}

#[test]
fn the_crosstalk_matrix_is_built_from_solo_rounds_and_a_clean_assignment_passes() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-a", "ch_a", LevelMemberKind::Sendspin),
        ("sendspin-dev-b", "ch_b", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    let room = Room::new(&[("sendspin-dev-a", "ch_a", -30.0), ("sendspin-dev-b", "ch_b", -30.0)])
        .with_leak("sendspin-dev-a", "ch_b", -35.0)
        .with_leak("sendspin-dev-b", "ch_a", -33.0);
    let (decision, _) = run(&mut solver, &room);
    let plan = decision.plan().expect("should converge");
    let xt = &plan.crosstalk;
    assert_eq!(xt.channels, vec!["ch_a".to_string(), "ch_b".to_string()]);
    assert_eq!(xt.rows.len(), 2);
    let row_a = xt.rows.iter().find(|r| r.member == "sendspin-dev-a").unwrap();
    assert_eq!(row_a.leak_db[0], 0.0, "the diagonal is zero by construction");
    // The injected leak is -35 dB, but it lands under the room's noise floor, so
    // the row can only prove "at least as clean as -driven_snr" — see the
    // dynamic-range note on CrosstalkRow::leak_db.
    assert!(row_a.leak_db[1] <= MAX_CROSSTALK_DB, "row A leak was {:?}", row_a.leak_db);
    assert!(row_a.leak_db[1] >= -row_a.driven_snr_db - 0.001, "a row cannot resolve leakage below its own floor: {:?}", row_a.leak_db);
    assert!(matches!(xt.verdict, CrosstalkVerdict::Usable), "{:?}", xt.verdict);
    assert!(xt.worst.as_ref().unwrap().leak_db <= MAX_CROSSTALK_DB);
}

#[test]
fn a_leaky_assignment_fails_the_verdict_without_blocking_the_level_solve() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-a", "ch_a", LevelMemberKind::Sendspin),
        ("sendspin-dev-b", "ch_b", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    // -6 dB of leakage: the estimator would see a spurious peak only 2x down.
    let room = Room::new(&[("sendspin-dev-a", "ch_a", -30.0), ("sendspin-dev-b", "ch_b", -30.0)]).with_leak("sendspin-dev-a", "ch_b", -6.0);
    let (decision, _) = run(&mut solver, &room);
    let plan = decision.plan().expect("crosstalk is reported, not refused");
    match &plan.crosstalk.verdict {
        CrosstalkVerdict::Failed { worst, mislabelled, message } => {
            assert!(!mislabelled);
            assert_eq!(worst.member, "sendspin-dev-a");
            assert_eq!(worst.channel, "ch_b");
            assert!(message.contains("reassign"), "{message}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!plan.crosstalk.verdict.is_usable());
    assert!(plan.warnings.iter().any(|w| w.contains("leaks")), "{:?}", plan.warnings);
}

#[test]
fn a_mislabelled_assignment_is_detected() {
    let mut solver = LevelSolver::new(specs(&[
        ("sendspin-dev-a", "ch_a", LevelMemberKind::Sendspin),
        ("sendspin-dev-b", "ch_b", LevelMemberKind::Sendspin),
    ]))
    .unwrap();
    // Member A's energy lands *louder* in B's channel than in its own: the
    // frequency table does not match the speakers.
    let room = Room::new(&[("sendspin-dev-a", "ch_a", -30.0), ("sendspin-dev-b", "ch_b", -30.0)]).with_leak("sendspin-dev-a", "ch_b", 3.0);
    let (decision, _) = run(&mut solver, &room);
    let plan = decision.plan().expect("still converges");
    assert!(matches!(plan.crosstalk.verdict, CrosstalkVerdict::Failed { mislabelled: true, .. }), "{:?}", plan.crosstalk.verdict);
}

#[test]
fn shared_channels_make_the_crosstalk_verdict_not_applicable_and_force_sequential() {
    // The existing click track: both members emit both bursts (plan §2.2).
    let shared = specs(&[("sendspin-dev-a", "a", LevelMemberKind::Sendspin), ("sendspin-dev-b", "a", LevelMemberKind::Sendspin)]);
    assert!(LevelSolver::with_config(shared.clone(), LevelConfig::default()).is_err(), "parallel needs per-member channels");
    let mut solver = LevelSolver::with_config(shared, LevelConfig::sequential()).unwrap();
    let room = Room::new(&[("sendspin-dev-a", "a", -30.0), ("sendspin-dev-b", "a", -32.0)]);
    let (decision, _) = run(&mut solver, &room);
    let plan = decision.plan().expect("should converge");
    assert!(matches!(plan.crosstalk.verdict, CrosstalkVerdict::NotApplicable { .. }), "{:?}", plan.crosstalk.verdict);
    for m in &plan.members {
        assert!(m.peak_snr_db.unwrap() >= plan.target_snr_db);
    }
}

#[test]
fn sequential_mode_converges_and_its_budget_grows_with_the_member_count() {
    let members = specs(&[
        ("sendspin-dev-a", "a", LevelMemberKind::Sendspin),
        ("sendspin-dev-b", "b", LevelMemberKind::Sendspin),
        ("sendspin-dev-c", "c", LevelMemberKind::Sendspin),
    ]);
    let mut solver = LevelSolver::with_config(members, LevelConfig::sequential()).unwrap();
    assert_eq!(solver.round_budget(), 3 * SEQUENTIAL_ROUNDS_PER_MEMBER + SEQUENTIAL_ROUNDS_SLACK);
    let room = Room::new(&[("sendspin-dev-a", "a", -28.0), ("sendspin-dev-b", "b", -34.0), ("sendspin-dev-c", "c", -31.0)]);
    let (decision, rounds) = run(&mut solver, &room);
    let plan = decision.plan().expect("should converge");
    assert!(rounds <= solver.round_budget(), "{rounds} rounds over budget");
    // Solo rounds are the crosstalk rows for free, so the matrix is complete.
    assert_eq!(plan.crosstalk.rows.len(), 3);
}

#[test]
fn the_round_bound_holds_against_a_room_that_never_settles() {
    let mut solver =
        LevelSolver::new(specs(&[("sendspin-dev-a", "a", LevelMemberKind::Sendspin), ("sendspin-dev-b", "b", LevelMemberKind::Sendspin)]))
            .unwrap();
    let budget = solver.round_budget();
    // A pathological room: the reported SNR is unrelated to the level, so no
    // correction ever helps. The solve must stop, not spin.
    let mut step = solver.begin();
    let mut rounds = 0;
    let mut refusal = None;
    for n in 0..budget * 3 {
        rounds += 1;
        let snr = if n % 2 == 0 { 12.0 } else { 18.0 };
        let obs = RoundObservation {
            excited: step.excite.clone(),
            channels: vec![ChannelReading::new("a", snr, 50.0), ChannelReading::new("b", snr + 1.0, 50.0)],
            clipped: false,
            mic_peak: 0.2,
        };
        match solver.observe(obs) {
            LevelDecision::Continue(next) => step = next,
            LevelDecision::Refused(r) => {
                refusal = Some(r);
                break;
            }
            LevelDecision::Converged(_) => panic!("must not converge on a room that never reaches target"),
        }
    }
    let r = refusal.expect("must refuse rather than loop");
    assert!(rounds <= budget, "ran {rounds} rounds against a budget of {budget}");
    assert!(matches!(r.reason, RefusalReason::RoundBoundExhausted | RefusalReason::TooQuietAtMaxLevel), "{:?}: {}", r.reason, r.message);
    assert!(!r.member.is_empty());
}

#[test]
fn restore_plan_covers_ap2_snapshot_semantics_and_skips_untouched_members() {
    let mut members = specs(&[
        ("sendspin-dev-k", "k", LevelMemberKind::Sendspin),
        ("ap2-dev-known", "n", LevelMemberKind::Airplay2),
        ("ap2-dev-unknown", "u", LevelMemberKind::Airplay2),
        ("pwsink-dev-x", "x", LevelMemberKind::PwSink),
    ]);
    members[1].snapshot_level = Some(64);
    let mut solver = LevelSolver::new(members).unwrap();
    // Nothing written yet.
    assert!(solver.restore_plan().is_empty());
    let _ = solver.begin();
    let plan = solver.restore_plan();
    // Three adjustable members were written; the pw-sink one never is.
    assert_eq!(plan.len(), 3);
    let known = plan.iter().find(|p| p.node_name == "ap2-dev-known").unwrap();
    assert_eq!(known.knob, LevelKnob::SnapshotRestore);
    assert_eq!(known.level, Some(64));
    let unknown = plan.iter().find(|p| p.node_name == "ap2-dev-unknown").unwrap();
    assert_eq!(unknown.level, None);
    assert!(unknown.note.contains("device-authoritative"), "{}", unknown.note);
}

#[test]
fn the_first_step_starts_low_and_only_writes_adjustable_members() {
    let mut solver =
        LevelSolver::new(specs(&[("sendspin-dev-k", "k", LevelMemberKind::Sendspin), ("pwsink-dev-x", "x", LevelMemberKind::PwSink)]))
            .unwrap();
    let step = solver.begin();
    assert_eq!(step.round, 1);
    assert_eq!(step.purpose, RampPurpose::Probe);
    assert_eq!(step.excite, Excitation::All);
    assert_eq!(step.levels.len(), 2);
    assert_eq!(step.changed.len(), 1, "only the sendspin member is writable");
    assert_eq!(step.changed[0].node_name, "sendspin-dev-k");
    assert_eq!(step.changed[0].level, START_LEVEL);
    const { assert!(START_LEVEL < 50, "the cold start must sit below calibrate::DEFAULT_CAL_VOLUME") };
}

#[test]
fn a_missing_channel_is_a_wiring_bug_and_says_so() {
    let mut solver = LevelSolver::new(specs(&[("sendspin-dev-k", "k", LevelMemberKind::Sendspin)])).unwrap();
    let step = solver.begin();
    let obs = RoundObservation {
        excited: step.excite.clone(),
        channels: vec![ChannelReading::new("not_k", 40.0, 50.0)],
        clipped: false,
        mic_peak: 0.2,
    };
    let r = solver.observe(obs).refusal().cloned().expect("should refuse");
    assert_eq!(r.reason, RefusalReason::MissingChannel);
    assert_eq!(r.member, "sendspin-dev-k");
    assert!(r.message.contains("\"k\""), "{}", r.message);
}

#[test]
fn an_empty_member_list_is_rejected_at_construction() {
    assert!(LevelSolver::new(Vec::new()).is_err());
}

#[test]
fn frequency_audit_accepts_the_plans_four_channel_set_and_flags_bad_ones() {
    // Plan §6.2's worked example.
    assert!(audit_frequency_assignment(&[2000.0, 2500.0, 3050.0, 3700.0]).is_empty());
    // 2x1500 = 3000 lands on the 3000 Hz channel.
    let harmonic = audit_frequency_assignment(&[1500.0, 3000.0]);
    assert!(harmonic.iter().any(|c| c.kind == FrequencyConflictKind::Harmonic), "{harmonic:?}");
    // Too close together.
    let spacing = audit_frequency_assignment(&[2000.0, 2200.0]);
    assert!(spacing.iter().any(|c| c.kind == FrequencyConflictKind::Spacing));
    // Outside the phone-mic + small-speaker band.
    let band = audit_frequency_assignment(&[300.0, 9000.0]);
    assert_eq!(band.iter().filter(|c| c.kind == FrequencyConflictKind::OutOfBand).count(), 2);
}

#[test]
fn the_target_keeps_a_real_margin_over_the_estimators_refusal_threshold() {
    // Guards the one number this module exists to choose.
    const { assert!(TARGET_PEAK_SNR_DB - MIN_PEAK_SNR_DB >= 10.0) };
    // The crosstalk thresholds must stay clear of the estimator's own
    // ambiguity floor, or a "usable" assignment could still be refused.
    let ambiguity_db = 20.0 * MIN_SECOND_PEAK_RATIO.log10();
    assert!(MAX_CROSSTALK_DB < -ambiguity_db - 10.0, "{MAX_CROSSTALK_DB} vs {ambiguity_db}");
    assert!(MARGINAL_CROSSTALK_DB < -ambiguity_db);
}
