//! Playback-level learning for microphone-assisted alignment
//! (docs/mic-alignment-plan.md §7).
//!
//! Solves the **two-sided** level constraint a measurement needs: the *sum* of all
//! members must stay under the microphone's clipping ceiling (clipping is
//! broadband, so one clipped block corrupts every channel, not just the loud
//! speaker's), while each *individual* channel needs enough margin over the noise
//! and reverb floor in its own band.
//!
//! Because the measurement channels are separable, this parallelises: every member
//! ramps at once and each channel's SNR is read independently. The same pass yields
//! the crosstalk matrix that validates a frequency assignment (plan §6.2).
//!
//! Pure decision logic over observations — no I/O, no daemon state — so it is
//! testable without hardware and without a microphone.
//!
//! ## How it is driven
//!
//! The orchestrator (`align/measure.rs`, W3) owns the audio, the volume knobs and
//! the API; this module owns only the arithmetic:
//!
//! ```ignore
//! let mut solver = LevelSolver::new(specs)?;
//! let mut step = solver.begin();
//! loop {
//!     apply(&step.changed);                 // write levels (sendspin/AP2)
//!     excite(&step.excite);                 // mute/unmute per step.excite
//!     let obs = capture_one_round();        // ≥ MIN_PERIODS_USED pattern periods
//!     match solver.observe(obs) {
//!         LevelDecision::Continue(next) => step = next,
//!         LevelDecision::Converged(plan) => break Ok(plan),
//!         LevelDecision::Refused(why) => break Err(why),
//!     }
//! }
//! ```
//!
//! Every decision is a pure function of the observations fed in, so the whole
//! ramp is unit-testable against a simulated room (see the tests at the bottom:
//! cold start, a 20 dB near/far spread, an infeasible room, a member with no
//! knob).
//!
//! ## Both sides of the constraint are *measured*, not modelled
//!
//! The quiet side comes from the estimator's own [`ChannelEstimate::peak_snr_db`].
//! The loud side comes from the ingest's aggregate peak
//! (`align_mic::MicStatus::peak`) plus its sticky clip flag. Deliberately no
//! per-member absolute-SPL model: that would need a calibrated microphone, and
//! "the observed aggregate peak must stay under the ceiling" *is* the sum
//! constraint, only measured instead of guessed.
//!
//! The one thing that cannot be measured before it is written is the level knob's
//! taper (see [`LEVEL_TAPER_NOTE`]), so the first correction uses an assumed taper
//! and every later one uses the taper realised by the previous round. A wrong
//! assumption therefore costs extra rounds, never a wrong fixed point.
//!
//! ## What "member kind" changes — and what it does *not* decide
//!
//! Plan §7: sendspin has a live volume, and AP2 has one but leaves it
//! device-authoritative outside the session (so it needs explicit
//! snapshot/restore, [`LevelSolver::restore_plan`]).
//!
//! **A pw-sink member's level is not a property of its kind** (W20, plan §12.3.2).
//! Its host runs the receiver agent, whose `SetVolume` drives the receiving sink
//! and whose `HostState` reports the level back — so a pw-sink host whose agent
//! answers *right now* is levellable exactly like AP2, and one whose agent is gone
//! (or whose sink has no volume lever at all) is not. Two members of the same kind
//! can therefore differ, and one member's answer can change between two positions
//! of the same run. The resolution happens where the capability can be *asked*
//! (`calibrate::level_plan`) and arrives here as
//! [`LevelMemberSpec::with_knob`] — [`LevelMemberKind::knob`] is only the floor a
//! kind guarantees by itself.
//!
//! A member that cannot be turned down does not drop out of the solve — it
//! *constrains* it, because its share of the clip ceiling is fixed and the
//! adjustable members have to fit in what is left. That asymmetry is modelled in
//! [`LevelKnob`] and is why [`RefusalReason::FixedMemberClips`] and
//! [`RefusalReason::NoLevelKnob`] exist as separate refusals.

#![allow(dead_code)] // driven by the orchestration in W3 (plan §14); unit-tested now.

use crate::align::calibrate::MemberKind;
use crate::align::estimator::{ChannelEstimate, Estimate, MIN_PEAK_SNR_DB};
use serde::Serialize;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Targets and margins
// ---------------------------------------------------------------------------

/// Peak SNR every channel must reach before measuring, in dB.
///
/// The estimator refuses below [`MIN_PEAK_SNR_DB`] (15 dB), so 25 dB leaves a
/// **10 dB margin**. The margin is not arbitrary:
///
/// * Plan §5.4.1: the cliff from "0.15 ms" to "meaningless" spans only ~3 dB, and
///   15 dB sits *on* the safe side of it — landing the learning phase there would
///   mean measuring at the edge.
/// * The floor the SNR is measured against is the noise **and reverb** floor, and
///   it is not stationary: it rises when the other members are excited together
///   (plan §6.2 calls the reverb pile-up the real scaling limit), and a room's
///   noise floor moves several dB between one 6 s round and the next (a fridge, a
///   car, the user breathing on the phone).
/// * Levels are written through a coarse 0–100 knob whose taper is unknown, so the
///   realised level lands within a dB or two of the intended one.
///
/// 10 dB means the measurement still passes its own refusal check after the floor
/// rises 10 dB between learning and measuring. Above ~30 dB the extra margin buys
/// nothing measurable (plan §5.4.1: "noise is not the limiting factor") and costs
/// clip headroom, which is the side that fails hard.
pub const TARGET_PEAK_SNR_DB: f64 = 25.0;

/// A channel this far above [`TARGET_PEAK_SNR_DB`] gets trimmed back even when
/// nothing is clipping. Surplus level is not free: it eats the shared clip
/// ceiling and raises the reverb floor in *every other* channel. 10 dB of slack
/// keeps the loop from chasing the knob's quantisation.
pub const SURPLUS_TOLERANCE_DB: f64 = 10.0;

/// Aim a raise this far past the target so the next round lands above it rather
/// than exactly on it.
pub const RAISE_OVERSHOOT_DB: f64 = 2.0;

/// Largest level change (in realised dB) any single round may command. A ramp
/// that jumps further than this can overshoot straight into clipping, and a
/// clipped round is wasted for *every* channel, not just the one that jumped.
pub const MAX_STEP_DB: f64 = 12.0;

/// Changes smaller than this are not worth a round trip (a level write on a
/// sendspin device is a protocol round trip, and on AP2 an RTSP `SET_PARAMETER`).
pub const MIN_STEP_DB: f64 = 1.5;

/// Cold-start level (0–100). Below `calibrate::DEFAULT_CAL_VOLUME` (50) on
/// purpose: ramping up costs one extra round, whereas starting into clipping
/// invalidates the round *and* is unpleasant for anyone in the room.
pub const START_LEVEL: u8 = 30;

/// Lowest level the solve will command. Under this many device tapers collapse to
/// silence and the realised dB-per-step becomes wildly nonlinear, so a level below
/// it is not a usable operating point — "cannot be turned down further" is the
/// honest verdict instead.
pub const MIN_LEVEL: u8 = 5;
/// Highest level the knobs accept (`outputs::sendspin::volume::set_volume` clamps to 100).
pub const MAX_LEVEL: u8 = 100;

/// Target for the ingest's aggregate peak (`align_mic::MicStatus::peak`, 0.0–1.0)
/// while all members are excited: −6 dBFS.
///
/// **Assumption awaiting a real room.** With AGC and every other browser
/// processing off (plan §4.2) nothing manages headroom, and the hardware gain
/// ahead of the ADC is chosen by the phone, not by us — so the *acoustic* level
/// at which a given phone clips is unknown and unknowable from here. What we can
/// see is the digital peak. 6 dB of headroom covers a transient the meter's decay
/// smoothed over (`MicStatus::peak` is a decaying peak, so it is a *lower* bound
/// on the true recent peak) plus the few dB a hand-held phone moves by. W0/W3
/// should record what real devices report here; if 0.5 turns out to leave whole
/// channels unnecessarily quiet, it is one constant.
pub const CLIP_TARGET_PEAK: f32 = 0.5;

/// Aggregate peak that is treated as clipping even without a clip flag: −3 dBFS.
/// Above this, the next transient will clip, and finding out costs a round.
pub const CLIP_BACKOFF_PEAK: f32 = 0.71;

