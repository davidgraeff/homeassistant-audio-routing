//! Tests for the alignment session: audibility resolution, teardown, and the by-ear path.

use super::*;

/// The safety timeout must be an **idle** timeout, not a deadline from `start`.
///
/// A near-field walk round a large apartment legitimately outlasts
/// `SESSION_TIMEOUT`, and the one-shot `sleep(SESSION_TIMEOUT)` version tore the
/// session down mid-walk as a lost session — which made §1.2's advice to keep one
/// continuous session for everything that should be coherent impossible to honour.
/// Each arrival re-solos its speaker, so the walk refreshes this for free.
#[tokio::test]
async fn an_arrival_refreshes_the_idle_timeout() {
    let f = UnionFixture::new("idle", &[("sendspin-dev-idlea", MemberKind::Sendspin), ("sendspin-dev-idleb", MemberKind::Sendspin)]).await;

    // Stand in for a long walk: idle past the point a deadline-based watchdog fired.
    f.go_idle(SESSION_TIMEOUT * 2).await;
    assert!(f.idle().await.expect("session") >= SESSION_TIMEOUT, "precondition: looks abandoned");

    // Arriving at the next speaker is a solo, and that must postpone the teardown.
    f.mgr.solo("sendspin-dev-idleb".to_string(), 30).await.expect("solo");
    assert!(f.idle().await.expect("session") < SESSION_TIMEOUT, "an arrival must postpone the teardown");
}

#[test]
fn click_wav_is_a_valid_two_second_stereo_pattern() {
    let wav = click_wav();
    // RIFF/WAVE header present.
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    // Stereo, 44100, 16-bit.
    assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), CHANNELS);
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), RATE);
    // data length = 2 s * rate * channels * 2 bytes.
    let expect = (PATTERN_SECS * RATE as f64) as usize * CHANNELS as usize * 2;
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize, expect);
}

#[test]
fn same_set_ignores_order_and_dupes_only_by_value() {
    assert!(same_set(&["a".into(), "b".into()], &["b".into(), "a".into()]));
    assert!(!same_set(&["a".into()], &["a".into(), "b".into()]));
}

fn member(node_name: &str, kind: MemberKind) -> AlignMember {
    AlignMember { node_name: node_name.to_string(), kind, node_id: None }
}

fn members() -> Vec<AlignMember> {
    vec![
        member("sendspin-dev-a", MemberKind::Sendspin),
        member("sendspin-dev-b", MemberKind::Sendspin),
        member("ap2-dev-c", MemberKind::Airplay2),
    ]
}

/// What one fake host can do, each lever independently absent — which is not a
/// contrivance: the real agent's node-`Props` fallback (a virtual sink) reports a level
/// with `mute: None`, and a host that is not receiving reports neither.
#[derive(Debug, Clone, Default)]
struct HostLevers {
    muted: Option<bool>,
    /// The host's own cubic 0.0–1.0 level.
    level: Option<f32>,
}

/// A stand-in for a pw-sink host's agent ([`OutOfBandMute`]): it owns the outputs it
/// was told about, refuses the rest (`None`, as a disconnected agent or a sink with no
/// volume lever does), and records every write so a test can assert who silenced what —
/// and, for the level, that nothing was written at all.
#[derive(Default)]
struct FakeHost {
    /// `node_name → its levers`. Absent ⇒ this seam does not own the output at all.
    state: std::sync::Mutex<HashMap<String, HostLevers>>,
    /// Mute writes that must fail, to exercise the fall back to the relay.
    refuse_writes: BTreeSet<String>,
    /// Level writes that must fail — the race where the capability was `Some` and the
    /// agent went away before the write.
    refuse_levels: BTreeSet<String>,
    /// Every attempted level write, in order. Attempts rather than successes, because
    /// "teardown must not write an invented level" is a claim about what it *tried*.
    level_writes: std::sync::Mutex<Vec<(String, f32)>>,
}

