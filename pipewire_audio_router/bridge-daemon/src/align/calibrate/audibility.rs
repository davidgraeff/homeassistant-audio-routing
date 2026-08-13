//! Which members are audible, and how each one is silenced.
//!
//! Alignment needs exactly one speaker sounding at a time, and every transport
//! silences differently: a sendspin device takes a stream-level mute, an AP2 receiver
//! a volume write, a pw-sink target an out-of-band level. [`SilenceChannel`] and
//! [`LevelChannel`] state which channel applies to a member, and the `*_plan`
//! functions turn "make these members audible" into the concrete writes — plus the
//! matching restore plan, because every one of them has to be undone.
//!
//! The plans are computed before anything is written and recorded as they are
//! applied. That is what makes teardown able to restore exactly what was changed
//! rather than what it assumes was changed.

use super::*;

/// How one member is silenced while another is soloed — resolved **per output**, from what
/// that output can actually do.
///
/// Deliberately *not* derived from [`MemberKind`] alone: a pw-sink member usually **can**
/// be silenced (by its host agent) and sometimes cannot (agent gone, or a sink with no
/// volume lever), so two members of the same kind can differ — and one member's answer can
/// change between two positions of the same run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SilenceChannel {
    /// sendspin's in-band protocol Mute (plus the calibration playback level).
    SendspinInBand,
    /// AP2's in-band mute (`ap2_volume`) — plus, for the session's duration only, its
    /// calibration level (W18; snapshotted and given back on teardown).
    Ap2InBand,
    /// The host itself, out of band ([`OutOfBandMute`]).
    OutOfBand,
    /// This daemon's own relay hook (`relay_delay`): the universal fallback, needing no
    /// cooperation from the device or its host (plan §12.3.2, W17).
    Relay,
}

impl SilenceChannel {
    /// The channel a member's transport gives it *by itself*, before any per-output
    /// capability is consulted. `None` ⇒ no in-band mute, so the answer depends on the
    /// output — see [`silence_plan`].
    ///
    /// **The only place a mute mechanism is derived from a kind.** Everything else — the
    /// audibility pass, the teardown restore — keys off the resolved channel.
    pub(crate) fn in_band(kind: MemberKind) -> Option<Self> {
        match kind {
            MemberKind::Sendspin => Some(Self::SendspinInBand),
            MemberKind::Airplay2 => Some(Self::Ap2InBand),
            // NOT "⇒ Relay": a pw-sink host is usually mutable out of band. See the type
            // docs.
            MemberKind::PwSink => None,
        }
    }
}

/// How one member's **level** is reached while the session runs — resolved **per output**,
/// from what that output can actually do (W20, plan §12.3.2).
///
/// The level's twin of [`SilenceChannel`], and deliberately the same shape, because it is
/// the same kind of question: not "what kind of member is this?" but "what can I do to
/// *this* output right now?". A pw-sink member with a live receiver agent is levellable
/// and one without is not, so two members of the same kind differ and one member's answer
/// changes when its agent drops mid-walk.
///
/// One thing is **not** like the mute: there is no universal fallback. `relay_delay` can
/// impose silence on anything (it has a mute), but it has no gain, so [`Self::None`] is a
/// real outcome rather than a degraded one — and it is the outcome plan §7 cares about,
/// because a member nobody can turn down sets the clip ceiling every other member has to
/// fit under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelChannel {
    /// sendspin's live per-device level, on the calibration 0–100 scale. Snapshotted in
    /// [`Session::saved_sendspin`].
    SendspinLive,
    /// AP2's level, device-authoritative outside the session and *driven* for its duration
    /// (W18) — snapshotted in [`Session::saved_ap2_volumes`] and given back at teardown.
    Ap2Snapshot,
    /// The host's own sink level, over the receiver agent ([`OutOfBandMute::set_level`]).
    /// The same obligation as [`Self::Ap2Snapshot`] and for the same reason: the level
    /// belongs to the host, and it is only restorable because the host reports it
    /// ([`Session::saved_oob_levels`]).
    OutOfBand,
    /// No level knob this daemon can reach: no agent is answering for this host, or its
    /// sink has neither a device route nor a node volume ("lever: none"), or it is a future
    /// output kind with nothing of its own.
    ///
    /// Reported, never silently skipped ([`AlignState::unlevellable`]): the run can still
    /// go ahead, but this member's level is a *constraint* on it.
    None,
}

