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
use crate::align::relay_delay::MeasureChannels;
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
}

// The session's pieces. Re-exported so the code and its tests keep addressing them
// by name: these boundaries organise the file, they are not an interface.
mod audibility;
mod click;
mod session;
mod status;

pub(crate) use audibility::*;
pub(crate) use click::*;
pub(crate) use session::*;
pub(crate) use status::*;

#[cfg(test)]
mod tests;