/// Smallest cut worth making in response to clipping. A member with less surplus
/// than this cannot be cut without pushing its own channel towards the target, so
/// it is not a candidate — that is what turns "clipping" into a refusal instead of
/// an infinite trade-off between two channels.
pub const CLIP_CUT_MIN_DB: f64 = 3.0;

/// Round budget for [`RampMode::Parallel`]: every member ramps in the same round,
/// so the count does not grow with N.
///
/// Round 1 observes the cold start. Round 2 applies the correction computed with
/// the *assumed* taper. Rounds 3+ apply corrections computed with each member's
/// *measured* taper, which is exact to the knob's local nonlinearity — so two to
/// three rounds is the expected case (asserted by the tests) and the remaining
/// budget absorbs a clip backoff and a floor that moved. At ≥3 pattern periods
/// (~6 s) per round the typical case is ~12–18 s, inside plan §8's ~20 s learning
/// budget; the 6-round worst case is ~36 s and overruns it. That overrun is
/// deliberate: a slow success beats a refusal.
pub const MAX_PARALLEL_ROUNDS: usize = 6;

/// Round budget for [`RampMode::Sequential`], per member, plus
/// [`SEQUENTIAL_ROUNDS_SLACK`]. One round to read the member, one to correct it;
/// the slack covers the aggregate check and one clip backoff.
pub const SEQUENTIAL_ROUNDS_PER_MEMBER: usize = 2;
/// See [`SEQUENTIAL_ROUNDS_PER_MEMBER`].
pub const SEQUENTIAL_ROUNDS_SLACK: usize = 4;

/// Crosstalk at or below this (dB, relative to the driven channel) is harmless.
///
/// A member's leakage into another member's band arrives at a *different* time
/// than that band's own burst, so it competes as the estimator's "second peak".
/// The estimator refuses when the runner-up is within
/// [`MIN_SECOND_PEAK_RATIO`](crate::align::estimator::MIN_SECOND_PEAK_RATIO) (1.4× ≈ −3 dB) of the winner, so −20 dB (10×) keeps
/// a 7× margin over its own ambiguity floor — room enough for the room to add a
/// reflection of its own at a similar level without tipping the verdict.
pub const MAX_CROSSTALK_DB: f64 = -20.0;

/// Crosstalk between this and [`MAX_CROSSTALK_DB`] is *marginal*: still ~4× clear
/// of the estimator's [`MIN_SECOND_PEAK_RATIO`](crate::align::estimator::MIN_SECOND_PEAK_RATIO), so nothing refuses today, but a
/// single early reflection off the leaking speaker can close the remaining gap —
/// and plan §5.6 is explicit that a merged early reflection is the one error this
/// signal design cannot detect. Reassign frequencies if it can be done cheaply.
pub const MARGINAL_CROSSTALK_DB: f64 = -12.0;

/// Plan §6.2: minimum spacing between assigned burst frequencies.
pub const MIN_CHANNEL_SPACING_HZ: f64 = 500.0;
/// Plan §6.2: no channel within this of 2× or 3× another channel, because a burst
/// driven hard makes harmonic distortion in the speaker and a harmonic landing on
/// another channel is a *stable* spurious peak — far worse than noise.
pub const HARMONIC_GUARD_HZ: f64 = 300.0;
/// Plan §6.2: usable band of a phone microphone plus a small speaker.
pub const USABLE_BAND_HZ: (f64, f64) = (800.0, 6000.0);

/// **Assumption awaiting a real room.** The 0–100 level knob is treated as linear
/// in *amplitude*, i.e. `gain_db = 20·log10(level/100)`.
///
/// It is a guess in both transports, for different reasons: the sendspin device
/// applies the 0–100 value in its own firmware (`outputs::sendspin::volume::volume_cmd`
/// just ships the number), and AP2 maps 0.0–1.0 to dB inside the vendored sender
/// (`ap2_volume`, `airplay_client::Connection::set_volume`) with a curve we do not
/// control. Either could be linear-amplitude, cubic, or dB-linear.
///
/// This is safe to get wrong because the loop is closed: the taper only sets the
/// *step size*, and after a member's first level change its realised dB-per-
/// commanded-dB is measured from the SNR it actually produced and used from then
/// on (clamped to [`TAPER_MIN`]…[`TAPER_MAX`]). A wrong assumption costs rounds,
/// not accuracy. If a device turns out to be badly nonlinear, the fix is to seed
/// the taper per member kind rather than to change this doc.
pub const LEVEL_TAPER_NOTE: &str = "level 0-100 assumed linear in amplitude; measured per member after the first change";

/// Clamp on the measured taper (realised dB per commanded dB). Outside this range
/// the "measurement" is almost certainly the room floor moving, not the knob.
pub const TAPER_MIN: f64 = 0.3;
/// See [`TAPER_MIN`].
pub const TAPER_MAX: f64 = 3.0;
/// A commanded change smaller than this teaches nothing about the taper (the SNR
/// reading's own scatter dominates).
const MIN_TAPER_COMMAND_DB: f64 = 2.0;

// ---------------------------------------------------------------------------
// Level arithmetic
// ---------------------------------------------------------------------------

/// Level (0–100) → gain in dB under the assumed taper ([`LEVEL_TAPER_NOTE`]).
/// Level 100 is 0 dB (the reference), everything below is negative.
pub fn level_to_db(level: u8) -> f64 {
    20.0 * (f64::from(level.max(1)) / 100.0).log10()
}

/// Inverse of [`level_to_db`], clamped to the usable knob range.
pub fn db_to_level(db: f64) -> u8 {
    let raw = 100.0 * 10f64.powf(db / 20.0);
    raw.round().clamp(f64::from(MIN_LEVEL), f64::from(MAX_LEVEL)) as u8
}

/// AP2's knob is a 0.0–1.0 scalar (`ap2_volume::set_volume`), so the solve's
/// 0–100 level converts by /100. The receiver's own mapping of that scalar to dB
/// is device-side and is exactly the unknown [`LEVEL_TAPER_NOTE`] describes.
pub fn ap2_scalar(level: u8) -> f32 {
    f32::from(level.min(MAX_LEVEL)) / 100.0
}

/// Move `level` by `delta_db` of *realised* acoustic change, given the member's
/// measured `taper` (realised dB per commanded dB).
fn shift_level(level: u8, delta_db: f64, taper: f64) -> u8 {
    db_to_level(level_to_db(level) + delta_db / taper.clamp(TAPER_MIN, TAPER_MAX))
}

/// Linear amplitude (0.0–1.0) → dBFS, floored so a silent capture is finite.
fn peak_to_dbfs(peak: f32) -> f64 {
    20.0 * f64::from(peak.max(1e-6)).log10()
}

// ---------------------------------------------------------------------------
// Member model
// ---------------------------------------------------------------------------

/// What can be done to a member's playback level during the session (plan §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelKnob {
    /// Live, no bookkeeping needed beyond the session's existing snapshot
    /// (`calibrate::Session::saved_sendspin`).
    Live,
    /// Exists, but the level is normally device-authoritative — the session must
    /// snapshot it before the first write and restore it on teardown
    /// ([`LevelSolver::restore_plan`]).
    ///
    /// Two members wear this: an **AP2** receiver (`ap2_volume`), and a **pw-sink**
    /// host whose receiver agent is answering right now (W20) — the same shape for
    /// the same reason, that the level lives on the device and is only *readable*
    /// because the far end reports it.
    SnapshotRestore,
    /// No level knob at all: the solve can only report "too quiet / too loud, change
    /// it at the device".
    ///
    /// Still reachable, and not a legacy arm — a sink with neither a device route nor
    /// a node volume (the agent's own diagnostic prints `lever: <none>` for exactly
    /// that), or a pw-sink host whose agent is not connected, genuinely has no level
    /// this daemon can reach. There is no fallback for it either: unlike the *mute*,
    /// which the relay can always impose (plan §12.3.2/W17), the relay has no gain —
    /// so this is the arm that makes a member set the clip ceiling.
    None,
}

impl LevelKnob {
    pub fn is_adjustable(self) -> bool {
        !matches!(self, LevelKnob::None)
    }
}