impl LevelChannel {
    /// The level channel a member's transport carries *by itself*. `None` ⇒ the transport
    /// has none, so the answer depends on the output — see [`level_plan`].
    ///
    /// **The only place a level mechanism is derived from a kind**, exactly as
    /// [`SilenceChannel::in_band`] is for the mute.
    pub(crate) fn in_band(kind: MemberKind) -> Option<Self> {
        match kind {
            MemberKind::Sendspin => Some(Self::SendspinLive),
            MemberKind::Airplay2 => Some(Self::Ap2Snapshot),
            // NOT "⇒ None": a pw-sink host's agent usually *can* set its level. That was
            // the wrong claim W20 corrected (plan §7's table said "none in this path").
            MemberKind::PwSink => None,
        }
    }

    /// What this channel means to the level solver — the bridge to `align_levels`, so the
    /// resolved per-output answer reaches the solve instead of its kind-derived floor
    /// (`align_levels::LevelMemberSpec::with_knob`).
    ///
    /// Two answers to "is this member levellable?" is how a member ends up adjustable in
    /// the solver and un-levellable in the UI, so this conversion exists rather than a
    /// second match somewhere else.
    pub fn knob(self) -> crate::align::levels::LevelKnob {
        use crate::align::levels::LevelKnob;
        match self {
            Self::SendspinLive => LevelKnob::Live,
            Self::Ap2Snapshot | Self::OutOfBand => LevelKnob::SnapshotRestore,
            Self::None => LevelKnob::None,
        }
    }

    /// Can this member be levelled at all this run?
    pub fn is_levellable(self) -> bool {
        self != Self::None
    }
}

/// Per member: should it be audible? The pure half of
/// [`AlignManager::apply_audibility`], so "exactly these are audible" is testable
/// without a speaker — a set, not a (reference, target) pair, because solo-one is
/// now the primary need (plan §12.2) and §7's all-play round needs N.
pub(crate) fn audibility_plan(members: &[AlignMember], audible: &BTreeSet<String>) -> Vec<(String, MemberKind, bool)> {
    members.iter().map(|m| (m.node_name.clone(), m.kind, audible.contains(&m.node_name))).collect()
}

/// Resolve every member's [`SilenceChannel`] for this position — the one place the
/// question "how do I silence this member?" is answered.
///
/// In-band where the transport has a mute; otherwise the out-of-band silencer if it owns
/// the output *right now*; otherwise the relay. Re-resolved on every audibility change
/// rather than cached at formation, because an agent can disconnect mid-run and the run
/// must degrade to the relay instead of quietly leaving a speaker audible.
pub(crate) async fn silence_plan(
    plan: &[(String, MemberKind, bool)],
    out_of_band: Option<&Arc<dyn OutOfBandMute>>,
) -> Vec<(String, SilenceChannel, bool)> {
    let mut resolved = Vec::with_capacity(plan.len());
    for (node, kind, audible) in plan {
        let channel = match SilenceChannel::in_band(*kind) {
            Some(channel) => channel,
            // One question, not two: `muted` answering `Some` *is* the capability.
            None => match out_of_band {
                Some(silencer) if silencer.muted(node).await.is_some() => SilenceChannel::OutOfBand,
                _ => SilenceChannel::Relay,
            },
        };
        resolved.push((node.clone(), channel, *audible));
    }
    resolved
}

