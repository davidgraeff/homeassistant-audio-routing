//! Latency-alignment ("calibration") session — the backend for the alignment
//! panel (frontend `AlignPanel.svelte`).
//!
//! A session plays a test signal on every member off one clock, with **exactly the
//! members it names audible** and the rest muted. Which members are audible is what
//! the two consumers differ on: the by-ear path makes two audible (a fixed
//! **reference** and the **target** being tuned) and the user drags the target's
//! delay until they coincide; the microphone path (align/measure.rs) solos **one**
//! at a time so a per-member SNR can be attributed at all (plan §12.2, §7.1).
//!
//! ## The group is formed by the session, not found by it
//!
//! A session takes an arbitrary **selection of outputs** and forms a temporary
//! exclusive group around them (`align/group.rs`, plan §12.1) — it no longer
//! requires a group to already exist for some source set. While the session runs,
//! nothing else reaches those speakers. The by-ear path goes through the same
//! machinery, which is what stops it being a special case: `start` resolves an
//! existing group's members and then holds exactly those.
//!
//! ## The selection is the run's whole scope, and re-selecting is free (plan §12.3.1)
//!
//! Forming that group reconnects every sendspin member, and releasing it reconnects
//! them again — tens of seconds each way (plan §2.3). A multi-position run that
//! re-formed per position would pay both waves at every position, which is the cost
//! §1.1.1 removed from the write path.
//!
//! So `start` means "**hold all of these for the whole run**", not "align these now":
//!
//! - the first `start` forms one hold over the union of every speaker the run will
//!   touch;
//! - a later `start` naming the same speakers, or **any subset** of them, keeps that
//!   hold — same id, same anchor, same senders, same click loop — and only moves
//!   mutes. [`AlignManager::begin`] asks `align_group::plan_hold` and takes
//!   [`Self::rescope`] in that case;
//! - only a `start` needing a speaker the hold does not cover forms a new one, which
//!   tears the old session down first and pays the wave.
//!
//! Which held members are *audible* is the per-position knob ([`Self::set_audible`],
//! `POST /api/align/audible`): live, free, and the thing a walk should be driving.
//! [`AlignState::hold_cost`] says which of the two just happened, in words, because a
//! caller cannot otherwise tell an expensive `start` from a free one.
//!
//! ## Audibility is mute, and not every member mutes the same way
//!
//! "Exactly these members are audible" is the whole procedure (plan §12.2): a member that
//! keeps playing through another's solo puts a second arrival in front of the microphone,
//! which either refuses as `AmbiguousPeak` — failing **every** member of the step, not
//! just the noisy one — or merges into one pulled peak, i.e. plan §5.6's silent bias by
//! construction. So silencing has to work for *every* member, and the mechanism is a
//! **per-output** question rather than a property of [`MemberKind`]:
//!
//! - sendspin and AP2 mute **in band**, and that is preferred where it exists — the device
//!   knows it is muted, and the stored/device volume is untouched so unmuting restores it;
//! - a pw-sink host can usually be muted **out of band** by its agent
//!   ([`OutOfBandMute`]): also better than the fallback, because the stream keeps flowing
//!   and only the receiver's own sink volume moves, so its jitter buffer never re-anchors;
//! - otherwise — an agent that is not connected, a sink with *no volume lever at all*, a
//!   future output kind with nothing of its own — the member is silenced by this daemon at
//!   the **relay hook** (`relay_delay`, plan §12.3.2/W17). It needs no cooperation
//!   whatsoever, which is exactly why it is the floor under the other two.
//!
//! [`SilenceChannel`] is that decision, resolved in one place
//! ([`AlignManager::apply_audibility`] via [`silence_plan`]) so no second answer to the
//! question can exist, and re-resolved per position because the answer can change while a
//! run is walking (an agent can drop). [`LevelChannel`] is its twin for the **level** and is
//! resolved in the same pass, from the same seam, on the same schedule — see below.
//!
//! ## Levels: session-owned, and *driven* for the session's duration
//!
//! The calibration level is a **0–100** integer and it is **per member** — a near speaker
//! can be 20 dB hotter at the microphone than a far one, so one number for the group is
//! not a level model (plan §7). The session owns that map ([`AlignState::levels`], W19),
//! which is what makes it survive a page reload mid-walk; a member the run has not touched
//! yet has no entry and falls back to the session's single [`AlignState::volume`].
//!
//! **Nothing here is persisted, and that is the requirement rather than a gap.** The right
//! calibration level depends on where the phone is, so the same speaker legitimately wants
//! a different level at a different position of the *same* multi-position walk. A stored
//! value would be a good seed and a bad promise — do not add a store casually.
//!
//! How the level reaches a member is, like the mute, a **per-output** question
//! ([`LevelChannel`], resolved by [`level_plan`] in the same pass as the silencing) — not a
//! property of [`MemberKind`], which is what plan §7's table originally claimed (W20):
//!
//! - **sendspin** takes it live on the same 0–100 scale, no bookkeeping beyond
//!   [`Session::saved_sendspin`];
//! - **AP2** is *driven for the session's duration* (W18). Its level is normally
//!   device-authoritative, so the session snapshots it ([`Session::saved_ap2_volumes`]),
//!   writes its own while it runs, and puts the snapshot back on every teardown path. When
//!   **no** pre-session level was known, restore writes **nothing**: on a
//!   device-authoritative knob an invented level is worse than leaving it where the
//!   calibration left it (plan §7, and [`align_levels::LevelRestore::level`]'s precedent).
//!   `ap2_volume` speaks **0.0–1.0**, so [`ap2_level`] is the one place the two scales
//!   meet;
//! - **a pw-sink host with a live agent** is the same shape as AP2 and for the same reason
//!   (W20): its level lives on the host, the agent's `SetVolume` drives it and its
//!   `HostState` reports it back, so the session snapshots
//!   ([`Session::saved_oob_levels`]), drives, and restores — writing nothing for a host
//!   whose level was never reported. [`host_level`] is where that scale meets ours;
//! - **anything else** — no agent answering, a sink with no volume lever at all
//!   ("lever: none"), a future kind — has no level knob, and unlike the mute there is *no*
//!   fallback, because the relay has a mute and no gain. Such a member is **named**
//!   ([`AlignState::unlevellable`], [`AlignState::level_note`]) because it sets the clip
//!   ceiling the others have to fit under (plan §7).
//!
//! [`align_levels::LevelRestore::level`]: crate::align::levels::LevelRestore::level
//!
//! ## Test signal
//!
//! An **alternating two-tone click** (a high "A" click, then a low "B" click,
//! one second apart → a 2 s loop, [`click_wav`]). A single uniform click would
//! be ambiguous once a member's delay approaches the click spacing — you can't
//! tell which click you're lining up. The A/B alternation disambiguates: a
//! target that has slipped a whole click lands its A on the reference's B, which
//! is audibly wrong, so offsets up to ~2 s are unmistakable.
//!
//! ## Why it's server-owned
//!
//! The session mutes the non-audible members (per-device sendspin volume), holds
//! their group exclusively and runs a looping player into that group's sync anchor.
//! If that lived in the browser, a closed tab would leave speakers muted, a click
//! looping forever and the house's routing displaced. So the daemon owns it: it
//! snapshots levels and mutes on start, restores them on stop, releases the hold,
//! and arms a safety timeout that does all of that if the UI goes away.
//!
//! Adjusting a member's offset reuses the existing knobs (sendspin static
//! delay), so this module only owns playback + muting, not the persisted offsets.

