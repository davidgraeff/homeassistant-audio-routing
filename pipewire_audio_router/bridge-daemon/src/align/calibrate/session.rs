//! One alignment run's lifetime: start it, keep it alive, and always give the room
//! back.
//!
//! [`Session`] is the live state — the group being aligned, the members, the delay
//! lines and mutes currently in force. Starting one may *form* a group that did not
//! exist, which costs a device reconnect, so `begin` is deliberately the only path in.
//!
//! Teardown lives here with the session on purpose. It is the one funnel every path
//! reaches — finished, refused, abandoned, timed out, or the UI simply went away —
//! and every restore the run owes the room happens in it. Splitting it out is how a
//! restore step gets skipped, and a skipped restore leaves a speaker muted with
//! nothing on screen saying why.
//!
//! `arm_timeout` is what makes "the UI went away" a real case rather than a leak.

use super::*;

pub(crate) struct Session {
    /// [`AlignState::sources`] — the identity the caller started it by.
    pub(crate) key: Vec<String>,
    pub(crate) members: Vec<AlignMember>,
    pub(crate) reference: Option<String>,
    pub(crate) target: Option<String>,
    /// The members currently audible (everything else muted).
    pub(crate) audible: BTreeSet<String>,
    /// Playback level (0–100) applied to the audible members, and the fallback for a
    /// member [`Self::levels`] has no entry for.
    pub(crate) volume: u8,
    /// **Per-member** calibration levels (0–100), keyed by node name — every level this
    /// session has applied, echoed as [`AlignState::levels`] (W19).
    ///
    /// Kept here rather than in the browser so the session is the single copy: a reload
    /// mid-walk reads it back instead of knowing only the currently-audible member's level.
    /// Recorded by [`Self::record_levels`] wherever a level is applied, so what this says
    /// and what the speakers were given cannot drift apart.
    ///
    /// A `BTreeMap` because it is serialised: a stable key order keeps the status frame
    /// byte-identical between polls that changed nothing.
    ///
    /// **Never persisted, and never a restore source** — see the module docs.
    pub(crate) levels: BTreeMap<String, u8>,
    /// **Per-member wire channels** ([`crate::align::relay_delay::MeasureChannels`]),
    /// keyed by node name — only members that are not on the default `both`, echoed as
    /// [`AlignState::channels`].
    ///
    /// The session is the single copy for the same reason as `levels`: the choice belongs
    /// to where the microphone is standing, so a reload mid-run must read it back rather
    /// than forget it. The *effect* lives in `relay_delay`, which is also what teardown
    /// clears — this map is what the UI renders and what a re-solo re-asserts.
    pub(crate) channels: BTreeMap<String, crate::align::relay_delay::MeasureChannels>,
    /// Set to stop the looping player thread.
    pub(crate) stop: Arc<AtomicBool>,
    /// Last time the user did something that proves they are still there — any change
    /// of audibility or level (`set_audible` / `solo` / `select` / `set_level`).
    ///
    /// The safety timeout is an **idle** timeout measured from this, not a deadline
    /// measured from `start`. A near-field walk round a large apartment legitimately
    /// runs longer than `SESSION_TIMEOUT`, and the earlier one-shot version tore the
    /// session down mid-walk as `SessionLost` — the plan's advice to keep one continuous
    /// session for everything that should be coherent (§1.2) was not honourable. Each
    /// arrival re-solos its speaker, so a walk refreshes this without the measurement
    /// code having to know the watchdog exists.
    pub(crate) activity: Arc<std::sync::Mutex<std::time::Instant>>,
    /// Sendspin **desired levels** captured on start, restored on teardown.
    pub(crate) saved_sendspin: HashMap<String, u8>,
    /// Sendspin **desired mute states** captured on start. Restored exactly rather
    /// than blanket-unmuted: a member the user had muted before the session must
    /// still be muted after it, and the old teardown unmuted everything.
    pub(crate) saved_sendspin_mutes: HashMap<String, bool>,
    /// AP2 mute states captured on start, same reasoning.
    pub(crate) saved_ap2_mutes: HashMap<String, bool>,
    /// AP2 **levels** captured on start (`ap2_volume`'s native 0.0–1.0), restored on
    /// teardown — W18, and the reason §12.2's slider is no longer a no-op for an AP2
    /// member: the session *drives* that level for as long as it runs.
    ///
    /// **Absence is meaningful and is not the same as zero.** `ap2_volume` treats a missing
    /// desired volume as genuinely *unknown* (nothing has read the receiver and the user has
    /// set nothing), and an AP2 receiver's level is device-authoritative — there is even a
    /// deliberate decision not to impose one on connect. So a member with no entry here is a
    /// member teardown must **leave alone**: writing a plausible-looking level would be
    /// inventing one, which on an amplifier is the failure mode that matters. Same shape,
    /// and for the same reason, as `align_levels::LevelRestore::level: Option<u8>`.
    ///
    /// Stored on the receiver's own scale rather than the calibration 0–100 so that putting
    /// it back is exact (see [`ap2_level`]).
    ///
    /// **What "leave the receiver alone" can and cannot cover today.** No RTSP write is made
    /// for an unknown member — which is the part that reaches an amplifier — but the write
    /// this session already made left two marks in `ap2_volume` that this module cannot
    /// erase: the desired-volume map now holds the calibration level (so restoring the
    /// snapshotted *mute* re-sends it once, since `set_muted` sends the stored desired), and
    /// `set_volume` records the node in `user_set`, so a later reconnect re-applies it as
    /// though the user had asked for it. Undoing that needs `Ap2Control` to grow a way to
    /// forget a level (and/or to write one without claiming user intent); it is a change in
    /// `outputs/ap2/volume.rs`, not here.
    pub(crate) saved_ap2_volumes: HashMap<String, f32>,
    /// **Out-of-band** mute states captured on start, for members with no in-band mute
    /// whose host can silence them ([`OutOfBandMute`]) — same snapshot/restore discipline
    /// as the two above. Empty when no silencer is wired or none of them owns a member, in
    /// which case those members are silenced at the relay instead, which needs no
    /// snapshot: it is this daemon's own transient state and it is dropped wholesale on
    /// release.
    pub(crate) saved_oob_mutes: HashMap<String, bool>,
    /// **Out-of-band levels** captured on start, on the host's own cubic 0.0–1.0 scale, for
    /// members whose level lives on their host and is readable through the agent
    /// ([`OutOfBandMute::level`]) — W20, and the reason §12.2's slider is no longer a no-op
    /// for a pw-sink member either.
    ///
    /// **Absence is meaningful and is not the same as zero**, exactly as for
    /// [`Self::saved_ap2_volumes`]: no entry means no pre-session level was ever known, and
    /// then teardown must **leave the host alone**. That case is not exotic — the agent
    /// reports a level only while it is *receiving* our stream (the lever is found through
    /// the receive stream's target sink), so a host whose stream came up after the snapshot
    /// pass is genuinely unknown to us, and inventing a level for someone's desktop is the
    /// failure mode that matters.
    ///
    /// Unlike AP2 there is nothing else to undo: [`crate::outputs::pwsink::agent::Agents`] stores no
    /// desired level and re-applies none on reconnect, so a write leaves no daemon-side
    /// mark — no `set_volume_transient`/`forget_volume` counterpart is needed here.
    pub(crate) saved_oob_levels: HashMap<String, f32>,
    /// How each member's level was reached at the last audibility pass, keyed by node name
    /// — echoed as [`AlignState::level_channels`] and the source of
    /// [`AlignState::unlevellable`].
    ///
    /// Kept on the session rather than recomputed in [`Session::state`] because resolving it
    /// asks the far end, which a status poll must never do. Seeded from the member kinds at
    /// construction (pessimistic for a pw-sink member: un-levellable until a host has
    /// actually answered) and replaced by every [`AlignManager::apply_audibility`] pass.
    pub(crate) level_channels: BTreeMap<String, LevelChannel>,
    /// The temporary exclusive group this session formed. Released by
    /// [`AlignManager::teardown`] on every exit path.
    ///
    /// Held over the run's **whole scope** and deliberately outliving individual
    /// `start` calls (plan §12.3.1): a `start` that re-selects a subset updates the
    /// fields above and leaves this alone.
    pub(crate) hold: ExclusiveHold,
    /// Did the last `start` reuse this hold rather than form it?
    pub(crate) hold_reused: bool,
    /// [`AlignState::hold_cost`] for the last `start`.
    pub(crate) hold_cost: String,
}

