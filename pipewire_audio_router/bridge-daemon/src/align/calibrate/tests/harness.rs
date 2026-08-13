//! Fakes and fixtures shared by the subject modules beside this one.

use super::super::*;

pub(super) fn member(node_name: &str, kind: MemberKind) -> AlignMember {
    AlignMember { node_name: node_name.to_string(), kind, node_id: None }
}

pub(super) fn members() -> Vec<AlignMember> {
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
pub(super) struct HostLevers {
    pub(super) muted: Option<bool>,
    /// The host's own cubic 0.0–1.0 level.
    pub(super) level: Option<f32>,
}

/// A stand-in for a pw-sink host's agent ([`OutOfBandMute`]): it owns the outputs it
/// was told about, refuses the rest (`None`, as a disconnected agent or a sink with no
/// volume lever does), and records every write so a test can assert who silenced what —
/// and, for the level, that nothing was written at all.
#[derive(Default)]
pub(super) struct FakeHost {
    /// `node_name → its levers`. Absent ⇒ this seam does not own the output at all.
    pub(super) state: std::sync::Mutex<HashMap<String, HostLevers>>,
    /// Mute writes that must fail, to exercise the fall back to the relay.
    pub(super) refuse_writes: BTreeSet<String>,
    /// Level writes that must fail — the race where the capability was `Some` and the
    /// agent went away before the write.
    pub(super) refuse_levels: BTreeSet<String>,
    /// Every attempted level write, in order. Attempts rather than successes, because
    /// "teardown must not write an invented level" is a claim about what it *tried*.
    pub(super) level_writes: std::sync::Mutex<Vec<(String, f32)>>,
}

impl FakeHost {
    pub(super) fn new(outputs: &[(&str, HostLevers)]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(outputs.iter().map(|(n, l)| (n.to_string(), l.clone())).collect()),
            ..Default::default()
        })
    }

    /// A host with a mute lever only (what W17's tests exercise).
    pub(super) fn owning(outputs: &[(&str, bool)]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m)| (n.to_string(), HostLevers { muted: Some(*m), level: None })).collect(),
            ),
            ..Default::default()
        })
    }

    /// A host with both levers — the ordinary pw-sink case W20 is about.
    pub(super) fn levelling(outputs: &[(&str, bool, f32)]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m, l)| (n.to_string(), HostLevers { muted: Some(*m), level: Some(*l) })).collect(),
            ),
            ..Default::default()
        })
    }

    pub(super) fn refusing(outputs: &[(&str, bool)], refuse: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m)| (n.to_string(), HostLevers { muted: Some(*m), level: None })).collect(),
            ),
            refuse_writes: refuse.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        })
    }

    /// Both levers present, but the level write does not take.
    pub(super) fn refusing_levels(outputs: &[(&str, bool, f32)], refuse: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(
                outputs.iter().map(|(n, m, l)| (n.to_string(), HostLevers { muted: Some(*m), level: Some(*l) })).collect(),
            ),
            refuse_levels: refuse.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        })
    }

    pub(super) fn is_muted(&self, output: &str) -> Option<bool> {
        self.state.lock().unwrap().get(output).and_then(|l| l.muted)
    }

    pub(super) fn level_of(&self, output: &str) -> Option<f32> {
        self.state.lock().unwrap().get(output).and_then(|l| l.level)
    }

    /// Stand in for an agent that disconnected mid-run: the host answers nothing at all.
    pub(super) fn drop_agent(&self, output: &str) {
        self.state.lock().unwrap().remove(output);
    }

    pub(super) fn level_writes(&self) -> Vec<(String, f32)> {
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

pub(super) fn audible(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Scaffolding for the union-hold tests: a manager with a session already
/// running over `held`, without a PipeWire graph (so no anchor to wait for) and
/// without touching the process-global hold registry — see
/// `ExclusiveHold::for_test`.
pub(super) struct UnionFixture {
    pub(super) mgr: AlignManager,
    /// The very control stores the manager writes through, so a test can assert what the
    /// session did to a member's level without a device (both are in-process state).
    pub(super) sendspin: SharedSendspinControl,
    pub(super) ap2: SharedAp2Control,
    pub(super) groups: SharedGroups,
    pub(super) changes: crate::pw::thread::ChangeNotifier,
    pub(super) _changes_rx: tokio::sync::broadcast::Receiver<()>,
    pub(super) routing: crate::store::routing::SharedRouting,
    pub(super) outputs: crate::store::outputs::SharedOutputs,
    pub(super) hold_id: u64,
    pub(super) anchor: u32,
}

impl UnionFixture {
    pub(super) async fn new(tag: &str, held: &[(&str, MemberKind)]) -> Self {
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
            channels: BTreeMap::new(),
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

    pub(super) fn deps(&self) -> HoldDeps<'_> {
        HoldDeps { groups: &self.groups, changes: &self.changes, routing: &self.routing, outputs: &self.outputs }
    }

    /// Stand in for what `begin` snapshots — the pre-session levels teardown owes the
    /// user. An AP2 member left out of `ap2` is the *unknown* case (§7): the entry is
    /// absent, not zero.
    pub(super) async fn snapshot(&self, sendspin: &[(&str, u8)], ap2: &[(&str, f32)]) {
        let mut guard = self.mgr.session.lock().await;
        let session = guard.as_mut().expect("the fixture's session is running");
        session.saved_sendspin = sendspin.iter().map(|(n, v)| (n.to_string(), *v)).collect();
        session.saved_ap2_volumes = ap2.iter().map(|(n, v)| (n.to_string(), *v)).collect();
    }

    /// The out-of-band half of the same snapshot (W20), on the host's own 0.0–1.0 scale.
    /// A member left out is the *unknown* case: the entry is absent, not zero, and
    /// teardown must write nothing for it.
    pub(super) async fn snapshot_oob(&self, levels: &[(&str, f32)]) {
        let mut guard = self.mgr.session.lock().await;
        let session = guard.as_mut().expect("the fixture's session is running");
        session.saved_oob_levels = levels.iter().map(|(n, v)| (n.to_string(), *v)).collect();
    }

    /// The identity a re-form would change: the hold's id and its group's anchor.
    pub(super) async fn identity(&self) -> Option<(u64, u32)> {
        self.mgr.session.lock().await.as_ref().map(|s| (s.hold.id(), s.hold.anchor_node_id()))
    }

    /// How long the session has looked idle to the safety watchdog.
    pub(super) async fn idle(&self) -> Option<Duration> {
        let guard = self.mgr.session.lock().await;
        let s = guard.as_ref()?;
        let elapsed = s.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner).elapsed();
        Some(elapsed)
    }

    /// Backdate the activity mark, standing in for a user who has been walking.
    pub(super) async fn go_idle(&self, by: Duration) {
        let guard = self.mgr.session.lock().await;
        let s = guard.as_ref().expect("session");
        let mut a = s.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *a -= by;
    }
}