/// Resolve every member's [`LevelChannel`] for this position — the one place the question
/// "how do I set this member's level?" is answered (W20).
///
/// The level twin of [`silence_plan`], called next to it and on the same schedule: on every
/// audibility change, never cached at formation, because an agent can disconnect mid-run
/// and the run must then *report* the member as un-levellable rather than keep issuing
/// writes into nothing.
///
/// Two calls into the seam rather than one because the two capabilities are genuinely
/// independent: `HostState` carries `muted` and `volume` as separate `Option`s and the
/// agent's node-`Props` fallback (a virtual sink) fills in the level while leaving the mute
/// unknown. Such a member is levelled by its host and silenced by the relay, and no single
/// question could have said that.
pub(crate) async fn level_plan(
    plan: &[(String, MemberKind, bool)],
    out_of_band: Option<&Arc<dyn OutOfBandMute>>,
) -> Vec<(String, LevelChannel, bool)> {
    let mut resolved = Vec::with_capacity(plan.len());
    for (node, kind, audible) in plan {
        let channel = match LevelChannel::in_band(*kind) {
            Some(channel) => channel,
            // One question, not two: `level` answering `Some` *is* the capability.
            None => match out_of_band {
                Some(seam) if seam.level(node).await.is_some() => LevelChannel::OutOfBand,
                _ => LevelChannel::None,
            },
        };
        resolved.push((node.clone(), channel, *audible));
    }
    resolved
}

/// The level channels a member list resolves to **before any host has been asked** — the
/// answer its transport carries, and [`LevelChannel::None`] for a member whose transport
/// carries none.
///
/// Only a seed ([`Session::level_channels`]): the real answer needs the seam, and the first
/// [`AlignManager::apply_audibility`] pass replaces it. Pessimistic on purpose — a member
/// wrongly seeded as levellable would hide its clip-ceiling role (plan §7), which is the
/// one thing the status has to say out loud.
pub(crate) fn kind_level_channels(members: &[AlignMember]) -> BTreeMap<String, LevelChannel> {
    members.iter().map(|m| (m.node_name.clone(), LevelChannel::in_band(m.kind).unwrap_or(LevelChannel::None))).collect()
}

/// Per member whose **level lives on its host**: the level teardown must write back, or
/// `None` for "write nothing".
///
/// The out-of-band twin of [`restore_ap2_level_plan`], pure for the same reason — the
/// "unknown ⇒ do not write" decision is the one that is silent when it goes wrong, and it
/// has to be assertable without a host. Every such member is named even when its answer is
/// `None`, so a member cannot be quietly dropped from the restore.
pub(crate) fn restore_oob_level_plan(members: &[AlignMember], saved: &HashMap<String, f32>) -> Vec<(String, Option<f32>)> {
    members
        .iter()
        .filter(|m| LevelChannel::in_band(m.kind).is_none())
        .map(|m| (m.node_name.clone(), saved.get(&m.node_name).copied()))
        .collect()
}

/// The level channels of members whose level is nobody's to set here — the resolved answer
/// [`AlignState::unlevellable`] reports, taken from [`level_plan`] rather than from the
/// members' kinds.
pub(crate) fn unlevellable_members(channels: &BTreeMap<String, LevelChannel>) -> Vec<String> {
    channels.iter().filter(|(_, c)| !c.is_levellable()).map(|(n, _)| n.clone()).collect()
}

/// The one sentence [`AlignState::level_note`] carries, for the members that genuinely have
/// no level knob — `labels` being what the *user* calls them (the rename store first, as
/// `align_group::resolve_labels` established), never node names.
///
/// Said here rather than taken from `align_group::ExclusiveHold::level_constraint` because
/// that one derives its list from the member **kinds**, so it claims this of every pw-sink
/// member — which W20 made wrong: a pw-sink host with a live agent is levellable, and
/// telling the user it sets the clip ceiling would send them to the wrong speaker.
///
/// Plan §7's point, said out loud: the danger is not that such a member is too *quiet* (a
/// report-only nuisance) but that it **clips**, because then turning every other speaker
/// down cannot rescue the measurement. Audibility is not at stake — W17 silences every
/// member kind, out of band or at the relay.
pub(crate) fn level_note(labels: &[String]) -> Option<String> {
    if labels.is_empty() {
        return None;
    }
    let named = labels.iter().map(|l| format!("'{l}'")).collect::<Vec<_>>().join(", ");
    Some(if labels.len() == 1 {
        format!(
            "{named} has no level control alignment can reach right now — no receiver agent is answering for it, or its sink has no volume \
             lever at all, so its volume can only be changed on the host itself. It therefore sets the clip ceiling every other speaker has \
             to fit under: if it clips, turning all the others down cannot rescue the measurement (plan §7). It is still silenced while \
             another speaker is soloed."
        )
    } else {
        format!(
            "{named} have no level control alignment can reach right now — no receiver agent is answering for them, or their sinks have no \
             volume lever at all, so their volumes can only be changed on the hosts themselves. They therefore set the clip ceiling every \
             other speaker has to fit under: if one of them clips, turning all the others down cannot rescue the measurement (plan §7). \
             They are still silenced while another speaker is soloed."
        )
    })
}