impl Session {
    pub(crate) fn state(&self) -> AlignState {
        // The resolved per-output answer (W20), not one derived from the member kinds:
        // `ExclusiveHold::unlevellable`/`level_constraint` still key off the kind and so
        // would claim this of every pw-sink member, including one its agent is levelling
        // perfectly well.
        let unlevellable = unlevellable_members(&self.level_channels);
        let labels: Vec<String> = unlevellable.iter().map(|n| self.hold.label(n).to_string()).collect();
        AlignState {
            active: true,
            sources: self.key.clone(),
            reference: self.reference.clone(),
            target: self.target.clone(),
            members: self.members.clone(),
            volume: self.volume,
            levels: self.levels.clone(),
            mode: self.hold.mode(),
            outputs: self.hold.outputs(),
            hold_id: self.hold.id(),
            hold_reused: self.hold_reused,
            hold_cost: self.hold_cost.clone(),
            level_note: level_note(&labels),
            unlevellable,
            level_channels: self.level_channels.clone(),
            audible: self.audible.iter().cloned().collect(),
            channels: self.channels.clone(),
            interference: self.hold.interference(),
            displaced: self.hold.displaced().to_vec(),
            closes_in_s: Some(self.closes_in().as_secs()),
            idle_timeout_s: SESSION_TIMEOUT.as_secs(),
            timeout_slack_s: TIMEOUT_POLL.as_secs(),
        }
    }

