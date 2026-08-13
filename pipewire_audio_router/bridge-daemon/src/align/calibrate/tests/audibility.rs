//! Audibility and levels: which channel silences or levels each member, and that
//! every teardown path restores exactly what it changed.

use super::super::*;
use super::harness::*;

/// The decision itself, on its own: "how do I silence this member?" is a **per-output**
/// question, so two members of the same kind must be able to resolve differently.
#[tokio::test]
async fn the_silencing_channel_is_resolved_per_output_not_per_kind() {
    let m = vec![
        member("sendspin-dev-r", MemberKind::Sendspin),
        member("ap2-dev-r", MemberKind::Airplay2),
        member("pwsink-dev-r-agent", MemberKind::PwSink),
        member("pwsink-dev-r-none", MemberKind::PwSink),
    ];
    let plan = audibility_plan(&m, &audible(&["sendspin-dev-r"]));
    let channels = |resolved: Vec<(String, SilenceChannel, bool)>| resolved.into_iter().map(|(_, c, _)| c).collect::<Vec<_>>();

    // No silencer at all: everything without an in-band mute lands on the relay.
    assert_eq!(
        channels(silence_plan(&plan, None).await),
        vec![SilenceChannel::SendspinInBand, SilenceChannel::Ap2InBand, SilenceChannel::Relay, SilenceChannel::Relay]
    );

    // With one wired, the pw-sink member whose host owns it goes out of band and the
    // one it does not own still falls back — same kind, different answers.
    let host: Arc<dyn OutOfBandMute> = FakeHost::owning(&[("pwsink-dev-r-agent", false)]);
    assert_eq!(
        channels(silence_plan(&plan, Some(&host)).await),
        vec![SilenceChannel::SendspinInBand, SilenceChannel::Ap2InBand, SilenceChannel::OutOfBand, SilenceChannel::Relay]
    );
    // In-band is never overridden by a silencer that claims to own the output: a device
    // that can mute itself should, and the seam cannot take that away.
    let greedy: Arc<dyn OutOfBandMute> = FakeHost::owning(&[("sendspin-dev-r", false), ("ap2-dev-r", false)]);
    assert_eq!(channels(silence_plan(&plan, Some(&greedy)).await)[..2], [SilenceChannel::SendspinInBand, SilenceChannel::Ap2InBand]);
}

/// Plan §12.2: solo **one**, not two. This is also what unblocks §7's all-play
/// round, so both extremes are checked here.
#[test]
fn audibility_is_a_set_so_one_two_or_all_members_can_be_audible() {
    let m = members();
    let on = |set: &BTreeSet<String>| audibility_plan(&m, set).into_iter().filter(|(_, _, on)| *on).map(|(n, _, _)| n).collect::<Vec<_>>();
    // One — level-setting and the sequential measurement.
    assert_eq!(on(&audible(&["sendspin-dev-b"])), vec!["sendspin-dev-b".to_string()]);
    // Two — the by-ear reference/target comparison, no longer a special case.
    assert_eq!(on(&audible(&["sendspin-dev-a", "ap2-dev-c"])), vec!["sendspin-dev-a".to_string(), "ap2-dev-c".to_string()]);
    // All — §7's headroom round, which "reference + target" could not express.
    assert_eq!(on(&audible(&["sendspin-dev-a", "sendspin-dev-b", "ap2-dev-c"])).len(), 3);
    // None — every member muted (a valid state between rounds).
    assert!(on(&BTreeSet::new()).is_empty());
    // Kinds are carried through, because the two mute in different ways.
    let plan = audibility_plan(&m, &audible(&["ap2-dev-c"]));
    assert_eq!(plan.iter().filter(|(_, k, _)| *k == MemberKind::Airplay2).count(), 1);
}

/// The restore obligation teardown used to get wrong: a member the user had muted
/// before the session must still be muted after it.
#[test]
fn teardown_restores_the_snapshotted_mutes_not_blanket_unmute() {
    let m = members();
    let sendspin: HashMap<String, bool> =
        [("sendspin-dev-a".to_string(), true), ("sendspin-dev-b".to_string(), false)].into_iter().collect();
    let ap2: HashMap<String, bool> = [("ap2-dev-c".to_string(), true)].into_iter().collect();
    let plan = restore_mute_plan(&m, &sendspin, &ap2);
    assert_eq!(
        plan,
        vec![
            ("sendspin-dev-a".to_string(), SilenceChannel::SendspinInBand, true),
            ("sendspin-dev-b".to_string(), SilenceChannel::SendspinInBand, false),
            ("ap2-dev-c".to_string(), SilenceChannel::Ap2InBand, true),
        ]
    );
    // A member with no in-band mute is not in this plan at all: its restore is a
    // different obligation (the host's own state, or the relay mute the hold drops).
    let mut with_pwsink = m.clone();
    with_pwsink.push(member("pwsink-dev-d", MemberKind::PwSink));
    let plan = restore_mute_plan(&with_pwsink, &sendspin, &ap2);
    assert_eq!(plan.len(), 3, "the pw-sink member has no in-band mute to restore: {plan:?}");
    assert!(plan.iter().all(|(n, _, _)| n != "pwsink-dev-d"));
    // A member with nothing snapshotted (it appeared mid-session) is left
    // unmuted — the safe direction: audible, never silently muted forever.
    let plan = restore_mute_plan(&m, &HashMap::new(), &HashMap::new());
    assert!(plan.iter().all(|(_, _, muted)| !muted));
    // A kind's map is never read for the other kind (an AP2 name in the sendspin
    // map must not resurrect as an AP2 mute).
    let crossed: HashMap<String, bool> = [("ap2-dev-c".to_string(), true)].into_iter().collect();
    let plan = restore_mute_plan(&m, &crossed, &HashMap::new());
    assert!(plan.iter().all(|(_, _, muted)| !muted));
}