impl FakeHost {
    fn new(outputs: &[(&str, HostLevers)]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(outputs.iter().map(|(n, l)| (n.to_string(), l.clone())).collect()),
            ..Default::default()
        })
    }

    /// A host with a mute lever only (what W17's tests exercise).
    fn owning(outputs: &[(&str, bool)]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m)| (n.to_string(), HostLevers { muted: Some(*m), level: None })).collect(),
            ),
            ..Default::default()
        })
    }

    /// A host with both levers — the ordinary pw-sink case W20 is about.
    fn levelling(outputs: &[(&str, bool, f32)]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m, l)| (n.to_string(), HostLevers { muted: Some(*m), level: Some(*l) })).collect(),
            ),
            ..Default::default()
        })
    }

    fn refusing(outputs: &[(&str, bool)], refuse: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m)| (n.to_string(), HostLevers { muted: Some(*m), level: None })).collect(),
            ),
            refuse_writes: refuse.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        })
    }

    /// Both levers present, but the level write does not take.
    fn refusing_levels(outputs: &[(&str, bool, f32)], refuse: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m, l)| (n.to_string(), HostLevers { muted: Some(*m), level: Some(*l) })).collect(),
            ),
            refuse_levels: refuse.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        })
    }

    fn is_muted(&self, output: &str) -> Option<bool> {
        self.state.lock().unwrap().get(output).and_then(|l| l.muted)
    }

    fn level_of(&self, output: &str) -> Option<f32> {
        self.state.lock().unwrap().get(output).and_then(|l| l.level)
    }

    /// Stand in for an agent that disconnected mid-run: the host answers nothing at all.
    fn drop_agent(&self, output: &str) {
        self.state.lock().unwrap().remove(output);
    }

    fn level_writes(&self) -> Vec<(String, f32)> {
        self.level_writes.lock().unwrap().clone()
    }
}

impl OutOfBandMute for FakeHost {
    fn muted<'a>(&'a self, output: &'a str) -> Fut<'a, Option<bool>> {
        Box::pin(async move { self.is_muted(output) })
    }

    fn set_muted<'a>(&'a self, output: &'a str, muted: bool) -> Fut<'a, bool> {
        Box::pin(async move {
            if self.refuse_writes.contains(output) {
                return false;
            }
            let mut state = self.state.lock().unwrap();
            match state.get_mut(output).and_then(|l| l.muted.as_mut()) {
                Some(slot) => {
                    *slot = muted;
                    true
                }
                None => false,
            }
        })
    }

    fn level<'a>(&'a self, output: &'a str) -> Fut<'a, Option<f32>> {
        Box::pin(async move { self.level_of(output) })
    }

    fn set_level<'a>(&'a self, output: &'a str, level: f32) -> Fut<'a, bool> {
        Box::pin(async move {
            self.level_writes.lock().unwrap().push((output.to_string(), level));
            if self.refuse_levels.contains(output) {
                return false;
            }
            let mut state = self.state.lock().unwrap();
            match state.get_mut(output).and_then(|l| l.level.as_mut()) {
                Some(slot) => {
                    *slot = level;
                    true
                }
                None => false,
            }
        })
    }
}

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