/// Per member with an **in-band** mute: the state teardown must push — the one
/// **snapshotted at start**, not `false`.
///
/// The distinction matters: a speaker the user had muted before the session must
/// still be muted after it. Blanket-unmuting (what teardown used to do) silently
/// un-mutes it, and the sendspin mute the session uses is *transient*, so the stored
/// desired state still says "muted" — leaving the device and the UI disagreeing.
///
/// Members with no in-band mute are absent here, because their restore is a different
/// obligation: an out-of-band one is restored from `Session::saved_oob_mutes`, and a relay
/// mute is dropped wholesale by `ExclusiveHold::release` (nothing to snapshot — it is this
/// daemon's own transient state, and it never existed before the session).
pub(crate) fn restore_mute_plan(
    members: &[AlignMember],
    saved_sendspin: &HashMap<String, bool>,
    saved_ap2: &HashMap<String, bool>,
) -> Vec<(String, SilenceChannel, bool)> {
    members
        .iter()
        .filter_map(|m| {
            let channel = SilenceChannel::in_band(m.kind)?;
            let saved = if channel == SilenceChannel::SendspinInBand { saved_sendspin } else { saved_ap2 };
            Some((m.node_name.clone(), channel, saved.get(&m.node_name).copied().unwrap_or(false)))
        })
        .collect()
}

/// The AP2 state a session has to be able to put back, taken from `ap2_volume`'s two
/// snapshots at start: `(mutes, levels)`, both keyed by node name and covering the AP2
/// members only.
///
/// The asymmetry between the two is the whole point (W18):
///
/// - **mute** defaults to `false` when nothing is stored, because that is what an untouched
///   receiver is, and restoring "unmuted" is not an invention;
/// - **level** gets **no entry at all** when nothing is stored. `ap2_volume` distinguishes
///   "unknown" from "zero" and so must this: an absent level means teardown leaves that
///   receiver's own knob exactly where it is (plan §7).
pub(crate) fn ap2_snapshot(
    members: &[AlignMember],
    mutes: &HashMap<String, bool>,
    levels: &HashMap<String, f32>,
) -> (HashMap<String, bool>, HashMap<String, f32>) {
    let mut saved_mutes = HashMap::new();
    let mut saved_levels = HashMap::new();
    for m in members.iter().filter(|m| m.kind == MemberKind::Airplay2) {
        saved_mutes.insert(m.node_name.clone(), mutes.get(&m.node_name).copied().unwrap_or(false));
        // No `unwrap_or`: unknown must stay unknown all the way to the restore.
        if let Some(level) = levels.get(&m.node_name) {
            saved_levels.insert(m.node_name.clone(), *level);
        }
    }
    (saved_mutes, saved_levels)
}

/// Per **AP2** member: the level teardown must write back, or `None` for "write nothing".
///
/// Separate from [`restore_mute_plan`] because the two answers differ in kind — a mute
/// always has a right answer, a device-authoritative level does not — and pure so that the
/// "unknown ⇒ do not write" decision is assertable without a receiver. `None` is the one
/// case that is silent when it goes wrong: a restore that invents `0.0` mutes an amplifier
/// and a restore that invents the calibration level leaves it quiet, and either way the
/// user only finds out the next time they play music.
pub(crate) fn restore_ap2_level_plan(members: &[AlignMember], saved: &HashMap<String, f32>) -> Vec<(String, Option<f32>)> {
    members.iter().filter(|m| m.kind == MemberKind::Airplay2).map(|m| (m.node_name.clone(), saved.get(&m.node_name).copied())).collect()
}