/// The level half of the same obligation, through the **real** control store (no
/// device needed — `desired` is in-process state): the session lowers a member to
/// the calibration level, teardown puts the user's level back.
#[tokio::test]
async fn teardown_puts_the_users_levels_back() {
    let sendspin = crate::outputs::sendspin::volume::shared();
    let ap2 = crate::outputs::ap2::volume::shared();
    let groups: crate::routing::sync_group::SharedGroups =
        Arc::new(tokio::sync::Mutex::new(crate::routing::sync_group::GroupReconciler::new()));
    let mgr = AlignManager::new(sendspin.clone(), ap2, groups);
    let node = "sendspin-dev-leveltest";
    let m = vec![member(node, MemberKind::Sendspin)];

    // The user's level before the session.
    sendspin.lock().await.set_volume(node, 77).apply().await;
    let saved: HashMap<String, u8> =
        sendspin.lock().await.volumes().iter().filter(|(n, _)| *n == node).map(|(n, v)| (n.clone(), *v)).collect();
    assert_eq!(saved.get(node), Some(&77));

    // The session solos it at the calibration default, which overwrites the level.
    mgr.apply_audibility(&m, &audible(&[node]), DEFAULT_ALIGN_LEVEL).await;
    assert_eq!(sendspin.lock().await.volumes().get(node), Some(&DEFAULT_ALIGN_LEVEL));

    // Teardown's restore (the same statements, driven from the snapshot).
    let mut pending = Vec::new();
    {
        let mut c = sendspin.lock().await;
        for (n, v) in &saved {
            pending.push(c.set_volume(n, *v));
        }
    }
    for p in pending {
        p.apply().await;
    }
    assert_eq!(sendspin.lock().await.volumes().get(node), Some(&77), "the user's level is back");
}

/// W18's silent-failure surface: one scale meets another, and only the ends of the
/// range prove which way round it is. A factor of 100 the wrong way is a clamp at
/// either rail, i.e. silence or full scale on an amplifier.
#[test]
fn the_ap2_scale_conversion_is_a_percentage_of_full_scale() {
    // The rails: 0 % is silence, 100 % is the receiver's full scale — not 100.0.
    assert_eq!(ap2_level(0), 0.0);
    assert_eq!(ap2_level(100), 1.0);
    // The session's own default, and the middle, on the receiver's unit scale.
    assert!((ap2_level(DEFAULT_ALIGN_LEVEL) - 0.20).abs() < 1e-6, "{}", ap2_level(DEFAULT_ALIGN_LEVEL));
    assert!((ap2_level(50) - 0.5).abs() < 1e-6);
    // A 1 % level must be 0.01, never 1.0 — the mistake that would be inaudible in a
    // test that only checked ordering.
    assert!((ap2_level(1) - 0.01).abs() < 1e-6);
    // Out of range is clamped here rather than at the receiver, so the value written is
    // always the value this module thinks it wrote.
    assert_eq!(ap2_level(255), 1.0);
}

/// The optionality is the point (plan §7): `ap2_volume` distinguishes "unknown" from
/// "zero", so a snapshot must too, or teardown ends up writing an invented level to a
/// device whose level is its own.
#[test]
fn an_ap2_level_is_snapshotted_only_when_the_receiver_had_a_known_one() {
    let m = vec![
        member("sendspin-dev-s", MemberKind::Sendspin),
        member("ap2-dev-known", MemberKind::Airplay2),
        member("ap2-dev-unknown", MemberKind::Airplay2),
        member("pwsink-dev-p", MemberKind::PwSink),
    ];
    let mutes: HashMap<String, bool> = [("ap2-dev-known".to_string(), true)].into_iter().collect();
    let levels: HashMap<String, f32> = [("ap2-dev-known".to_string(), 0.62)].into_iter().collect();
    let (saved_mutes, saved_levels) = ap2_snapshot(&m, &mutes, &levels);

    // Mute: an entry for every AP2 member, defaulting to "unmuted" — restoring that is
    // not an invention, it is what an untouched receiver is.
    assert_eq!(saved_mutes.len(), 2);
    assert_eq!(saved_mutes.get("ap2-dev-known"), Some(&true));
    assert_eq!(saved_mutes.get("ap2-dev-unknown"), Some(&false));
    // Level: only the member that had one. No `0.0` stand-in.
    assert_eq!(saved_levels.get("ap2-dev-known"), Some(&0.62));
    assert!(!saved_levels.contains_key("ap2-dev-unknown"), "unknown must stay unknown: {saved_levels:?}");
    // Neither map reaches past the AP2 members (sendspin has its own snapshot, pw-sink
    // has no level knob at all).
    assert!(saved_mutes.keys().chain(saved_levels.keys()).all(|n| n.starts_with("ap2-dev-")));

    // …and that is exactly what the restore side sees: a write for one, nothing for the
    // other, with both members still named so neither is silently dropped.
    let plan = restore_ap2_level_plan(&m, &saved_levels);
    assert_eq!(plan, vec![("ap2-dev-known".to_string(), Some(0.62)), ("ap2-dev-unknown".to_string(), None)]);
}

/// W15 through the session: a pw-sink member is a member, and the status the UI
/// reads says what cannot be done to it (plan §7) — for a member that genuinely has no
/// level knob, which since W20 means "no agent is answering for it", not "it is a
/// pw-sink". Asserted both before any host has been asked (the pessimistic seed) and
/// after a real resolution pass with no seam wired at all.
#[tokio::test]
async fn a_pwsink_member_with_no_agent_is_reported_as_unlevellable_in_the_status() {
    let f = UnionFixture::new("pwsink", &[("sendspin-dev-pwska", MemberKind::Sendspin), ("pwsink-dev-office", MemberKind::PwSink)]).await;
    let expect_reported = |state: AlignState, when: &str| {
        assert_eq!(state.unlevellable, vec!["pwsink-dev-office".to_string()], "{when}");
        assert_eq!(state.level_channels.get("pwsink-dev-office"), Some(&LevelChannel::None), "{when}");
        assert_eq!(state.level_channels.get("sendspin-dev-pwska"), Some(&LevelChannel::SendspinLive), "{when}");
        let note = state.level_note.expect("un-levellable members are named in the status");
        assert!(note.contains("'office'"), "the display name, not the node name: {note}");
        assert!(note.contains("clips") && note.contains("cannot rescue"), "the §7 danger is stated: {note}");
        assert!(note.contains("no level control"), "{note}");
    };
    // Before anyone has asked a host: the seed, and it errs towards warning.
    expect_reported(f.mgr.status().await, "seeded");
    assert!(f.mgr.status().await.members.iter().any(|m| m.kind == MemberKind::PwSink));
    // And after a pass that really resolved it (no seam ⇒ nothing can level it).
    f.mgr.solo("sendspin-dev-pwska".into(), 20).await.unwrap();
    expect_reported(f.mgr.status().await, "resolved");
    let relay = crate::align::relay_delay::RelayDelay::global();
    assert!(relay.is_muted("pwsink-dev-office"), "un-levellable is not un-silenceable (W17)");
    f.mgr.stop().await;
}