use crate::align::group::{AlignMode, ExclusiveHold, HoldDeps, Interference};
use crate::outputs::ap2::volume::SharedAp2Control;
use crate::outputs::sendspin::volume::SharedSendspinControl;
use crate::sync_group::SharedGroups;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
/// Full A→B loop length. One click per second → a 2 s pattern.
const PATTERN_SECS: f64 = 2.0;
const CLICK_MS: f64 = 8.0;
const FREQ_A: f64 = 3000.0;
const FREQ_B: f64 = 1500.0;
const CLICK_AMP: f64 = 0.5;

/// Default level (0–100) audible members play at during calibration — the same
/// for every audible member so the ear (or the estimator) judges timing, not
/// loudness. The user can raise/lower it live from the alignment panel.
///
/// **20, not 50** (plan §12.2). The measured in-room readings from the W0 device
/// spike were 65 dB and 70 dB peak SNR on the two click channels — roughly 40 dB
/// above the 25 dB the estimator wants (§7) — so the old 50 was needlessly loud for
/// a procedure carried out standing in a living room. The level-learning phase
/// tunes each member from here; this is only the starting point.
pub const DEFAULT_ALIGN_LEVEL: u8 = 20;

/// Safety net: never leave a group muted with a click looping if the UI
/// vanishes. The session tears itself down after this if not stopped.
const SESSION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How often the safety watchdog re-checks idleness. Coarse on purpose: it decides
/// nothing time-critical, and the cost of firing up to a minute late is a minute of a
/// muted room that the user has already walked away from.
const TIMEOUT_POLL: Duration = Duration::from_secs(30);

/// The **one** place the calibration level's scale meets AirPlay-2's: a calibration
/// level is an integer **0–100** (what the UI's slider, [`AlignState::volume`] and
/// [`AlignState::levels`] all carry), while [`crate::outputs::ap2::volume::Ap2Control::set_volume`]
/// takes an **f32 0.0–1.0** which the sender maps to dB.
///
/// Written as a function rather than inline arithmetic because getting it wrong is
/// silent: a factor of 100 goes straight to a clamp at either end (a 1 that should be
/// 0.01 becomes full scale on someone's amplifier), and nothing in the type system or
/// the tests below would notice a number that is merely *wrong* rather than ill-typed.
///
/// The restore direction deliberately has **no** counterpart: what teardown writes back
/// is the receiver's own snapshotted `f32`, never a round-trip through 0–100, so putting
/// a level back cannot move it by a rounding step (see [`Session::saved_ap2_volumes`]).
fn ap2_level(level: u8) -> f32 {
    f32::from(level.min(100)) / 100.0
}