/// Member kinds as far as *levels* are concerned. A 1:1 mirror of
/// `calibrate::MemberKind` since the `PwSink` variant landed (this used to be a
/// deliberate superset, because the session could not represent a pw-sink member at
/// all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelMemberKind {
    Sendspin,
    Airplay2,
    PwSink,
}

impl LevelMemberKind {
    /// The knob this kind guarantees **by itself** — the floor, not the answer.
    ///
    /// `PwSink` answers [`LevelKnob::None`] because its own transport carries no
    /// level: whether such a member is levellable depends on whether its host's
    /// agent is answering *right now*, which cannot be decided from a kind and is
    /// not decided here (W20). A caller that can ask the host resolves it and hands
    /// the answer over as [`LevelMemberSpec::with_knob`]; a caller that cannot is
    /// left with the pessimistic answer, which is the safe direction — a member
    /// wrongly believed levellable would have its clip-ceiling role hidden, and plan
    /// §7 says naming that member is the whole point of the refusal.
    pub fn knob(self) -> LevelKnob {
        match self {
            LevelMemberKind::Sendspin => LevelKnob::Live,
            LevelMemberKind::Airplay2 => LevelKnob::SnapshotRestore,
            LevelMemberKind::PwSink => LevelKnob::None,
        }
    }

    pub fn is_adjustable(self) -> bool {
        self.knob().is_adjustable()
    }
}

impl From<MemberKind> for LevelMemberKind {
    fn from(k: MemberKind) -> Self {
        match k {
            MemberKind::Sendspin => LevelMemberKind::Sendspin,
            MemberKind::Airplay2 => LevelMemberKind::Airplay2,
            // No level knob in this path: it constrains the others rather than being
            // tuned (plan §7 — and the dangerous case is one that *clips*).
            MemberKind::PwSink => LevelMemberKind::PwSink,
        }
    }
}

/// One member as handed to the solve.
#[derive(Debug, Clone, Serialize)]
pub struct LevelMemberSpec {
    pub node_name: String,
    /// The estimator channel (`align_estimator::ChannelSpec::label`) this member's
    /// burst lands in. Under the shared click track (plan §2.2) several members may
    /// share a label — legal in [`RampMode::Sequential`], where only one member is
    /// excited at a time, and rejected in [`RampMode::Parallel`], where it would
    /// make the SNRs unattributable.
    pub channel: String,
    pub kind: LevelMemberKind,
    /// Level to restore on teardown, if known. `None` is legitimate for both
    /// device-authoritative knobs: an AP2 receiver's pre-session level is unknown
    /// until a device reports one (`ap2_volume` never invents a value), and a pw-sink
    /// host's is unknown until its agent has reported a `HostState` — and the honest
    /// restore for an unknown level is to leave the far end alone.
    pub snapshot_level: Option<u8>,
    /// The knob this **output** actually has, when the caller could ask (W20).
    ///
    /// `None` means "nobody asked", and then [`Self::knob`] falls back to what the
    /// kind guarantees. Set it for a member whose capability was resolved per output:
    /// a pw-sink host with a live agent is [`LevelKnob::SnapshotRestore`], the same
    /// one with no agent is [`LevelKnob::None`]. It is an override rather than a
    /// replacement so that the caller that *cannot* ask (`align_measure`'s Stage-1
    /// seam builds specs from kinds alone) keeps compiling and keeps the pessimistic
    /// answer.
    pub knob_override: Option<LevelKnob>,
}

impl LevelMemberSpec {
    pub fn new(node_name: impl Into<String>, channel: impl Into<String>, kind: LevelMemberKind) -> Self {
        Self { node_name: node_name.into(), channel: channel.into(), kind, snapshot_level: None, knob_override: None }
    }

    pub fn with_snapshot(mut self, level: u8) -> Self {
        self.snapshot_level = Some(level);
        self
    }

    /// Record the knob this output was resolved to have (see [`Self::knob_override`]).
    pub fn with_knob(mut self, knob: LevelKnob) -> Self {
        self.knob_override = Some(knob);
        self
    }

    /// **The** level knob for this member: the resolved per-output answer where one
    /// was given, else what the kind guarantees. Every decision in this module reads
    /// it through here, so the solve, the reports and the restore plan cannot end up
    /// with three opinions about one member.
    pub fn knob(&self) -> LevelKnob {
        self.knob_override.unwrap_or_else(|| self.kind.knob())
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How the orchestrator can excite members, which decides the round sequencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RampMode {
    /// Per-member bands (plan §6.2/W7): all members ramp in one round and each
    /// channel's SNR is read independently. Round count is flat in N.
    Parallel,
    /// Shared content (plan §6.1/W3): only a soloed member's level can be read, so
    /// the ramp visits members one at a time. Round count grows with N — and the
    /// solo rounds are exactly the crosstalk rows, so those come free here.
    Sequential,
}

#[derive(Debug, Clone, Serialize)]
pub struct LevelConfig {
    pub mode: RampMode,
    pub target_snr_db: f64,
    pub surplus_tolerance_db: f64,
    pub clip_target_peak: f32,
    pub clip_backoff_peak: f32,
    pub start_level: u8,
    /// Run solo rounds after the ramp converges to fill the crosstalk matrix.
    /// Needed in [`RampMode::Parallel`] because the estimator reports one peak per
    /// channel per window, so leakage arriving in another member's slot cannot be
    /// separated out of a round where everything plays at once — it takes a solo.
    pub crosstalk_pass: bool,
    /// Explicit round cap. `None` derives it from [`RampMode`] and the member
    /// count; see [`MAX_PARALLEL_ROUNDS`] and [`SEQUENTIAL_ROUNDS_PER_MEMBER`].
    pub max_rounds: Option<usize>,
}

impl Default for LevelConfig {
    fn default() -> Self {
        Self {
            mode: RampMode::Parallel,
            target_snr_db: TARGET_PEAK_SNR_DB,
            surplus_tolerance_db: SURPLUS_TOLERANCE_DB,
            clip_target_peak: CLIP_TARGET_PEAK,
            clip_backoff_peak: CLIP_BACKOFF_PEAK,
            start_level: START_LEVEL,
            crosstalk_pass: true,
            max_rounds: None,
        }
    }
}

impl LevelConfig {
    /// The Stage-1 (W3) configuration: shared click track, solo excitation, no
    /// separate crosstalk pass (the frequency assignment does not exist yet).
    pub fn sequential() -> Self {
        Self { mode: RampMode::Sequential, crosstalk_pass: false, ..Self::default() }
    }
}

// ---------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------

/// Which members were audible during an observed round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "excite", rename_all = "snake_case")]
pub enum Excitation {
    /// Every member audible — the only round that measures the *aggregate* peak,
    /// which is the clipping side of the constraint.
    All,
    /// One member audible. Attributes both its own SNR and, in every other
    /// channel, its leakage (a crosstalk row).
    Solo { node_name: String },
}

impl Excitation {
    pub fn solo(node_name: impl Into<String>) -> Self {
        Excitation::Solo { node_name: node_name.into() }
    }
}

/// One channel's reading, taken straight from the estimator.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelReading {
    pub label: String,
    pub peak_snr_db: f64,
    pub second_peak_ratio: f64,
}

impl ChannelReading {
    pub fn new(label: impl Into<String>, peak_snr_db: f64, second_peak_ratio: f64) -> Self {
        Self { label: label.into(), peak_snr_db, second_peak_ratio }
    }
}

impl From<&ChannelEstimate> for ChannelReading {
    fn from(c: &ChannelEstimate) -> Self {
        Self { label: c.label.clone(), peak_snr_db: c.peak_snr_db, second_peak_ratio: c.second_peak_ratio }
    }
}

/// Everything one ramp round observed.
#[derive(Debug, Clone, Serialize)]
pub struct RoundObservation {
    pub excited: Excitation,
    pub channels: Vec<ChannelReading>,
    /// A sample reached full scale during the round (`align_mic::MicWindow::clipped`
    /// or a non-zero `Estimate::clipped_samples`).
    pub clipped: bool,
    /// Aggregate peak over the round, 0.0–1.0 (`align_mic::MicStatus::peak`).
    pub mic_peak: f32,
}

impl RoundObservation {
    /// Build from an estimator report. Assumes the estimator was reset for this
    /// round — which the orchestrator does anyway, because every measuring
    /// transition re-acquires loop-phase lock (plan §8) — so `clipped_samples`
    /// counts this round only. Override [`Self::clipped`] if that ever changes.
    pub fn from_estimate(excited: Excitation, est: &Estimate, mic_peak: f32) -> Self {
        Self { excited, channels: est.channels.iter().map(ChannelReading::from).collect(), clipped: est.clipped_samples > 0, mic_peak }
    }