impl AlignManager {
    /// Set which two members are audible (reference vs. the target being tuned) —
    /// the by-ear comparison. A special case of [`Self::set_audible`].
    pub async fn select(&self, reference: String, target: String) -> Result<AlignState, String> {
        let (members, audible, volume, stop, state) = {
            let mut guard = self.session.lock().await;
            let session = guard.as_mut().ok_or("no alignment session is running")?;
            if !session.is_member(&reference) || !session.is_member(&target) {
                return Err("reference and target must both be members of the active group".to_string());
            }
            session.audible = [reference.clone(), target.clone()].into_iter().collect();
            session.reference = Some(reference);
            session.target = Some(target);
            // The by-ear path's own step. [`Session::activity`] always claimed `select`
            // refreshed the idle mark and only `set_audible` ever did, so a by-ear
            // session spent an hour being compared pair by pair could be torn down as
            // abandoned. Both callers of `apply_and_record` that a *user* drives now
            // agree with that doc comment.
            session.note_activity();
            let volume = session.volume;
            session.record_levels(volume);
            (session.members.clone(), session.audible.clone(), volume, session.stop.clone(), session.state())
        };
        // Re-apply against the new pair (lock released — apply_audibility awaits).
        Ok(self.apply_and_record(&members, &audible, volume, &stop, state).await)
    }

    /// Make exactly the named members audible at `level`, muting every other member.
    ///
    /// The general form the two callers need (plan §12.2, §7.1): **one** member for
    /// level-setting and for the sequential measurement (a shared click track means
    /// every speaker emits both bursts, so a per-member SNR cannot be attributed from
    /// two), and **all** of them for §7's all-play headroom round.
    ///
    /// `reference`/`target` are left alone — they describe the by-ear comparison, and
    /// a solo must not silently redefine it.
    pub async fn set_audible(&self, audible: Vec<String>, level: u8) -> Result<AlignState, String> {
        let level = level.min(100);
        let (members, audible, stop, state) = {
            let mut guard = self.session.lock().await;
            let session = guard.as_mut().ok_or("no alignment session is running")?;
            if let Some(unknown) = audible.iter().find(|n| !session.is_member(n)) {
                return Err(format!("'{unknown}' is not a member of the active alignment group"));
            }
            session.audible = audible.into_iter().collect();
            session.volume = level;
            session.note_activity();
            // W19: the level belongs to the members it was applied to, so a later solo of a
            // *different* speaker cannot lose it.
            session.record_levels(level);
            (session.members.clone(), session.audible.clone(), session.stop.clone(), session.state())
        };
        Ok(self.apply_and_record(&members, &audible, level, &stop, state).await)
    }

    /// Make exactly one member audible at `level` — plan §12.2's solo.
    pub async fn solo(&self, node_name: String, level: u8) -> Result<AlignState, String> {
        self.set_audible(vec![node_name], level).await
    }

    /// Emit only one wire channel for one member, or both again — the remedy for a member
    /// that drives a **stereo pair** ([`MeasureChannels`]).
    ///
    /// A pair radiates the identical click from two places, so the microphone hears two
    /// arrivals of near-equal amplitude and the member has no single arrival *time*: the
    /// estimator refuses it as an ambiguous peak (hardware, 2026-08-13 — a desktop pair
    /// read 1.1× between its two arrivals). One channel makes it one source.
    ///
    /// Live and free, like a mute: no reconnect, nothing persisted, and no effect on any
    /// other member. It counts as activity, because choosing it is the user working the
    /// run. What the *delay* solver then produces is that channel's speaker — which is
    /// the honest answer, since a pair heard off-axis has no other one.
    pub async fn set_channels(&self, node_name: String, channels: MeasureChannels) -> Result<AlignState, String> {
        let mut guard = self.session.lock().await;
        let session = guard.as_mut().ok_or("no alignment session is running")?;
        if !session.is_member(&node_name) {
            return Err(format!("'{node_name}' is not a member of the active alignment group"));
        }
        // The map holds only the non-default choices, so `channels[node] ?? both` reads
        // the same way `levels[node] ?? volume` does.
        if channels == MeasureChannels::Both {
            session.channels.remove(&node_name);
        } else {
            session.channels.insert(node_name.clone(), channels);
        }
        session.note_activity();
        crate::align::relay_delay::RelayDelay::global().set_channels(&node_name, channels);
        tracing::info!("alignment: '{node_name}' now emits {} channel(s) for this session", channels.as_str());
        Ok(session.state())
    }

