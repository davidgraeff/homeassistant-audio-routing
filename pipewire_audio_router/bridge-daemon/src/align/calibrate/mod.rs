//! Latency-alignment ("calibration") session — the backend for the alignment
//! panel (frontend `AlignPanel.svelte`).
//!
//! A session plays a test signal on every member off one clock, with **exactly the
//! members it names audible** and the rest muted. Which members are audible is what
//! the two consumers differ on: the by-ear path makes two audible (a fixed
//! **reference** and the **target** being tuned) and the user drags the target's
//! delay until they coincide; the microphone path (align/measure/mod.rs) solos **one**
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
//!
//! ## The idle timeout is part of the API, not an implementation detail
//!
//! That safety timeout is an **idle** timeout ([`SESSION_TIMEOUT`], plan §1.2) and the
//! hold it releases is **exclusive** (§12.3) — so when it fires, speakers that were held
//! go back to normal and any wizard still on screen is describing a session that no
//! longer exists. Two consequences shape the surface here:
//!
//! - the remaining time is **published** ([`AlignState::closes_in_s`], with
//!   [`AlignState::timeout_slack_s`] as the honest bound on it), because a real
//!   multi-position run ran out while its user was reading a review page — which is
//!   *quiet*, and therefore refreshes nothing. Alongside it, [`AlignManager::still_here`]
//!   is the one deliberate way to postpone it, and its docs explain why an open socket
//!   deliberately is not;
//! - the teardown is **pushed** ([`AlignManager::subscribe`], `GET /api/align/ws`).
//!   Every exit path goes through [`AlignManager::teardown`], which bumps the notifier
//!   after the restore, so "your session ended, and here is when" reaches the UI as an
//!   event instead of being inferred from a poll that came back inactive.