    fn reading(&self, label: &str) -> Option<&ChannelReading> {
        self.channels.iter().find(|c| c.label == label)
    }
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// Why a round is being run — for the UI's progress line and for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RampPurpose {
    /// Cold start: nothing has been observed yet.
    Probe,
    /// Moving levels towards the target.
    Ramp,
    /// Coming down from clipping (or from within [`CLIP_BACKOFF_PEAK`] of it).
    ClipBackoff,
    /// Solo round to fill a crosstalk row.
    Crosstalk,
    /// Everything is in band individually; confirm the *sum* does not clip.
    AggregateCheck,
}

/// The level set to apply for the next round.
#[derive(Debug, Clone, Serialize)]
pub struct MemberLevel {
    pub node_name: String,
    pub level: u8,
    pub kind: LevelMemberKind,
    pub knob: LevelKnob,
}

/// One step of the ramp: apply these levels, excite this, observe one round.
#[derive(Debug, Clone, Serialize)]
pub struct RampStep {
    /// 1-based index of the round about to be run.
    pub round: usize,
    /// The cap this run must stay inside ([`LevelSolver::round_budget`]).
    pub round_budget: usize,
    pub purpose: RampPurpose,
    pub excite: Excitation,
    /// The full level set — idempotent, safe to re-apply.
    pub levels: Vec<MemberLevel>,
    /// Only the entries that changed since the previous step, so the orchestrator
    /// can skip write round trips. Never contains a [`LevelKnob::None`] member.
    pub changed: Vec<MemberLevel>,
}

/// Per-member outcome, in both the converged plan and the refusal.
#[derive(Debug, Clone, Serialize)]
pub struct MemberLevelReport {
    pub node_name: String,
    pub channel: String,
    pub kind: LevelMemberKind,
    pub knob: LevelKnob,
    pub level: u8,
    /// Last observed peak SNR for this member's channel; `None` if never read.
    pub peak_snr_db: Option<f64>,
    /// Headroom over the estimator's own refusal threshold
    /// ([`MIN_PEAK_SNR_DB`]) — the number that says whether measuring will work.
    pub margin_db: Option<f64>,
    /// Last observed largest-to-runner-up peak ratio. Not a level decision, but it
    /// is the early warning for in-band leakage, so it is carried through.
    pub second_peak_ratio: Option<f64>,
    /// Realised dB per commanded dB, measured during the ramp (1.0 = the assumed
    /// taper, [`LEVEL_TAPER_NOTE`]).
    pub measured_taper: f64,
    pub reached_target: bool,
    /// What the user has to do by hand, when the daemon cannot do it.
    pub advice: Option<String>,
}

/// A converged level solve, fed forward into the measurement stage.
#[derive(Debug, Clone, Serialize)]
pub struct LevelPlan {
    pub levels: Vec<MemberLevel>,
    pub members: Vec<MemberLevelReport>,
    pub rounds_used: usize,
    pub target_snr_db: f64,
    /// Aggregate peak of the confirming all-members round (0.0–1.0).
    pub aggregate_peak: Option<f32>,
    pub crosstalk: CrosstalkReport,
    /// Non-fatal notes for the UI (a marginal frequency assignment, a member that
    /// could not be trimmed, …).
    pub warnings: Vec<String>,
}

/// Why the level phase refused. Plan §7: refusing is the required outcome — do not
/// hand a best-effort level set to the measurement stage with a warning attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// Adjustable member is at [`MAX_LEVEL`] and still below target.
    TooQuietAtMaxLevel,
    /// [`LevelKnob::None`] member below target: nothing to turn up from here.
    NoLevelKnob,
    /// The target cannot be reached for `member` without driving the microphone
    /// into clipping; `blocking_member` is the one setting the ceiling.
    ClipCeiling,
    /// A member that cannot be turned down is itself driving the mic into
    /// clipping, so lowering the adjustable members cannot rescue the round.
    FixedMemberClips,
    /// The round budget ran out without converging.
    RoundBoundExhausted,
    /// A member's channel never appeared in a round that excited it — the channel
    /// assignment and the estimator config disagree. A wiring bug, not a room.
    MissingChannel,
    /// Nothing to solve.
    NoMembers,
}

/// A refusal, naming the member the user can act on.
#[derive(Debug, Clone, Serialize)]
pub struct LevelRefusal {
    pub reason: RefusalReason,
    /// The member the message is about, and the one worth showing first.
    pub member: String,
    /// For [`RefusalReason::ClipCeiling`]: the *other* member, the loud one whose
    /// level is setting the ceiling. Naming only the quiet one would send the user
    /// to the wrong speaker.
    pub blocking_member: Option<String>,
    /// A sentence to show the user, with the numbers in it.
    pub message: String,
    pub members: Vec<MemberLevelReport>,
    pub rounds_used: usize,
    pub crosstalk: CrosstalkReport,
}

/// The solve's answer to one observed round.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum LevelDecision {
    Continue(RampStep),
    Converged(Box<LevelPlan>),
    Refused(Box<LevelRefusal>),
}

impl LevelDecision {
    pub fn is_refused(&self) -> bool {
        matches!(self, LevelDecision::Refused(_))
    }

    pub fn plan(&self) -> Option<&LevelPlan> {
        match self {
            LevelDecision::Converged(p) => Some(p),
            _ => None,
        }
    }

    pub fn refusal(&self) -> Option<&LevelRefusal> {
        match self {
            LevelDecision::Refused(r) => Some(r),
            _ => None,
        }
    }
}

/// What the orchestrator must undo on teardown.
#[derive(Debug, Clone, Serialize)]
pub struct LevelRestore {
    pub node_name: String,
    pub kind: LevelMemberKind,
    pub knob: LevelKnob,
    /// The level to write back. `None` = no pre-session level was known, so do not
    /// write one (see [`LevelMemberSpec::snapshot_level`]).
    pub level: Option<u8>,
    pub note: String,
}

// ---------------------------------------------------------------------------
// Crosstalk
// ---------------------------------------------------------------------------

/// One row of the crosstalk matrix: member `member` soloed, every channel read.
#[derive(Debug, Clone, Serialize)]
pub struct CrosstalkRow {
    pub member: String,
    /// The member's own channel (the row's diagonal).
    pub channel: String,
    /// Peak SNR of the driven channel during this row's round. A row measured
    /// while the driven channel was itself weak says nothing about leakage, so the
    /// verdict ignores rows below [`MIN_PEAK_SNR_DB`].
    pub driven_snr_db: f64,
    /// Leakage per channel, in dB **relative to the driven channel**, in
    /// [`CrosstalkReport::channels`] order. The diagonal is 0.0 by construction.
    ///
    /// Both terms are peak-over-floor SNRs from the *same* window, so the
    /// microphone's absolute gain and the room's floor cancel — which is what makes
    /// this comparable across rows measured at different levels.
    ///
    /// **The row's dynamic range is bounded by `driven_snr_db`.** Leakage that
    /// falls under the room's noise floor reads as ~0 dB SNR in that channel, so
    /// the most negative value a row can ever report is `-driven_snr_db` — "at
    /// least this clean", not "exactly this clean". That is why
    /// [`TARGET_PEAK_SNR_DB`] (25 dB) has to exceed `|MAX_CROSSTALK_DB|` (20 dB):
    /// otherwise the matrix could not certify its own threshold.
    pub leak_db: Vec<f64>,
    pub reliable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrosstalkPair {
    pub member: String,
    pub channel: String,
    pub leak_db: f64,
}

/// Is the frequency assignment usable, judged from measurement rather than from a
/// table (plan §6.2 requires exactly this).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum CrosstalkVerdict {
    /// Every off-diagonal entry at or below [`MAX_CROSSTALK_DB`], and every
    /// channel responds most strongly to its own member.
    Usable,
    /// Worst leakage between [`MAX_CROSSTALK_DB`] and [`MARGINAL_CROSSTALK_DB`].
    /// Measuring is allowed; reassign if it is cheap.
    Marginal { worst: CrosstalkPair, message: String },
    /// Leakage above [`MARGINAL_CROSSTALK_DB`], or a channel that responds more to
    /// another member than to its own (a mislabelled assignment). Reassign
    /// frequencies or fall back to Stage 1 (plan §6.2) — this is the orchestrator's
    /// decision, so it is reported, not refused.
    Failed { worst: CrosstalkPair, mislabelled: bool, message: String },
    /// Not enough usable rows yet.
    Incomplete { missing: Vec<String>, message: String },
    /// Members share channel labels (the shared click track, plan §2.2), so there
    /// is no per-member frequency assignment to validate.
    NotApplicable { message: String },
}