    /// Silence **every** member while keeping everything else the session is doing:
    /// the exclusive hold, the click track, the levels, the reference/target pair.
    ///
    /// Why the hold has to stay: `apply` needs the group still held and still playing
    /// off one clock, so a measurement run that has parked on a proposal cannot simply
    /// stop the session. But nothing should still be *audible* while the user reads a
    /// review page — the tick/tack looping under a page of numbers is grating, and it
    /// was the first thing a real run got complained about (2026-08-12).
    ///
    /// This is the daemon's own decision rather than an empty `POST /api/align/audible`
    /// from the panel, for the same reason silence is a per-output capability at all
    /// (plan §12.3.2): a **closed tab** must fall silent too. Deliberately not
    /// `note_activity` — a run parking is not the user proving they are still there,
    /// and the session's idle watchdog should keep counting.
    ///
    /// Levels are re-applied unchanged, so resuming is one ordinary `solo` away.
    pub async fn silence(&self) -> Result<AlignState, String> {
        let (members, audible, volume, stop, state) = {
            let mut guard = self.session.lock().await;
            let session = guard.as_mut().ok_or("no alignment session is running")?;
            session.audible.clear();
            let volume = session.volume;
            (session.members.clone(), session.audible.clone(), volume, session.stop.clone(), session.state())
        };
        Ok(self.apply_and_record(&members, &audible, volume, &stop, state).await)
    }

    /// Set the audible members' playback level (0–100) live — and record it against exactly
    /// those members ([`AlignState::levels`]), since they are who it reaches.
    pub async fn set_level(&self, volume: u8) -> Result<AlignState, String> {
        let volume = volume.min(100);
        let (members, audible, stop, state) = {
            let mut guard = self.session.lock().await;
            let session = guard.as_mut().ok_or("no alignment session is running")?;
            session.volume = volume;
            // Dragging the level slider is the user being present, and
            // [`Session::activity`] said so before this call actually did it.
            session.note_activity();
            session.record_levels(volume);
            (session.members.clone(), session.audible.clone(), session.stop.clone(), session.state())
        };
        Ok(self.apply_and_record(&members, &audible, volume, &stop, state).await)
    }