use crate::align::group::{AlignMode, ExclusiveHold, HoldDeps, Interference};
use crate::outputs::ap2::volume::SharedAp2Control;
use crate::outputs::sendspin::volume::SharedSendspinControl;
use crate::routing::sync_group::SharedGroups;
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
/// vanishes. The session tears itself down after this much **idleness** if not stopped.
///
/// It is reported to clients as [`AlignState::closes_in_s`], because the hold is
/// exclusive: when this fires, speakers that were held go back to normal and any wizard
/// still on screen is describing a session that no longer exists. A real
/// multi-position run walked into exactly that — reading a long review page is *quiet*,
/// so it does not refresh the timer — and the fix is that the remaining time is on
/// screen, with the thing that refreshes it named next to it.
const SESSION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How often the safety watchdog re-checks idleness. Coarse on purpose: it decides
/// nothing time-critical, and the cost of firing up to a minute late is a minute of a
/// muted room that the user has already walked away from.
///
/// It is also the **accuracy bound** on [`AlignState::closes_in_s`] and is published as
/// [`AlignState::timeout_slack_s`] for that reason: a client must say "about", because
/// the close can be this much later than the number it was given.
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
    /// (`routing::sync_settings::PWSINK_JITTER_MIN_MS`), so it cannot be placed arbitrarily early —
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
    /// How much longer this session may sit **idle** before the daemon tears it down and
    /// gives the speakers back — in whole seconds **relative to this frame**, `None`
    /// when nothing is running.
    ///
    /// Relative, never an absolute instant: the browser's clock and the daemon's differ
    /// by an unknown amount, and a client only needs to count down locally from what it
    /// was told and re-sync on the next frame.
    ///
    /// **`Some(0)` does not mean the session is gone.** It means the idle deadline has
    /// passed and the watchdog will take it at its next check, which is up to
    /// [`Self::timeout_slack_s`] away — so a UI must not render the disappearance until
    /// it is told about it (a frame with `active: false`).
    ///
    /// What refreshes it is *doing something to the run*, not looking at it: making
    /// members audible or changing a level ([`AlignManager::set_audible`],
    /// [`AlignManager::solo`], [`AlignManager::select`], [`AlignManager::set_level`]), a
    /// re-scoping `start`, or the deliberate [`AlignManager::still_here`]. Reading a
    /// proposal refreshes nothing, and neither does holding a socket open — see
    /// `still_here`'s docs for why that asymmetry is the whole point.
    pub closes_in_s: Option<u64>,
    /// The whole idle allowance ([`SESSION_TIMEOUT`]) in seconds — what
    /// [`Self::closes_in_s`] counts down from, so a client can phrase the rule ("15
    /// minutes without a change") without hard-coding the daemon's number.
    pub idle_timeout_s: u64,
    /// How much later than [`Self::closes_in_s`] the close can actually happen
    /// ([`TIMEOUT_POLL`]), because the watchdog is a poller.
    ///
    /// Published so a client is not left inventing a fudge factor: the honest rendering
    /// of `closes_in_s` is "in about N", and this is the size of "about".
    pub timeout_slack_s: u64,
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
            // No session, so nothing is counting down — but the two *rules* are still
            // worth stating, so a client that has only ever seen the inactive frame can
            // already say what the timeout is and how precise it is not.
            closes_in_s: None,
            idle_timeout_s: SESSION_TIMEOUT.as_secs(),
            timeout_slack_s: TIMEOUT_POLL.as_secs(),
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
            closes_in_s: Some(self.closes_in().as_secs()),
            idle_timeout_s: SESSION_TIMEOUT.as_secs(),
            timeout_slack_s: TIMEOUT_POLL.as_secs(),
        }
    }

    /// Mark the user as still present (see [`Session::activity`]).
    fn note_activity(&self) {
        // Poison-tolerant: this is a liveness hint, and a panic elsewhere must not
        // make the watchdog stop noticing that the user is still here.
        *self.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
    }

    /// How long this session has looked idle to the safety watchdog.
    ///
    /// One reader for both consumers — the watchdog's decision and
    /// [`AlignState::closes_in_s`] — so the number the user is counting down and the
    /// number the teardown is decided on cannot be two different opinions.
    fn idle(&self) -> Duration {
        self.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner).elapsed()
    }

    /// How much idle time is left before the watchdog would tear this session down.
    /// Saturating: past the deadline it is zero, which is honest — the session is
    /// *awaiting* its teardown, not already gone (see [`AlignState::closes_in_s`]).
    fn closes_in(&self) -> Duration {
        SESSION_TIMEOUT.saturating_sub(self.idle())
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
    /// Bumped whenever [`Self::status`] would answer differently, so `GET /api/align/ws`
    /// can push instead of the wizard polling — the same `watch` shape
    /// `align::measure`'s run socket uses.
    ///
    /// It lives on the **manager**, not on the session, and that is the whole reason it
    /// is worth having: the state change no client can predict is the session *ending*
    /// — by the idle timeout, by a superseding `start`, or by an explicit stop — and a
    /// notifier owned by the session would be dropped by exactly that event. Here
    /// [`Self::teardown`] bumps it on every exit path, so a disappearance arrives as an
    /// event rather than being noticed a few seconds later by a poll.
    changes: Arc<tokio::sync::watch::Sender<u64>>,
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
            changes: Arc::new(tokio::sync::watch::channel(0).0),
        }
    }

    /// A receiver that fires whenever [`Self::status`] would return something new —
    /// including the teardown that makes it inactive. Read by `GET /api/align/ws`.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Tell every session-socket subscriber that [`Self::status`] would now answer
    /// differently. Cheap and never fails: `watch` coalesces, and a bump with no
    /// subscribers is a counter increment.
    ///
    /// Not called for everything that can move `AlignState`: `interference` is appended
    /// by the announce arbiter and the duck holder through the *hold*, which has no way
    /// back to here, so those entries still arrive on the next poll. That is why the
    /// client keeps polling alongside the socket rather than treating it as complete.
    fn bump(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
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
    async fn apply_and_record(
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
    fn arm_timeout(&self, stop: Arc<AtomicBool>) {
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

#[cfg(test)]
mod tests;