/// The W20 decision on its own: "how do I set this member's level?" is a **per-output**
/// question, so two members of the same kind must resolve differently — and it must not
/// take a knob away from a member whose transport carries one.
#[tokio::test]
async fn the_level_channel_is_resolved_per_output_not_per_kind() {
    let m = vec![
        member("sendspin-dev-l", MemberKind::Sendspin),
        member("ap2-dev-l", MemberKind::Airplay2),
        member("pwsink-dev-l-agent", MemberKind::PwSink),
        member("pwsink-dev-l-none", MemberKind::PwSink),
    ];
    let plan = audibility_plan(&m, &audible(&["sendspin-dev-l"]));
    let channels = |resolved: Vec<(String, LevelChannel, bool)>| resolved.into_iter().map(|(_, c, _)| c).collect::<Vec<_>>();

    // No seam wired: a member whose transport carries no level has none at all. Note that
    // this is *not* the mute's answer — there the relay is a universal fallback, here
    // there is none, which is why `LevelChannel::None` has to stay reachable.
    assert_eq!(
        channels(level_plan(&plan, None).await),
        vec![LevelChannel::SendspinLive, LevelChannel::Ap2Snapshot, LevelChannel::None, LevelChannel::None]
    );

    // With a host that reports a level for one of them, that member is levelled out of
    // band and the other still is not — same kind, different answers.
    let host: Arc<dyn OutOfBandMute> = FakeHost::levelling(&[("pwsink-dev-l-agent", false, 0.7)]);
    assert_eq!(
        channels(level_plan(&plan, Some(&host)).await),
        vec![LevelChannel::SendspinLive, LevelChannel::Ap2Snapshot, LevelChannel::OutOfBand, LevelChannel::None]
    );

    // A host that owns the output but has no volume lever ("lever: none") is the *other*
    // way a pw-sink member ends up un-levellable, and it must not read as levellable just
    // because an agent is connected.
    let mute_only: Arc<dyn OutOfBandMute> = FakeHost::owning(&[("pwsink-dev-l-agent", false)]);
    assert_eq!(channels(level_plan(&plan, Some(&mute_only)).await)[2], LevelChannel::None);
    // …while its mute still goes out of band: the two capabilities are separate answers.
    assert_eq!(silence_plan(&plan, Some(&mute_only)).await[2].1, SilenceChannel::OutOfBand);

    // And the mirror image, which is the real reason they are two questions rather than
    // one: the agent's node-`Props` fallback (a virtual sink) reports a level with no mute,
    // so this member is levelled by its host and silenced by the relay.
    let level_only: Arc<dyn OutOfBandMute> = FakeHost::new(&[("pwsink-dev-l-agent", HostLevers { muted: None, level: Some(0.4) })]);
    assert_eq!(channels(level_plan(&plan, Some(&level_only)).await)[2], LevelChannel::OutOfBand);
    assert_eq!(silence_plan(&plan, Some(&level_only)).await[2].1, SilenceChannel::Relay);

    // An in-band level is never taken away by a seam that claims to own the output.
    let greedy: Arc<dyn OutOfBandMute> = FakeHost::levelling(&[("sendspin-dev-l", false, 0.5), ("ap2-dev-l", false, 0.5)]);
    assert_eq!(channels(level_plan(&plan, Some(&greedy)).await)[..2], [LevelChannel::SendspinLive, LevelChannel::Ap2Snapshot]);

    // And the bridge to the solver agrees with all of that, so a member cannot be
    // adjustable in the solve and un-levellable in the UI.
    use crate::align::levels::LevelKnob;
    assert_eq!(LevelChannel::SendspinLive.knob(), LevelKnob::Live);
    assert_eq!(LevelChannel::Ap2Snapshot.knob(), LevelKnob::SnapshotRestore);
    assert_eq!(LevelChannel::OutOfBand.knob(), LevelKnob::SnapshotRestore, "the same shape as AP2, for the same reason");
    assert_eq!(LevelChannel::None.knob(), LevelKnob::None);
    assert!(LevelChannel::OutOfBand.is_levellable() && !LevelChannel::None.is_levellable());
}

/// W20's point: a pw-sink host with a live agent **is** levellable, so §12.2's slider
/// reaches it — and the host's own level is handed back at teardown.
#[tokio::test]
async fn a_pwsink_members_level_is_driven_by_its_agent_and_restored_at_teardown() {
    let (spin, host_node) = ("sendspin-dev-oobl", "pwsink-dev-oobl");
    let f = UnionFixture::new("oobl", &[(spin, MemberKind::Sendspin), (host_node, MemberKind::PwSink)]).await;
    // What the host's master sink was on before the session (its agent reported it).
    let host = FakeHost::levelling(&[(host_node, false, 0.62)]);
    assert!(f.mgr.set_out_of_band_mute(host.clone()));
    f.snapshot_oob(&[(host_node, 0.62)]).await;

    // The slider, aimed at the pw-sink member. Before W20 this moved nothing at all and
    // the member was reported as having no level knob.
    let state = f.mgr.solo(host_node.into(), 40).await.unwrap();
    let driven = host.level_of(host_node).expect("the session drives the host's level");
    assert!((driven - 0.40).abs() < 1e-6, "40 % of full scale on the host's own scale, got {driven}");
    assert_eq!(state.levels.get(host_node), Some(&40), "and the session records it in the level the UI reads");
    assert_eq!(state.level_channels.get(host_node), Some(&LevelChannel::OutOfBand));
    assert!(state.unlevellable.is_empty(), "a levellable member must not be named as setting the clip ceiling");
    assert!(state.level_note.is_none(), "{:?}", state.level_note);
    // Its mute followed the same solo out of band, so the level it was given is the level
    // it plays, and the relay is not also holding it down.
    assert_eq!(host.is_muted(host_node), Some(false));
    assert!(!crate::align::relay_delay::RelayDelay::global().is_muted(host_node));
    // A silenced member gets no level write, exactly as for sendspin and AP2.
    let writes = host.level_writes().len();
    f.mgr.solo(spin.into(), 30).await.unwrap();
    assert_eq!(host.level_writes().len(), writes, "the muted member was not given a level");

    f.mgr.stop().await;
    let restored = host.level_of(host_node).expect("still known after the session");
    assert!((restored - 0.62).abs() < 1e-6, "the host's own level is back, got {restored}");
}