fn audible(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
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

/// Scaffolding for the union-hold tests: a manager with a session already
/// running over `held`, without a PipeWire graph (so no anchor to wait for) and
/// without touching the process-global hold registry — see
/// `ExclusiveHold::for_test`.
struct UnionFixture {
    mgr: AlignManager,
    /// The very control stores the manager writes through, so a test can assert what the
    /// session did to a member's level without a device (both are in-process state).
    sendspin: SharedSendspinControl,
    ap2: SharedAp2Control,
    groups: SharedGroups,
    changes: crate::pw::thread::ChangeNotifier,
    _changes_rx: tokio::sync::broadcast::Receiver<()>,
    routing: crate::store::routing::SharedRouting,
    outputs: crate::store::outputs::SharedOutputs,
    hold_id: u64,
    anchor: u32,
}

impl UnionFixture {
    async fn new(tag: &str, held: &[(&str, MemberKind)]) -> Self {
        let groups: SharedGroups = Arc::new(tokio::sync::Mutex::new(crate::routing::sync_group::GroupReconciler::new()));
        let (changes, _changes_rx) = tokio::sync::broadcast::channel(8);
        let (sendspin, ap2) = (crate::outputs::sendspin::volume::shared(), crate::outputs::ap2::volume::shared());
        let mgr = AlignManager::new(sendspin.clone(), ap2.clone(), groups.clone());
        let members: Vec<AlignMember> = held.iter().map(|(n, k)| member(n, *k)).collect();
        let hold =
            crate::align::group::ExclusiveHold::for_test(&groups, &changes, members.clone(), Default::default(), AlignMode::MultiPosition)
                .await;
        let (hold_id, anchor) = (hold.id(), hold.anchor_node_id());
        let audible: BTreeSet<String> = members.iter().take(2).map(|m| m.node_name.clone()).collect();
        *mgr.session.lock().await = Some(Session {
            key: members.iter().map(|m| m.node_name.clone()).collect(),
            members: members.clone(),
            activity: Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
            reference: members.first().map(|m| m.node_name.clone()),
            target: members.get(1).map(|m| m.node_name.clone()),
            audible,
            // Deliberately not the default: a reusing start must not reset it.
            volume: 33,
            levels: BTreeMap::new(),
            stop: Arc::new(AtomicBool::new(false)),
            saved_sendspin: HashMap::new(),
            saved_sendspin_mutes: HashMap::new(),
            saved_ap2_mutes: HashMap::new(),
            saved_ap2_volumes: HashMap::new(),
            saved_oob_mutes: HashMap::new(),
            saved_oob_levels: HashMap::new(),
            // Exactly what `begin` seeds: the kinds' own answer, replaced by the first
            // audibility pass. A pw-sink member is un-levellable until a host has answered.
            level_channels: kind_level_channels(&members),
            hold_cost: form_cost_note(members.len(), "no speakers were held yet"),
            hold,
            hold_reused: false,
        });
        // Nothing adopted and no routing: only the *formation* path consults those,
        // so a re-form fails fast and visibly instead of waiting for an anchor.
        let dir = std::env::temp_dir();
        let outputs = Arc::new(std::sync::Mutex::new(
            crate::store::outputs::OutputsStore::load(&dir.join(format!("calib-outputs-{tag}-{}.json", std::process::id()))).unwrap(),
        ));
        let routing = Arc::new(std::sync::Mutex::new(
            crate::store::routing::RoutingStore::load(&dir.join(format!("calib-routing-{tag}-{}.json", std::process::id()))).unwrap(),
        ));
        Self { mgr, sendspin, ap2, groups, changes, _changes_rx, routing, outputs, hold_id, anchor }
    }

    fn deps(&self) -> HoldDeps<'_> {
        HoldDeps { groups: &self.groups, changes: &self.changes, routing: &self.routing, outputs: &self.outputs }
    }

    /// Stand in for what `begin` snapshots — the pre-session levels teardown owes the
    /// user. An AP2 member left out of `ap2` is the *unknown* case (§7): the entry is
    /// absent, not zero.
    async fn snapshot(&self, sendspin: &[(&str, u8)], ap2: &[(&str, f32)]) {
        let mut guard = self.mgr.session.lock().await;
        let session = guard.as_mut().expect("the fixture's session is running");
        session.saved_sendspin = sendspin.iter().map(|(n, v)| (n.to_string(), *v)).collect();
        session.saved_ap2_volumes = ap2.iter().map(|(n, v)| (n.to_string(), *v)).collect();
    }

    /// The out-of-band half of the same snapshot (W20), on the host's own 0.0–1.0 scale.
    /// A member left out is the *unknown* case: the entry is absent, not zero, and
    /// teardown must write nothing for it.
    async fn snapshot_oob(&self, levels: &[(&str, f32)]) {
        let mut guard = self.mgr.session.lock().await;
        let session = guard.as_mut().expect("the fixture's session is running");
        session.saved_oob_levels = levels.iter().map(|(n, v)| (n.to_string(), *v)).collect();
    }

    /// The identity a re-form would change: the hold's id and its group's anchor.
    async fn identity(&self) -> Option<(u64, u32)> {
        self.mgr.session.lock().await.as_ref().map(|s| (s.hold.id(), s.hold.anchor_node_id()))
    }

    /// How long the session has looked idle to the safety watchdog.
    async fn idle(&self) -> Option<Duration> {
        let guard = self.mgr.session.lock().await;
        let s = guard.as_ref()?;
        let elapsed = s.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner).elapsed();
        Some(elapsed)
    }

    /// Backdate the activity mark, standing in for a user who has been walking.
    async fn go_idle(&self, by: Duration) {
        let guard = self.mgr.session.lock().await;
        let s = guard.as_ref().expect("session");
        let mut a = s.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *a -= by;
    }
}