/// The same meeting point for a **pw-sink host's** scale (W20): a calibration level is an
/// integer 0–100, while the receiver agent speaks the cubic **0.0–1.0** its own
/// `HostState::volume`, `wpctl` and HA's `volume_level` all use
/// ([`crate::outputs::pwsink::agent::Agents::set_volume`]).
///
/// Written out for the same reason as [`ap2_level`]: a factor of 100 the wrong way clamps
/// at a rail, and a rail on someone's desktop speakers is either silence or full scale.
/// Arithmetically identical to [`ap2_level`] today and deliberately *not* shared with it —
/// they are two independent far-end contracts, and folding them into one function would
/// make a future change to either one silently change the other.
///
/// The restore direction again has no counterpart: teardown writes back the host's own
/// snapshotted `f32` ([`Session::saved_oob_levels`]), never a round trip through 0–100, so
/// putting a level back cannot move it by a rounding step.
///
/// The knob's *taper* is not this function's business: the host applies a cubic curve of
/// its own, which is precisely the unknown `align_levels::LEVEL_TAPER_NOTE` describes and
/// the level solver measures rather than models.
fn host_level(level: u8) -> f32 {
    f32::from(level.min(100)) / 100.0
}

/// Builds the alternating two-tone click WAV (S16LE stereo, one 2 s loop).
pub fn click_wav() -> Vec<u8> {
    let total = (PATTERN_SECS * RATE as f64) as usize; // frames
    let half = total / 2;
    let click_frames = (CLICK_MS / 1000.0 * RATE as f64) as usize;
    let mut pcm = Vec::with_capacity(total * CHANNELS as usize * 2);
    for i in 0..total {
        // Click A burst at the pattern start, click B burst at the half point.
        let s = if i < click_frames {
            click_sample(i, click_frames, FREQ_A)
        } else if i >= half && i < half + click_frames {
            click_sample(i - half, click_frames, FREQ_B)
        } else {
            0.0
        };
        let v = (s * CLICK_AMP * f64::from(i16::MAX)) as i16;
        let le = v.to_le_bytes();
        pcm.extend_from_slice(&le); // FL
        pcm.extend_from_slice(&le); // FR
    }
    crate::audio::wav::build_wav(&pcm, RATE, 16, CHANNELS)
}

/// One burst sample: a sine at `freq` under a Hann envelope over the `n`-sample
/// burst, so it starts/ends at zero (no pop that would itself smear timing).
fn click_sample(i: usize, n: usize, freq: f64) -> f64 {
    let t = i as f64 / f64::from(RATE);
    let env = 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos();
    (2.0 * PI * freq * t).sin() * env
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    Sendspin,
    Airplay2,
    /// A remote PipeWire host (`pwsink-dev-*`). Alignable, with one property neither other
    /// kind has — its playout-delay knob has a **hard floor** of three packet times
    /// (`sync_settings::PWSINK_JITTER_MIN_MS`), so it cannot be placed arbitrarily early —
    /// and one thing neither of its own knobs comes with: **its transport carries neither a
    /// mute nor a level**. Both are the *host's*, reached through the receiver agent when one
    /// is answering, so both are resolved per output ([`SilenceChannel`], [`LevelChannel`])
    /// rather than being decided here (plan §7, §12.3.2 — the plan's original "no level knob
    /// in this path" was corrected by W20).
    PwSink,
}

/// A boxed future — the shape `align_measure`'s seam traits use, so the trait below can
/// be a trait object without pulling in an async-trait dependency.
type Fut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// **Seam.** Silencing *and levelling* an output **out of band**: on the host itself,
/// without altering the audio this daemon sends it.
///
/// A pw-sink host runs `pwsink_agent`, whose `set_mute`/`set_volume` drive the receiving
/// sink directly and whose `HostState` reports `muted` *and* `volume` back — so such a
/// member can be silenced and levelled and restored exactly like an in-band one, and for
/// the silencing part *better*: the stream keeps flowing, so the receiver's jitter buffer
/// never re-anchors and unmuting cannot introduce a discontinuity.
///
/// It is a trait, and optional, because the agent registry lives behind the API layer:
/// wiring it is one implementation plus one [`AlignManager::set_out_of_band_mute`] call at
/// startup. Until then — and whenever the answer is "cannot" — the relay mute
/// ([`SilenceChannel::Relay`]) takes over for audibility, which is correct, just not
/// optimal, and the level falls back to nothing at all ([`LevelChannel::None`]). Every
/// direction has to be honest for either fallback to work:
///
/// - [`Self::muted`] / [`Self::level`] answer `None` when this seam does not own the
///   output or cannot read that state: no agent connected, or a sink with neither a device
///   route nor node volume ("lever: none"), where there is simply nothing to drive.
/// - [`Self::set_muted`] / [`Self::set_level`] answer `false` when the write did not take
///   (the agent went away between the two calls). For the mute the caller falls back to
///   the relay in the same pass rather than leaving the member audible; for the level
///   there is nothing to fall back *to*, so the caller reports the member as
///   un-levellable instead — see [`LevelChannel::None`].
///
/// **One trait rather than two.** The two pairs are the same question ("what can this
/// host's agent do for me right now?") asked of the same registry, and W17's lesson was
/// that a second, separately-installed answer to a capability question is exactly how a
/// member ends up silenced by one mechanism and levelled by another that disagrees about
/// whether it exists. So there is one seam, one [`AlignManager::set_out_of_band_mute`],
/// one `OnceLock` — and the *answers* stay independent, because a host genuinely can have
/// one lever and not the other (a virtual sink reports `channel_volumes` with `mute:
/// None`).
pub trait OutOfBandMute: Send + Sync {
    /// `output`'s current mute state on its host, or `None` if this seam cannot silence it
    /// at all. Doubles as the capability query, so there is one question instead of two
    /// that can disagree.
    fn muted<'a>(&'a self, output: &'a str) -> Fut<'a, Option<bool>>;