/// The dangerous half, same as W18's: no pre-session level was known — the agent had not
/// reported one yet, which happens whenever the host's receive stream came up after the
/// snapshot pass — so teardown must write **nothing** rather than a plausible number. On
/// someone's desktop an invented level is the difference between a quiet machine and a
/// silent one.
#[tokio::test]
async fn a_host_with_no_known_pre_session_level_is_left_alone_at_teardown() {
    let (spin, host_node) = ("sendspin-dev-oobunk", "pwsink-dev-oobunk");
    let f = UnionFixture::new("oobunk", &[(spin, MemberKind::Sendspin), (host_node, MemberKind::PwSink)]).await;
    let host = FakeHost::levelling(&[(host_node, false, 0.9)]);
    f.mgr.set_out_of_band_mute(host.clone());
    // Deliberately no `snapshot_oob`: genuinely unknown.
    assert_eq!(
        restore_oob_level_plan(&[member(host_node, MemberKind::PwSink)], &HashMap::new()),
        vec![(host_node.to_string(), None)],
        "the restore plan says 'write nothing', which is what teardown must act on"
    );

    f.mgr.solo(host_node.into(), 20).await.unwrap();
    assert!((host.level_of(host_node).unwrap() - 0.20).abs() < 1e-6, "an unknown level does not stop the session driving one");
    let during = host.level_writes().len();
    assert_eq!(during, 1, "one write, the solo's: {:?}", host.level_writes());

    f.mgr.stop().await;
    assert_eq!(host.level_writes().len(), during, "teardown attempted a level write it had no value for: {:?}", host.level_writes());
    // So the host is still exactly where the calibration left it — never 0.0, which is
    // the invented value that would silence someone's machine.
    assert!((host.level_of(host_node).unwrap() - 0.20).abs() < 1e-6);
    // The mute, whose right answer *is* knowable, was still restored.
    assert_eq!(host.is_muted(host_node), Some(false));
}

/// The answer can change while a run is walking, so it is re-resolved rather than cached
/// — both ways it can go wrong: the agent is simply gone by the next position, and the
/// agent was there when the channel was resolved but the write did not take.
#[tokio::test]
async fn the_level_channel_is_re_resolved_when_an_agent_drops_mid_session() {
    let (spin, host_node) = ("sendspin-dev-oobdrop", "pwsink-dev-oobdrop");
    let f = UnionFixture::new("oobdrop", &[(spin, MemberKind::Sendspin), (host_node, MemberKind::PwSink)]).await;
    let host = FakeHost::levelling(&[(host_node, false, 0.5)]);
    f.mgr.set_out_of_band_mute(host.clone());

    let state = f.mgr.solo(host_node.into(), 30).await.unwrap();
    assert_eq!(state.level_channels.get(host_node), Some(&LevelChannel::OutOfBand));
    assert!(state.unlevellable.is_empty());

    // The agent disconnects between two positions of the walk.
    host.drop_agent(host_node);
    let state = f.mgr.solo(host_node.into(), 30).await.unwrap();
    assert_eq!(state.level_channels.get(host_node), Some(&LevelChannel::None), "the answer is re-taken, not cached at formation");
    assert_eq!(state.unlevellable, vec![host_node.to_string()], "and it is reported, not silently skipped");
    assert!(state.level_note.expect("named").contains("clip ceiling"));
    // Audibility still holds, at the relay: losing the level must not lose the solo.
    assert!(!crate::align::relay_delay::RelayDelay::global().is_muted(host_node), "this member is the soloed one");
    f.mgr.stop().await;

    // The race the capability query cannot close: `level` answered `Some`, and the write
    // still did not land. Same outcome, decided from the write rather than the query.
    let f = UnionFixture::new("oobrefuse", &[(spin, MemberKind::Sendspin), (host_node, MemberKind::PwSink)]).await;
    let host = FakeHost::refusing_levels(&[(host_node, false, 0.5)], &[host_node]);
    f.mgr.set_out_of_band_mute(host.clone());
    let state = f.mgr.solo(host_node.into(), 30).await.unwrap();
    assert_eq!(host.level_writes().len(), 1, "it did try");
    assert_eq!(state.level_channels.get(host_node), Some(&LevelChannel::None), "a write that did not take means no level knob");
    assert_eq!(state.unlevellable, vec![host_node.to_string()]);
    f.mgr.stop().await;
}