impl CrosstalkVerdict {
    /// Whether the assignment may be used for a parallel measurement.
    pub fn is_usable(&self) -> bool {
        matches!(self, CrosstalkVerdict::Usable | CrosstalkVerdict::Marginal { .. })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CrosstalkReport {
    /// Column order for every [`CrosstalkRow::leak_db`].
    pub channels: Vec<String>,
    pub rows: Vec<CrosstalkRow>,
    pub verdict: CrosstalkVerdict,
    pub worst: Option<CrosstalkPair>,
}

/// An a-priori conflict in a frequency assignment (plan §6.2's constraints).
/// Complements the measured verdict: this catches a bad *table*, the matrix
/// catches a bad *room*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrequencyConflictKind {
    /// Closer than [`MIN_CHANNEL_SPACING_HZ`].
    Spacing,
    /// Within [`HARMONIC_GUARD_HZ`] of 2× or 3× another channel.
    Harmonic,
    /// Outside [`USABLE_BAND_HZ`].
    OutOfBand,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrequencyConflict {
    pub kind: FrequencyConflictKind,
    pub a_hz: f64,
    pub b_hz: f64,
    pub message: String,
}

/// Check a frequency assignment against plan §6.2's constraints. Empty = clean.
/// Cheap, so run it before spending 2 s per row proving the same thing acoustically.
pub fn audit_frequency_assignment(freqs: &[f64]) -> Vec<FrequencyConflict> {
    let mut out = Vec::new();
    for &f in freqs {
        if f < USABLE_BAND_HZ.0 || f > USABLE_BAND_HZ.1 {
            out.push(FrequencyConflict {
                kind: FrequencyConflictKind::OutOfBand,
                a_hz: f,
                b_hz: f,
                message: format!("{f:.0} Hz is outside the usable {:.0}–{:.0} Hz band", USABLE_BAND_HZ.0, USABLE_BAND_HZ.1),
            });
        }
    }
    for (i, &a) in freqs.iter().enumerate() {
        for &b in &freqs[i + 1..] {
            if (a - b).abs() < MIN_CHANNEL_SPACING_HZ {
                out.push(FrequencyConflict {
                    kind: FrequencyConflictKind::Spacing,
                    a_hz: a,
                    b_hz: b,
                    message: format!("{a:.0} Hz and {b:.0} Hz are closer than {MIN_CHANNEL_SPACING_HZ:.0} Hz"),
                });
            }
        }
    }
    for &a in freqs {
        for &b in freqs {
            for mult in [2.0, 3.0] {
                if (a * mult - b).abs() < HARMONIC_GUARD_HZ {
                    out.push(FrequencyConflict {
                        kind: FrequencyConflictKind::Harmonic,
                        a_hz: a,
                        b_hz: b,
                        message: format!("{mult:.0}x{a:.0} Hz lands within {HARMONIC_GUARD_HZ:.0} Hz of the {b:.0} Hz channel"),
                    });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The solver
// ---------------------------------------------------------------------------

struct MemberState {
    spec: LevelMemberSpec,
    level: u8,
    /// Last level handed out in a [`RampStep`], for the `changed` subset.
    issued: Option<u8>,
    snr_db: Option<f64>,
    /// Level in force when `snr_db` was read (for the taper measurement).
    snr_level: u8,
    second_peak_ratio: Option<f64>,
    taper: f64,
    /// A level was written for this member, so teardown has something to undo.
    touched: bool,
}

impl MemberState {
    fn adjustable(&self) -> bool {
        self.spec.knob().is_adjustable()
    }

    fn report(&self, target: f64) -> MemberLevelReport {
        let advice = self.advice(target);
        MemberLevelReport {
            node_name: self.spec.node_name.clone(),
            channel: self.spec.channel.clone(),
            kind: self.spec.kind,
            knob: self.spec.knob(),
            level: self.level,
            peak_snr_db: self.snr_db,
            margin_db: self.snr_db.map(|s| s - MIN_PEAK_SNR_DB),
            second_peak_ratio: self.second_peak_ratio,
            measured_taper: self.taper,
            reached_target: self.snr_db.map(|s| s >= target).unwrap_or(false),
            advice,
        }
    }

    fn advice(&self, target: f64) -> Option<String> {
        if self.adjustable() {
            return None;
        }
        let snr = self.snr_db?;
        if snr < target {
            Some(format!(
                "{} has no level knob this daemon can reach: it reads {snr:.1} dB peak SNR and needs {target:.0} dB — raise its volume at \
                 the device or on its host, or move the phone closer to it",
                self.spec.node_name
            ))
        } else if snr > target + SURPLUS_TOLERANCE_DB {
            Some(format!(
                "{} has no level knob this daemon can reach and is {:.1} dB hotter than needed; it is eating the microphone's headroom, so \
                 lower it at the device if a later round clips",
                self.spec.node_name,
                snr - target
            ))
        } else {
            None
        }
    }
}

/// Accumulates observations across ramp rounds and decides the next move.
pub struct LevelSolver {
    cfg: LevelConfig,
    members: Vec<MemberState>,
    /// Column order of the crosstalk matrix (distinct channel labels, member order).
    channels: Vec<String>,
    /// Every member has its own channel label, so an all-members round attributes
    /// per member. False under the shared click track (plan §2.2).
    separable: bool,
    rounds: usize,
    /// An all-members round was observed with no clipping and peak within target.
    aggregate_ok: bool,
    aggregate_peak: Option<f32>,
    missing_channel: Option<String>,
    rows: HashMap<String, CrosstalkRow>,
    warnings: Vec<String>,
}

impl LevelSolver {
    pub fn new(members: Vec<LevelMemberSpec>) -> Result<Self, String> {
        Self::with_config(members, LevelConfig::default())
    }

    pub fn with_config(members: Vec<LevelMemberSpec>, cfg: LevelConfig) -> Result<Self, String> {
        if members.is_empty() {
            return Err("level learning needs at least one member".to_string());
        }
        let mut channels: Vec<String> = Vec::new();
        for m in &members {
            if !channels.iter().any(|c| c == &m.channel) {
                channels.push(m.channel.clone());
            }
        }
        let separable = channels.len() == members.len();
        if !separable && cfg.mode == RampMode::Parallel {
            return Err("parallel level learning needs one measurement channel per member (plan §6.2)".to_string());
        }
        let members = members
            .into_iter()
            .map(|spec| MemberState {
                level: if spec.knob().is_adjustable() { cfg.start_level.clamp(MIN_LEVEL, MAX_LEVEL) } else { MAX_LEVEL },
                spec,
                issued: None,
                snr_db: None,
                snr_level: cfg.start_level,
                second_peak_ratio: None,
                taper: 1.0,
                touched: false,
            })
            .collect();
        Ok(Self {
            cfg,
            members,
            channels,
            separable,
            rounds: 0,
            aggregate_ok: false,
            aggregate_peak: None,
            missing_channel: None,
            rows: HashMap::new(),
            warnings: Vec::new(),
        })
    }

    /// Hard cap on observed rounds. Flat in N for [`RampMode::Parallel`] (plus one
    /// solo round per member if the crosstalk pass is on, which is a measurement
    /// pass rather than a ramp); linear in N for [`RampMode::Sequential`], which
    /// can only read one member per round.
    pub fn round_budget(&self) -> usize {
        if let Some(n) = self.cfg.max_rounds {
            return n;
        }
        match self.cfg.mode {
            RampMode::Parallel => MAX_PARALLEL_ROUNDS + if self.cfg.crosstalk_pass { self.members.len() } else { 0 },
            RampMode::Sequential => SEQUENTIAL_ROUNDS_PER_MEMBER * self.members.len() + SEQUENTIAL_ROUNDS_SLACK,
        }
    }

    pub fn rounds_used(&self) -> usize {
        self.rounds
    }

    pub fn levels(&self) -> Vec<MemberLevel> {
        self.members.iter().map(Self::member_level).collect()
    }

    /// Restores for teardown: only members whose level this solve actually wrote.
    /// `align/calibrate.rs` already snapshots sendspin volumes, so the [`LevelKnob::Live`]
    /// entries are belt-and-braces; the [`LevelKnob::SnapshotRestore`] ones are the
    /// reason this exists (plan §7: an AP2 receiver's level — and a pw-sink host's
    /// master level — is device-authoritative outside the session, and the
    /// "no-impose" decision is suspended only for its duration).
    pub fn restore_plan(&self) -> Vec<LevelRestore> {
        self.members
            .iter()
            .filter(|m| m.touched && m.adjustable())
            .map(|m| {
                let note = match (m.spec.knob(), m.spec.snapshot_level) {
                    (LevelKnob::SnapshotRestore, None) => format!(
                        "{}: no pre-session level was known (this level is device-authoritative — the receiver's or its host's own), so \
                         leave it alone rather than inventing one",
                        m.spec.node_name
                    ),
                    (LevelKnob::SnapshotRestore, Some(l)) => {
                        format!("{}: restore the snapshotted level {l} and hand control back to the device", m.spec.node_name)
                    }
                    _ => format!("{}: restore the snapshotted level", m.spec.node_name),
                };
                LevelRestore {
                    node_name: m.spec.node_name.clone(),
                    kind: m.spec.kind,
                    knob: m.spec.knob(),
                    level: m.spec.snapshot_level,
                    note,
                }
            })
            .collect()
    }

    /// The first step: cold-start levels and the first excitation.
    pub fn begin(&mut self) -> RampStep {
        let excite = self.first_excitation();
        self.step(RampPurpose::Probe, excite)
    }

    /// Fold one observed round in and decide what happens next.
    pub fn observe(&mut self, obs: RoundObservation) -> LevelDecision {
        self.rounds += 1;
        self.ingest(&obs);
        self.decide(&obs)
    }

    pub fn crosstalk(&self) -> CrosstalkReport {
        let rows: Vec<CrosstalkRow> = self.members.iter().filter_map(|m| self.rows.get(&m.spec.node_name).cloned()).collect();
        let missing: Vec<String> = self
            .members
            .iter()
            .filter(|m| self.rows.get(&m.spec.node_name).map(|r| !r.reliable).unwrap_or(true))
            .map(|m| m.spec.node_name.clone())
            .collect();

        let mut worst: Option<CrosstalkPair> = None;
        let mut mislabelled = false;
        for row in rows.iter().filter(|r| r.reliable) {
            let own = self.channels.iter().position(|c| c == &row.channel);
            for (j, &leak) in row.leak_db.iter().enumerate() {
                if own == Some(j) {
                    continue;
                }
                // A channel that responds *more* to a foreign member than to its
                // own is not "leaky", it is assigned to the wrong speaker.
                if leak > 0.0 {
                    mislabelled = true;
                }
                if worst.as_ref().map(|w| leak > w.leak_db).unwrap_or(true) {
                    worst = Some(CrosstalkPair { member: row.member.clone(), channel: self.channels[j].clone(), leak_db: leak });
                }
            }
        }

        let verdict = if !self.separable {
            CrosstalkVerdict::NotApplicable {
                message: "members share measurement channels (the shared click track, plan §2.2), so there is no per-member frequency \
                 assignment to validate"
                    .to_string(),
            }
        } else if self.members.len() < 2 {
            CrosstalkVerdict::NotApplicable { message: "a single member cannot cross-talk with anything".to_string() }
        } else if !missing.is_empty() {
            CrosstalkVerdict::Incomplete { message: format!("no usable solo round yet for {}", missing.join(", ")), missing }
        } else {
            match worst.clone() {
                None => CrosstalkVerdict::Usable,
                Some(w) if mislabelled => CrosstalkVerdict::Failed {
                    message: format!(
                        "channel {} responds more strongly to {} than to its own member ({:+.1} dB): the frequency assignment does not \
                         match the speakers",
                        w.channel, w.member, w.leak_db
                    ),
                    worst: w,
                    mislabelled: true,
                },
                Some(w) if w.leak_db > MARGINAL_CROSSTALK_DB => CrosstalkVerdict::Failed {
                    message: format!(
                        "{} leaks {:.1} dB into channel {} (needs {MAX_CROSSTALK_DB:.0} dB or better): reassign the burst frequencies or \
                         fall back to sequential excitation",
                        w.member, w.leak_db, w.channel
                    ),
                    worst: w,
                    mislabelled: false,
                },
                Some(w) if w.leak_db > MAX_CROSSTALK_DB => CrosstalkVerdict::Marginal {
                    message: format!(
                        "{} leaks {:.1} dB into channel {}: usable, but only {:.1} dB clear of the marginal threshold",
                        w.member,
                        w.leak_db,
                        w.channel,
                        MARGINAL_CROSSTALK_DB - w.leak_db
                    ),
                    worst: w,
                },
                Some(_) => CrosstalkVerdict::Usable,
            }
        };
        CrosstalkReport { channels: self.channels.clone(), rows, verdict, worst }
    }

    // -- observation folding ------------------------------------------------

    fn ingest(&mut self, obs: &RoundObservation) {
        if obs.excited == Excitation::All {
            self.aggregate_peak = Some(obs.mic_peak);
            self.aggregate_ok = !obs.clipped && obs.mic_peak <= self.cfg.clip_backoff_peak;
        }
        // Which members' own-channel readings this round can be attributed to.
        let attributable: Vec<usize> = match &obs.excited {
            Excitation::All if self.separable => (0..self.members.len()).collect(),
            Excitation::All => Vec::new(),
            Excitation::Solo { node_name } => self.members.iter().position(|m| &m.spec.node_name == node_name).into_iter().collect(),
        };
        for i in attributable {
            let label = self.members[i].spec.channel.clone();
            let Some(r) = obs.reading(&label) else {
                self.missing_channel = Some(self.members[i].spec.node_name.clone());
                continue;
            };
            let (snr, ratio) = (r.peak_snr_db, r.second_peak_ratio);
            let m = &mut self.members[i];
            // Learn the knob's realised taper from the change the last write made.
            if let Some(prev) = m.snr_db {
                let commanded = level_to_db(m.level) - level_to_db(m.snr_level);
                if commanded.abs() >= MIN_TAPER_COMMAND_DB {
                    let ratio = (snr - prev) / commanded;
                    if ratio.is_finite() && ratio > 0.0 {
                        m.taper = (0.5 * m.taper + 0.5 * ratio).clamp(TAPER_MIN, TAPER_MAX);
                    }
                }
            }
            m.snr_db = Some(snr);
            m.snr_level = m.level;
            m.second_peak_ratio = Some(ratio);
        }
        if let Excitation::Solo { node_name } = &obs.excited {
            self.record_row(node_name.clone(), obs);
        }
    }

    /// A solo round is a crosstalk row for free (plan §7).
    fn record_row(&mut self, member: String, obs: &RoundObservation) {
        if !self.separable {
            return;
        }
        let Some(m) = self.members.iter().find(|m| m.spec.node_name == member) else { return };
        let channel = m.spec.channel.clone();
        let Some(driven) = obs.reading(&channel) else { return };
        let driven_snr_db = driven.peak_snr_db;
        let leak_db = self
            .channels
            .iter()
            .map(|c| {
                if c == &channel {
                    0.0
                } else {
                    // Both terms are peak-over-floor in the same window, so the mic
                    // gain and the room floor cancel out of the difference.
                    obs.reading(c).map(|r| r.peak_snr_db - driven_snr_db).unwrap_or(f64::from(-120))
                }
            })
            .collect();
        let reliable = driven_snr_db >= MIN_PEAK_SNR_DB;
        self.rows.insert(member.clone(), CrosstalkRow { member, channel, driven_snr_db, leak_db, reliable });
    }

    // -- the decision -------------------------------------------------------

    fn decide(&mut self, obs: &RoundObservation) -> LevelDecision {
        if let Some(name) = self.missing_channel.clone() {
            let channel = self.member(&name).map(|m| m.spec.channel.clone()).unwrap_or_default();
            return self.refuse(
                RefusalReason::MissingChannel,
                name.clone(),
                None,
                format!(
                    "member {name} is assigned measurement channel \"{channel}\", which the estimator did not report — the channel \
                 assignment and the estimator config disagree"
                ),
            );
        }

        let target = self.cfg.target_snr_db;

        // A member with no knob and not enough level is unfixable from here.
        if let Some(i) = self.members.iter().position(|m| !m.adjustable() && m.snr_db.map(|s| s < target).unwrap_or(false)) {
            let m = &self.members[i];
            let snr = m.snr_db.unwrap_or_default();
            let name = m.spec.node_name.clone();
            return self.refuse(
                RefusalReason::NoLevelKnob,
                name.clone(),
                None,
                format!(
                    "{name} reads {snr:.1} dB peak SNR and needs {target:.0} dB, but it has no level knob this daemon can reach: raise its \
                     volume at the device or on its host, or move the phone closer to it, then start again"
                ),
            );
        }

        // --- the loud side ---------------------------------------------------
        let clipping = obs.clipped || obs.mic_peak >= self.cfg.clip_backoff_peak;
        if clipping {
            if let Some(d) = self.back_off_from_clipping(obs) {
                return d;
            }
        }

        // --- the quiet side --------------------------------------------------
        // Aggregate headroom bounds how much *more* level the sum can take. Only an
        // all-members round measures it; until one has been observed the levels are
        // still near the cold start, so treating it as unbounded is safe.
        let headroom_db = match self.aggregate_peak {
            Some(p) => peak_to_dbfs(self.cfg.clip_target_peak) - peak_to_dbfs(p),
            None => f64::INFINITY,
        };

        let deficient: Vec<usize> =
            self.members.iter().enumerate().filter(|(_, m)| m.snr_db.map(|s| s < target).unwrap_or(false)).map(|(i, _)| i).collect();

        if !deficient.is_empty() && headroom_db < MIN_STEP_DB {
            // Cannot raise anyone without clipping. Trim a surplus member first;
            // if nobody has surplus to give, the constraint is genuinely two-sided
            // and unsatisfiable — refuse and name both roles.
            if let Some(step) = self.trim_surplus(RampPurpose::ClipBackoff) {
                return LevelDecision::Continue(step);
            }
            let quiet = self.members[deficient[0]].spec.node_name.clone();
            let (loud, loud_snr) = self.loudest();
            let quiet_snr = self.members[deficient[0]].snr_db.unwrap_or_default();
            return self.refuse(
                RefusalReason::ClipCeiling,
                quiet.clone(),
                Some(loud.clone()),
                format!(
                    "{quiet} only reaches {quiet_snr:.1} dB peak SNR (needs {target:.0} dB) and the microphone is already at {:.0}% of \
                     full scale, mostly from {loud} at {loud_snr:.1} dB — no level split satisfies both. Move the phone to a spot with a \
                     less extreme near/far difference, or turn {loud} down at the device.",
                    self.aggregate_peak.unwrap_or(1.0) * 100.0
                ),
            );
        }

        let mut moved = false;
        for i in 0..self.members.len() {
            let Some(snr) = self.members[i].snr_db else { continue };
            if !self.members[i].adjustable() {
                continue;
            }
            let deficit = target - snr;
            let surplus = snr - (target + self.cfg.surplus_tolerance_db);
            if deficit > 0.0 {
                if self.members[i].level >= MAX_LEVEL {
                    let name = self.members[i].spec.node_name.clone();
                    return self.refuse(
                        RefusalReason::TooQuietAtMaxLevel,
                        name.clone(),
                        None,
                        format!(
                            "{name} is at the maximum playback level and still only reaches {snr:.1} dB peak SNR, {:.1} dB short of the \
                             {target:.0} dB the estimator needs: move the phone closer to it, or raise the receiver's own volume",
                            deficit
                        ),
                    );
                }
                let raise = (deficit + RAISE_OVERSHOOT_DB).min(MAX_STEP_DB).min(headroom_db.max(0.0));
                if self.raise(i, raise) {
                    moved = true;
                }
            } else if surplus > 0.0 && self.members[i].level > MIN_LEVEL {
                let cut = surplus.min(MAX_STEP_DB);
                if cut >= MIN_STEP_DB && self.set_level(i, shift_level(self.members[i].level, -cut, self.members[i].taper)) {
                    moved = true;
                }
            }
        }
        if moved {
            let excite = self.next_excitation();
            return self.continue_or_bound(RampPurpose::Ramp, excite);
        }

        // Nothing to move. Anything still unread?
        if let Some(i) = self.members.iter().position(|m| m.snr_db.is_none()) {
            let name = self.members[i].spec.node_name.clone();
            return self.continue_or_bound(RampPurpose::Probe, Excitation::Solo { node_name: name });
        }

        // Every channel is in band. Confirm the sum, then fill the matrix.
        if !self.aggregate_ok {
            return self.continue_or_bound(RampPurpose::AggregateCheck, Excitation::All);
        }
        if self.cfg.crosstalk_pass && self.separable && self.members.len() > 1 {
            if let Some(name) = self.next_crosstalk_row() {
                return self.continue_or_bound(RampPurpose::Crosstalk, Excitation::Solo { node_name: name });
            }
        }
        self.converge()
    }

    /// Cut whatever can be cut in response to clipping. `None` when no cut is
    /// possible, which is the caller's cue to refuse.
    fn back_off_from_clipping(&mut self, obs: &RoundObservation) -> Option<LevelDecision> {
        let target = self.cfg.target_snr_db;
        let excited: Vec<usize> = match &obs.excited {
            Excitation::All => (0..self.members.len()).collect(),
            Excitation::Solo { node_name } => self.members.iter().position(|m| &m.spec.node_name == node_name).into_iter().collect(),
        };
        let mut cut_any = false;
        for &i in &excited {
            let m = &self.members[i];
            if !m.adjustable() || m.level <= MIN_LEVEL {
                continue;
            }
            // Only members with real surplus are cuttable: cutting a member that is
            // already near the target just trades one channel's failure for
            // another's, and the loop would never terminate.
            let cut = match m.snr_db {
                None => MAX_STEP_DB.min(6.0),
                Some(s) if s - target >= CLIP_CUT_MIN_DB => (s - target).min(MAX_STEP_DB),
                Some(_) => continue,
            };
            let next = shift_level(m.level, -cut, m.taper);
            if self.set_level(i, next) {
                cut_any = true;
            }
        }
        if cut_any {
            let excite = obs.excited.clone();
            return Some(self.continue_or_bound(RampPurpose::ClipBackoff, excite));
        }

        // Nothing cuttable. Who is setting the ceiling?
        let (loud, loud_snr) = self.loudest();
        let loud_state = self.member(&loud)?;
        let fixed = !loud_state.adjustable();
        let at_floor = loud_state.level <= MIN_LEVEL;
        let peak_pct = obs.mic_peak * 100.0;
        if fixed {
            return Some(self.refuse(
                RefusalReason::FixedMemberClips,
                loud.clone(),
                Some(loud.clone()),
                format!(
                    "{loud} drives the microphone to {peak_pct:.0}% of full scale ({loud_snr:.1} dB peak SNR) and has no level knob this \
                     daemon can reach, so turning the other members down cannot help: lower it at the device or on its host, or move the \
                     phone away from it"
                ),
            ));
        }
        let quiet = self
            .members
            .iter()
            .filter(|m| m.snr_db.map(|s| s < target).unwrap_or(false))
            .min_by(|a, b| a.snr_db.unwrap_or_default().total_cmp(&b.snr_db.unwrap_or_default()))
            .map(|m| m.spec.node_name.clone());
        let message = if at_floor {
            format!(
                "{loud} is already at the minimum usable level and still drives the microphone to {peak_pct:.0}% of full scale: move the \
                 phone away from it"
            )
        } else {
            format!(
                "the microphone clips at {peak_pct:.0}% of full scale before every channel reaches {target:.0} dB peak SNR, and no member \
                 has level to spare: {loud} sets the ceiling at {loud_snr:.1} dB"
            )
        };
        Some(self.refuse(RefusalReason::ClipCeiling, quiet.unwrap_or_else(|| loud.clone()), Some(loud), message))
    }

    /// Trim every member with surplus, for the case where the aggregate is out of
    /// headroom but nothing has clipped yet.
    fn trim_surplus(&mut self, purpose: RampPurpose) -> Option<RampStep> {
        let target = self.cfg.target_snr_db;
        let mut moved = false;
        for i in 0..self.members.len() {
            let m = &self.members[i];
            if !m.adjustable() || m.level <= MIN_LEVEL {
                continue;
            }
            let Some(s) = m.snr_db else { continue };
            if s - target < CLIP_CUT_MIN_DB {
                continue;
            }
            let cut = (s - target).min(MAX_STEP_DB);
            let next = shift_level(m.level, -cut, m.taper);
            if self.set_level(i, next) {
                moved = true;
            }
        }
        if !moved {
            return None;
        }
        let excite = self.next_excitation();
        Some(self.step(purpose, excite))
    }

    fn raise(&mut self, i: usize, raise_db: f64) -> bool {
        if raise_db < MIN_STEP_DB {
            return false;
        }
        let m = &self.members[i];
        let mut next = shift_level(m.level, raise_db, m.taper);
        // The knob is 0–100 integers, so a small raise at a low level can round to
        // nothing. Nudge by one step so a round always makes progress and the round
        // bound is a real bound rather than a livelock.
        if next == m.level && m.level < MAX_LEVEL {
            next = m.level + 1;
        }
        self.set_level(i, next)
    }

    fn set_level(&mut self, i: usize, level: u8) -> bool {
        let level = level.clamp(MIN_LEVEL, MAX_LEVEL);
        let m = &mut self.members[i];
        if m.level == level {
            return false;
        }
        m.level = level;
        m.touched = true;
        true
    }

    fn loudest(&self) -> (String, f64) {
        self.members
            .iter()
            .filter(|m| m.snr_db.is_some())
            .max_by(|a, b| a.snr_db.unwrap_or_default().total_cmp(&b.snr_db.unwrap_or_default()))
            .map(|m| (m.spec.node_name.clone(), m.snr_db.unwrap_or_default()))
            .unwrap_or_else(|| (self.members[0].spec.node_name.clone(), f64::NAN))
    }

    fn member(&self, node_name: &str) -> Option<&MemberState> {
        self.members.iter().find(|m| m.spec.node_name == node_name)
    }

    fn next_crosstalk_row(&self) -> Option<String> {
        self.members
            .iter()
            .find(|m| self.rows.get(&m.spec.node_name).map(|r| !r.reliable).unwrap_or(true))
            .map(|m| m.spec.node_name.clone())
    }

    fn first_excitation(&self) -> Excitation {
        match self.cfg.mode {
            RampMode::Parallel => Excitation::All,
            RampMode::Sequential => Excitation::Solo { node_name: self.members[0].spec.node_name.clone() },
        }
    }

    /// In parallel mode every member ramps together. In sequential mode pick the
    /// member that needs the most attention: never-read first, then the largest
    /// distance from the target.
    fn next_excitation(&self) -> Excitation {
        if self.cfg.mode == RampMode::Parallel {
            return Excitation::All;
        }
        let target = self.cfg.target_snr_db;
        if let Some(m) = self.members.iter().find(|m| m.snr_db.is_none()) {
            return Excitation::Solo { node_name: m.spec.node_name.clone() };
        }
        let worst = self
            .members
            .iter()
            .filter(|m| m.adjustable())
            .max_by(|a, b| {
                let d = |m: &MemberState| (target - m.snr_db.unwrap_or_default()).abs();
                d(a).total_cmp(&d(b))
            })
            .unwrap_or(&self.members[0]);
        Excitation::Solo { node_name: worst.spec.node_name.clone() }
    }

    fn member_level(m: &MemberState) -> MemberLevel {
        MemberLevel { node_name: m.spec.node_name.clone(), level: m.level, kind: m.spec.kind, knob: m.spec.knob() }
    }

    fn step(&mut self, purpose: RampPurpose, excite: Excitation) -> RampStep {
        let mut changed = Vec::new();
        for m in &mut self.members {
            if !m.spec.knob().is_adjustable() {
                continue;
            }
            if m.issued != Some(m.level) {
                m.issued = Some(m.level);
                m.touched = true;
                changed.push(MemberLevel { node_name: m.spec.node_name.clone(), level: m.level, kind: m.spec.kind, knob: m.spec.knob() });
            }
        }
        RampStep { round: self.rounds + 1, round_budget: self.round_budget(), purpose, excite, levels: self.levels(), changed }
    }

    fn continue_or_bound(&mut self, purpose: RampPurpose, excite: Excitation) -> LevelDecision {
        if self.rounds >= self.round_budget() {
            let target = self.cfg.target_snr_db;
            let worst = self
                .members
                .iter()
                .filter(|m| m.snr_db.map(|s| s < target).unwrap_or(true))
                .min_by(|a, b| a.snr_db.unwrap_or(f64::NEG_INFINITY).total_cmp(&b.snr_db.unwrap_or(f64::NEG_INFINITY)))
                .map(|m| (m.spec.node_name.clone(), m.snr_db))
                .unwrap_or_else(|| self.loudest_name_snr());
            let (name, snr) = worst;
            let snr_text = snr.map(|s| format!("{s:.1} dB")).unwrap_or_else(|| "no reading".to_string());
            let budget = self.round_budget();
            return self.refuse(
                RefusalReason::RoundBoundExhausted,
                name.clone(),
                None,
                format!(
                    "the playback level did not settle within {budget} rounds; {name} is still at {snr_text} peak SNR against a \
                     {target:.0} dB target. The room or the phone's position is changing faster than the ramp can follow — hold the phone \
                     still and try again"
                ),
            );
        }
        LevelDecision::Continue(self.step(purpose, excite))
    }

    fn loudest_name_snr(&self) -> (String, Option<f64>) {
        let (n, s) = self.loudest();
        (n, if s.is_nan() { None } else { Some(s) })
    }

    fn converge(&mut self) -> LevelDecision {
        let crosstalk = self.crosstalk();
        let mut warnings = self.warnings.clone();
        match &crosstalk.verdict {
            CrosstalkVerdict::Marginal { message, .. } | CrosstalkVerdict::Failed { message, .. } => warnings.push(message.clone()),
            _ => {}
        }
        for m in &self.members {
            if let Some(a) = m.advice(self.cfg.target_snr_db) {
                warnings.push(a);
            }
        }
        LevelDecision::Converged(Box::new(LevelPlan {
            levels: self.levels(),
            members: self.members.iter().map(|m| m.report(self.cfg.target_snr_db)).collect(),
            rounds_used: self.rounds,
            target_snr_db: self.cfg.target_snr_db,
            aggregate_peak: self.aggregate_peak,
            crosstalk,
            warnings,
        }))
    }

    fn refuse(&self, reason: RefusalReason, member: String, blocking_member: Option<String>, message: String) -> LevelDecision {
        LevelDecision::Refused(Box::new(LevelRefusal {
            reason,
            member,
            blocking_member,
            message,
            members: self.members.iter().map(|m| m.report(self.cfg.target_snr_db)).collect(),
            rounds_used: self.rounds,
            crosstalk: self.crosstalk(),
        }))
    }
}

#[cfg(test)]
mod tests {
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
                    assert!(
                        next.round <= next.round_budget + 1,
                        "step {n} advertised round {} over budget {}",
                        next.round,
                        next.round_budget
                    );
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
        let room =
            Room::new(&[("sendspin-dev-a", "ch_a", -30.0), ("sendspin-dev-b", "ch_b", -30.0)]).with_leak("sendspin-dev-a", "ch_b", -6.0);
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
        let room =
            Room::new(&[("sendspin-dev-a", "ch_a", -30.0), ("sendspin-dev-b", "ch_b", -30.0)]).with_leak("sendspin-dev-a", "ch_b", 3.0);
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
        let mut solver = LevelSolver::new(specs(&[
            ("sendspin-dev-a", "a", LevelMemberKind::Sendspin),
            ("sendspin-dev-b", "b", LevelMemberKind::Sendspin),
        ]))
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
        assert!(
            matches!(r.reason, RefusalReason::RoundBoundExhausted | RefusalReason::TooQuietAtMaxLevel),
            "{:?}: {}",
            r.reason,
            r.message
        );
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
}