/// The core claim of plan §12.3.1: re-selecting speakers that are already held is
/// **not** a restart. A re-form means a new group, a new anchor and new per-device
/// senders — every sendspin member reconnecting twice — so the assertion is on the
/// hold's *identity*, not on some visible side effect.
#[tokio::test]
async fn re_selecting_held_speakers_keeps_the_hold_its_anchor_and_its_senders() {
    let f = UnionFixture::new(
        "subset",
        &[
            ("sendspin-dev-uniona", MemberKind::Sendspin),
            ("sendspin-dev-unionb", MemberKind::Sendspin),
            ("ap2-dev-unionc", MemberKind::Airplay2),
        ],
    )
    .await;
    let before = f.identity().await.unwrap();
    assert_eq!(before, (f.hold_id, f.anchor));

    // A position that hears two of the three — and a different mode, which is a
    // property of the run rather than of the group.
    let state = f
        .mgr
        .start_outputs(&f.deps(), vec!["sendspin-dev-unionb".into(), "sendspin-dev-uniona".into()], AlignMode::NearField)
        .await
        .expect("a subset of the held union starts inside the hold");
    assert_eq!(f.identity().await, Some(before), "same hold, same anchor: nothing re-formed, nothing reconnected");
    assert_eq!(state.hold_id, f.hold_id);
    assert!(state.hold_reused);
    assert!(state.hold_cost.contains("no speaker reconnected"), "{}", state.hold_cost);
    assert_eq!(state.mode, AlignMode::NearField);
    // The hold is still the whole union; the *selection* is this position's.
    assert_eq!(state.outputs.len(), 3);
    // `start_outputs` normalises the key it echoes (sorted, deduped).
    assert_eq!(state.sources, vec!["sendspin-dev-uniona".to_string(), "sendspin-dev-unionb".to_string()]);
    assert_eq!(state.audible, vec!["sendspin-dev-uniona".to_string(), "sendspin-dev-unionb".to_string()]);
    assert_eq!(state.volume, 33, "a re-scope must not reset a tuned level");
    // …and it records that level against the members it just applied it to, so the map
    // and the speakers agree on every path that applies a level (W19).
    assert_eq!(state.levels.get("sendspin-dev-uniona"), Some(&33));
    assert_eq!(state.levels.get("sendspin-dev-unionb"), Some(&33));
    assert!(!state.levels.contains_key("ap2-dev-unionc"), "the member this position cannot hear was given nothing");
    assert_eq!(f.groups.lock().await.align_hold_outputs().len(), 3, "the routing override still covers the union");

    // The same union again, unsorted and with a duplicate (the caller is a UI).
    let state = f
        .mgr
        .start_outputs(
            &f.deps(),
            vec!["ap2-dev-unionc".into(), "sendspin-dev-uniona".into(), "ap2-dev-unionc".into(), "sendspin-dev-unionb".into()],
            AlignMode::MultiPosition,
        )
        .await
        .unwrap();
    assert_eq!(f.identity().await, Some(before), "the unchanged union does not re-form either");
    assert!(state.hold_reused);
    f.mgr.stop().await;
}