    /// Mark the user as still present (see [`Session::activity`]).
    pub(crate) fn note_activity(&self) {
        // Poison-tolerant: this is a liveness hint, and a panic elsewhere must not
        // make the watchdog stop noticing that the user is still here.
        *self.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
    }

    /// How long this session has looked idle to the safety watchdog.
    ///
    /// One reader for both consumers — the watchdog's decision and
    /// [`AlignState::closes_in_s`] — so the number the user is counting down and the
    /// number the teardown is decided on cannot be two different opinions.
    pub(crate) fn idle(&self) -> Duration {
        self.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner).elapsed()
    }

    /// How much idle time is left before the watchdog would tear this session down.
    /// Saturating: past the deadline it is zero, which is honest — the session is
    /// *awaiting* its teardown, not already gone (see [`AlignState::closes_in_s`]).
    pub(crate) fn closes_in(&self) -> Duration {
        SESSION_TIMEOUT.saturating_sub(self.idle())
    }

    pub(crate) fn is_member(&self, node_name: &str) -> bool {
        self.members.iter().any(|m| m.node_name == node_name)
    }

    /// Record `level` against every member it is about to be applied to — i.e. the
    /// currently audible set, which is exactly who [`AlignManager::apply_audibility`]
    /// writes a level to (W19).
    ///
    /// Called from every path that changes audibility or the level, so
    /// [`AlignState::levels`] describes what the speakers were actually given rather than a
    /// second, drifting opinion. Members that are *not* audible keep their previous entry:
    /// a solo of one speaker must not forget what another was set to two positions ago.
    pub(crate) fn record_levels(&mut self, level: u8) {
        for node in &self.audible {
            self.levels.insert(node.clone(), level);
        }
    }
}