    /// Make exactly `audible` audible; silence every other member, each through the
    /// channel that member actually has ([`SilenceChannel`], resolved here and nowhere
    /// else):
    ///
    /// - **sendspin** — the in-band protocol Mute plus the calibration playback level;
    /// - **AP2** — the in-band mute, plus the calibration level (W18): the receiver's own
    ///   level is authoritative *outside* a session, but a session that could not set it
    ///   left §12.2's slider a no-op for every AP2 member, so it is imposed for the
    ///   session's duration and given back by [`Self::teardown`];
    /// - **out of band** — the host's own agent, where it owns the output
    ///   ([`OutOfBandMute`]);
    /// - **the relay hook** — everything else, so a member with no mute of its own (and no
    ///   reachable host) still cannot play through another member's solo.
    ///
    /// In-band and out-of-band are preferred because the device knows it is muted and its
    /// audio path is undisturbed; the relay mute is the floor that makes audibility a
    /// guarantee rather than a request.
    ///
    /// The **level** of the audible members is applied in the same pass and through the same
    /// per-output discipline ([`LevelChannel`], W20): sendspin live, AP2 driven for the
    /// session, a pw-sink host's own sink level out of band — and nothing at all for a member
    /// whose level nobody here can reach, which is *reported* rather than skipped
    /// ([`AlignState::unlevellable`]). Returns what each member resolved to, so the session
    /// can say so and a level solve can use the right knob.
    ///
    /// Set-based rather than "reference + target" (plan §12.2): one-at-a-time is the
    /// primary need now, and a pair is just a two-element set.
    pub(crate) async fn apply_audibility(
        &self,
        members: &[AlignMember],
        audible: &BTreeSet<String>,
        volume: u8,
    ) -> BTreeMap<String, LevelChannel> {
        // Both capabilities resolved here and nowhere else, on the same schedule, from the
        // same member list — an agent that dropped since the last position changes both
        // answers and must change them together.
        let wanted = audibility_plan(members, audible);
        let plan = silence_plan(&wanted, self.out_of_band.get()).await;
        let mut levels = level_plan(&wanted, self.out_of_band.get()).await;
        let relay = crate::align::relay_delay::RelayDelay::global();

        // The relay mute goes first, and as one batch under one lock: it is instant and
        // local, so a member the run wants silent is silent *now* — including one that is
        // about to be silenced out of band, because that involves a host round trip which
        // can fail. Every member is named, so a member another channel is handling is
        // explicitly *un*-muted here and the two mechanisms can never both be left holding
        // one output down; a position whose members all mute themselves creates no entries
        // at all, leaving the RT gate closed (one atomic load per block, as before).
        relay.set_mutes(plan.iter().map(|(node, channel, on)| {
            (node.as_str(), !*on && !matches!(channel, SilenceChannel::SendspinInBand | SilenceChannel::Ap2InBand))
        }));

        // Out-of-band **levels** next, and before the out-of-band unmute below: a member is
        // never unmuted at a level this position did not choose. (The relay batch above may
        // already have released an audible member whose *mute* channel is the relay while its
        // level is its host's — one click at the old level, which delays the estimator's
        // amplitude-stability gate rather than corrupting a reading, since the gate wants four
        // steady periods.)
        //
        // A write that does not take has **no fallback** — the relay has a mute, not a gain —
        // so the member is re-resolved to `LevelChannel::None` right here and reported as
        // un-levellable. Skipping it silently would leave a speaker at an unknown level while
        // the solve believed it had set one, which plan §7 says must instead name the member.
        if let Some(seam) = self.out_of_band.get() {
            let mut lost: BTreeSet<String> = BTreeSet::new();
            for (node, _, _) in levels.iter().filter(|(_, channel, on)| *on && *channel == LevelChannel::OutOfBand) {
                if !seam.set_level(node, host_level(volume)).await {
                    tracing::warn!(
                        "alignment: '{node}' could not be levelled on its host (agent gone?) — it is reported as un-levellable for this \
                         position, so it constrains the other members' levels instead of being tuned (plan §7)"
                    );
                    lost.insert(node.clone());
                }
            }
            for entry in levels.iter_mut().filter(|(node, _, _)| lost.contains(node)) {
                entry.1 = LevelChannel::None;
            }
        }

        // Then out of band on the host, where that is available — preferred, because the
        // stream keeps flowing and the receiver's jitter buffer never re-anchors. The relay
        // mute is handed over only once the write has *landed*: a write that does not take
        // leaves the member relay-muted, so an agent that disappeared between resolving the
        // channel and using it cannot leave a speaker audible through another's solo.
        if let Some(silencer) = self.out_of_band.get() {
            let mut handed_over: Vec<(&str, bool)> = Vec::new();
            for (node, _, on) in plan.iter().filter(|(_, channel, _)| *channel == SilenceChannel::OutOfBand) {
                if silencer.set_muted(node, !on).await {
                    handed_over.push((node.as_str(), false));
                } else {
                    tracing::warn!(
                        "alignment: '{node}' could not be silenced on its host (agent gone?) — the relay mute is holding it instead, \
                         which needs no cooperation from it"
                    );
                }
            }
            if !handed_over.is_empty() {
                relay.set_mutes(handed_over);
            }
        }

        // Collect every sendspin push first, then release the control lock before
        // awaiting any of them: holding it across a loop of per-device sends let one
        // unreachable speaker freeze the whole wizard *and* the volume API.
        let mut pending = Vec::new();
        {
            let mut c = self.sendspin.lock().await;
            for (node, _, on) in plan.iter().filter(|(_, channel, _)| *channel == SilenceChannel::SendspinInBand) {
                pending.push(c.set_mute(node, !on));
                if *on {
                    pending.push(c.set_volume(node, volume));
                }
            }
        }
        for p in pending {
            p.apply().await;
        }
        // AP2 last, and both knobs per member under one lock. **Level before unmute**, which
        // is not cosmetic: `Ap2Control::set_muted(false)` re-sends the *stored* desired level,
        // so setting the level first makes that single write carry the calibration level —
        // the other order writes the old level to the receiver and then corrects it, which is
        // an audible step on an amplifier and a needless second RTSP round trip.
        //
        // A member that is being silenced gets no level write at all: the level is a property
        // of the members the microphone (or the ear) is meant to hear, exactly as for
        // sendspin above, and a write to a muted receiver would only queue a level for an
        // unmute that a later pass may never make.
        for (node, _, on) in plan.iter().filter(|(_, channel, _)| *channel == SilenceChannel::Ap2InBand) {
            let mut c = self.ap2.lock().await;
            if *on {
                // `set_volume_transient`, not `set_volume`: the session is *imposing* this
                // level for its duration, not recording a choice the user made. `set_volume`
                // adds the node to `user_set`, and `Ap2Control::register` re-applies a
                // `user_set` level on every later reconnect — so a calibration level of 20 %
                // would come back on an amplifier, as if asked for, long after teardown. That
                // is a mark no teardown can reach.
                //
                // The cost is deliberate: without the `user_set` claim, an AP2 session that
                // reconnects *mid*-alignment no longer re-applies the calibration level. That
                // failure is detectable — the member goes quiet or changes level, and the gate
                // refuses with a reason — whereas the permanent claim is silent. A detectable
                // loss beats a silent side effect.
                c.set_volume_transient(node, ap2_level(volume));
            }
            c.set_muted(node, !on);
        }
        levels.into_iter().map(|(node, channel, _)| (node, channel)).collect()
    }