/// The other side of the same rule: a speaker the hold does not cover *must*
/// re-form, which tears the running session down. Here the re-form then fails on
/// the adoption gate (nothing is adopted in the fixture), which is exactly what
/// makes it observable without a graph — a reuse would have returned `Ok` and left
/// the session running.
#[tokio::test]
async fn a_genuinely_different_union_re_forms_and_gives_the_old_hold_back() {
    let f = UnionFixture::new("reform", &[("sendspin-dev-reforma", MemberKind::Sendspin), ("sendspin-dev-reformb", MemberKind::Sendspin)])
        .await;
    assert!(f.identity().await.is_some());
    let err = f
        .mgr
        .start_outputs(&f.deps(), vec!["sendspin-dev-reforma".into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
        .await
        .expect_err("the fixture adopts nothing, so the re-form is refused at the adoption gate");
    assert!(err.contains("Outputs page"), "{err}");
    assert!(f.identity().await.is_none(), "the previous session was torn down: this was a re-form, not a re-scope");
    assert!(f.groups.lock().await.align_hold_outputs().is_empty(), "and its routing override is gone");
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

// ---- The idle timeout, as something the user can see coming -------------------
//
// The hold is exclusive (plan §12.3), so when the idle timeout fires the speakers go
// back to normal and any wizard on screen is describing a session that no longer
// exists. A real multi-position run walked into exactly that, because reading a review
// page is *quiet*. These tests pin the two halves of the fix: the remaining time is in
// the status, and its disappearance is pushed.

/// `closes_in_s` has to be a live reading of the idleness the watchdog decides on —
/// shrinking while nothing happens and jumping back the moment something does.
///
/// Asserted through the public status rather than through `activity`, because the
/// number a user counts down and the number the teardown is decided on being the same
/// number is the actual claim (they are one function, `Session::idle`).
#[tokio::test]
async fn the_reported_remaining_time_shrinks_while_idle_and_jumps_back_on_activity() {
    let (a, b) = ("sendspin-dev-cdowna", "sendspin-dev-cdownb");
    let f = UnionFixture::new("closesin", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;

    let fresh = f.mgr.status().await.closes_in_s.expect("a live session says when it would close");
    assert!(fresh <= SESSION_TIMEOUT.as_secs(), "never more than the whole allowance, got {fresh}");
    assert!(fresh + 5 >= SESSION_TIMEOUT.as_secs(), "a fresh session has nearly all of it, got {fresh}");

    // Five minutes of a user reading a proposal: silent, so the clock runs down.
    f.go_idle(Duration::from_secs(5 * 60)).await;
    let idled = f.mgr.status().await.closes_in_s.unwrap();
    assert!(idled < fresh, "idling has to shrink it: {idled} vs {fresh}");
    assert!(idled.abs_diff(SESSION_TIMEOUT.as_secs() - 5 * 60) <= 5, "it counts the idleness, got {idled}");

    // Soloing a speaker is the audibility change §1.2 relies on a walk making.
    f.mgr.solo(b.to_string(), 30).await.expect("solo");
    let refreshed = f.mgr.status().await.closes_in_s.unwrap();
    assert!(refreshed > idled, "an audibility change must give the whole allowance back: {refreshed} vs {idled}");
    assert!(refreshed + 5 >= SESSION_TIMEOUT.as_secs(), "…all of it, got {refreshed}");

    // The two constants that let a client phrase it honestly: it may say "15 minutes
    // without a change", and it must say "about", because the watchdog only looks every
    // `TIMEOUT_POLL`.
    let st = f.mgr.status().await;
    assert_eq!(st.idle_timeout_s, SESSION_TIMEOUT.as_secs());
    assert_eq!(st.timeout_slack_s, TIMEOUT_POLL.as_secs());
    assert!(st.timeout_slack_s > 0, "a slack of zero would invite a UI to promise a precise second");

    // Past the deadline it saturates at zero rather than wrapping — and zero means
    // "awaiting teardown", not "gone": the session is still here, and still says so.
    f.go_idle(SESSION_TIMEOUT * 2).await;
    let st = f.mgr.status().await;
    assert_eq!(st.closes_in_s, Some(0));
    assert!(st.active, "the watchdog has not looked yet, so the session is still real");

    // No session, nothing counting down — but the rules are still stated.
    f.mgr.stop().await;
    let st = f.mgr.status().await;
    assert_eq!(st.closes_in_s, None, "an inactive state has no deadline to report");
    assert_eq!(st.idle_timeout_s, SESSION_TIMEOUT.as_secs());
    assert_eq!(AlignState::inactive().closes_in_s, None);
}

/// The by-ear path's two steps refresh the timer too.
///
/// `Session::activity`'s doc comment always claimed `select` and `set_level` did this
/// and only `set_audible` ever called `note_activity` — so a by-ear session being
/// compared pair by pair for a quarter of an hour was torn down as abandoned. The UI now
/// *tells* the user that changing what they hear or its level refreshes the timer, which
/// makes the discrepancy a promise rather than a doc bug.
#[tokio::test]
async fn the_by_ear_steps_refresh_the_idle_timeout_as_the_docs_always_claimed() {
    let (a, b) = ("sendspin-dev-byeara", "sendspin-dev-byearb");
    let f = UnionFixture::new("byearidle", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;

    // Picking a reference/target pair.
    f.go_idle(Duration::from_secs(10 * 60)).await;
    f.mgr.select(a.to_string(), b.to_string()).await.expect("select");
    assert!(f.idle().await.expect("session") < Duration::from_secs(60), "`select` refreshes the idle mark");

    // Dragging the level.
    f.go_idle(Duration::from_secs(10 * 60)).await;
    f.mgr.set_level(42).await.expect("set_level");
    assert!(f.idle().await.expect("session") < Duration::from_secs(60), "`set_level` refreshes the idle mark");

    // The explicit "I am still here", which is the only remedy on a page that is
    // otherwise silent by nature (a proposal being read).
    f.go_idle(Duration::from_secs(14 * 60)).await;
    let st = f.mgr.still_here().await.expect("a live session can be kept open");
    assert!(st.closes_in_s.unwrap() + 5 >= SESSION_TIMEOUT.as_secs(), "one whole fresh allowance, got {:?}", st.closes_in_s);

    // …and *parking the run* is not activity, on purpose: `silence()` is what a run does
    // when it stops playing to let the user read, which is exactly the case the countdown
    // exists for. If it refreshed the timer there would be nothing to warn about.
    f.go_idle(Duration::from_secs(10 * 60)).await;
    let before = f.mgr.status().await.closes_in_s.unwrap();
    f.mgr.silence().await.expect("silence");
    let after = f.mgr.status().await.closes_in_s.unwrap();
    assert!(after <= before, "parking a run must keep the watchdog counting: {after} vs {before}");

    f.mgr.stop().await;
    assert!(f.mgr.still_here().await.is_err(), "there is nothing to keep open once it has stopped");
}

/// The push half: every exit path bumps the notifier, so a client hears about the
/// teardown instead of noticing it at the next poll.
///
/// The notifier lives on the **manager** for exactly this reason — one owned by the
/// session would be dropped by the event it exists to report.
#[tokio::test]
async fn the_notifier_fires_on_teardown_so_the_disappearance_is_pushed() {
    let (a, b) = ("sendspin-dev-pusha", "sendspin-dev-pushb");
    let f = UnionFixture::new("pushstop", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    let mut rx = f.mgr.subscribe();
    assert!(!rx.has_changed().unwrap(), "a fresh subscription is up to date, not pre-armed");

    // An ordinary change first, so the test cannot pass by never distinguishing them.
    f.mgr.solo(b.to_string(), 25).await.expect("solo");
    assert!(rx.has_changed().unwrap(), "a change to a live session is pushed");
    rx.borrow_and_update();

    f.mgr.stop().await;
    assert!(rx.has_changed().unwrap(), "the teardown is pushed");
    rx.borrow_and_update();
    assert!(!f.mgr.status().await.active, "…and what a subscriber then reads is the inactive state");

    // The *other* teardown paths go through the same `teardown`, which is where the bump
    // is: a superseding start that re-forms (and here fails at the adoption gate, leaving
    // no session) has to reach a subscriber too.
    let f = UnionFixture::new("pushreform", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    let rx = f.mgr.subscribe();
    let _ = f
        .mgr
        .start_outputs(&f.deps(), vec![a.into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
        .await
        .expect_err("the fixture adopts nothing, so the re-form is refused");
    assert!(rx.has_changed().unwrap(), "a superseding start's teardown is pushed as well");
    assert!(!f.mgr.status().await.active);
}

/// The watchdog end to end: an idle session is taken, torn down, and the teardown is
/// pushed — the sequence a review page left open overnight actually produces.
///
/// Paused clock, so the 15 minutes cost nothing. This is also what pins the watchdog
/// being a *loop*: it sleeps in `TIMEOUT_POLL` slices and only fires once idleness has
/// really accumulated, so a session refreshed in between survives the same task.
#[tokio::test(start_paused = true)]
async fn an_idle_session_is_torn_down_by_its_watchdog_and_the_close_is_pushed() {
    let (a, b) = ("sendspin-dev-wdoga", "sendspin-dev-wdogb");
    let f = UnionFixture::new("watchdog", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    let mut rx = f.mgr.subscribe();
    let stop = f.mgr.session.lock().await.as_ref().expect("session").stop.clone();
    f.mgr.arm_timeout(stop);

    // One slice in, with the user still working: it must not fire.
    f.mgr.solo(b.to_string(), 20).await.expect("solo");
    rx.borrow_and_update();
    tokio::time::sleep(TIMEOUT_POLL * 2).await;
    assert!(f.mgr.status().await.active, "a session in use survives the watchdog's checks");

    // Now stop touching it. Rather than sleeping fifteen paused minutes with the session
    // lock changing hands, backdate the mark the watchdog reads — which is precisely what
    // a quarter of an hour of reading does to it.
    f.go_idle(SESSION_TIMEOUT).await;
    tokio::time::timeout(SESSION_TIMEOUT, rx.changed()).await.expect("the teardown is pushed").expect("notifier alive");

    let st = f.mgr.status().await;
    assert!(!st.active, "the watchdog gave the speakers back");
    assert_eq!(st.closes_in_s, None);
    assert!(f.groups.lock().await.align_hold_outputs().is_empty(), "…and released the exclusive hold with them");
}

/// Plan §11's requirement for the session socket: **one full state on connect**, so a
/// client needs no separate initial fetch.
///
/// Driven over a real socket (a hand-rolled handshake — no test-only WebSocket client
/// dependency) because the claim is about the transport: `status_socket` sends before it
/// waits, and a version that waited for the first *change* would look fine in every
/// unit test and leave a wizard blank until the user touched something.
#[tokio::test]
async fn a_socket_opened_while_a_session_is_live_gets_the_current_state_at_once() {
    let (a, b) = ("sendspin-dev-wsa", "sendspin-dev-wsb");
    let f = UnionFixture::new("sessionws", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    f.mgr.solo(b.to_string(), 31).await.expect("solo");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let mgr = f.mgr.clone();
    // The same wiring `api::align::align_ws` does, minus `AppState`.
    let app = axum::Router::new().route(
        "/ws",
        axum::routing::get(move |ws: axum::extract::ws::WebSocketUpgrade| {
            let mgr = mgr.clone();
            async move {
                ws.on_upgrade(move |socket| async move {
                    let changes = mgr.subscribe();
                    crate::align::status_ws::status_socket(socket, changes, move || {
                        let mgr = mgr.clone();
                        Box::pin(async move { serde_json::to_string(&mgr.status().await).ok() })
                    })
                    .await;
                })
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let mut sock = ws_connect(addr).await;
    // Not preceded by any request of ours: whatever arrives is unprompted. Read as a
    // `Value` because `AlignState` is a serialise-only DTO — and because the assertions
    // that matter are about the *wire* shape a client parses.
    let first = ws_read_status(&mut sock).await;
    assert_eq!(first["active"], serde_json::json!(true), "the socket describes the session that is already running");
    assert_eq!(first["audible"], serde_json::json!([b]), "…including where the run currently is");
    assert_eq!(first["volume"], serde_json::json!(31));
    assert!(first["closes_in_s"].as_u64().is_some(), "and when it would close: {}", first["closes_in_s"]);
    assert_eq!(first["timeout_slack_s"], serde_json::json!(TIMEOUT_POLL.as_secs()));
    // Full state, not a delta: a client must not need a separate initial fetch.
    for field in ["members", "outputs", "levels", "level_channels", "hold_id", "displaced"] {
        assert!(!first[field].is_null(), "the connect frame is a whole status; '{field}' is missing");
    }

    // Then one frame per change, ending with the one that says it is over — which is the
    // frame the whole socket exists for.
    f.mgr.solo(a.to_string(), 44).await.expect("solo");
    let next = ws_read_status(&mut sock).await;
    assert_eq!(next["audible"], serde_json::json!([a]));

    f.mgr.stop().await;
    let closed = ws_read_status(&mut sock).await;
    assert_eq!(closed["active"], serde_json::json!(false), "the teardown arrives as an event");
    assert_eq!(closed["closes_in_s"], serde_json::Value::Null);

    server.abort();
}

/// Open a WebSocket by hand and return the stream positioned at the first frame.
///
/// A literal RFC-6455 handshake instead of a client library: the daemon has no
/// WebSocket-client dependency and one test is not a reason to grow the build. The key
/// below is the RFC's own example value — the server only has to hash it.
async fn ws_connect(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    sock.write_all(
        b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
          Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
    )
    .await
    .expect("handshake request");
    // Read exactly the response head, byte at a time, so nothing of the first frame is
    // swallowed into a buffer this function then throws away.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        sock.read_exact(&mut byte).await.expect("response head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 101"), "the upgrade was refused: {head}");
    sock
}

/// One pushed status frame, parsed.
async fn ws_read_status(sock: &mut tokio::net::TcpStream) -> serde_json::Value {
    serde_json::from_str(&ws_read_text(sock).await).expect("a status frame is JSON")
}

/// Read one unmasked text frame (what a server sends) and return its payload.
async fn ws_read_text(sock: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut head = [0u8; 2];
    sock.read_exact(&mut head).await.expect("frame header");
    assert_eq!(head[0] & 0x0f, 0x1, "expected a text frame, got opcode {:#x}", head[0] & 0x0f);
    assert_eq!(head[1] & 0x80, 0, "a server frame is never masked");
    let len = match head[1] & 0x7f {
        126 => {
            let mut ext = [0u8; 2];
            sock.read_exact(&mut ext).await.expect("16-bit length");
            u16::from_be_bytes(ext) as usize
        }
        // A status frame is hundreds of bytes and never megabytes, so the 64-bit form
        // cannot occur; failing loudly beats silently reading the wrong count.
        127 => panic!("a status frame should never need a 64-bit length"),
        short => short as usize,
    };
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload).await.expect("frame payload");
    String::from_utf8(payload).expect("a status frame is UTF-8 JSON")
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