/// Teardown is a funnel every exit path reaches, and the host level is now one more thing
/// it owes. Asserted per path, because a path that skips it leaves someone's desktop at a
/// calibration volume.
#[tokio::test]
async fn every_teardown_path_restores_the_host_level_it_changed() {
    let user_level = 0.62f32;
    // 1. A normal stop.
    let (spin, host_node) = ("sendspin-dev-tdoob", "pwsink-dev-tdoob");
    let f = UnionFixture::new("tdoob", &[(spin, MemberKind::Sendspin), (host_node, MemberKind::PwSink)]).await;
    let host = FakeHost::levelling(&[(host_node, false, user_level)]);
    f.mgr.set_out_of_band_mute(host.clone());
    f.snapshot_oob(&[(host_node, user_level)]).await;
    f.mgr.solo(host_node.into(), 15).await.unwrap();
    assert!((host.level_of(host_node).unwrap() - 0.15).abs() < 1e-6, "the session took it down");
    f.mgr.stop().await;
    assert!((host.level_of(host_node).unwrap() - user_level).abs() < 1e-6, "normal stop");

    // 2. A superseding start: it re-forms (the union is different) and then fails at the
    //    adoption gate, which is what makes the old session's teardown observable here.
    let f = UnionFixture::new("tdoobreform", &[(spin, MemberKind::Sendspin), (host_node, MemberKind::PwSink)]).await;
    let host = FakeHost::levelling(&[(host_node, false, user_level)]);
    f.mgr.set_out_of_band_mute(host.clone());
    f.snapshot_oob(&[(host_node, user_level)]).await;
    f.mgr.solo(host_node.into(), 15).await.unwrap();
    let _ = f
        .mgr
        .start_outputs(&f.deps(), vec![spin.into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
        .await
        .expect_err("the fixture adopts nothing, so the re-form is refused");
    assert!((host.level_of(host_node).unwrap() - user_level).abs() < 1e-6, "superseding start");

    // 3. The safety timeout. Its watchdog takes the session and calls exactly this
    //    teardown (a 15-minute sleep is not a test), so drive that.
    let f = UnionFixture::new("tdoobtimeout", &[(spin, MemberKind::Sendspin), (host_node, MemberKind::PwSink)]).await;
    let host = FakeHost::levelling(&[(host_node, false, user_level)]);
    f.mgr.set_out_of_band_mute(host.clone());
    f.snapshot_oob(&[(host_node, user_level)]).await;
    f.mgr.solo(host_node.into(), 15).await.unwrap();
    let timed_out = f.mgr.session.lock().await.take().expect("the watchdog takes the session");
    f.mgr.teardown(timed_out).await;
    assert!((host.level_of(host_node).unwrap() - user_level).abs() < 1e-6, "safety timeout");

    // 4. A `start` that lost the race calls the same `teardown` on its own half-built
    //    session (the generation check in `begin`), and `Drop` without a release is
    //    `align_group`'s last resort, tested there. Neither can skip the restore, because
    //    both go through this one function.
    crate::align::relay_delay::RelayDelay::global().unmute_all([host_node]);
}

/// The second scale meeting point (W20). Same silent-failure surface as the AP2 one: a
/// factor of 100 the wrong way is a clamp at a rail, and a rail on someone's desktop
/// speakers is either silence or full scale.
#[test]
fn the_host_scale_conversion_is_a_percentage_of_full_scale() {
    assert_eq!(host_level(0), 0.0);
    assert_eq!(host_level(100), 1.0);
    assert!((host_level(DEFAULT_ALIGN_LEVEL) - 0.20).abs() < 1e-6, "{}", host_level(DEFAULT_ALIGN_LEVEL));
    assert!((host_level(1) - 0.01).abs() < 1e-6, "1 % must be 0.01, never 1.0");
    assert_eq!(host_level(255), 1.0, "clamped here, so the value written is the value we think we wrote");
}

/// W17, and the correction to it: silencing is a **per-output** decision, so all four
/// channels have to be exercised on one member list.
#[tokio::test]
async fn audibility_silences_every_member_through_the_channel_it_actually_has() {
    let relay = crate::align::relay_delay::RelayDelay::global();
    let (spin, ap2_node) = ("sendspin-dev-chan", "ap2-dev-chan");
    // Two pw-sink members: one whose host agent owns it, one no agent can reach (or
    // whose sink has no volume lever at all — same answer, "cannot").
    let (host_node, orphan) = ("pwsink-dev-chan-host", "pwsink-dev-chan-orphan");
    let m = vec![
        member(spin, MemberKind::Sendspin),
        member(ap2_node, MemberKind::Airplay2),
        member(host_node, MemberKind::PwSink),
        member(orphan, MemberKind::PwSink),
    ];
    let sendspin = crate::outputs::sendspin::volume::shared();
    let ap2 = crate::outputs::ap2::volume::shared();
    let new_mgr = || {
        let groups: SharedGroups = Arc::new(tokio::sync::Mutex::new(crate::routing::sync_group::GroupReconciler::new()));
        AlignManager::new(sendspin.clone(), ap2.clone(), groups)
    };

    // 1. No out-of-band silencer wired: a member with no in-band mute can only be
    //    silenced here, at the relay. Without that it would keep playing the click
    //    through this solo, which is the §12.3.2 hazard.
    let mgr = new_mgr();
    mgr.apply_audibility(&m, &audible(&[spin]), 20).await;
    assert!(relay.is_muted(host_node) && relay.is_muted(orphan), "no in-band mute and no host ⇒ the relay silences them");
    assert!(!relay.is_muted(spin) && !relay.is_muted(ap2_node), "a member that mutes itself is never relay-muted");
    // …and the in-band channels are used exactly as before: the device knowing it is
    // muted is preferred wherever it is available. (The sendspin mute itself is
    // *transient* by design — `set_mute` stores no desired state — so what is
    // observable in-process is the calibration level that goes with it.)
    assert_eq!(sendspin.lock().await.volumes().get(spin), Some(&20), "the soloed member went down the sendspin channel");
    assert_eq!(ap2.lock().await.mutes().get(ap2_node), Some(&true), "AP2 is muted by the receiver itself");

    // 2. With a host agent that owns one of them, that member is silenced out of band —
    //    better, because its stream keeps flowing and its jitter buffer never
    //    re-anchors — and the relay must NOT also be holding it down.
    let mgr = new_mgr();
    let host = FakeHost::owning(&[(host_node, false)]);
    assert!(mgr.set_out_of_band_mute(host.clone()));
    assert!(!mgr.set_out_of_band_mute(host.clone()), "installed once");
    mgr.apply_audibility(&m, &audible(&[spin]), 20).await;
    assert_eq!(host.is_muted(host_node), Some(true), "the host silenced it");
    assert!(!relay.is_muted(host_node), "so the relay released it — never both");
    assert!(relay.is_muted(orphan), "the member no agent owns still needs the fallback");

    // 3. Soloing the host-owned member releases it on both mechanisms, and the decision
    //    is re-taken every time (an agent can drop mid-run).
    mgr.apply_audibility(&m, &audible(&[host_node]), 20).await;
    assert_eq!(host.is_muted(host_node), Some(false));
    assert!(!relay.is_muted(host_node));
    assert!(relay.is_muted(orphan));
    assert!(!relay.is_muted(spin), "the now-silent sendspin member is still muted in band, never at the relay");

    // 4. A member list that mutes itself entirely creates no relay state at all — no
    //    entry, so the RT gate this session contributes stays closed and a router with
    //    no unmutable member pays nothing for the mechanism. (Asserted per output
    //    rather than on `any_muted`, which is process-global and shared with the tests
    //    running beside this one.)
    let mgr = new_mgr();
    mgr.apply_audibility(&m[..2], &audible(&[spin]), 20).await;
    assert!(relay.status(spin).is_none() && relay.status(ap2_node).is_none(), "nothing to silence at the relay ⇒ no entries");

    relay.unmute_all([host_node, orphan]);
}

/// The fallback has to survive the race it exists for: capability was `Some` when the
/// channel was resolved and the write still did not take.
#[tokio::test]
async fn a_host_write_that_does_not_take_falls_back_to_the_relay_mute() {
    let relay = crate::align::relay_delay::RelayDelay::global();
    let (spin, node) = ("sendspin-dev-refuse", "pwsink-dev-refuse");
    let m = vec![member(spin, MemberKind::Sendspin), member(node, MemberKind::PwSink)];
    let groups: SharedGroups = Arc::new(tokio::sync::Mutex::new(crate::routing::sync_group::GroupReconciler::new()));
    let mgr = AlignManager::new(crate::outputs::sendspin::volume::shared(), crate::outputs::ap2::volume::shared(), groups);
    mgr.set_out_of_band_mute(FakeHost::refusing(&[(node, false)], &[node]));
    mgr.apply_audibility(&m, &audible(&[spin]), 20).await;
    assert!(relay.is_muted(node), "an agent that went away must not leave the member audible");
    relay.unmute_all([node]);
}

/// Every path out of a session has to give audibility back. The mute is dropped by
/// `ExclusiveHold::release`, so this is really "every path reaches release" — asserted
/// per path, because that is the property that breaks silently.
#[tokio::test]
async fn every_teardown_path_drops_the_relay_calibration_mute() {
    let relay = crate::align::relay_delay::RelayDelay::global();

    // 1. A normal stop.
    let f = UnionFixture::new("mutestop", &[("sendspin-dev-mutestop", MemberKind::Sendspin), ("pwsink-dev-mutestop", MemberKind::PwSink)])
        .await;
    f.mgr.solo("sendspin-dev-mutestop".into(), 20).await.unwrap();
    assert!(relay.is_muted("pwsink-dev-mutestop"), "the solo silenced the member that cannot silence itself");
    f.mgr.stop().await;
    assert!(!relay.is_muted("pwsink-dev-mutestop"), "a normal stop gives audibility back");

    // 2. A superseding start. It re-forms (the union is different) and fails at the
    //    adoption gate, which is exactly what makes the teardown observable here — the
    //    old session is gone either way, so its mute must be gone with it.
    let f = UnionFixture::new(
        "mutereform",
        &[("sendspin-dev-mutereform", MemberKind::Sendspin), ("pwsink-dev-mutereform", MemberKind::PwSink)],
    )
    .await;
    f.mgr.solo("sendspin-dev-mutereform".into(), 20).await.unwrap();
    assert!(relay.is_muted("pwsink-dev-mutereform"));
    let _ = f
        .mgr
        .start_outputs(&f.deps(), vec!["sendspin-dev-mutereform".into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
        .await
        .expect_err("the fixture adopts nothing, so the re-form is refused");
    assert!(!relay.is_muted("pwsink-dev-mutereform"), "a superseded session must not leave a speaker silent");

    // 3. The safety timeout. Its watchdog takes the session and calls exactly this
    //    teardown (a 15-minute sleep is not a test), so drive that.
    let f = UnionFixture::new(
        "mutetimeout",
        &[("sendspin-dev-mutetimeout", MemberKind::Sendspin), ("pwsink-dev-mutetimeout", MemberKind::PwSink)],
    )
    .await;
    f.mgr.solo("sendspin-dev-mutetimeout".into(), 20).await.unwrap();
    assert!(relay.is_muted("pwsink-dev-mutetimeout"));
    let timed_out = f.mgr.session.lock().await.take().expect("the watchdog takes the session");
    f.mgr.teardown(timed_out).await;
    assert!(!relay.is_muted("pwsink-dev-mutetimeout"), "the timeout path restores audibility too");

    // 4. `Drop` without a release is `align_group`'s last resort, tested there.
}

/// W18: the level slider has to *reach* an AP2 member — and then hand the receiver's own
/// level back. Driven through the real `Ap2Control`, whose desired-volume map is what a
/// live sender would be told to send.
#[tokio::test]
async fn an_ap2_members_level_is_driven_for_the_session_and_restored_at_teardown() {
    let (spin, ap2_node) = ("sendspin-dev-ap2lvl", "ap2-dev-ap2lvl");
    let f = UnionFixture::new("ap2lvl", &[(spin, MemberKind::Sendspin), (ap2_node, MemberKind::Airplay2)]).await;
    // What the receiver was on before the session, known to us because something read it
    // (`note_reported_volume`) or the user set it.
    f.ap2.lock().await.set_volume(ap2_node, 0.62);
    f.snapshot(&[(spin, 77)], &[(ap2_node, 0.62)]).await;

    // §12.2's slider, aimed at the AP2 member. Before W18 this moved nothing at all.
    let state = f.mgr.solo(ap2_node.into(), 40).await.unwrap();
    let driven = f.ap2.lock().await.volumes().get(ap2_node).copied().expect("the session drives the AP2 level");
    assert!((driven - 0.40).abs() < 1e-6, "40 % of full scale on the receiver's own scale, got {driven}");
    assert_eq!(state.levels.get(ap2_node), Some(&40), "and the session records it in the level the UI reads");
    // Its mute follows the same solo, so the level it was just given is the level it plays.
    assert_eq!(f.ap2.lock().await.mutes().get(ap2_node), Some(&false));

    f.mgr.stop().await;
    let restored = f.ap2.lock().await.volumes().get(ap2_node).copied().expect("still known after the session");
    assert!((restored - 0.62).abs() < 1e-6, "the receiver's own level is back, got {restored}");
}

/// The other half of W18, and the one that is dangerous to get wrong: no pre-session
/// level was known, so teardown must write **nothing** rather than a plausible-looking
/// number (plan §7 — an AP2 level is the device's, and `0.0` would leave an amplifier
/// silent while the calibration default would leave it quiet).
#[tokio::test]
async fn an_ap2_member_with_no_known_pre_session_level_is_left_alone_at_teardown() {
    let (spin, ap2_node) = ("sendspin-dev-ap2unk", "ap2-dev-ap2unk");
    let f = UnionFixture::new("ap2unk", &[(spin, MemberKind::Sendspin), (ap2_node, MemberKind::Airplay2)]).await;
    // Nothing has read this receiver and the user has set nothing: genuinely unknown.
    assert!(!f.ap2.lock().await.volumes().contains_key(ap2_node));
    f.snapshot(&[(spin, 77)], &[]).await;
    assert_eq!(
        restore_ap2_level_plan(&[member(ap2_node, MemberKind::Airplay2)], &HashMap::new()),
        vec![(ap2_node.to_string(), None)],
        "the restore plan says 'write nothing', which is what teardown must act on"
    );

    f.mgr.solo(ap2_node.into(), 20).await.unwrap();
    let driven = f.ap2.lock().await.volumes().get(ap2_node).copied().expect("an unknown level does not stop the session driving one");
    assert!((driven - 0.20).abs() < 1e-6);

    f.mgr.stop().await;
    // Stronger than "did not overwrite": teardown *removes the session's own mark*, so
    // the receiver is back to genuinely unknown — the state it was in before — rather
    // than carrying a level this daemon invented or a `user_set` claim it never made.
    // (`Ap2Control::forget_volume`; the assertion used to be "still 0.20", which only
    // proved we had not overwritten our own value.)
    let after = f.ap2.lock().await.volumes().get(ap2_node).copied();
    assert_eq!(after, None, "an unknown pre-session level must end unknown, not {after:?}");
    assert_ne!(after, Some(0.0), "0.0 is the invented value that would silence a receiver");
}

/// W19's core claim: the levels are the **session's**, per member, so tuning one speaker
/// cannot forget another's — which is what a page reload used to lose.
#[tokio::test]
async fn per_member_levels_survive_a_solo_of_a_different_member() {
    let (a, b, c) = ("sendspin-dev-lvla", "sendspin-dev-lvlb", "ap2-dev-lvlc");
    let f = UnionFixture::new("lvlmap", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin), (c, MemberKind::Airplay2)]).await;

    // Walk position 1: the near speaker needs very little.
    f.mgr.solo(a.into(), 12).await.unwrap();
    // Position 2: the far one needs much more — and A's 12 must still be there.
    let state = f.mgr.solo(b.into(), 55).await.unwrap();
    assert_eq!(state.levels.get(a), Some(&12), "the previous member's level is not forgotten by a solo of another");
    assert_eq!(state.levels.get(b), Some(&55));
    assert_eq!(state.volume, 55, "`volume` still tracks the most recent level (the frontend's fallback)");
    // A member nobody has tuned yet is absent, so `levels[node] ?? volume` is answerable
    // without a "not set" sentinel.
    assert!(!state.levels.contains_key(c));

    // Coming back to A re-applies *its* level, and reading it back is what a reloaded
    // page does — this is the whole reason the map is server-side.
    f.mgr.solo(a.into(), 12).await.unwrap();
    let reloaded = f.mgr.status().await;
    assert_eq!(reloaded.levels.get(a), Some(&12));
    assert_eq!(reloaded.levels.get(b), Some(&55));
    f.mgr.stop().await;
}

/// `AlignState::levels` must describe what the speakers were *given*, not a second
/// opinion — so it is asserted against the control store the level was written to.
#[tokio::test]
async fn align_state_levels_report_what_was_applied() {
    let (a, b, c) = ("sendspin-dev-appa", "sendspin-dev-appb", "sendspin-dev-appc");
    let f = UnionFixture::new("applied", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin), (c, MemberKind::Sendspin)]).await;

    // §7's all-play round: N members audible at one level, recorded against all of them.
    let state = f.mgr.set_audible(vec![a.into(), b.into()], 30).await.unwrap();
    assert_eq!(state.levels.get(a), Some(&30));
    assert_eq!(state.levels.get(b), Some(&30));
    assert_eq!(f.sendspin.lock().await.volumes().get(a), Some(&30), "and that is the level the device was given");
    assert_eq!(f.sendspin.lock().await.volumes().get(b), Some(&30));

    // A live slider move reaches exactly the audible members, so it is recorded against
    // exactly those.
    let state = f.mgr.set_level(45).await.unwrap();
    assert_eq!(state.levels.get(a), Some(&45));
    assert_eq!(state.levels.get(b), Some(&45));
    assert!(!state.levels.contains_key(c), "a muted member was not given 45, so it must not claim to have it");
    assert_eq!(f.sendspin.lock().await.volumes().get(a), Some(&45));

    // Out-of-range input is clamped before it is recorded, so the map and the device
    // cannot disagree about what 200 meant.
    let state = f.mgr.solo(c.into(), 200).await.unwrap();
    assert_eq!(state.levels.get(c), Some(&100));
    assert_eq!(f.sendspin.lock().await.volumes().get(c), Some(&100));
    f.mgr.stop().await;
}