    /// Mute/unmute `output` on its host. `false` ⇒ it did not take.
    fn set_muted<'a>(&'a self, output: &'a str, muted: bool) -> Fut<'a, bool>;

    /// `output`'s current level on its host, **on the host's own scale** (cubic 0.0–1.0,
    /// what `HostState::volume` and `/api/outputs` report), or `None` if this seam cannot
    /// level it at all.
    ///
    /// Doubles as the capability query *and* the snapshot, for the same reason
    /// [`Self::muted`] does: one question cannot disagree with itself, and a level we have
    /// never read is not a level teardown may claim to restore. The host's own scale
    /// rather than the calibration 0–100 so that putting it back is exact — the same
    /// decision as [`Session::saved_ap2_volumes`], and [`host_level`] is the one place the
    /// two scales meet.
    fn level<'a>(&'a self, output: &'a str) -> Fut<'a, Option<f32>>;

    /// Set `output`'s level on its host (cubic 0.0–1.0). `false` ⇒ it did not take, and
    /// the member has no level this run — say so, do not skip it silently.
    fn set_level<'a>(&'a self, output: &'a str, level: f32) -> Fut<'a, bool>;
}

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
    fn in_band(kind: MemberKind) -> Option<Self> {
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
    fn in_band(kind: MemberKind) -> Option<Self> {
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

#[derive(Debug, Clone, Serialize)]
pub struct AlignMember {
    pub node_name: String,
    pub kind: MemberKind,
    /// Unused for the current member kinds (sendspin + AP2 are both virtual and
    /// muted in-band, not by PipeWire node id). Always `None`; kept for the API
    /// shape.
    pub node_id: Option<u32>,
}

/// One alignable group (a source-set with ≥1 present member), for the picker.
#[derive(Debug, Clone, Serialize)]
pub struct AlignGroup {
    pub sources: Vec<String>,
    pub members: Vec<AlignMember>,
}

/// Current calibration state, echoed to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct AlignState {
    pub active: bool,
    /// The session's stable identity, as the UI compares it: the **source set** for
    /// a session started from a source-card group (`start`), the **selected output
    /// set** for one started from a selection (`start_outputs`) — in both cases the
    /// thing the user picked, so "is this session mine?" is answerable.
    ///
    /// With the union hold (plan §12.3.1) this is the *latest* `start`'s selection,
    /// which may be a subset of [`Self::outputs`] — the speakers actually held.
    pub sources: Vec<String>,
    /// The fixed member everything is aligned against.
    pub reference: Option<String>,
    /// The member currently being tuned (audible alongside the reference).
    pub target: Option<String>,
    pub members: Vec<AlignMember>,
    /// Playback level (0–100) most recently applied to the audible members — and the
    /// **default** for any member [`Self::levels`] has no entry for.
    pub volume: u8,
    /// **Per-member** calibration level (0–100), keyed by node name: what was last applied
    /// to that speaker, for every speaker this session has applied a level to (W19).
    ///
    /// Read it as `levels[node] ?? volume` — a member the run has not reached yet is simply
    /// absent, so a consumer never has to distinguish "not set" from "set to something".
    /// The session owns this (not the browser), which is the whole point: reloading the page
    /// in the middle of a walk still knows what each speaker was set to.
    ///
    /// **Not persisted**, deliberately (see the module docs): the correct level depends on
    /// where the phone is, so the same speaker wants a different one at a different position
    /// of the same run. It also has nothing to do with the *restore* obligation — what
    /// teardown puts back is the user's pre-session level, never one of these.
    pub levels: BTreeMap<String, u8>,
    /// Which acoustic promise this session is making (plan §1).
    pub mode: AlignMode,
    /// The outputs held exclusively for the session (its temporary group) — the run's
    /// **whole scope**, not one position's (plan §12.3.1).
    pub outputs: Vec<String>,
    /// Identity of the exclusive hold behind this session. **Unchanged across a
    /// `start` that re-selects a subset**, which is how a caller can see that nothing
    /// re-formed and nothing reconnected; a new number means a new group, a new anchor
    /// and a fresh reconnect wave.
    pub hold_id: u64,
    /// Did the most recent `start` reuse the hold it found (free) instead of forming
    /// one (expensive)?
    pub hold_reused: bool,
    /// What the most recent `start` cost, in words — forming a group reconnects every
    /// member, changing which held members are audible does not. Said here because it
    /// is the one thing about this API that surprises people (plan §12.3.1).
    pub hold_cost: String,
    /// Held members with **no level knob this daemon can reach** (plan §7): they constrain
    /// the others' levels instead of being tuned.
    ///
    /// A **per-output** answer since W20, not a list of `pwsink-dev-*`: a pw-sink host whose
    /// receiver agent is answering is levelled out of band like any other member, so what
    /// lands here is a host with no agent, a sink with no volume lever, or a future kind
    /// with nothing of its own. Re-resolved on every audibility change, so a member appears
    /// here the moment its agent drops mid-walk — see [`Self::level_channels`].
    pub unlevellable: Vec<String>,
    /// One sentence saying what that means — including the part that is not obvious:
    /// such a member sets the clip ceiling, so if *it* clips, turning every other
    /// speaker down cannot rescue the measurement. `None` when every member has a
    /// level knob.
    pub level_note: Option<String>,
    /// How each member's level is reached this position ([`LevelChannel`]), keyed by node
    /// name — the resolved answer [`Self::unlevellable`] is derived from, published because
    /// a consumer building a level solve needs the knob (`LevelChannel::knob`) and must not
    /// re-derive it from the member's kind.
    pub level_channels: BTreeMap<String, LevelChannel>,
    /// The members currently audible — one for level-setting/measurement, two for
    /// the by-ear comparison, N for §7's all-play round.
    pub audible: Vec<String>,
    /// Exclusivity violations recorded so far (plan §12.3), newest last. A **peek**:
    /// the measurement state machine drains these, so the status endpoint must not.
    pub interference: Vec<Interference>,
    /// The routing intent the session is displacing while it holds these speakers —
    /// what the UI shows as "these will stop playing what they are playing now".
    pub displaced: Vec<crate::store::routing::RoutingLink>,
}

impl AlignState {
    fn inactive() -> Self {
        Self {
            active: false,
            sources: Vec::new(),
            reference: None,
            target: None,
            members: Vec::new(),
            volume: DEFAULT_ALIGN_LEVEL,
            levels: BTreeMap::new(),
            mode: AlignMode::default(),
            outputs: Vec::new(),
            hold_id: 0,
            hold_reused: false,
            hold_cost: String::new(),
            unlevellable: Vec::new(),
            level_note: None,
            level_channels: BTreeMap::new(),
            audible: Vec::new(),
            interference: Vec::new(),
            displaced: Vec::new(),
        }
    }
}

/// What forming a hold costs, for [`AlignState::hold_cost`]. `why` is
/// `align_group::HoldPlan`'s reason.
fn form_cost_note(members: usize, why: &str) -> String {
    format!(
        "this start formed a new exclusive group around {members} speaker(s) ({why}) — every sendspin member reconnected for it \
         and will reconnect once more when the session ends (tens of seconds each way, plan §2.3). \
         The selection is the run's WHOLE scope: to work one position at a time, keep this session and POST /api/align/audible \
         with the speakers you can hear from where you stand. Mutes are live and cost nothing; starting again does not."
    )
}

/// What re-selecting inside the union costs: nothing.
fn scope_cost_note(selected: usize, held: usize) -> String {
    format!(
        "no speaker reconnected: the {selected} selected speaker(s) are already inside the exclusive group held for {held}, \
         so this start only changed mutes. Re-selecting held speakers is free — forming a group is what is expensive (plan §12.3.1)."
    )
}

struct Session {
    /// [`AlignState::sources`] — the identity the caller started it by.
    key: Vec<String>,
    members: Vec<AlignMember>,
    reference: Option<String>,
    target: Option<String>,
    /// The members currently audible (everything else muted).
    audible: BTreeSet<String>,
    /// Playback level (0–100) applied to the audible members, and the fallback for a
    /// member [`Self::levels`] has no entry for.
    volume: u8,
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
    levels: BTreeMap<String, u8>,
    /// Set to stop the looping player thread.
    stop: Arc<AtomicBool>,
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
    activity: Arc<std::sync::Mutex<std::time::Instant>>,
    /// Sendspin **desired levels** captured on start, restored on teardown.
    saved_sendspin: HashMap<String, u8>,
    /// Sendspin **desired mute states** captured on start. Restored exactly rather
    /// than blanket-unmuted: a member the user had muted before the session must
    /// still be muted after it, and the old teardown unmuted everything.
    saved_sendspin_mutes: HashMap<String, bool>,
    /// AP2 mute states captured on start, same reasoning.
    saved_ap2_mutes: HashMap<String, bool>,
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
    saved_ap2_volumes: HashMap<String, f32>,
    /// **Out-of-band** mute states captured on start, for members with no in-band mute
    /// whose host can silence them ([`OutOfBandMute`]) — same snapshot/restore discipline
    /// as the two above. Empty when no silencer is wired or none of them owns a member, in
    /// which case those members are silenced at the relay instead, which needs no
    /// snapshot: it is this daemon's own transient state and it is dropped wholesale on
    /// release.
    saved_oob_mutes: HashMap<String, bool>,
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
    saved_oob_levels: HashMap<String, f32>,
    /// How each member's level was reached at the last audibility pass, keyed by node name
    /// — echoed as [`AlignState::level_channels`] and the source of
    /// [`AlignState::unlevellable`].
    ///
    /// Kept on the session rather than recomputed in [`Session::state`] because resolving it
    /// asks the far end, which a status poll must never do. Seeded from the member kinds at
    /// construction (pessimistic for a pw-sink member: un-levellable until a host has
    /// actually answered) and replaced by every [`AlignManager::apply_audibility`] pass.
    level_channels: BTreeMap<String, LevelChannel>,
    /// The temporary exclusive group this session formed. Released by
    /// [`AlignManager::teardown`] on every exit path.
    ///
    /// Held over the run's **whole scope** and deliberately outliving individual
    /// `start` calls (plan §12.3.1): a `start` that re-selects a subset updates the
    /// fields above and leaves this alone.
    hold: ExclusiveHold,
    /// Did the last `start` reuse this hold rather than form it?
    hold_reused: bool,
    /// [`AlignState::hold_cost`] for the last `start`.
    hold_cost: String,
}

impl Session {
    fn state(&self) -> AlignState {
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
            interference: self.hold.interference(),
            displaced: self.hold.displaced().to_vec(),
        }
    }

    /// Mark the user as still present (see [`Session::activity`]).
    fn note_activity(&self) {
        // Poison-tolerant: this is a liveness hint, and a panic elsewhere must not
        // make the watchdog stop noticing that the user is still here.
        *self.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
    }

    fn is_member(&self, node_name: &str) -> bool {
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
    fn record_levels(&mut self, level: u8) {
        for node in &self.audible {
            self.levels.insert(node.clone(), level);
        }
    }
}

/// The alignment session manager (one at a time). Cloneable — holds shared
/// handles + the cached click WAV.
#[derive(Clone)]
pub struct AlignManager {
    session: Arc<tokio::sync::Mutex<Option<Session>>>,
    sendspin: SharedSendspinControl,
    ap2: SharedAp2Control,
    groups: SharedGroups,
    click: Arc<Vec<u8>>,
    /// Bumped by every `start` **and** every `stop`. Group formation takes seconds
    /// and must not hold the session lock (the status endpoint would stall), so a
    /// start that finds the counter moved on while it was forming knows it lost the
    /// house to someone else and tears its own half-built session down instead of
    /// clobbering theirs.
    start_gen: Arc<AtomicU64>,
    /// Serialises `start` with `start`, so two concurrent starts can never have two
    /// holds half-formed at once — the second one's teardown-then-form runs strictly
    /// after the first has finished, which is what keeps the process-global hold
    /// registry's single slot describing the newest hold. `stop` deliberately does
    /// **not** take this: stopping must work while a start is still forming (plan
    /// §12.2), and `start_gen` is what makes that safe.
    start_lock: Arc<tokio::sync::Mutex<()>>,
    /// The out-of-band silencer, if the wiring that owns the pw-sink agent registry has
    /// installed one ([`Self::set_out_of_band_mute`]). Set at most once, so reading it on
    /// every audibility change costs nothing and needs no lock.
    out_of_band: Arc<OnceLock<Arc<dyn OutOfBandMute>>>,
}

/// Per member: should it be audible? The pure half of
/// [`AlignManager::apply_audibility`], so "exactly these are audible" is testable
/// without a speaker — a set, not a (reference, target) pair, because solo-one is
/// now the primary need (plan §12.2) and §7's all-play round needs N.
fn audibility_plan(members: &[AlignMember], audible: &BTreeSet<String>) -> Vec<(String, MemberKind, bool)> {
    members.iter().map(|m| (m.node_name.clone(), m.kind, audible.contains(&m.node_name))).collect()
}

/// Resolve every member's [`SilenceChannel`] for this position — the one place the
/// question "how do I silence this member?" is answered.
///
/// In-band where the transport has a mute; otherwise the out-of-band silencer if it owns
/// the output *right now*; otherwise the relay. Re-resolved on every audibility change
/// rather than cached at formation, because an agent can disconnect mid-run and the run
/// must degrade to the relay instead of quietly leaving a speaker audible.
async fn silence_plan(
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
async fn level_plan(
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
fn kind_level_channels(members: &[AlignMember]) -> BTreeMap<String, LevelChannel> {
    members.iter().map(|m| (m.node_name.clone(), LevelChannel::in_band(m.kind).unwrap_or(LevelChannel::None))).collect()
}

/// Per member whose **level lives on its host**: the level teardown must write back, or
/// `None` for "write nothing".
///
/// The out-of-band twin of [`restore_ap2_level_plan`], pure for the same reason — the
/// "unknown ⇒ do not write" decision is the one that is silent when it goes wrong, and it
/// has to be assertable without a host. Every such member is named even when its answer is
/// `None`, so a member cannot be quietly dropped from the restore.
fn restore_oob_level_plan(members: &[AlignMember], saved: &HashMap<String, f32>) -> Vec<(String, Option<f32>)> {
    members
        .iter()
        .filter(|m| LevelChannel::in_band(m.kind).is_none())
        .map(|m| (m.node_name.clone(), saved.get(&m.node_name).copied()))
        .collect()
}

/// The level channels of members whose level is nobody's to set here — the resolved answer
/// [`AlignState::unlevellable`] reports, taken from [`level_plan`] rather than from the
/// members' kinds.
fn unlevellable_members(channels: &BTreeMap<String, LevelChannel>) -> Vec<String> {
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
fn level_note(labels: &[String]) -> Option<String> {
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
fn restore_mute_plan(
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
fn ap2_snapshot(
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
fn restore_ap2_level_plan(members: &[AlignMember], saved: &HashMap<String, f32>) -> Vec<(String, Option<f32>)> {
    members.iter().filter(|m| m.kind == MemberKind::Airplay2).map(|m| (m.node_name.clone(), saved.get(&m.node_name).copied())).collect()
}

fn same_set(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

impl AlignManager {
    pub fn new(sendspin: SharedSendspinControl, ap2: SharedAp2Control, groups: SharedGroups) -> Self {
        Self {
            session: Arc::new(tokio::sync::Mutex::new(None)),
            sendspin,
            ap2,
            groups,
            click: Arc::new(click_wav()),
            start_gen: Arc::new(AtomicU64::new(0)),
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            out_of_band: Arc::new(OnceLock::new()),
        }
    }

    /// Install the out-of-band silencer ([`OutOfBandMute`]) — once, at startup, by whoever
    /// owns the pw-sink agent registry. Returns `false` if one was already installed.
    ///
    /// Without it every member that has no in-band mute is silenced at the relay, which is
    /// correct but coarser (see the trait docs), so this is an upgrade rather than a
    /// requirement — nothing here refuses to run because it is missing.
    pub fn set_out_of_band_mute(&self, silencer: Arc<dyn OutOfBandMute>) -> bool {
        self.out_of_band.set(silencer).is_ok()
    }

    /// Build member lists for every running group (the picker's source of truth).
    ///
    /// A session's own temporary group is filtered out: it is an artefact of the
    /// running session, not something the user can pick to align.
    ///
    /// **pw-sink members are missing here**, and knowingly: `sync_group`'s
    /// `GroupSnapshot` carries only the sendspin and AP2 members, so a group's pw-sink
    /// targets cannot be listed or held from this by-ear entry point — even though
    /// [`Self::start_outputs`] admits them perfectly well (they are ordinary
    /// [`MemberKind::PwSink`] members there). Selecting them on the Outputs page is the
    /// path that works; closing this one needs the snapshot to grow a `pwsink_members`
    /// field, which belongs with `sync_group`.
    pub async fn groups(&self) -> Vec<AlignGroup> {
        let snap = self.groups.lock().await.snapshot();
        snap.into_iter()
            .filter(|g| g.sources != [crate::align::group::ALIGN_HOLD_SOURCE])
            .map(|g| {
                let mut members = Vec::new();
                for n in g.sendspin_members {
                    members.push(AlignMember { node_name: n, kind: MemberKind::Sendspin, node_id: None });
                }
                for n in g.ap2_members {
                    members.push(AlignMember { node_name: n, kind: MemberKind::Airplay2, node_id: None });
                }
                AlignGroup { sources: g.sources, members }
            })
            .collect()
    }

    /// Current state (inactive if no session is running).
    pub async fn status(&self) -> AlignState {
        self.session.lock().await.as_ref().map(Session::state).unwrap_or_else(AlignState::inactive)
    }

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
    async fn begin(&self, deps: &HoldDeps<'_>, key: Vec<String>, outputs: Vec<String>, mode: AlignMode) -> Result<AlignState, String> {
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

        // Safety timeout: tear down if still the same session after the deadline.
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
    /// - the safety timeout, re-armed so a long walk is not cut off 15 minutes after
    ///   the *first* position (the same `stop` handle, so the older watchdog still
    ///   recognises the session and does nothing).
    ///
    /// The playback level is deliberately **not** reset to [`DEFAULT_ALIGN_LEVEL`]: by
    /// this point the user (or the level phase) has tuned it, and a start that is
    /// explicitly not a restart must not blast a re-learned level back to the default.
    async fn rescope(&self, key: Vec<String>, outputs: Vec<String>, mode: AlignMode) -> Result<AlignState, String> {
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
            let volume = session.volume;
            session.record_levels(volume);
            (session.members.clone(), session.audible.clone(), volume, session.stop.clone(), session.state())
        };
        let state = self.apply_and_record(&members, &audible, volume, &stop, state).await;
        self.arm_timeout(stop);
        tracing::info!(
            "alignment session re-scoped ({mode:?}) to {:?} inside the hold over {:?}: {}",
            state.sources,
            state.outputs,
            state.hold_cost
        );
        Ok(state)
    }

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

    /// Set the audible members' playback level (0–100) live — and record it against exactly
    /// those members ([`AlignState::levels`]), since they are who it reaches.
    pub async fn set_level(&self, volume: u8) -> Result<AlignState, String> {
        let volume = volume.min(100);
        let (members, audible, stop, state) = {
            let mut guard = self.session.lock().await;
            let session = guard.as_mut().ok_or("no alignment session is running")?;
            session.volume = volume;
            session.record_levels(volume);
            (session.members.clone(), session.audible.clone(), session.stop.clone(), session.state())
        };
        Ok(self.apply_and_record(&members, &audible, volume, &stop, state).await)
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
    async fn apply_audibility(&self, members: &[AlignMember], audible: &BTreeSet<String>, volume: u8) -> BTreeMap<String, LevelChannel> {
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
                c.set_volume_transient(node, ap2_level(volume)).await;
            }
            c.set_muted(node, !on).await;
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
    async fn apply_and_record(
        &self,
        members: &[AlignMember],
        audible: &BTreeSet<String>,
        volume: u8,
        stop: &Arc<AtomicBool>,
        applied: AlignState,
    ) -> AlignState {
        let channels = self.apply_audibility(members, audible, volume).await;
        let mut guard = self.session.lock().await;
        match guard.as_mut() {
            Some(session) if Arc::ptr_eq(&session.stop, stop) => {
                session.level_channels = channels;
                session.state()
            }
            _ => applied,
        }
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
    async fn teardown(&self, mut session: Session) {
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
                c.set_muted(node, *muted).await;
            }
            for (node, level) in restore_ap2_level_plan(&session.members, &session.saved_ap2_volumes) {
                match level {
                    Some(level) => {
                        // Transient again: putting the user's own level back is not the user
                        // asking for it either, and claiming intent here would leave exactly
                        // the mark the drive side just avoided.
                        c.set_volume_transient(&node, level).await;
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
    }

    /// Spawn a watchdog that tears the session down after `SESSION_TIMEOUT`,
    /// but only if it's still the very session identified by `stop` (a newer
    /// session has its own `stop`, so a restart doesn't get killed early).
    fn arm_timeout(&self, stop: Arc<AtomicBool>) {
        let session = self.session.clone();
        let this = self.clone();
        tokio::spawn(async move {
            // Idle timeout, not a deadline: sleep in slices and only tear down once the
            // user has been *quiet* for `SESSION_TIMEOUT`. A one-shot
            // `sleep(SESSION_TIMEOUT)` killed long near-field walks mid-walk (§1.2).
            loop {
                tokio::time::sleep(TIMEOUT_POLL).await;
                if stop.load(Ordering::Relaxed) {
                    return; // already stopped
                }
                let idle = {
                    let guard = session.lock().await;
                    match guard.as_ref() {
                        // A different session now owns the slot; this watchdog is spent.
                        Some(s) if !Arc::ptr_eq(&s.stop, &stop) => return,
                        Some(s) => s.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner).elapsed(),
                        None => return,
                    }
                };
                if idle >= SESSION_TIMEOUT {
                    break;
                }
            }
            let taken = {
                let mut guard = session.lock().await;
                match guard.as_ref() {
                    Some(s) if Arc::ptr_eq(&s.stop, &stop) => guard.take(),
                    _ => None,
                }
            };
            if let Some(s) = taken {
                tracing::info!("alignment session timed out; restoring levels/mutes and releasing the exclusive hold");
                this.teardown(s).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
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
        let f =
            UnionFixture::new("idle", &[("sendspin-dev-idlea", MemberKind::Sendspin), ("sendspin-dev-idleb", MemberKind::Sendspin)]).await;

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
        let on =
            |set: &BTreeSet<String>| audibility_plan(&m, set).into_iter().filter(|(_, _, on)| *on).map(|(n, _, _)| n).collect::<Vec<_>>();
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
        let groups: crate::sync_group::SharedGroups = Arc::new(tokio::sync::Mutex::new(crate::sync_group::GroupReconciler::new()));
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
            let groups: SharedGroups = Arc::new(tokio::sync::Mutex::new(crate::sync_group::GroupReconciler::new()));
            let (changes, _changes_rx) = tokio::sync::broadcast::channel(8);
            let (sendspin, ap2) = (crate::outputs::sendspin::volume::shared(), crate::outputs::ap2::volume::shared());
            let mgr = AlignManager::new(sendspin.clone(), ap2.clone(), groups.clone());
            let members: Vec<AlignMember> = held.iter().map(|(n, k)| member(n, *k)).collect();
            let hold = crate::align::group::ExclusiveHold::for_test(
                &groups,
                &changes,
                members.clone(),
                Default::default(),
                AlignMode::MultiPosition,
            )
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
        let f =
            UnionFixture::new("reform", &[("sendspin-dev-reforma", MemberKind::Sendspin), ("sendspin-dev-reformb", MemberKind::Sendspin)])
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
        let f =
            UnionFixture::new("pwsink", &[("sendspin-dev-pwska", MemberKind::Sendspin), ("pwsink-dev-office", MemberKind::PwSink)]).await;
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
            let groups: SharedGroups = Arc::new(tokio::sync::Mutex::new(crate::sync_group::GroupReconciler::new()));
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
        let groups: SharedGroups = Arc::new(tokio::sync::Mutex::new(crate::sync_group::GroupReconciler::new()));
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
        let f =
            UnionFixture::new("mutestop", &[("sendspin-dev-mutestop", MemberKind::Sendspin), ("pwsink-dev-mutestop", MemberKind::PwSink)])
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
        f.ap2.lock().await.set_volume(ap2_node, 0.62).await;
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
        f.ap2.lock().await.set_volume(ap2_node, user_ap2).await;
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
        f.ap2.lock().await.set_volume(ap2_node, user_ap2).await;
        f.sendspin.lock().await.set_volume(spin, user_spin).apply().await;
        f.snapshot(&[(spin, user_spin)], &[(ap2_node, user_ap2)]).await;
        f.mgr.set_audible(vec![spin.into(), ap2_node.into()], 15).await.unwrap();
        let _ = f
            .mgr
            .start_outputs(&f.deps(), vec![spin.into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
            .await
            .expect_err("the fixture adopts nothing, so the re-form is refused");
        expect_restored(
            f.sendspin.lock().await.volumes().get(spin),
            f.ap2.lock().await.volumes().get(ap2_node).copied(),
            "superseding start",
        );

        // 3. The safety timeout. Its watchdog takes the session and calls exactly this
        //    teardown (a 15-minute sleep is not a test), so drive that.
        let (spin, ap2_node) = ("sendspin-dev-tdtimeout", "ap2-dev-tdtimeout");
        let f = UnionFixture::new("tdtimeout", &[(spin, MemberKind::Sendspin), (ap2_node, MemberKind::Airplay2)]).await;
        f.ap2.lock().await.set_volume(ap2_node, user_ap2).await;
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
}