pub(crate) fn same_set(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

impl AlignManager {
    /// Start a session for the group identified by its `sources` set — the by-ear
    /// entry point from a source card.
    ///
    /// Resolves that group's present members and then takes **exactly those**
    /// exclusively, so the by-ear path runs on the same temporary group as the
    /// measured ones instead of playing its click on top of whatever music the group
    /// was already carrying. Reference/target default to the first two members.
    pub async fn start(&self, deps: &HoldDeps<'_>, sources: Vec<String>) -> Result<AlignState, String> {
        // Resolve the group + members from the live layout (the selection this
        // session will hold).
        let group = {
            let snap = self.groups.lock().await.snapshot();
            snap.into_iter().find(|g| same_set(&g.sources, &sources)).ok_or("no such active group")?
        };
        let outputs: Vec<String> = group.sendspin_members.iter().chain(group.ap2_members.iter()).cloned().collect();
        if outputs.len() < 2 {
            return Err("a group needs at least two present members to align".to_string());
        }
        self.begin(deps, group.sources, outputs, AlignMode::Manual).await
    }

    /// Start a session for an arbitrary **selection of outputs** (plan §12.1): the
    /// wizard's entry point from the Outputs page. Forms a temporary exclusive group
    /// around the selection, independent of how those speakers are routed now.
    ///
    /// `outputs` is the run's **entire scope** — every speaker the run will touch,
    /// which for a multi-position walk means the whole apartment (or the whole floor),
    /// *not* the first position's audible set (plan §12.3.1). Forming the group is what
    /// costs a reconnect wave, so it is done once; each position then narrows the run
    /// with [`Self::set_audible`], which is live and free.
    ///
    /// Calling this again with the same speakers, or with any subset of them, is
    /// therefore **not** a restart: the hold, its anchor and its senders survive, and
    /// only audibility moves ([`Self::rescope`]). Only a selection needing a speaker
    /// the hold does not cover forms a new group — and pays for it.
    pub async fn start_outputs(&self, deps: &HoldDeps<'_>, outputs: Vec<String>, mode: AlignMode) -> Result<AlignState, String> {
        let mut key = outputs.clone();
        key.sort();
        key.dedup();
        self.begin(deps, key, outputs, mode).await
    }

    /// The shared body of both entry points. `key` is the identity echoed as
    /// `AlignState::sources`; `outputs` is what gets held.
    ///
    /// Two paths, and which one runs is the whole of plan §12.3.1: if the hold already
    /// in place covers `outputs`, [`Self::rescope`] reuses it (free); otherwise a new
    /// one is formed, which reconnects every member of the old *and* the new set.
    pub(crate) async fn begin(
        &self,
        deps: &HoldDeps<'_>,
        key: Vec<String>,
        outputs: Vec<String>,
        mode: AlignMode,
    ) -> Result<AlignState, String> {
        let _serialise = self.start_lock.lock().await;

        // Reuse before anything else — before the generation bump, before teardown —
        // because on this path there is nothing to supersede: the session, its hold,
        // its anchor and its click loop all stay exactly as they are.
        let plan = {
            let guard = self.session.lock().await;
            crate::align::group::plan_hold(guard.as_ref().map(|s| s.hold.held()), &outputs)
        };
        let why = match plan.form_reason() {
            None => return self.rescope(key, outputs, mode).await,
            Some(why) => why.to_string(),
        };

        let generation = self.start_gen.fetch_add(1, Ordering::SeqCst) + 1;

        // Tear the previous session down FIRST and with the lock released: forming a
        // group takes seconds (a reconcile pass plus a dial), and holding the session
        // lock across that would stall every status poll the UI makes.
        if let Some(old) = self.session.lock().await.take() {
            self.teardown(old).await;
        }

        let hold = ExclusiveHold::form(deps, outputs, mode).await?;
        let members = hold.members().to_vec();
        let members_held = members.len();
        let anchor = hold.anchor_node_id();

        // Snapshot levels and mutes so teardown can put them back exactly.
        let (mut saved_sendspin, mut saved_sendspin_mutes) = (HashMap::new(), HashMap::new());
        {
            let c = self.sendspin.lock().await;
            let (vols, mutes) = (c.volumes(), c.mutes());
            for m in members.iter().filter(|m| m.kind == MemberKind::Sendspin) {
                saved_sendspin.insert(m.node_name.clone(), vols.get(&m.node_name).copied().unwrap_or(100));
                saved_sendspin_mutes.insert(m.node_name.clone(), mutes.get(&m.node_name).copied().unwrap_or(false));
            }
        }
        // AP2: mute *and* level (W18), because the session drives both for its duration.
        // Taken in one lock so the two cannot describe different moments.
        let (saved_ap2_mutes, saved_ap2_volumes) = {
            let c = self.ap2.lock().await;
            let (mutes, levels) = (c.mutes(), c.volumes());
            drop(c);
            ap2_snapshot(&members, &mutes, &levels)
        };
        // The same discipline for members whose own transport carries neither knob, whose
        // host can do it instead (a pw-sink agent reports `HostState::muted` *and*
        // `volume`): snapshot both so teardown puts them back, exactly like the two above.
        // Either answer being absent is meaningful — an unsnapshotted mute means the relay
        // will hold that member down (nothing to restore), and an unsnapshotted level means
        // teardown must leave the host's own level exactly where it is (W20, plan §7).
        let (mut saved_oob_mutes, mut saved_oob_levels) = (HashMap::new(), HashMap::new());
        if let Some(seam) = self.out_of_band.get() {
            let needs_host = |m: &&AlignMember| SilenceChannel::in_band(m.kind).is_none() || LevelChannel::in_band(m.kind).is_none();
            for m in members.iter().filter(needs_host) {
                if let Some(muted) = seam.muted(&m.node_name).await {
                    saved_oob_mutes.insert(m.node_name.clone(), muted);
                }
                if let Some(level) = seam.level(&m.node_name).await {
                    saved_oob_levels.insert(m.node_name.clone(), level);
                }
            }
        }

        // Loop the click into the hold's anchor until stopped. This is also what
        // pumps every per-device relay, so it must keep running for the session's
        // whole life.
        let stop = Arc::new(AtomicBool::new(false));
        {
            let click = self.click.clone();
            let stop = stop.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::pw::player::play_loop_to_target(anchor, &click, stop) {
                    tracing::warn!("alignment playback ended with error: {e}");
                }
            });
        }

        let reference = members.first().map(|m| m.node_name.clone());
        let target = members.get(1).map(|m| m.node_name.clone());
        let audible: BTreeSet<String> = reference.iter().chain(target.iter()).cloned().collect();
        // The first audibility pass applies the default to that pair, so record it there and
        // then: `levels` must never say something the speakers were not given.
        let levels: BTreeMap<String, u8> = audible.iter().map(|n| (n.clone(), DEFAULT_ALIGN_LEVEL)).collect();
        let level_channels = kind_level_channels(&members);
        let mut session = Session {
            key,
            members,
            activity: Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
            reference,
            target,
            audible,
            volume: DEFAULT_ALIGN_LEVEL,
            levels,
            channels: BTreeMap::new(),
            stop: stop.clone(),
            saved_sendspin,
            saved_sendspin_mutes,
            saved_ap2_mutes,
            saved_ap2_volumes,
            saved_oob_mutes,
            saved_oob_levels,
            level_channels,
            hold_cost: form_cost_note(members_held, &why),
            hold,
            hold_reused: false,
        };
        // The first pass is also this session's first capability resolution, and it happens
        // before the session is published: a status poll can never see the kind-derived
        // seed.
        session.level_channels = self.apply_audibility(&session.members, &session.audible, session.volume).await;
        let state = session.state();

        // Did another start overtake us while we were forming? Then it owns the
        // house now; give ours back rather than clobbering it.
        {
            let mut guard = self.session.lock().await;
            if self.start_gen.load(Ordering::SeqCst) != generation {
                drop(guard);
                self.teardown(session).await;
                return Err("a newer alignment session was started while this one was still forming".to_string());
            }
            *guard = Some(session);
        }
        self.bump();

        // Safety timeout: tear down once the session has been *idle* this long.
        self.arm_timeout(stop);
        tracing::info!("alignment session started ({mode:?}) for {:?}: {}", state.outputs, state.hold_cost);
        if let Some(note) = &state.level_note {
            tracing::info!("alignment session: {note}");
        }
        Ok(state)
    }

    /// A `start` whose speakers are **already held**: keep the hold and re-aim the
    /// session inside it (plan §12.3.1).
    ///
    /// This is the path a multi-position run takes at every position after the first,
    /// and the reason the run costs one reconnect wave instead of one per position.
    /// Nothing that would reconnect a speaker is touched — not the reconciler's
    /// override, not the reservation, not the anchor, not the click loop, not the
    /// level/mute snapshot (still the one taken when the hold formed, so teardown
    /// restores what the user actually had). What *does* change:
    ///
    /// - the identity echoed as `AlignState::sources`, so the caller can see which
    ///   selection this is;
    /// - the mode, since the promise is a property of the run, not of the group;
    /// - reference/target and audibility, defaulting to the selection's first two
    ///   members exactly as a fresh start does — per-position audibility proper is
    ///   [`Self::set_audible`]'s job, not `start`'s;
    /// - the **idle mark** ([`Session::note_activity`]), so a long walk is not cut off 15
    ///   minutes after the *first* position. Note what this is not: it does not arm a
    ///   second watchdog. Plan §12.3.1 says the timeout is "re-armed" here, which was
    ///   true of the one-shot deadline and became misleading when §1.2 turned it into an
    ///   idle timeout — arming another watchdog on the same `stop` handle postpones
    ///   nothing at all (both watchdogs read the same `activity`), it only leaks a task
    ///   per position. Refreshing the mark is what actually buys the walk its time.
    ///
    /// The playback level is deliberately **not** reset to [`DEFAULT_ALIGN_LEVEL`]: by
    /// this point the user (or the level phase) has tuned it, and a start that is
    /// explicitly not a restart must not blast a re-learned level back to the default.
    pub(crate) async fn rescope(&self, key: Vec<String>, outputs: Vec<String>, mode: AlignMode) -> Result<AlignState, String> {
        let (members, audible, volume, stop, state) = {
            let mut guard = self.session.lock().await;
            // `plan_hold` only answers `Scope` when a session exists, and `begin` holds
            // `start_lock` across both, so this cannot be `None` — but a `stop` racing
            // in is cheap to answer honestly rather than with a panic.
            let session = guard.as_mut().ok_or("the alignment session ended while this start was being handled")?;
            session.key = key;
            session.hold.set_mode(mode);
            session.reference = outputs.first().cloned();
            session.target = outputs.get(1).cloned();
            session.audible = session.reference.iter().chain(session.target.iter()).cloned().collect();
            session.hold_reused = true;
            session.hold_cost = scope_cost_note(outputs.len(), session.members.len());
            // A `start` is a person picking speakers, so it is activity by any reading —
            // and this is the call a multi-position walk makes at every position.
            session.note_activity();
            let volume = session.volume;
            session.record_levels(volume);
            (session.members.clone(), session.audible.clone(), volume, session.stop.clone(), session.state())
        };
        let state = self.apply_and_record(&members, &audible, volume, &stop, state).await;
        tracing::info!(
            "alignment session re-scoped ({mode:?}) to {:?} inside the hold over {:?}: {}",
            state.sources,
            state.outputs,
            state.hold_cost
        );
        Ok(state)
    }

    /// "I am still here" — postpone the idle teardown without changing anything about the
    /// run ([`AlignState::closes_in_s`]).
    ///
    /// It exists because the countdown would otherwise be a deadline with no remedy: the
    /// step where a session runs out is the **review page**, and reading a proposal is
    /// silent — no solo, no level, nothing the watchdog counts. Telling a user their room
    /// is about to be handed back in two minutes and offering nothing to do about it is
    /// worse than not telling them.
    ///
    /// Why this is safe when a **held socket** would not be. The timeout exists so that a
    /// closed tab cannot leave a room muted, and a *forgotten* open tab is the same
    /// hazard — an open `GET /api/align/ws` therefore counts for nothing, and neither
    /// does a frame sent on it, or a status poll. What this endpoint requires is a person
    /// pressing a button now, which is the one piece of evidence the watchdog is actually
    /// looking for. The distinction is entirely in who initiates it, so:
    ///
    /// - a client must call this from a **click**, never from a timer. A UI that renewed
    ///   the session automatically would reimplement the leak this whole mechanism
    ///   exists to prevent, and it would do it invisibly;
    /// - it grants one fresh [`SESSION_TIMEOUT`], not an exemption. There is deliberately
    ///   no "keep open indefinitely": the failure this guards against is someone walking
    ///   away, and a session that cannot expire has no defence against that at all.
    pub async fn still_here(&self) -> Result<AlignState, String> {
        let state = {
            let guard = self.session.lock().await;
            let session = guard.as_ref().ok_or("no alignment session is running")?;
            session.note_activity();
            session.state()
        };
        // Pushed, because the whole point is that the countdown on screen jumps back.
        self.bump();
        tracing::info!("alignment session kept open for another {}s at the user's request", SESSION_TIMEOUT.as_secs());
        Ok(state)
    }

    /// Drain the exclusivity violations recorded against the running session (plan
    /// §12.3). Empty when nothing is running. **Drains**: each report is meant to be
    /// acted on exactly once, by discarding the named member's measurement.
    pub async fn take_interference(&self) -> Vec<Interference> {
        match self.session.lock().await.as_ref() {
            Some(s) => s.hold.take_interference(),
            None => Vec::new(),
        }
    }

    /// Stop the session: click off, levels/mutes restored, exclusive hold released.
    /// Works in any state and at any point (plan §12.2), including while a start is
    /// still forming — that start finds the generation moved and tears itself down.
    pub async fn stop(&self) -> AlignState {
        self.start_gen.fetch_add(1, Ordering::SeqCst);
        let session = self.session.lock().await.take();
        if let Some(s) = session {
            tracing::info!("alignment session stopped for {:?}", s.hold.outputs());
            self.teardown(s).await;
        }
        AlignState::inactive()
    }

    /// Undo everything the session did, in the order that leaves the least room for
    /// a half-restored house: audio off, then per-device state back, then the
    /// exclusive hold released (which is what lets the displaced music return).
    ///
    /// Every step is idempotent and none can fail in a way that skips a later one, so
    /// this is safe on every exit path — normal stop, the safety timeout, a
    /// superseding start, or a formation that lost a race. Mutes are restored to the
    /// **snapshotted** state rather than blanket-unmuted: a speaker the user had
    /// muted before the session must still be muted after it.
    ///
    /// Levels the same way, and now for all three channels that have one: sendspin's from
    /// [`Session::saved_sendspin`], AP2's from [`Session::saved_ap2_volumes`], a pw-sink
    /// host's from [`Session::saved_oob_levels`] — with the one asymmetry W18 turned on and
    /// W20 inherited, that a member whose pre-session level was **unknown** is written
    /// *nothing at all* instead of a guess. That is a no-op rather than a fallible step, so it
    /// cannot skip the restores after it.
    ///
    /// The **relay** mutes are not restored here but dropped by
    /// [`ExclusiveHold::release`](crate::align::group::ExclusiveHold::release) at the end —
    /// one infallible removal scoped to exactly the outputs that hold took, so no exit
    /// path can skip it and a late release can never silence a newer session's member.
    pub(crate) async fn teardown(&self, mut session: Session) {
        tracing::debug!("alignment session teardown (hold {}): restoring {} member(s)", session.hold.id(), session.members.len());
        session.stop.store(true, Ordering::Relaxed);
        let plan = restore_mute_plan(&session.members, &session.saved_sendspin_mutes, &session.saved_ap2_mutes);
        let mut pending = Vec::new();
        {
            let mut c = self.sendspin.lock().await;
            for (node, _, muted) in plan.iter().filter(|(_, channel, _)| *channel == SilenceChannel::SendspinInBand) {
                pending.push(c.set_mute(node, *muted));
            }
            for (n, v) in &session.saved_sendspin {
                pending.push(c.set_volume(n, *v));
            }
        }
        for p in pending {
            p.apply().await;
        }
        {
            let mut c = self.ap2.lock().await;
            // Level first, mute second — the same ordering argument as `apply_audibility`, in
            // reverse: restoring the mute re-sends the stored desired level, so the level has
            // to be the user's by then or the restore writes ours one last time.
            //
            // `None` means no pre-session level was ever known, and then the *only* correct
            // action is to write nothing (plan §7). Not a failure and not a step that can be
            // skipped: there is nothing to do, so the loop continues to the next member and
            // the mute restore below still happens.
            // Mute **first**, then the level. `set_muted` sends `effective_volume`, which is
            // `0.0` for a node with no stored desired level — so forgetting the level before
            // restoring the mute would push a receiver to −∞ dB. The house order elsewhere is
            // level-then-mute; here it is inverted on purpose, and this comment is why.
            for (node, _, muted) in plan.iter().filter(|(_, channel, _)| *channel == SilenceChannel::Ap2InBand) {
                c.set_muted(node, *muted);
            }
            for (node, level) in restore_ap2_level_plan(&session.members, &session.saved_ap2_volumes) {
                match level {
                    Some(level) => {
                        // Transient again: putting the user's own level back is not the user
                        // asking for it either, and claiming intent here would leave exactly
                        // the mark the drive side just avoided.
                        c.set_volume_transient(&node, level);
                    }
                    None => {
                        // Nothing to put back. `forget_volume` drops the desired level *and*
                        // any claim, returning the receiver to "we do not know and will not
                        // impose" — which is the state it was in before the session. Writing
                        // an invented level is the one thing that must not happen here.
                        c.forget_volume(&node);
                        tracing::info!(
                            "alignment teardown: '{node}' had no known AirPlay level before the session, so its own level is left \
                             exactly where it is rather than being given an invented one (it is the receiver's to own)"
                        );
                    }
                }
            }
        }
        // Out-of-band levels and mutes, from the same snapshot. Best effort by nature: a host
        // that has gone away cannot be restored, and saying so is better than pretending.
        //
        // Level before mute, the house order — for a host it is not load-bearing the way it is
        // for AP2 (the agent's `SetVolume` and `SetMute` are independent messages, and
        // `apply_master` keeps the current level when only the mute changes), but it does mean
        // there is never a moment where the host is unmuted at the calibration level.
        //
        // `None` again means no pre-session level was ever known, and then the only correct
        // action is to write nothing (plan §7, W20). A no-op rather than a fallible step, so
        // it cannot skip the mute restore after it.
        if let Some(seam) = self.out_of_band.get() {
            for (node, level) in restore_oob_level_plan(&session.members, &session.saved_oob_levels) {
                match level {
                    Some(level) => {
                        if !seam.set_level(&node, level).await {
                            tracing::warn!("alignment teardown: '{node}' could not have its level restored on its host (agent gone?)");
                        }
                    }
                    // Covers both "no agent ever reported one" and "no agent at all", which
                    // want the same action and are not worth distinguishing here: there is
                    // nothing to put back either way.
                    None => tracing::info!(
                        "alignment teardown: '{node}' had no known host level before the session, so nothing is written back — if the \
                         session drove its level it stays exactly where the calibration left it, which is better than an invented one: on \
                         someone's desktop that is the difference between a quiet machine and a silent one"
                    ),
                }
            }
            for (node, muted) in &session.saved_oob_mutes {
                if !seam.set_muted(node, *muted).await {
                    tracing::warn!("alignment teardown: '{node}' could not be restored on its host (agent gone?)");
                }
            }
        }
        // Last, so no other music source reaches these speakers until their levels
        // and mutes are already back where the user left them. This is also what drops the
        // relay mutes.
        session.hold.release().await;
        // The push that matters most, and the reason the notifier lives on the manager
        // rather than on the session (see [`Self::changes`]): every exit path comes
        // through here — the idle timeout, an explicit stop, a superseding start, a
        // formation that lost its race — so a client learns that its session is gone as
        // an event, not by noticing a poll a few seconds later. Bumped after the restore,
        // so the frame that says `active: false` is only sent once it is true of the
        // speakers as well as of the slot.
        self.bump();
    }

    /// Spawn the safety watchdog: tear the session down once it has been **idle** for
    /// `SESSION_TIMEOUT`, and only while it is still the very session identified by
    /// `stop` (a newer session has its own `stop`, so a restart is not killed early).
    ///
    /// Idle, not a deadline from `start`: it sleeps in `TIMEOUT_POLL` slices and re-reads
    /// [`Session::idle`] each time. A one-shot `sleep(SESSION_TIMEOUT)` killed long
    /// near-field walks mid-walk (plan §1.2), and the coarse slice is why
    /// [`AlignState::closes_in_s`] has to be reported as approximate.
    ///
    /// **One watchdog per session**, and exactly one: a `start` that re-scopes an existing
    /// hold refreshes the idle mark instead of arming another (see [`Self::rescope`]).
    /// The decision and the take happen under a **single** lock acquisition, so a
    /// [`Session::note_activity`] landing between "it looks idle" and "take it" cannot
    /// lose to a verdict this task had already reached — the earlier two-step version
    /// could tear down a session that had just been refreshed.
    pub(crate) fn arm_timeout(&self, stop: Arc<AtomicBool>) {
        let session = self.session.clone();
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(TIMEOUT_POLL).await;
                if stop.load(Ordering::Relaxed) {
                    return; // already stopped
                }
                let taken = {
                    let mut guard = session.lock().await;
                    match guard.as_ref() {
                        // A different session now owns the slot, or none does: either way
                        // this watchdog is spent and ours has been torn down already.
                        Some(s) if !Arc::ptr_eq(&s.stop, &stop) => return,
                        None => return,
                        Some(s) if s.idle() >= SESSION_TIMEOUT => guard.take(),
                        // Still being used. Keep watching rather than returning: the
                        // session has no other watchdog, so a task that gives up here
                        // leaves the hazard this exists for.
                        Some(_) => None,
                    }
                };
                if let Some(s) = taken {
                    tracing::info!(
                        "alignment session was idle for {}s; restoring levels/mutes and releasing the exclusive hold",
                        SESSION_TIMEOUT.as_secs()
                    );
                    this.teardown(s).await;
                    return;
                }
            }
        });
    }
}