/// Teardown is a funnel every exit path reaches, and the level restore is now two
/// obligations rather than one (sendspin's *and* AP2's). Asserted per path, because a
/// path that skips a restore leaves a speaker quiet until someone notices by ear.
#[tokio::test]
async fn every_teardown_path_restores_the_levels_it_changed() {
    // The pre-session state each path has to reproduce.
    let (user_spin, user_ap2) = (77u8, 0.62f32);
    let expect_restored = |spin: Option<&u8>, ap2: Option<f32>, path: &str| {
        assert_eq!(spin, Some(&user_spin), "{path}: the sendspin level the user had");
        let ap2 = ap2.unwrap_or_else(|| panic!("{path}: the AP2 level went missing entirely"));
        assert!((ap2 - user_ap2).abs() < 1e-6, "{path}: the AP2 receiver's own level is back, got {ap2}");
    };

    // 1. A normal stop.
    let (spin, ap2_node) = ("sendspin-dev-tdstop", "ap2-dev-tdstop");
    let f = UnionFixture::new("tdstop", &[(spin, MemberKind::Sendspin), (ap2_node, MemberKind::Airplay2)]).await;
    f.ap2.lock().await.set_volume(ap2_node, user_ap2);
    f.sendspin.lock().await.set_volume(spin, user_spin).apply().await;
    f.snapshot(&[(spin, user_spin)], &[(ap2_node, user_ap2)]).await;
    f.mgr.set_audible(vec![spin.into(), ap2_node.into()], 15).await.unwrap();
    assert_eq!(f.sendspin.lock().await.volumes().get(spin), Some(&15), "the session took both members down");
    f.mgr.stop().await;
    expect_restored(f.sendspin.lock().await.volumes().get(spin), f.ap2.lock().await.volumes().get(ap2_node).copied(), "normal stop");

    // 2. A superseding start: it re-forms (the union is different) and then fails at the
    //    adoption gate, which is what makes the old session's teardown observable here.
    let (spin, ap2_node) = ("sendspin-dev-tdreform", "ap2-dev-tdreform");
    let f = UnionFixture::new("tdreform", &[(spin, MemberKind::Sendspin), (ap2_node, MemberKind::Airplay2)]).await;
    f.ap2.lock().await.set_volume(ap2_node, user_ap2);
    f.sendspin.lock().await.set_volume(spin, user_spin).apply().await;
    f.snapshot(&[(spin, user_spin)], &[(ap2_node, user_ap2)]).await;
    f.mgr.set_audible(vec![spin.into(), ap2_node.into()], 15).await.unwrap();
    let _ = f
        .mgr
        .start_outputs(&f.deps(), vec![spin.into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
        .await
        .expect_err("the fixture adopts nothing, so the re-form is refused");
    expect_restored(f.sendspin.lock().await.volumes().get(spin), f.ap2.lock().await.volumes().get(ap2_node).copied(), "superseding start");

    // 3. The safety timeout. Its watchdog takes the session and calls exactly this
    //    teardown (a 15-minute sleep is not a test), so drive that.
    let (spin, ap2_node) = ("sendspin-dev-tdtimeout", "ap2-dev-tdtimeout");
    let f = UnionFixture::new("tdtimeout", &[(spin, MemberKind::Sendspin), (ap2_node, MemberKind::Airplay2)]).await;
    f.ap2.lock().await.set_volume(ap2_node, user_ap2);
    f.sendspin.lock().await.set_volume(spin, user_spin).apply().await;
    f.snapshot(&[(spin, user_spin)], &[(ap2_node, user_ap2)]).await;
    f.mgr.set_audible(vec![spin.into(), ap2_node.into()], 15).await.unwrap();
    let timed_out = f.mgr.session.lock().await.take().expect("the watchdog takes the session");
    f.mgr.teardown(timed_out).await;
    expect_restored(f.sendspin.lock().await.volumes().get(spin), f.ap2.lock().await.volumes().get(ap2_node).copied(), "safety timeout");

    // 4. A `start` that lost the race calls the same `teardown` on its own half-built
    //    session (asserted through the generation check in `begin`), and `Drop` without a
    //    release is `align_group`'s last resort, tested there.
}

#[test]
fn the_default_calibration_level_is_twenty() {
    // Plan §12.2: 50 was ~40 dB above the estimator's target in a real room.
    assert_eq!(DEFAULT_ALIGN_LEVEL, 20);
    assert_eq!(AlignState::inactive().volume, DEFAULT_ALIGN_LEVEL);
    assert_eq!(AlignState::inactive().mode, AlignMode::MultiPosition);
    // No session, no per-member levels: they are the session's, never a stored default
    // (W19 — a persisted level would be a promise about a phone position nobody made).
    assert!(AlignState::inactive().levels.is_empty());
}

/// Plan §12.2's stereo-pair remedy, through the session: the choice is per member, it is
/// what the status reports, and **teardown gives both channels back**.
///
/// The last part is the one that would be silent if it broke: a mask left behind would
/// keep half of somebody's stereo pair silent for as long as the daemon runs, which is
/// the calibration mute's failure mode with a longer fuse.
#[tokio::test]
async fn a_member_can_be_measured_through_one_channel_and_gets_both_back() {
    let f = UnionFixture::new("chan", &[("pwsink-dev-desk", MemberKind::PwSink), ("sendspin-dev-chana", MemberKind::Sendspin)]).await;
    let relay = crate::align::relay_delay::RelayDelay::global();
    relay.unmask_all(["pwsink-dev-desk", "sendspin-dev-chana"]); // a shared global: start clean

    // Absent from the map ⇒ both, so a consumer never distinguishes "unset" from
    // "set to the default".
    assert!(f.mgr.status().await.channels.is_empty());
    assert_eq!(relay.channels("pwsink-dev-desk"), MeasureChannels::Both);

    let state = f.mgr.set_channels("pwsink-dev-desk".into(), MeasureChannels::Left).await.unwrap();
    assert_eq!(state.channels.get("pwsink-dev-desk"), Some(&MeasureChannels::Left));
    assert_eq!(state.channels.len(), 1, "one member's choice is not the group's");
    assert_eq!(relay.channels("pwsink-dev-desk"), MeasureChannels::Left, "and it reached the relay hook");
    assert_eq!(relay.channels("sendspin-dev-chana"), MeasureChannels::Both);

    // A solo elsewhere must not disturb it: the choice belongs to the member, not to
    // whoever is currently audible.
    f.mgr.solo("sendspin-dev-chana".into(), 20).await.unwrap();
    assert_eq!(f.mgr.status().await.channels.get("pwsink-dev-desk"), Some(&MeasureChannels::Left));
    assert_eq!(relay.channels("pwsink-dev-desk"), MeasureChannels::Left);

    // Back to both by hand: the entry goes, rather than being recorded as a default.
    let state = f.mgr.set_channels("pwsink-dev-desk".into(), MeasureChannels::Both).await.unwrap();
    assert!(state.channels.is_empty());

    // And teardown clears it even when the user never did.
    f.mgr.set_channels("pwsink-dev-desk".into(), MeasureChannels::Right).await.unwrap();
    assert!(f.mgr.set_channels("nobody-dev-x".into(), MeasureChannels::Left).await.is_err(), "only members");
    f.mgr.stop().await;
    assert_eq!(relay.channels("pwsink-dev-desk"), MeasureChannels::Both, "teardown gives both channels back");
    // Per-output, never `any_active()`: this is the process-global relay and the other
    // tests in this file legitimately hold their own members' mutes at the same time.
    assert!(relay.status("pwsink-dev-desk").is_none(), "and leaves no entry behind on it at all");
}