    /// [`Self::apply_audibility`], plus folding the capabilities that pass resolved back into
    /// the session — so the state a caller gets back already says what can be done to each
    /// member *now*, and an agent that dropped shows up as un-levellable in the same answer
    /// rather than at the next poll.
    ///
    /// Locking twice is deliberate. Resolving asks the far end, which must not happen under
    /// the session lock (one wedged host would stall every status poll, the bug the sendspin
    /// loop below was already fixed for), and the answer belongs on the session because a
    /// status poll arriving before the next position must not fall back to the kind-derived
    /// seed. `stop` identifies the session, so a pass whose session was stopped or superseded
    /// while it was talking to the speakers writes into nothing and answers with the state it
    /// applied — `applied` — instead of resurrecting a dead session's.
    pub(crate) async fn apply_and_record(
        &self,
        members: &[AlignMember],
        audible: &BTreeSet<String>,
        volume: u8,
        stop: &Arc<AtomicBool>,
        applied: AlignState,
    ) -> AlignState {
        let channels = self.apply_audibility(members, audible, volume).await;
        let state = {
            let mut guard = self.session.lock().await;
            match guard.as_mut() {
                Some(session) if Arc::ptr_eq(&session.stop, stop) => {
                    session.level_channels = channels;
                    session.state()
                }
                _ => applied,
            }
        };
        // Every user-driven change to a live session funnels through here (`select`,
        // `set_audible`/`solo`, `set_level`, `silence`, `rescope`), so this is the one
        // push that covers all of them — including the refreshed `closes_in_s`, which is
        // what makes a countdown on a second screen jump back too.
        self.bump();
        state
    }
}
