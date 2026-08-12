//! Measurement orchestration for microphone-assisted alignment
//! (docs/mic-alignment-plan.md §8).
//!
//! Drives the state machine that turns a microphone capture into a set of
//! per-member delay corrections: arm → learn → measure → solve → write → settle →
//! verify. Owns the binding between an alignment session (`align/calibrate.rs`) and the
//! mic ingest (`align/mic.rs`), and feeds the captured audio to the estimator
//! (`align/estimator.rs`).
//!
//! Every transition into a measuring state passes through one gate: **re-acquire
//! loop-phase lock with stable amplitude before accepting a window**. That single
//! mechanism covers mute settling, device reconnects, the user moving, and the mic
//! socket reconnecting — see plan §8.
//!
//! ## Why one grid, and what breaks it
//!
//! Stage 1 excitation (plan §6.1) solos one member at a time, so each member is
//! measured in its *own* window — yet the phases have to be comparable across
//! windows. They are, for one reason: the estimator reduces every arrival to
//! `mic_frame_index mod pattern_period`, and the anchor keeps looping the same 2 s
//! WAV throughout, so the grid origin is one unknown constant shared by every
//! member and every pass (`align_estimator`'s module docs spell this out). It
//! cancels in every difference, which is all this module consumes.
//!
//! Exactly one thing invalidates it: the **mic socket reconnecting**, because
//! `align_mic` restarts its frame counter at 0 and the relationship between frame
//! 0 and the content loop is then a *different* unknown constant. Observations
//! therefore carry a `grid_epoch`; a reconnect bumps it and every observation from
//! the previous epoch is discarded rather than silently compared across the seam.
//!
//! ## Two ways arrivals are acquired (plan §1)
//!
//! The estimator, the gate, the §2.4.2 interval solver and the `Proposed` → apply →
//! verify sequence are shared. What differs is *where the phone is* while each
//! member is measured, and therefore what the numbers mean:
//!
//! * **Multi-position** ([`Mode::SweetSpot`], the default): the phone sits at a
//!   listening position and the run walks the member list itself, twice. Each
//!   arrival is electrical delay **+** the propagation path to that spot, so the
//!   result aligns *that spot*. With [`MeasureDeps::chained`] it becomes a
//!   **chain**: the user aligns one locally-audible set, repositions, and aligns
//!   the next through shared **overlap** speakers ([`run_chain`], plan §1.1). A
//!   plain single-position run is the one-step case of the same thing.
//! * **Near field** ([`Mode::NearField`], W8a): the **user** walks to each speaker
//!   and holds the phone at it, one speaker at a time, and tells the daemon so
//!   ([`MeasureManager::arrival`]). At arm's length the path term collapses below
//!   ~1 ms, so what is left is the *wire*, which is correct everywhere rather than
//!   at one spot. See [`run_walk`] for the orchestration and
//!   [`closure_report`] for the drift measurement that makes a minutes-long walk
//!   usable.
//!
//! ## What is honestly measured, and what is not
//!
//! * **Measured:** each member's arrival time at the microphone, i.e. electrical
//!   delay **+** acoustic propagation (plan §1). A mic in one place aligns *that
//!   place*; near field aligns the wire — **provided the user really does hold the
//!   phone at each speaker.** A phone held a metre away instead reintroduces ~3 ms
//!   of path error on that member, and **nothing in the measurement can detect
//!   that**: it looks like a perfectly good reading of a slightly different
//!   speaker.
//! * **Not knowable from here:** an early reflection inside the analysis window
//!   biases a speaker by up to a couple of milliseconds while every quality metric
//!   says "excellent" (plan §5.6). [`transitivity`] is the only cross-check in this
//!   design that can expose it, and even that has a confound documented on the
//!   function itself. Near field *reduces* this — at arm's length the direct sound
//!   dominates a reflection by far more than the 0.9× that produced §5.6's numbers
//!   — but it does not remove it, and no check here can say which happened.
//! * **Not knowable at all with this signal:** an arrival spread wider than half a
//!   pattern period (±1 s). It wraps, and nothing in the capture distinguishes a
//!   wrap from a small offset. [`MAX_TRUSTED_SPREAD_MS`] refuses near that edge
//!   instead of pretending.
//!
//! ## The knobs point in different directions (plan §2.4.1, §2.4.2)
//!
//! There is **no reference member**. A sendspin `static_delay_ms` is an *advance* —
//! the device subtracts it from the target instant and plays that much earlier —
//! while an AP2 render delay and a pw-sink playout delay are true delays. So each
//! member reaches a bounded *interval* of arrivals ([`MemberInterval`]), the group
//! can only be aligned at a target inside all of them, and [`choose_target`] picks
//! the one that keeps the largest knob smallest. Two consequences worth carrying in
//! your head while reading [`solve`]:
//!
//! * a sendspin-only group is aligned to its **earliest** member, not its latest —
//!   the exact inverse of what plan §9.1 says, and less latency rather than more;
//! * a mixed group can be genuinely **unalignable**, because an advance-only member
//!   cannot be pushed later and a delay-only member cannot be pulled earlier, and no
//!   knob moves either member's intrinsic arrival. That is a refusal, not a
//!   best-effort write.
//!
//! ## Stage
//!
//! Sequential solo-alternation only (plan §6.1). Members are measured one at a
//! time, and the pass order **alternates** — forwards, then backwards — so that a
//! mic-vs-audio clock drift averages out across members instead of accumulating
//! down the member list. The parallel, frequency-division excitation of plan §6.2
//! is W7 and nothing here assumes it: the channel set is the existing click
//! track's two *frequency* channels (A at 3 kHz, B at 1.5 kHz), which every
//! soloed member emits (plan §2.2).

use crate::align::calibrate::MemberKind;
use crate::align::estimator::{
    Estimator, EstimatorConfig, Quality, RejectReason, CLICK_A_LABEL, CLICK_B_LABEL, MIN_PEAK_SNR_DB, MIN_PERIODS_USED,
};
use crate::align::levels::TARGET_PEAK_SNR_DB;
use crate::align::mic::{MicStatus, MicWindow};
use crate::util::locks::LockRecover;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

// ---------------------------------------------------------------- tunables

/// How often the run loop pulls new audio and re-checks its bindings. Well under
/// the 2 s pattern period, so a period boundary is never missed by more than one
/// poll, and cheap: a pull is a memcpy plus one pass of the estimator's filters.
const POLL: Duration = Duration::from_millis(250);

/// The pattern the anchor is looping: `calibrate::click_wav`'s 2 s A/B loop.
const PATTERN_MS: f64 = crate::align::estimator::PATTERN_SECS * 1000.0;

/// Frames handed to the estimator in one push. ~100 ms at either capture rate:
/// large enough that the per-block oscillator re-anchoring is negligible, small
/// enough that a period boundary lands inside a pull rather than being skipped.
const CHUNK_FRAMES: usize = 4_800;

/// Guard between soloing a member and starting to accept audio (plan §6.1).
/// The protocol mute is *not* sample-precise — it lands somewhere inside the
/// stream's send-ahead window, which is itself a per-group high-water mark that
/// can be several hundred milliseconds — so one pattern period (2 s) plus a
/// second of send-ahead slack is the floor. Anything the guard misses is caught
/// by the gate, which is why the guard can be a constant rather than a
/// negotiation.
const MUTE_GUARD: Duration = Duration::from_millis(3_000);

/// Complete pattern periods the gate must accumulate before it will lock, and the
/// window the estimate is then taken over. [`MIN_PERIODS_USED`] (3) is the
/// estimator's own floor — below it the line fit has no residual and therefore no
/// standard error — and one more buys a period of margin against a single dropped
/// period without doubling the cost. 4 periods = 8 s per member per pass, which
/// is the plan's §6.1 budget.
const GATE_MIN_PERIODS: usize = MIN_PERIODS_USED + 1;

/// Peak-amplitude spread tolerated across the gate's window, in dB.
///
/// This is the "stable amplitude" half of the lock. 3 dB is deliberately loose:
/// it must not trip on a hand-held phone's normal wobble (a 15 % distance change
/// at arm's length is ~1.2 dB), while still catching the two failures that matter
/// — the previous member's mute not having settled yet (the other speaker's
/// contribution appearing/disappearing is many dB) and an AGC/AEC gain ramp.
const GATE_AMP_TOL_DB: f64 = 3.0;

/// A *monotonic* decay of this much across the gate's window is treated as the
/// behavioural signature of echo cancellation converging (plan §4.2): AEC is
/// designed to remove loudspeaker sound from a mic signal and it adapts over
/// seconds, so the burst gets quieter every period. Lower than
/// [`GATE_AMP_TOL_DB`] because a *consistent direction* is far more diagnostic
/// than the same spread arriving as jitter.
const GATE_AEC_DECAY_DB: f64 = 1.5;

/// Below this peak sample value the capture is treated as silent rather than
/// unstable, so the failure says "no tone reached the microphone" (a muted or
/// disconnected speaker) instead of "amplitude unstable". −60 dBFS.
const GATE_SILENCE_PEAK: f32 = 0.001;

/// Gate timeout when only a mute has to settle. Generous next to
/// [`MUTE_GUARD`] + 4 periods (11 s) so a couple of restarts still fit.
const GATE_TIMEOUT_SETTLE: Duration = Duration::from_secs(45);

/// Gate timeout after a delay write. A reconnecting sendspin device takes **tens
/// of seconds** to render again regardless of how the previous session ended
/// (plan §2.3, hardware-confirmed), and a write wave reconnects several of them,
/// so this is minutes, not seconds. Getting this wrong is the difference between
/// "verification failed" and "verification never had a chance".
const GATE_TIMEOUT_RECONNECT: Duration = Duration::from_secs(180);

/// Passes over the member list during the measurement stage. Two is the minimum
/// that gives a *common* drift fit (a single pass has no time baseline) and a
/// repeatability check, which together are what make the drift correction and the
/// "the user moved" detection honest rather than assumed.
const MEASURE_PASSES: usize = 2;

/// Passes during verification. One: the residual only has to confirm what the
/// solve already predicted, and every extra pass costs another reconnect-length
/// gate per member.
const VERIFY_PASSES: usize = 1;

/// How many times a mic reconnect (or an equivalent grid-invalidating event) may
/// restart the whole measurement set before it is refused. One retry is worth
/// having; a second means the phone is not staying connected and the user needs to
/// hear that.
const MAX_SET_RESTARTS: u32 = 1;

/// How long a near-field walk waits at one step for the user to say "I am at this
/// speaker now" ([`MeasureManager::arrival`]) before giving up.
///
/// Long enough to climb a flight of stairs with a phone in one hand, and
/// deliberately **well inside** `calibrate::SESSION_TIMEOUT` (15 minutes from the
/// `start`/rescope that armed it), which is the walk's real budget: the alignment
/// session's own watchdog tears the group down at that point whatever this says.
/// See [`run_walk`] for what that means for a very long walk.
const WALK_ARRIVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Times a near-field walk may be restarted from its first speaker after the
/// capture reconnected (plan §1.2: a walk is *one* capture, so a reconnect voids
/// everything measured so far).
///
/// Higher than [`MAX_SET_RESTARTS`] because each restart costs the **user** a walk
/// rather than the daemon a loop, and because every restart needs a deliberate
/// `arrival` call — there is no runaway here. A third restart means the phone is not
/// staying connected, and being told that is better than walking the house again.
const MAX_WALK_RESTARTS: u32 = 2;

/// Largest mic-vs-audio clock drift the closure measurement will accept as an
/// explanation, in ppm — the plausibility bound behind
/// [`ClosureReport::tolerance_ms`].
///
/// Two independent consumer crystals are each specified to a few tens of ppm and
/// measure at up to ~100 ppm in practice (plan §5.4.1 exercises exactly 100 ppm as
/// the realistic phone case), so 200 ppm is the pessimistic end of real hardware
/// rather than a guess. Above it the difference between the walk's two readings of
/// the same speaker is far more likely to be something that *moved* than something
/// that *drifted*, and since the correction is applied to every member the honest
/// answer is to refuse the whole walk.
pub const MAX_CLOSURE_DRIFT_PPM: f64 = 200.0;

/// Floor under [`ClosureReport::tolerance_ms`], in ms.
///
/// A short walk earns almost no drift allowance from the ppm bound above, and
/// refusing it for a fraction of a millisecond would be theatre: a hand-held phone's
/// own position at the anchor is worth ~3 ms of path (§1's 3 ms per metre) and
/// nothing in the measurement can see it. So the closure check never claims to
/// resolve better than that.
pub const MIN_CLOSURE_TOL_MS: f64 = 3.0;

/// Wait after the write wave before verification starts. Not the settling
/// *mechanism* — that is the per-member gate with [`GATE_TIMEOUT_RECONNECT`] —
/// just enough that the first gate does not spend its window watching a device
/// that has not even dropped its connection yet.
const SETTLE_GRACE: Duration = Duration::from_secs(8);

/// Widest member-to-member arrival spread this signal can measure, as a fraction
/// of the pattern period.
///
/// Phases live on the pattern and differences wrap at ±½ period, so a spread near
/// half a period is indistinguishable from its own wrap. 0.4 (800 ms of the 2 s
/// click track) keeps a fifth of a period of margin, and comfortably covers the
/// largest real case the plan cites (an AP2 member carrying an 800 ms render
/// delay is *already* the thing being corrected — its own delay is part of
/// `current_delays`, not of the spread, see [`solve`]).
pub const MAX_TRUSTED_SPREAD_FRACTION: f64 = 0.4;

/// Tolerance for the cross-band transitivity check, in ms. See [`transitivity`]
/// for why it cannot be tightened to the estimator's own precision (~0.05 ms):
/// a loudspeaker's crossover genuinely delays 1.5 kHz and 3 kHz differently, and
/// that legitimate difference is indistinguishable from the reflection bias this
/// check is hunting.
pub const TRANSITIVITY_TOL_MS: f64 = 3.0;

/// Tolerance for pass-to-pass repeatability, in ms. The delay knobs are integer
/// milliseconds (plan §2.4), so a member whose measured arrival moves by more
/// than 1 ms between passes cannot inform the write.
pub const REPEATABILITY_TOL_MS: f64 = 1.0;

/// Tolerance for the post-write residual, in ms. One millisecond of knob
/// granularity plus rounding, plus a little for the measurement itself.
pub const RESIDUAL_TOL_MS: f64 = 2.0;

/// How far a chain step's **two overlaps** may disagree at the new position before
/// the step is refused, in ms (plan §1.1).
///
/// **This is a plausibility bound, not a precision one, and the difference is the
/// whole point.** Two overlaps will *not* read the same at a new position, and
/// nothing is wrong when they do not: the previous step aligned them at the
/// *previous* position, so what is left here is how their relative geometry changed
/// between the two spots — `(path₁(P2) − path₁(P1)) − (path₂(P2) − path₂(P1))` — which
/// is real, and which nothing in the capture can separate from a measurement error.
///
/// 8 ms is ~2.7 m of that change at §1's 3 ms per metre, which covers two speakers in
/// one room read from an adjoining position. What it *does* catch is the class of
/// failure that would poison the chain, because the shift a step derives from its
/// overlaps is applied to **every** speaker aligned so far: an overlap that was never
/// really aligned, a phase that wrapped, a reflection the estimator locked onto (§5.6
/// measured +5.2 ms and called it excellent), a speaker that has been moved between
/// positions. Those are 5 ms to hundreds of ms, not a couple.
///
/// What it cannot catch is the 1–2 ms per-speaker bias §5.6 describes — the same blind
/// spot [`TRANSITIVITY_TOL_MS`] has and for the same reason. So half the measured
/// disagreement is *reported* as that joint's contribution to the chain's error
/// ([`ChainError`]) rather than being treated as zero.
pub const OVERLAP_AGREEMENT_TOL_MS: f64 = 8.0;

/// Times one position of a chain may be measured again after the capture reconnected
/// (plan §1.2).
///
/// Higher than [`MAX_SET_RESTARTS`] for the same reason [`MAX_WALK_RESTARTS`] is: a
/// reconnect here costs the **user** a position, not the daemon a loop. It voids the
/// position being measured and nothing else — see [`run_chain`] for why the earlier
/// positions survive a new frame and what makes that safe rather than convenient.
const MAX_CHAIN_STEP_RESTARTS: u32 = 2;

/// How long a provisional delay line is given to fill before the position that has to
/// be measured *through* it is measured.
///
/// `relay_delay`'s docs make this a precondition rather than a detail: an un-primed
/// line emits silence, and a reading taken through one is a dropout the gate would
/// diagnose as something else. Filling costs exactly the delay itself in audio, so this
/// is generous — and if it expires, the useful conclusion is that no audio is reaching
/// that output at all.
const PROVISIONAL_PRIME_TIMEOUT: Duration = Duration::from_secs(20);

/// Every wall-clock quantity the run loop depends on, in one place.
///
/// Real runs use [`Timing::real`], which is the constants above. It is a struct
/// rather than a set of constants for one reason: the unit tests drive the *whole*
/// state machine (estimator, gate, drift fit and solve all real, only the
/// transport faked), and at the real pattern period a single run takes minutes.
///
/// The end-to-end tests therefore run on `tokio`'s **paused** clock
/// (`#[tokio::test(start_paused = true)]`, via the `test-util` dev-dependency) with
/// `Timing::real` — the production constants, no shrinking, and the whole suite
/// still finishes in seconds. Keeping the quantities in a struct is what let the
/// shorter-pattern variants exist for the focused tests where a 2 s grid would only
/// add noise; it also removed two hard-coded 1 s / 800 ms assumptions from the
/// solver, since everything that used to assume "2 s" now derives from `pattern_ms`.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The loop the anchor is playing. Fixes the analysis grid, the burst spacing
    /// and the wrap-around limit.
    pub pattern_ms: f64,
    pub mute_guard: Duration,
    pub poll: Duration,
    pub settle_grace: Duration,
    pub gate_settle_timeout: Duration,
    pub gate_reconnect_timeout: Duration,
    /// How long a near-field walk waits for the user at one step.
    pub walk_arrival_timeout: Duration,
}

impl Timing {
    pub fn real() -> Self {
        Self {
            pattern_ms: PATTERN_MS,
            mute_guard: MUTE_GUARD,
            poll: POLL,
            settle_grace: SETTLE_GRACE,
            gate_settle_timeout: GATE_TIMEOUT_SETTLE,
            gate_reconnect_timeout: GATE_TIMEOUT_RECONNECT,
            walk_arrival_timeout: WALK_ARRIVAL_TIMEOUT,
        }
    }

    /// Where B sits inside the pattern: the click track puts its second burst at
    /// the half point (`calibrate::click_wav`).
    fn nominal_ab_ms(&self) -> f64 {
        self.pattern_ms / 2.0
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::real()
    }
}

/// Largest sendspin advance the API accepts (`api.rs`'s `delay_ms.min(5000)`,
/// plan §2.4). There is no named constant on the write path to borrow, so this
/// mirrors it; the two must move together.
pub const SENDSPIN_ADVANCE_MAX_MS: u16 = 5_000;

/// Which way a member's knob moves its arrival (plan §2.4.1).
///
/// This is the part that was wrong before W14. A sendspin device **subtracts**
/// `static_delay_ms` from the target playback instant (`sendspin-cpp`'s
/// `sync_task.cpp:593`; our own `required_send_ahead_us` adds it to the group lead
/// for exactly that reason), so raising it makes the speaker play **earlier**. An
/// AP2 render delay and a pw-sink playout delay both make it play **later**. A
/// mixed group therefore holds knobs of both signs at once, which is why the
/// solver intersects achievable-arrival intervals instead of picking a reference
/// member (plan §2.4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobPolarity {
    /// Raising the knob moves the arrival **earlier** (sendspin `static_delay_ms`).
    Advance,
    /// Raising the knob moves the arrival **later** (AP2 render delay, pw-sink
    /// playout delay).
    Delay,
}

impl KnobPolarity {
    /// The noun to put in front of the number in the UI. A user who is shown
    /// "delay 12 ms" for a knob that actually advances the speaker has been told
    /// the opposite of the truth, which is the whole point of §2.4.1.
    fn noun(self) -> &'static str {
        match self {
            KnobPolarity::Advance => "advance",
            KnobPolarity::Delay => "delay",
        }
    }

    /// Which way raising the knob moves the sound.
    fn direction(self) -> &'static str {
        match self {
            KnobPolarity::Advance => "earlier",
            KnobPolarity::Delay => "later",
        }
    }

    fn opposite(self) -> Self {
        match self {
            KnobPolarity::Advance => KnobPolarity::Delay,
            KnobPolarity::Delay => KnobPolarity::Advance,
        }
    }
}

/// One member kind's knob: a polarity plus a range — plan §2.4.2's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Knob {
    pub polarity: KnobPolarity,
    /// Smallest value the write path accepts. **Not always 0**: pw-sink's playout
    /// delay is floored at three packet times, so a pw-sink member cannot be
    /// placed arbitrarily early and that floor can be what pins the group's
    /// target.
    pub min_ms: u16,
    pub max_ms: u16,
}

/// The knob plan §2.4.2 models for each member kind.
pub fn knob_of(kind: MemberKind) -> Knob {
    match kind {
        MemberKind::Sendspin => Knob { polarity: KnobPolarity::Advance, min_ms: 0, max_ms: SENDSPIN_ADVANCE_MAX_MS },
        MemberKind::Airplay2 => {
            Knob { polarity: KnobPolarity::Delay, min_ms: 0, max_ms: crate::outputs::ap2::server::AP2_RENDER_DELAY_MAX_MS }
        }
        // The floor is the whole reason pw-sink is modelled separately from AP2:
        // three packet times of playout buffer is the least the receiving module
        // will accept (`sync_settings::PWSINK_JITTER_MIN_MS`).
        MemberKind::PwSink => Knob {
            polarity: KnobPolarity::Delay,
            min_ms: crate::sync_settings::PWSINK_JITTER_MIN_MS,
            max_ms: crate::sync_settings::PWSINK_JITTER_MAX_MS,
        },
    }
}

// ---------------------------------------------------------------- public state

/// Which acoustic promise the run is making (plan §1). The two are never treated as
/// each other, because they promise *different things to the user*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The phone stays at a listening position; the run drives the member list
    /// itself, twice. Aligns that position (plan §1.1's single step).
    SweetSpot,
    /// The user walks to each speaker holding the phone at it and says so, one
    /// speaker at a time, then returns to the first for the closure reading (W8a).
    /// Aligns the wire, so it is right everywhere — see [`run_walk`].
    NearField,
}

impl Mode {
    /// Whether arrivals are acquired by the user walking rather than by the run
    /// stepping the member list on its own.
    fn is_walk(self) -> bool {
        matches!(self, Mode::NearField)
    }
}

/// Plan §8's state machine, plus one state the plan's diagram omits: `Proposed`.
///
/// §11 requires `apply` to be a separate, explicit step, so the machine *must*
/// park somewhere between SOLVING and WRITING waiting for the user. That parking
/// state is `Proposed`; nothing is ever written without passing through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Arming,
    Learning,
    /// Near field only: parked, waiting for the user to say where they are
    /// ([`WalkProgress::next`] says which call is expected). Not terminal — the run
    /// is alive and holding the group — so `start` still refuses until it is
    /// abandoned.
    Walking,
    /// Chained multi-position only: parked between positions, waiting for the user to
    /// say which speakers they can hear from where they are now
    /// ([`ChainProgress::next`]). Not terminal, for the same reason [`Self::Walking`]
    /// is not — the run is alive, the group is held, and the aligned set carries
    /// provisional delays that a second `start` would strand.
    Positioning,
    Measuring,
    Solving,
    Proposed,
    Writing,
    Settling,
    Verifying,
    Done,
    Refused,
}

impl Phase {
    /// Whether a new run may be started from here.
    fn is_terminal(self) -> bool {
        matches!(self, Phase::Idle | Phase::Done | Phase::Refused | Phase::Proposed)
    }
}

/// Machine-readable half of a refusal. Every variant reaches the API with a
/// sentence the UI can show (plan §5.5: "it didn't work" is not acceptable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    /// No alignment session is running (there is nothing playing to measure).
    NoSession,
    /// The session went away mid-run.
    SessionLost,
    /// The session is still running but for a *different* group.
    SessionChanged,
    /// No microphone capture was connected when the run started.
    MicMissing,
    /// The microphone capture went away mid-run and did not come back.
    MicLost,
    /// The capture reconnected too often; each reconnect resets the timing
    /// reference (see the module docs).
    MicReconnected,
    /// The requested mode, or an option within it, is not implemented. Today that
    /// is only linking a near-field walk to a previously aligned set (plan §1.2's
    /// cross-session case), which needs W12's Δ propagation — W8b.
    ModeUnsupported,
    /// Near field: the walk's two readings of its first speaker disagree by more
    /// than any plausible clock drift can explain, so *something moved* and the
    /// whole walk is untrustworthy — not just one reading (see [`ClosureReport`]).
    ClosureError,
    /// Near field: an `arrival`/`close` call does not match what the walk is waiting
    /// for — an unknown speaker, one already measured, or closing before every
    /// member has been visited.
    WalkOutOfOrder,
    /// Chaining: a `position`/`finish` call does not match where the chain is — an
    /// unknown speaker, one already aligned at an earlier position, an overlap that
    /// was never aligned, or finishing with speakers still unaligned.
    ChainOutOfOrder,
    /// Chaining: a step after the first named **no** overlap, so there is nothing
    /// tying this position to the speakers already aligned (plan §1.1).
    OverlapMissing,
    /// Chaining: the step's two overlaps disagree by more than plausible geometry can
    /// explain, so the common shift this step would apply to the *whole* already-aligned
    /// set cannot be trusted (plan §1.1, [`OVERLAP_AGREEMENT_TOL_MS`]).
    OverlapDisagreement,
    /// Chaining: the provisional delay line refused the value the chain asked it for —
    /// the ratchet of plan §1.1 has run past `relay_delay::MAX_DELAY_MS`.
    ProvisionalRange,
    /// Near field: nobody said "I am at a speaker" within
    /// [`Timing::walk_arrival_timeout`].
    WalkTimeout,
    /// The estimator refused; `estimator_reason` carries its verdict verbatim.
    Estimator,
    /// The loop-phase gate never locked within its timeout.
    GateTimeout,
    /// Exclusivity was violated on a member and the reading could not be retaken
    /// (plan §12.3). A barge-in outranks the alignment hold by design, so this is a
    /// legitimate loss, not a bug — but it must be reported as itself.
    Interference,
    /// Arrival spread too close to the ±½-period wrap to be trusted.
    AmbiguousSpread,
    /// Cross-band transitivity failed — blocks the write (plan §10.2).
    Transitivity,
    /// The same member measured differently between passes.
    Repeatability,
    /// No knob setting can make the group arrive together: the members'
    /// achievable-arrival intervals do not all overlap (plan §2.4.2). Covers both
    /// the single-member case (one member would need more than its knob allows)
    /// and the genuinely mixed one — a sendspin member can only be moved *earlier*
    /// from where it sits and an AP2/pw-sink member only *later*, so a group whose
    /// sendspin member is already the earliest is not alignable at all.
    KnobRange,
    /// Post-write residual too large.
    ResidualTooLarge,
    /// A delay write failed.
    WriteFailed,
    /// The user abandoned the run.
    Cancelled,
    /// Something structural: too few members, an unusable capture rate, …
    Internal,
}

/// A refusal, in both machine and human form.
#[derive(Debug, Clone, Serialize)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// The estimator's own reason, when the refusal came from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimator_reason: Option<RejectReason>,
}

impl Refusal {
    fn new(kind: RefusalKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), member: None, estimator_reason: None }
    }

    fn for_member(kind: RefusalKind, member: &str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), member: Some(member.to_string()), estimator_reason: None }
    }
}

/// Non-blocking things the user still needs to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// A proposed delay raises the group's send-ahead requirement, which
    /// reconfigures the *whole group's* stream rather than one device (plan §9.2).
    SendAheadHighWater,
    /// The gate saw the burst amplitude decay monotonically — the behavioural
    /// signature of echo cancellation converging (plan §4.2).
    AecSuspected,
    /// The level-learning phase (W4) is not implemented; the session's single
    /// calibration level was used for every member.
    LevelLearningSkipped,
    /// The capture reconnected and the measurement set was restarted.
    MicReconnected,
    /// The mic-vs-audio clock drift could not be fitted (only one pass).
    NoDriftFit,
    /// Near field's premise — the phone is *at* each speaker, so the propagation
    /// path is negligible — is the user's to keep, and nothing here can check it.
    /// Raised on every near-field run rather than only when something looks wrong,
    /// because the failure it describes looks like a good measurement.
    NearFieldPathAssumed,
    /// Exclusivity was violated during the run (plan §12.3). Kept as a warning even
    /// when the affected window was successfully retaken, because it explains why the
    /// run took longer than it should have.
    Interference,
    /// A chain step was linked to the already-aligned set through **one** overlap.
    /// That reading is applied as a common shift to every speaker aligned so far and
    /// anchors everything after it, and with one overlap nothing checks it — so the
    /// step is weaker than the others and the chain's error stops being boundable
    /// (plan §1.1).
    OneOverlap,
    /// Every position of a chain is aligned at *its own* spot, so speakers aligned at
    /// different positions are related only through the overlaps — approximate in the
    /// doorway between two rooms. Raised on every chained run, because the failure it
    /// describes looks like a perfectly good result.
    ChainScope,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub kind: WarningKind,
    pub message: String,
}

impl Warning {
    fn new(kind: WarningKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// One member's result for a single pass.
#[derive(Debug, Clone, Serialize)]
pub struct MemberMeasurement {
    /// Arrival phase of the 3 kHz "A" burst, ms on the estimator's grid.
    pub phase_a_ms: f64,
    /// Arrival phase of the 1.5 kHz "B" burst, ms on the same grid.
    pub phase_b_ms: f64,
    pub std_error_ms: f64,
    pub peak_snr_db: f64,
    pub second_peak_ratio: f64,
    pub drift_ppm: f64,
    pub periods_used: usize,
}

/// One accepted measurement of one member in one pass.
#[derive(Debug, Clone, Serialize)]
pub struct MemberObservation {
    pub node_name: String,
    pub pass: usize,
    /// Which capture the phases belong to (see the module docs). Observations from
    /// different epochs are never compared.
    pub grid_epoch: u64,
    /// Pattern-period index at the centre of the accepted window — the abscissa of
    /// the common drift fit, and the same origin convention the estimator uses for
    /// its own intercept.
    pub period_centre: f64,
    #[serde(flatten)]
    pub m: MemberMeasurement,
}

/// Per-member progress, for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct MemberProgress {
    pub node_name: String,
    pub kind: MemberKind,
    /// Calibration level used while this member was soloed (W4 seam).
    pub level: u8,
    /// The delay this member had when the run started (the revert target).
    pub current_delay_ms: u16,
    pub passes_done: usize,
    /// Most recent accepted measurement.
    pub last: Option<MemberMeasurement>,
    /// What the gate is currently waiting for, or why the last attempt failed.
    pub note: Option<String>,
}

/// One member's proposed write.
#[derive(Debug, Clone, Serialize)]
pub struct ProposedDelay {
    pub node_name: String,
    pub kind: MemberKind,
    /// Measured arrival relative to the earliest-arriving member, ms (≥ 0). This
    /// is the *acoustic* answer, before any knob arithmetic.
    pub arrival_ms: f64,
    pub current_delay_ms: u16,
    pub new_delay_ms: u16,
    /// `new − current`. Its sign says how the knob *moves*, not which way the
    /// sound moves — read it together with [`Self::polarity`], because raising a
    /// sendspin advance makes that speaker play earlier.
    pub added_ms: i32,
    pub std_error_ms: f64,
    /// How much of this member's raw phase was attributed to **mic-vs-audio clock
    /// drift** rather than to the speaker, ms, relative to the first reading of the
    /// run: `slope × (this member's measurement time − the first measurement time)`.
    ///
    /// It is the visible half of [`fit_drift`]'s slope, and in near field it is the
    /// closure error distributed by *when in the walk* each member was measured — so a
    /// speaker read early in the walk is corrected by little and the last one by
    /// nearly the whole closure error. Reported because a correction that silently
    /// reshapes every number is exactly the kind of thing that should be inspectable.
    ///
    /// A member measured **more than once** (the closure anchor; every member of a
    /// multi-position run) is quoted at the *mean* of its measurement times, which is
    /// where the line fit places it. So the anchor's correction lands between its two
    /// visits rather than at either of them.
    pub drift_correction_ms: f64,
    /// The member whose knob lands at the smallest value — the one the others are
    /// moved towards. Kept as `reference` for the API's sake, but it is an
    /// *outcome* of the interval intersection, not an input to it (plan §2.4.2:
    /// there is no chosen reference any more).
    pub is_reference: bool,
    /// Which way this member's knob moves it (plan §2.4.1).
    pub polarity: KnobPolarity,
    pub knob_min_ms: u16,
    pub knob_max_ms: u16,
    /// This member's achievable arrivals on the same scale as [`Self::arrival_ms`]
    /// — the interval the common target had to fall inside.
    pub achievable_lo_ms: f64,
    pub achievable_hi_ms: f64,
    /// One legible sentence: what the knob becomes, and in which direction. The
    /// UI can render its own from the fields above, but a user must never be shown
    /// "delay" for an advance, so the daemon says it once, correctly.
    pub effect: String,
}

/// §10.2's transitivity check. See [`transitivity`] for what it can and cannot
/// see — the honesty of this whole feature rests on that doc comment.
#[derive(Debug, Clone, Serialize)]
pub struct TransitivityCheck {
    pub worst_pair: Option<(String, String)>,
    pub worst_ms: f64,
    pub tolerance_ms: f64,
    pub passed: bool,
    /// Plain-language statement of the blind spot this check does *not* close.
    pub caveat: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepeatabilityCheck {
    pub worst_member: Option<String>,
    pub worst_ms: f64,
    pub tolerance_ms: f64,
    pub passed: bool,
}

/// Near field's **closure measurement**: the walk's first speaker, measured again at
/// the end (plan §1's "bonus property", §5.3's missing drift baseline).
///
/// ## Why it exists
///
/// A near-field walk measures each member **once**, in walk order, so there is no
/// second pass and therefore no time baseline for [`fit_drift`]'s slope — and a walk
/// through a house takes minutes. At 100 ppm that is *milliseconds* of accumulated
/// mic-vs-audio clock creep, which is indistinguishable from real offsets and would
/// be written straight into the speakers as delay. Revisiting the first speaker
/// closes the loop: two readings of the *same* member with the *same* knob, separated
/// by the whole walk, and their difference is the accumulated drift.
///
/// ## How the correction is distributed
///
/// It is not distributed by a second mechanism. Feeding the closure observation to
/// [`fit_drift`] makes the anchor the one member with two points, so the pooled slope
/// reduces exactly to `error_ms / span_periods`, and every single-reading member's
/// offset becomes `phase − slope × period_centre` — its correction is therefore
/// *proportional to when in the walk it was measured*, which is what accumulated
/// drift actually does. [`ProposedDelay::drift_correction_ms`] reports each member's
/// share. So the closure measurement **is** the drift fit, rather than feeding one.
///
/// ## What it cannot separate — and this is load-bearing
///
/// The difference between the two readings is drift **plus anything that genuinely
/// changed** between them: a speaker that was moved, a phone held at a different
/// distance on the way back, a knob written mid-walk. The arithmetic cannot tell
/// them apart, and it applies the correction either way. All it can do is refuse a
/// difference too large for *any* plausible clock — see [`Self::tolerance_ms`] — and
/// a slow real change inside that bound is silently smeared across the group.
#[derive(Debug, Clone, Serialize)]
pub struct ClosureReport {
    /// The walk's first speaker: the one measured twice.
    pub anchor: String,
    /// Second reading minus first, ms, wrapped into ±½ pattern. Signed: which way
    /// the clocks ran matters for reading the per-member corrections.
    pub error_ms: f64,
    /// Pattern periods between the two readings — the drift's time baseline.
    pub span_periods: f64,
    /// The same span in seconds, which is what the ppm bound is expressed against.
    pub span_s: f64,
    /// The drift [`Self::error_ms`] implies over that span, ppm.
    pub drift_ppm: f64,
    /// The largest error the walk's own duration can explain as clock drift:
    /// `max(MIN_CLOSURE_TOL_MS, span_s × MAX_CLOSURE_DRIFT_PPM / 1000)`.
    ///
    /// Deliberately a *rate* bound rather than a fixed number of milliseconds. A
    /// speaker that moved shows up as a large error over a short walk (an absurd
    /// implied ppm), while genuine clock drift is bounded in ppm by construction —
    /// so the discrimination lives in the rate, not the magnitude. The consequence
    /// is that a long walk earns a large absolute allowance, and a real change
    /// inside it cannot be caught.
    pub tolerance_ms: f64,
    pub passed: bool,
    /// Plain statement of what a pass does and does not establish.
    pub caveat: &'static str,
}

/// Whether a near-field walk is acquiring the measurement or confirming the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkPurpose {
    /// The measurement walk: one arrival per member, then the closure → `Proposed`.
    Measure,
    /// The post-write walk (plan §10.1's residual).
    ///
    /// Near field **has** to verify by walking again, and this is not a preference.
    /// A stationary residual measures `wire + path(P)`; after a correct near-field
    /// write the wire terms are equal, so what is left is each speaker's path
    /// difference to wherever the phone is standing — tens of milliseconds. It would
    /// fail [`RESIDUAL_TOL_MS`] every single time and report a correct alignment as
    /// broken. The only residual that means anything for a wire alignment is measured
    /// where the wire was.
    Verify,
}

/// What a near-field walk expects the UI to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkAction {
    /// `POST /api/align/measure/arrival` naming the speaker the user is standing at.
    Arrival,
    /// `POST /api/align/measure/close`: every member has been measured; walk back to
    /// [`WalkProgress::anchor`] for the closure reading.
    Close,
    /// A reading is in progress — the accepted call is already being served.
    Busy,
    /// The walk is over (look at [`MeasureStatus::phase`] for how it ended). Kept
    /// visible rather than cleared, because the closure numbers are part of the
    /// verdict.
    Done,
}

/// Near field's live state: where the walk is, what is expected next, and the two
/// things about a walk's *result* that a user has to be told rather than infer.
#[derive(Debug, Clone, Serialize)]
pub struct WalkProgress {
    pub purpose: WalkPurpose,
    pub next: WalkAction,
    /// The first speaker measured — the one to come back to. `None` before the walk
    /// has started.
    pub anchor: Option<String>,
    /// Members measured so far, **in walk order**. That order is the abscissa of the
    /// drift correction, which is why it is reported rather than being a set.
    pub measured: Vec<String>,
    /// Members still to visit, in no particular order — the walk order is the user's
    /// to choose (plan §12.1: near field's UI owns it).
    pub remaining: Vec<String>,
    /// The member being read right now.
    pub reading: Option<String>,
    /// Times this walk has had to start over because the capture reconnected
    /// (plan §1.2 — a walk is one capture).
    pub restarts: u32,
    /// One sentence telling the user what to do next.
    pub prompt: String,
    pub closure: Option<ClosureReport>,
    /// What this walk's result is coherent *with*, said plainly because the opposite
    /// is easy to assume (plan §1.2). Linking two walks through an overlap speaker is
    /// W8b and needs W12's Δ propagation, so it does not exist here.
    pub scope_note: &'static str,
    /// Where each arrival's playback level comes from (plan §12.2).
    pub level_note: &'static str,
}

/// Near field states its own scope rather than letting the user assume the flattering
/// reading. One walk is internally coherent and nothing more.
const WALK_SCOPE_NOTE: &str =
    "the speakers in this walk are aligned to each other, and to nothing else: this result is not related to any \
     set aligned in an earlier session, even where the two share a speaker. Linking two walks through a shared speaker is not \
     implemented (it needs the multi-position chaining machinery), so anything that must sound coherent has to be walked in one \
     session.";

/// Plan §12.2: near field folds the level into each arrival, so there is no level
/// *learning* phase to skip — the level is only meaningful at the speaker, and the
/// risk there inverts from too-quiet to clipping.
const WALK_LEVEL_NOTE: &str = "each speaker is measured at the level set for it when you arrived (POST /api/align/audible while you stand \
     there, and watch /api/align/mic/signal go green): at arm's length a level chosen anywhere else is wrong, and the danger is \
     clipping rather than being too quiet.";

// --------------------------------------------- multi-position chaining (W12, §1.1)

/// What a chained multi-position run expects the UI to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainAction {
    /// `POST /api/align/measure/position` naming the speakers audible from where the
    /// user is standing now, plus the overlaps from the already-aligned set.
    Position,
    /// Every held speaker has been aligned at some position: `POST
    /// /api/align/measure/finish` renormalises the whole chain and proposes the write.
    /// A further position is still accepted while this is showing — a user may want to
    /// re-link a region through more overlaps before finishing.
    Finish,
    /// A position is being measured; the accepted call is already being served.
    Busy,
    /// The chain is over (see [`MeasureStatus::phase`]). Kept visible rather than
    /// cleared, because the per-step numbers are the verdict.
    Done,
}

/// How well a step's link to the already-aligned set could be checked (plan §1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapConfidence {
    /// The first position. Nothing was aligned yet, so there is nothing to link to and
    /// nothing to check — this step *defines* the chain's reference.
    Origin,
    /// One overlap. The step is anchored, but that single reading is applied as a
    /// common shift to the entire already-aligned set **and** anchors everything
    /// measured after it, with nothing to check it against.
    Single,
    /// Two or more overlaps: their disagreement is an independent estimate of this
    /// joint's error — spatial redundancy, which is what §1.1 asks for.
    Checked,
}

/// One overlap as it read at the new position.
#[derive(Debug, Clone, Serialize)]
pub struct ChainOverlap {
    pub node_name: String,
    /// Its arrival at *this* position, on this step's own scale. Already includes the
    /// provisional delay it is carrying — that is what makes the chain work (plan §1.1).
    pub arrival_ms: f64,
    /// The provisional delay it was carrying while this reading was taken.
    pub applied_ms: f64,
}

/// One member's provisional delay, as the relay is applying it right now.
///
/// Provisional means exactly that: it lives in the per-device delay line
/// (`align/relay_delay.rs`), nothing is persisted, and a daemon restart drops it (plan
/// §1.1.1). The real knobs are written **once**, at the end of the chain.
#[derive(Debug, Clone, Serialize)]
pub struct ProvisionalDelay {
    pub node_name: String,
    /// What the chain's arithmetic holds for this member, ms — exact, so a step's Δ
    /// does not accumulate rounding.
    pub delay_ms: f64,
    /// What was actually pushed to the line, in whole ms. The line itself is
    /// sample-accurate; this rounding is the same 1 ms granularity the final write has
    /// (plan §1.1.2 item 4), and for an overlap it is *observed* rather than assumed,
    /// because the next position measures the overlap through it.
    pub applied_ms: u16,
}

/// One position of a chain, after it was measured (plan §1.1).
#[derive(Debug, Clone, Serialize)]
pub struct ChainStep {
    /// 1-based, in the order the user walked.
    pub index: usize,
    /// The speakers this position aligned — the ones that were not already aligned.
    pub members: Vec<String>,
    /// The already-aligned speakers this position was linked through.
    pub overlaps: Vec<ChainOverlap>,
    pub confidence: OverlapConfidence,
    /// Worst pairwise disagreement between the overlaps at this position, ms. `None`
    /// for a first or single-overlap step, which have nothing to compare.
    pub disagreement_ms: Option<f64>,
    /// The pair [`Self::disagreement_ms`] came from.
    pub worst_pair: Option<(String, String)>,
    pub tolerance_ms: f64,
    /// Where the already-aligned set is judged to arrive at this position: the mean of
    /// the overlap readings. `None` for the first step.
    pub anchor_ms: Option<f64>,
    /// The common delay this step added to **every** member of the already-aligned
    /// set, ms. Non-zero exactly when a new member at this position arrived *later*
    /// than the aligned set does — and it goes to all of them, not just the overlap,
    /// because a common delay added to an aligned set preserves its internal
    /// alignment. That is the trick the whole feature rests on (plan §1.1).
    pub delta_ms: f64,
    /// The arrival this position was aligned at, on this step's own scale.
    pub target_ms: f64,
    /// Arrival spread across this position's members, ms.
    pub spread_ms: f64,
    /// Mic-vs-audio clock drift fitted at *this* position, ppm. Each position has its
    /// own fit; there is no single figure for a chain.
    pub drift_ppm: f64,
    /// Half of [`Self::disagreement_ms`] — how far this joint's common shift can be
    /// out, given that the anchor is the mean of two readings that disagree by that
    /// much. `None` when the step had nothing to check it with, which is what makes the
    /// chain's total unboundable (see [`ChainError`]).
    pub joint_error_ms: Option<f64>,
    /// Which capture this position was measured in. Positions can differ — the overlap
    /// re-measurement is what bridges a new frame — and a position whose *own* readings
    /// spanned two captures is discarded rather than solved (plan §1.2).
    pub grid_epoch: u64,
    /// The §10 checks over this position's own readings. They block the step: a chain
    /// must not carry a position that failed transitivity into every position after it.
    pub checks: Checks,
    pub note: String,
}

/// What the chain's accumulated error can and cannot be said to be (plan §1.1).
#[derive(Debug, Clone, Serialize)]
pub struct ChainError {
    /// True when *every* joint was checked by two overlaps.
    pub bounded: bool,
    /// The joints' worst case: the sum of each step's [`ChainStep::joint_error_ms`].
    /// `None` — deliberately, rather than a partial sum — when any joint was linked
    /// through a single overlap, because a total that silently omitted the one
    /// unmeasurable joint would be worse than no total.
    pub joint_ms: Option<f64>,
    pub message: String,
}

/// A chained run's live state: where the chain is, what it wants next, and the two
/// things about a chain's *result* the user has to be told rather than infer.
#[derive(Debug, Clone, Serialize)]
pub struct ChainProgress {
    pub next: ChainAction,
    pub steps: Vec<ChainStep>,
    /// Every speaker aligned so far, in the order they were aligned.
    pub aligned: Vec<String>,
    /// Held speakers not aligned at any position yet. `finish` refuses while this is
    /// non-empty: a member with no reading has nothing to write.
    pub remaining: Vec<String>,
    /// What the relay is applying right now (plan §1.1.1). Nothing is persisted.
    pub provisional: Vec<ProvisionalDelay>,
    /// The smallest provisional delay in the aligned set, ms — the floor that ratchets
    /// upward because every step can only *add*. `finish` subtracts it globally, which
    /// is a common shift and therefore free (plan §1.1, §2.4.2).
    pub floor_ms: f64,
    /// The position being measured right now.
    pub measuring: Option<usize>,
    /// Times the position in flight has been restarted because the capture reconnected.
    pub restarts: u32,
    pub prompt: String,
    pub error: ChainError,
    /// Why the last position was rejected, when it was.
    ///
    /// The chain **stays alive** through this: the positions already aligned keep their
    /// provisional delays, and the user can stand where they are and post the position
    /// again with better overlaps or the phone further from a wall. Losing a whole
    /// apartment's chain because one joint's overlaps disagreed would be the wrong
    /// trade. Cleared as soon as a position is accepted.
    pub refusal: Option<Refusal>,
    /// What a chain's result is coherent *with*, said plainly because the flattering
    /// reading is the easy one to assume (plan §1.1).
    pub scope_note: &'static str,
}

/// Plan §1.1's honesty clause, stated on every chained run rather than inferred from
/// the numbers.
const CHAIN_SCOPE_NOTE: &str =
    "each position is aligned at the spot it was measured from, and the positions are tied to one another only through their overlap \
     speakers. Two speakers aligned at *different* positions are therefore related only indirectly: in the doorway between two rooms they \
     are approximate, and the overlap disagreements above are the only bound on how approximate. That is inherent to measuring from \
     several places with one microphone — given the premise that no single position hears everything, it is the trade this mode makes. A \
     result that is right everywhere at once is what near-field mode is for.";

/// §10.3's merged-peak check — a documented seam, not an implementation.
#[derive(Debug, Clone, Serialize)]
pub struct MergedPeakCheck {
    pub state: &'static str,
    pub reason: &'static str,
}

impl MergedPeakCheck {
    /// Why this is a seam rather than a check.
    ///
    /// The merged-peak test needs **every** member audible on one identical burst.
    /// The session's audibility control solos *at most two* members
    /// (`calibrate::apply_audibility` keeps the reference and the target), so an
    /// N-member merged peak is not expressible without new excitation — that is
    /// W7's `cal_gate`. The pairwise version that *is* expressible would resolve
    /// no better than the estimator's guard distance (burst + analysis window,
    /// ~12 ms at 48 kHz): two arrivals closer than that merge into one candidate
    /// by construction, so it would report "single peak" for a 5 ms error that the
    /// residual check already catches to ~0.1 ms. It would cost a
    /// reconnect-length gate per member and tell the user nothing new.
    fn seam() -> Self {
        Self {
            state: "not_implemented",
            reason: "needs every member audible on one identical burst (W7 excitation); the two-member form it could \
                     use today resolves no better than ~12 ms, which the residual check already beats",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResidualCheck {
    pub worst_member: Option<String>,
    pub worst_ms: f64,
    pub tolerance_ms: f64,
    pub passed: bool,
}

/// The three §10 checks, in one place, reported whether they pass or fail.
#[derive(Debug, Clone, Serialize)]
pub struct Checks {
    pub transitivity: TransitivityCheck,
    /// Pass-to-pass agreement. `None` when it is not *available* — a single-pass
    /// multi-position run — and also `None` for **near field**, where it would be
    /// available but vacuous: the only member with two readings is the closure
    /// anchor, and the drift slope was fitted from exactly those two points, so its
    /// residual is zero by construction. Showing that as a green check would be
    /// reporting an identity as evidence. [`Self::closure`] is near field's
    /// equivalent, and it is a real check.
    pub repeatability: Option<RepeatabilityCheck>,
    pub merged_peak: MergedPeakCheck,
    /// Near field's closure measurement (`None` for multi-position, which has no
    /// walk to close).
    pub closure: Option<ClosureReport>,
}

/// What `apply` would write, and the confidence behind it.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    /// The member whose knob ends up smallest — everyone else is moved towards it.
    /// An *outcome* of the solve, not a choice: plan §2.4.2 replaced "pick the
    /// latest-arriving member" with an interval intersection. Also the arrival the
    /// post-write residual is measured against.
    pub reference: String,
    pub pattern_ms: f64,
    /// Arrival spread across the group, ms.
    pub spread_ms: f64,
    /// Fitted mic-vs-audio clock drift, ppm.
    pub drift_ppm: f64,
    /// The common arrival every member is being moved to, on the same scale as
    /// [`ProposedDelay::arrival_ms`] (0 = the earliest member as measured). May be
    /// negative: a sendspin group is aligned *earlier* than anyone currently
    /// arrives whenever a member already carries an advance.
    pub target_ms: f64,
    /// The intersection of every member's achievable arrivals — the interval
    /// `target_ms` was chosen from (plan §2.4.2).
    pub feasible_lo_ms: f64,
    pub feasible_hi_ms: f64,
    /// The largest knob value the proposal writes. This is what the target choice
    /// minimises: both polarities cost latency (an AP2 delay directly, a sendspin
    /// advance through `required_send_ahead_us`), so plan §9.2's "keep the applied
    /// delay small" generalises to "keep the biggest knob small".
    pub largest_knob_ms: u16,
    pub members: Vec<ProposedDelay>,
    pub checks: Checks,
    pub warnings: Vec<Warning>,
    /// Set when a check blocks the write. The numbers stay visible on purpose: a
    /// green residual with a failed transitivity check is the interesting failure
    /// and must not be hidden (plan §10).
    pub blocked: Option<Refusal>,
}

/// Post-write verification (plan §10).
#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    pub residual: ResidualCheck,
    pub transitivity: TransitivityCheck,
    pub merged_peak: MergedPeakCheck,
    pub observations: Vec<MemberObservation>,
    pub passed: bool,
    /// What this verification covered, when that is less than "the group". `None` for a
    /// single-position run, where every member was re-measured.
    ///
    /// A **chain** can only be verified where the phone is, which is the last position:
    /// the residual is measured over that position's own set (its new members *and* its
    /// overlaps, which the step's Δ put in step with them), and the earlier positions
    /// are not re-measured. Saying so is the same honesty §10.4 needed for a walk — a
    /// residual taken from one spot over speakers aligned at another would fail every
    /// time and report a correct chain as broken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_note: Option<String>,
}

/// What the gate is doing right now, surfaced so a stuck run explains itself.
#[derive(Debug, Clone, Serialize)]
pub struct GateProgress {
    pub locked: bool,
    pub periods: usize,
    pub needed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<GateReason>,
    pub message: String,
    pub restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

/// `GET /api/align/measure`.
#[derive(Debug, Clone, Serialize)]
pub struct MeasureStatus {
    pub phase: Phase,
    pub mode: Mode,
    /// The group being measured (the alignment session's identity).
    pub sources: Vec<String>,
    pub sample_rate: u32,
    /// One sentence describing what the run is doing or why it stopped.
    pub message: String,
    pub members: Vec<MemberProgress>,
    pub observations: Vec<MemberObservation>,
    pub proposal: Option<Proposal>,
    pub verification: Option<Verification>,
    pub refusal: Option<Refusal>,
    pub warnings: Vec<Warning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateProgress>,
    /// Near field only: where the walk is and what it wants next. `None` for a
    /// multi-position run, which needs nothing from the user between arrivals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk: Option<WalkProgress>,
    /// Chained multi-position only: where the chain is, what it wants next, the
    /// per-position numbers and what the result is *not* coherent with. `None` for a
    /// single-position run and for a near-field walk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<ChainProgress>,
    /// `POST measure/apply` will be accepted.
    pub can_apply: bool,
    /// `POST measure/revert` has something to restore.
    pub can_revert: bool,
    /// The group a pending revert belongs to — the alignment session's sources as
    /// they were when the write happened. `Some` exactly while [`Self::can_revert`]
    /// is true, **including after `abandon`**, so the daemon rather than the UI is
    /// the one that remembers which group has an outstanding write. Without it a
    /// page reload loses the only pointer back to a destructive change (plan §9.4).
    pub revert_scope: Option<Vec<String>>,
    pub elapsed_s: u64,
}

// ---------------------------------------------------------------- injection

/// Boxed future, so the traits below stay object-safe without an `async_trait`
/// dependency.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMember {
    pub node_name: String,
    pub kind: MemberKind,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub active: bool,
    pub sources: Vec<String>,
    pub members: Vec<SessionMember>,
    /// The session's calibration playback level (0–100), and the default for any
    /// member [`Self::levels`] has no entry for.
    pub level: u8,
    /// **Per-member** level as the session last applied it (`calibrate`'s W19 map).
    ///
    /// Near field's whole level model, and the reason this is read rather than
    /// decided here (plan §12.2): the user stands at a speaker, moves the slider
    /// until the signal check goes green, and *that* is the level the arrival must be
    /// measured at. A level chosen anywhere else — including one this module picked
    /// for the group — is wrong at arm's length.
    pub levels: HashMap<String, u8>,
}

impl SessionSnapshot {
    /// The level to measure `node_name` at: what the session last applied to it,
    /// falling back to the session default for a member never soloed yet.
    fn level_for(&self, node_name: &str) -> u8 {
        self.levels.get(node_name).copied().unwrap_or(self.level)
    }
}

/// What the orchestration needs from the alignment session (`align/calibrate.rs`).
///
/// Deliberately two methods: this module must **not** own muting, volume,
/// playback or teardown — that machinery already exists and is live-tested, and a
/// second copy of it would be a second way to leave a room muted with a click
/// looping. Being a trait is what lets the state machine be unit-tested without a
/// PipeWire graph.
pub trait SessionControl: Send + Sync {
    fn snapshot(&self) -> Fut<'_, SessionSnapshot>;
    /// Make exactly one member audible at `level`, muting every other member.
    fn solo(&self, node_name: String, level: u8) -> Fut<'_, Result<(), String>>;
    /// Drain the exclusivity violations recorded since the last call (plan §12.3).
    ///
    /// Draining rather than peeking, because every entry is consumed here: one for the
    /// member being measured aborts its window with the cause named, and one for any
    /// other member still becomes a warning, so nothing is silently dropped.
    fn take_interference(&self) -> Fut<'_, Vec<crate::align::group::Interference>>;
}

impl SessionControl for crate::align::calibrate::AlignManager {
    fn snapshot(&self) -> Fut<'_, SessionSnapshot> {
        Box::pin(async move {
            let s = self.status().await;
            SessionSnapshot {
                active: s.active,
                sources: s.sources,
                members: s.members.iter().map(|m| SessionMember { node_name: m.node_name.clone(), kind: m.kind }).collect(),
                level: s.volume,
                levels: s.levels.into_iter().collect(),
            }
        })
    }

    fn take_interference(&self) -> Fut<'_, Vec<crate::align::group::Interference>> {
        Box::pin(async move { crate::align::calibrate::AlignManager::take_interference(self).await })
    }

    fn solo(&self, node_name: String, level: u8) -> Fut<'_, Result<(), String>> {
        Box::pin(async move {
            // `AlignManager::solo` is the one-element case of the session's set-based
            // audibility, and it applies the level in the same call.
            //
            // Deliberately not `select(x, x)`, which the earlier version used: that
            // reaches the same audible set, but it also overwrites the session's
            // reference/target with `x`. Those two belong to the *by-ear* panel, and a
            // measurement run silently rewriting them would leave the manual path
            // pointing at whichever member happened to be measured last.
            self.solo(node_name, level).await.map(|_| ())
        })
    }
}

/// The mic ingest, as this module consumes it (`align_mic`'s W3 hand-off).
pub trait MicFeed: Send + Sync {
    fn status(&self) -> MicStatus;
    fn window_from(&self, first_frame: u64, frames: usize) -> Option<MicWindow>;
}

/// The process-global ingest.
pub struct LiveMic;

impl MicFeed for LiveMic {
    fn status(&self) -> MicStatus {
        crate::align::mic::shared().status()
    }

    fn window_from(&self, first_frame: u64, frames: usize) -> Option<MicWindow> {
        crate::align::mic::shared().window_from(first_frame, frames)
    }
}

/// Writes one member's delay knob.
///
/// Implemented in `api.rs` **on top of the existing endpoint handlers** rather
/// than on `sync_settings`, so the persistence order, the clamping, the live push
/// and — the one that matters — the per-device reconnect and its group-wide
/// high-water exception are not duplicated here (plan §9.3).
pub trait DelayWriter: Send + Sync {
    fn write(&self, node_name: String, kind: MemberKind, delay_ms: u16) -> Fut<'_, Result<String, String>>;
}

/// Everything a run needs from the daemon, assembled by the API handler so this
/// module never sees `AppState`.
pub struct MeasureDeps {
    pub mode: Mode,
    /// Run the multi-position mode as a **chain** (plan §1.1): park between positions
    /// and take one [`MeasureManager::position`] call per listening spot, each naming
    /// the speakers audible there plus the overlaps from the already-aligned set.
    ///
    /// Opt-in rather than inferred, because it changes who drives the run. `false` is
    /// the single-position case — which is a chain with one step, and behaves exactly as
    /// it did before W12: the daemon measures every member itself from wherever the
    /// phone is sitting. Ignored for [`Mode::NearField`], which has its own acquisition
    /// (a walk needs no overlaps at all, §1.2).
    pub chained: bool,
    /// Speakers from an **earlier**, already-aligned run that this one should be made
    /// coherent with, through a shared overlap member (plan §1.2's cross-session case,
    /// §12.1's "link this set or keep it independent?").
    ///
    /// Empty means independent, which is the only thing implemented. Chaining *within*
    /// one run exists ([`Self::chained`]); linking across **runs** does not, because
    /// nothing stores a finished run's aligned set with its delays — that is W8b. A
    /// non-empty list is refused as [`RefusalKind::ModeUnsupported`] rather than
    /// accepted and quietly ignored: a run that *said* it linked but did not would leave
    /// the user believing in a coherence that does not exist.
    pub link_to: Vec<String>,
    pub session: Arc<dyn SessionControl>,
    pub mic: Arc<dyn MicFeed>,
    pub writer: Arc<dyn DelayWriter>,
    /// The provisional delay line (plan §1.1.1). A chain applies its per-step delays
    /// here and writes the real knobs once, at the end; a single-position run never
    /// touches it.
    pub relay: Arc<dyn RelayControl>,
    /// Each member's currently persisted delay, keyed by node name.
    pub current_delays: HashMap<String, u16>,
    pub send_ahead: SendAheadContext,
    pub timing: Timing,
}

/// Inputs for the plan §9.2 send-ahead warning.
///
/// A sendspin group's send-ahead is a high-water mark over its members'
/// `min_buffer_ms + static_delay_ms` (`outputs::sendspin::server::required_send_ahead_us`).
/// Raising it reconfigures the *group's* stream — every speaker in the room goes
/// quiet for tens of seconds — where a smaller change reconnects only the one
/// device. So the solve warns before crossing that line.
///
/// **What feeds the mark, after §2.4.1.** `static_delay_ms` is an **advance**, and
/// it is added to the group lead *because* the device plays that much earlier. So
/// the quantity that lifts the high-water mark is an advance, not a delay: an AP2
/// render delay or a pw-sink playout delay happens inside that member's own
/// sender and never touches the sendspin lead. [`Self::mark_ms`] therefore takes
/// **advances**, and [`solve`] passes it only the members whose polarity is
/// [`KnobPolarity::Advance`].
#[derive(Debug, Clone, Default)]
pub struct SendAheadContext {
    /// The floor from everything that is not a member advance: the configured
    /// group lead, and the codec's decode floor.
    pub floor_ms: u32,
    /// What a member that reports no `min_buffer_ms` is assumed to need.
    pub unreported_floor_ms: u32,
    /// Per-sendspin-member `min_buffer_ms` as the device reported it.
    pub min_buffer_ms: HashMap<String, Option<u32>>,
}

impl SendAheadContext {
    /// The send-ahead mark a given set of **advances** implies, in ms. Anything in
    /// `advances` that this context does not know a `min_buffer_ms` for is ignored,
    /// which is what keeps a delay-polarity member from being counted into a lead
    /// it has no part in.
    fn mark_ms(&self, advances: &HashMap<String, u16>) -> u32 {
        self.min_buffer_ms
            .iter()
            .map(|(node, min_buffer)| min_buffer.unwrap_or(self.unreported_floor_ms) + u32::from(advances.get(node).copied().unwrap_or(0)))
            .fold(self.floor_ms, u32::max)
    }
}

// ---------------------------------------------------------------- the gate

/// One gate, one set of thresholds.
#[derive(Debug, Clone, Copy)]
pub struct GateConfig {
    pub min_periods: usize,
    pub amp_tolerance_db: f64,
    pub timeout: Duration,
}

impl GateConfig {
    /// Entering a measuring state after a solo switch.
    pub fn mute_settle(timing: &Timing) -> Self {
        Self { min_periods: GATE_MIN_PERIODS, amp_tolerance_db: GATE_AMP_TOL_DB, timeout: timing.gate_settle_timeout }
    }

    /// Entering a measuring state after a delay write, i.e. across a device
    /// reconnect (plan §2.3: tens of seconds, per device).
    pub fn reconnect(timing: &Timing) -> Self {
        Self { min_periods: GATE_MIN_PERIODS, amp_tolerance_db: GATE_AMP_TOL_DB, timeout: timing.gate_reconnect_timeout }
    }
}

/// One pattern period's worth of evidence, as the gate consumes it. Derived from
/// the mic status, the raw window and the estimator — so the gate itself is pure
/// decision logic and is unit-testable without audio.
#[derive(Debug, Clone)]
pub struct GateSample {
    /// Since the gate started (its timeout is measured against this).
    pub elapsed: Duration,
    pub connected: bool,
    /// The capture restarted: frame counter reset, so the grid origin moved.
    pub reconnected: bool,
    /// A sequence gap landed in the accumulated window.
    pub gap: bool,
    /// A sample hit full scale in the accumulated window.
    pub clipped: bool,
    /// Peak |sample| over *this* period — the amplitude-stability input.
    pub peak: f32,
    /// Complete periods the estimator has fitted so far.
    pub periods_used: usize,
    /// The estimator's verdict over the accumulated window, verbatim.
    pub quality: Quality,
    /// Set when exclusivity was violated on this member during the window, carrying
    /// `align_group::Interference::reason` verbatim. Judged before anything else,
    /// because it *causes* the level changes the other checks would otherwise blame
    /// on the room or the phone.
    pub interference: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    MicDisconnected,
    MicReconnected,
    SequenceGap,
    Clipped,
    Silent,
    /// Exclusivity was violated on this member — a barge-in announcement played on
    /// it, or a voice-duck hold attenuated it (`align_group::InterferenceCause`).
    ///
    /// This exists so the failure names the doorbell. Without it the level change an
    /// announcement causes is caught by the amplitude-stability check and reported as
    /// "hold the phone still" — the exact misdiagnosis plan §12.3 exists to prevent.
    Interference,
    /// The tone keeps arriving and then stopping: this member's *stream* is
    /// breaking up, not the room or the phone.
    ///
    /// Hardware-observed (2026-08-11): a sendspin device can wedge into a state
    /// where a stream renders intermittently, and a reconnect clears it — nudging
    /// the static delay up and down is enough, because that forces the device to
    /// reconnect. Without this reason the symptom is caught by the amplitude-spread
    /// check instead and reported as "hold the phone still", which sends the user
    /// after entirely the wrong thing.
    Intermittent,
    UnstableAmplitude,
    AecSuspected,
    /// Not enough periods yet — the ordinary "still working" state.
    Acquiring,
    /// The estimator is not (yet) happy; its own message is carried through.
    Estimator,
}

/// The gate's verdict on one sample.
#[derive(Debug, Clone)]
pub struct GateStep {
    /// The accumulated window is trustworthy: measure it.
    pub locked: bool,
    /// Drop the accumulated window and re-acquire from scratch.
    pub restart: bool,
    /// Unrecoverable inside this gate (timeout, or the mic went away).
    pub failed: Option<Refusal>,
    pub progress: GateProgress,
}

/// **The** gate (plan §8): re-acquire loop-phase lock with stable amplitude
/// before accepting any window.
///
/// One mechanism, used at every entry into a measuring state — after a solo
/// switch, after a delay write's reconnect wave, and after any disturbance the
/// capture itself reports. Lock means *all* of:
///
/// 1. the capture is connected, is the same capture it was (no reconnect), and
///    the accumulated window contains no sequence gap and no clipped sample —
///    plan §5.5 refuses to measure on any of those;
/// 2. at least [`GateConfig::min_periods`] complete pattern periods have
///    accumulated *since the disturbance*, so the estimator's line fit has a
///    residual and therefore a standard error;
/// 3. the estimator accepts that window on its own terms — peak SNR, second-peak
///    ratio and standard error all inside its thresholds;
/// 4. the burst amplitude was stable across the window
///    ([`GateConfig::amp_tolerance_db`]), which is what actually catches a mute
///    that has not settled, and — as a monotonic decay — the behavioural AEC
///    signature of plan §4.2.
///
/// Anything in (1) or (4) discards the window and starts over; (2) and (3) are
/// simply "not yet" until the timeout, at which point the *last* reason is
/// reported — including the estimator's own sentence, so the user learns "the
/// tone is 6 dB above the noise floor" rather than "timed out".
pub struct Gate {
    cfg: GateConfig,
    /// Per-period peaks over the accumulated window.
    peaks: Vec<f32>,
    restarts: u32,
    aec_suspected: bool,
    last: Option<(GateReason, String)>,
    member: Option<String>,
    /// A tone was heard from this member at some point.
    heard: bool,
    /// Times the tone went silent again *after* having been heard. A muted or
    /// still-reconnecting speaker is silent from the start and never increments
    /// this; a speaker whose stream keeps breaking up does, and the two need
    /// completely different advice — see [`GateReason::Intermittent`].
    lost_after_heard: u32,
}

impl Gate {
    pub fn new(cfg: GateConfig) -> Self {
        Self { cfg, peaks: Vec::new(), restarts: 0, aec_suspected: false, last: None, member: None, heard: false, lost_after_heard: 0 }
    }

    /// Label the gate with the member it is waiting on, for the UI.
    pub fn for_member(mut self, node_name: &str) -> Self {
        self.member = Some(node_name.to_string());
        self
    }

    /// True if the burst amplitude decayed monotonically across the window — the
    /// echo-cancellation signature (plan §4.2). Needs three points: two can decay
    /// by chance.
    pub fn aec_suspected(&self) -> bool {
        self.aec_suspected
    }

    fn progress(&self, locked: bool, periods: usize) -> GateProgress {
        let (waiting_for, message) = match (locked, &self.last) {
            (true, _) => (None, "loop-phase lock acquired with stable amplitude".to_string()),
            (false, Some((r, m))) => (Some(*r), m.clone()),
            (false, None) => (Some(GateReason::Acquiring), "waiting for the first pattern period".to_string()),
        };
        GateProgress {
            locked,
            periods,
            needed: self.cfg.min_periods,
            waiting_for,
            message,
            restarts: self.restarts,
            member: self.member.clone(),
        }
    }

    fn restart(&mut self, reason: GateReason, message: impl Into<String>, elapsed: Duration) -> GateStep {
        self.peaks.clear();
        self.restarts += 1;
        self.last = Some((reason, message.into()));
        let progress = self.progress(false, 0);
        GateStep { locked: false, restart: true, failed: self.timeout(elapsed, reason), progress }
    }

    fn waiting(&mut self, reason: GateReason, message: impl Into<String>, periods: usize, elapsed: Duration) -> GateStep {
        self.last = Some((reason, message.into()));
        let progress = self.progress(false, periods);
        GateStep { locked: false, restart: false, failed: self.timeout(elapsed, reason), progress }
    }

    /// The timeout is checked on every non-locked sample, and reports the reason
    /// the gate was *last* waiting on rather than a bare "timed out".
    fn timeout(&self, elapsed: Duration, reason: GateReason) -> Option<Refusal> {
        if elapsed < self.cfg.timeout {
            return None;
        }
        let detail = self.last.as_ref().map(|(_, m)| m.clone()).unwrap_or_else(|| "nothing was received".to_string());
        let secs = self.cfg.timeout.as_secs();
        let (kind, estimator_reason) = match reason {
            GateReason::MicDisconnected => (RefusalKind::MicLost, None),
            GateReason::MicReconnected => (RefusalKind::MicReconnected, None),
            GateReason::Estimator => (RefusalKind::Estimator, None),
            GateReason::Interference => (RefusalKind::Interference, None),
            _ => (RefusalKind::GateTimeout, None),
        };
        let mut r = Refusal::new(kind, format!("gave up after {secs}s waiting for a stable measurement: {detail}"));
        r.member = self.member.clone();
        r.estimator_reason = estimator_reason;
        Some(r)
    }

    /// Feed one completed pattern period.
    pub fn observe(&mut self, s: &GateSample) -> GateStep {
        if !s.connected {
            return self.restart(
                GateReason::MicDisconnected,
                "the microphone capture is not connected — reopen it on the phone (the alignment session is still running)",
                s.elapsed,
            );
        }
        if s.reconnected {
            return self.restart(
                GateReason::MicReconnected,
                "the microphone capture reconnected, which restarts its frame counter and with it the timing reference — \
                 everything measured before it has to be discarded",
                s.elapsed,
            );
        }
        if s.gap {
            return self.restart(
                GateReason::SequenceGap,
                "the microphone stream dropped a block, so arrival times across it are shifted by an unknown amount — \
                 keep the phone's screen on and its browser tab in front",
                s.elapsed,
            );
        }
        if s.clipped {
            return self.restart(
                GateReason::Clipped,
                "the microphone clipped; clipping is broadband, so it corrupts every measurement channel at once — \
                 lower the calibration level or move the phone back",
                s.elapsed,
            );
        }
        if let Some(reason) = &s.interference {
            // First, deliberately: an announcement playing over the calibration tone
            // changes the level, so every check below would fire with a wrong
            // explanation.
            return self.restart(GateReason::Interference, reason.clone(), s.elapsed);
        }
        if s.peak < GATE_SILENCE_PEAK {
            if self.heard {
                self.lost_after_heard += 1;
            }
            // Silent *from the start* and silent *after being heard* are different
            // faults with different remedies, so they get different reasons. One
            // dropout can be a mute settling; a second means the stream itself is
            // not continuous.
            if self.lost_after_heard >= 2 {
                return self.restart(
                    GateReason::Intermittent,
                    "this speaker's tone keeps stopping and starting, so its audio stream is not continuous — \
                     nothing about the room or the phone can fix that. Reconnect the speaker (nudging its static delay \
                     up and back down forces it) and check that its wire codec keeps up, then measure again",
                    s.elapsed,
                );
            }
            return self.restart(
                GateReason::Silent,
                "no tone from this speaker reached the microphone — it may be muted, disconnected, or still reconnecting \
                 after a delay change",
                s.elapsed,
            );
        }

        self.heard = true;
        self.peaks.push(s.peak);
        if let Some(spread) = amplitude_spread_db(&self.peaks) {
            if spread > self.cfg.amp_tolerance_db {
                let tol = self.cfg.amp_tolerance_db;
                return self.restart(
                    GateReason::UnstableAmplitude,
                    format!(
                        "the tone's level moved by {spread:.1} dB across the last {} pattern periods (limit {tol:.1} dB) — \
                         either a mute has not settled yet or the phone is moving; hold it still, or put it down",
                        self.peaks.len()
                    ),
                    s.elapsed,
                );
            }
            if monotonic_decay_db(&self.peaks).is_some_and(|d| d > GATE_AEC_DECAY_DB) {
                self.aec_suspected = true;
                return self.restart(
                    GateReason::AecSuspected,
                    "the tone got quieter every pattern period, which is what echo cancellation looks like as it \
                     converges — it is designed to remove exactly the signal being measured. Turn it off in the browser \
                     (or use a different browser); this measurement cannot be trusted while it is on",
                    s.elapsed,
                );
            }
        }

        if s.periods_used < self.cfg.min_periods {
            let need = self.cfg.min_periods;
            let have = s.periods_used;
            return self.waiting(
                GateReason::Acquiring,
                format!("acquiring loop-phase lock ({have}/{need} pattern periods)"),
                have,
                s.elapsed,
            );
        }
        match &s.quality {
            Quality::Rejected { reason, message } => {
                let mut step = self.waiting(GateReason::Estimator, message.clone(), s.periods_used, s.elapsed);
                if let Some(failed) = step.failed.as_mut() {
                    failed.estimator_reason = Some(*reason);
                }
                step
            }
            Quality::Accepted => {
                self.last = None;
                let progress = self.progress(true, s.periods_used);
                GateStep { locked: true, restart: false, failed: None, progress }
            }
        }
    }
}

/// Peak-to-peak spread of a series of amplitudes, in dB. `None` until there are
/// two points; `INFINITY` if anything is silent (the caller has already treated
/// silence as its own case).
fn amplitude_spread_db(peaks: &[f32]) -> Option<f64> {
    if peaks.len() < 2 {
        return None;
    }
    let max = peaks.iter().copied().fold(f32::MIN, f32::max) as f64;
    let min = peaks.iter().copied().fold(f32::MAX, f32::min) as f64;
    if min <= 0.0 {
        return Some(f64::INFINITY);
    }
    Some(20.0 * (max / min).log10())
}

/// Total decay in dB if the series is *strictly* decreasing over at least three
/// points, else `None`. A consistent direction is the AEC signature; jitter of
/// the same size is not.
fn monotonic_decay_db(peaks: &[f32]) -> Option<f64> {
    if peaks.len() < 3 {
        return None;
    }
    if !peaks.windows(2).all(|w| w[1] < w[0]) {
        return None;
    }
    let first = f64::from(peaks[0]);
    let last = f64::from(peaks[peaks.len() - 1]);
    if last <= 0.0 {
        return Some(f64::INFINITY);
    }
    Some(20.0 * (first / last).log10())
}

// ---------------------------------------------------------------- solve (§9)

/// Wrap a difference into ±half a period.
fn wrap_sym(d: f64, period: f64) -> f64 {
    d - period * (d / period).round()
}

/// A common-drift fit: per-member phase offsets on one shared origin, plus the
/// one drift slope they share.
#[derive(Debug, Clone)]
pub struct DriftFit {
    /// Offset at period 0 per member, ms (unwrapped; only differences are
    /// meaningful).
    pub offsets: HashMap<String, f64>,
    /// ms of phase per pattern period.
    pub slope_ms_per_period: f64,
    /// Whether the slope could be fitted at all (it needs a member measured in
    /// two different passes).
    pub fitted: bool,
}

impl DriftFit {
    pub fn drift_ppm(&self, pattern_ms: f64) -> f64 {
        if pattern_ms <= 0.0 {
            return 0.0;
        }
        self.slope_ms_per_period / pattern_ms * 1e6
    }
}

/// Fit one drift slope shared by every member, and each member's offset.
///
/// The mic clock is not the audio clock, so every member's measured phase creeps
/// at the same rate; with sequential soloing the members are measured *at
/// different times*, so an uncorrected creep looks exactly like a real offset.
/// Alternating the pass order (plan §6.1) makes the error average out; fitting the
/// slope explicitly removes it, and the two together are why a 100 ppm phone
/// clock does not silently cost several milliseconds over a minute-long run.
///
/// The slope is pooled across members (each member contributes its own
/// deviations from its own mean), which is the only way to see it at all when
/// each member has just two observations.
pub fn fit_drift(obs: &[MemberObservation], pattern_ms: f64, phase: impl Fn(&MemberObservation) -> f64) -> DriftFit {
    let mut by_member: HashMap<&str, Vec<(f64, f64)>> = HashMap::new();
    for o in obs {
        let entry = by_member.entry(o.node_name.as_str()).or_default();
        // Unwrap against this member's first observation, so a member whose phase
        // sits near the period boundary does not average to the far side.
        let base = entry.first().map_or_else(|| phase(o), |(_, y)| *y);
        entry.push((o.period_centre, base + wrap_sym(phase(o) - base, pattern_ms)));
    }
    let mut num = 0.0;
    let mut den = 0.0;
    for series in by_member.values() {
        let n = series.len() as f64;
        let xbar = series.iter().map(|(x, _)| x).sum::<f64>() / n;
        let ybar = series.iter().map(|(_, y)| y).sum::<f64>() / n;
        for (x, y) in series {
            num += (x - xbar) * (y - ybar);
            den += (x - xbar) * (x - xbar);
        }
    }
    let fitted = den > 0.0;
    let slope = if fitted { num / den } else { 0.0 };
    let offsets = by_member
        .into_iter()
        .map(|(name, series)| {
            let n = series.len() as f64;
            let xbar = series.iter().map(|(x, _)| x).sum::<f64>() / n;
            let ybar = series.iter().map(|(_, y)| y).sum::<f64>() / n;
            (name.to_string(), ybar - slope * xbar)
        })
        .collect();
    DriftFit { offsets, slope_ms_per_period: slope, fitted }
}

/// Turn circular offsets into a linear arrival ordering with the earliest member
/// at 0.
///
/// Phases wrap at the pattern period, so "later" only means anything relative to
/// an anchor. Differences are wrapped into ±½ period against `order[0]`, then the
/// whole set is shifted so the minimum is 0 — which is also what makes
/// [`MAX_TRUSTED_SPREAD_MS`] a meaningful check: if the true spread exceeded half
/// a period, this is where it would have silently folded.
pub fn linearise(offsets: &HashMap<String, f64>, order: &[String], pattern_ms: f64) -> Vec<(String, f64)> {
    let Some(anchor) = order.iter().find_map(|n| offsets.get(n).copied()) else {
        return Vec::new();
    };
    let mut out: Vec<(String, f64)> =
        order.iter().filter_map(|n| offsets.get(n).map(|v| (n.clone(), wrap_sym(v - anchor, pattern_ms)))).collect();
    let min = out.iter().map(|(_, d)| *d).fold(f64::INFINITY, f64::min);
    for (_, d) in &mut out {
        *d -= min;
    }
    out
}

/// §10.2's transitivity check, in the only form a single mic position can
/// actually realise.
///
/// **The plan's literal formulation is arithmetically vacuous here, and this is
/// worth stating plainly.** §10.2 says: align B and C against A, then measure B
/// against C and expect ~0. But every phase in this design is read off *one*
/// shared grid (see the module docs), so `d(B,C)` is by construction
/// `d(A,C) − d(A,B)`: the triangle closes exactly, whatever the per-speaker bias.
/// No arrangement of A-referenced measurements can expose a per-speaker constant.
///
/// What *is* independent is the **frequency band**. The click track carries two
/// bursts, 3 kHz and 1.5 kHz, and every soloed member emits both (plan §2.2), so
/// each pair's delta is measured twice over two different bands. An early
/// reflection — the §5.6 blind spot — arrives at a fixed *delay*, so its
/// interference with the direct sound is strongly frequency dependent: it biases
/// the two bands differently. Closing the triangle with edges from different
/// bands therefore has a non-zero residual exactly when a *per-speaker*
/// band-dependent bias exists, and cancels when the bias is *shared* by every
/// speaker — which is precisely the discrimination §5.6 asks for.
///
/// The residual reduces to `|split_i − split_j|` where
/// `split_i = phase_B(i) − phase_A(i) − 1000 ms`.
///
/// **Confound, and why the tolerance is 3 ms rather than 0.1 ms.** A loudspeaker
/// crossover genuinely delays 1.5 kHz and 3 kHz differently — often by a
/// millisecond or two, and differently for different models. In a mixed group
/// that legitimate difference is *indistinguishable* from a reflection bias by
/// this check. So the tolerance is set where it catches gross per-speaker bias
/// without refusing every mixed-model group, and a pass here is **not** proof
/// that §5.6 did not happen. Only W9 (chirp + matched filter) can resolve the
/// direct arrival from an early reflection properly.
pub fn transitivity(obs: &[MemberObservation], timing: &Timing, tolerance_ms: f64) -> TransitivityCheck {
    let pattern_ms = timing.pattern_ms;
    let mut splits: Vec<(String, f64)> = Vec::new();
    let mut by_member: HashMap<&str, Vec<f64>> = HashMap::new();
    for o in obs {
        by_member
            .entry(o.node_name.as_str())
            .or_default()
            .push(wrap_sym(o.m.phase_b_ms - o.m.phase_a_ms - timing.nominal_ab_ms(), pattern_ms));
    }
    for o in obs {
        if splits.iter().any(|(n, _)| n == &o.node_name) {
            continue;
        }
        let v = &by_member[o.node_name.as_str()];
        splits.push((o.node_name.clone(), v.iter().sum::<f64>() / v.len() as f64));
    }
    let mut worst = 0.0;
    let mut worst_pair = None;
    for (i, (a, sa)) in splits.iter().enumerate() {
        for (b, sb) in splits.iter().skip(i + 1) {
            let d = (sa - sb).abs();
            if d > worst {
                worst = d;
                worst_pair = Some((a.clone(), b.clone()));
            }
        }
    }
    TransitivityCheck {
        worst_pair,
        worst_ms: worst,
        tolerance_ms,
        passed: worst <= tolerance_ms,
        caveat: "measured as cross-band agreement, the only independent pairing a single mic position offers; a pass does \
                 not rule out an early-reflection bias shared by both bands, and a loudspeaker crossover can fail it \
                 legitimately (plan §5.6)",
    }
}

/// Compare a near-field walk's two readings of its first speaker — see
/// [`ClosureReport`] for what this does and does not establish.
///
/// Both readings are of the same member with the same knob and the same content, so
/// on one continuous capture (plan §1.2) the difference has no legitimate source
/// except the two clocks running at different rates. The check is on the *rate* the
/// difference implies, not on its size, because that is the axis on which drift and
/// movement differ.
pub fn closure_report(first: &MemberObservation, again: &MemberObservation, timing: &Timing) -> ClosureReport {
    let pattern_ms = timing.pattern_ms;
    let error_ms = wrap_sym(again.m.phase_a_ms - first.m.phase_a_ms, pattern_ms);
    let span_periods = again.period_centre - first.period_centre;
    let span_s = span_periods * pattern_ms / 1000.0;
    // A non-positive span means the two readings are not separated in time at all, so
    // there is no baseline: report an infinite implied rate rather than dividing by
    // ~0 and printing a plausible-looking number.
    let drift_ppm = if span_periods > 0.0 { error_ms / (span_periods * pattern_ms) * 1e6 } else { f64::INFINITY };
    let tolerance_ms = MIN_CLOSURE_TOL_MS.max(span_s * MAX_CLOSURE_DRIFT_PPM / 1000.0);
    ClosureReport {
        anchor: first.node_name.clone(),
        error_ms,
        span_periods,
        span_s,
        drift_ppm,
        tolerance_ms,
        passed: span_periods > 0.0 && error_ms.abs() <= tolerance_ms,
        caveat: "this is accumulated mic-vs-audio clock drift plus anything that genuinely changed between the two readings — a \
                 speaker that was moved, or the phone held at a different distance on the way back. The two cannot be told apart \
                 here; what is refused is a difference too large for any plausible clock, and a slow real change inside that bound \
                 is spread across the group instead of being caught",
    }
}

/// Pass-to-pass agreement, after the common drift has been removed. This is what
/// catches the user having moved between passes — which biases a member without
/// making any single pass look bad.
pub fn repeatability(obs: &[MemberObservation], fit: &DriftFit, pattern_ms: f64, tolerance_ms: f64) -> Option<RepeatabilityCheck> {
    let mut by_member: HashMap<&str, Vec<f64>> = HashMap::new();
    for o in obs {
        by_member.entry(o.node_name.as_str()).or_default().push(o.m.phase_a_ms - fit.slope_ms_per_period * o.period_centre);
    }
    let mut worst = 0.0;
    let mut worst_member = None;
    let mut any = false;
    for (name, values) in &by_member {
        if values.len() < 2 {
            continue;
        }
        any = true;
        let base = values[0];
        let spread = values
            .iter()
            .map(|v| wrap_sym(v - base, pattern_ms))
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)));
        let d = spread.1 - spread.0;
        if d > worst {
            worst = d;
            worst_member = Some((*name).to_string());
        }
    }
    if !any {
        return None;
    }
    Some(RepeatabilityCheck { worst_member, worst_ms: worst, tolerance_ms, passed: worst <= tolerance_ms })
}

// ------------------------------------------------- the feasible-interval model

/// One member as the solver sees it: where it arrives now, which way its knob can
/// move it, and how far (plan §2.4.2).
#[derive(Debug, Clone)]
pub struct MemberInterval {
    pub node_name: String,
    pub kind: MemberKind,
    pub knob: Knob,
    /// The knob's value right now.
    pub current_ms: u16,
    /// Measured arrival **with the current knob value**, ms on the linearised
    /// scale (`τ_i`, earliest measured member at 0).
    pub arrival_ms: f64,
    /// Where this member would arrive with its knob at zero — the *intrinsic*
    /// arrival: `τ_i + a_i` for an advance, `τ_i − d_i` for a delay. Knob changes
    /// do not move it, which is why an infeasible group cannot be fixed by
    /// pre-setting knobs (see [`choose_target`]).
    pub base_ms: f64,
    /// Earliest arrival this member can be given.
    pub lo_ms: f64,
    /// Latest arrival this member can be given.
    pub hi_ms: f64,
}

impl MemberInterval {
    fn new(node_name: String, kind: MemberKind, current_ms: u16, arrival_ms: f64) -> Self {
        let knob = knob_of(kind);
        let current = f64::from(current_ms);
        let (min, max) = (f64::from(knob.min_ms), f64::from(knob.max_ms));
        // Knob at zero, then the reachable window either side of it.
        let (base_ms, lo_ms, hi_ms) = match knob.polarity {
            // `τ_i + a_i − a` for `a ∈ [min, max]`: bigger knob, earlier arrival.
            KnobPolarity::Advance => {
                let base = arrival_ms + current;
                (base, base - max, base - min)
            }
            // `τ_i − d_i + d` for `d ∈ [min, max]`: bigger knob, later arrival.
            KnobPolarity::Delay => {
                let base = arrival_ms - current;
                (base, base + min, base + max)
            }
        };
        Self { node_name, kind, knob, current_ms, arrival_ms, base_ms, lo_ms, hi_ms }
    }

    /// The knob value that puts this member at `target`, before rounding. Inside
    /// `[knob.min_ms, knob.max_ms]` for any `target` in `[lo_ms, hi_ms]`.
    fn knob_for(&self, target: f64) -> f64 {
        match self.knob.polarity {
            KnobPolarity::Advance => self.base_ms - target,
            KnobPolarity::Delay => target - self.base_ms,
        }
    }

    /// "advance 12 ms (was 0 ms) — 12 ms earlier than it plays now".
    fn describe(&self, new_ms: u16) -> String {
        let noun = self.knob.polarity.noun();
        let moved = i32::from(new_ms) - i32::from(self.current_ms);
        if moved == 0 {
            return format!("{noun} stays at {} ms", self.current_ms);
        }
        let dir = if moved > 0 { self.knob.polarity.direction() } else { KnobPolarity::opposite(self.knob.polarity).direction() };
        format!("{noun} {new_ms} ms (was {} ms) — plays {} ms {dir}", self.current_ms, moved.abs())
    }
}

/// Build one [`MemberInterval`] per arrival, from the member's kind and its current
/// knob value.
///
/// Shared by the single-position solve and the chain's final renormalisation, because
/// §2.4.2's model is the same in both — what differs is only where the arrivals came
/// from (measured at one spot, or synthesised from the chain's provisional delays; see
/// [`solve_chain`]).
fn intervals_for(arrivals: &[(String, f64)], members: &[SessionMember], current_delays: &HashMap<String, u16>) -> Vec<MemberInterval> {
    let kinds: HashMap<&str, MemberKind> = members.iter().map(|m| (m.node_name.as_str(), m.kind)).collect();
    arrivals
        .iter()
        .map(|(name, arrival)| {
            MemberInterval::new(
                name.clone(),
                kinds.get(name.as_str()).copied().unwrap_or(MemberKind::Sendspin),
                current_delays.get(name).copied().unwrap_or(0),
                *arrival,
            )
        })
        .collect()
}

/// The common arrival the group will be moved to, and the interval it came from.
#[derive(Debug, Clone, Copy)]
pub struct KnobSolution {
    pub target_ms: f64,
    pub lo_ms: f64,
    pub hi_ms: f64,
    /// The largest knob value `target_ms` implies, before rounding. This is the
    /// quantity that was minimised.
    pub largest_knob_ms: f64,
}

/// Plan §2.4.2: intersect every member's achievable arrivals and pick the target
/// inside that minimises the largest knob value.
///
/// **Why an intersection and not a reference member.** Plan §9.1 said the
/// reference must be the latest-arriving member because "the knobs only add
/// delay". Half of that is false: a sendspin advance only moves a speaker
/// *earlier* (§2.4.1). So each member reaches a bounded *interval* of arrivals,
/// the group can only be aligned at a `T` inside all of them, and which member
/// ends up at knob zero falls out of the arithmetic rather than being chosen.
///
/// **The target choice.** `knob_i(T)` falls with `T` for an advance and rises with
/// it for a delay, so `max_i knob_i(T)` is convex and piecewise linear with slopes
/// ±1. With advances `A` and delays `D`:
///
/// * `D` empty → the maximum falls monotonically, so `T = hi` — for a
///   sendspin-only group that is exactly `min_i(τ_i + a_i)`, the earliest
///   *intrinsic* arrival, which takes advance 0 while everyone else is advanced to
///   meet it. The inversion of §9.1, and the case most likely to regress.
/// * `A` empty → the maximum rises monotonically, so `T = lo`: the latest
///   intrinsic arrival (or a pw-sink floor, whichever is later) takes the smallest
///   delay.
/// * both → the two arms cross at `(max_A base + min_D base) / 2`, clamped into
///   the feasible interval.
///
/// **Why refusing is not defensive.** An advance-polarity member can never be made
/// to arrive later than its intrinsic time and a delay-polarity one never earlier,
/// and no knob moves `base_ms`. So a group whose sendspin member is intrinsically
/// earlier than its AP2 member (plus any pw-sink floor) has an empty intersection
/// and **cannot be aligned by writing knobs at all** — the honest answer is to say
/// which two members and by how much they miss, not to write a best effort.
pub fn choose_target(intervals: &[MemberInterval]) -> Result<KnobSolution, Refusal> {
    let mut latest_lo: Option<&MemberInterval> = None;
    let mut earliest_hi: Option<&MemberInterval> = None;
    for iv in intervals {
        if latest_lo.is_none_or(|b| iv.lo_ms > b.lo_ms) {
            latest_lo = Some(iv);
        }
        if earliest_hi.is_none_or(|b| iv.hi_ms < b.hi_ms) {
            earliest_hi = Some(iv);
        }
    }
    let (Some(lo_member), Some(hi_member)) = (latest_lo, earliest_hi) else {
        return Err(Refusal::new(RefusalKind::Internal, "no members to solve"));
    };
    let (lo, hi) = (lo_member.lo_ms, hi_member.hi_ms);
    if lo > hi {
        return Err(infeasible(lo_member, hi_member));
    }
    debug_assert!(intervals.iter().all(|iv| iv.lo_ms <= hi && iv.hi_ms >= lo));

    let advance_base =
        intervals.iter().filter(|iv| iv.knob.polarity == KnobPolarity::Advance).map(|iv| iv.base_ms).fold(f64::NEG_INFINITY, f64::max);
    let delay_base =
        intervals.iter().filter(|iv| iv.knob.polarity == KnobPolarity::Delay).map(|iv| iv.base_ms).fold(f64::INFINITY, f64::min);
    let target = match (advance_base.is_finite(), delay_base.is_finite()) {
        // Advances only: the biggest advance shrinks as the target moves later.
        (true, false) => hi,
        // Delays only: the biggest delay shrinks as the target moves earlier.
        (false, true) => lo,
        // Both: the rising and falling arms cross halfway between the extremes.
        (true, true) => ((advance_base + delay_base) / 2.0).clamp(lo, hi),
        (false, false) => return Err(Refusal::new(RefusalKind::Internal, "no members to solve")),
    };
    let largest_knob_ms = intervals.iter().map(|iv| iv.knob_for(target)).fold(0.0, f64::max);
    Ok(KnobSolution { target_ms: target, lo_ms: lo, hi_ms: hi, largest_knob_ms })
}

/// The refusal for an empty intersection: which two members, which way each can
/// move, how far apart they stay, and what actually helps.
///
/// `floor` is the member that cannot be placed early enough — its achievable
/// interval *starts* after everyone else's — and `ceiling` the one that cannot be
/// placed late enough. Naming both is the point: one name alone would leave the
/// user adjusting the wrong speaker.
fn infeasible(floor: &MemberInterval, ceiling: &MemberInterval) -> Refusal {
    let gap = floor.lo_ms - ceiling.hi_ms;
    // Both names are in the sentence; the machine-readable `member` carries the one
    // whose *ceiling* is the binding constraint, so a UI that highlights one row
    // highlights the speaker that cannot be moved far enough.
    Refusal::for_member(
        RefusalKind::KnobRange,
        &ceiling.node_name,
        format!(
            "'{}' and '{}' cannot be made to arrive together, and no knob setting changes that. '{}' can only be placed between \
             {:.0} and {:.0} ms (its {} knob spans {}–{} ms, and raising it only ever moves it {}), while '{}' can only reach \
             {:.0} to {:.0} ms (its {} knob spans {}–{} ms, {}) — the two ranges miss each other by {gap:.0} ms. Changing either \
             knob slides that member along its own range without moving the range, so this cannot be settled from here: move one \
             of the two speakers (or the listening position), take one of them out of the group, or align this group by ear.",
            floor.node_name,
            ceiling.node_name,
            floor.node_name,
            floor.lo_ms,
            floor.hi_ms,
            floor.knob.polarity.noun(),
            floor.knob.min_ms,
            floor.knob.max_ms,
            floor.knob.polarity.direction(),
            ceiling.node_name,
            ceiling.lo_ms,
            ceiling.hi_ms,
            ceiling.knob.polarity.noun(),
            ceiling.knob.min_ms,
            ceiling.knob.max_ms,
            ceiling.knob.polarity.direction(),
        ),
    )
}

/// The arrival pipeline shared by a single-position solve and one chain step: refuse a
/// set that spans two captures, fit the common drift out, put every member on one
/// linear scale with the earliest at 0, and refuse a spread too close to the wrap.
///
/// Extracted rather than duplicated because a chain runs it **per position** — each
/// position is its own capture-relative set of arrivals — and the chain's arithmetic
/// (plan §1.1) consumes exactly this and nothing else.
struct Arrivals {
    fit: DriftFit,
    /// Earliest member at 0, in `order`.
    linear: Vec<(String, f64)>,
    spread_ms: f64,
    /// Where the reported per-member drift corrections are quoted from: the earliest
    /// reading in the set. A common shift cancels in every difference, so the choice is
    /// presentational — but it has to be *stated*, because "0.4 ms of drift" means
    /// nothing without saying since when.
    drift_origin: f64,
}

fn arrivals_of(observations: &[MemberObservation], order: &[String], timing: &Timing) -> Result<Arrivals, Refusal> {
    let pattern_ms = timing.pattern_ms;
    if let Some(first) = observations.first() {
        if observations.iter().any(|o| o.grid_epoch != first.grid_epoch) {
            return Err(Refusal::new(
                RefusalKind::MicReconnected,
                "the microphone capture restarted during the measurement, so the phases come from two different timing \
                 references and cannot be compared",
            ));
        }
    }
    let fit = fit_drift(observations, pattern_ms, |o| o.m.phase_a_ms);
    let drift_origin = observations.iter().map(|o| o.period_centre).fold(f64::INFINITY, f64::min);
    let linear = linearise(&fit.offsets, order, pattern_ms);
    let spread_ms = linear.iter().map(|(_, d)| *d).fold(0.0, f64::max);
    if spread_ms > pattern_ms * MAX_TRUSTED_SPREAD_FRACTION {
        return Err(Refusal::new(
            RefusalKind::AmbiguousSpread,
            format!(
                "the members' arrivals span {spread_ms:.0} ms, which is too close to half the 2 s test pattern \
                 ({:.0} ms) to be told apart from its own wrap-around — align this group roughly by ear first, then \
                 measure again",
                pattern_ms / 2.0
            ),
        ));
    }
    Ok(Arrivals { fit, linear, spread_ms, drift_origin })
}

impl Arrivals {
    /// How much of one member's phase was attributed to mic-vs-audio clock drift, ms,
    /// relative to [`Self::drift_origin`]. See [`ProposedDelay::drift_correction_ms`].
    fn drift_correction(&self, observations: &[MemberObservation], name: &str) -> f64 {
        let mut n = 0.0;
        let mut sum = 0.0;
        for o in observations.iter().filter(|o| o.node_name == name) {
            sum += o.period_centre - self.drift_origin;
            n += 1.0;
        }
        if n == 0.0 {
            0.0
        } else {
            self.fit.slope_ms_per_period * (sum / n)
        }
    }
}

/// The largest per-member standard error each member was measured with.
fn worst_std_errors(observations: &[MemberObservation]) -> HashMap<String, f64> {
    let mut m: HashMap<String, f64> = HashMap::new();
    for o in observations {
        let e = m.entry(o.node_name.clone()).or_insert(0.0);
        *e = e.max(o.m.std_error_ms);
    }
    m
}

/// The knob half of a proposal: turn a solved target into per-member writes, name the
/// member left at the smallest knob, and refuse a value the rounding pushed out of range.
struct ProposedKnobs {
    members: Vec<ProposedDelay>,
    reference: String,
    largest_knob_ms: u16,
    /// Only the [`KnobPolarity::Advance`] members, before and after — the two sets the
    /// §9.2 high-water check compares.
    current_advances: HashMap<String, u16>,
    proposed_advances: HashMap<String, u16>,
}

fn propose_knobs(
    intervals: &[MemberInterval],
    solution: &KnobSolution,
    std_errors: &HashMap<String, f64>,
    drift_correction: &dyn Fn(&str) -> f64,
) -> Result<ProposedKnobs, Refusal> {
    let mut proposed_advances: HashMap<String, u16> = HashMap::new();
    let mut current_advances: HashMap<String, u16> = HashMap::new();
    let mut members = Vec::new();
    for iv in intervals {
        let exact = iv.knob_for(solution.target_ms);
        // The knobs are integer milliseconds (plan §1.1.2: the write-back, not the
        // estimator, is the precision bottleneck), so this rounds — and a value the
        // rounding pushed outside the range is refused rather than silently clamped
        // into a setting nobody solved for.
        let rounded = exact.round();
        if rounded < f64::from(iv.knob.min_ms) || rounded > f64::from(iv.knob.max_ms) {
            let noun = iv.knob.polarity.noun();
            return Err(Refusal::for_member(
                RefusalKind::KnobRange,
                &iv.node_name,
                format!(
                    "'{}' would need {rounded:.0} ms of {noun}, but its knob only accepts {}–{} ms",
                    iv.node_name, iv.knob.min_ms, iv.knob.max_ms
                ),
            ));
        }
        let new_ms = rounded as u16;
        if iv.knob.polarity == KnobPolarity::Advance {
            proposed_advances.insert(iv.node_name.clone(), new_ms);
            current_advances.insert(iv.node_name.clone(), iv.current_ms);
        }
        members.push(ProposedDelay {
            is_reference: false, // filled in below, once every knob is known
            node_name: iv.node_name.clone(),
            kind: iv.kind,
            arrival_ms: iv.arrival_ms,
            current_delay_ms: iv.current_ms,
            new_delay_ms: new_ms,
            added_ms: i32::from(new_ms) - i32::from(iv.current_ms),
            std_error_ms: std_errors.get(&iv.node_name).copied().unwrap_or(0.0),
            drift_correction_ms: drift_correction(&iv.node_name),
            polarity: iv.knob.polarity,
            knob_min_ms: iv.knob.min_ms,
            knob_max_ms: iv.knob.max_ms,
            achievable_lo_ms: iv.lo_ms,
            achievable_hi_ms: iv.hi_ms,
            effect: iv.describe(new_ms),
        });
    }
    // What the target choice minimised, as it will actually be written. Rounding is
    // monotonic, so this can only differ from the exact optimum by the rounding.
    let largest_knob_ms = members.iter().map(|m| m.new_delay_ms).max().unwrap_or(0);
    debug_assert!((f64::from(largest_knob_ms) - solution.largest_knob_ms).abs() <= 0.5 + 1e-9);

    // The member left with the smallest knob is the one the others were moved
    // towards, and the arrival the post-write residual is measured against.
    let reference = members
        .iter()
        .fold(None::<&ProposedDelay>, |acc, m| match acc {
            Some(best) if best.new_delay_ms <= m.new_delay_ms => acc,
            _ => Some(m),
        })
        .map(|m| m.node_name.clone())
        .ok_or_else(|| Refusal::new(RefusalKind::Internal, "no members to solve"))?;
    for m in &mut members {
        m.is_reference = m.node_name == reference;
    }
    Ok(ProposedKnobs { members, reference, largest_knob_ms, current_advances, proposed_advances })
}

/// §9.2's other half: warn before a write lifts the group's send-ahead high-water
/// mark, because that reconfigures every member's stream instead of reconnecting one
/// device.
///
/// After §2.4.1 the quantity that lifts it is an **advance** — a sendspin device plays
/// its static delay early, so the lead has to cover it — which is why this compares
/// advances and not delays. And per §1.1.2's operational asymmetry the provisional
/// delays never feed the mark at all, so for a chain this is the *first* time the mark
/// is checked: before the write, not after.
fn send_ahead_warning(ctx: &SendAheadContext, current: &HashMap<String, u16>, proposed: &HashMap<String, u16>) -> Option<Warning> {
    let before = ctx.mark_ms(current);
    let after = ctx.mark_ms(proposed);
    (after > before).then(|| {
        Warning::new(
            WarningKind::SendAheadHighWater,
            format!(
                "these settings raise the group's send-ahead from {before} ms to {after} ms, which reconfigures the whole \
                 group's stream — every speaker in it goes quiet for tens of seconds, not just the ones being changed. It is \
                 the sendspin advances that do this: a device plays its static delay early, so the group's lead has to cover it"
            ),
        )
    })
}

/// Everything [`solve`] needs. Pure data, so the whole §9 arithmetic is testable
/// without a mic, a session or a runtime.
pub struct SolveInput<'a> {
    pub timing: Timing,
    pub members: &'a [SessionMember],
    pub observations: &'a [MemberObservation],
    pub current_delays: &'a HashMap<String, u16>,
    pub send_ahead: &'a SendAheadContext,
    /// Near field's closure measurement, when the arrivals came from a walk.
    ///
    /// `None` is a multi-position run, and everything about the solve is then exactly
    /// as it was before W8a. `Some` changes three reporting decisions and no
    /// arithmetic: the drift slope came from the closure rather than from a second
    /// pass (so [`WarningKind::NoDriftFit`] cannot apply), the repeatability check
    /// would be vacuous (see [`Checks::repeatability`]), and the closure itself is
    /// reported as the check that replaced it.
    pub closure: Option<ClosureReport>,
}

/// Plan §9, as §2.4.2 rewrote it: linearise the arrivals, intersect every
/// member's achievable-arrival interval, take the target that minimises the
/// largest knob, and run the pre-write checks.
///
/// **This is not "delay everyone towards the latest member".** A sendspin knob is
/// an advance (§2.4.1), so a sendspin-only group is aligned to its *earliest*
/// intrinsic arrival and every other member is advanced to meet it — the inversion
/// of what §9.1 says, and the reason the reference is now an output rather than an
/// input.
///
/// Returns a [`Proposal`] even when a check blocks the write — the numbers and the
/// reason are shown together, because a green residual with a failed transitivity
/// check is the interesting failure (plan §10) and hiding it would be the one
/// thing this feature must not do.
pub fn solve(input: &SolveInput<'_>) -> Result<Proposal, Refusal> {
    let pattern_ms = input.timing.pattern_ms;
    let order: Vec<String> = input.members.iter().map(|m| m.node_name.clone()).collect();
    for m in input.members {
        if !input.observations.iter().any(|o| o.node_name == m.node_name) {
            return Err(Refusal::for_member(
                RefusalKind::Internal,
                &m.node_name,
                format!("'{}' was never measured, so the group cannot be solved", m.node_name),
            ));
        }
    }
    let mut warnings = Vec::new();
    let arrivals = arrivals_of(input.observations, &order, &input.timing)?;
    let (fit, spread_ms) = (&arrivals.fit, arrivals.spread_ms);
    if !fit.fitted {
        warnings.push(Warning::new(
            WarningKind::NoDriftFit,
            "the mic-vs-audio clock drift could not be fitted (every member was measured only once), so no drift \
             correction was applied",
        ));
    }

    // §2.4.2: model each member's knob as a polarity plus a range, intersect the
    // achievable arrivals, and take the target that minimises the largest knob.
    // There is no reference member to pick — which member ends at knob zero falls
    // out of the arithmetic (see [`choose_target`]).
    let intervals = intervals_for(&arrivals.linear, input.members, input.current_delays);
    let solution = choose_target(&intervals)?;

    let knobs = propose_knobs(&intervals, &solution, &worst_std_errors(input.observations), &|name| {
        arrivals.drift_correction(input.observations, name)
    })?;
    let ProposedKnobs { members, reference, largest_knob_ms, current_advances, proposed_advances } = knobs;
    warnings.extend(send_ahead_warning(input.send_ahead, &current_advances, &proposed_advances));

    let checks = Checks {
        transitivity: transitivity(input.observations, &input.timing, TRANSITIVITY_TOL_MS),
        // Suppressed for a walk, not skipped for convenience: the closure anchor is
        // the only member with two readings and the slope was fitted from exactly
        // those two, so the check's answer is 0 ms whatever happened. See
        // [`Checks::repeatability`].
        repeatability: match input.closure {
            Some(_) => None,
            None => repeatability(input.observations, fit, pattern_ms, REPEATABILITY_TOL_MS),
        },
        merged_peak: MergedPeakCheck::seam(),
        closure: input.closure.clone(),
    };
    // §10.2 is mandatory and *blocking*: a per-speaker bias that breaks it would
    // otherwise be written into a system the user had aligned by ear.
    let mut blocked = None;
    // Near field's closure comes first, because it invalidates the whole set rather
    // than one member: the drift correction it produces was applied to *every*
    // arrival, so if it is not credible then neither is any number below it. The
    // proposal is still returned — the numbers belong next to the reason (plan §10).
    if let Some(c) = checks.closure.as_ref().filter(|c| !c.passed) {
        blocked = Some(Refusal::for_member(
            RefusalKind::ClosureError,
            &c.anchor,
            format!(
                "'{}' measured {:.2} ms differently at the end of the walk than at the start (limit {:.1} ms over the {:.0} s this \
                 walk took). Over that time the difference implies a {:.0} ppm clock offset, which is more than two clocks do — so \
                 something moved between the two readings rather than drifted: the speaker itself, or the phone held at a different \
                 distance when you came back. That correction was applied to every speaker in the walk, so the whole walk is \
                 discarded, not just this reading. Walk it again, holding the phone the same way at the first speaker both times.",
                c.anchor, c.error_ms, c.tolerance_ms, c.span_s, c.drift_ppm
            ),
        ));
    }
    if blocked.is_none() && !checks.transitivity.passed {
        let (a, b) = checks.transitivity.worst_pair.clone().unwrap_or_default();
        blocked = Some(Refusal::new(
            RefusalKind::Transitivity,
            format!(
                "the two test tones disagree by {:.2} ms about how far apart '{a}' and '{b}' are (limit {:.1} ms). \
                 A speaker's arrival is being pulled by an early reflection, or its crossover delays the two tones \
                 differently — either way the measured offset is not the electrical one, so nothing is written. \
                 Move the phone away from walls and hard surfaces and measure again, or align this group by ear.",
                checks.transitivity.worst_ms, checks.transitivity.tolerance_ms
            ),
        ));
    }
    if let Some(rep) = checks.repeatability.as_ref().filter(|r| !r.passed) {
        if blocked.is_none() {
            let who = rep.worst_member.clone().unwrap_or_default();
            blocked = Some(Refusal::for_member(
                RefusalKind::Repeatability,
                &who,
                format!(
                    "'{who}' measured {:.2} ms differently between the two passes (limit {:.1} ms) — the phone or the \
                     room moved during the measurement. Put the phone down at the listening position and measure again.",
                    rep.worst_ms, rep.tolerance_ms
                ),
            ));
        }
    }

    Ok(Proposal {
        reference,
        pattern_ms,
        spread_ms,
        drift_ppm: fit.drift_ppm(pattern_ms),
        target_ms: solution.target_ms,
        feasible_lo_ms: solution.lo_ms,
        feasible_hi_ms: solution.hi_ms,
        largest_knob_ms,
        members,
        checks,
        warnings,
        blocked,
    })
}

// ------------------------------------------- the chain's arithmetic (plan §1.1)

/// One position of a chain, as the arithmetic sees it. Pure data: the whole of §1.1's
/// algebra is testable without a mic, a session, a relay or a runtime.
pub struct ChainStepInput<'a> {
    /// Every already-aligned member with the **provisional** delay it is carrying, ms.
    /// Empty for the first position.
    pub aligned: &'a HashMap<String, f64>,
    /// This position's arrivals, earliest at 0, from [`arrivals_of`] — the step's new
    /// members *and* its overlaps, all measured in this one capture.
    pub arrivals: &'a [(String, f64)],
    /// Which of [`Self::arrivals`] are overlaps. Must be a subset of
    /// [`Self::aligned`], and must be non-empty for every position after the first.
    pub overlaps: &'a [String],
    pub tolerance_ms: f64,
}

/// What one position decided.
#[derive(Debug, Clone)]
pub struct ChainSolution {
    /// The arrival this position was aligned at, on this step's own scale: the
    /// **latest** of the step's members and the aligned set's anchor.
    pub target_ms: f64,
    /// Where the already-aligned set is judged to arrive here (the mean of the overlap
    /// readings). `None` for the first position.
    pub anchor_ms: Option<f64>,
    /// The common delay every member of the already-aligned set must gain.
    pub delta_ms: f64,
    /// The new provisional delay for every member this step touches: the step's new
    /// members, plus — when `delta_ms > 0` — every member of the aligned set.
    pub provisional: HashMap<String, f64>,
    pub overlaps: Vec<ChainOverlap>,
    pub disagreement_ms: Option<f64>,
    pub worst_pair: Option<(String, String)>,
    pub confidence: OverlapConfidence,
    /// Half the disagreement: how far this joint's common shift can be out.
    pub joint_error_ms: Option<f64>,
    /// The sentence describing what this step did, including its confidence.
    pub note: String,
}

/// Plan §1.1's algebra, in one function.
///
/// **The target is the latest arrival among the step's members**, where "the aligned
/// set" counts as one member arriving at the mean of its overlaps. Each new member is
/// then delayed to that target. If a *new* member arrives later than the aligned set
/// does, the aligned set has to gain Δ — and because **a common delay added to an
/// already-aligned set preserves that set's internal alignment**, Δ goes to *every*
/// member of it, not just to the overlap that was measured. That is the trick the whole
/// feature rests on, and it is the reason the chain can only ever *add* (hence §1.1's
/// ratcheting floor, and hence the global renormalisation in [`solve_chain`]).
///
/// **Why the anchor is the mean of two overlaps rather than one reading.** Two overlaps
/// will not read the same here — they were aligned at the *previous* position and their
/// paths differ at this one (see [`OVERLAP_AGREEMENT_TOL_MS`]) — so where the aligned
/// set "arrives" at this position is ambiguous by their disagreement. The mean is the
/// choice that minimises the worst error, and half the disagreement is reported as this
/// joint's error rather than being rounded to zero.
///
/// **Why disagreement refuses instead of warning.** The shift derived here is applied as
/// a common delay to the entire already-aligned set *and* anchors every position
/// measured afterwards. A §5.6 reflection bias on it therefore propagates to the whole
/// apartment with nothing downstream able to see it. With one overlap there is no
/// redundancy at all at exactly that point; with two there is, and spending it on a
/// warning nobody reads would be the same as not having it.
pub fn chain_step(input: &ChainStepInput<'_>) -> Result<ChainSolution, Refusal> {
    let arrival_of = |n: &str| input.arrivals.iter().find(|(m, _)| m == n).map(|(_, v)| *v);
    let new_members: Vec<(String, f64)> = input.arrivals.iter().filter(|(n, _)| !input.overlaps.iter().any(|o| o == n)).cloned().collect();
    if new_members.is_empty() {
        return Err(Refusal::new(
            RefusalKind::ChainOutOfOrder,
            "this position named no speakers that are not already aligned, so there is nothing for it to align",
        ));
    }

    let mut overlaps = Vec::new();
    for o in input.overlaps {
        let Some(arrival_ms) = arrival_of(o) else {
            return Err(Refusal::for_member(
                RefusalKind::Internal,
                o,
                format!("the overlap '{o}' was not measured at this position, so this step cannot be linked to the aligned set"),
            ));
        };
        let Some(applied_ms) = input.aligned.get(o).copied() else {
            return Err(Refusal::for_member(
                RefusalKind::ChainOutOfOrder,
                o,
                format!("'{o}' was offered as an overlap but has not been aligned at an earlier position, so it links to nothing"),
            ));
        };
        overlaps.push(ChainOverlap { node_name: o.clone(), arrival_ms, applied_ms });
    }

    // The first position defines the chain's reference; every later one has to be tied
    // to it, and the only thing that can tie it is a speaker measured in both places.
    if overlaps.is_empty() && !input.aligned.is_empty() {
        return Err(Refusal::new(
            RefusalKind::OverlapMissing,
            format!(
                "this position named no overlap, so nothing relates it to the {} speaker(s) already aligned — the two sets would each \
                 be internally aligned and mutually meaningless. Name one or two speakers you can hear from *both* places (two is what \
                 lets the step be checked, one is accepted but cannot be), or align this position in a separate run.",
                input.aligned.len()
            ),
        ));
    }

    // The overlaps' worst pairwise disagreement: an independent estimate of this
    // joint's error, and the only check the chain has at its most dangerous point.
    let mut disagreement_ms = None;
    let mut worst_pair = None;
    for (i, a) in overlaps.iter().enumerate() {
        for b in overlaps.iter().skip(i + 1) {
            let d = (a.arrival_ms - b.arrival_ms).abs();
            if disagreement_ms.is_none_or(|w| d > w) {
                disagreement_ms = Some(d);
                worst_pair = Some((a.node_name.clone(), b.node_name.clone()));
            }
        }
    }
    if let (Some(d), Some((a, b))) = (disagreement_ms, worst_pair.clone()) {
        if d > input.tolerance_ms {
            return Err(Refusal::for_member(
                RefusalKind::OverlapDisagreement,
                &a,
                format!(
                    "'{a}' and '{b}' are both already aligned, so from here they should still read as a plausible pair — but they arrive \
                     {d:.1} ms apart at this position, and the limit is {:.0} ms. Two overlaps are *not* expected to read identically: \
                     they were aligned at the previous position, and from this one their paths differ, so a few ms is normal and is \
                     exactly what the limit allows for. {d:.1} ms is more than the geometry of one room explains, so one of the two \
                     readings is wrong — an early reflection the microphone locked onto, an overlap that was not really aligned, or a \
                     speaker that has been moved. This step's shift would be applied to *every* speaker aligned so far, so a wrong \
                     reading here moves the whole apartment: nothing was changed. Pick overlaps you can hear clearly from where you are, \
                     keep the phone away from walls, and measure this position again.",
                    input.tolerance_ms
                ),
            ));
        }
    }

    let anchor_ms = match overlaps.is_empty() {
        true => None,
        false => Some(overlaps.iter().map(|o| o.arrival_ms).sum::<f64>() / overlaps.len() as f64),
    };
    // Target = the latest arrival among the step's members, the aligned set included.
    let latest_new = new_members.iter().map(|(_, a)| *a).fold(f64::NEG_INFINITY, f64::max);
    let target_ms = anchor_ms.map_or(latest_new, |a| a.max(latest_new));
    let delta_ms = anchor_ms.map_or(0.0, |a| (target_ms - a).max(0.0));

    let mut provisional: HashMap<String, f64> = HashMap::new();
    for (name, arrival) in &new_members {
        // ≥ 0 by construction: `target_ms` is at least the latest new arrival, and the
        // relay can only delay (plan §1.1.1 — the chain never needs to advance).
        provisional.insert(name.clone(), (target_ms - arrival).max(0.0));
    }
    if delta_ms > 0.0 {
        // The whole aligned set, not just the overlap that was measured.
        for (name, applied) in input.aligned {
            provisional.insert(name.clone(), applied + delta_ms);
        }
    }

    let confidence = match overlaps.len() {
        0 => OverlapConfidence::Origin,
        1 => OverlapConfidence::Single,
        _ => OverlapConfidence::Checked,
    };
    let joint_error_ms = disagreement_ms.map(|d| d / 2.0);
    let note = chain_note(&overlaps, confidence, disagreement_ms, delta_ms, input.aligned.len());
    Ok(ChainSolution {
        target_ms,
        anchor_ms,
        delta_ms,
        provisional,
        overlaps,
        disagreement_ms,
        worst_pair,
        confidence,
        joint_error_ms,
        note,
    })
}

/// One sentence saying what a step did and how well it could be checked.
fn chain_note(
    overlaps: &[ChainOverlap],
    confidence: OverlapConfidence,
    disagreement_ms: Option<f64>,
    delta_ms: f64,
    aligned: usize,
) -> String {
    let names: Vec<&str> = overlaps.iter().map(|o| o.node_name.as_str()).collect();
    let shift = match delta_ms > 0.005 {
        true => format!(
            "A speaker at this position arrives later than the set aligned so far, so all {aligned} of those speakers gained {delta_ms:.1} \
             ms — a common delay, which is why their alignment with each other survives it. "
        ),
        false => format!("The set aligned so far already arrives last here, so none of its {aligned} speakers had to move. "),
    };
    match confidence {
        OverlapConfidence::Origin => "the first position: these speakers define the chain's reference, and there is nothing yet for them \
             to be checked against."
            .to_string(),
        OverlapConfidence::Single => format!(
            "{shift}Linked through the single overlap '{}'. That one reading is what places this position against everything already \
             aligned, and with only one overlap nothing checks it — this joint is the weakest in the chain.",
            names.first().copied().unwrap_or_default()
        ),
        OverlapConfidence::Checked => format!(
            "{shift}Linked through {} overlaps ({}), which disagree by {:.1} ms here — expected, since they were aligned at the previous \
             position and their paths differ at this one. Half of that, {:.1} ms, is this joint's own error.",
            names.len(),
            names.join(", "),
            disagreement_ms.unwrap_or(0.0),
            disagreement_ms.unwrap_or(0.0) / 2.0
        ),
    }
}

/// What the chain's accumulated error can honestly be said to be (plan §1.1).
///
/// The joints are the *only* thing that is measurable here: each step's two overlaps
/// bound how far its common shift can be out, and those shifts compose, so the worst
/// case is their sum. One single-overlap step anywhere in the chain removes the bound
/// entirely — that joint has no independent estimate at all — and the honest answer is
/// then no total rather than a total with a hole in it.
///
/// What no arrangement of this bounds is the per-speaker bias of §5.6, which is why the
/// message says so instead of letting a small number read as an accuracy claim.
pub fn chain_error(steps: &[ChainStep]) -> ChainError {
    let joints: Vec<&ChainStep> = steps.iter().filter(|s| s.confidence != OverlapConfidence::Origin).collect();
    let unchecked: Vec<String> = joints
        .iter()
        .filter(|s| s.confidence == OverlapConfidence::Single)
        .map(|s| format!("position {} ('{}')", s.index, s.overlaps.first().map(|o| o.node_name.as_str()).unwrap_or("?")))
        .collect();
    if !unchecked.is_empty() {
        return ChainError {
            bounded: false,
            joint_ms: None,
            message: format!(
                "the chain's accumulated error cannot be bounded: {} was linked through a single overlap, so that joint has no \
                 independent error estimate — nothing here can say how far its common shift is out, and every position aligned after it \
                 inherits the same unknown. No total is given, because a total that quietly left out the one joint it could not measure \
                 would be worse than none.",
                unchecked.join(" and ")
            ),
        };
    }
    if joints.is_empty() {
        return ChainError {
            bounded: true,
            joint_ms: Some(0.0),
            message: "one position, so there are no joints between positions to accumulate error across. What is left is the ordinary \
                      single-position result: aligned at the spot it was measured from."
                .to_string(),
        };
    }
    let total: f64 = joints.iter().filter_map(|s| s.joint_error_ms).sum();
    let each: Vec<String> = joints.iter().map(|s| format!("position {}: {:.1} ms", s.index, s.joint_error_ms.unwrap_or(0.0))).collect();
    ChainError {
        bounded: true,
        joint_ms: Some(total),
        message: format!(
            "every joint was checked by two overlaps, and their disagreements bound how far each joint's common shift can be out ({}). \
             Composed, the worst these joints can be wrong by is {total:.1} ms — that is the error *between* regions, i.e. how far a \
             speaker aligned at one position can be out relative to one aligned at another. It says nothing about per-speaker bias: an \
             early reflection inside the analysis window (plan §5.6) biases a speaker by 1–2 ms while every check in this design still \
             passes, and that is not included here because nothing here can measure it.",
            each.join("; ")
        ),
    }
}

/// The chain's final solve: renormalise globally, then write once (plan §1.1, §2.4.2).
pub struct ChainSolveInput<'a> {
    pub timing: Timing,
    /// Every held member — the chain must have aligned all of them.
    pub members: &'a [SessionMember],
    /// Each member's provisional delay at the end of the chain, ms.
    pub provisional: &'a HashMap<String, f64>,
    pub current_delays: &'a HashMap<String, u16>,
    pub send_ahead: &'a SendAheadContext,
    /// The positions, in order — for the aggregate checks and the error statement.
    pub steps: &'a [ChainStep],
    /// Every reading the chain took, for the per-member standard errors.
    pub observations: &'a [MemberObservation],
}

/// Turn the finished chain into the single write (plan §1.1's "renormalise globally at
/// the end", through §2.4.2's solver).
///
/// **Why the renormalisation is the interval solver and not a subtraction.** §1.1 says
/// "subtract the global minimum from every speaker", and that is right for a set of pure
/// delays — but the *knobs* are not pure delays: a sendspin knob is an advance (§2.4.1),
/// so with mixed polarities "the minimum" is not a delay anyone can subtract, and the
/// free common shift has to be chosen inside the intersection of what every member's
/// knob can reach. That is exactly [`choose_target`]'s job, so this hands the chain to
/// it rather than reimplementing normalisation beside it.
///
/// **The one line that makes that possible.** A member the chain gave `pᵢ` of provisional
/// delay to must, after the write, arrive `pᵢ` later than it did at chain start. The
/// chain aligned them, so they all arrive *together* afterwards — which means at chain
/// start they arrived at `max(p) − pᵢ`. Feeding that as the "measured arrival" makes the
/// solver's own target the free common shift: `knobᵢ(T) = dᵢ + pᵢ + (T − max p)` for a
/// delay and `aᵢ − pᵢ − (T − max p)` for an advance, so picking `T` *is* renormalising,
/// and it picks the one that keeps the largest knob smallest (§9.2). A sendspin-only
/// chain therefore still lands on its earliest member with advance 0.
///
/// The arrivals are **not** acoustic here and the field says so: they are the chain's
/// own bookkeeping, and the last position's overlaps are the only place a single capture
/// ever saw two regions at once.
pub fn solve_chain(input: &ChainSolveInput<'_>) -> Result<Proposal, Refusal> {
    for m in input.members {
        if !input.provisional.contains_key(&m.node_name) {
            return Err(Refusal::for_member(
                RefusalKind::ChainOutOfOrder,
                &m.node_name,
                format!("'{}' was not aligned at any position, so the chain cannot be written", m.node_name),
            ));
        }
    }
    let floor = input.provisional.values().copied().fold(f64::INFINITY, f64::min);
    let ceiling = input.provisional.values().copied().fold(f64::NEG_INFINITY, f64::max);
    if !floor.is_finite() || !ceiling.is_finite() {
        return Err(Refusal::new(RefusalKind::Internal, "the chain aligned no speakers, so there is nothing to write"));
    }
    // "Arrival at chain start" per the doc comment above: the member that needed the
    // most provisional delay is the one that arrived earliest.
    let arrivals: Vec<(String, f64)> = input
        .members
        .iter()
        .map(|m| (m.node_name.clone(), ceiling - input.provisional.get(&m.node_name).copied().unwrap_or(0.0)))
        .collect();
    let intervals = intervals_for(&arrivals, input.members, input.current_delays);
    let solution = choose_target(&intervals)?;
    let std_errors = worst_std_errors(input.observations);
    let knobs = propose_knobs(&intervals, &solution, &std_errors, &|_| 0.0)?;
    let ProposedKnobs { members, reference, largest_knob_ms, current_advances, proposed_advances } = knobs;

    let mut warnings = Vec::new();
    // §1.1.2's operational asymmetry: the provisional delays never feed
    // `required_send_ahead_us`, so the walk cannot feel the mark approaching. This is
    // the first and only time the chain can check it — before the write, not after.
    warnings.extend(send_ahead_warning(input.send_ahead, &current_advances, &proposed_advances));
    if let Some(one) = input.steps.iter().find(|s| s.confidence == OverlapConfidence::Single) {
        warnings.push(Warning::new(
            WarningKind::OneOverlap,
            format!(
                "position {} is linked to the rest of the chain through the single overlap '{}'. That one reading was applied as a common \
                 shift to every speaker aligned before it and anchors everything aligned after it, and with one overlap there is nothing \
                 to check it against — a reflection biasing it (plan §5.6) would move the whole chain with no check here noticing. Where \
                 the room allows it, use two shared speakers.",
                one.index,
                one.overlaps.first().map(|o| o.node_name.as_str()).unwrap_or("?")
            ),
        ));
    }
    warnings.push(Warning::new(WarningKind::ChainScope, CHAIN_SCOPE_NOTE));

    // The aggregate checks are the **worst** any position produced, not a re-derivation
    // across positions: the arrivals of two positions are not comparable (that is the
    // premise of the whole mode), so there is no cross-position triangle to close. Every
    // step already blocked on its own checks, so these are reporting, not gating.
    let checks = Checks {
        transitivity: input
            .steps
            .iter()
            .map(|s| s.checks.transitivity.clone())
            .fold(None::<TransitivityCheck>, |acc, t| match acc {
                Some(w) if w.worst_ms >= t.worst_ms => Some(w),
                _ => Some(t),
            })
            .unwrap_or_else(|| transitivity(&[], &input.timing, TRANSITIVITY_TOL_MS)),
        repeatability: input.steps.iter().filter_map(|s| s.checks.repeatability.clone()).fold(
            None::<RepeatabilityCheck>,
            |acc, r| match acc {
                Some(w) if w.worst_ms >= r.worst_ms => Some(w),
                _ => Some(r),
            },
        ),
        merged_peak: MergedPeakCheck::seam(),
        closure: None,
    };
    Ok(Proposal {
        reference,
        pattern_ms: input.timing.pattern_ms,
        // The ratchet §1.1 warns about, and what the renormalisation removed: how much
        // delay the chain accumulated from its floor, not an acoustic spread.
        spread_ms: ceiling - floor,
        // Per position, so there is no single figure; the largest is the honest one to
        // put on a summary, and every step carries its own.
        drift_ppm: input.steps.iter().map(|s| s.drift_ppm).fold(0.0, |a, b| if b.abs() > a.abs() { b } else { a }),
        target_ms: solution.target_ms,
        feasible_lo_ms: solution.lo_ms,
        feasible_hi_ms: solution.hi_ms,
        largest_knob_ms,
        members,
        checks,
        warnings,
        blocked: None,
    })
}

/// §10.1: after settling, every member should arrive with the reference.
pub fn residual(obs: &[MemberObservation], reference: &str, pattern_ms: f64, tolerance_ms: f64) -> ResidualCheck {
    let fit = fit_drift(obs, pattern_ms, |o| o.m.phase_a_ms);
    let Some(ref_offset) = fit.offsets.get(reference).copied() else {
        return ResidualCheck { worst_member: None, worst_ms: f64::INFINITY, tolerance_ms, passed: false };
    };
    let mut worst = 0.0;
    let mut worst_member = None;
    for (name, off) in &fit.offsets {
        let d = wrap_sym(off - ref_offset, pattern_ms).abs();
        if d > worst {
            worst = d;
            worst_member = Some(name.clone());
        }
    }
    ResidualCheck { worst_member, worst_ms: worst, tolerance_ms, passed: worst <= tolerance_ms }
}

// ---------------------------------------------------------------- feeder

/// Pulls the mic ring into the estimator, contiguously, and keeps the per-period
/// peaks the gate needs.
///
/// Positioned at the *head* of the capture when it is armed: everything older
/// belongs to a state the run has already left behind (a different solo, a
/// pre-write delay), and feeding it would put the disturbance inside the window
/// the gate is about to judge.
struct Feeder<'m> {
    mic: &'m dyn MicFeed,
    rate: u32,
    period_frames: u64,
    /// Next frame index to read.
    next: u64,
    /// First pattern period fully inside the window.
    first_period: u64,
    /// Complete periods pulled so far.
    completed: u64,
    peaks: HashMap<u64, f32>,
    /// Ingest counters at arm time, so "did it happen inside *this* window" is a
    /// comparison rather than a guess.
    base_gaps: u64,
    base_clips: u64,
    seen_gaps: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct Pulled {
    disconnected: bool,
    reconnected: bool,
    gap: bool,
    clipped: bool,
    new_periods: u64,
}

impl<'m> Feeder<'m> {
    fn new(mic: &'m dyn MicFeed, rate: u32, pattern_ms: f64) -> Self {
        let period_frames = (pattern_ms / 1000.0 * f64::from(rate)).round() as u64;
        let mut f = Feeder {
            mic,
            rate,
            period_frames: period_frames.max(1),
            next: 0,
            first_period: 0,
            completed: 0,
            peaks: HashMap::new(),
            base_gaps: 0,
            base_clips: 0,
            seen_gaps: 0,
        };
        f.arm();
        f
    }

    /// Drop everything buffered and start a fresh window at the capture head.
    fn arm(&mut self) {
        let st = self.mic.status();
        self.next = st.frames_received;
        // The period the head sits inside is already partly gone, so the first
        // period this window can *complete* is the next one.
        self.first_period = self.next / self.period_frames + 1;
        self.completed = 0;
        self.peaks.clear();
        self.base_gaps = st.gap_count;
        self.base_clips = st.clip_count;
        self.seen_gaps = st.gap_count;
    }

    fn pull(&mut self, est: &mut Estimator) -> Pulled {
        let mut out = Pulled::default();
        let st = self.mic.status();
        if !st.connected {
            out.disconnected = true;
            return out;
        }
        if st.sample_rate != self.rate || st.frames_received < self.next {
            out.reconnected = true;
            return out;
        }
        // Both counters are per-capture and monotonic, so an increase since the
        // window was armed means it happened inside the window. Sticky until the
        // window is re-armed, which matches the estimator's own latched verdict.
        out.gap = st.gap_count > self.base_gaps;
        out.clipped = st.clip_count > self.base_clips;
        if st.gap_count > self.seen_gaps {
            est.note_gap();
            self.seen_gaps = st.gap_count;
        }

        let mut avail = st.frames_received - self.next;
        if avail > st.capacity_frames as u64 {
            // The run fell so far behind that the ring recycled: the missing audio
            // is unrecoverable, so treat it as a gap and re-arm rather than
            // stitching two non-adjacent stretches together.
            est.note_gap();
            out.gap = true;
            self.arm();
            return out;
        }
        while avail > 0 {
            let len = avail.min(CHUNK_FRAMES as u64) as usize;
            let Some(w) = self.mic.window_from(self.next, len) else {
                // Overwritten between the status read and the window read.
                est.note_gap();
                out.gap = true;
                self.arm();
                return out;
            };
            for (i, s) in w.samples.iter().enumerate() {
                let p = (self.next + i as u64) / self.period_frames;
                let e = self.peaks.entry(p).or_insert(0.0);
                *e = e.max(s.abs());
            }
            est.push_block(w.first_frame, &w.samples);
            self.next += len as u64;
            avail -= len as u64;
        }
        let completed = (self.next / self.period_frames).saturating_sub(self.first_period);
        out.new_periods = completed.saturating_sub(self.completed);
        self.completed = completed;
        out
    }

    /// Peak of the most recently completed period.
    fn last_peak(&mut self) -> f32 {
        let p = self.first_period + self.completed.saturating_sub(1);
        let peak = self.peaks.get(&p).copied().unwrap_or(0.0);
        self.peaks.retain(|k, _| *k >= p);
        peak
    }

    /// Pattern-period index at the centre of the accumulated window — the same
    /// origin convention the estimator reports its intercept on.
    fn period_centre(&self) -> f64 {
        self.first_period as f64 + (self.completed.max(1) - 1) as f64 / 2.0
    }
}

// ---------------------------------------------------------------- manager

/// W4 seam (plan §7): the per-member calibration level the measurement uses, plus
/// the exact hand-off `align_levels` expects, built and type-checked here so
/// wiring it up is a loop rather than an interface negotiation.
#[derive(Debug, Clone)]
pub struct LevelSeam {
    /// The level each member is actually measured at, keyed by node name. This is
    /// the *only* field the measurement stage consumes, and W4's
    /// `LevelPlan::levels` drops straight into it.
    pub levels: HashMap<String, u8>,
    /// The member model `align_levels::LevelSolver::with_config` takes.
    // The hand-off contract for W4, exercised by
    // `the_level_seam_matches_what_the_level_solver_actually_takes` but not by the
    // pass-through itself — same convention as `align_mic::MicWindow`'s W3 fields.
    #[allow(dead_code)]
    pub specs: Vec<crate::align::levels::LevelMemberSpec>,
    /// The Stage-1 configuration — see [`learn_levels`] for why it must be
    /// sequential.
    #[allow(dead_code)]
    pub config: crate::align::levels::LevelConfig,
    pub note: String,
    /// False while the level-learning phase is unimplemented.
    pub learned: bool,
}

/// The LEARNING state of plan §8, as a pass-through — and the seam for W4.
///
/// **What W4 needs from here.** `align_levels::LevelSolver` *drives* the
/// excitation rather than being told about it, so wiring it is:
///
/// ```ignore
/// let mut solver = LevelSolver::with_config(seam.specs, seam.config)?;
/// let mut step = solver.begin();
/// loop {
///     // `step.excite` says what to play; `step.levels` what to play it at.
///     apply(&step.levels);                                  // session.solo(…, level)
///     let est = gate_and_measure(&step.excite).await?;      // the same gate as below
///     let obs = RoundObservation::from_estimate(step.excite.clone(), &est, mic.status().peak);
///     match solver.observe(obs) {
///         LevelDecision::Continue(next) => step = next,
///         LevelDecision::Converged(plan) => break plan,     // → LevelSeam::levels
///         LevelDecision::Refused(r) => return Err(r),       // plan §7: refuse, do not best-effort
///     }
/// }
/// ```
///
/// The member model is built below and is deliberately *not* invented: every
/// member's burst lands in the **same** estimator channel under the shared click
/// track (plan §2.2), so `LevelConfig::sequential()` is not a preference but a
/// requirement — the parallel mode rejects duplicate channel labels at
/// construction because per-member SNR would be unattributable. There is a unit
/// test that both of those hold against the real API.
///
/// **Why this is still a seam.** Two capabilities are missing, and neither is
/// mine to add:
///
/// * The solver needs one `Excitation::All` round to measure the *aggregate* peak
///   — the clipping half of the constraint (`align_levels` line "if
///   !self.aggregate_ok"). The session's audibility control makes **at most two**
///   members audible (`calibrate::apply_audibility` solos reference + target), so
///   a group of three or more cannot honour that round. It needs W7's per-device
///   `cal_gate`, or a new all-members mode in `align/calibrate.rs`.
/// * AP2 members' level knob is `LevelKnob::SnapshotRestore`, i.e. it requires a
///   pre-session snapshot restored on teardown. That snapshot belongs next to
///   `calibrate::Session::saved_sendspin` (plan §7 says so explicitly), and
///   getting it wrong leaves a receiver stuck at a calibration volume. Sendspin's
///   knob is `LevelKnob::Live` and needs nothing new, so a sendspin-only group is
///   what W4 can light up first.
///
/// Until then every member is measured at the session's single calibration level
/// and the user is told so, rather than being left to wonder why a far speaker was
/// too quiet to measure.
fn learn_levels(session_level: u8, members: &[SessionMember]) -> LevelSeam {
    use crate::align::levels::{LevelConfig, LevelMemberKind, LevelMemberSpec};
    LevelSeam {
        levels: members.iter().map(|m| (m.node_name.clone(), session_level)).collect(),
        specs: members
            .iter()
            // `snapshot_level` is left `None` on purpose: the pre-session level is
            // owned by the session (`saved_sendspin`), which already restores it on
            // teardown, and inventing a value here would fight that.
            .map(|m| LevelMemberSpec::new(m.node_name.clone(), CLICK_A_LABEL, LevelMemberKind::from(m.kind)))
            .collect(),
        config: LevelConfig::sequential(),
        note: format!(
            "level learning (W4) is not wired up: every member is measured at the session's calibration level \
             ({session_level}). A speaker too quiet for the estimator will be refused with its own SNR rather than \
             turned up."
        ),
        learned: false,
    }
}

/// What the user's "I am here now" calls turn into — the one channel both
/// user-driven acquisitions are parked on.
///
/// The daemon cannot see where the phone is (auto-detecting the nearest speaker would
/// need per-speaker excitation, which is W7 and does not exist), so both a near-field
/// walk and a multi-position chain are driven by these and by nothing else. One channel
/// rather than two, so the "validate under the state lock, then mark busy" rule that
/// makes a double-tap impossible exists once.
#[derive(Debug, Clone)]
enum RunCommand {
    /// Near field: solo this speaker at this level, gate, and take its reading.
    Arrival { node_name: String, level: Option<u8> },
    /// Near field: take the closure reading at the walk's first speaker.
    Close,
    /// Chaining: measure this position — these speakers, linked to the already-aligned
    /// set through these overlaps (plan §1.1).
    Position { members: Vec<String>, overlaps: Vec<String> },
    /// Chaining: every held speaker is aligned; renormalise globally and propose.
    Finish,
}

/// Why a member's measurement stopped short.
enum StepError {
    /// Give up on the whole run.
    Refuse(Refusal),
    /// The grid moved (the capture reconnected): discard the set and start over.
    RestartSet(Refusal),
}

struct Inner {
    phase: Phase,
    mode: Mode,
    /// The run is a chain (plan §1.1): it takes [`MeasureManager::position`] calls, and
    /// `apply` reads this rather than trusting the request, for the same reason it reads
    /// [`Self::mode`] — how the arrivals were acquired decides how the write can be
    /// checked.
    chained: bool,
    sources: Vec<String>,
    sample_rate: u32,
    message: String,
    members: Vec<MemberProgress>,
    observations: Vec<MemberObservation>,
    proposal: Option<Proposal>,
    verification: Option<Verification>,
    refusal: Option<Refusal>,
    warnings: Vec<Warning>,
    gate: Option<GateProgress>,
    /// Near field only (see [`WalkProgress`]). Kept after the run ends so the closure
    /// numbers stay next to the verdict.
    walk: Option<WalkProgress>,
    /// Chained multi-position only (see [`ChainProgress`]). Kept after the run ends so
    /// the per-position numbers stay next to the verdict.
    chain: Option<ChainProgress>,
    /// Where [`MeasureManager::arrival`], [`MeasureManager::close`],
    /// [`MeasureManager::position`] and [`MeasureManager::finish`] post to. Owned by the
    /// state rather than by the run task so the *validation* happens under this lock —
    /// which is what makes a double-tap on "I'm here" impossible rather than merely
    /// unlikely.
    cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<RunCommand>>,
    /// The provisional delay line, and what this run currently has applied to it, ms
    /// (plan §1.1.1).
    ///
    /// On the state rather than only in the run task because **`abandon` has to be able
    /// to put them back too**: nothing is persisted, but a line left applied keeps
    /// shifting that speaker's audio for as long as the daemon runs, and a closed tab
    /// must not be able to leave one behind.
    relay: Option<Arc<dyn RelayControl>>,
    provisional: HashMap<String, f64>,
    /// §9.4: every member's delay at session start, for one-click revert.
    snapshot: HashMap<String, (MemberKind, u16)>,
    /// Members whose delay this session actually wrote — and therefore the only
    /// ones a revert touches. Reverting a member that was never written would
    /// reconnect a device for nothing, and a reconnect is tens of seconds of
    /// silence (plan §2.3).
    written: Vec<String>,
    /// The group the entries in `written` belong to, kept alive for exactly as long
    /// as they are (see [`MeasureStatus::revert_scope`]).
    revert_sources: Vec<String>,
    started: Option<std::time::Instant>,
    cancel: Arc<AtomicBool>,
    running: bool,
    /// Bumped on every state change, so `measure_ws` can push instead of the UI
    /// polling (plan §11). Behind an `Arc` because a reset (`abandon`, `start`)
    /// replaces the whole `Inner` and must **not** disconnect the sockets that are
    /// already watching — the sender is carried across.
    changes: Arc<tokio::sync::watch::Sender<u64>>,
}

impl Inner {
    fn idle() -> Self {
        Self::idle_watching(Arc::new(tokio::sync::watch::channel(0).0))
    }

    /// A fresh idle state that keeps an existing notifier's subscribers.
    fn idle_watching(changes: Arc<tokio::sync::watch::Sender<u64>>) -> Self {
        Self {
            phase: Phase::Idle,
            mode: Mode::SweetSpot,
            chained: false,
            sources: Vec::new(),
            sample_rate: 0,
            message: "no measurement has been started".to_string(),
            members: Vec::new(),
            observations: Vec::new(),
            proposal: None,
            verification: None,
            refusal: None,
            warnings: Vec::new(),
            gate: None,
            walk: None,
            chain: None,
            cmd_tx: None,
            relay: None,
            provisional: HashMap::new(),
            snapshot: HashMap::new(),
            written: Vec::new(),
            revert_sources: Vec::new(),
            started: None,
            cancel: Arc::new(AtomicBool::new(false)),
            running: false,
            changes,
        }
    }

    /// Tell every `measure_ws` subscriber that [`Self::status`] would now return
    /// something different. Cheap and never fails: `watch` coalesces, and a bump
    /// with no subscribers is a counter increment.
    fn bump(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn status(&self) -> MeasureStatus {
        MeasureStatus {
            phase: self.phase,
            mode: self.mode,
            sources: self.sources.clone(),
            sample_rate: self.sample_rate,
            message: self.message.clone(),
            members: self.members.clone(),
            observations: self.observations.clone(),
            proposal: self.proposal.clone(),
            verification: self.verification.clone(),
            refusal: self.refusal.clone(),
            warnings: self.warnings.clone(),
            gate: self.gate.clone(),
            walk: self.walk.clone(),
            chain: self.chain.clone(),
            can_apply: self.phase == Phase::Proposed && self.proposal.as_ref().is_some_and(|p| p.blocked.is_none()),
            can_revert: !self.written.is_empty(),
            revert_scope: (!self.written.is_empty()).then(|| self.revert_sources.clone()),
            elapsed_s: self.started.map(|s| s.elapsed().as_secs()).unwrap_or(0),
        }
    }

    fn warn(&mut self, w: Warning) {
        if !self.warnings.iter().any(|e| e.kind == w.kind) {
            self.warnings.push(w);
            self.bump();
        }
    }

    fn note(&mut self, node_name: &str, note: Option<String>) {
        if let Some(m) = self.members.iter_mut().find(|m| m.node_name == node_name) {
            m.note = note;
        }
        self.bump();
    }

    /// Record that `node_name`'s knob was written, together with the group it
    /// belongs to — the pair a revert needs, kept together so neither can be set
    /// without the other.
    fn mark_written(&mut self, node_name: &str) {
        if self.written.is_empty() {
            self.revert_sources = self.sources.clone();
        }
        if !self.written.iter().any(|n| n == node_name) {
            self.written.push(node_name.to_string());
        }
        self.bump();
    }

    /// Drop every provisional delay this run has applied (plan §1.1.1: nothing is
    /// persisted, but a live line keeps shifting audio for as long as the daemon runs).
    ///
    /// Idempotent, infallible and safe to call from anywhere — it is the teardown, and
    /// the one thing a chain owes the user back whatever else happened.
    fn clear_provisional(&mut self) -> usize {
        let applied = std::mem::take(&mut self.provisional);
        if let Some(relay) = self.relay.clone() {
            for name in applied.keys() {
                relay.clear(name);
            }
        }
        if let Some(c) = self.chain.as_mut() {
            c.provisional.clear();
        }
        applied.len()
    }
}

/// The measurement orchestrator: one run at a time, process-wide.
pub struct MeasureManager {
    inner: Arc<Mutex<Inner>>,
}

/// The process-wide orchestrator, in the same shape `align_mic` uses — it is a
/// single resource (one mic, one session, one group) with nothing per-request to
/// thread through `AppState`.
pub fn shared() -> &'static MeasureManager {
    static M: OnceLock<MeasureManager> = OnceLock::new();
    M.get_or_init(|| MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) })
}

impl MeasureManager {
    pub fn status(&self) -> MeasureStatus {
        self.inner.lock_recover().status()
    }

    /// A receiver that fires whenever [`Self::status`] would return something new
    /// (plan §11: progress is pushed, not polled). Survives `abandon`/`start`,
    /// which replace the state but carry the notifier across.
    #[allow(dead_code)] // used by `measure_ws`, whose route api.rs owns
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.lock_recover().changes.subscribe()
    }

    /// `POST /api/align/measure/start`.
    ///
    /// Refuses up front on everything that can be known without playing anything: no
    /// alignment session, too few members, no microphone.
    ///
    /// **Every mode starts here**, and what differs afterwards is who steps the member
    /// list: a single-position run does it itself, twice; a **chained** run parks in
    /// [`Phase::Positioning`] and waits for [`Self::position`] once per listening spot,
    /// then [`Self::finish`]; a near-field run parks in [`Phase::Walking`] and waits for
    /// [`Self::arrival`] once per speaker, then [`Self::close`] (plan §1, §1.1, §12.2).
    pub async fn start(&self, deps: MeasureDeps) -> Result<MeasureStatus, Refusal> {
        if !deps.link_to.is_empty() {
            return Err(Refusal::new(
                RefusalKind::ModeUnsupported,
                format!(
                    "this run cannot be linked to speakers aligned in an earlier *run* ({}). Chaining positions inside one run \
                     exists — start with `chain: true` and post a position per listening spot — but nothing stores a finished run's \
                     aligned set together with the delays it applied, so there is nothing for a new run to propagate a shift into. \
                     A run's result is therefore coherent within itself and unrelated to any earlier one, even where the two share a \
                     speaker: align everything that has to sound coherent in one run.",
                    deps.link_to.join(", ")
                ),
            ));
        }
        {
            let inner = self.inner.lock_recover();
            if inner.running || !inner.phase.is_terminal() {
                return Err(Refusal::new(
                    RefusalKind::Internal,
                    format!("a measurement is already running ({:?}); abandon it first", inner.phase),
                ));
            }
        }
        let session = deps.session.snapshot().await;
        if !session.active {
            return Err(Refusal::new(
                RefusalKind::NoSession,
                "no alignment session is running — start one for the group first, so the test pattern is playing on \
                 every member off one clock",
            ));
        }
        if session.members.len() < 2 {
            return Err(Refusal::new(RefusalKind::Internal, "a group needs at least two present members to align"));
        }
        let mic = deps.mic.status();
        if !mic.connected {
            return Err(Refusal::new(
                RefusalKind::MicMissing,
                "no microphone capture is connected. Open the alignment panel on a phone over HTTPS and start the \
                 microphone before measuring.",
            ));
        }

        let snapshot: HashMap<String, (MemberKind, u16)> = session
            .members
            .iter()
            .map(|m| (m.node_name.clone(), (m.kind, deps.current_delays.get(&m.node_name).copied().unwrap_or(0))))
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        // Both user-driven acquisitions need a way for the user to say where they are:
        // near field per speaker, a chain per position. Unbounded because every send is
        // gated by the state check in `arrival`/`close`/`position`/`finish`, so at most
        // one command is ever in flight.
        let chained = deps.chained && !deps.mode.is_walk();
        let (cmd_tx, cmd_rx) = match deps.mode.is_walk() || chained {
            true => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                (Some(tx), Some(rx))
            }
            false => (None, None),
        };
        let status = {
            let mut inner = self.inner.lock_recover();
            // The notifier outlives the state it reports on, so an open `measure_ws`
            // sees the new run rather than being silently disconnected.
            *inner = Inner::idle_watching(inner.changes.clone());
            inner.cmd_tx = cmd_tx;
            inner.phase = Phase::Arming;
            inner.mode = deps.mode;
            inner.chained = chained;
            // Held from the start, so `abandon` can drop a provisional delay even if the
            // run task is wedged between two positions (plan §1.1.1).
            inner.relay = Some(deps.relay.clone());
            inner.sources = session.sources.clone();
            inner.sample_rate = mic.sample_rate;
            inner.message = "arming: checking the session, the capture and the loop-phase lock".to_string();
            inner.members = session
                .members
                .iter()
                .map(|m| MemberProgress {
                    node_name: m.node_name.clone(),
                    kind: m.kind,
                    level: session.level,
                    current_delay_ms: deps.current_delays.get(&m.node_name).copied().unwrap_or(0),
                    passes_done: 0,
                    last: None,
                    note: None,
                })
                .collect();
            inner.snapshot = snapshot;
            inner.started = Some(std::time::Instant::now());
            inner.cancel = cancel.clone();
            inner.running = true;
            inner.bump();
            inner.status()
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let outcome = run_measure(&deps, &inner, &cancel, cmd_rx).await;
            finish(&inner, &cancel, outcome);
        });
        Ok(status)
    }

    /// `POST /api/align/measure/arrival` — near field's "I am at this speaker now".
    ///
    /// This call *is* the near-field measurement loop. The daemon has no way to tell
    /// which speaker the phone is next to (per-speaker excitation is W7 and does not
    /// exist), so the user says it, and the run then solos that member, applies its
    /// level, gates and measures it — plan §12.2's per-arrival level, and one pass in
    /// walk order rather than two.
    ///
    /// Everything is validated **under the state lock**, and the walk is marked
    /// [`WalkAction::Busy`] before the lock is released. A second tap on "I'm here"
    /// therefore refuses rather than queueing a duplicate reading.
    ///
    /// `level` overrides the level to measure at; `None` uses the level the session
    /// last applied to this speaker (which is what
    /// `POST /api/align/audible` sets while the user is standing there watching
    /// `/api/align/mic/signal` go green).
    pub fn arrival(&self, node_name: String, level: Option<u8>) -> Result<MeasureStatus, Refusal> {
        self.command(Some(node_name), level)
    }

    /// `POST /api/align/measure/close` — "I have walked back to the first speaker".
    ///
    /// The closure reading (plan §1, §5.3): a second look at the walk's first member,
    /// whose difference from the first look is the drift accumulated over the whole
    /// walk. See [`ClosureReport`] for the arithmetic and for what it cannot separate.
    /// Refused until every member has been visited — a walk with a hole in it has
    /// nothing to close.
    pub fn close(&self) -> Result<MeasureStatus, Refusal> {
        self.command(None, None)
    }

    /// `POST /api/align/measure/position` — a chain's "these are the speakers I can hear
    /// from where I am now" (plan §1.1).
    ///
    /// `members` are the speakers to align at this position; `overlaps` are speakers
    /// *already* aligned at an earlier position that are still audible here, and they are
    /// what ties the two regions together. **Two overlaps rather than one** is the whole
    /// reason this is not a warning: the shift a step derives from its overlaps is applied
    /// as a common delay to every speaker aligned so far *and* anchors everything measured
    /// after it, so with one overlap a §5.6 reflection bias propagates through the whole
    /// apartment with nothing able to see it. One is accepted — a user may genuinely have
    /// only one shared speaker — and reported as reduced confidence.
    ///
    /// Everything is validated **under the state lock** and the chain is marked
    /// [`ChainAction::Busy`] before it is released, so a second tap refuses rather than
    /// queueing a duplicate position.
    pub fn position(&self, members: Vec<String>, overlaps: Vec<String>) -> Result<MeasureStatus, Refusal> {
        let mut g = self.inner.lock_recover();
        let (known, aligned) = self.chain_preflight(&g)?;
        if members.is_empty() {
            return Err(Refusal::new(
                RefusalKind::ChainOutOfOrder,
                "a position has to name at least one speaker to align. Post the speakers you can hear from where you are standing, \
                 plus one or two you have already aligned and can still hear from here.",
            ));
        }
        if members.len() + overlaps.len() < 2 {
            return Err(Refusal::new(
                RefusalKind::ChainOutOfOrder,
                "a position needs at least two speakers to be worth measuring: one to align, and something to align it *to* — either \
                 another speaker at this position or an overlap from the set already aligned.",
            ));
        }
        for name in members.iter().chain(overlaps.iter()) {
            if !known.iter().any(|n| n == name) {
                return Err(Refusal::for_member(
                    RefusalKind::ChainOutOfOrder,
                    name,
                    format!("'{name}' is not one of the speakers this run is holding, so it cannot be measured here"),
                ));
            }
            if members.iter().filter(|n| *n == name).count() + overlaps.iter().filter(|n| *n == name).count() > 1 {
                return Err(Refusal::for_member(
                    RefusalKind::ChainOutOfOrder,
                    name,
                    format!("'{name}' is named twice in this position; each speaker is either being aligned here or used as an overlap"),
                ));
            }
        }
        for name in &members {
            if aligned.iter().any(|n| n == name) {
                return Err(Refusal::for_member(
                    RefusalKind::ChainOutOfOrder,
                    name,
                    format!(
                        "'{name}' was already aligned at an earlier position. Offer it as an *overlap* instead — that is exactly what an \
                         overlap is, and it is what links this position to the earlier one."
                    ),
                ));
            }
        }
        // Ordered deliberately: at the first position "there is nothing to overlap with"
        // is the useful sentence, and "that speaker is not aligned" would be technically
        // true of every speaker and help nobody.
        if aligned.is_empty() && !overlaps.is_empty() {
            return Err(Refusal::new(
                RefusalKind::ChainOutOfOrder,
                "this is the first position, so nothing has been aligned yet and there is nothing to overlap with — post just the \
                 speakers you can hear from here. They become the reference the rest of the chain is built on.",
            ));
        }
        for name in &overlaps {
            if !aligned.iter().any(|n| n == name) {
                return Err(Refusal::for_member(
                    RefusalKind::ChainOutOfOrder,
                    name,
                    format!(
                        "'{name}' was offered as an overlap but has not been aligned at any earlier position, so it links this position to \
                         nothing. An overlap has to be a speaker that already has a delay from an earlier position."
                    ),
                ));
            }
        }
        if !aligned.is_empty() && overlaps.is_empty() {
            return Err(Refusal::new(
                RefusalKind::OverlapMissing,
                format!(
                    "this position named no overlap, so nothing would relate it to the {} speaker(s) already aligned: the two sets would \
                     each be internally aligned and mutually meaningless. Name one or two speakers you can hear from *both* places — two \
                     is what lets this joint be checked against itself, one is accepted but cannot be.",
                    aligned.len()
                ),
            ));
        }
        self.dispatch(&mut g, RunCommand::Position { members, overlaps }, "measuring this position — stay where you are and hold still")
    }

    /// `POST /api/align/measure/finish` — "every speaker is aligned at some position".
    ///
    /// This is where plan §1.1's **global renormalisation** happens and where the single
    /// write is solved. Refused while any held speaker is still unaligned: a member with
    /// no reading anywhere has nothing to write, and silently leaving it at its old knob
    /// would produce a group that is *partly* aligned without saying so.
    pub fn finish(&self) -> Result<MeasureStatus, Refusal> {
        let mut g = self.inner.lock_recover();
        let (_, aligned) = self.chain_preflight(&g)?;
        let remaining: Vec<String> = g.chain.as_ref().map(|c| c.remaining.clone()).unwrap_or_default();
        if !remaining.is_empty() {
            return Err(Refusal::new(
                RefusalKind::ChainOutOfOrder,
                format!(
                    "{} speaker(s) have not been aligned at any position yet ({}), and a speaker with no reading has nothing to write. \
                     Either walk to a position where you can hear them — with one or two of the {} already-aligned speakers as overlaps — \
                     or abandon this run and start one holding only the speakers you mean to align.",
                    remaining.len(),
                    remaining.join(", "),
                    aligned.len()
                ),
            ));
        }
        self.dispatch(&mut g, RunCommand::Finish, "renormalising the chain and solving the knobs to write")
    }

    /// The state a chaining call needs, or the reason it cannot be accepted here. Shared
    /// by [`Self::position`] and [`Self::finish`] so the two cannot disagree about which
    /// states accept what.
    fn chain_preflight(&self, g: &Inner) -> Result<(Vec<String>, Vec<String>), Refusal> {
        if !g.chained {
            return Err(Refusal::new(
                RefusalKind::ChainOutOfOrder,
                match g.mode.is_walk() {
                    true => "this is a near-field walk: it takes one arrival per speaker (/api/align/measure/arrival) and a closure, not \
                             positions. A walk needs no overlaps at all — it is one continuous capture from end to end."
                        .to_string(),
                    false => "this run is not chained: it measures every held member itself, from wherever the phone is sitting, and \
                              aligns that one position. Start it with `chain: true` to align a set, reposition, and align the next \
                              through overlaps."
                        .to_string(),
                },
            ));
        }
        if g.phase != Phase::Positioning {
            return Err(Refusal::new(
                RefusalKind::ChainOutOfOrder,
                format!("the chain is not waiting for you right now (it is {:?})", g.phase),
            ));
        }
        match g.chain.as_ref().map(|c| c.next) {
            Some(ChainAction::Position | ChainAction::Finish) => {}
            Some(ChainAction::Busy) => {
                return Err(Refusal::new(RefusalKind::ChainOutOfOrder, "the chain is busy measuring a position; wait for it to finish"))
            }
            Some(ChainAction::Done) => {
                return Err(Refusal::new(RefusalKind::ChainOutOfOrder, "this chain is finished; it has nothing left to measure"))
            }
            // `Phase::Positioning` is only ever set after the chain state is published,
            // so this is a daemon bug rather than a state a user can be in — say that
            // instead of blaming the user.
            None => return Err(Refusal::new(RefusalKind::Internal, "the run says it is chaining but has no chain state")),
        }
        let known: Vec<String> = g.members.iter().map(|m| m.node_name.clone()).collect();
        let aligned: Vec<String> = g.chain.as_ref().map(|c| c.aligned.clone()).unwrap_or_default();
        Ok((known, aligned))
    }

    /// Hand a validated chaining command to the run task and mark the chain busy, under
    /// the lock the validation ran under — the same ordering [`Self::command`] uses, and
    /// what makes a double-tap impossible rather than merely unlikely.
    fn dispatch(&self, g: &mut Inner, cmd: RunCommand, prompt: &str) -> Result<MeasureStatus, Refusal> {
        let tx = g.cmd_tx.clone().ok_or_else(|| Refusal::new(RefusalKind::Internal, "this run is no longer accepting positions"))?;
        if tx.send(cmd).is_err() {
            return Err(Refusal::new(RefusalKind::Internal, "the measurement run has ended, so it cannot take this position"));
        }
        if let Some(c) = g.chain.as_mut() {
            c.next = ChainAction::Busy;
            c.refusal = None;
            c.prompt = prompt.to_string();
        }
        g.bump();
        Ok(g.status())
    }

    /// The shared half of [`Self::arrival`] and [`Self::close`]: one validation path,
    /// so the two cannot drift apart on which states accept what.
    fn command(&self, node_name: Option<String>, level: Option<u8>) -> Result<MeasureStatus, Refusal> {
        let mut g = self.inner.lock_recover();
        if !g.mode.is_walk() {
            return Err(Refusal::new(
                RefusalKind::WalkOutOfOrder,
                match g.chained {
                    true => "this is a chained multi-position run: it takes one position per listening spot \
                             (/api/align/measure/position) and a finish, not arrivals. Near-field mode is the one that walks."
                        .to_string(),
                    false => "this is a multi-position run: it measures every member itself, from wherever the phone is sitting, and \
                              takes no arrivals. Near-field mode is the one that walks."
                        .to_string(),
                },
            ));
        }
        if g.phase != Phase::Walking {
            return Err(Refusal::new(
                RefusalKind::WalkOutOfOrder,
                format!("the walk is not waiting for you right now (it is {:?})", g.phase),
            ));
        }
        let known: Vec<String> = g.members.iter().map(|m| m.node_name.clone()).collect();
        let expected = g.walk.as_ref().map(|w| w.next);
        let anchor = g.walk.as_ref().and_then(|w| w.anchor.clone());
        match (expected, &node_name) {
            (Some(WalkAction::Arrival), Some(_)) | (Some(WalkAction::Close), None) => {}
            (Some(WalkAction::Arrival), None) => {
                return Err(Refusal::new(
                    RefusalKind::WalkOutOfOrder,
                    "there are still speakers to visit, so there is nothing to close yet — post an arrival for each of them \
                     first, then come back to the one you started at",
                ))
            }
            (Some(WalkAction::Close), Some(_)) => {
                return Err(Refusal::for_member(
                    RefusalKind::WalkOutOfOrder,
                    anchor.as_deref().unwrap_or_default(),
                    format!(
                        "every speaker in this walk has been measured; what is left is the closure reading at '{}', the one you \
                         started at — post to /api/align/measure/close once you are back there",
                        anchor.as_deref().unwrap_or("the first speaker")
                    ),
                ))
            }
            (Some(WalkAction::Busy), _) => {
                return Err(Refusal::new(RefusalKind::WalkOutOfOrder, "the walk is busy taking a reading; wait for it to finish"))
            }
            (Some(WalkAction::Done), _) => {
                return Err(Refusal::new(RefusalKind::WalkOutOfOrder, "this walk is finished; it has nothing left to measure"))
            }
            // `Phase::Walking` is only ever set after the walk state is published, so
            // this is unreachable rather than a state a user can be in — say that
            // instead of blaming the user for a daemon bug.
            (None, _) => return Err(Refusal::new(RefusalKind::Internal, "the run says it is walking but has no walk state")),
        }
        if let Some(name) = node_name.as_deref() {
            if !known.iter().any(|n| n == name) {
                return Err(Refusal::for_member(
                    RefusalKind::WalkOutOfOrder,
                    name,
                    format!("'{name}' is not one of the speakers this run is holding, so it cannot be measured here"),
                ));
            }
            if g.walk.as_ref().is_some_and(|w| w.measured.iter().any(|n| n == name)) {
                return Err(Refusal::for_member(
                    RefusalKind::WalkOutOfOrder,
                    name,
                    format!(
                        "'{name}' has already been measured in this walk. Each speaker is visited once; the only repeat is the \
                         closure reading at '{}' once every other speaker is done.",
                        anchor.as_deref().unwrap_or("the first speaker")
                    ),
                ));
            }
        }
        let tx = g.cmd_tx.clone().ok_or_else(|| Refusal::new(RefusalKind::Internal, "this run is no longer accepting arrivals"))?;
        let cmd = match node_name.clone() {
            Some(node_name) => RunCommand::Arrival { node_name, level },
            None => RunCommand::Close,
        };
        // Sent before the state is marked busy and while the lock is still held: the
        // run task cannot act on it until it can take this lock, so there is no window
        // in which its own progress update is overwritten by the line below.
        if tx.send(cmd).is_err() {
            return Err(Refusal::new(RefusalKind::Internal, "the measurement run has ended, so it cannot take this reading"));
        }
        if let Some(w) = g.walk.as_mut() {
            let who = node_name.clone().or_else(|| w.anchor.clone()).unwrap_or_default();
            w.reading = Some(who.clone());
            w.next = WalkAction::Busy;
            w.prompt = format!("measuring '{who}' — keep the phone where it is and hold still");
        }
        g.bump();
        Ok(g.status())
    }

    /// `POST /api/align/measure/apply` — the explicit write step (plan §11).
    ///
    /// The **run's** mode and chaining win over the request's: a near-field proposal can
    /// only be verified by walking again (see [`WalkPurpose::Verify`] for why a stationary
    /// residual would fail every time) and a chain's can only be verified at the position
    /// the phone is standing at, so which verification runs is a property of how the
    /// arrivals were acquired, not of this call.
    ///
    /// This is also the **one write wave** of a chained run (plan §1.1.1): the provisional
    /// delay lines are dropped here, because the knobs now carry what they were standing
    /// in for.
    pub async fn apply(&self, deps: MeasureDeps) -> Result<MeasureStatus, Refusal> {
        let mut deps = deps;
        let proposal = {
            let inner = self.inner.lock_recover();
            deps.mode = inner.mode;
            deps.chained = inner.chained;
            if inner.running {
                return Err(Refusal::new(RefusalKind::Internal, "the measurement is still running"));
            }
            if inner.phase != Phase::Proposed {
                return Err(Refusal::new(RefusalKind::Internal, format!("there is no proposal to apply ({:?})", inner.phase)));
            }
            let p = inner.proposal.clone().ok_or_else(|| Refusal::new(RefusalKind::Internal, "there is no proposal to apply"))?;
            if let Some(blocked) = p.blocked.clone() {
                return Err(blocked);
            }
            p
        };
        let cancel = Arc::new(AtomicBool::new(false));
        // A fresh channel for the verification walk: the measurement walk's is closed
        // by now, and reusing it could deliver a stale command to the new walk.
        let (cmd_tx, cmd_rx) = match deps.mode.is_walk() {
            true => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                (Some(tx), Some(rx))
            }
            false => (None, None),
        };
        let status = {
            let mut inner = self.inner.lock_recover();
            inner.phase = Phase::Writing;
            inner.message = "writing the solved delays".to_string();
            inner.verification = None;
            inner.refusal = None;
            inner.walk = None;
            // The chain state stays: its per-position numbers are part of the verdict, and
            // `run_apply` reads the last position's set to decide what a chain's residual
            // can honestly cover.
            inner.cmd_tx = cmd_tx;
            inner.cancel = cancel.clone();
            inner.running = true;
            inner.bump();
            inner.status()
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let outcome = run_apply(&deps, &inner, &cancel, &proposal, cmd_rx).await;
            finish(&inner, &cancel, outcome);
        });
        Ok(status)
    }

    /// `POST /api/align/measure/revert` — plan §9.4's one click back.
    pub async fn revert(&self, writer: &dyn DelayWriter) -> Result<MeasureStatus, Refusal> {
        let restore: Vec<(String, MemberKind, u16)> = {
            let inner = self.inner.lock_recover();
            if inner.running {
                return Err(Refusal::new(RefusalKind::Internal, "a measurement is running; abandon it first"));
            }
            inner.written.iter().filter_map(|n| inner.snapshot.get(n).map(|(kind, ms)| (n.clone(), *kind, *ms))).collect()
        };
        if restore.is_empty() {
            return Err(Refusal::new(RefusalKind::Internal, "this measurement has not written any delay, so there is nothing to restore"));
        }
        let mut failures = Vec::new();
        let mut restored = Vec::new();
        for (node_name, kind, delay_ms) in &restore {
            match writer.write(node_name.clone(), *kind, *delay_ms).await {
                Ok(_) => restored.push(node_name.clone()),
                Err(e) => failures.push(format!("{node_name}: {e}")),
            }
        }
        let mut inner = self.inner.lock_recover();
        inner.written.retain(|n| !restored.contains(n));
        if inner.written.is_empty() {
            // Nothing left to revert, so the scope has nothing to point at either.
            inner.revert_sources.clear();
        }
        if failures.is_empty() {
            inner.message = format!("restored the delays {} member(s) had before this session", restored.len());
        } else {
            // The ones that failed stay in `written`, so a second revert retries
            // exactly those rather than reconnecting the whole group again.
            inner.message = format!("restoring the previous delays partly failed: {}", failures.join("; "));
        }
        inner.bump();
        Ok(inner.status())
    }

    /// `DELETE /api/align/measure` — abandon, leaving delays untouched.
    pub fn abandon(&self) -> MeasureStatus {
        let mut inner = self.inner.lock_recover();
        inner.cancel.store(true, Ordering::Relaxed);
        // Nothing was persisted (plan §1.1.1), but a delay line left applied keeps
        // shifting that speaker for as long as the daemon runs — so a closed tab must not
        // be able to leave one behind. Dropped *before* the state is replaced, since the
        // state is what remembers which lines are live.
        let lines = inner.clear_provisional();
        let snapshot = std::mem::take(&mut inner.snapshot);
        let written = std::mem::take(&mut inner.written);
        let revert_sources = std::mem::take(&mut inner.revert_sources);
        let changes = inner.changes.clone();
        *inner = Inner::idle_watching(changes);
        let provisional = match lines {
            0 => String::new(),
            n => format!(" the {n} provisional delay(s) the chain was applying are gone, so the speakers are back to their stored knobs;"),
        };
        if written.is_empty() {
            inner.message = format!("measurement abandoned;{provisional} no delays were changed");
        } else {
            // The snapshot outlives the run, so a user who applied and *then*
            // abandoned can still get back — and `revert_scope` outlives it too, so
            // the answer to "which group?" does not depend on the UI having kept it.
            inner.message =
                format!("measurement abandoned; the {} delay(s) that were written are still in place (revert to undo)", written.len());
            inner.snapshot = snapshot;
            inner.written = written;
            inner.revert_sources = revert_sources;
        }
        inner.bump();
        inner.status()
    }
}

/// Park the state machine on a terminal state.
///
/// Skipped entirely when the run's own cancel flag is set: that flag is what
/// `abandon` raises, and each run owns a fresh one, so a run that was abandoned
/// (or superseded by a newer one) must not write its late verdict over the state
/// the user is now looking at.
fn finish(inner: &Arc<Mutex<Inner>>, cancel: &AtomicBool, outcome: Result<Phase, Refusal>) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let mut g = inner.lock_recover();
    g.running = false;
    g.gate = None;
    // The walk itself stays visible — its closure numbers are part of the verdict —
    // but nothing more can be posted to it.
    g.cmd_tx = None;
    if let Some(w) = g.walk.as_mut() {
        w.next = WalkAction::Done;
        w.reading = None;
    }
    // Same for the chain: its per-position numbers are the verdict and stay readable.
    if let Some(c) = g.chain.as_mut() {
        c.next = ChainAction::Done;
        c.measuring = None;
    }
    match outcome {
        Ok(phase) => {
            g.phase = phase;
            if phase == Phase::Done {
                g.message = "aligned and verified".to_string();
            }
        }
        Err(refusal) => {
            // A run that ends without a write owes its provisional delays back: the
            // proposal it was standing behind is gone, so leaving the lines applied would
            // silently misalign normal playback (plan §1.1.1). A *successful* run keeps
            // them — the user is listening to the proposal, and `run_apply` drops them as
            // the real knobs take over.
            g.clear_provisional();
            g.message = refusal.message.clone();
            g.refusal = Some(refusal);
            g.phase = Phase::Refused;
        }
    }
    g.bump();
}

fn set_phase(inner: &Arc<Mutex<Inner>>, phase: Phase, message: impl Into<String>) {
    let mut g = inner.lock_recover();
    g.phase = phase;
    g.message = message.into();
    g.bump();
}

/// ARMING → LEARNING → MEASURING → SOLVING → (park in) PROPOSED, or
/// ARMING → WALKING ⇄ MEASURING → SOLVING → PROPOSED for near field.
async fn run_measure(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    cmd_rx: Option<tokio::sync::mpsc::UnboundedReceiver<RunCommand>>,
) -> Result<Phase, Refusal> {
    let session = bind(deps, inner, cancel).await?;
    let rate = deps.mic.status().sample_rate;

    // A chain has its own acquisition *and* its own solve (plan §1.1's global
    // renormalisation), so it owns the path all the way to `Proposed` rather than
    // handing observations back to the single-position solve below — the two positions'
    // arrivals are not comparable, which is the premise of the whole mode.
    if deps.chained && !deps.mode.is_walk() {
        let mut rx =
            cmd_rx.ok_or_else(|| Refusal::new(RefusalKind::Internal, "a chained run was started without a way to accept positions"))?;
        return run_chain(deps, inner, cancel, &mut rx, &session, rate).await;
    }

    // Plan §12.2: "near field breaks the two-phase shape". Its level is only
    // meaningful *at* the speaker and the risk there inverts from too-quiet to
    // clipping, so there is no group-wide learning phase to run or to skip — the level
    // is folded into each arrival, which is also what makes near field one pass
    // instead of two.
    let (observations, closure) = if deps.mode.is_walk() {
        let mut rx =
            cmd_rx.ok_or_else(|| Refusal::new(RefusalKind::Internal, "a near-field run was started without a way to accept arrivals"))?;
        inner.lock_recover().warn(Warning::new(
            WarningKind::NearFieldPathAssumed,
            "near field measures the wiring rather than one listening position, and it does so by assuming the phone is *at* each \
             speaker: hold it within a hand's width of the driver. A phone held a metre away adds about 3 ms of travel to that \
             speaker's reading, and nothing in this measurement can tell that apart from the speaker genuinely being 3 ms late.",
        ));
        let (obs, closure) = run_walk(deps, inner, cancel, &mut rx, WalkPurpose::Measure, &session, rate).await?;
        (obs, Some(closure))
    } else {
        (measure_passes(deps, inner, cancel, &session, rate).await?, None)
    };

    set_phase(inner, Phase::Solving, "solving");
    let proposal = solve(&SolveInput {
        timing: deps.timing,
        members: &session.members,
        observations: &observations,
        current_delays: &deps.current_delays,
        send_ahead: &deps.send_ahead,
        closure,
    })?;
    let blocked = proposal.blocked.clone();
    {
        let mut g = inner.lock_recover();
        for w in &proposal.warnings {
            g.warn(w.clone());
        }
        g.proposal = Some(proposal);
        g.bump();
    }
    if let Some(blocked) = blocked {
        return Err(blocked);
    }
    set_phase(inner, Phase::Proposed, "measured; review the proposed delays, then apply them");
    Ok(Phase::Proposed)
}

/// The multi-position measurement stage: the run steps the member list itself,
/// [`MEASURE_PASSES`] times, alternating direction (plan §6.1).
///
/// Unchanged by W8a — near field goes through [`run_walk`] instead — and kept apart
/// from [`run_measure`] only so the two acquisition strategies read as the
/// alternatives they are.
async fn measure_passes(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    session: &SessionSnapshot,
    rate: u32,
) -> Result<Vec<MemberObservation>, Refusal> {
    set_phase(inner, Phase::Learning, "learning playback levels");
    let plan = learn_levels(session.level, &session.members);
    {
        let mut g = inner.lock_recover();
        if !plan.learned {
            g.warn(Warning::new(WarningKind::LevelLearningSkipped, plan.note.clone()));
        }
        for m in &mut g.members {
            m.level = plan.levels.get(&m.node_name).copied().unwrap_or(session.level);
        }
        g.bump();
    }

    let mut epoch = 0u64;
    let mut restarts = 0u32;
    loop {
        {
            let mut g = inner.lock_recover();
            g.observations.clear();
            g.bump();
        }
        match measure_set(deps, inner, cancel, &session.members, &plan.levels, session.level, "", epoch, rate).await {
            Ok(observations) => return Ok(observations),
            Err(StepError::Refuse(r)) => return Err(r),
            Err(StepError::RestartSet(r)) => {
                if restarts >= MAX_SET_RESTARTS {
                    return Err(r);
                }
                restarts += 1;
                epoch += 1;
                let mut g = inner.lock_recover();
                g.warn(Warning::new(WarningKind::MicReconnected, r.message.clone()));
                for m in &mut g.members {
                    m.passes_done = 0;
                    m.last = None;
                }
                g.bump();
            }
        }
    }
}

/// One set of members, measured [`MEASURE_PASSES`] times with the pass order
/// **alternating** (plan §6.1), inside one grid epoch.
///
/// The unit both stationary acquisitions are built from: a single-position run measures
/// the whole group this way, and a chain measures **one position's** members plus its
/// overlaps this way. A capture reconnect is returned as [`StepError::RestartSet`]
/// rather than retried here, because what a new frame costs differs — a set can simply
/// be retaken, while for a chain it is a position the *user* has to stand at again
/// (plan §1.2).
#[allow(clippy::too_many_arguments)] // one set's worth of context; a struct would only move the list
async fn measure_set(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    members: &[SessionMember],
    levels: &HashMap<String, u8>,
    default_level: u8,
    label: &str,
    epoch: u64,
    rate: u32,
) -> Result<Vec<MemberObservation>, StepError> {
    let mut observations: Vec<MemberObservation> = Vec::new();
    for pass in 0..MEASURE_PASSES {
        // Alternate the order so a mic-clock drift averages out across members
        // instead of accumulating down the list (plan §6.1).
        let mut order: Vec<&SessionMember> = members.iter().collect();
        if pass % 2 == 1 {
            order.reverse();
        }
        for member in order {
            set_phase(inner, Phase::Measuring, format!("{label}measuring '{}' (pass {}/{})", member.node_name, pass + 1, MEASURE_PASSES));
            let level = levels.get(&member.node_name).copied().unwrap_or(default_level);
            let cfg = GateConfig::mute_settle(&deps.timing);
            let o = measure_member(deps, inner, cancel, member, level, cfg, pass, epoch, rate).await?;
            let mut g = inner.lock_recover();
            if let Some(p) = g.members.iter_mut().find(|m| m.node_name == member.node_name) {
                p.passes_done += 1;
                p.last = Some(o.m.clone());
            }
            g.observations.push(o.clone());
            g.bump();
            drop(g);
            observations.push(o);
        }
    }
    Ok(observations)
}

// ------------------------------------------------------------ near field (W8a)

/// Near field's acquisition stage: **the user** walks to each speaker, the daemon
/// measures whichever one it is told about, and the walk ends where it began.
///
/// ## Why it is shaped like this
///
/// Three things make near field different from [`measure_passes`], and all three come
/// straight out of the physics rather than from taste:
///
/// 1. **Arrival is user-driven, one speaker at a time.** The daemon cannot tell where
///    the phone is; working it out would need per-speaker excitation so the nearest
///    member could be identified from the capture, which is W7 and does not exist. So
///    the UI says "I am at this speaker now" ([`MeasureManager::arrival`]) and the run
///    solos *that* member, sets its level, gates it and measures it.
/// 2. **Level is set per arrival, not up front** (plan §12.2). At arm's length the
///    risk inverts from too-quiet to clipping, and a level chosen from anywhere else
///    is simply wrong. So there is no learning phase here; the level comes from the
///    session's per-member map, i.e. from whatever the user settled on while standing
///    there.
/// 3. **One pass, plus a closure.** Walking the house twice is not acceptable, and one
///    pass has no time baseline for the drift slope (plan §5.3) — over a walk that
///    takes minutes, a 100 ppm clock is *milliseconds* of creep that would be written
///    into the speakers as delay. Revisiting the first speaker at the end supplies the
///    baseline: see [`ClosureReport`].
///
/// ## Plan §1.2 is the rule this must not break
///
/// A walk is **one continuous capture** from the first speaker to the closure. Within
/// it every arrival is comparable because the grid is `frame_index mod period`; across
/// a reconnect nothing is, because `align_mic` restarts its frame counter. So a
/// reconnect noticed anywhere in the walk discards **everything** and the walk starts
/// again from its first speaker — and the user is told that in so many words, because
/// silently mixing two frames would produce confident nonsense.
///
/// ## The budget nobody can extend from here
///
/// `calibrate::SESSION_TIMEOUT` tears the alignment session down 15 minutes after the
/// `start` that armed it, whatever this function is doing. A walk that needs longer
/// therefore ends as [`RefusalKind::SessionLost`] mid-walk, and the remedy is to start
/// the session again and walk again — splitting it into two linked sessions is plan
/// §1.2's cross-session case, which is W8b.
async fn run_walk(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RunCommand>,
    purpose: WalkPurpose,
    session: &SessionSnapshot,
    rate: u32,
) -> Result<(Vec<MemberObservation>, ClosureReport), Refusal> {
    let all: Vec<String> = session.members.iter().map(|m| m.node_name.clone()).collect();
    let mut restarts = 0u32;
    'walk: loop {
        let mut order: Vec<String> = Vec::new();
        let mut observations: Vec<MemberObservation> = Vec::new();
        let mut levels_used: HashMap<String, u8> = HashMap::new();
        {
            let mut g = inner.lock_recover();
            g.observations.clear();
            for m in &mut g.members {
                m.passes_done = 0;
                m.last = None;
            }
            g.bump();
        }
        loop {
            let remaining: Vec<String> = all.iter().filter(|n| !order.contains(n)).cloned().collect();
            let next = if remaining.is_empty() { WalkAction::Close } else { WalkAction::Arrival };
            let prompt = walk_prompt(purpose, next, &order, &remaining, restarts);
            set_walk(
                inner,
                WalkProgress {
                    purpose,
                    next,
                    anchor: order.first().cloned(),
                    measured: order.clone(),
                    remaining,
                    reading: None,
                    restarts,
                    prompt: prompt.clone(),
                    closure: None,
                    scope_note: WALK_SCOPE_NOTE,
                    level_note: WALK_LEVEL_NOTE,
                },
            );
            set_phase(inner, Phase::Walking, prompt);

            let command = next_command(deps, inner, cancel, rx, "walked to a speaker").await?;
            // Which member, at which level, and which pass this reading belongs to.
            let (name, level, pass) = match &command {
                RunCommand::Arrival { node_name, level } => {
                    let level = level.unwrap_or_else(|| session.level_for(node_name));
                    (node_name.clone(), level, 0usize)
                }
                // Unreachable: `MeasureManager::command` refuses a chain call on a walk
                // under the state lock before anything is sent. Named rather than
                // ignored, so a future channel change fails loudly instead of quietly
                // dropping a user's call.
                RunCommand::Position { .. } | RunCommand::Finish => {
                    return Err(Refusal::new(RefusalKind::Internal, "a chaining call reached the near-field walk"))
                }
                RunCommand::Close => {
                    let Some(anchor) = order.first().cloned() else {
                        return Err(Refusal::new(
                            RefusalKind::WalkOutOfOrder,
                            "nothing has been measured yet, so there is nothing to close",
                        ));
                    };
                    // Deliberately the *same* level as the first reading: the phase does
                    // not depend on level, but keeping it identical removes one more way
                    // for the two readings to differ for a reason that is not drift.
                    let level = levels_used.get(&anchor).copied().unwrap_or_else(|| session.level_for(&anchor));
                    (anchor, level, 1usize)
                }
            };
            let Some(member) = session.members.iter().find(|m| m.node_name == name) else {
                return Err(Refusal::for_member(
                    RefusalKind::WalkOutOfOrder,
                    &name,
                    format!("'{name}' is not a member of the group being aligned"),
                ));
            };
            let closing = matches!(command, RunCommand::Close);
            set_phase(
                inner,
                Phase::Measuring,
                match closing {
                    true => format!("closing the walk at '{name}' — this reading is what separates clock drift from real offsets"),
                    false => format!("measuring '{name}' ({}/{} speakers)", order.len() + 1, all.len()),
                },
            );
            {
                let mut g = inner.lock_recover();
                if let Some(p) = g.members.iter_mut().find(|m| m.node_name == name) {
                    p.level = level;
                }
                g.bump();
            }
            // A verification walk happens right after a write wave, so the first
            // speaker the user reaches may still be reconnecting: tens of seconds of
            // silence that is expected, not a fault (plan §2.3).
            let cfg = match purpose {
                WalkPurpose::Measure => GateConfig::mute_settle(&deps.timing),
                WalkPurpose::Verify => GateConfig::reconnect(&deps.timing),
            };
            match measure_member(deps, inner, cancel, member, level, cfg, pass, u64::from(restarts), rate).await {
                Ok(o) => {
                    {
                        let mut g = inner.lock_recover();
                        if let Some(p) = g.members.iter_mut().find(|m| m.node_name == name) {
                            p.passes_done += 1;
                            p.last = Some(o.m.clone());
                        }
                        g.observations.push(o.clone());
                        g.bump();
                    }
                    levels_used.insert(name.clone(), level);
                    if closing {
                        // The anchor's first reading is the one this is compared against.
                        let Some(first) = observations.iter().find(|x| x.node_name == name) else {
                            return Err(Refusal::new(RefusalKind::Internal, "the walk closed on a speaker it had not measured"));
                        };
                        let closure = closure_report(first, &o, &deps.timing);
                        observations.push(o);
                        {
                            let mut g = inner.lock_recover();
                            if let Some(w) = g.walk.as_mut() {
                                w.next = WalkAction::Busy;
                                w.reading = None;
                                w.closure = Some(closure.clone());
                                w.prompt = closure_prompt(&closure);
                            }
                            g.bump();
                        }
                        return Ok((observations, closure));
                    }
                    order.push(name);
                    observations.push(o);
                }
                Err(StepError::Refuse(r)) => return Err(r),
                Err(StepError::RestartSet(r)) => {
                    // Plan §1.2: the capture *is* the reference frame, so this voids the
                    // whole walk rather than one reading. Nothing is silently mixed
                    // across the seam, and the user is told they have to start over.
                    if restarts >= MAX_WALK_RESTARTS {
                        let mut r = r;
                        r.message = format!(
                            "{} This walk has already been restarted {restarts} time(s); the phone's capture is not staying \
                             connected long enough to measure a whole walk, so there is no point asking you to do it again.",
                            r.message
                        );
                        return Err(r);
                    }
                    restarts += 1;
                    inner.lock_recover().warn(Warning::new(
                        WarningKind::MicReconnected,
                        format!(
                            "the microphone capture restarted during the walk. Everything measured within one capture is \
                             comparable and nothing is comparable across a restart, so the {} reading(s) taken so far have been \
                             discarded — the walk has to start again from the first speaker.",
                            order.len()
                        ),
                    ));
                    continue 'walk;
                }
            }
        }
    }
}

/// Publish the walk's state. Separate from [`set_phase`] because a walk changes
/// *what the user should do* independently of the phase.
fn set_walk(inner: &Arc<Mutex<Inner>>, walk: WalkProgress) {
    let mut g = inner.lock_recover();
    g.walk = Some(walk);
    g.bump();
}

/// The sentence the user reads while the walk waits for them.
fn walk_prompt(purpose: WalkPurpose, next: WalkAction, measured: &[String], remaining: &[String], restarts: u32) -> String {
    let restarted = match restarts {
        0 => String::new(),
        _ => "The capture restarted, so this walk begins again from the first speaker. ".to_string(),
    };
    let what = match purpose {
        WalkPurpose::Measure => "measure",
        WalkPurpose::Verify => "check",
    };
    match next {
        WalkAction::Close => format!(
            "{restarted}All {} speakers have been read. Now walk back to '{}' — the one you started at — hold the phone at it the \
             same way you did the first time, and post to /api/align/measure/close. That second reading is the only thing that can \
             separate the clock drift accumulated over this walk from real offsets.",
            measured.len(),
            measured.first().map(String::as_str).unwrap_or("the first speaker")
        ),
        _ => format!(
            "{restarted}Walk to a speaker you have not done yet, hold the phone within a hand's width of it, set its level until \
             the signal check goes green, and post its name to /api/align/measure/arrival. {} left to {what}: {}. Keep the \
             microphone capture open the whole way — it is the timing reference, and reopening it restarts this walk.",
            remaining.len(),
            remaining.join(", ")
        ),
    }
}

/// What the closure reading turned out to say, in one sentence.
fn closure_prompt(c: &ClosureReport) -> String {
    match c.passed {
        true => format!(
            "walk closed: '{}' read {:.2} ms differently after {:.0} s, i.e. {:.0} ppm of clock drift, which has been taken out of \
             every speaker's reading in proportion to when it was measured",
            c.anchor, c.error_ms, c.span_s, c.drift_ppm
        ),
        false => format!(
            "walk closed badly: '{}' read {:.2} ms differently after {:.0} s (limit {:.1} ms), which is more than clock drift can \
             account for",
            c.anchor, c.error_ms, c.span_s, c.tolerance_ms
        ),
    }
}

/// Wait for the user's next "I am here", while keeping an eye on everything that can
/// make the wait pointless.
///
/// [`bind`] is polled throughout rather than only when a command arrives: a walk
/// between floors takes minutes, and finding out at the *next* speaker that the
/// session timed out three minutes ago wastes the walk. A microphone that disconnects
/// while parked is fatal for the same reason it is fatal mid-reading — the capture is
/// the timing reference (plan §1.2) — and [`bind`] says so in those words.
///
/// Shared by the walk and the chain: both park waiting for a person to move, and
/// [`Timing::walk_arrival_timeout`] is the same budget either way. `did` names what the
/// user did not do, so the timeout reads correctly for both.
async fn next_command(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RunCommand>,
    did: &str,
) -> Result<RunCommand, Refusal> {
    let deadline = Instant::now() + deps.timing.walk_arrival_timeout;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
        }
        bind(deps, inner, cancel).await?;
        let now = Instant::now();
        if now >= deadline {
            return Err(Refusal::new(
                RefusalKind::WalkTimeout,
                format!(
                    "nobody {did} for {} minutes, so the run gave up rather than holding these speakers indefinitely. \
                     Start the measurement again — the alignment session is still yours.",
                    deps.timing.walk_arrival_timeout.as_secs() / 60
                ),
            ));
        }
        tokio::select! {
            command = rx.recv() => return match command {
                Some(c) => Ok(c),
                // The sender lives in `Inner`, which `abandon` (and a fresh `start`)
                // replaces — so a closed channel is exactly "this run is over".
                None => Err(Refusal::new(RefusalKind::Cancelled, "abandoned")),
            },
            _ = tokio::time::sleep(deps.timing.poll.min(deadline - now)) => {}
        }
    }
}

// ------------------------------------------- chained multi-position (W12, plan §1.1)

/// The chain's acquisition stage: the user aligns a locally-audible set, repositions,
/// and aligns the next through shared **overlap** speakers.
///
/// ## What each position does
///
/// One position measures its own speakers **plus its overlaps** — [`measure_set`], two
/// alternating passes, exactly as a single-position run measures a group — and then
/// [`chain_step`] does §1.1's algebra on the result. The delays it produces are
/// **provisional**: they go into the per-device delay line (`align/relay_delay.rs`, plan
/// §1.1.1) so the user can hear the alignment and so the *next* position measures each
/// overlap through the delay it is carrying, which is what makes the chain composable.
/// The real knobs are written **once**, by `apply`, after the last position.
///
/// ## The two costs this deliberately does not pay
///
/// * **One hold for the whole run** (plan §12.3.1). The union was held by
///   `POST /api/align/start` and nothing here re-forms it: a position is a *subset*, and
///   the audibility it needs is the sequential solo the measurement already does. Five
///   positions therefore cost one formation wave, not ten.
/// * **One write wave** (plan §1.1.1). Nothing between positions touches a device knob,
///   so no speaker reconnects during the walk.
///
/// ## Plan §1.2, and where this had to differ from it
///
/// §1.2's rule is that everything comparable lives in one continuous capture. A
/// *position* obeys it strictly: its own readings are one epoch, and a reconnect mid-position
/// discards that position's readings and asks the user to stand there again
/// ([`MAX_CHAIN_STEP_RESTARTS`]) rather than solving across the seam.
///
/// Across positions it is **not** a shared frame that carries the chain, and that is
/// deliberate: what crosses a position boundary is a *provisional delay in
/// milliseconds*, not a phase, and every position re-measures its overlaps in its own
/// frame. So a capture that reconnects *between* two positions costs nothing — the
/// overlap reading is what re-anchors the new frame, which is exactly what §1.2's
/// parenthetical ("each step is its own position but also — if the capture was
/// interrupted — its own frame") is pointing at. Each step therefore records its
/// [`ChainStep::grid_epoch`], and nothing in the chain ever compares two steps'
/// observations; the honest bound on the joint is the overlap disagreement, not the
/// capture's continuity.
async fn run_chain(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RunCommand>,
    session: &SessionSnapshot,
    rate: u32,
) -> Result<Phase, Refusal> {
    let all: Vec<String> = session.members.iter().map(|m| m.node_name.clone()).collect();
    set_phase(inner, Phase::Learning, "learning playback levels");
    let plan = learn_levels(session.level, &session.members);
    {
        let mut g = inner.lock_recover();
        if !plan.learned {
            g.warn(Warning::new(WarningKind::LevelLearningSkipped, plan.note.clone()));
        }
        for m in &mut g.members {
            m.level = plan.levels.get(&m.node_name).copied().unwrap_or(session.level);
        }
        g.bump();
    }

    // The chain's whole state: what each aligned member is carrying, in the order it was
    // aligned, plus the positions and every reading they took.
    let mut provisional: HashMap<String, f64> = HashMap::new();
    let mut aligned: Vec<String> = Vec::new();
    let mut steps: Vec<ChainStep> = Vec::new();
    let mut all_obs: Vec<MemberObservation> = Vec::new();
    let mut epoch = 0u64;
    let mut refusal: Option<Refusal> = None;

    loop {
        let remaining: Vec<String> = all.iter().filter(|n| !aligned.contains(n)).cloned().collect();
        let next = if remaining.is_empty() { ChainAction::Finish } else { ChainAction::Position };
        let prompt = chain_prompt(next, &remaining, steps.len());
        set_chain(inner, chain_progress(next, &steps, &aligned, &remaining, &provisional, None, 0, prompt.clone(), refusal.clone()));
        set_phase(inner, Phase::Positioning, prompt);

        let (members, overlaps) = match next_command(deps, inner, cancel, rx, "posted a position").await? {
            RunCommand::Position { members, overlaps } => (members, overlaps),
            RunCommand::Finish => break,
            // Unreachable: `MeasureManager::command` refuses a walk call on a chained run
            // under the state lock. Named rather than ignored, so a future channel change
            // fails loudly instead of quietly dropping a user's call.
            RunCommand::Arrival { .. } | RunCommand::Close => {
                return Err(Refusal::new(RefusalKind::Internal, "a near-field call reached the multi-position chain"))
            }
        };
        refusal = None;
        let index = steps.len() + 1;
        // The step's members, in one deterministic order: its own speakers first, then
        // the overlaps. That order is the drift fit's abscissa and the linearisation's
        // anchor, so it is a decision rather than an accident.
        let step_members: Vec<SessionMember> =
            members.iter().chain(overlaps.iter()).filter_map(|n| session.members.iter().find(|m| &m.node_name == n).cloned()).collect();
        let order: Vec<String> = step_members.iter().map(|m| m.node_name.clone()).collect();

        let mut restarts = 0u32;
        let outcome = loop {
            let base = all_obs.len();
            {
                let mut g = inner.lock_recover();
                g.observations.truncate(base);
                if let Some(c) = g.chain.as_mut() {
                    c.next = ChainAction::Busy;
                    c.measuring = Some(index);
                    c.restarts = restarts;
                    c.prompt = format!("measuring position {index} — stay where you are and keep still");
                }
                g.bump();
            }
            match measure_set(deps, inner, cancel, &step_members, &plan.levels, session.level, &format!("position {index}: "), epoch, rate)
                .await
            {
                Ok(obs) => break Ok(obs),
                Err(StepError::Refuse(r)) => break Err(r),
                Err(StepError::RestartSet(r)) => {
                    // Plan §1.2: this position's readings came from two captures, so they
                    // are not comparable with each other. The *chain* survives — the
                    // positions already aligned are linked by delays, not by this frame —
                    // but this position has to be measured again from where the user is.
                    epoch += 1;
                    if restarts >= MAX_CHAIN_STEP_RESTARTS {
                        break Err(Refusal {
                            message: format!(
                                "{} Position {index} has already been restarted {restarts} time(s); the phone's capture is not staying \
                                 connected long enough to measure one position, so there is no point asking again. The {} speaker(s) \
                                 aligned at earlier positions are untouched and still carry their provisional delays.",
                                r.message,
                                aligned.len()
                            ),
                            ..r
                        });
                    }
                    restarts += 1;
                    inner.lock_recover().warn(Warning::new(
                        WarningKind::MicReconnected,
                        format!(
                            "the microphone capture restarted while position {index} was being measured. Everything measured within one \
                             capture is comparable and nothing is comparable across a restart, so this position's readings have been \
                             discarded and it is being measured again. The {} speaker(s) aligned at earlier positions are not affected: \
                             what carries a chain from one position to the next is the delay each speaker is holding, and this position \
                             re-measures its overlaps in the new capture — that re-measurement is what re-anchors it.",
                            aligned.len()
                        ),
                    ));
                }
            }
        };

        let accepted =
            match outcome.and_then(|obs| chain_solve_step(deps, index, &step_members, &order, &overlaps, &provisional, epoch, obs)) {
                Ok(step) => step,
                Err(r) => {
                    if !chain_step_retryable(r.kind) {
                        return Err(r);
                    }
                    // The chain stays parked: the aligned set and its provisional delays
                    // are untouched, and the user can try this position again.
                    tracing::info!("alignment chain: position {index} refused ({:?}) — {}", r.kind, r.message);
                    refusal = Some(r);
                    continue;
                }
            };

        // The chain's state is what the **line is applying**, not the ideal the step
        // solved for. Both the line and the knobs are set in whole milliseconds (plan
        // §1.1.2 item 4), and a model that disagreed with reality by half a millisecond
        // would put that error into the alignment between the already-aligned set and
        // every position measured after it. Rounding *here* keeps the two identical, and
        // the error stays ≤0.5 ms per member instead of accumulating down the chain —
        // every later position re-measures its overlaps through the line, so what was
        // applied is observed rather than assumed.
        let applied: HashMap<String, f64> = accepted.provisional.iter().map(|(n, v)| (n.clone(), v.round())).collect();
        // Applied *before* the step counts as accepted, so a line that refuses cannot
        // leave the chain believing in a delay nobody applied.
        let changed: Vec<(String, f64)> = applied
            .iter()
            .filter(|(n, v)| provisional.get(*n).is_none_or(|prev| (prev - *v).abs() > 1e-9))
            .map(|(n, v)| (n.clone(), *v))
            .collect();
        apply_provisional(deps, inner, cancel, &changed).await?;

        for (name, value) in &applied {
            provisional.insert(name.clone(), *value);
        }
        for name in &members {
            if !aligned.contains(name) {
                aligned.push(name.clone());
            }
        }
        all_obs.extend(accepted.observations);
        if accepted.step.confidence == OverlapConfidence::Single {
            let one = accepted.step.overlaps.first().map(|o| o.node_name.clone()).unwrap_or_default();
            inner.lock_recover().warn(Warning::new(
                WarningKind::OneOverlap,
                format!(
                    "position {index} is linked to everything aligned before it through the single overlap '{one}'. That one reading is \
                     applied as a common shift to every speaker already aligned and anchors every position after it, and with one overlap \
                     there is nothing to check it against — so this joint is the chain's weakest, and the chain's total error can no \
                     longer be bounded.",
                ),
            ));
        }
        steps.push(accepted.step);
        {
            let mut g = inner.lock_recover();
            g.provisional = provisional.clone();
            g.bump();
        }
    }

    // ---- finish: renormalise globally, then propose the single write ----------
    set_phase(inner, Phase::Solving, "renormalising the whole chain, then solving the knobs to write");
    {
        let mut g = inner.lock_recover();
        if let Some(c) = g.chain.as_mut() {
            c.next = ChainAction::Busy;
            c.measuring = None;
            c.prompt = "renormalising: taking the accumulated delay back out as a common shift, which moves nothing relative to \
                        anything else"
                .to_string();
        }
        g.bump();
    }
    let proposal = solve_chain(&ChainSolveInput {
        timing: deps.timing,
        members: &session.members,
        provisional: &provisional,
        current_delays: &deps.current_delays,
        send_ahead: &deps.send_ahead,
        steps: &steps,
        observations: &all_obs,
    })?;
    let blocked = proposal.blocked.clone();
    {
        let mut g = inner.lock_recover();
        for w in &proposal.warnings {
            g.warn(w.clone());
        }
        g.proposal = Some(proposal);
        let done = chain_progress(
            ChainAction::Done,
            &steps,
            &aligned,
            &[],
            &provisional,
            None,
            0,
            "the chain is solved: review the proposed knobs, then apply them. Nothing has been written yet — what you are hearing is the \
             provisional delay line."
                .to_string(),
            None,
        );
        g.chain = Some(done);
        g.bump();
    }
    if let Some(blocked) = blocked {
        return Err(blocked);
    }
    set_phase(inner, Phase::Proposed, "the chain is measured; review the proposed delays, then apply them");
    Ok(Phase::Proposed)
}

/// One position's arithmetic and its own §10 checks, kept out of [`run_chain`] so the
/// orchestration reads as orchestration.
///
/// The checks **block the step**, exactly as they block a single-position write: a
/// position that failed transitivity would otherwise be carried into every position
/// after it, since its shift is applied to the whole aligned set.
struct AcceptedStep {
    step: ChainStep,
    /// The new provisional delay for every member this position touches — the step's own
    /// speakers, plus the whole already-aligned set when Δ > 0.
    provisional: HashMap<String, f64>,
    observations: Vec<MemberObservation>,
}

#[allow(clippy::too_many_arguments)] // one position's worth of context; a struct would only move the list
fn chain_solve_step(
    deps: &MeasureDeps,
    index: usize,
    step_members: &[SessionMember],
    order: &[String],
    overlaps: &[String],
    aligned: &HashMap<String, f64>,
    epoch: u64,
    observations: Vec<MemberObservation>,
) -> Result<AcceptedStep, Refusal> {
    let arrivals = arrivals_of(&observations, order, &deps.timing)?;
    let solution = chain_step(&ChainStepInput { aligned, arrivals: &arrivals.linear, overlaps, tolerance_ms: OVERLAP_AGREEMENT_TOL_MS })?;
    let checks = Checks {
        transitivity: transitivity(&observations, &deps.timing, TRANSITIVITY_TOL_MS),
        repeatability: repeatability(&observations, &arrivals.fit, deps.timing.pattern_ms, REPEATABILITY_TOL_MS),
        merged_peak: MergedPeakCheck::seam(),
        closure: None,
    };
    if !checks.transitivity.passed {
        let (a, b) = checks.transitivity.worst_pair.clone().unwrap_or_default();
        return Err(Refusal::new(
            RefusalKind::Transitivity,
            format!(
                "at position {index} the two test tones disagree by {:.2} ms about how far apart '{a}' and '{b}' are (limit {:.1} ms). A \
                 speaker's arrival is being pulled by an early reflection, or its crossover delays the two tones differently — either \
                 way the measured offset is not the electrical one. This position was not accepted; the speakers aligned at earlier \
                 positions are untouched. Move the phone away from walls and hard surfaces and measure this position again.",
                checks.transitivity.worst_ms, checks.transitivity.tolerance_ms
            ),
        ));
    }
    if let Some(rep) = checks.repeatability.as_ref().filter(|r| !r.passed) {
        let who = rep.worst_member.clone().unwrap_or_default();
        return Err(Refusal::for_member(
            RefusalKind::Repeatability,
            &who,
            format!(
                "at position {index}, '{who}' measured {:.2} ms differently between the two passes (limit {:.1} ms) — the phone or the \
                 room moved while this position was being measured. Put the phone down where you are listening and measure this position \
                 again; nothing aligned earlier was affected.",
                rep.worst_ms, rep.tolerance_ms
            ),
        ));
    }
    let step = ChainStep {
        index,
        members: step_members.iter().map(|m| m.node_name.clone()).filter(|n| !overlaps.contains(n)).collect(),
        overlaps: solution.overlaps.clone(),
        confidence: solution.confidence,
        disagreement_ms: solution.disagreement_ms,
        worst_pair: solution.worst_pair,
        tolerance_ms: OVERLAP_AGREEMENT_TOL_MS,
        anchor_ms: solution.anchor_ms,
        delta_ms: solution.delta_ms,
        target_ms: solution.target_ms,
        spread_ms: arrivals.spread_ms,
        drift_ppm: arrivals.fit.drift_ppm(deps.timing.pattern_ms),
        joint_error_ms: solution.joint_error_ms,
        grid_epoch: epoch,
        checks,
        note: solution.note,
    };
    Ok(AcceptedStep { step, provisional: solution.provisional, observations })
}

/// Whether a refusal is about *this position's reading* — in which case the chain stays
/// alive and the user can stand there and try again — or about the run itself, which
/// ends it.
///
/// Losing a whole apartment's chain because one joint's overlaps disagreed would be the
/// wrong trade: the positions already aligned are still good and still carry their
/// provisional delays. Anything about the run's *bindings* (the session, the capture,
/// the delay line, cancellation) is fatal, because retrying cannot help.
fn chain_step_retryable(kind: RefusalKind) -> bool {
    matches!(
        kind,
        RefusalKind::OverlapDisagreement
            | RefusalKind::OverlapMissing
            | RefusalKind::ChainOutOfOrder
            | RefusalKind::Transitivity
            | RefusalKind::Repeatability
            | RefusalKind::AmbiguousSpread
            | RefusalKind::Estimator
            | RefusalKind::GateTimeout
            | RefusalKind::Interference
            | RefusalKind::MicReconnected
    )
}

/// Push the chain's provisional delays to the relay and wait for the lines that changed
/// to fill (plan §1.1.1).
///
/// The exact value stays in the chain's own arithmetic and only the *pushed* value is
/// rounded to whole milliseconds, so a step's Δ cannot accumulate rounding. For an
/// overlap that rounding is not even an assumption: the next position measures the
/// overlap *through* the line, so what was applied is observed.
async fn apply_provisional(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    changed: &[(String, f64)],
) -> Result<(), Refusal> {
    if changed.is_empty() {
        return Ok(());
    }
    let mut waiting: Vec<String> = Vec::new();
    for (name, delay_ms) in changed {
        let applied = delay_ms.round().clamp(0.0, f64::from(u16::MAX)) as u16;
        deps.relay.set_delay_ms(name, applied).map_err(|e| {
            Refusal::for_member(
                RefusalKind::ProvisionalRange,
                name,
                format!(
                    "the provisional delay line refused {applied} ms on '{name}': {e}. A chain can only ever *add* delay, so its floor \
                     ratchets upward across an apartment (plan §1.1) — and the renormalisation that takes it back out only happens once, \
                     at the end. This chain has run past what the line can hold: align fewer positions per run, or align the loudest \
                     room first so the ratchet starts from a smaller number.",
                ),
            )
        })?;
        {
            let mut g = inner.lock_recover();
            g.provisional.insert(name.clone(), *delay_ms);
            if g.relay.is_none() {
                g.relay = Some(deps.relay.clone());
            }
        }
        if applied > 0 {
            waiting.push(name.clone());
        }
    }
    let deadline = Instant::now() + PROVISIONAL_PRIME_TIMEOUT;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
        }
        waiting.retain(|name| !deps.relay.status(name).is_none_or(|s| s.primed));
        if waiting.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Refusal::for_member(
                RefusalKind::ProvisionalRange,
                &waiting[0],
                format!(
                    "the provisional delay line on '{}' never filled ({} s). A line fills from the audio flowing to that output, so this \
                     says no audio is reaching it — and a position measured through a half-filled line reads as a dropout rather than as \
                     a delay, which is why this refuses instead of measuring.",
                    waiting[0],
                    PROVISIONAL_PRIME_TIMEOUT.as_secs()
                ),
            ));
        }
        set_phase(inner, Phase::Measuring, format!("waiting for the provisional delay on {} speaker(s) to take effect", waiting.len()));
        tokio::time::sleep(deps.timing.poll.min(deadline - now)).await;
    }
}

/// Assemble what the UI reads about the chain. One place, so the aligned set, the
/// provisional delays and the error statement cannot disagree with each other.
#[allow(clippy::too_many_arguments)] // one snapshot's worth of state
fn chain_progress(
    next: ChainAction,
    steps: &[ChainStep],
    aligned: &[String],
    remaining: &[String],
    provisional: &HashMap<String, f64>,
    measuring: Option<usize>,
    restarts: u32,
    prompt: String,
    refusal: Option<Refusal>,
) -> ChainProgress {
    let mut lines: Vec<ProvisionalDelay> = provisional
        .iter()
        .map(|(node_name, delay_ms)| ProvisionalDelay {
            node_name: node_name.clone(),
            delay_ms: *delay_ms,
            applied_ms: delay_ms.round().clamp(0.0, f64::from(u16::MAX)) as u16,
        })
        .collect();
    lines.sort_by(|a, b| a.node_name.cmp(&b.node_name));
    // The ratchet §1.1 warns about. Zero before anything is aligned, rather than an
    // infinity a UI would render as a number.
    let floor = provisional.values().copied().fold(f64::INFINITY, f64::min);
    ChainProgress {
        next,
        steps: steps.to_vec(),
        aligned: aligned.to_vec(),
        remaining: remaining.to_vec(),
        floor_ms: if floor.is_finite() { floor } else { 0.0 },
        provisional: lines,
        measuring,
        restarts,
        prompt,
        error: chain_error(steps),
        refusal,
        scope_note: CHAIN_SCOPE_NOTE,
    }
}

fn set_chain(inner: &Arc<Mutex<Inner>>, chain: ChainProgress) {
    let mut g = inner.lock_recover();
    g.chain = Some(chain);
    g.bump();
}

/// The sentence the user reads while the chain waits for them to move.
fn chain_prompt(next: ChainAction, remaining: &[String], done: usize) -> String {
    match (next, done) {
        (ChainAction::Finish, _) => "every speaker has been aligned at some position. POST /api/align/measure/finish to take the \
             accumulated delay back out — a common shift, so nothing moves relative to anything else — and see the knobs that would be \
             written. You can still post another position first: re-linking a region through more overlaps only makes the chain tighter."
            .to_string(),
        (_, 0) => format!(
            "sit where you listen to the first set of speakers. Post the ones you can hear clearly from there to \
             /api/align/measure/position — that set is aligned *for that spot*, and it becomes the reference the rest of the chain is \
             built on. {} speaker(s) to align: {}.",
            remaining.len(),
            remaining.join(", ")
        ),
        _ => format!(
            "move to the next listening position, then post the speakers you can hear from there **plus one or two you have already \
             aligned and can still hear from here**. Two overlaps are what let this joint be checked against itself; one is accepted but \
             cannot be checked, and the chain's total error stops being boundable. {} speaker(s) left: {}.",
            remaining.len(),
            remaining.join(", ")
        ),
    }
}

/// WRITING → SETTLING → VERIFYING → DONE.
async fn run_apply(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    proposal: &Proposal,
    cmd_rx: Option<tokio::sync::mpsc::UnboundedReceiver<RunCommand>>,
) -> Result<Phase, Refusal> {
    let session = bind(deps, inner, cancel).await?;
    let pattern_ms = deps.timing.pattern_ms;
    let rate = deps.mic.status().sample_rate;

    // One reconnect wave: every write is issued back to back, and a member whose
    // delay is unchanged is not written at all — writing it would reconnect a
    // device for nothing (plan §2.3: tens of seconds each).
    set_phase(inner, Phase::Writing, "writing the solved delays");
    let mut wrote = 0usize;
    for m in &proposal.members {
        if m.new_delay_ms == m.current_delay_ms {
            continue;
        }
        match deps.writer.write(m.node_name.clone(), m.kind, m.new_delay_ms).await {
            Ok(msg) => {
                wrote += 1;
                inner.lock_recover().mark_written(&m.node_name);
                tracing::info!("alignment write: {msg}");
            }
            Err(e) => {
                return Err(Refusal::for_member(
                    RefusalKind::WriteFailed,
                    &m.node_name,
                    format!("writing '{}''s delay failed: {e}. Use revert to restore the delays from before this session.", m.node_name),
                ))
            }
        }
    }
    // The real knobs now carry what the delay lines were standing in for, so the lines
    // have to go — otherwise every chained member would be delayed twice (plan §1.1.1:
    // the provisional delay is a *stand-in* for the knob, not an addition to it). Done
    // immediately after the write wave rather than before it, so nothing is briefly
    // un-delayed while the writes are being issued; the reconnect-length gate below
    // absorbs the transient either way.
    let cleared = inner.lock_recover().clear_provisional();
    if cleared > 0 {
        tracing::info!("alignment chain: dropped {cleared} provisional delay line(s); the written knobs carry them now");
    }

    if wrote == 0 {
        set_phase(inner, Phase::Verifying, "nothing to write — the group was already aligned; verifying");
    } else {
        set_phase(inner, Phase::Settling, format!("settling: {wrote} device(s) reconnect to pick their new delay up"));
        sleep_cancellable(deps.timing.settle_grace, deps.timing.poll, cancel).await?;
    }

    set_phase(inner, Phase::Verifying, "verifying");
    // A chain can only be checked where the phone is, which is the **last** position.
    // Its own set — the position's speakers and its overlaps, which that step's Δ put in
    // step with them — is the one set that is genuinely aligned here; every other
    // position was aligned somewhere else, and measuring it from here would report a
    // correct chain as broken for exactly the reason §10.4 gives for a walk.
    let chain_scope: Option<(Vec<String>, usize)> = inner.lock_recover().chain.as_ref().and_then(|c| {
        c.steps.last().map(|last| {
            let mut set = last.members.clone();
            set.extend(last.overlaps.iter().map(|o| o.node_name.clone()));
            (set, c.steps.len())
        })
    });
    let observations = if deps.mode.is_walk() {
        // A near-field write can only be checked from where it was measured — at the
        // speakers. See [`WalkPurpose::Verify`]: a stationary residual would measure
        // the phone's distance to each speaker and fail every time.
        let mut rx = cmd_rx
            .ok_or_else(|| Refusal::new(RefusalKind::Internal, "a near-field verification was started without a way to accept arrivals"))?;
        let (observations, closure) = run_walk(deps, inner, cancel, &mut rx, WalkPurpose::Verify, &session, rate).await?;
        if !closure.passed {
            return Err(Refusal::for_member(
                RefusalKind::ClosureError,
                &closure.anchor,
                format!(
                    "the check walk did not close: '{}' read {:.2} ms differently at the end than at the start (limit {:.1} ms over \
                     {:.0} s). The written delays may well be right, but this walk cannot confirm them — nothing was changed by \
                     the check itself, so revert if you want the previous delays back, or walk the check again.",
                    closure.anchor, closure.error_ms, closure.tolerance_ms, closure.span_s
                ),
            ));
        }
        observations
    } else {
        let mut observations = Vec::new();
        for pass in 0..VERIFY_PASSES {
            let mut order: Vec<&SessionMember> = session.members.iter().collect();
            if let Some((set, _)) = chain_scope.as_ref() {
                order.retain(|m| set.contains(&m.node_name));
            }
            if pass % 2 == 1 {
                order.reverse();
            }
            for member in order {
                set_phase(inner, Phase::Verifying, format!("verifying '{}'", member.node_name));
                let level =
                    inner.lock_recover().members.iter().find(|m| m.node_name == member.node_name).map(|m| m.level).unwrap_or(session.level);
                // The reconnect-length gate: a written device is silent for tens of
                // seconds before it renders again (plan §2.3).
                let cfg = GateConfig::reconnect(&deps.timing);
                match measure_member(deps, inner, cancel, member, level, cfg, pass, u64::MAX, rate).await {
                    Ok(o) => observations.push(o),
                    Err(StepError::Refuse(r)) | Err(StepError::RestartSet(r)) => return Err(r),
                }
            }
        }
        observations
    };

    // The reference has to be inside the set that was actually re-measured, so a chain
    // measures its residual against whichever of the last position's speakers ended with
    // the smallest knob — the same "everyone was moved towards this one" rule the
    // single-position solve uses, restricted to what the phone can hear from here.
    let reference = match chain_scope.as_ref() {
        None => proposal.reference.clone(),
        Some((set, _)) => proposal
            .members
            .iter()
            .filter(|m| set.contains(&m.node_name))
            .min_by(|a, b| a.new_delay_ms.cmp(&b.new_delay_ms).then_with(|| a.node_name.cmp(&b.node_name)))
            .map(|m| m.node_name.clone())
            .unwrap_or_else(|| proposal.reference.clone()),
    };
    let residual = residual(&observations, &reference, pattern_ms, RESIDUAL_TOL_MS);
    let trans = transitivity(&observations, &deps.timing, TRANSITIVITY_TOL_MS);
    let passed = residual.passed && trans.passed;
    let verification = Verification {
        residual: residual.clone(),
        transitivity: trans.clone(),
        merged_peak: MergedPeakCheck::seam(),
        observations,
        passed,
        scope_note: chain_scope.as_ref().map(|(set, positions)| {
            format!(
                "this checked the last of {positions} position(s) only — the {} speaker(s) that position aligned, measured against '{}' \
                 from where the phone is now. The earlier positions were aligned at *their* spots, so a reading of them from here would \
                 be their distance to this spot rather than the write, and it would fail however correct the chain is. Re-checking them \
                 means walking the chain again.",
                set.len(),
                reference
            )
        }),
    };
    {
        let mut g = inner.lock_recover();
        g.verification = Some(verification);
        g.bump();
    }
    if !trans.passed {
        let (a, b) = trans.worst_pair.unwrap_or_default();
        return Err(Refusal::new(
            RefusalKind::Transitivity,
            format!(
                "after writing, the two test tones disagree by {:.2} ms about '{a}' vs '{b}' (limit {:.1} ms), so the \
                 delays that were written cannot be trusted — revert, move the phone away from walls, and measure again.",
                trans.worst_ms, trans.tolerance_ms
            ),
        ));
    }
    if !residual.passed {
        let who = residual.worst_member.clone().unwrap_or_default();
        return Err(Refusal::for_member(
            RefusalKind::ResidualTooLarge,
            &who,
            format!(
                "after writing and settling, '{who}' still arrives {:.2} ms away from the reference (limit {:.1} ms). \
                 The delay may not have taken effect yet, or the first measurement was wrong — revert and measure again.",
                residual.worst_ms, residual.tolerance_ms
            ),
        ));
    }
    Ok(Phase::Done)
}

/// The session/mic binding (plan §11: the measurement needs both).
async fn bind(deps: &MeasureDeps, inner: &Arc<Mutex<Inner>>, cancel: &AtomicBool) -> Result<SessionSnapshot, Refusal> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
    }
    let session = deps.session.snapshot().await;
    if !session.active {
        return Err(Refusal::new(
            RefusalKind::SessionLost,
            "the alignment session stopped, so nothing is playing to measure — the microphone is still connected. \
             Start the session again.",
        ));
    }
    let expected = inner.lock_recover().sources.clone();
    if !expected.is_empty() && !same_set(&expected, &session.sources) {
        return Err(Refusal::new(
            RefusalKind::SessionChanged,
            format!("the alignment session moved to a different group ({:?}), so this measurement no longer applies", session.sources),
        ));
    }
    let mic = deps.mic.status();
    if !mic.connected {
        return Err(Refusal::new(
            RefusalKind::MicLost,
            "the microphone capture disconnected — the alignment session is still running, but there is nothing to \
             measure with. Reopen the capture on the phone.",
        ));
    }
    Ok(session)
}

fn same_set(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

async fn sleep_cancellable(d: Duration, poll: Duration, cancel: &AtomicBool) -> Result<(), Refusal> {
    let deadline = Instant::now() + d;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
        }
        tokio::time::sleep(poll.min(deadline - now)).await;
    }
}

/// Solo one member, pass the gate, and take the estimate over the window the gate
/// approved.
#[allow(clippy::too_many_arguments)] // one measurement's worth of context; a struct would only move the list
async fn measure_member(
    deps: &MeasureDeps,
    inner: &Arc<Mutex<Inner>>,
    cancel: &AtomicBool,
    member: &SessionMember,
    level: u8,
    cfg: GateConfig,
    pass: usize,
    grid_epoch: u64,
    rate: u32,
) -> Result<MemberObservation, StepError> {
    let name = member.node_name.as_str();
    let timing = &deps.timing;
    deps.session
        .solo(name.to_string(), level)
        .await
        .map_err(|e| StepError::Refuse(Refusal::for_member(RefusalKind::SessionLost, name, format!("could not solo '{name}': {e}"))))?;

    let mut est = Estimator::new(estimator_config(rate, timing.pattern_ms))
        .map_err(|e| StepError::Refuse(Refusal::for_member(RefusalKind::Internal, name, e)))?;
    let mic = deps.mic.clone();
    let mut feeder = Feeder::new(mic.as_ref(), rate, timing.pattern_ms);
    let mut gate = Gate::new(cfg).for_member(name);

    // Plan §6.1's guard: a mute lands somewhere inside the stream's send-ahead
    // window, so nothing captured until it has surely landed can be judged.
    sleep_cancellable(timing.mute_guard, timing.poll, cancel).await.map_err(StepError::Refuse)?;
    feeder.arm();
    let started = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StepError::Refuse(Refusal::new(RefusalKind::Cancelled, "abandoned")));
        }
        bind(deps, inner, cancel).await.map_err(StepError::Refuse)?;

        let pulled = feeder.pull(&mut est);
        let elapsed = started.elapsed();

        // Exclusivity is real but deliberately not absolute (plan §12.3): a barge-in
        // announcement and a voice-duck hold both outrank the alignment hold, because
        // nobody wants an alarm suppressed by a calibration. What must not happen is
        // losing *why* — so the cause is carried into the gate, and an entry for
        // another member is kept as a warning rather than dropped on the floor.
        let mut interference: Option<String> = None;
        for i in deps.session.take_interference().await {
            inner.lock_recover().warn(Warning::new(WarningKind::Interference, i.reason.clone()));
            if i.member == name {
                interference = Some(i.reason);
            }
        }
        // A period boundary is the gate's tick. A disturbance is judged
        // immediately, because waiting a whole period to notice a disconnect would
        // make every failure two seconds slower to explain.
        let ticks = if pulled.disconnected || pulled.reconnected || pulled.gap || pulled.clipped { 1 } else { pulled.new_periods };
        for _ in 0..ticks {
            // One aggregation per tick: `estimate()` is a read, but it medians and
            // line-fits every retained period, so it is not free enough to call
            // twice.
            let e = est.estimate();
            let sample = GateSample {
                elapsed,
                connected: !pulled.disconnected,
                reconnected: pulled.reconnected,
                gap: pulled.gap,
                clipped: pulled.clipped,
                peak: feeder.last_peak(),
                periods_used: e.channels.iter().map(|c| c.periods_used).min().unwrap_or(0),
                quality: e.quality.clone(),
                interference: interference.take(),
            };
            let step = gate.observe(&sample);
            {
                let mut g = inner.lock_recover();
                g.gate = Some(step.progress.clone());
                // `note` bumps the change notifier, so the gate's progress reaches
                // `measure_ws` in the same push as the note it belongs to.
                g.note(name, Some(step.progress.message.clone()));
            }
            if gate.aec_suspected() {
                inner.lock_recover().warn(Warning::new(
                    WarningKind::AecSuspected,
                    "the tone's level decayed monotonically during a measurement, which is the behavioural signature of \
                     echo cancellation converging (plan §4.2). Treat every number here with suspicion until it is off.",
                ));
            }
            if let Some(failed) = step.failed {
                let mut r = failed;
                r.member = Some(name.to_string());
                return Err(if r.kind == RefusalKind::MicReconnected { StepError::RestartSet(r) } else { StepError::Refuse(r) });
            }
            if step.restart {
                if pulled.reconnected {
                    return Err(StepError::RestartSet(Refusal::for_member(
                        RefusalKind::MicReconnected,
                        name,
                        "the microphone capture reconnected, which restarts the timing reference every earlier \
                         measurement was on — measuring the group again from the start",
                    )));
                }
                est.reset();
                feeder.arm();
                break;
            }
            if step.locked {
                let (Some(a), Some(b)) = (e.channel(CLICK_A_LABEL), e.channel(CLICK_B_LABEL)) else {
                    return Err(StepError::Refuse(Refusal::for_member(
                        RefusalKind::Internal,
                        name,
                        "the estimator returned no A/B channels",
                    )));
                };
                inner.lock_recover().note(name, None);
                return Ok(MemberObservation {
                    node_name: name.to_string(),
                    pass,
                    grid_epoch,
                    period_centre: feeder.period_centre(),
                    m: MemberMeasurement {
                        phase_a_ms: a.phase_ms,
                        phase_b_ms: b.phase_ms,
                        std_error_ms: a.std_error_ms.max(b.std_error_ms),
                        peak_snr_db: a.peak_snr_db.min(b.peak_snr_db),
                        second_peak_ratio: a.second_peak_ratio.min(b.second_peak_ratio),
                        drift_ppm: a.drift_ppm,
                        periods_used: a.periods_used.min(b.periods_used),
                    },
                });
            }
        }
        tokio::time::sleep(timing.poll).await;
    }
}

/// The estimator's configuration for a given capture rate and pattern: the
/// existing click track's two frequency channels (plan §2.2 — every member emits
/// both), on whatever pattern the anchor is actually looping.
fn estimator_config(rate: u32, pattern_ms: f64) -> EstimatorConfig {
    EstimatorConfig { pattern_secs: pattern_ms / 1000.0, ..EstimatorConfig::click_track(rate) }
}

// ---- Signal check (plan §12's per-channel SNR readout) --------------------

/// How much recent audio the **pre-flight** signal check analyses, in pattern
/// periods.
///
/// **Deliberately shorter than [`GATE_MIN_PERIODS`], and they must not be merged**
/// (plan §12.2). They answer different questions:
///
/// * the gate decides whether a *phase* may be written into a speaker's delay, so
///   it needs [`MIN_PERIODS_USED`] complete periods plus a period of margin — a
///   line fit with a residual, and therefore a standard error;
/// * the pre-flight decides whether a user standing in the room with a volume
///   slider is loud enough yet. It needs a rough SNR and nothing else, and its
///   window is also its **staleness**: with four periods, a slider move takes 8 s
///   to show up, which is not a control, it is a lag.
///
/// Two periods is the floor rather than a preference. The estimator only closes a
/// pattern period it saw *whole* (`PeriodAcc::close`), so the partial periods at
/// each end of an arbitrary window are dropped: two periods of audio yield exactly
/// one analysed period, one period would yield none. [`signal_check_window`]
/// therefore also has to handle "nothing closed yet" as its own answer rather than
/// grading an empty median as 0 dB.
const PREFLIGHT_PERIODS: usize = 2;

/// The two windows above are a *deliberate* difference, checked at compile time so
/// that tidying them into one constant fails the build rather than silently making
/// the level slider unusable (or the measurement gate unsound).
const _: () = assert!(PREFLIGHT_PERIODS < GATE_MIN_PERIODS, "the pre-flight verdict must be faster than the measurement gate");
const _: () = assert!(GATE_MIN_PERIODS > MIN_PERIODS_USED, "the gate needs the estimator's floor plus margin");

/// What the level is good for, in the only terms that decide it.
///
/// The mic meter (`align_mic::MicStatus::peak`) cannot answer this: it is a
/// *decaying broadband* peak, and the calibration click is an 8 ms burst once per
/// second — a 0.8 % duty cycle — so the meter reads anywhere between the true
/// burst peak and ~20 dB below it depending on when it samples. It is a
/// "the mic is alive" indicator. Whether a measurement can *succeed* is decided by
/// per-channel peak SNR, which is what this reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalVerdict {
    /// At or above [`TARGET_PEAK_SNR_DB`] — margin for the floor to move.
    Good,
    /// Measurable, but inside the ~3 dB cliff's shadow (plan §5.4.1): it will
    /// work now and may not after someone starts a dishwasher.
    Marginal,
    /// Below the estimator's own refusal threshold — a measurement would refuse.
    TooQuiet,
    /// Clipped or gapped: no level change fixes it (plan §5.5 refuses either way).
    Unusable,
}

/// One measurement channel's live signal quality.
#[derive(Debug, Clone, Serialize)]
pub struct SignalChannel {
    pub label: String,
    pub center_hz: f64,
    pub peak_snr_db: f64,
    pub second_peak_ratio: f64,
    /// Arrival phase on the shared grid. Meaningless in isolation (§3) — shown
    /// only so a user can see it is stable between polls.
    pub phase_ms: f64,
    pub periods_used: usize,
}

/// Plan §12's per-channel SNR readout: is the capture good enough to measure?
///
/// Deliberately **session-independent and side-effect free** — it starts nothing,
/// mutes nothing and writes nothing. It answers the question a user has while
/// holding a phone and turning a volume knob, which is exactly when they must not
/// be forced to commit to a run to find out.
#[derive(Debug, Clone, Serialize)]
pub struct SignalCheck {
    pub verdict: SignalVerdict,
    /// One sentence naming the problem and the action, if any.
    pub message: String,
    pub sample_rate: u32,
    pub periods: usize,
    pub gap: bool,
    pub clipped: bool,
    /// The channel that decides the verdict — a measurement is only as good as
    /// its worst channel, so an average would flatter a capture that cannot work.
    pub worst_peak_snr_db: Option<f64>,
    pub channels: Vec<SignalChannel>,
}

impl SignalCheck {
    fn unusable(message: impl Into<String>, sample_rate: u32) -> Self {
        Self {
            verdict: SignalVerdict::Unusable,
            message: message.into(),
            sample_rate,
            periods: 0,
            gap: false,
            clipped: false,
            worst_peak_snr_db: None,
            channels: Vec::new(),
        }
    }
}

/// Run the signal check over the live ingest. `None` from the mic means no
/// capture is connected or too little audio has arrived yet.
pub fn signal_check(pattern_ms: f64) -> SignalCheck {
    let status = crate::align::mic::shared().status();
    if !status.connected {
        return SignalCheck::unusable("No microphone is connected — press start on the capture control.", status.sample_rate);
    }
    let rate = status.sample_rate;
    let frames = ((pattern_ms / 1000.0) * f64::from(rate) * PREFLIGHT_PERIODS as f64) as usize;
    match crate::align::mic::shared().window(frames) {
        Some(w) => signal_check_window(&w, pattern_ms),
        None => SignalCheck::unusable(
            format!("Still collecting audio — {PREFLIGHT_PERIODS} pattern periods are needed before the level can be judged."),
            rate,
        ),
    }
}

/// The pure half, so the verdict logic is testable without a microphone.
pub fn signal_check_window(w: &MicWindow, pattern_ms: f64) -> SignalCheck {
    if w.clipped {
        return SignalCheck::unusable(
            "The capture is clipping — turn the speakers down; no level of loudness fixes a clipped microphone.",
            w.sample_rate,
        );
    }
    if w.gap {
        return SignalCheck::unusable(
            "Audio blocks were lost on the way to the daemon — check the network before judging the level.",
            w.sample_rate,
        );
    }
    let mut est = match Estimator::new(estimator_config(w.sample_rate, pattern_ms)) {
        Ok(e) => e,
        Err(e) => return SignalCheck::unusable(format!("Cannot analyse this capture: {e}"), w.sample_rate),
    };
    est.push_block(w.first_frame, &w.samples);
    let estimate = est.estimate();

    let channels: Vec<SignalChannel> = estimate
        .channels
        .iter()
        .map(|c| SignalChannel {
            label: c.label.clone(),
            center_hz: c.center_hz,
            peak_snr_db: c.peak_snr_db,
            second_peak_ratio: c.second_peak_ratio,
            phase_ms: c.phase_ms,
            periods_used: c.periods_used,
        })
        .collect();

    // With the pre-flight's short window ([`PREFLIGHT_PERIODS`]) it is possible for
    // *no* pattern period to have closed — the estimator only keeps a period it saw
    // whole, and a two-period window that happens to sit exactly on the grid has a
    // partial period at each end and nothing in between. Its SNR is then 0 dB by
    // construction (the median of an empty set), which would read as "far too quiet"
    // and send the user to turn the speakers up for no reason.
    //
    // Deliberately gated on `periods_seen` (periods *closed*) and not on a channel's
    // `periods_used` (periods that contained a detectable burst): a window whose
    // periods all closed but were too quiet to use must still be graded `TooQuiet`,
    // which is the whole point of the estimator reporting SNR from `all` in that
    // case.
    if estimate.periods_seen == 0 {
        return SignalCheck::unusable(
            "Still collecting audio — no complete test-pattern period has been captured yet.".to_string(),
            w.sample_rate,
        );
    }

    // The worst channel decides: both bursts have to be measurable, because §10.2's
    // transitivity check compares them against each other.
    let worst = channels.iter().map(|c| c.peak_snr_db).fold(f64::INFINITY, f64::min);
    let worst = if worst.is_finite() { Some(worst) } else { None };

    let (verdict, message) = match worst {
        None => (SignalVerdict::Unusable, "No measurement channels were analysed.".to_string()),
        Some(snr) if snr >= TARGET_PEAK_SNR_DB => {
            (SignalVerdict::Good, format!("Good level: {snr:.0} dB on the weaker tone, with margin to spare."))
        }
        Some(snr) if snr >= MIN_PEAK_SNR_DB => (
            SignalVerdict::Marginal,
            format!(
                "Usable but tight: {snr:.0} dB on the weaker tone, against {TARGET_PEAK_SNR_DB:.0} dB wanted. \
                 Raise the speakers or move the phone closer, or a little extra room noise will spoil the measurement."
            ),
        ),
        Some(snr) => (
            SignalVerdict::TooQuiet,
            format!(
                "Too quiet to measure: {snr:.0} dB on the weaker tone, and the estimator refuses below \
                 {MIN_PEAK_SNR_DB:.0} dB. Raise the speakers, move the phone closer, or quieten the room."
            ),
        ),
    };

    SignalCheck {
        verdict,
        message,
        sample_rate: w.sample_rate,
        periods: estimate.periods_seen,
        gap: false,
        clipped: false,
        worst_peak_snr_db: worst,
        channels,
    }
}

// ---- W21: is a relay-side delay a device-side delay? (plan §1.1.1) --------
//
// The deferred-write scheme rests on an assumption: a provisional delay of *d* in the
// relay (`align/relay_delay.rs`) and a knob of *d* on the device produce the same audible
// shift. If they do not, every position the user walks is verified against a geometry
// the real write then fails to reproduce, and §10's verification only finds out at the
// end of the apartment. This section retires the assumption by measuring it — once,
// early, on **one** speaker — and reports what it found rather than acting on it.
//
// ## What it actually measures, which is not what §1.1.1 says
//
// §1.1.1 expects "a per-transport constant to correct for". It cannot be that, and the
// reason is §1.1.2 item 3: the device arm has to be a **difference of two
// post-reconnect readings**, because a reconnect shifts that device's offset by some ε
// that a single reading cannot separate from the knob. A difference cancels every
// constant — including exactly the constant §1.1.1 wanted. So this experiment measures
// the **scale and the sign** of the device knob against the delay line, i.e. "does a
// knob change of *N* move the sound by *N*, and in which direction", and that is the
// right quantity anyway:
//
// * a **sign** error inverts the whole solve (§2.4.1);
// * a **scale** error *g* leaves every member wrong by `(g−1)·d_i`, which is not a
//   common shift and cannot be absorbed;
// * a **constant** per transport kind *is* a common shift, and a common shift is free
//   (§2.4.2) — so within one kind it is harmless, and *between* kinds no experiment on
//   one speaker can see it.
//
// It reports the ε it stumbles over on the way ([`EquivalenceReport::reconnect_epsilon_ms`]),
// which is the first actual number this design has for the quantity item 3 is about.
//
// ## Why every arm is bracketed, which §1.1.2 item 3 did not budget for
//
// Two post-reconnect readings are still not enough. A reconnect takes tens of seconds
// (§2.3) and the mic-vs-audio clock runs at up to ~100 ppm (§5.4.1) — 6 ms of phase
// creep over a 60 s reconnect, against a step of 20 ms. The step cannot simply be made
// larger, because §9.2's send-ahead high-water mark caps it (see [`EQUIV_STEP_MS`]). So
// each arm is measured as **baseline → changed → baseline**, and the shift is taken
// against the *mean* of the two baselines: that cancels linear drift exactly, without
// having to trust a drift estimate. It costs the device arm a third write, and it is
// what makes the numbers mean anything.

/// The knob step both arms apply — plan §1.1.1's *N*, in milliseconds.
///
/// **20 ms, and what set it.** In order of how binding each consideration is:
///
/// * **Exactly one wire-codec frame** (`outputs::sendspin::codec::OPUS_FRAME_FRAMES` = 960 frames
///   = 20 ms at 48 kHz), and this is the *deciding* one. §1.1.2 item 2: a relay-side
///   delay that is not a whole codec frame moves a transient to a different position
///   inside the MDCT window, so the measured *peak* can move by a fraction of a frame
///   that the device knob would never produce. A step of exactly one frame leaves the
///   content-to-frame phase untouched, which nulls that confound **for this
///   measurement** — it does not remove it from a real, arbitrary alignment delay.
/// * **Big enough to dwarf the estimator.** §5.4.1's worst *accepted* delta error is
///   0.14 ms (a reverb train) and its `std_error_ms` refusal threshold is 1 ms, so this
///   is ~140× the former and 20× the latter.
/// * **Small enough not to lift the group's send-ahead high-water mark** (§9.2): a
///   sendspin advance is added to the group lead, and crossing the mark restarts the
///   *whole group's* stream instead of one device. [`plan_equivalence`] checks that
///   against the real numbers and **refuses** rather than shrinking the step — a
///   smaller step would give up the codec-frame property above and shrink the signal
///   against the clock drift the brackets are already fighting.
/// * Far inside the ±½-pattern wrap (±1000 ms), so no shift measured here is ambiguous.
pub const EQUIV_STEP_MS: u16 = 20;

// The property the docs above claim, asserted rather than trusted: if the wire codec's
// frame size ever changes, §1.1.2 item 2's confound is silently back in the numbers.
const _: () = assert!(
    EQUIV_STEP_MS as usize * (crate::pw::capture::SAMPLE_RATE as usize / 1000) == crate::outputs::sendspin::codec::OPUS_FRAME_FRAMES,
    "the equivalence step must be exactly one Opus frame, or a relay-side delay moves the codec's window phase and the device's does not"
);

/// Smallest arm-to-arm discrepancy this experiment will call real, in ms.
///
/// Not a statistical bound — that is [`EquivalenceReport::uncertainty_ms`] — but a
/// *usefulness* one: the knobs are integer milliseconds and pw-sink's has a 15 ms floor
/// (§1.1.2 item 4), so nothing downstream could act on a correction finer than half a
/// millisecond even if this measurement resolved it.
pub const EQUIV_MIN_MEANINGFUL_MS: f64 = 0.5;

/// Readings one experiment takes: three per arm (baseline, changed, baseline).
pub const EQUIV_STEPS: usize = 6;

/// How long the relay delay line is given to fill after the step is applied.
///
/// `relay_delay`'s docs make this an orchestration precondition rather than a detail: an
/// un-primed line emits silence, and a measurement taken through it sees a dropout the
/// gate would report as something else entirely. Filling costs exactly
/// [`EQUIV_STEP_MS`] of audio, so this is generous — and if it *does* expire, the useful
/// conclusion is that no audio is reaching that output at all.
const EQUIV_PRIME_TIMEOUT: Duration = Duration::from_secs(10);

/// The provisional delay line (`align/relay_delay.rs`), as this module drives it: the
/// equivalence experiment's relay arm, and every step of a chain (plan §1.1.1).
///
/// A trait for the same reason [`MicFeed`] and [`DelayWriter`] are: both callers have to
/// be exercised without a PipeWire graph, and the line's own unit tests already cover the
/// sample arithmetic.
pub trait RelayControl: Send + Sync {
    /// Apply a provisional delay of `delay_ms` to `output` (`0` clears it).
    fn set_delay_ms(&self, output: &str, delay_ms: u16) -> Result<(), String>;
    /// What the line is doing — the **applied** frame count and whether it has primed.
    fn status(&self, output: &str) -> Option<crate::align::relay_delay::DelayStatus>;
    /// Drop `output`'s provisional delay. Infallible: it is a teardown step.
    fn clear(&self, output: &str);
}

/// The process-global delay line the three relays actually read. Constructed by the API
/// handler that assembles [`MeasureDeps`], so a run and the equivalence experiment hold
/// the same handle rather than two that could differ.
pub struct LiveRelay;

impl RelayControl for LiveRelay {
    fn set_delay_ms(&self, output: &str, delay_ms: u16) -> Result<(), String> {
        crate::align::relay_delay::RelayDelay::global()
            .set_delay_us(output, u64::from(delay_ms) * 1_000)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn status(&self, output: &str) -> Option<crate::align::relay_delay::DelayStatus> {
        crate::align::relay_delay::RelayDelay::global().status(output)
    }

    fn clear(&self, output: &str) {
        crate::align::relay_delay::RelayDelay::global().clear(output);
    }
}

/// Everything the equivalence experiment needs, assembled by the API handler.
///
/// [`Self::base`] is the *same* bundle a measurement run takes, deliberately: this
/// experiment solos through the same session, measures through the same mic and gate,
/// and writes through the same endpoint-backed [`DelayWriter`], so nothing about
/// persistence, clamping, the per-device reconnect or its group-wide high-water
/// exception is duplicated here (plan §9.3). It also carries the delay line the relay arm
/// drives — one handle rather than two — so `mode`, `chained` and `link_to` are the only
/// unused parts.
pub struct EquivalenceDeps {
    pub base: MeasureDeps,
    /// Override the member the experiment runs on. `None` lets
    /// [`plan_equivalence`] choose, which is the intended path — the choice is a
    /// property of the *transport*, not a preference.
    pub member: Option<String>,
}

/// Which member the experiment runs on, and the step it will take on its knob.
#[derive(Debug, Clone, Serialize)]
pub struct EquivalencePlan {
    pub member: String,
    pub kind: MemberKind,
    pub knob: Knob,
    /// The knob value both baselines are read at — the member's **current** value, so
    /// the baselines describe the user's own configuration and the happy path leaves
    /// the knob where it found it.
    pub from_ms: u16,
    /// What the knob was persisted as, before being clamped into the knob's own range.
    /// Differs from [`Self::from_ms`] only for a member sitting below its floor (a
    /// pw-sink output with no override), and it is what the restore writes back — so the
    /// user gets their own value, not the one this experiment had to start from.
    pub stored_ms: u16,
    pub to_ms: u16,
    /// `to − from`. Negative when the knob had no headroom upwards, in which case the
    /// measured shift is negated before it is compared (see [`EquivalenceReport`]).
    pub delta_ms: i32,
    pub why_member: String,
    pub why_step: String,
}

/// Which kind is preferred, and why it is not a preference.
///
/// **sendspin first.** Its knob is the only [`KnobPolarity::Advance`] in the design, so
/// it is the only member on which the *sign* can be confirmed rather than assumed —
/// §2.4.1 settled the sign from five independent readings of the code, and this is the
/// one place a measurement can disagree with them. It is also the transport whose write
/// costs the tens of seconds §2.3 is about, i.e. the transport the whole deferred-write
/// scheme exists for.
///
/// **AP2 second.** Its knob is a plain delay, so there is no sign question, and
/// `api.rs`'s handler pushes it *live* to the running stream — so its "reconnect" may
/// not happen at all, which makes it a poor probe of the ε item 3 is about.
///
/// **pw-sink last.** Its knob is floored at `PWSINK_JITTER_MIN_MS`, so its baseline
/// cannot be a device zero, and a write reloads the receiving module (an audible gap of
/// its own).
fn equiv_kind_rank(kind: MemberKind) -> u8 {
    match kind {
        MemberKind::Sendspin => 0,
        MemberKind::Airplay2 => 1,
        MemberKind::PwSink => 2,
    }
}

/// Choose the member and the step, or refuse — the whole "run it on one speaker,
/// deliberately" decision in one pure function.
pub fn plan_equivalence(
    members: &[SessionMember],
    current_delays: &HashMap<String, u16>,
    send_ahead: &SendAheadContext,
    requested: Option<&str>,
) -> Result<EquivalencePlan, Refusal> {
    if members.is_empty() {
        return Err(Refusal::new(
            RefusalKind::Internal,
            "the alignment session is holding no members, so there is no speaker to measure the equivalence on",
        ));
    }
    let mut ranked: Vec<&SessionMember> = members.iter().collect();
    ranked.sort_by(|a, b| equiv_kind_rank(a.kind).cmp(&equiv_kind_rank(b.kind)).then_with(|| a.node_name.cmp(&b.node_name)));
    if let Some(name) = requested {
        let m = members.iter().find(|m| m.node_name == name).ok_or_else(|| {
            Refusal::for_member(
                RefusalKind::Internal,
                name,
                format!("'{name}' is not a member of the group this session is holding, so its knob cannot be measured here"),
            )
        })?;
        ranked = vec![m];
    }
    let step = EQUIV_STEP_MS;
    let others = ranked.len().saturating_sub(1);
    let mut blocked: Vec<String> = Vec::new();
    for m in ranked {
        let knob = knob_of(m.kind);
        let stored = current_delays.get(&m.node_name).copied().unwrap_or(0);
        let from = stored.clamp(knob.min_ms, knob.max_ms);
        // Prefer stepping *up* from where the knob already sits, so the last write of
        // the arm puts it back and the happy path needs no restoring write at all.
        let delta: i32 = if from + step <= knob.max_ms {
            i32::from(step)
        } else if from >= knob.min_ms + step {
            -i32::from(step)
        } else {
            blocked.push(format!(
                "'{}' has no {step} ms of headroom in either direction on a knob of {}..={} ms currently at {from} ms",
                m.node_name, knob.min_ms, knob.max_ms
            ));
            continue;
        };
        let to = (i32::from(from) + delta) as u16;
        // §9.2: only an *advance* feeds the group's send-ahead high-water mark, and the
        // provisional delay never does at all (§1.1.2's operational asymmetry) — so this
        // is a constraint on the device arm alone.
        if knob.polarity == KnobPolarity::Advance {
            let mut advances: HashMap<String, u16> =
                current_delays.iter().filter(|(n, _)| send_ahead.min_buffer_ms.contains_key(*n)).map(|(n, v)| (n.clone(), *v)).collect();
            let before = send_ahead.mark_ms(&advances);
            advances.insert(m.node_name.clone(), to);
            let after = send_ahead.mark_ms(&advances);
            if after > before {
                blocked.push(format!(
                    "advancing '{}' by {step} ms would lift the group's send-ahead high-water mark from {before} ms to {after} ms, \
                     which restarts every speaker in the group's stream rather than reconnecting one device (plan §9.2)",
                    m.node_name
                ));
                continue;
            }
        }
        let why_member = match m.kind {
            MemberKind::Sendspin => format!(
                "'{}' is a sendspin member. Its static delay is the only *advance* in the design (§2.4.1), so it is the only knob whose \
                 sign a measurement can confirm rather than assume, and sendspin is the transport whose write costs the tens of seconds \
                 (§2.3) the deferred-write scheme exists to avoid. {others} other member(s) were not used: the equivalence is a property \
                 of the transport, and paying a reconnect wave per member for it would defeat the purpose.",
                m.node_name
            ),
            MemberKind::Airplay2 => format!(
                "'{}' is an AirPlay-2 member, used because this group has no sendspin member. Its render delay is a plain delay, so there \
                 is no sign question here — and `api.rs` pushes it *live* to the running stream, so the two baselines may not be separated \
                 by a reconnect at all, which makes the ε this experiment reports a weaker number than it would be on sendspin. \
                 {others} other member(s) were not used.",
                m.node_name
            ),
            MemberKind::PwSink => format!(
                "'{}' is a PipeWire-host member, used because this group has no sendspin or AirPlay-2 member. Its playout delay is floored \
                 at {} ms, so the baselines are read at its current value rather than at zero, and a write reloads the receiving module. \
                 {others} other member(s) were not used.",
                m.node_name, knob.min_ms
            ),
        };
        let why_step = format!(
            "{step} ms: exactly one wire-codec frame (960 frames at 48 kHz), so the relay-side step leaves the codec's window phase \
             untouched and §1.1.2 item 2's confound is nulled; ~140× the estimator's worst accepted error and 20× its std-error refusal \
             threshold (§5.4.1); and small enough to leave the group's send-ahead high-water mark where it is (§9.2). The knob goes \
             {from} → {to} ms and back to {from} ms."
        );
        return Ok(EquivalencePlan {
            member: m.node_name.clone(),
            kind: m.kind,
            knob,
            from_ms: from,
            stored_ms: stored,
            to_ms: to,
            delta_ms: delta,
            why_member,
            why_step,
        });
    }
    Err(Refusal::new(
        RefusalKind::KnobRange,
        format!(
            "no member of this group can take a {step} ms knob step, so the relay-vs-device equivalence cannot be measured on it: {}. \
             Either align a group with some knob headroom, or raise the group's send-ahead lead first — a step that crossed the \
             high-water mark would silence the whole group to measure one speaker.",
            blocked.join("; ")
        ),
    ))
}

/// One arm of the experiment: three readings at two knob (or delay-line) values.
#[derive(Debug, Clone, Serialize)]
pub struct EquivalenceArm {
    /// `"relay"` or `"device"`.
    pub arm: &'static str,
    /// What was asked for, ms — signed for the device arm (see
    /// [`EquivalencePlan::delta_ms`]).
    pub commanded_ms: f64,
    /// What was really applied. For the relay arm this is the line's own sample-exact
    /// figure (`DelayStatus::delay_frames` at its rate); for the device arm it is the
    /// integer millisecond that was written, because that is all the knob can express.
    pub applied_ms: f64,
    /// The measured shift for that change: `changed − mean(baseline_before,
    /// baseline_after)`, positive = the speaker arrived **later**. Averaged over the
    /// click track's two bands.
    pub shift_ms: f64,
    /// 1σ, propagated from the three readings' own standard errors.
    pub uncertainty_ms: f64,
    /// How much the 3 kHz and 1.5 kHz bands disagree about that shift. A delay delays
    /// both identically, so this is the only visible handle on a band-dependent effect
    /// (a loudspeaker crossover, or §1.1.2 item 2) — it cannot attribute it.
    pub band_spread_ms: f64,
    /// The three raw phases (channel A), for inspection. Only differences mean anything.
    pub baseline_before_ms: f64,
    pub changed_ms: f64,
    pub baseline_after_ms: f64,
    /// `baseline_after − baseline_before`: the same configuration read twice. Whatever
    /// this is, the bracket has just removed half of it from [`Self::shift_ms`].
    pub baseline_disagreement_ms: f64,
    pub span_s: f64,
    /// [`Self::baseline_disagreement_ms`] as a rate. For the relay arm this is pure
    /// mic-vs-audio clock drift (nothing else happened between the readings); for the
    /// device arm it is drift **plus** whatever the two reconnects did differently.
    pub drift_ppm: f64,
    /// What the write path said, verbatim — the evidence that a reconnect was actually
    /// forced ("reconnecting just this speaker to apply") or was not ("device not
    /// connected"). Empty for the relay arm, which is the point of the relay arm.
    pub writes: Vec<String>,
}

/// What the two arms said about each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EquivalenceVerdict {
    /// No difference larger than [`EquivalenceReport::resolution_ms`] was resolved.
    /// **Not** "the two are equivalent": this experiment has a resolution, and the
    /// claim is bounded by it.
    WithinResolution,
    /// The device knob moved the speaker by a different *amount* than the delay line
    /// did. Reported, never applied: it is a scale, and scaling every member's delay is
    /// a decision for whoever owns the write path, not for the measurement.
    ScaleDisagrees,
    /// The device knob moved the speaker the **opposite** way from the polarity the
    /// solver assumes (§2.4.1). Far more serious than any offset: every proposal the
    /// solver makes for this kind is inverted.
    SignInverted,
    /// The knob produced no measurable shift at all — it was ignored, or the write did
    /// not reach the device.
    KnobHadNoEffect,
    /// The **delay line** produced no measurable shift, so there is nothing to compare a
    /// knob against. Judged first, because charging this to the device would point the
    /// reader at the wrong half of the daemon — and it invalidates the provisional half of
    /// the deferred-write scheme rather than the write.
    RelayLineHadNoEffect,
}

/// What the borrowed state was put back to, on every exit path.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    /// The provisional delay line was cleared (or was never set).
    pub relay_cleared: bool,
    /// The knob's value at the end. `Some(v)` with `v` equal to the plan's `from_ms` is
    /// the intended outcome; anything else is a failure that is spelled out below.
    pub knob_left_at_ms: Option<u16>,
    /// A restoring write had to be issued because the run stopped with the knob at the
    /// stepped value.
    pub knob_rewritten: bool,
    pub failures: Vec<String>,
    pub message: String,
}

/// The result — a number with an uncertainty and a stated scope, not a boolean.
#[derive(Debug, Clone, Serialize)]
pub struct EquivalenceReport {
    pub plan: EquivalencePlan,
    /// The polarity the solver assumes for this kind (§2.4.1).
    pub polarity_assumed: KnobPolarity,
    /// The polarity the measurement saw. `None` when the shift was too small to have a
    /// direction, which is itself a finding ([`EquivalenceVerdict::KnobHadNoEffect`]).
    pub polarity_observed: Option<KnobPolarity>,
    pub relay: EquivalenceArm,
    pub device: EquivalenceArm,
    /// The device arm's shift expressed as the equivalent **relay delay**: the shift
    /// for a knob *increase* of [`EQUIV_STEP_MS`], negated when the knob's polarity is
    /// an advance. Directly comparable with `relay.shift_ms`.
    pub device_equivalent_delay_ms: f64,
    /// `device_equivalent_delay_ms − relay.shift_ms` — what the two arms disagree by
    /// for this step.
    pub discrepancy_ms: f64,
    /// The same thing as a factor: `device_equivalent_delay_ms / relay.shift_ms`. This
    /// is the shape a correction would take if anyone applied one, because a scale error
    /// grows with the delay while a constant does not.
    ///
    /// `None` when the relay arm measured no shift to divide by
    /// ([`EquivalenceVerdict::RelayLineHadNoEffect`]).
    pub scale: Option<f64>,
    /// 1σ on [`Self::discrepancy_ms`], propagated from the six readings.
    pub uncertainty_ms: f64,
    /// The bar a discrepancy has to clear to be called real:
    /// `max(3σ, EQUIV_MIN_MEANINGFUL_MS, reconnect_variation_ms)`.
    pub resolution_ms: f64,
    /// How much this speaker's arrival moved across a reconnect **with the knob
    /// unchanged**, with the clock drift the relay arm measured taken out.
    ///
    /// This is the ε of §1.1.2 item 3, measured rather than argued: the reason the
    /// device arm cannot be a single post-reconnect reading. One sample of one
    /// reconnect, so it bounds ε rather than characterising it.
    pub reconnect_epsilon_ms: f64,
    /// The part of the device arm's baseline disagreement that the relay arm's drift
    /// rate does **not** explain — i.e. how differently two reconnects landed. It is a
    /// floor under [`Self::resolution_ms`] because the bracket attributes it to drift
    /// and cannot know better.
    pub reconnect_variation_ms: f64,
    pub verdict: EquivalenceVerdict,
    /// One sentence with both numbers in it.
    pub headline: String,
    /// What a correction *would* be, said explicitly and applied nowhere.
    pub implied_correction: String,
    /// Everything this measurement does not establish. Not decoration: each line is a
    /// claim someone would otherwise make from these numbers.
    pub cannot_tell: Vec<&'static str>,
    /// Things worth saying about this particular run.
    pub notes: Vec<String>,
    pub reconnects: usize,
}

/// Plan §8's shape, for an experiment that has no solve and no proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EquivPhase {
    Idle,
    Arming,
    /// The relay arm: three readings, no reconnects, so a refusal here has cost the
    /// user nothing but time.
    RelayArm,
    /// The device arm: three writes, each of which reconnects the speaker.
    DeviceArm,
    /// Putting the borrowed delay back. Reached from success, refusal **and**
    /// cancellation.
    Restoring,
    Done,
    Refused,
}

/// `GET /api/align/equivalence`.
#[derive(Debug, Clone, Serialize)]
pub struct EquivalenceStatus {
    pub phase: EquivPhase,
    pub message: String,
    pub plan: Option<EquivalencePlan>,
    pub steps_done: usize,
    pub steps_total: usize,
    pub report: Option<EquivalenceReport>,
    pub refusal: Option<Refusal>,
    pub restore: Option<RestoreReport>,
    /// Carried straight from the shared measurement machinery: the same gate, so the
    /// same "what is it waiting for" sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateProgress>,
    pub warnings: Vec<Warning>,
    pub elapsed_s: u64,
}

struct EquivInner {
    phase: EquivPhase,
    message: String,
    plan: Option<EquivalencePlan>,
    steps_done: usize,
    report: Option<EquivalenceReport>,
    refusal: Option<Refusal>,
    restore: Option<RestoreReport>,
    started: Option<std::time::Instant>,
    cancel: Arc<AtomicBool>,
    running: bool,
}

impl EquivInner {
    fn idle() -> Self {
        Self {
            phase: EquivPhase::Idle,
            message: "the relay-vs-device equivalence has not been measured".to_string(),
            plan: None,
            steps_done: 0,
            report: None,
            refusal: None,
            restore: None,
            started: None,
            cancel: Arc::new(AtomicBool::new(false)),
            running: false,
        }
    }
}

/// The experiment's state, in the shape the run task needs it: its own fields, plus the
/// measurement machinery's [`Inner`] as a **progress sink**.
///
/// Reusing `Inner` rather than reimplementing it is what lets [`measure_member`] — with
/// its gate, its interference draining, its AEC warning and its session/mic re-binding
/// — be used unchanged. Both share one notifier, so a gate message reaches a socket
/// watching *this* experiment.
#[derive(Clone)]
struct EquivState {
    inner: Arc<Mutex<EquivInner>>,
    scratch: Arc<Mutex<Inner>>,
    changes: Arc<tokio::sync::watch::Sender<u64>>,
}

impl EquivState {
    fn new() -> Self {
        let changes = Arc::new(tokio::sync::watch::channel(0).0);
        Self {
            inner: Arc::new(Mutex::new(EquivInner::idle())),
            scratch: Arc::new(Mutex::new(Inner::idle_watching(changes.clone()))),
            changes,
        }
    }

    fn bump(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn status(&self) -> EquivalenceStatus {
        let g = self.inner.lock_recover();
        // Taken *after* the experiment's own lock, always in this order — the run task
        // never holds `scratch` while reaching for `inner`.
        let s = self.scratch.lock_recover();
        EquivalenceStatus {
            phase: g.phase,
            message: g.message.clone(),
            plan: g.plan.clone(),
            steps_done: g.steps_done,
            steps_total: EQUIV_STEPS,
            report: g.report.clone(),
            refusal: g.refusal.clone(),
            restore: g.restore.clone(),
            gate: s.gate.clone(),
            warnings: s.warnings.clone(),
            elapsed_s: g.started.map(|t| t.elapsed().as_secs()).unwrap_or(0),
        }
    }

    fn set(&self, phase: EquivPhase, message: impl Into<String>) {
        {
            let mut g = self.inner.lock_recover();
            g.phase = phase;
            g.message = message.into();
        }
        self.bump();
    }

    fn say(&self, message: impl Into<String>) {
        self.inner.lock_recover().message = message.into();
        self.bump();
    }
}

/// The relay-vs-device equivalence experiment: one at a time, process-wide.
pub struct EquivalenceManager {
    st: EquivState,
}

/// The process-wide experiment, for the same reason [`shared`] is process-wide: one
/// mic, one session, one group.
// Used by the API handlers that own the routes, which `api.rs` has yet to add.
#[allow(dead_code)]
pub fn equivalence() -> &'static EquivalenceManager {
    static M: OnceLock<EquivalenceManager> = OnceLock::new();
    M.get_or_init(|| EquivalenceManager { st: EquivState::new() })
}

impl EquivalenceManager {
    pub fn status(&self) -> EquivalenceStatus {
        self.st.status()
    }

    /// Fires whenever [`Self::status`] would return something new — including the
    /// gate's progress, because the experiment spends most of its wall clock inside
    /// gates (plan §11).
    #[allow(dead_code)] // used by `equivalence_ws`, whose route api.rs owns
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.st.changes.subscribe()
    }

    /// `POST /api/align/equivalence` — start the experiment.
    ///
    /// Refuses up front on everything knowable without playing anything: a measurement
    /// run in flight (both would solo the same session), no alignment session, no
    /// microphone, no knob headroom.
    #[allow(dead_code)] // wired by api.rs, which owns the router
    pub async fn start(&self, deps: EquivalenceDeps) -> Result<EquivalenceStatus, Refusal> {
        {
            let g = self.st.inner.lock_recover();
            if g.running {
                return Err(Refusal::new(
                    RefusalKind::Internal,
                    format!(
                        "the equivalence experiment is already running ({:?}); it restores the delay it borrowed before it finishes, so \
                         let it end rather than starting a second one",
                        g.phase
                    ),
                ));
            }
        }
        // The two cannot share the session: both solo members, and this one reconnects a
        // speaker three times.
        let measuring = shared().status();
        if !measuring.phase.is_terminal() {
            return Err(Refusal::new(
                RefusalKind::Internal,
                format!(
                    "a measurement run is in progress ({:?}); it and this experiment would fight over the same session",
                    measuring.phase
                ),
            ));
        }
        let session = deps.base.session.snapshot().await;
        if !session.active {
            return Err(Refusal::new(
                RefusalKind::NoSession,
                "no alignment session is running — start one for the group first, so the test pattern is playing off one clock",
            ));
        }
        if !deps.base.mic.status().connected {
            return Err(Refusal::new(
                RefusalKind::MicMissing,
                "no microphone capture is connected. Open the alignment panel on a phone over HTTPS and start the microphone first.",
            ));
        }
        // Checked here as well as in the run so a refusal the user can act on
        // ("no headroom", "it would move the high-water mark") is the reply to their
        // request rather than a state they have to poll for.
        let plan = plan_equivalence(&session.members, &deps.base.current_delays, &deps.base.send_ahead, deps.member.as_deref())?;

        let cancel = Arc::new(AtomicBool::new(false));
        let status = {
            let mut g = self.st.inner.lock_recover();
            *g = EquivInner::idle();
            g.phase = EquivPhase::Arming;
            g.plan = Some(plan);
            g.message = "arming: checking the session, the capture and the speaker's knob".to_string();
            g.started = Some(std::time::Instant::now());
            g.cancel = cancel.clone();
            g.running = true;
            drop(g);
            *self.st.scratch.lock_recover() = Inner::idle_watching(self.st.changes.clone());
            self.st.bump();
            self.st.status()
        };
        let st = self.st.clone();
        tokio::spawn(async move { drive_equivalence(deps, st, cancel).await });
        Ok(status)
    }

    /// `DELETE /api/align/equivalence` — abandon.
    ///
    /// The run still finishes its **restore**: the delay it borrowed is not the user's,
    /// and leaving a speaker 20 ms out because someone closed a tab is not an option.
    /// So this marks the experiment cancelled and lets the task put the state back,
    /// which is why the status stays readable afterwards instead of being reset.
    #[allow(dead_code)] // wired by api.rs, which owns the router
    pub fn abandon(&self) -> EquivalenceStatus {
        {
            let mut g = self.st.inner.lock_recover();
            g.cancel.store(true, Ordering::Relaxed);
            if g.running {
                g.message = "abandoning: putting the borrowed delay back before stopping".to_string();
            } else {
                *g = EquivInner::idle();
            }
        }
        self.st.bump();
        self.st.status()
    }
}

/// What the run has changed and therefore owes the user back.
#[derive(Debug, Clone, Default)]
struct EquivApplied {
    member: String,
    /// A provisional delay is live on the line.
    relay_set: bool,
    /// The knob value this run last wrote, and the value it must end at.
    knob_written: Option<u16>,
    knob_original: u16,
    writes: usize,
}

/// Run the experiment, then restore — on success, on refusal and on cancellation, with
/// no path in between.
async fn drive_equivalence(deps: EquivalenceDeps, st: EquivState, cancel: Arc<AtomicBool>) {
    let mut applied = EquivApplied::default();
    let outcome = equivalence_body(&deps, &st, &cancel, &mut applied).await;
    let restore = restore_equivalence(&deps, &st, &applied).await;
    let mut g = st.inner.lock_recover();
    g.running = false;
    match outcome {
        Ok(report) => {
            g.message = report.headline.clone();
            g.phase = EquivPhase::Done;
            match report.verdict {
                EquivalenceVerdict::WithinResolution => tracing::info!("alignment equivalence: {}", report.headline),
                // A sign or scale disagreement invalidates what the solver would write,
                // so it goes out at a level someone reading a log will see.
                _ => tracing::warn!("alignment equivalence: {}", report.headline),
            }
            g.report = Some(report);
        }
        Err(refusal) => {
            // A cancelled run reports the restore rather than the refusal: "abandoned"
            // is what the user already knows, and where the delay ended up is not.
            g.message = match refusal.kind {
                RefusalKind::Cancelled => format!("abandoned; {}", restore.message),
                _ => refusal.message.clone(),
            };
            g.refusal = Some(refusal);
            g.phase = EquivPhase::Refused;
        }
    }
    g.restore = Some(restore);
    drop(g);
    st.bump();
}

/// Give the borrowed state back. Deliberately **not** cancellable: it is the teardown.
async fn restore_equivalence(deps: &EquivalenceDeps, st: &EquivState, applied: &EquivApplied) -> RestoreReport {
    if applied.member.is_empty() {
        return RestoreReport {
            relay_cleared: true,
            knob_left_at_ms: None,
            knob_rewritten: false,
            failures: Vec::new(),
            message: "nothing was applied, so there was nothing to put back".to_string(),
        };
    }
    st.set(EquivPhase::Restoring, format!("putting '{}''s delay back where it was", applied.member));
    let mut failures = Vec::new();
    // The line first: it is infallible and instant, and it is the one that would
    // otherwise keep shifting audio for as long as the daemon runs.
    deps.base.relay.clear(&applied.member);
    let relay_cleared = deps.base.relay.status(&applied.member).is_none_or(|s| s.delay_us == 0);
    if !relay_cleared {
        failures.push(format!("the provisional delay on '{}' is still applied", applied.member));
    }
    let mut knob_rewritten = false;
    let mut knob_left_at_ms = applied.knob_written;
    // The kind lives on the plan (the delay map is keyed by node name only), and the
    // plan is published before anything is applied — so if a knob was written, its kind
    // is known.
    let kind = st.inner.lock_recover().plan.as_ref().map(|p| p.kind);
    if let (Some(written), Some(kind)) = (applied.knob_written, kind) {
        if written != applied.knob_original {
            knob_rewritten = true;
            match deps.base.writer.write(applied.member.clone(), kind, applied.knob_original).await {
                Ok(msg) => {
                    knob_left_at_ms = Some(applied.knob_original);
                    tracing::info!("alignment equivalence restore: {msg}");
                }
                Err(e) => failures.push(format!(
                    "putting '{}''s knob back to {} ms failed: {e} — it is still at {} ms",
                    applied.member, applied.knob_original, written
                )),
            }
        }
    }
    let line = match applied.relay_set {
        true => "the provisional delay is cleared",
        // The clear above ran anyway — it is idempotent and cheap, and a line left
        // applied would shift that speaker for as long as the daemon runs.
        false => "no provisional delay was left applied",
    };
    let message = match (failures.is_empty(), knob_rewritten) {
        (true, true) => format!(
            "restored: {line} and '{}''s knob is back at {} ms (one more reconnect, because the run stopped with the step still applied)",
            applied.member, applied.knob_original
        ),
        (true, false) => {
            format!("restored: {line} and '{}''s knob is at the {} ms it started at", applied.member, applied.knob_original)
        }
        (false, _) => format!("restoring did not fully succeed: {}", failures.join("; ")),
    };
    RestoreReport { relay_cleared, knob_left_at_ms, knob_rewritten, failures, message }
}

/// The experiment itself: bind, plan, relay arm, device arm, compare.
async fn equivalence_body(
    deps: &EquivalenceDeps,
    st: &EquivState,
    cancel: &AtomicBool,
    applied: &mut EquivApplied,
) -> Result<EquivalenceReport, Refusal> {
    st.set(EquivPhase::Arming, "arming: checking the session, the capture and which speaker to use");
    // Not `bind` yet: `bind` compares the session's group against the sources the
    // scratch state remembers, and this run is what puts them there.
    let session = deps.base.session.snapshot().await;
    if !session.active {
        return Err(Refusal::new(
            RefusalKind::NoSession,
            "no alignment session is running — nothing is playing to measure. Start one for the group first.",
        ));
    }
    let mic = deps.base.mic.status();
    if !mic.connected {
        return Err(Refusal::new(RefusalKind::MicMissing, "no microphone capture is connected, so there is nothing to measure with"));
    }
    let rate = mic.sample_rate;
    let pattern_ms = deps.base.timing.pattern_ms;
    let plan = plan_equivalence(&session.members, &deps.base.current_delays, &deps.base.send_ahead, deps.member.as_deref())?;
    let Some(member) = session.members.iter().find(|m| m.node_name == plan.member).cloned() else {
        return Err(Refusal::new(RefusalKind::Internal, "the chosen member vanished from the session between planning and measuring"));
    };
    let level = session.level_for(&plan.member);
    {
        // The scratch state is what `measure_member` publishes into and what its `bind`
        // checks the session against, so it is seeded here — one member, because this
        // experiment holds exactly one.
        let mut g = st.scratch.lock_recover();
        g.sources = session.sources.clone();
        g.sample_rate = rate;
        g.members = vec![MemberProgress {
            node_name: plan.member.clone(),
            kind: plan.kind,
            level,
            current_delay_ms: plan.from_ms,
            passes_done: 0,
            last: None,
            note: None,
        }];
        g.bump();
    }
    {
        let mut g = st.inner.lock_recover();
        g.plan = Some(plan.clone());
    }
    applied.member = plan.member.clone();
    applied.knob_original = plan.stored_ms;
    st.bump();

    // ---- the relay arm: three readings, no reconnect (§1.1.2 item 3) --------
    st.set(EquivPhase::RelayArm, format!("relay arm: measuring '{}' with and without a {} ms delay line", plan.member, EQUIV_STEP_MS));
    let settle = GateConfig::mute_settle(&deps.base.timing);
    let r1 = equiv_read(deps, st, cancel, &member, level, settle, 0, rate, "relay baseline (no provisional delay)").await?;
    deps.base.relay.set_delay_ms(&plan.member, EQUIV_STEP_MS).map_err(|e| {
        Refusal::for_member(RefusalKind::Internal, &plan.member, format!("the provisional delay line refused {EQUIV_STEP_MS} ms: {e}"))
    })?;
    applied.relay_set = true;
    let relay_applied_ms = equiv_wait_primed(deps, st, cancel, &plan.member).await?;
    let r2 = equiv_read(deps, st, cancel, &member, level, settle, 1, rate, "relay stepped (delay line applied)").await?;
    deps.base.relay.clear(&plan.member);
    applied.relay_set = false;
    let r3 = equiv_read(deps, st, cancel, &member, level, settle, 2, rate, "relay baseline again (the drift bracket)").await?;
    let relay = equiv_arm("relay", f64::from(EQUIV_STEP_MS), relay_applied_ms, &r1, &r2, &r3, pattern_ms, Vec::new());

    // ---- the device arm: three writes, each a reconnect --------------------
    st.set(
        EquivPhase::DeviceArm,
        format!(
            "device arm: writing '{}''s knob {} → {} → {} ms, one reading after each — every write reconnects the speaker, which is \
             tens of seconds of silence each time (plan §2.3)",
            plan.member, plan.from_ms, plan.to_ms, plan.from_ms
        ),
    );
    let reconnect = GateConfig::reconnect(&deps.base.timing);
    let mut writes = Vec::new();
    writes.push(equiv_write(deps, st, cancel, applied, &plan, plan.from_ms).await?);
    let d1 =
        equiv_read(deps, st, cancel, &member, level, reconnect, 3, rate, "device baseline (knob unchanged, after a reconnect)").await?;
    writes.push(equiv_write(deps, st, cancel, applied, &plan, plan.to_ms).await?);
    let d2 = equiv_read(deps, st, cancel, &member, level, reconnect, 4, rate, "device stepped (knob written, after a reconnect)").await?;
    writes.push(equiv_write(deps, st, cancel, applied, &plan, plan.from_ms).await?);
    let d3 = equiv_read(deps, st, cancel, &member, level, reconnect, 5, rate, "device baseline again (the drift bracket)").await?;
    let device = equiv_arm("device", f64::from(plan.delta_ms), f64::from(plan.delta_ms.abs()), &d1, &d2, &d3, pattern_ms, writes);

    Ok(equiv_compare(plan, relay, device, &r3, &d1, pattern_ms, applied.writes))
}

/// Issue one knob write and let the device start reacting to it.
async fn equiv_write(
    deps: &EquivalenceDeps,
    st: &EquivState,
    cancel: &AtomicBool,
    applied: &mut EquivApplied,
    plan: &EquivalencePlan,
    value_ms: u16,
) -> Result<String, Refusal> {
    st.say(format!("writing '{}''s knob to {value_ms} ms", plan.member));
    // Recorded *before* the await: a write that times out or panics has still been
    // issued, and the restore has to assume it landed.
    applied.knob_written = Some(value_ms);
    applied.writes += 1;
    let msg = deps.base.writer.write(plan.member.clone(), plan.kind, value_ms).await.map_err(|e| {
        Refusal::for_member(
            RefusalKind::WriteFailed,
            &plan.member,
            format!("writing '{}''s knob to {value_ms} ms failed: {e}. Nothing can be concluded from a half-applied step.", plan.member),
        )
    })?;
    tracing::info!("alignment equivalence write: {msg}");
    // Not the settling *mechanism* — that is the reconnect-length gate — just enough
    // that the gate does not spend its window watching a device which has not dropped
    // its connection yet (the same reason `run_apply` waits here).
    sleep_cancellable(deps.base.timing.settle_grace, deps.base.timing.poll, cancel).await?;
    Ok(msg)
}

/// Wait for the delay line to fill, and return the delay it is **really** applying, in
/// ms (`relay_delay`'s sample-exact figure, not what was asked for).
async fn equiv_wait_primed(deps: &EquivalenceDeps, st: &EquivState, cancel: &AtomicBool, output: &str) -> Result<f64, Refusal> {
    let deadline = Instant::now() + EQUIV_PRIME_TIMEOUT;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Refusal::new(RefusalKind::Cancelled, "abandoned"));
        }
        let Some(status) = deps.base.relay.status(output) else {
            return Err(Refusal::for_member(
                RefusalKind::Internal,
                output,
                format!("the provisional delay on '{output}' disappeared before it could be measured"),
            ));
        };
        let applied_ms = crate::align::relay_delay::us_for_frames(status.delay_frames, status.rate) as f64 / 1000.0;
        if status.primed {
            return Ok(applied_ms);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Refusal::for_member(
                RefusalKind::GateTimeout,
                output,
                format!(
                    "the {EQUIV_STEP_MS} ms delay line on '{output}' never filled ({:.0} ms of audio still missing after {} s). It fills \
                     from the audio flowing to that output, so this says no audio is reaching it — measuring through a half-filled line \
                     would read as a dropout, not as a delay.",
                    status.prime_remaining_us as f64 / 1000.0,
                    EQUIV_PRIME_TIMEOUT.as_secs()
                ),
            ));
        }
        st.say(format!("waiting for the {EQUIV_STEP_MS} ms delay line on '{output}' to fill"));
        tokio::time::sleep(deps.base.timing.poll.min(deadline - now)).await;
    }
}

/// One reading, through the same solo, gate and estimator every other measurement in
/// this file uses.
#[allow(clippy::too_many_arguments)] // one reading's worth of context
async fn equiv_read(
    deps: &EquivalenceDeps,
    st: &EquivState,
    cancel: &AtomicBool,
    member: &SessionMember,
    level: u8,
    cfg: GateConfig,
    step: usize,
    rate: u32,
    what: &str,
) -> Result<MemberObservation, Refusal> {
    st.say(format!("reading {}/{EQUIV_STEPS}: {what}", step + 1));
    // `pass` doubles as the step index so the scratch observations stay distinguishable;
    // `grid_epoch` is 0 throughout because a capture reconnect refuses (below) rather
    // than starting a new epoch.
    let observation = measure_member(&deps.base, &st.scratch, cancel, member, level, cfg, step, 0, rate).await.map_err(|e| match e {
        StepError::Refuse(r) => r,
        // Plan §1.2: the capture *is* the reference frame, and this experiment compares
        // readings taken minutes apart. A reconnect voids all of them, and re-running is
        // three more reconnects — so it refuses instead of restarting itself.
        StepError::RestartSet(mut r) => {
            r.message = format!(
                "{} Every reading in this experiment is compared against the others, so all {} taken so far are void. Nothing was left \
                 changed; run it again once the capture is staying connected.",
                r.message, step
            );
            r
        }
    })?;
    {
        let mut g = st.inner.lock_recover();
        g.steps_done = step + 1;
    }
    st.bump();
    Ok(observation)
}

/// Per-band phase of one reading. Only differences are meaningful — the two bursts sit
/// at different points in the pattern, so the absolute values are not comparable with
/// each other.
fn equiv_phases(o: &MemberObservation) -> [f64; 2] {
    [o.m.phase_a_ms, o.m.phase_b_ms]
}

/// The shift between two readings, per band and then averaged, wrapped the short way.
fn equiv_shift(from: &MemberObservation, to: &MemberObservation, pattern_ms: f64) -> f64 {
    let (from, to) = (equiv_phases(from), equiv_phases(to));
    (wrap_sym(to[0] - from[0], pattern_ms) + wrap_sym(to[1] - from[1], pattern_ms)) / 2.0
}

/// A bracketed arm: the shift of the middle reading against the **mean** of the two
/// baselines, which cancels a linear drift between them exactly.
fn equiv_bracket(b1: &MemberObservation, mid: &MemberObservation, b2: &MemberObservation, pattern_ms: f64) -> (f64, f64) {
    let mut per_band = [0.0f64; 2];
    for (i, band) in per_band.iter_mut().enumerate() {
        let s1 = wrap_sym(equiv_phases(mid)[i] - equiv_phases(b1)[i], pattern_ms);
        let s2 = wrap_sym(equiv_phases(mid)[i] - equiv_phases(b2)[i], pattern_ms);
        *band = (s1 + s2) / 2.0;
    }
    ((per_band[0] + per_band[1]) / 2.0, (per_band[0] - per_band[1]).abs())
}

#[allow(clippy::too_many_arguments)] // one arm's worth of context
fn equiv_arm(
    arm: &'static str,
    commanded_ms: f64,
    applied_ms: f64,
    b1: &MemberObservation,
    mid: &MemberObservation,
    b2: &MemberObservation,
    pattern_ms: f64,
    writes: Vec<String>,
) -> EquivalenceArm {
    let (shift_ms, band_spread_ms) = equiv_bracket(b1, mid, b2, pattern_ms);
    let baseline_disagreement_ms = equiv_shift(b1, b2, pattern_ms);
    let span_s = (b2.period_centre - b1.period_centre) * pattern_ms / 1000.0;
    let drift_ppm = match span_s > 0.0 {
        true => baseline_disagreement_ms / (span_s * 1000.0) * 1e6,
        false => 0.0,
    };
    let uncertainty_ms = (mid.m.std_error_ms.powi(2) + (b1.m.std_error_ms.powi(2) + b2.m.std_error_ms.powi(2)) / 4.0).sqrt();
    EquivalenceArm {
        arm,
        commanded_ms,
        applied_ms,
        shift_ms,
        uncertainty_ms,
        band_spread_ms,
        baseline_before_ms: b1.m.phase_a_ms,
        changed_ms: mid.m.phase_a_ms,
        baseline_after_ms: b2.m.phase_a_ms,
        baseline_disagreement_ms,
        span_s,
        drift_ppm,
        writes,
    }
}

/// Turn the two arms into the answer — the one function where the sign conventions all
/// meet, so they are spelled out rather than implied.
#[allow(clippy::too_many_arguments)] // the whole comparison's inputs
fn equiv_compare(
    plan: EquivalencePlan,
    relay: EquivalenceArm,
    device: EquivalenceArm,
    relay_last_baseline: &MemberObservation,
    device_first_baseline: &MemberObservation,
    pattern_ms: f64,
    reconnects: usize,
) -> EquivalenceReport {
    let polarity_assumed = plan.knob.polarity;
    let step = f64::from(EQUIV_STEP_MS);
    // The device arm may have stepped *down* (no headroom upwards), so normalise to the
    // shift a knob **increase** of one step produces.
    let per_increase = match plan.delta_ms {
        0 => 0.0,
        d => device.shift_ms * step / f64::from(d),
    };
    // …and then express it as the relay delay it is equivalent to. An advance knob is
    // expected to move the sound *earlier*, so its equivalent delay is the negation.
    let device_equivalent_delay_ms = match polarity_assumed {
        KnobPolarity::Advance => -per_increase,
        KnobPolarity::Delay => per_increase,
    };
    let discrepancy_ms = device_equivalent_delay_ms - relay.shift_ms;
    let uncertainty_ms = (relay.uncertainty_ms.powi(2) + device.uncertainty_ms.powi(2)).sqrt();

    // The relay arm saw no reconnects, so its baseline disagreement is pure clock drift;
    // whatever the device arm's baselines disagree by *beyond* that rate is the two
    // reconnects landing differently, and the bracket has just charged it to drift.
    let drift_ms_per_s = match relay.span_s > 0.0 {
        true => relay.baseline_disagreement_ms / relay.span_s,
        false => 0.0,
    };
    let reconnect_variation_ms = (device.baseline_disagreement_ms - drift_ms_per_s * device.span_s).abs();
    // ε (§1.1.2 item 3): the same speaker, the same knob value, either side of one
    // reconnect, with the drift the relay arm measured taken out.
    let cross_span_s = (device_first_baseline.period_centre - relay_last_baseline.period_centre) * pattern_ms / 1000.0;
    let reconnect_epsilon_ms = equiv_shift(relay_last_baseline, device_first_baseline, pattern_ms) - drift_ms_per_s * cross_span_s;

    let resolution_ms = (3.0 * uncertainty_ms).max(EQUIV_MIN_MEANINGFUL_MS).max(reconnect_variation_ms);
    let polarity_observed = match per_increase.abs() > resolution_ms {
        true if per_increase < 0.0 => Some(KnobPolarity::Advance),
        true => Some(KnobPolarity::Delay),
        false => None,
    };
    // Judged before anything about the device: if the delay line produced no shift there
    // is nothing to compare a knob *against*, and blaming the device for that would point
    // the reader at the wrong half of the daemon.
    let relay_worked = relay.shift_ms.abs() > resolution_ms;
    let verdict = match (relay_worked, polarity_observed) {
        (false, _) => EquivalenceVerdict::RelayLineHadNoEffect,
        (_, None) => EquivalenceVerdict::KnobHadNoEffect,
        (_, Some(p)) if p != polarity_assumed => EquivalenceVerdict::SignInverted,
        _ if discrepancy_ms.abs() > resolution_ms => EquivalenceVerdict::ScaleDisagrees,
        _ => EquivalenceVerdict::WithinResolution,
    };
    // `None` rather than a NaN: this is serialised, and `serde_json` cannot represent a
    // NaN at all — a status that fails to serialise would take the whole endpoint with
    // it.
    let scale = relay_worked.then(|| device_equivalent_delay_ms / relay.shift_ms);
    let noun = polarity_assumed.noun();
    let headline = match verdict {
        EquivalenceVerdict::WithinResolution => format!(
            "'{}': a {:.2} ms delay line and a {} ms {noun} agree to {:+.2} ms (±{:.2} ms, 1σ) — no difference beyond the ±{:.2} ms this \
             experiment can resolve, and the knob moves the sound {} as §2.4.1 says. Measured on one speaker of one transport.",
            plan.member,
            relay.applied_ms,
            EQUIV_STEP_MS,
            discrepancy_ms,
            uncertainty_ms,
            resolution_ms,
            polarity_assumed.direction()
        ),
        EquivalenceVerdict::ScaleDisagrees => format!(
            "'{}': the delay line shifted it {:.2} ms but a {} ms {noun} shifted it the equivalent of {:.2} ms — a difference of \
             {:+.2} ms (±{:.2} ms, 1σ; resolution ±{:.2} ms), i.e. a factor of {:.3}. The two are NOT interchangeable at this speaker: a \
             provisional delay of d ms lands as {:.3}×d once written.",
            plan.member,
            relay.shift_ms,
            EQUIV_STEP_MS,
            device_equivalent_delay_ms,
            discrepancy_ms,
            uncertainty_ms,
            resolution_ms,
            scale.unwrap_or_default(),
            scale.unwrap_or_default()
        ),
        EquivalenceVerdict::SignInverted => format!(
            "'{}': the knob moves the sound the WRONG WAY. A {} ms change made it arrive {:.2} ms {}, where the solver assumes an \
             {noun} (i.e. {}). §2.4.1's polarity for {:?} is wrong on this device, and every delay the solver proposes for this kind is \
             inverted — do not write any of them until this is resolved. The delay line itself behaved as asked ({:.2} ms for {:.2} ms).",
            plan.member,
            EQUIV_STEP_MS,
            per_increase.abs(),
            match per_increase > 0.0 {
                true => "later",
                false => "earlier",
            },
            polarity_assumed.direction(),
            plan.kind,
            relay.shift_ms,
            relay.applied_ms
        ),
        EquivalenceVerdict::KnobHadNoEffect => format!(
            "'{}': a {} ms knob change moved it {:+.2} ms — nothing, within the ±{:.2} ms this experiment resolves. The knob was written \
             ({}) but the device did not act on it, so the final write of an alignment would silently do nothing on this speaker. The \
             delay line itself worked ({:.2} ms for {:.2} ms).",
            plan.member,
            EQUIV_STEP_MS,
            per_increase,
            resolution_ms,
            device.writes.last().map(String::as_str).unwrap_or("no reply"),
            relay.shift_ms,
            relay.applied_ms
        ),
        EquivalenceVerdict::RelayLineHadNoEffect => format!(
            "'{}': the {:.2} ms provisional delay moved it {:+.2} ms — nothing, within the ±{:.2} ms this experiment resolves. So the \
             *delay line* is what failed, not the knob, and nothing can be concluded about the equivalence: a walk driven by this line \
             would be aligning against a delay it is not applying. The knob, for what it is worth, shifted the equivalent of {:.2} ms.",
            plan.member, relay.applied_ms, relay.shift_ms, resolution_ms, device_equivalent_delay_ms
        ),
    };
    let implied_correction = match verdict {
        EquivalenceVerdict::WithinResolution => format!(
            "none. Nothing here is corrected for anywhere — an equivalence within ±{resolution_ms:.2} ms is what the deferred-write \
             scheme already assumes."
        ),
        EquivalenceVerdict::ScaleDisagrees => format!(
            "a provisional delay of d ms would need a knob of d/{:.3} = {:.3}×d ms to land where the walk verified it. This is NOT \
             applied: it is one speaker of one transport, and a scale correction belongs to whoever owns the write path, after this has \
             been reproduced.",
            scale.unwrap_or_default(),
            scale.map(|s| 1.0 / s).unwrap_or_default()
        ),
        EquivalenceVerdict::SignInverted => format!(
            "the polarity of {:?}'s knob in `knob_of` would have to be flipped from {noun} — a code change, not a factor, and not one \
             this measurement makes on its own.",
            plan.kind
        ),
        EquivalenceVerdict::KnobHadNoEffect => {
            "none is possible: no factor turns zero into a delay. Find out why the write did not take before trusting any alignment on \
             this transport."
                .to_string()
        }
        EquivalenceVerdict::RelayLineHadNoEffect => {
            "none, and this is the one verdict that invalidates the *provisional* half of the scheme rather than the write: fix the delay \
             line before running a chained alignment at all."
                .to_string()
        }
    };
    // Things that are true of *this* run and would otherwise have to be inferred from
    // the numbers by someone who already knew what to look for.
    let mut notes = Vec::new();
    let relay_error_ms = relay.shift_ms - relay.applied_ms;
    if relay_error_ms.abs() > resolution_ms {
        notes.push(format!(
            "the delay line itself was {relay_error_ms:+.2} ms off what it was asked for ({:.2} ms applied, {:.2} ms measured). Everything \
             above is measured *against* the line, so if the line is wrong the discrepancy is not the device's.",
            relay.applied_ms, relay.shift_ms
        ));
    }
    if device.writes.iter().any(|m| m.contains("not connected")) {
        notes.push(
            "at least one knob write reported the device as not connected, so it may not have reconnected between the readings — the ε \
             this run reports is then not a reconnect's ε at all."
                .to_string(),
        );
    }
    if !device.writes.iter().any(|m| m.contains("reconnect")) {
        notes.push(format!(
            "no write said it was reconnecting the speaker ({}). For {:?} that can be correct — an AP2 render delay is pushed live — but on \
             sendspin it means the knob took effect some other way, and the two baselines are then not separated by a reconnect.",
            device.writes.join("; "),
            plan.kind
        ));
    }
    if plan.stored_ms != plan.from_ms {
        notes.push(format!(
            "'{}''s knob was stored as {} ms, which is below the {} ms floor its write path enforces, so the baselines were read at \
             {} ms instead. Restoring writes the stored value back and the write path clamps it again — which turns \"no override\" into \
             an explicit one on this output.",
            plan.member, plan.stored_ms, plan.knob.min_ms, plan.from_ms
        ));
    }
    let worst_band = relay.band_spread_ms.max(device.band_spread_ms);
    if worst_band > TRANSITIVITY_TOL_MS {
        notes.push(format!(
            "the two test tones disagree by up to {worst_band:.2} ms about the same shift (limit {TRANSITIVITY_TOL_MS:.1} ms elsewhere in \
             this design). A delay delays both bands identically, so something band-dependent is in the path — a crossover, a reflection, \
             or the codec — and it is not attributable from here."
        ));
    }
    EquivalenceReport {
        plan,
        polarity_assumed,
        polarity_observed,
        relay,
        device,
        device_equivalent_delay_ms,
        discrepancy_ms,
        scale,
        uncertainty_ms,
        resolution_ms,
        reconnect_epsilon_ms,
        reconnect_variation_ms,
        verdict,
        headline,
        implied_correction,
        cannot_tell: EQUIV_CANNOT_TELL.to_vec(),
        notes,
        reconnects,
    }
}

/// What this experiment does not establish. Each line is a claim someone would
/// otherwise make from these numbers, so they travel *with* the numbers.
const EQUIV_CANNOT_TELL: [&str; 6] = [
    "It ran on ONE speaker of ONE transport. The other members of the same kind are not covered (each has its own firmware state, its own \
     buffer fill and its own ε), and the other two transports are not covered at all — their knobs are applied by different code, in a \
     different place, with a different reconnect cost.",
    "It cannot see a *constant* difference between applying a delay in the relay and applying it in the device. The device arm is a \
     difference of two post-reconnect readings (plan §1.1.2 item 3), which cancels any constant — so §1.1.1's \"per-transport constant to \
     correct for\" is not what this measures. Within one transport kind such a constant is a common shift, which alignment is free to \
     absorb; between kinds it is not, and nothing measurable on one speaker could tell you.",
    "It cannot expose §1.1.2 item 2's codec-frame-phase effect, and does not try to: the step is exactly one Opus frame, which leaves the \
     content-to-frame phase untouched. A real alignment delay is an arbitrary number of milliseconds, and for those the effect is inherent \
     — the decoded audio is still delayed by exactly d, but the measured peak position can move by a fraction of a frame that the device \
     knob would never produce.",
    "It says nothing about the write-back's precision (plan §1.1.2 item 4). The knobs are integer milliseconds and pw-sink's has a 15 ms \
     floor, so an alignment loses up to 0.5 ms per member — and a sub-15 ms pw-sink delay cannot be written at all — whatever this \
     experiment concludes. That is arithmetic, not something to measure.",
    "Its ε is one sample of one reconnect. It bounds how far a reconnect can move this speaker's arrival; it does not characterise the \
     distribution, and a second run may well differ.",
    "It cannot separate a genuinely quiet room from a lucky one. Every reading went through the same gate and refusal rules as an \
     alignment (plan §5.5), so it inherits §5.6's blind spot: an early reflection inside the analysis window biases both arms silently, \
     and it biases them by the *same* amount, which is exactly why the difference survives it.",
];

// ---- Status push (plan §11: "progress should be pushed, not polled") ------

/// `GET /api/align/equivalence/ws` — the experiment's status, pushed.
///
/// Worth a socket for the same reason the measurement is: it spends minutes inside
/// gates, waiting for a speaker to come back, and the *message* is the only thing
/// moving. Registered in `api.rs`, which owns the router.
#[allow(dead_code)] // the route belongs to api.rs
pub async fn equivalence_ws(ws: axum::extract::ws::WebSocketUpgrade) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| {
        let m = equivalence();
        status_socket(socket, m.subscribe(), || serde_json::to_string(&m.status()).ok())
    })
}

/// `GET /api/align/measure/ws` — the run status, pushed.
///
/// One full [`MeasureStatus`] on connect, then one on every change. Same shape a
/// `GET /api/align/measure` poll returns, so a client can use either and the
/// socket is purely a latency improvement: a measurement spends most of its time
/// inside a gate whose *message* is the only thing moving, and polling that at any
/// useful rate is what §11 objected to.
///
/// Registered in `api.rs`, which owns the router.
pub async fn measure_ws(ws: axum::extract::ws::WebSocketUpgrade) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| {
        let m = shared();
        // Subscribed *before* the first status is read, so a change that lands between
        // the two is a redundant push rather than a missed one.
        status_socket(socket, m.subscribe(), || serde_json::to_string(&m.status()).ok())
    })
}

/// The push loop both sockets use: one full status on connect, then one on every
/// change. Shared because the difference between them is only which state is
/// serialised, and a second copy of the closed-tab handling below is how one of them
/// ends up leaking a subscription.
async fn status_socket(
    mut socket: axum::extract::ws::WebSocket,
    mut changes: tokio::sync::watch::Receiver<u64>,
    snapshot: impl Fn() -> Option<String>,
) {
    use axum::extract::ws::Message;

    let mut push = true;
    loop {
        if push {
            let Some(json) = snapshot() else {
                return;
            };
            if socket.send(Message::Text(json.into())).await.is_err() {
                return;
            }
        }
        // Either the run state moved, or the client said something. Reading the
        // socket matters even though this endpoint takes no commands: it is how a
        // closed tab is noticed, and without it a dead socket would sit here holding
        // a subscription until the next status change happened to fail to send.
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return; // the notifier is gone: the process is shutting down
                }
                push = true;
            }
            msg = socket.recv() => match msg {
                // Text/binary is reserved for future control messages; ignoring it
                // keeps an older daemon usable with a newer client. A client frame is
                // not a state change, so it must not trigger a push of its own.
                Some(Ok(Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => push = false,
                _ => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;
    use std::sync::atomic::AtomicU64;

    // ------------------------------------------------------------- gate tests

    fn clean(elapsed_ms: u64, periods: usize, peak: f32) -> GateSample {
        GateSample {
            elapsed: Duration::from_millis(elapsed_ms),
            connected: true,
            reconnected: false,
            gap: false,
            clipped: false,
            peak,
            periods_used: periods,
            quality: Quality::Accepted,
            interference: None,
        }
    }

    fn rejected(reason: RejectReason, msg: &str) -> Quality {
        Quality::Rejected { reason, message: msg.to_string() }
    }

    #[test]
    fn the_gate_locks_only_after_enough_stable_periods() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        for p in 1..GATE_MIN_PERIODS {
            let step = gate.observe(&clean(p as u64 * 2000, p, 0.2));
            assert!(!step.locked, "locked after only {p} periods");
            assert_eq!(step.progress.waiting_for, Some(GateReason::Acquiring));
        }
        let step = gate.observe(&clean(8000, GATE_MIN_PERIODS, 0.2));
        assert!(step.locked, "{:?}", step.progress);
        assert!(!step.restart && step.failed.is_none());
    }

    #[test]
    fn the_gate_passes_the_estimators_refusal_through_verbatim() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        let mut s = clean(2000, GATE_MIN_PERIODS, 0.2);
        s.quality = rejected(RejectReason::LowSnr, "the a tone is only 6.0 dB above the room's noise floor");
        let step = gate.observe(&s);
        assert!(!step.locked);
        assert_eq!(step.progress.waiting_for, Some(GateReason::Estimator));
        assert!(step.progress.message.contains("6.0 dB"), "the estimator's own sentence must survive: {}", step.progress.message);
        // …and it is still the message the user gets when the gate finally gives up.
        s.elapsed = GATE_TIMEOUT_SETTLE;
        let step = gate.observe(&s);
        let failed = step.failed.expect("the timeout must fail the gate");
        assert_eq!(failed.kind, RefusalKind::Estimator);
        assert_eq!(failed.estimator_reason, Some(RejectReason::LowSnr));
        assert!(failed.message.contains("noise floor"), "{}", failed.message);
    }

    #[test]
    fn a_gap_or_a_clip_restarts_the_window_instead_of_measuring_it() {
        for (label, mutate) in [("gap", 0), ("clip", 1)] {
            let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
            gate.observe(&clean(2000, 1, 0.2));
            gate.observe(&clean(4000, 2, 0.2));
            let mut s = clean(6000, GATE_MIN_PERIODS, 0.2);
            if mutate == 0 {
                s.gap = true;
            } else {
                s.clipped = true;
            }
            let step = gate.observe(&s);
            assert!(step.restart, "{label} must discard the accumulated window");
            assert!(!step.locked);
            // The count restarts: a fresh set of periods is required afterwards.
            let step = gate.observe(&clean(8000, GATE_MIN_PERIODS, 0.2));
            assert!(step.locked, "a clean window after the {label} must be usable again");
        }
    }

    #[test]
    fn a_disconnect_and_a_reconnect_are_reported_differently() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        let mut s = clean(1000, GATE_MIN_PERIODS, 0.2);
        s.connected = false;
        let step = gate.observe(&s);
        assert!(step.restart && !step.locked);
        assert_eq!(step.progress.waiting_for, Some(GateReason::MicDisconnected));
        s.elapsed = GATE_TIMEOUT_SETTLE;
        assert_eq!(gate.observe(&s).failed.map(|f| f.kind), Some(RefusalKind::MicLost));

        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        let mut s = clean(1000, GATE_MIN_PERIODS, 0.2);
        s.reconnected = true;
        let step = gate.observe(&s);
        assert_eq!(step.progress.waiting_for, Some(GateReason::MicReconnected));
        assert!(step.progress.message.contains("timing reference"), "{}", step.progress.message);
    }

    #[test]
    fn an_unstable_amplitude_blocks_the_lock_even_when_the_estimator_is_happy() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        gate.observe(&clean(2000, 1, 0.20));
        gate.observe(&clean(4000, 2, 0.21));
        // +6 dB in one period: the other speaker's mute had not settled.
        let step = gate.observe(&clean(6000, GATE_MIN_PERIODS, 0.42));
        assert!(step.restart, "a 6 dB jump must not be measured");
        assert_eq!(step.progress.waiting_for, Some(GateReason::UnstableAmplitude));
        assert!(step.progress.message.contains("dB"));
    }

    #[test]
    fn a_monotonic_decay_is_reported_as_the_aec_signature() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        // −0.9 dB per period, three periods: inside the 3 dB spread tolerance, so
        // only the *direction* gives it away (plan §4.2).
        gate.observe(&clean(2000, 1, 0.300));
        gate.observe(&clean(4000, 2, 0.270));
        let step = gate.observe(&clean(6000, 3, 0.243));
        assert_eq!(step.progress.waiting_for, Some(GateReason::AecSuspected));
        assert!(gate.aec_suspected());
        assert!(step.restart);
        assert!(amplitude_spread_db(&[0.300, 0.270, 0.243]).unwrap() < GATE_AMP_TOL_DB, "the spread alone must not catch this");
    }

    /// Plan §12.3's whole reason for existing: a barge-in announcement changes the
    /// level on the member being measured, and *without* this the amplitude-stability
    /// check catches it and blames the user's hand. The cause must win, and it must be
    /// the reason the gate reports when it eventually gives up.
    #[test]
    fn interference_is_blamed_on_the_announcement_not_the_user() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        let reason = "an urgent announcement (#7) played on 'sendspin-dev-kitchen' during the measurement".to_string();

        // A level jump big enough that the amplitude check *would* fire — but the
        // announcement that caused it is known, so that is what gets reported.
        gate.observe(&clean(2000, 1, 0.30));
        let mut s = clean(4000, 2, 0.60);
        s.interference = Some(reason.clone());
        let step = gate.observe(&s);
        assert!(step.restart, "the window spanning an announcement must be discarded");
        assert_eq!(step.progress.waiting_for, Some(GateReason::Interference));
        assert_eq!(step.progress.message, reason, "the cause must be quoted verbatim");
        assert!(!step.progress.message.contains("hold it still"));

        // And the timeout inherits it, so the refusal names the doorbell rather than
        // degrading into a generic gate timeout.
        let mut late = clean(60_000, 0, 0.60);
        late.interference = Some(reason.clone());
        let failed = gate.observe(&late).failed.expect("the gate must give up eventually");
        assert_eq!(failed.kind, RefusalKind::Interference);
        assert!(failed.message.contains("announcement"), "{}", failed.message);
    }

    #[test]
    fn silence_says_no_tone_arrived_rather_than_unstable() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        let step = gate.observe(&clean(2000, 1, 0.0));
        assert_eq!(step.progress.waiting_for, Some(GateReason::Silent));
        assert!(step.progress.message.contains("no tone"), "{}", step.progress.message);
    }

    /// A speaker whose stream keeps breaking up must be diagnosed as a stream
    /// fault, not as a moving phone. Hardware-observed on 2026-08-11: a wedged
    /// sendspin device renders intermittently until it is reconnected, and the
    /// amplitude-spread check would otherwise blame the user's hand.
    #[test]
    fn an_intermittent_stream_is_named_as_a_stream_fault_not_a_moving_phone() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        // Heard, lost, heard, lost: the signature of a discontinuous stream.
        assert_eq!(gate.observe(&clean(2000, 1, 0.30)).progress.waiting_for, Some(GateReason::Acquiring));
        let first = gate.observe(&clean(4000, 2, 0.0));
        assert_eq!(first.progress.waiting_for, Some(GateReason::Silent), "one dropout can still be a mute settling");
        assert_eq!(gate.observe(&clean(6000, 1, 0.30)).progress.waiting_for, Some(GateReason::Acquiring));
        let step = gate.observe(&clean(8000, 2, 0.0));
        assert_eq!(step.progress.waiting_for, Some(GateReason::Intermittent));
        // The advice must point at the speaker, and name the remedy that works.
        assert!(step.progress.message.contains("not continuous"), "{}", step.progress.message);
        assert!(step.progress.message.contains("static delay"), "{}", step.progress.message);
        assert!(!step.progress.message.contains("hold it still"), "{}", step.progress.message);
    }

    /// The counterpart: a speaker that never made a sound must stay `Silent`, so
    /// "muted or still reconnecting" is not mislabelled as a broken stream.
    #[test]
    fn a_speaker_that_never_sounded_stays_silent_not_intermittent() {
        let mut gate = Gate::new(GateConfig::mute_settle(&Timing::real()));
        for i in 0..4 {
            let step = gate.observe(&clean(2000 * (i + 1), 1, 0.0));
            assert_eq!(step.progress.waiting_for, Some(GateReason::Silent), "period {i}");
        }
    }

    #[test]
    fn the_reconnect_gate_waits_tens_of_seconds_not_five() {
        // Plan §2.3 is the single most important constraint on the orchestration:
        // a reconnecting sendspin device takes tens of seconds to render again.
        assert!(GATE_TIMEOUT_RECONNECT >= Duration::from_secs(120));
        let mut gate = Gate::new(GateConfig::reconnect(&Timing::real()));
        let mut s = clean(60_000, 0, 0.0);
        s.peak = 0.0;
        assert!(gate.observe(&s).failed.is_none(), "a minute of silence after a write is expected, not a failure");
    }

    // --------------------------------------------------------- solve tests

    fn member(name: &str) -> SessionMember {
        SessionMember { node_name: name.to_string(), kind: MemberKind::Sendspin }
    }

    /// One observation with an explicit A phase; B follows A by the nominal 1 s
    /// unless `band_bias` says otherwise.
    fn obs(name: &str, pass: usize, centre: f64, phase_a: f64, band_bias: f64) -> MemberObservation {
        MemberObservation {
            node_name: name.to_string(),
            pass,
            grid_epoch: 0,
            period_centre: centre,
            m: MemberMeasurement {
                phase_a_ms: phase_a.rem_euclid(2000.0),
                phase_b_ms: (phase_a + 1000.0 + band_bias).rem_euclid(2000.0),
                std_error_ms: 0.05,
                peak_snr_db: 40.0,
                second_peak_ratio: 20.0,
                drift_ppm: 0.0,
                periods_used: 4,
            },
        }
    }

    fn solve_of(members: &[SessionMember], observations: &[MemberObservation], delays: &[(&str, u16)]) -> Result<Proposal, Refusal> {
        let current: HashMap<String, u16> = delays.iter().map(|(n, d)| ((*n).to_string(), *d)).collect();
        let ctx = SendAheadContext::default();
        solve(&SolveInput { timing: Timing::real(), members, observations, current_delays: &current, send_ahead: &ctx, closure: None })
    }

    /// **The W14 inversion, and the assertion most likely to regress.** Before
    /// §2.4.1 this group was aligned to `b`, the *latest*-arriving member, by
    /// delaying `a` and `c` towards it. A sendspin knob is an advance, so the group
    /// is aligned to `a`, the *earliest*, and `b` and `c` are advanced to meet it —
    /// the same relative geometry, mirrored knobs, and less latency rather than
    /// more.
    #[test]
    fn a_sendspin_only_group_aligns_to_the_earliest_member_and_advances_the_rest() {
        let members = [member("a"), member("b"), member("c")];
        // b arrives 12 ms after a, c 5 ms after a.
        let o = [
            obs("a", 0, 0.0, 300.0, 0.0),
            obs("b", 0, 4.0, 312.0, 0.0),
            obs("c", 0, 8.0, 305.0, 0.0),
            obs("a", 1, 12.0, 300.0, 0.0),
            obs("b", 1, 16.0, 312.0, 0.0),
            obs("c", 1, 20.0, 305.0, 0.0),
        ];
        let p = solve_of(&members, &o, &[]).expect("accepted");
        let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
        // T = min_i(τ_i + a_i) = 0: the earliest intrinsic arrival takes advance 0.
        assert!((p.target_ms - 0.0).abs() < 0.01, "target {}", p.target_ms);
        assert_eq!(by("a").new_delay_ms, 0);
        assert_eq!(by("b").new_delay_ms, 12);
        assert_eq!(by("c").new_delay_ms, 5);
        assert_eq!(p.reference, "a", "the member left at knob zero is the earliest, not the latest");
        assert!(by("a").is_reference && !by("b").is_reference);
        assert_eq!(p.largest_knob_ms, 12);
        assert!((p.spread_ms - 12.0).abs() < 0.01, "spread {}", p.spread_ms);
        // Every knob is an advance, and it says so in words the UI can show.
        assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
        assert!(by("b").effect.contains("advance 12 ms"), "{}", by("b").effect);
        assert!(by("b").effect.contains("earlier"), "{}", by("b").effect);
        assert!(!by("b").effect.contains("delay"), "a sendspin knob must never be called a delay: {}", by("b").effect);
    }

    /// The same geometry seen through [`choose_target`] alone: no member can be
    /// placed later than its own intrinsic arrival, so the intersection's ceiling is
    /// the earliest of them and that is where the target lands.
    #[test]
    fn the_advance_only_intersection_is_capped_by_the_earliest_intrinsic_arrival() {
        let ivs = [
            MemberInterval::new("a".into(), MemberKind::Sendspin, 0, 0.0),
            MemberInterval::new("b".into(), MemberKind::Sendspin, 0, 12.0),
            // Already carrying 30 ms of advance: it *can* be moved later, up to 30 ms
            // past where it arrives now, because lowering the knob gives that back.
            MemberInterval::new("c".into(), MemberKind::Sendspin, 30, 5.0),
        ];
        assert_eq!(ivs[2].base_ms, 35.0, "the intrinsic arrival includes the advance already applied");
        assert_eq!(ivs[2].hi_ms, 35.0);
        assert_eq!(ivs[0].lo_ms, -f64::from(SENDSPIN_ADVANCE_MAX_MS));
        let sol = choose_target(&ivs).expect("feasible");
        assert!((sol.hi_ms - 0.0).abs() < 1e-9, "hi {}", sol.hi_ms);
        assert!((sol.target_ms - 0.0).abs() < 1e-9, "target {}", sol.target_ms);
        assert!((sol.largest_knob_ms - 35.0).abs() < 1e-9, "largest {}", sol.largest_knob_ms);
    }

    #[test]
    fn existing_knob_values_are_folded_in_and_the_largest_one_is_kept_as_small_as_possible() {
        // Plan §9.2, generalised by §2.4.2: a common shift changes no relative
        // timing, so the *largest* knob should be as small as possible.
        let members = [member("a"), member("b")];
        // Both arrive together *because* a already carries 40 ms of advance and b
        // 10 ms; the measurement therefore says "leave the difference alone, but give
        // the common 10 ms back".
        let o = [obs("a", 0, 0.0, 500.0, 0.0), obs("b", 0, 4.0, 500.0, 0.0), obs("a", 1, 8.0, 500.0, 0.0), obs("b", 1, 12.0, 500.0, 0.0)];
        let p = solve_of(&members, &o, &[("a", 40), ("b", 10)]).expect("accepted");
        let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
        assert_eq!(by("a").new_delay_ms, 30);
        assert_eq!(by("b").new_delay_ms, 0);
        assert_eq!(by("a").added_ms, -10);
        assert_eq!(p.largest_knob_ms, 30);
        // The group ends up playing 10 ms *later* than it does now, because b's
        // advance was the only thing holding it early.
        assert!((p.target_ms - 10.0).abs() < 0.01, "target {}", p.target_ms);
        assert!(by("a").effect.contains("later"), "lowering an advance plays later: {}", by("a").effect);
    }

    #[test]
    fn a_spread_near_half_a_period_is_refused_rather_than_wrapped() {
        let members = [member("a"), member("b")];
        let o = [obs("a", 0, 0.0, 0.0, 0.0), obs("b", 0, 4.0, 900.0, 0.0), obs("a", 1, 8.0, 0.0, 0.0), obs("b", 1, 12.0, 900.0, 0.0)];
        let r = solve_of(&members, &o, &[]).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::AmbiguousSpread);
        assert!(r.message.contains("wrap"), "{}", r.message);
    }

    #[test]
    fn a_member_that_was_never_measured_is_not_silently_left_out() {
        let members = [member("a"), member("b"), member("ghost")];
        let o = [obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 4.0, 110.0, 0.0)];
        let r = solve_of(&members, &o, &[]).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::Internal);
        assert_eq!(r.member.as_deref(), Some("ghost"));
    }

    #[test]
    fn observations_from_two_captures_are_never_compared() {
        let members = [member("a"), member("b")];
        let mut o = vec![obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 4.0, 110.0, 0.0)];
        o[1].grid_epoch = 1;
        let r = solve_of(&members, &o, &[]).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::MicReconnected);
    }

    #[test]
    fn a_common_clock_drift_is_fitted_out_of_the_offsets() {
        // 100 ppm on a 2 s pattern = 0.2 ms of phase per period. Measured
        // alternately (a,b then b,a) the raw phases move, but the *offsets* must
        // not: this is what stops a phone's clock from being written into the
        // speakers as a delay.
        let members = [member("a"), member("b")];
        let drift = 0.2;
        let truth_b = 6.0;
        let o = [
            obs("a", 0, 0.0, 300.0, 0.0),
            obs("b", 0, 5.0, 300.0 + truth_b + 5.0 * drift, 0.0),
            obs("b", 1, 10.0, 300.0 + truth_b + 10.0 * drift, 0.0),
            obs("a", 1, 15.0, 300.0 + 15.0 * drift, 0.0),
        ];
        let fit = fit_drift(&o, 2000.0, |o| o.m.phase_a_ms);
        assert!((fit.slope_ms_per_period - drift).abs() < 1e-6, "slope {}", fit.slope_ms_per_period);
        assert!((fit.drift_ppm(2000.0) - 100.0).abs() < 0.01, "ppm {}", fit.drift_ppm(2000.0));
        let p = solve_of(&members, &o, &[]).expect("accepted");
        let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
        assert_eq!(p.reference, "a", "a arrives first, so it is the one left at knob zero");
        assert_eq!(by("b").new_delay_ms, 6, "the drift must not become a knob value");
        assert_eq!(by("a").new_delay_ms, 0);
        assert!((p.drift_ppm - 100.0).abs() < 0.01);
    }

    #[test]
    fn a_single_pass_reports_that_no_drift_could_be_fitted() {
        let members = [member("a"), member("b")];
        let o = [obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 4.0, 103.0, 0.0)];
        let p = solve_of(&members, &o, &[]).expect("accepted");
        assert!(!p.warnings.is_empty());
        assert!(p.warnings.iter().any(|w| w.kind == WarningKind::NoDriftFit));
        assert!(p.checks.repeatability.is_none(), "one pass cannot be repeatable");
    }

    #[test]
    fn a_per_speaker_band_bias_blocks_the_write() {
        // Plan §5.6's blind spot, made visible: one speaker's arrival is pulled by
        // an early reflection, which biases 1.5 kHz and 3 kHz differently. Every
        // other metric still looks excellent — this is the only check that can see
        // it, and it must BLOCK, not warn (plan §10.2).
        let members = [member("a"), member("b"), member("c")];
        let o = [
            obs("a", 0, 0.0, 300.0, 0.0),
            obs("b", 0, 4.0, 306.0, 4.5), // biased speaker
            obs("c", 0, 8.0, 303.0, 0.0),
            obs("a", 1, 12.0, 300.0, 0.0),
            obs("b", 1, 16.0, 306.0, 4.5),
            obs("c", 1, 20.0, 303.0, 0.0),
        ];
        let p = solve_of(&members, &o, &[]).expect("a proposal is still produced");
        assert!(!p.checks.transitivity.passed);
        assert!((p.checks.transitivity.worst_ms - 4.5).abs() < 0.01, "worst {}", p.checks.transitivity.worst_ms);
        let blocked = p.blocked.expect("a transitivity failure must block the write");
        assert_eq!(blocked.kind, RefusalKind::Transitivity);
        assert!(blocked.message.contains("nothing is written"), "{}", blocked.message);
        // The numbers stay visible next to the refusal (plan §10).
        assert_eq!(p.members.len(), 3);
    }

    #[test]
    fn a_bias_shared_by_every_speaker_does_not_block() {
        // §5.6: "a per-speaker bias breaks transitivity; a bias shared by all
        // speakers does not" — and a shared bias is also harmless, because every
        // quantity consumed here is a difference.
        let members = [member("a"), member("b")];
        let o = [obs("a", 0, 0.0, 300.0, 6.0), obs("b", 0, 4.0, 309.0, 6.0), obs("a", 1, 8.0, 300.0, 6.0), obs("b", 1, 12.0, 309.0, 6.0)];
        let p = solve_of(&members, &o, &[]).expect("accepted");
        assert!(p.checks.transitivity.passed, "worst {}", p.checks.transitivity.worst_ms);
        assert!(p.blocked.is_none());
        // b arrives 9 ms later, so b is the one advanced (§2.4.1's inversion).
        assert_eq!(p.members.iter().find(|m| m.node_name == "b").unwrap().new_delay_ms, 9);
        assert_eq!(p.members.iter().find(|m| m.node_name == "a").unwrap().new_delay_ms, 0);
    }

    #[test]
    fn transitivity_arithmetic_is_the_cross_band_difference() {
        // Directly, without the solve around it: the residual of a triangle closed
        // with edges from two different bands is |split_i − split_j|.
        let o = [obs("a", 0, 0.0, 100.0, 0.0), obs("b", 0, 1.0, 100.0, 1.0), obs("c", 0, 2.0, 100.0, -1.0)];
        let t = transitivity(&o, &Timing::real(), TRANSITIVITY_TOL_MS);
        assert!((t.worst_ms - 2.0).abs() < 1e-9, "worst {}", t.worst_ms);
        let pair = t.worst_pair.expect("a worst pair");
        assert!(pair == ("b".into(), "c".into()) || pair == ("c".into(), "b".into()));
        assert!(t.passed, "2 ms is inside the crossover-confound tolerance");
        assert!(t.caveat.contains("crossover"));
    }

    #[test]
    fn a_member_that_moved_between_passes_blocks_on_repeatability() {
        let members = [member("a"), member("b")];
        let o = [
            obs("a", 0, 0.0, 300.0, 0.0),
            obs("b", 0, 4.0, 305.0, 0.0),
            obs("b", 1, 8.0, 305.0, 0.0),
            obs("a", 1, 12.0, 307.0, 0.0), // moved 7 ms between passes
        ];
        let p = solve_of(&members, &o, &[]).expect("a proposal is still produced");
        let rep = p.checks.repeatability.expect("two passes are checkable");
        assert!(!rep.passed, "worst {}", rep.worst_ms);
        assert_eq!(p.blocked.map(|b| b.kind), Some(RefusalKind::Repeatability));
    }

    /// A mixed group meets **in the middle**, which the old reference-member solver
    /// could not express: the sendspin member is advanced and the AP2 member delayed
    /// at the same time, and the target that minimises the larger of the two knobs
    /// sits halfway between their intrinsic arrivals.
    #[test]
    fn a_mixed_group_converges_from_both_directions_at_once() {
        let members = [SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }, member("s")];
        // The AP2 member arrives first but already carries 1500 ms of render delay,
        // so its *intrinsic* arrival is 1500 ms earlier than the sendspin member's.
        let o = [
            obs("ap2-dev-x", 0, 0.0, 300.0, 0.0),
            obs("s", 0, 4.0, 900.0, 0.0),
            obs("ap2-dev-x", 1, 8.0, 300.0, 0.0),
            obs("s", 1, 12.0, 900.0, 0.0),
        ];
        let p = solve_of(&members, &o, &[("ap2-dev-x", 1500)]).expect("feasible from both sides");
        let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
        assert_eq!(by("ap2-dev-x").polarity, KnobPolarity::Delay);
        assert_eq!(by("s").polarity, KnobPolarity::Advance);
        // Intrinsic arrivals: ap2 at −1500, s at +600. Halfway is −450, and both
        // knobs then hold 1050 ms — the smallest possible largest knob.
        assert!((p.target_ms + 450.0).abs() < 0.01, "target {}", p.target_ms);
        assert_eq!(by("ap2-dev-x").new_delay_ms, 1050);
        assert_eq!(by("s").new_delay_ms, 1050);
        assert_eq!(p.largest_knob_ms, 1050);
        // Any other target inside the interval makes one of the two knobs bigger.
        let ivs = [
            MemberInterval::new("ap2-dev-x".into(), MemberKind::Airplay2, 1500, 0.0),
            MemberInterval::new("s".into(), MemberKind::Sendspin, 0, 600.0),
        ];
        let sol = choose_target(&ivs).expect("feasible");
        for probe in [sol.lo_ms, sol.hi_ms, sol.target_ms - 100.0, sol.target_ms + 100.0] {
            let worst = ivs.iter().map(|iv| iv.knob_for(probe)).fold(0.0, f64::max);
            assert!(worst >= sol.largest_knob_ms - 1e-9, "target {probe} beats the chosen one: {worst} < {}", sol.largest_knob_ms);
        }
        // And the write is described in each member's own direction.
        assert!(by("s").effect.contains("advance 1050 ms"), "{}", by("s").effect);
        assert!(by("ap2-dev-x").effect.contains("delay 1050 ms"), "{}", by("ap2-dev-x").effect);
    }

    /// §2.4.2's real mixed-group failure, and the reason the solver refuses instead
    /// of best-effort: a sendspin member that is already the earliest can only be
    /// moved *earlier*, and an AP2 member that is already the latest only *later*, so
    /// the two achievable ranges diverge and nothing can be written.
    #[test]
    fn a_mixed_group_that_can_only_diverge_is_refused_and_names_both_members() {
        let members = [member("s"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
        let o = [
            obs("s", 0, 0.0, 300.0, 0.0),
            obs("ap2-dev-x", 0, 4.0, 320.0, 0.0),
            obs("s", 1, 8.0, 300.0, 0.0),
            obs("ap2-dev-x", 1, 12.0, 320.0, 0.0),
        ];
        let r = solve_of(&members, &o, &[]).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::KnobRange);
        // Both names, because changing the wrong speaker is the failure mode here.
        assert!(r.message.contains("'s'"), "{}", r.message);
        assert!(r.message.contains("'ap2-dev-x'"), "{}", r.message);
        // …and *why*: each member's direction, and how far apart they stay.
        assert!(r.message.contains("advance"), "{}", r.message);
        assert!(r.message.contains("delay"), "{}", r.message);
        assert!(r.message.contains("20 ms"), "the shortfall must be quantified: {}", r.message);
        assert!(!r.message.contains("would need"), "this is not a one-member knob overflow: {}", r.message);
        assert_eq!(r.member.as_deref(), Some("s"), "the binding ceiling is the sendspin member's own arrival");
    }

    /// pw-sink cannot be placed arbitrarily early: its playout delay is floored at
    /// three packet times, and that floor — not the arrivals — is what pins the
    /// target here.
    #[test]
    fn the_pw_sink_floor_can_be_what_constrains_the_target() {
        let k = knob_of(MemberKind::PwSink);
        assert_eq!(k.polarity, KnobPolarity::Delay);
        assert_eq!(k.min_ms, crate::sync_settings::PWSINK_JITTER_MIN_MS);
        assert!(k.min_ms > 0, "the floor is the whole reason pw-sink is modelled apart from AP2");

        let floor = f64::from(k.min_ms);
        let ivs = [
            // Already at the floor, arriving first: it cannot be placed any earlier
            // than `base + floor`, which is exactly where it arrives now.
            MemberInterval::new("pwsink-dev-x".into(), MemberKind::PwSink, k.min_ms, 0.0),
            MemberInterval::new("s".into(), MemberKind::Sendspin, 0, 10.0),
        ];
        assert!((ivs[0].base_ms + floor).abs() < 1e-9, "base {}", ivs[0].base_ms);
        let sol = choose_target(&ivs).expect("feasible");
        // Unconstrained, the two arms would cross at (10 + −15)/2 = −2.5 ms and both
        // knobs would hold 12.5 ms. The floor forbids anything before 0, so the
        // target is clamped there and the pw-sink member stays at its minimum.
        assert!((sol.lo_ms - 0.0).abs() < 1e-9, "lo {}", sol.lo_ms);
        assert!((sol.target_ms - 0.0).abs() < 1e-9, "target {}", sol.target_ms);
        assert!((ivs[0].knob_for(sol.target_ms) - floor).abs() < 1e-9);
        assert!((ivs[1].knob_for(sol.target_ms) - 10.0).abs() < 1e-9);
        assert!((sol.largest_knob_ms - floor).abs() < 1e-9, "the floor is the largest knob: {}", sol.largest_knob_ms);
    }

    /// §9.2's check, now driven by **advances** (§2.4.2's "new consequence"): a
    /// sendspin device plays its static delay early, so the group's lead has to
    /// cover it, and raising it reconfigures every member's stream.
    #[test]
    fn an_advance_that_crosses_the_groups_send_ahead_high_water_mark_warns() {
        let members = [member("a"), member("b")];
        let o = [obs("a", 0, 0.0, 300.0, 0.0), obs("b", 0, 4.0, 340.0, 0.0), obs("a", 1, 8.0, 300.0, 0.0), obs("b", 1, 12.0, 340.0, 0.0)];
        let current: HashMap<String, u16> = HashMap::new();
        let ctx = SendAheadContext {
            floor_ms: 100,
            unreported_floor_ms: 0,
            min_buffer_ms: [("a".to_string(), Some(80)), ("b".to_string(), Some(80))].into_iter().collect(),
        };
        let p = solve(&SolveInput {
            timing: Timing::real(),
            members: &members,
            observations: &o,
            current_delays: &current,
            send_ahead: &ctx,
            closure: None,
        })
        .expect("accepted");
        // b arrives 40 ms later, so b takes a 40 ms *advance* ⇒ 80+40 = 120 > the
        // 100 ms floor ⇒ the group's stream is reconfigured, not one connection.
        let b = p.members.iter().find(|m| m.node_name == "b").unwrap();
        assert_eq!((b.polarity, b.new_delay_ms), (KnobPolarity::Advance, 40));
        assert!(p.warnings.iter().any(|w| w.kind == WarningKind::SendAheadHighWater), "{:?}", p.warnings);
        assert!(p.warnings.iter().any(|w| w.message.contains("120 ms")));
        assert!(p.warnings.iter().any(|w| w.message.contains("advance")), "the warning must name what causes it: {:?}", p.warnings);
        // The same solve with plenty of floor does not warn.
        let ctx = SendAheadContext { floor_ms: 500, ..ctx };
        let p = solve(&SolveInput {
            timing: Timing::real(),
            members: &members,
            observations: &o,
            current_delays: &current,
            send_ahead: &ctx,
            closure: None,
        })
        .expect("accepted");
        assert!(!p.warnings.iter().any(|w| w.kind == WarningKind::SendAheadHighWater));
    }

    /// The other half of the same rule, and the delay-only case: an AP2 group still
    /// aligns to its **latest** member (§9.1 was right for delay knobs), and a delay
    /// knob — which happens inside that member's own sender — must never be counted
    /// into the sendspin group's lead, however large it is.
    #[test]
    fn an_ap2_only_group_delays_towards_the_latest_and_never_lifts_the_lead() {
        let ap2 = |n: &str| SessionMember { node_name: n.to_string(), kind: MemberKind::Airplay2 };
        let members = [ap2("ap2-dev-early"), ap2("ap2-dev-late")];
        let o = [
            obs("ap2-dev-early", 0, 0.0, 300.0, 0.0),
            obs("ap2-dev-late", 0, 4.0, 900.0, 0.0),
            obs("ap2-dev-early", 1, 8.0, 300.0, 0.0),
            obs("ap2-dev-late", 1, 12.0, 900.0, 0.0),
        ];
        let current: HashMap<String, u16> = HashMap::new();
        // Deliberately over-broad: the context claims a `min_buffer_ms` for both AP2
        // members, so if the solve fed *delays* into the mark this would warn.
        let ctx = SendAheadContext {
            floor_ms: 100,
            unreported_floor_ms: 0,
            min_buffer_ms: [("ap2-dev-early".to_string(), Some(80)), ("ap2-dev-late".to_string(), Some(80))].into_iter().collect(),
        };
        let p = solve(&SolveInput {
            timing: Timing::real(),
            members: &members,
            observations: &o,
            current_delays: &current,
            send_ahead: &ctx,
            closure: None,
        })
        .expect("accepted");
        assert_eq!(p.members.iter().find(|m| m.node_name == "ap2-dev-early").unwrap().new_delay_ms, 600);
        assert_eq!(p.members.iter().find(|m| m.node_name == "ap2-dev-late").unwrap().new_delay_ms, 0);
        assert_eq!(p.reference, "ap2-dev-late", "with delay knobs only, the latest member is still the one left alone");
        assert!(!p.warnings.iter().any(|w| w.kind == WarningKind::SendAheadHighWater), "{:?}", p.warnings);
    }

    #[test]
    fn only_advances_feed_the_send_ahead_mark() {
        let ctx =
            SendAheadContext { floor_ms: 100, unreported_floor_ms: 0, min_buffer_ms: [("s".to_string(), Some(80))].into_iter().collect() };
        let map = |pairs: &[(&str, u16)]| -> HashMap<String, u16> { pairs.iter().map(|(n, v)| ((*n).to_string(), *v)).collect() };
        // A sendspin advance is added to the lead, because the device plays that early.
        assert_eq!(ctx.mark_ms(&map(&[("s", 300)])), 380);
        // A member the context knows nothing about contributes nothing, however big.
        assert_eq!(ctx.mark_ms(&map(&[("s", 0), ("ap2-dev-x", 2000)])), 100);
    }

    #[test]
    fn the_residual_check_measures_against_the_chosen_reference() {
        let o = [obs("a", 0, 0.0, 300.0, 0.0), obs("b", 0, 4.0, 300.4, 0.0), obs("c", 0, 8.0, 299.6, 0.0)];
        let r = residual(&o, "a", 2000.0, RESIDUAL_TOL_MS);
        assert!(r.passed, "worst {}", r.worst_ms);
        let o = [obs("a", 0, 0.0, 300.0, 0.0), obs("b", 0, 4.0, 309.0, 0.0)];
        let r = residual(&o, "a", 2000.0, RESIDUAL_TOL_MS);
        assert!(!r.passed);
        assert_eq!(r.worst_member.as_deref(), Some("b"));
        // An unknown reference is a failure, never a pass by default.
        assert!(!residual(&o, "nobody", 2000.0, RESIDUAL_TOL_MS).passed);
    }

    #[test]
    fn linearising_puts_the_earliest_member_at_zero_and_wraps_the_short_way() {
        let offsets: HashMap<String, f64> = [("a".to_string(), 1990.0), ("b".to_string(), 5.0)].into_iter().collect();
        let order = vec!["a".to_string(), "b".to_string()];
        let out = linearise(&offsets, &order, 2000.0);
        // b is 15 ms *after* a, not 1985 ms before it.
        assert_eq!(out[0].0, "a");
        assert!((out[0].1 - 0.0).abs() < 1e-9);
        assert!((out[1].1 - 15.0).abs() < 1e-9, "{:?}", out);
    }

    // ------------------------------------------------- state machine, end to end

    /// A session that records what is soloed. No PipeWire, no mute protocol — the
    /// real `AlignManager` impl above is three lines over `select`/`set_level`, and
    /// what needs testing is the orchestration's decisions.
    struct FakeSession {
        members: Vec<SessionMember>,
        soloed: Arc<Mutex<Option<String>>>,
        active: Arc<AtomicBool>,
        /// Exclusivity violations to hand out on the next drain (plan §12.3).
        interference: Arc<Mutex<Vec<crate::align::group::Interference>>>,
        /// The session's per-member level map (`calibrate`'s W19 field), which is where
        /// a near-field arrival's level comes from when the request does not override
        /// it (plan §12.2).
        levels: Arc<Mutex<HashMap<String, u8>>>,
    }

    impl SessionControl for FakeSession {
        fn take_interference(&self) -> Fut<'_, Vec<crate::align::group::Interference>> {
            Box::pin(async move { std::mem::take(&mut *self.interference.lock_recover()) })
        }

        fn snapshot(&self) -> Fut<'_, SessionSnapshot> {
            Box::pin(async move {
                SessionSnapshot {
                    active: self.active.load(Ordering::Relaxed),
                    sources: vec!["src".to_string()],
                    members: self.members.clone(),
                    level: 50,
                    levels: self.levels.lock_recover().clone(),
                }
            })
        }

        fn solo(&self, node_name: String, level: u8) -> Fut<'_, Result<(), String>> {
            Box::pin(async move {
                // The real `AlignManager::solo` applies the level in the same call and
                // records it in `AlignState::levels`; both halves matter to near field,
                // so the fake does both.
                self.levels.lock_recover().insert(node_name.clone(), level);
                *self.soloed.lock_recover() = Some(node_name);
                Ok(())
            })
        }
    }

    /// A capture that renders the *real* click track for whichever member is
    /// soloed, delayed by that member's true arrival time, and hands out windows
    /// on demand from virtual time.
    ///
    /// This is a transport stand-in, not a stand-in for the thing under test: the
    /// estimator, the gate, the drift fit and the solve all run for real. What it
    /// cannot stand in for is a room — no reflections, no noise floor, no phone
    /// microphone, no AGC. Those need W0's device (see the report).
    struct FakeMic {
        rate: u32,
        pattern_ms: f64,
        soloed: Arc<Mutex<Option<String>>>,
        /// True arrival per member, in ms. Behind a lock so a test can *move a
        /// speaker* mid-walk, which is the failure the closure check exists to catch.
        arrivals: Arc<Mutex<HashMap<String, f64>>>,
        start: Instant,
        frames: AtomicU64,
        connected: Arc<AtomicBool>,
        /// Mic-vs-audio clock offset, as ms of phase per second of capture. This is
        /// what a phone's clock running at a different rate looks like from here: every
        /// member's measured arrival creeps at the same rate, indistinguishable from a
        /// real offset within any single reading.
        drift_ms_per_s: f64,
        /// Absolute frame at which the capture "reconnects" (0 = never).
        ///
        /// Scheduled rather than immediate on purpose: a reconnect is only *detectable*
        /// as the frame counter going backwards past what a reader has already consumed
        /// ([`Feeder::pull`]), so it has to happen while a reading is in progress. A
        /// test sets this a few seconds ahead, which lands it inside the next reading —
        /// after the mute guard, before the gate has its periods.
        reconnect_at: Arc<AtomicU64>,
        /// A *mechanism* under test, moving the soloed member's arrival on top of its
        /// fixed `arrivals` entry. [`NoShift`] for everything that is not W21.
        shift: Arc<dyn ArrivalShift>,
    }

    /// How something the daemon does moves a member's arrival — the seam a test injects
    /// physics through. Two implementations: [`RelayShift`], where a provisional delay
    /// simply delays that speaker (the chain's premise, plan §1.1), and [`EquivPhysics`],
    /// where the delay line's gain, the knob's *sign* and a reconnect's ε are all
    /// injectable, because whether those match is exactly what W21 measures.
    trait ArrivalShift: Send + Sync {
        fn shift_ms(&self, member: &str) -> f64;

        /// Whether this member is currently producing no sound at all — a wedged device
        /// (plan §2.3.2), and the only honest way to make a gate genuinely time out.
        fn silent(&self, _member: &str) -> bool {
            false
        }
    }

    /// How far the frame counter jumps back at a simulated reconnect: ten seconds,
    /// which is comfortably more than any reader can have consumed.
    const FAKE_RECONNECT_SHIFT_S: u64 = 10;

    impl FakeMic {
        fn frames_now(&self) -> u64 {
            let raw = (self.start.elapsed().as_secs_f64() * f64::from(self.rate)) as u64;
            let at = self.reconnect_at.load(Ordering::Relaxed);
            let shift = match at != 0 && raw >= at {
                true => u64::from(self.rate) * FAKE_RECONNECT_SHIFT_S,
                false => 0,
            };
            let f = raw.saturating_sub(shift);
            self.frames.store(f, Ordering::Relaxed);
            f
        }

        /// The click track's shape: an 8 ms Hann-enveloped 3 kHz burst at the
        /// period start and a 1.5 kHz one at the half point, exactly as
        /// `calibrate::click_wav` lays it out — on the pattern the test is using.
        fn sample(&self, frame: u64, arrival_ms: f64) -> f32 {
            let rate = f64::from(self.rate);
            let period = self.pattern_ms / 1000.0 * rate;
            let burst = 0.008 * rate;
            let t = (frame as f64 - arrival_ms / 1000.0 * rate).rem_euclid(period);
            let mut v = 0.0;
            for (offset, hz) in [(0.0, 3000.0), (period / 2.0, 1500.0)] {
                let k = t - offset;
                if k >= 0.0 && k <= burst {
                    let env = 0.5 - 0.5 * (TAU * k / burst).cos();
                    v += 0.3 * env * (TAU * hz * k / rate).sin();
                }
            }
            v as f32
        }
    }

    impl MicFeed for FakeMic {
        fn status(&self) -> MicStatus {
            MicStatus {
                connected: self.connected.load(Ordering::Relaxed),
                sample_rate: self.rate,
                frames_received: self.frames_now(),
                blocks_received: 0,
                gap_count: 0,
                peak: 0.3,
                clipped: false,
                clip_count: 0,
                buffered_frames: self.rate as usize * 10,
                capacity_frames: self.rate as usize * 10,
            }
        }

        fn window_from(&self, first_frame: u64, frames: usize) -> Option<MicWindow> {
            let head = self.frames_now();
            if first_frame + frames as u64 > head {
                return None;
            }
            let soloed = self.soloed.lock_recover().clone();
            if soloed.as_deref().is_some_and(|s| self.shift.silent(s)) {
                return Some(MicWindow { samples: vec![0.0; frames], first_frame, sample_rate: self.rate, gap: false, clipped: false });
            }
            let arrival = soloed
                .as_deref()
                .map(|s| self.arrivals.lock_recover().get(s).copied().unwrap_or(0.0) + self.shift.shift_ms(s))
                .unwrap_or(0.0);
            // The clock offset is applied per sample from the absolute frame, so it is a
            // genuine linear phase ramp rather than a per-window constant.
            let samples = (0..frames)
                .map(|i| {
                    let f = first_frame + i as u64;
                    let drift = self.drift_ms_per_s * (f as f64 / f64::from(self.rate));
                    self.sample(f, arrival + drift)
                })
                .collect();
            Some(MicWindow { samples, first_frame, sample_rate: self.rate, gap: false, clipped: false })
        }
    }

    /// A synthetic capture of the click track at a chosen burst amplitude over a
    /// chosen noise floor, for the signal check's verdict logic.
    ///
    /// `first_frame` is the absolute position the window claims, i.e. where it sits
    /// on the estimator's analysis grid. It matters for a short window: the
    /// estimator only keeps a pattern period it saw *whole*, so the number of
    /// analysed periods depends on the alignment as well as the length.
    fn signal_window_at(first_frame: u64, rate: u32, pattern_ms: f64, periods: usize, amp: f64, noise: f64) -> MicWindow {
        let mut w = signal_window(rate, pattern_ms, periods, amp, noise);
        w.first_frame = first_frame;
        w
    }

    fn signal_window(rate: u32, pattern_ms: f64, periods: usize, amp: f64, noise: f64) -> MicWindow {
        let r = f64::from(rate);
        let period = pattern_ms / 1000.0 * r;
        let burst = 0.008 * r;
        let frames = (period * periods as f64) as usize;
        // Deterministic pseudo-noise (an LCG), so the test cannot flake.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let samples = (0..frames)
            .map(|i| {
                let t = (i as f64).rem_euclid(period);
                let mut v = 0.0;
                for (offset, hz) in [(0.0, 3000.0), (period / 2.0, 1500.0)] {
                    let k = t - offset;
                    if (0.0..=burst).contains(&k) {
                        let env = 0.5 - 0.5 * (TAU * k / burst).cos();
                        v += amp * env * (TAU * hz * k / r).sin();
                    }
                }
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                let n = ((seed >> 33) as f64 / f64::from(u32::MAX >> 1)) - 1.0;
                (v + n * noise) as f32
            })
            .collect();
        MicWindow { samples, first_frame: 0, sample_rate: rate, gap: false, clipped: false }
    }

    #[test]
    fn signal_check_grades_the_level_by_the_worst_channel() {
        // A quiet room: both tones well clear of the floor.
        let good = signal_check_window(&signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.000_5), 2000.0);
        assert_eq!(good.verdict, SignalVerdict::Good, "{}", good.message);
        assert_eq!(good.channels.len(), 2, "both click-track tones must be graded");
        // The verdict follows the *worst* channel, never an average.
        let worst = good.channels.iter().map(|c| c.peak_snr_db).fold(f64::INFINITY, f64::min);
        assert!((good.worst_peak_snr_db.unwrap() - worst).abs() < 1e-9);

        // Loud speaker, loud room: measurable but without margin. Recorded because
        // it pins the offset W2 measured between *broadband* SNR (burst peak over
        // noise RMS, here ≈ −6 dB) and the *reported* peak SNR, which the matched
        // filter's processing gain lifts by roughly 24 dB — so a capture that looks
        // hopeless by ear can still measure, and the meter cannot tell you that.
        let tight = signal_check_window(&signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.5), 2000.0);
        assert_eq!(tight.verdict, SignalVerdict::Marginal, "{}", tight.message);
        assert!(tight.message.contains("tight"), "{}", tight.message);

        // A speaker far too quiet for the room: refused rather than attempted, so
        // no delay is ever written from it.
        let bad = signal_check_window(&signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.005, 0.05), 2000.0);
        assert_eq!(bad.verdict, SignalVerdict::TooQuiet, "{}", bad.message);
        assert!(bad.message.contains("Too quiet"), "{}", bad.message);
    }

    /// Plan §12.2: the green indicator has to beat the measurement gate, and it does
    /// so on a shorter window (the ordering itself is a compile-time assertion next
    /// to the two constants). What is tested here is that the shorter window still
    /// *grades* — a faster verdict that is wrong would be worse than a slow one.
    #[test]
    fn the_preflight_grades_on_a_shorter_window_than_the_gate() {
        // A window that is not aligned to the analysis grid, which is the live case:
        // the partial period at each end is dropped, leaving one whole one.
        let period_frames = 2000.0 / 1000.0 * 48_000.0;
        let offset = (period_frames / 3.0) as u64;
        let good = signal_check_window(&signal_window_at(offset, 48_000, 2000.0, PREFLIGHT_PERIODS, 0.15, 0.000_5), 2000.0);
        assert_eq!(good.verdict, SignalVerdict::Good, "{}", good.message);
        assert_eq!(good.periods, 1, "two periods of audio yield exactly one analysed period");
        assert_eq!(good.channels.len(), 2, "both tones are still graded");
        // One period has no line fit, so there is no phase — which the pre-flight does
        // not need (plan §12.2: a rough SNR, not a phase).
        assert!(good.channels.iter().all(|c| c.phase_ms == 0.0));

        let bad = signal_check_window(&signal_window_at(offset, 48_000, 2000.0, PREFLIGHT_PERIODS, 0.005, 0.05), 2000.0);
        assert_eq!(bad.verdict, SignalVerdict::TooQuiet, "{}", bad.message);
        assert!(bad.message.contains("Too quiet"), "{}", bad.message);
    }

    /// The corner the short window creates: a window that happens to sit exactly on
    /// the analysis grid has a partial period at each end and no whole one between
    /// them. The estimator then reports 0 dB from an empty median, which must not be
    /// shown as "far too quiet" — it would send the user to turn the speakers up for
    /// no reason.
    #[test]
    fn a_preflight_window_with_no_complete_period_says_so_rather_than_too_quiet() {
        let aligned = signal_check_window(&signal_window_at(0, 48_000, 2000.0, PREFLIGHT_PERIODS, 0.15, 0.000_5), 2000.0);
        assert_eq!(aligned.periods, 0, "this is the case the guard exists for");
        assert_eq!(aligned.verdict, SignalVerdict::Unusable, "{}", aligned.message);
        assert!(aligned.message.contains("Still collecting"), "{}", aligned.message);
        assert!(!aligned.message.contains("quiet"), "a loud capture must never be called quiet: {}", aligned.message);
    }

    #[test]
    fn signal_check_refuses_clipped_and_gapped_captures_before_grading() {
        let mut w = signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.000_5);
        w.clipped = true;
        let clipped = signal_check_window(&w, 2000.0);
        assert_eq!(clipped.verdict, SignalVerdict::Unusable);
        // The action must be the correct one: turning up cannot fix clipping.
        assert!(clipped.message.contains("down"), "{}", clipped.message);

        let mut w = signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.000_5);
        w.gap = false;
        w.gap = true;
        let gapped = signal_check_window(&w, 2000.0);
        assert_eq!(gapped.verdict, SignalVerdict::Unusable);
        assert!(gapped.message.contains("lost"), "{}", gapped.message);
    }

    #[derive(Default)]
    struct FakeWriter {
        writes: Mutex<Vec<(String, u16)>>,
    }

    impl FakeWriter {
        /// The knob value this member last had written to it — what a device would be
        /// rendering at.
        fn last(&self, node_name: &str) -> Option<u16> {
            self.writes.lock_recover().iter().rev().find(|(n, _)| n == node_name).map(|(_, ms)| *ms)
        }

        /// How many times this member's knob has been written, i.e. how many reconnects
        /// it has been through.
        fn count(&self, node_name: &str) -> usize {
            self.writes.lock_recover().iter().filter(|(n, _)| n == node_name).count()
        }
    }

    impl DelayWriter for FakeWriter {
        fn write(&self, node_name: String, _kind: MemberKind, delay_ms: u16) -> Fut<'_, Result<String, String>> {
            Box::pin(async move {
                self.writes.lock_recover().push((node_name.clone(), delay_ms));
                // Worded like the real sendspin handler's reply, because W21 reads it:
                // whether a write said it forced a reconnect is evidence about what the
                // two device-arm baselines are separated by (`api.rs`'s
                // `set_sendspin_delay_handler`).
                Ok(format!("set '{node_name}' static delay to {delay_ms} ms (reconnecting just this speaker to apply)"))
            })
        }
    }

    /// Production timing throughout — the async tests below run on `tokio`'s
    /// **paused** clock (`start_paused`), so a 3 s mute guard and four 2 s pattern
    /// periods per member cost no wall-clock time. The fake capture derives its
    /// frame count from `tokio::time::Instant`, so virtual time produces real
    /// audio: the estimator, the gate, the drift fit and the solve all run against
    /// the same numbers a real run uses.
    fn deps_for(arrivals: &[(&str, f64)]) -> (MeasureDeps, Arc<FakeWriter>, Arc<AtomicBool>, Arc<AtomicBool>) {
        let rig = Rig::new(arrivals, Mode::SweetSpot, 0.0);
        let (writer, active, connected) = (rig.writer.clone(), rig.active.clone(), rig.connected.clone());
        (rig.deps, writer, active, connected)
    }

    /// Everything a test needs to reach into a run: the injected truth, the transport's
    /// failure levers, and the deps the run is driven with.
    struct Rig {
        deps: MeasureDeps,
        writer: Arc<FakeWriter>,
        active: Arc<AtomicBool>,
        connected: Arc<AtomicBool>,
        /// The true per-member arrival the fake capture renders. Mutable mid-run.
        arrivals: Arc<Mutex<HashMap<String, f64>>>,
        /// Absolute frame at which the capture should look like it reconnected
        /// (plan §1.2). See [`FakeMic::reconnect_at`].
        reconnect_at: Arc<AtomicU64>,
        /// The fake capture, so a test can read its frame clock to schedule the above.
        mic: Arc<FakeMic>,
        levels: Arc<Mutex<HashMap<String, u8>>>,
        /// The provisional delay line the chain applies through (plan §1.1.1), and the
        /// thing that makes an overlap's *next* reading include the delay it is carrying.
        relay: Arc<FakeRelay>,
    }

    /// A provisional delay moves the speaker it is applied to — the physics the whole
    /// chain rests on, since a later position measures each overlap *through* the line it
    /// is carrying (plan §1.1).
    ///
    /// Deliberately nothing else: the line's own sample arithmetic and its cap are
    /// `relay_delay`'s tests, and the relay-vs-device *equivalence* is W21's.
    struct RelayShift {
        relay: Arc<FakeRelay>,
    }

    impl ArrivalShift for RelayShift {
        fn shift_ms(&self, member: &str) -> f64 {
            self.relay.applied_ms(member)
        }
    }

    impl Rig {
        fn new(arrivals: &[(&str, f64)], mode: Mode, drift_ms_per_s: f64) -> Self {
            Self::of_kinds(&arrivals.iter().map(|(n, a)| (*n, MemberKind::Sendspin, *a)).collect::<Vec<_>>(), mode, drift_ms_per_s)
        }

        /// The same rig with an explicit kind per member, for the mixed-polarity cases
        /// §2.4.2 is about.
        fn of_kinds(arrivals: &[(&str, MemberKind, f64)], mode: Mode, drift_ms_per_s: f64) -> Self {
            let timing = Timing::real();
            let members: Vec<SessionMember> =
                arrivals.iter().map(|(n, k, _)| SessionMember { node_name: (*n).to_string(), kind: *k }).collect();
            let soloed = Arc::new(Mutex::new(None));
            let active = Arc::new(AtomicBool::new(true));
            let connected = Arc::new(AtomicBool::new(true));
            let levels = Arc::new(Mutex::new(HashMap::new()));
            let arrivals: Arc<Mutex<HashMap<String, f64>>> =
                Arc::new(Mutex::new(arrivals.iter().map(|(n, _, a)| ((*n).to_string(), *a)).collect()));
            let reconnect_at = Arc::new(AtomicU64::new(0));
            let session = Arc::new(FakeSession {
                members,
                soloed: soloed.clone(),
                active: active.clone(),
                interference: Arc::new(Mutex::new(Vec::new())),
                levels: levels.clone(),
            });
            // Every rig carries a line, and an unused one applies nothing — so the tests
            // that predate W12 measure exactly what they measured before.
            let relay = Arc::new(FakeRelay::new());
            let mic = Arc::new(FakeMic {
                rate: 48_000,
                pattern_ms: timing.pattern_ms,
                soloed,
                arrivals: arrivals.clone(),
                start: Instant::now(),
                frames: AtomicU64::new(0),
                connected: connected.clone(),
                drift_ms_per_s,
                reconnect_at: reconnect_at.clone(),
                shift: Arc::new(RelayShift { relay: relay.clone() }),
            });
            let writer = Arc::new(FakeWriter::default());
            let deps = MeasureDeps {
                mode,
                chained: false,
                link_to: Vec::new(),
                session,
                mic: mic.clone(),
                writer: writer.clone(),
                relay: relay.clone(),
                current_delays: HashMap::new(),
                send_ahead: SendAheadContext::default(),
                timing,
            };
            Rig { deps, writer, active, connected, arrivals, reconnect_at, levels, mic, relay }
        }
    }

    /// The whole machine over synthetic audio: ARMING → LEARNING → MEASURING →
    /// SOLVING → PROPOSED, with a real estimator behind a fake transport.
    #[tokio::test(start_paused = true)]
    async fn a_full_run_proposes_delays_that_match_the_injected_arrivals() {
        let (deps, _w, _a, _c) = deps_for(&[("early", 0.0), ("late", 9.0)]);
        let inner = Arc::new(Mutex::new(Inner::idle()));
        inner.lock_recover().members = vec![
            MemberProgress {
                node_name: "early".into(),
                kind: MemberKind::Sendspin,
                level: 50,
                current_delay_ms: 0,
                passes_done: 0,
                last: None,
                note: None,
            },
            MemberProgress {
                node_name: "late".into(),
                kind: MemberKind::Sendspin,
                level: 50,
                current_delay_ms: 0,
                passes_done: 0,
                last: None,
                note: None,
            },
        ];
        let cancel = AtomicBool::new(false);
        let phase = run_measure(&deps, &inner, &cancel, None).await.expect("the run must reach a proposal");
        assert_eq!(phase, Phase::Proposed);
        let status = inner.lock_recover().status();
        let p = status.proposal.expect("a proposal");
        assert_eq!(p.reference, "early", "sendspin knobs advance, so the earliest member is the one left alone");
        let late = p.members.iter().find(|m| m.node_name == "late").unwrap();
        assert_eq!(late.new_delay_ms, 9, "9 ms of injected offset must come back as 9 ms of advance");
        assert_eq!(late.polarity, KnobPolarity::Advance);
        assert_eq!(p.members.iter().find(|m| m.node_name == "early").unwrap().new_delay_ms, 0);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.checks.transitivity.passed);
        // Every member measured twice, alternating.
        assert_eq!(status.observations.len(), 4);
        assert_eq!(status.observations[0].node_name, "early");
        assert_eq!(status.observations[2].node_name, "late", "pass 2 runs the list backwards");
        // The W4 seam is reported, not hidden.
        assert!(status.warnings.iter().any(|w| w.kind == WarningKind::LevelLearningSkipped));
    }

    #[tokio::test(start_paused = true)]
    async fn losing_the_session_mid_run_says_which_one_went_away() {
        let (deps, _w, active, _c) = deps_for(&[("a", 0.0), ("b", 5.0)]);
        let inner = Arc::new(Mutex::new(Inner::idle()));
        inner.lock_recover().sources = vec!["src".to_string()];
        active.store(false, Ordering::Relaxed);
        let cancel = AtomicBool::new(false);
        let r = run_measure(&deps, &inner, &cancel, None).await.expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::SessionLost);
        assert!(r.message.contains("microphone is still connected"), "{}", r.message);
    }

    #[tokio::test(start_paused = true)]
    async fn losing_the_microphone_mid_run_says_which_one_went_away() {
        let (deps, _w, _a, connected) = deps_for(&[("a", 0.0), ("b", 5.0)]);
        let inner = Arc::new(Mutex::new(Inner::idle()));
        connected.store(false, Ordering::Relaxed);
        let cancel = AtomicBool::new(false);
        let r = run_measure(&deps, &inner, &cancel, None).await.expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::MicLost);
        assert!(r.message.contains("session is still running"), "{}", r.message);
    }

    #[tokio::test(start_paused = true)]
    async fn abandoning_stops_the_run_without_writing_anything() {
        let (deps, writer, _a, _c) = deps_for(&[("a", 0.0), ("b", 5.0)]);
        let inner = Arc::new(Mutex::new(Inner::idle()));
        let cancel = AtomicBool::new(true);
        let r = run_measure(&deps, &inner, &cancel, None).await.expect_err("must stop");
        assert_eq!(r.kind, RefusalKind::Cancelled);
        assert!(writer.writes.lock_recover().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn apply_writes_only_what_changed_and_verifies_the_result() {
        // The proposal from a real run, then the write + settle + verify half. The
        // fake mic renders each member at its *post-write* arrival, so the residual
        // is what a correctly applied delay would produce.
        let (deps, writer, _a, _c) = deps_for(&[("early", 0.0), ("late", 7.0)]);
        let inner = Arc::new(Mutex::new(Inner::idle()));
        let cancel = AtomicBool::new(false);
        run_measure(&deps, &inner, &cancel, None).await.expect("proposal");
        let proposal = inner.lock_recover().proposal.clone().expect("a proposal");
        assert_eq!(proposal.reference, "early");

        // Post-write the two members arrive together — and they do so at the *earlier*
        // arrival, because what was written is an advance on the late one.
        let (mut deps2, writer2, _a2, _c2) = deps_for(&[("early", 0.0), ("late", 0.0)]);
        deps2.writer = writer2.clone();
        let phase = run_apply(&deps2, &inner, &cancel, &proposal, None).await.expect("verified");
        assert_eq!(phase, Phase::Done);
        let writes = writer2.writes.lock_recover().clone();
        assert_eq!(writes, vec![("late".to_string(), 7)], "only the member whose knob changed is written");
        assert!(writer.writes.lock_recover().is_empty());
        let v = inner.lock_recover().verification.clone().expect("a verification");
        assert!(v.passed, "residual {} ms", v.residual.worst_ms);
        assert!(v.residual.worst_ms < RESIDUAL_TOL_MS);
        assert_eq!(v.merged_peak.state, "not_implemented");
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_that_did_not_take_fails_verification_instead_of_claiming_success() {
        let (deps, _w, _a, _c) = deps_for(&[("early", 0.0), ("late", 7.0)]);
        let inner = Arc::new(Mutex::new(Inner::idle()));
        let cancel = AtomicBool::new(false);
        run_measure(&deps, &inner, &cancel, None).await.expect("proposal");
        let proposal = inner.lock_recover().proposal.clone().expect("a proposal");
        // The delay never took: the members still arrive 7 ms apart.
        let (deps2, _w2, _a2, _c2) = deps_for(&[("early", 0.0), ("late", 7.0)]);
        let r = run_apply(&deps2, &inner, &cancel, &proposal, None).await.expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::ResidualTooLarge);
        let v = inner.lock_recover().verification.clone().expect("the numbers are still reported");
        assert!(!v.passed);
    }

    // ----------------------------------------------------------- misc units

    #[test]
    fn the_phase_machine_only_starts_from_a_terminal_state() {
        for p in [Phase::Idle, Phase::Done, Phase::Refused, Phase::Proposed] {
            assert!(p.is_terminal(), "{p:?}");
        }
        for p in [
            Phase::Arming,
            Phase::Learning,
            // Parked waiting for the user is *not* terminal: the run is alive and
            // holding the group, so a second `start` must refuse rather than quietly
            // abandoning a walk in progress — or, for a chain, stranding an apartment's
            // worth of provisional delays with nothing left that knows about them.
            Phase::Walking,
            Phase::Positioning,
            Phase::Measuring,
            Phase::Solving,
            Phase::Writing,
            Phase::Settling,
            Phase::Verifying,
        ] {
            assert!(!p.is_terminal(), "{p:?}");
        }
    }

    #[test]
    fn the_send_ahead_mark_takes_the_largest_member_requirement() {
        let ctx = SendAheadContext {
            floor_ms: 50,
            unreported_floor_ms: 250,
            min_buffer_ms: [("a".to_string(), Some(100)), ("b".to_string(), None)].into_iter().collect(),
        };
        let none: HashMap<String, u16> = HashMap::new();
        assert_eq!(ctx.mark_ms(&none), 250, "an unreported member still needs the codec floor");
        let delays: HashMap<String, u16> = [("a".to_string(), 400u16)].into_iter().collect();
        assert_eq!(ctx.mark_ms(&delays), 500, "a member's delay is part of its send-ahead");
    }

    #[test]
    fn the_level_seam_matches_what_the_level_solver_actually_takes() {
        use crate::align::levels::{LevelConfig, LevelSolver, RampMode};
        let members = [member("a"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
        let seam = learn_levels(42, &members);
        assert!(!seam.learned);
        assert_eq!(seam.levels.get("a"), Some(&42));
        assert_eq!(seam.levels.len(), 2);
        assert!(seam.note.contains("W4"));
        // Stage 1 shares one estimator channel across every member (plan §2.2), so
        // the solve *must* be sequential — the parallel mode refuses exactly this
        // member model, and that refusal is the thing worth pinning down before W4
        // is wired up rather than after.
        assert_eq!(seam.config.mode, RampMode::Sequential);
        assert!(LevelSolver::with_config(seam.specs.clone(), seam.config.clone()).is_ok());
        let parallel = LevelConfig { mode: RampMode::Parallel, ..seam.config.clone() };
        let err = LevelSolver::with_config(seam.specs.clone(), parallel).err().expect("one channel for two members");
        assert!(err.contains("one measurement channel per member"), "{err}");
        // AP2's knob needs a snapshot/restore the session does not have yet — the
        // documented half of why this is still a seam.
        assert_eq!(seam.specs[1].kind.knob(), crate::align::levels::LevelKnob::SnapshotRestore);
        assert!(seam.specs.iter().all(|s| s.snapshot_level.is_none()));
    }

    #[tokio::test]
    async fn revert_restores_only_the_members_that_were_actually_written() {
        // Plan §9.4 is one click back — but a revert that rewrites every member's
        // delay would reconnect devices that were never touched, and each reconnect
        // is tens of seconds of silence (plan §2.3).
        let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
        {
            let mut g = m.inner.lock_recover();
            g.snapshot =
                [("a".to_string(), (MemberKind::Sendspin, 12u16)), ("b".to_string(), (MemberKind::Sendspin, 0u16))].into_iter().collect();
            g.written = vec!["a".to_string()];
        }
        let w = FakeWriter::default();
        let s = m.revert(&w).await.expect("restores");
        assert_eq!(w.writes.lock_recover().clone(), vec![("a".to_string(), 12)]);
        assert!(!s.can_revert, "there is nothing left to restore");
        assert_eq!(s.revert_scope, None, "nothing pending, so nothing to scope");
        assert!(m.revert(&w).await.is_err(), "a second revert must refuse rather than reconnect the group again");
        assert_eq!(w.writes.lock_recover().len(), 1);
    }

    /// The write survives `abandon`, so the *pointer to it* has to as well — a page
    /// reload is otherwise the end of the only route back from a destructive change
    /// (plan §9.4).
    #[test]
    fn abandoning_after_a_write_keeps_the_revert_snapshot_and_its_scope() {
        let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
        {
            let mut g = m.inner.lock_recover();
            g.sources = vec!["src-a".to_string(), "src-b".to_string()];
            g.snapshot = [("a".to_string(), (MemberKind::Sendspin, 12u16))].into_iter().collect();
            g.mark_written("a");
        }
        let before = m.status();
        assert_eq!(before.revert_scope.as_deref(), Some(["src-a".to_string(), "src-b".to_string()].as_slice()));

        let s = m.abandon();
        assert!(s.can_revert, "a written delay must stay revertable after abandoning");
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.sources, Vec::<String>::new(), "the run itself is gone");
        assert_eq!(
            s.revert_scope.as_deref(),
            Some(["src-a".to_string(), "src-b".to_string()].as_slice()),
            "the group the pending revert belongs to must outlive the run"
        );
        // Still there on a later poll, i.e. it is state and not a one-shot reply.
        assert_eq!(m.status().revert_scope, s.revert_scope);
    }

    #[test]
    fn a_run_with_nothing_written_has_no_revert_scope() {
        let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
        m.inner.lock_recover().sources = vec!["src".to_string()];
        let s = m.status();
        assert!(!s.can_revert);
        assert_eq!(s.revert_scope, None, "`can_revert` and `revert_scope` must agree");
        assert_eq!(m.abandon().revert_scope, None);
    }

    /// Plan §11: the status is pushed, not polled. `measure_ws` is a thin wrapper
    /// over this notification, so what is worth testing is that the notification
    /// actually fires on a state change — and that a reset does not disconnect a
    /// subscriber that is already watching.
    #[tokio::test]
    async fn the_status_notifier_fires_on_a_change_and_survives_a_reset() {
        let m = MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) };
        let mut rx = m.subscribe();
        assert!(!rx.has_changed().unwrap(), "a fresh subscriber starts level");

        set_phase(&m.inner, Phase::Measuring, "measuring 'a'");
        assert!(rx.has_changed().expect("the notifier must still be alive"));
        rx.changed().await.expect("a change");
        assert_eq!(m.status().phase, Phase::Measuring);

        m.inner.lock_recover().warn(Warning::new(WarningKind::NoDriftFit, "no fit"));
        assert!(rx.has_changed().unwrap(), "a warning is a change too");
        rx.changed().await.unwrap();

        // `abandon` replaces the whole state; the socket watching it must not be cut
        // off by that, or the UI would go silent exactly when the run ended.
        m.abandon();
        assert!(rx.has_changed().expect("abandon must not drop the notifier"));
        rx.changed().await.expect("the reset is itself a change");
        assert_eq!(m.status().phase, Phase::Idle);
    }

    // ------------------------------------------------- near field (W8a), end to end

    fn manager() -> MeasureManager {
        MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) }
    }

    /// Poll the run's own status until it says what the test is waiting for. On the
    /// paused clock these sleeps cost nothing and the fake capture's frame clock
    /// advances with them, so a whole walk runs in milliseconds of real time.
    async fn wait_for(m: &MeasureManager, what: &str, pred: impl Fn(&MeasureStatus) -> bool) -> MeasureStatus {
        for _ in 0..40_000u32 {
            let s = m.status();
            if pred(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let s = m.status();
        panic!("timed out waiting for {what}; phase {:?}, walk {:?}, message {}", s.phase, s.walk.map(|w| w.next), s.message);
    }

    async fn wait_ready(m: &MeasureManager, next: WalkAction) -> MeasureStatus {
        wait_for(m, "the walk to be ready", |s| s.walk.as_ref().is_some_and(|w| w.next == next)).await
    }

    async fn wait_terminal(m: &MeasureManager) -> MeasureStatus {
        wait_for(m, "the run to reach a terminal state", |s| matches!(s.phase, Phase::Proposed | Phase::Refused | Phase::Done)).await
    }

    /// Drive a whole walk exactly as the UI would: one arrival per speaker in the
    /// user's own order, then the closure back at the first one.
    async fn walk_all(m: &MeasureManager, order: &[&str]) {
        for name in order {
            wait_ready(m, WalkAction::Arrival).await;
            m.arrival((*name).to_string(), None).unwrap_or_else(|r| panic!("arrival at '{name}' refused: {}", r.message));
        }
        let s = wait_ready(m, WalkAction::Close).await;
        assert!(s.walk.as_ref().is_some_and(|w| w.remaining.is_empty()));
        m.close().unwrap_or_else(|r| panic!("close refused: {}", r.message));
    }

    fn proposed(s: &MeasureStatus, node: &str) -> ProposedDelay {
        s.proposal.as_ref().expect("a proposal").members.iter().find(|m| m.node_name == node).expect("the member").clone()
    }

    /// The headline case: the user walks to each speaker, and what comes back is each
    /// speaker's **wire** delay — including plan §2.4.2's inversion, since a
    /// sendspin-only group is aligned to its *earliest* member.
    #[tokio::test(start_paused = true)]
    async fn a_near_field_walk_recovers_the_injected_wire_delays() {
        let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, 0.0);
        let levels = rig.levels.clone();
        let m = manager();
        let started = m.start(rig.deps).await.expect("near field must start");
        assert_eq!(started.mode, Mode::NearField);

        // Deliberately not the member order: the walk order is the user's to choose.
        walk_all(&m, &["b", "a", "c"]).await;
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);

        let p = s.proposal.clone().expect("a proposal");
        assert_eq!(p.reference, "a", "sendspin knobs advance, so the earliest arrival is left alone — not the walk's first speaker");
        assert_eq!(proposed(&s, "a").new_delay_ms, 0);
        assert_eq!(proposed(&s, "b").new_delay_ms, 6);
        assert_eq!(proposed(&s, "c").new_delay_ms, 11);
        assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(s.can_apply);

        let w = s.walk.clone().expect("a walk");
        assert_eq!(w.purpose, WalkPurpose::Measure);
        assert_eq!(w.next, WalkAction::Done);
        assert_eq!(w.measured, vec!["b".to_string(), "a".to_string(), "c".to_string()], "the walk order is reported as walked");
        assert_eq!(w.anchor.as_deref(), Some("b"), "the closure anchor is where the walk started");
        let c = w.closure.clone().expect("a closure report");
        assert!(c.passed, "{c:?}");
        assert_eq!(c.anchor, "b");
        assert!(c.span_s > 20.0, "the closure has to span the whole walk to be worth anything: {} s", c.span_s);

        // One reading per member plus the closure.
        assert_eq!(s.observations.len(), 4);
        assert_eq!(s.observations.iter().filter(|o| o.node_name == "b").count(), 2);

        // §12.2: the level was applied per arrival, from the session's own map.
        assert_eq!(levels.lock_recover().len(), 3, "every speaker was soloed at its own level");

        // The closure replaces repeatability rather than sitting beside it, and the
        // drift *was* fitted, so there is no "no drift fit" warning.
        assert!(p.checks.repeatability.is_none(), "a walk's repeatability check would be an identity, not evidence");
        assert!(p.checks.closure.is_some());
        assert!(!s.warnings.iter().any(|w| w.kind == WarningKind::NoDriftFit));
        assert!(!s.warnings.iter().any(|w| w.kind == WarningKind::LevelLearningSkipped), "near field has no level phase to skip");

        // The premise the user has to keep is stated, on every run.
        let warn = s.warnings.iter().find(|w| w.kind == WarningKind::NearFieldPathAssumed).expect("the path assumption is stated");
        assert!(warn.message.contains("3 ms"), "{}", warn.message);
        assert!(warn.message.contains("nothing in this measurement can tell that apart"), "{}", warn.message);
        // …and so is what the result is *not* coherent with (plan §1.2, W8b).
        assert!(w.scope_note.contains("not related to any"), "{}", w.scope_note);
        assert!(w.level_note.contains("clipping"), "{}", w.level_note);
    }

    /// The closure measurement doing the job it exists for: a mic clock running fast
    /// makes every arrival creep, one pass cannot see it (plan §5.3), and without the
    /// closure the creep would be written into the speakers as delay.
    #[tokio::test(start_paused = true)]
    async fn the_closure_measurement_recovers_the_drift_and_takes_it_back_out() {
        // 0.1 ms of phase per second of capture = 100 ppm, the realistic phone figure
        // §5.4.1 exercises.
        let drift = 0.1;
        let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, drift);
        let m = manager();
        m.start(rig.deps).await.expect("started");
        walk_all(&m, &["a", "b", "c"]).await;
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);

        let c = s.walk.clone().expect("a walk").closure.expect("a closure");
        // The closure error is the drift accumulated over the walk it actually took.
        let expected = drift * c.span_s;
        assert!((c.error_ms - expected).abs() < 0.5, "closure {} ms, expected about {expected:.2} ms over {} s", c.error_ms, c.span_s);
        assert!((c.drift_ppm - 100.0).abs() < 20.0, "implied drift {} ppm", c.drift_ppm);
        assert!(c.passed, "100 ppm is exactly what this is supposed to accept: {c:?}");
        let p = s.proposal.clone().expect("a proposal");
        assert!((p.drift_ppm - 100.0).abs() < 20.0, "the solve's own fit must agree: {} ppm", p.drift_ppm);

        // And the arrivals come back as the *wire* delays, not as wire + creep.
        assert_eq!(proposed(&s, "a").new_delay_ms, 0);
        assert_eq!(proposed(&s, "b").new_delay_ms, 6);
        assert_eq!(proposed(&s, "c").new_delay_ms, 11);

        // The correction is distributed by *when* each member was measured: later in
        // the walk, more correction. Without it 'c' would have been out by more than
        // the knob's own 1 ms granularity, which is what makes the closure worth
        // walking back for rather than assuming the clocks agree.
        let (ca, cb, cc) = (proposed(&s, "a"), proposed(&s, "b"), proposed(&s, "c"));
        assert!(cb.drift_correction_ms > 0.0 && cc.drift_correction_ms > cb.drift_correction_ms, "b {cb:?} c {cc:?}");
        assert!(cc.drift_correction_ms > 1.0, "an uncorrected walk would have been wrong by {} ms", cc.drift_correction_ms);
        assert!(cc.drift_correction_ms < c.error_ms, "no member can be corrected by more than the whole closure error");
        // 'a' is the anchor, so it was read at both ends of the walk and its correction
        // is quoted where the fit places it — between the two visits, not at either.
        assert!(
            cb.drift_correction_ms < ca.drift_correction_ms && ca.drift_correction_ms < cc.drift_correction_ms,
            "the anchor sits between its two visits: b {} a {} c {}",
            cb.drift_correction_ms,
            ca.drift_correction_ms,
            cc.drift_correction_ms
        );
    }

    /// The refusal that matters: if the two readings of the anchor disagree by more
    /// than any clock can explain, something *moved*, and since the correction was
    /// applied to every member the whole walk goes — not one reading.
    #[tokio::test(start_paused = true)]
    async fn an_implausible_closure_refuses_the_whole_walk() {
        let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, 0.0);
        let arrivals = rig.arrivals.clone();
        let m = manager();
        m.start(rig.deps).await.expect("started");

        for name in ["a", "b", "c"] {
            wait_ready(&m, WalkAction::Arrival).await;
            m.arrival(name.to_string(), None).expect("accepted");
        }
        wait_ready(&m, WalkAction::Close).await;
        // Between the last speaker and the walk back, 'a' moves 40 ms — a speaker
        // dragged across a room, or the phone held at arm's length this time.
        arrivals.lock_recover().insert("a".to_string(), 40.0);
        m.close().expect("accepted");

        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Refused, "{}", s.message);
        let r = s.refusal.clone().expect("a refusal");
        assert_eq!(r.kind, RefusalKind::ClosureError);
        assert_eq!(r.member.as_deref(), Some("a"));
        assert!(r.message.contains("whole walk is discarded"), "{}", r.message);
        assert!(r.message.contains("ppm"), "the implied rate is what makes the verdict: {}", r.message);
        assert!(!s.can_apply, "nothing may be written from an unclosed walk");

        // The numbers stay visible next to the refusal (plan §10).
        let c = s.walk.clone().expect("a walk").closure.expect("a closure");
        assert!(!c.passed);
        assert!(c.error_ms.abs() > 30.0, "closure {} ms", c.error_ms);
        assert!(c.drift_ppm.abs() > MAX_CLOSURE_DRIFT_PPM, "{} ppm", c.drift_ppm);
        let p = s.proposal.clone().expect("the proposal is still reported, blocked");
        assert_eq!(p.blocked.map(|b| b.kind), Some(RefusalKind::ClosureError));
        assert!(p.checks.closure.is_some_and(|c| !c.passed));
    }

    /// Plan §1.2: a walk is **one capture**. A reconnect restarts `align_mic`'s frame
    /// counter, so nothing measured before it can be compared with anything after —
    /// the walk starts again from its first speaker, and the user is told so instead of
    /// two frames being silently mixed.
    #[tokio::test(start_paused = true)]
    async fn a_mic_reconnect_mid_walk_restarts_the_walk_instead_of_mixing_frames() {
        let rig = Rig::new(&[("a", 0.0), ("b", 6.0), ("c", 11.0)], Mode::NearField, 0.0);
        let (reconnect_at, mic) = (rig.reconnect_at.clone(), rig.mic.clone());
        let m = manager();
        m.start(rig.deps).await.expect("started");

        for name in ["a", "b"] {
            wait_ready(&m, WalkAction::Arrival).await;
            m.arrival(name.to_string(), None).expect("accepted");
        }
        let s = wait_ready(&m, WalkAction::Arrival).await;
        assert_eq!(s.walk.as_ref().unwrap().measured, vec!["a".to_string(), "b".to_string()]);

        // Schedule the reconnect for 5 s from now: past the 3 s mute guard, so it lands
        // inside the next reading, which is the only place it is detectable.
        reconnect_at.store(mic.frames_now() + u64::from(mic.rate) * 5, Ordering::Relaxed);
        m.arrival("c".to_string(), None).expect("accepted");

        let s = wait_for(&m, "the walk to restart", |s| s.walk.as_ref().is_some_and(|w| w.restarts == 1 && w.next == WalkAction::Arrival))
            .await;
        let w = s.walk.clone().expect("a walk");
        assert!(w.measured.is_empty(), "everything from the old capture must be discarded, not kept: {:?}", w.measured);
        assert_eq!(w.remaining.len(), 3);
        assert_eq!(w.anchor, None, "there is no anchor until the walk starts again");
        assert!(s.observations.is_empty(), "no observation may survive the seam");
        assert!(w.prompt.contains("begins again"), "{}", w.prompt);
        let warn = s.warnings.iter().find(|x| x.kind == WarningKind::MicReconnected).expect("the user has to be told");
        assert!(warn.message.contains("start again from the first speaker"), "{}", warn.message);
        assert!(warn.message.contains("discarded"), "{}", warn.message);

        // And the restarted walk still works: same run, one capture, from scratch.
        walk_all(&m, &["a", "b", "c"]).await;
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
        assert_eq!(proposed(&s, "c").new_delay_ms, 11);
        // Every observation the solve used came from the *same* capture.
        let epochs: Vec<u64> = s.observations.iter().map(|o| o.grid_epoch).collect();
        assert!(epochs.windows(2).all(|w| w[0] == w[1]), "{epochs:?}");
        assert_eq!(epochs.first().copied(), Some(1), "the post-reconnect epoch, not the pre-reconnect one");
    }

    /// Near field's write is checked by **walking again**, because a residual measured
    /// from one spot would be each speaker's distance to that spot rather than the
    /// write (see [`WalkPurpose::Verify`]).
    #[tokio::test(start_paused = true)]
    async fn applying_a_near_field_proposal_verifies_by_walking_again() {
        let rig = Rig::new(&[("early", 0.0), ("late", 7.0)], Mode::NearField, 0.0);
        let m = manager();
        m.start(rig.deps).await.expect("started");
        walk_all(&m, &["early", "late"]).await;
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);

        // Post-write the two arrive together, at the earlier time: what was written is
        // an advance on the late one.
        let after = Rig::new(&[("early", 0.0), ("late", 0.0)], Mode::NearField, 0.0);
        let writer = after.writer.clone();
        m.apply(after.deps).await.expect("apply accepted");

        let s = wait_ready(&m, WalkAction::Arrival).await;
        let w = s.walk.clone().expect("a verification walk");
        assert_eq!(w.purpose, WalkPurpose::Verify, "the check is a walk, not a reading from wherever the phone is");
        assert!(w.prompt.contains("check"), "{}", w.prompt);
        walk_all(&m, &["early", "late"]).await;

        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Done, "{}", s.message);
        assert_eq!(writer.writes.lock_recover().clone(), vec![("late".to_string(), 7)], "only the knob that changed is written");
        let v = s.verification.clone().expect("a verification");
        assert!(v.passed, "residual {} ms", v.residual.worst_ms);
        assert!(v.residual.worst_ms < RESIDUAL_TOL_MS);
    }

    /// The out-of-order cases, all of which are states the user can act on — so they
    /// are refusals with a sentence, never a 500 and never silence (plan §11).
    #[tokio::test(start_paused = true)]
    async fn the_walk_refuses_calls_that_do_not_match_where_it_is() {
        let rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::NearField, 0.0);
        let m = manager();
        m.start(rig.deps).await.expect("started");
        wait_ready(&m, WalkAction::Arrival).await;

        // A speaker that is not in the group.
        let r = m.arrival("ghost".to_string(), None).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
        assert_eq!(r.member.as_deref(), Some("ghost"));
        // Closing before anything has been walked.
        let r = m.close().expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
        assert!(r.message.contains("nothing to close"), "{}", r.message);

        m.arrival("a".to_string(), None).expect("accepted");
        // A second tap while the reading is in flight.
        let r = m.arrival("b".to_string(), None).expect_err("must refuse");
        assert!(r.message.contains("busy"), "{}", r.message);

        wait_ready(&m, WalkAction::Arrival).await;
        // The same speaker twice — the only legitimate repeat is the closure.
        let r = m.arrival("a".to_string(), None).expect_err("must refuse");
        assert!(r.message.contains("already been measured"), "{}", r.message);
        assert!(r.message.contains("closure reading at 'a'"), "{}", r.message);

        m.arrival("b".to_string(), None).expect("accepted");
        wait_ready(&m, WalkAction::Close).await;
        // An arrival when only the closure is left names where to go.
        let r = m.arrival("b".to_string(), None).expect_err("must refuse");
        assert!(r.message.contains("closure reading at 'a'"), "{}", r.message);

        // Abandoning mid-walk stops the run and writes nothing.
        let after = m.abandon();
        assert_eq!(after.phase, Phase::Idle);
        assert!(after.walk.is_none(), "abandoning clears the walk with the run");
        let r = m.close().expect_err("an abandoned walk takes nothing");
        assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
    }

    /// A multi-position run takes no arrivals, and says why rather than accepting one
    /// and doing nothing with it.
    #[tokio::test(start_paused = true)]
    async fn a_multi_position_run_is_not_a_walk_and_says_so() {
        let m = manager();
        let r = m.arrival("a".to_string(), None).expect_err("idle refuses");
        assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);

        let rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::SweetSpot, 0.0);
        m.start(rig.deps).await.expect("started");
        let r = m.arrival("a".to_string(), None).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::WalkOutOfOrder);
        assert!(r.message.contains("measures every member itself"), "{}", r.message);

        // …and the multi-position path is otherwise exactly what it was: two passes,
        // a real repeatability check, no walk state, and the level-phase seam reported.
        let s = wait_for(&m, "the multi-position run to propose", |s| matches!(s.phase, Phase::Proposed | Phase::Refused)).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
        assert!(s.walk.is_none(), "a multi-position run has no walk");
        assert_eq!(s.observations.len(), 4, "two passes over two members");
        let p = s.proposal.expect("a proposal");
        assert!(p.checks.repeatability.is_some(), "two passes are checkable, and that check must not have been dropped");
        assert!(p.checks.closure.is_none());
        assert!(s.warnings.iter().any(|w| w.kind == WarningKind::LevelLearningSkipped));
        assert!(!s.warnings.iter().any(|w| w.kind == WarningKind::NearFieldPathAssumed));
    }

    /// Linking a walk to a previously aligned set is plan §1.2's cross-session case and
    /// it is **not** implemented. Refusing is the honest answer: a run that claimed to
    /// link and did not would leave the user believing in coherence that is not there.
    #[tokio::test(start_paused = true)]
    async fn linking_to_an_earlier_run_is_refused_rather_than_ignored() {
        let mut rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::NearField, 0.0);
        rig.deps.link_to = vec!["sendspin-dev-hall".to_string()];
        let m = manager();
        let r = m.start(rig.deps).await.expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::ModeUnsupported);
        assert!(r.message.contains("sendspin-dev-hall"), "{}", r.message);
        assert!(r.message.contains("in one run"), "{}", r.message);
        assert_eq!(m.status().phase, Phase::Idle, "a refused start must leave no run behind");
    }

    // ------------------------------------ multi-position chaining (W12), end to end

    /// A chained run over synthetic audio, with the provisional delay line wired to the
    /// physics ([`RelayShift`]): a delay the chain applies really does move that speaker,
    /// which is what makes an overlap's *next* reading include it (plan §1.1).
    fn chain_rig(members: &[&str]) -> Rig {
        let mut rig = Rig::new(&members.iter().map(|n| (*n, 0.0)).collect::<Vec<_>>(), Mode::SweetSpot, 0.0);
        rig.deps.chained = true;
        rig
    }

    fn chain_rig_of(members: &[(&str, MemberKind)]) -> Rig {
        let mut rig = Rig::of_kinds(&members.iter().map(|(n, k)| (*n, *k, 0.0)).collect::<Vec<_>>(), Mode::SweetSpot, 0.0);
        rig.deps.chained = true;
        rig
    }

    async fn wait_chain(m: &MeasureManager) -> MeasureStatus {
        wait_for(m, "the chain to be waiting for a position", |s| {
            s.chain.as_ref().is_some_and(|c| matches!(c.next, ChainAction::Position | ChainAction::Finish))
        })
        .await
    }

    fn chain_of(s: &MeasureStatus) -> ChainProgress {
        s.chain.clone().unwrap_or_else(|| panic!("a chain; phase {:?}, message {}", s.phase, s.message))
    }

    fn applied(s: &MeasureStatus, node: &str) -> u16 {
        chain_of(s).provisional.iter().find(|p| p.node_name == node).map(|p| p.applied_ms).unwrap_or_else(|| {
            panic!(
                "'{node}' has no provisional delay; lines are {:?}",
                chain_of(s).provisional.iter().map(|p| &p.node_name).collect::<Vec<_>>()
            )
        })
    }

    /// Stand at a listening position: tell the fake capture what is audible from there —
    /// wire delay **plus that spot's path** per speaker, exactly as a mic in one place
    /// hears it (plan §1) — and then post the position.
    ///
    /// The provisional delays are *not* part of these numbers: the rig's [`RelayShift`]
    /// adds whatever the line is carrying, which is the whole point.
    async fn at_position(
        m: &MeasureManager,
        arrivals: &Arc<Mutex<HashMap<String, f64>>>,
        heard: &[(&str, f64)],
        members: &[&str],
        overlaps: &[&str],
    ) {
        wait_chain(m).await;
        *arrivals.lock_recover() = heard.iter().map(|(n, a)| ((*n).to_string(), *a)).collect();
        m.position(members.iter().map(|s| (*s).to_string()).collect(), overlaps.iter().map(|s| (*s).to_string()).collect())
            .unwrap_or_else(|r| panic!("position {members:?} through {overlaps:?} refused: {}", r.message));
    }

    /// The headline case: two rooms, one microphone, and a result that is right in *both*
    /// places — recovered from arrivals injected separately per position.
    #[tokio::test(start_paused = true)]
    async fn a_two_step_chain_recovers_the_arrivals_injected_at_both_positions() {
        let rig = chain_rig(&["a", "b", "c", "d"]);
        let (writer, relay, arrivals) = (rig.writer.clone(), rig.relay.clone(), rig.arrivals.clone());
        let m = manager();
        m.start(rig.deps).await.expect("a chained run must start");

        // Position 1, in the living room: 'b' arrives 5 ms after 'a', so 'a' is delayed to
        // meet it — the latest arrival is the target (plan §1.1).
        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
        let s = wait_chain(&m).await;
        assert_eq!(applied(&s, "a"), 5, "the earlier speaker is delayed to the later one");
        assert_eq!(applied(&s, "b"), 0);
        assert_eq!(chain_of(&s).steps[0].confidence, OverlapConfidence::Origin);
        assert_eq!(chain_of(&s).remaining, vec!["c".to_string(), "d".to_string()]);

        // Position 2, in the kitchen: 'a' and 'b' are still audible and are the overlaps.
        // With the 5 ms 'a' is carrying they arrive together here at 25 ms, and the new
        // speakers arrive earlier, so nothing already aligned has to move.
        at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("c", 10.0), ("d", 13.0)], &["c", "d"], &["a", "b"]).await;
        let s = wait_chain(&m).await;
        let chain = chain_of(&s);
        assert_eq!(chain.next, ChainAction::Finish, "every speaker is aligned somewhere: {}", chain.prompt);
        assert_eq!(chain.steps.len(), 2);
        let two = &chain.steps[1];
        assert_eq!(two.confidence, OverlapConfidence::Checked);
        assert!(two.disagreement_ms.is_some_and(|d| d < 1.0), "the two overlaps agree here: {two:?}");
        assert!(two.delta_ms.abs() < 0.6, "nothing new arrives later than the aligned set, so Δ is 0: {}", two.delta_ms);
        assert_eq!(applied(&s, "c"), 15);
        assert_eq!(applied(&s, "d"), 12);
        assert_eq!(applied(&s, "a"), 5, "an aligned member only moves when Δ says so");
        assert_eq!(chain.floor_ms, 0.0, "the floor has not ratcheted yet — 'b' is still at zero");

        // Finishing renormalises globally and proposes the one write.
        m.finish().expect("finish accepted");
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
        assert!(writer.writes.lock_recover().is_empty(), "a chain writes nothing until `apply` (plan §1.1.1)");
        assert!(relay.applied_ms("c") > 0.0, "the proposal the user is listening to is the delay line");

        // Both positions' geometry is reproduced by the knobs: an advance of `x` plays
        // that much earlier, so a member that needed 5 ms *more* delay than another ends
        // with 5 ms *less* advance.
        assert_eq!(proposed(&s, "a").new_delay_ms, 10);
        assert_eq!(proposed(&s, "b").new_delay_ms, 15);
        assert_eq!(proposed(&s, "c").new_delay_ms, 0);
        assert_eq!(proposed(&s, "d").new_delay_ms, 3);
        let p = s.proposal.clone().expect("a proposal");
        assert_eq!(p.reference, "c", "the member that needed the most delay ends at knob zero (§2.4.2)");
        assert!(p.members.iter().all(|x| x.polarity == KnobPolarity::Advance));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(s.can_apply);
        // Position 1's 5 ms between 'a' and 'b' survives, and so does position 2's 3 ms.
        assert_eq!(proposed(&s, "b").new_delay_ms - proposed(&s, "a").new_delay_ms, 5);
        assert_eq!(proposed(&s, "d").new_delay_ms - proposed(&s, "c").new_delay_ms, 3);

        // What it does *not* promise is stated, on every chained run.
        let scope = s.warnings.iter().find(|w| w.kind == WarningKind::ChainScope).expect("the doorway caveat is not optional");
        assert!(scope.message.contains("doorway"), "{}", scope.message);
        assert!(scope.message.contains("indirectly"), "{}", scope.message);
        assert!(chain_of(&s).error.bounded, "both joints were checked by two overlaps");
    }

    /// **The trick the whole feature rests on** (plan §1.1): when a new speaker arrives
    /// later than the already-aligned set, the set gains Δ — and Δ goes to *every* member
    /// of it, not just to the overlap that was measured, because a common delay preserves
    /// that set's internal alignment.
    #[tokio::test(start_paused = true)]
    async fn a_delta_moves_the_whole_aligned_set_and_not_just_the_overlap() {
        // 'e' is aligned at position 1 and is **not** audible at position 2, so it is
        // never measured there: it can only move if Δ was propagated to the whole set.
        let rig = chain_rig(&["a", "b", "e", "c", "d"]);
        let arrivals = rig.arrivals.clone();
        let m = manager();
        m.start(rig.deps).await.expect("started");

        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0), ("e", 2.0)], &["a", "b", "e"], &[]).await;
        let s = wait_chain(&m).await;
        assert_eq!((applied(&s, "a"), applied(&s, "b"), applied(&s, "e")), (5, 0, 3));

        // At position 2 the overlaps arrive together at 15 ms while the new speakers
        // arrive at 30 and 25: the target is 30, so the aligned set is 15 ms early.
        at_position(&m, &arrivals, &[("a", 10.0), ("b", 15.0), ("c", 30.0), ("d", 25.0)], &["c", "d"], &["a", "b"]).await;
        let s = wait_chain(&m).await;
        let two = &chain_of(&s).steps[1];
        assert!((two.delta_ms - 15.0).abs() < 0.6, "Δ should be 15 ms: {}", two.delta_ms);
        assert_eq!(applied(&s, "a"), 20, "the measured overlap gained Δ");
        assert_eq!(applied(&s, "b"), 15, "so did the other overlap");
        assert_eq!(applied(&s, "e"), 18, "and so did the member that was not even audible here — that is the trick");
        assert_eq!(applied(&s, "c"), 0);
        assert_eq!(applied(&s, "d"), 5);
        // Position 1's internal alignment is untouched by the common shift.
        assert_eq!(applied(&s, "a") - applied(&s, "b"), 5);
        assert_eq!(applied(&s, "e") - applied(&s, "b"), 3);
        assert!((chain_of(&s).floor_ms - 0.0).abs() < 1e-9, "'c' is the new floor at 0 ms");
        assert!(two.note.contains("common delay"), "{}", two.note);

        // And the floor that ratcheted is taken back out by the global renormalisation, so
        // nothing is left carrying delay for nothing (plan §1.1).
        m.finish().expect("finish accepted");
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
        let p = s.proposal.clone().expect("a proposal");
        assert_eq!(p.members.iter().map(|x| x.new_delay_ms).min(), Some(0), "the renormalisation puts the smallest knob back at zero");
        assert_eq!(proposed(&s, "a").new_delay_ms, 0);
        assert_eq!(proposed(&s, "b").new_delay_ms, 5);
        assert_eq!(proposed(&s, "e").new_delay_ms, 2);
        assert_eq!(proposed(&s, "c").new_delay_ms, 20);
        assert_eq!(proposed(&s, "d").new_delay_ms, 15);
        assert!((p.spread_ms - 20.0).abs() < 0.6, "the ratchet the renormalisation removed: {} ms", p.spread_ms);
    }

    /// Two overlaps that disagree by more than plausible geometry **refuse the step**
    /// (plan §1.1) — and the chain survives it, because the positions already aligned are
    /// still good and still carry their delays.
    #[tokio::test(start_paused = true)]
    async fn overlaps_that_disagree_refuse_the_step_and_leave_the_chain_alive() {
        let rig = chain_rig(&["a", "b", "c"]);
        let arrivals = rig.arrivals.clone();
        let m = manager();
        m.start(rig.deps).await.expect("started");
        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
        wait_chain(&m).await;

        // 'a' carries 5 ms, so the overlaps read 15 and 25 here: 10 ms apart, which is
        // more than one room's geometry explains.
        at_position(&m, &arrivals, &[("a", 10.0), ("b", 25.0), ("c", 8.0)], &["c"], &["a", "b"]).await;
        let s = wait_for(&m, "the step to be refused", |s| s.chain.as_ref().is_some_and(|c| c.refusal.is_some())).await;
        let chain = chain_of(&s);
        let r = chain.refusal.clone().expect("a step refusal");
        assert_eq!(r.kind, RefusalKind::OverlapDisagreement);
        // The refusal has to set the expectation, not just report a number: two overlaps
        // are *not* expected to agree exactly, and the tolerance is a plausibility check.
        assert!(r.message.contains("not* expected to read identically"), "{}", r.message);
        assert!(r.message.contains("a few ms is normal"), "{}", r.message);
        assert!(r.message.contains("every* speaker aligned so far"), "the stake has to be in the sentence: {}", r.message);
        assert_eq!(chain.steps.len(), 1, "the bad position was not recorded");
        assert!(!chain.aligned.contains(&"c".to_string()));
        assert_eq!(applied(&s, "a"), 5, "position 1 is untouched");
        assert_eq!(s.phase, Phase::Positioning, "the chain is still waiting, not refused");
        assert!(s.refusal.is_none(), "a step refusal is not the run's refusal");

        // Standing somewhere the two overlaps *are* plausible, the same position works.
        at_position(&m, &arrivals, &[("a", 15.0), ("b", 20.0), ("c", 8.0)], &["c"], &["a", "b"]).await;
        let s = wait_chain(&m).await;
        assert_eq!(chain_of(&s).steps.len(), 2, "{}", chain_of(&s).prompt);
        assert!(chain_of(&s).refusal.is_none(), "an accepted position clears the last refusal");
        assert_eq!(applied(&s, "c"), 12, "the overlaps read 20 ms here, so 'c' at 8 ms is delayed by 12");
    }

    /// One overlap is **possible** — a user may genuinely have only one shared speaker —
    /// and it is reported as what it is: a joint nothing checks, and a chain whose total
    /// error cannot be bounded (plan §1.1).
    #[tokio::test(start_paused = true)]
    async fn a_single_overlap_step_is_accepted_and_reported_as_reduced_confidence() {
        let rig = chain_rig(&["a", "b", "c"]);
        let arrivals = rig.arrivals.clone();
        let m = manager();
        m.start(rig.deps).await.expect("started");
        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
        wait_chain(&m).await;
        at_position(&m, &arrivals, &[("a", 20.0), ("c", 10.0)], &["c"], &["a"]).await;
        let s = wait_chain(&m).await;

        let chain = chain_of(&s);
        let two = &chain.steps[1];
        assert_eq!(two.confidence, OverlapConfidence::Single);
        assert_eq!(two.disagreement_ms, None, "one overlap has nothing to disagree with");
        assert_eq!(two.joint_error_ms, None, "and therefore no error estimate at all");
        assert!(two.note.contains("weakest"), "{}", two.note);
        assert_eq!(applied(&s, "c"), 15, "'a' reads 25 ms here with its 5 ms, so 'c' at 10 is delayed by 15");

        // The honest answer about the total is *no* total, not a partial one.
        assert!(!chain.error.bounded);
        assert_eq!(chain.error.joint_ms, None);
        assert!(chain.error.message.contains("cannot be bounded"), "{}", chain.error.message);
        assert!(chain.error.message.contains("single overlap"), "{}", chain.error.message);
        let warn = s.warnings.iter().find(|w| w.kind == WarningKind::OneOverlap).expect("the user has to be told which step was weaker");
        assert!(warn.message.contains("nothing to check it against"), "{}", warn.message);

        m.finish().expect("finish accepted");
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
        // …and it is still on the proposal, where the user decides whether to write.
        assert!(s.proposal.as_ref().expect("a proposal").warnings.iter().any(|w| w.kind == WarningKind::OneOverlap));
        assert!(!chain_of(&s).error.bounded);
    }

    /// Plan §1.1.1: the real knobs are written **once**, after the last position. A
    /// per-position write would spend the run's wall clock waiting for speakers to come
    /// back (§2.3: tens of seconds each).
    #[tokio::test(start_paused = true)]
    async fn a_multi_step_chain_writes_exactly_one_wave_and_drops_the_delay_lines() {
        let rig = chain_rig(&["a", "b", "c"]);
        let (relay, arrivals) = (rig.relay.clone(), rig.arrivals.clone());
        let m = manager();
        m.start(rig.deps).await.expect("started");
        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
        wait_chain(&m).await;
        at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("c", 10.0)], &["c"], &["a", "b"]).await;
        wait_chain(&m).await;
        m.finish().expect("finish accepted");
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "{}", s.message);
        assert!(relay.applied_ms("a") > 0.0, "the alignment is in the line until `apply`");

        // Post-write the whole set arrives together — the knobs now carry what the lines
        // were standing in for, so the fake capture is told to render them level.
        let after = chain_rig(&["a", "b", "c"]);
        *after.arrivals.lock_recover() = [("a", 0.0), ("b", 0.0), ("c", 0.0)].iter().map(|(n, v)| ((*n).to_string(), *v)).collect();
        let writer = after.writer.clone();
        m.apply(after.deps).await.expect("apply accepted");
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Done, "{}", s.message);

        // One wave: every knob written at most once, and only the ones that changed.
        let writes = writer.writes.lock_recover().clone();
        for (name, _) in &writes {
            assert_eq!(writer.count(name), 1, "'{name}' was written more than once: {writes:?}");
        }
        assert!(writes.len() <= 3, "a three-speaker chain cannot need more than three writes: {writes:?}");
        // 'c' needed the most provisional delay, so the renormalisation leaves it at
        // advance 0 — which is where it already was, and writing it would reconnect a
        // speaker for nothing (plan §2.3).
        assert_eq!(proposed(&s, "c").new_delay_ms, 0);
        assert_eq!(writer.count("c"), 0, "an unchanged knob is not written: {writes:?}");
        assert_eq!(writer.last("a"), Some(10));
        assert_eq!(writer.last("b"), Some(15));
        // And the provisional lines are gone, or every chained member would be delayed
        // twice (plan §1.1.1: the line is a stand-in for the knob, not an addition).
        assert_eq!(relay.applied_ms("a"), 0.0);
        assert!(chain_of(&s).provisional.is_empty());

        // The verification says what it covered: the *last* position only (§10.4's rule,
        // applied to a chain).
        let v = s.verification.clone().expect("a verification");
        assert!(v.passed, "residual {} ms", v.residual.worst_ms);
        let note = v.scope_note.clone().expect("a chain must not imply it re-checked every position");
        assert!(note.contains("last of 2 position(s)"), "{note}");
        assert!(note.contains("walking the chain again"), "{note}");
    }

    /// Plan §1.2: a *position* is one continuous capture. A reconnect inside one discards
    /// that position's readings — nothing is ever solved across the seam — while the
    /// positions already aligned survive, because what carries a chain from one position
    /// to the next is the delay each speaker holds, and the next position re-measures its
    /// overlaps in the new frame.
    #[tokio::test(start_paused = true)]
    async fn a_mic_reconnect_voids_the_position_in_flight_without_mixing_frames() {
        let rig = chain_rig(&["a", "b", "c"]);
        let (reconnect_at, mic, arrivals) = (rig.reconnect_at.clone(), rig.mic.clone(), rig.arrivals.clone());
        let m = manager();
        m.start(rig.deps).await.expect("started");
        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
        let s = wait_chain(&m).await;
        let before = s.observations.len();
        assert_eq!(before, 4, "two members, two passes");

        // 5 s from now: past the 3 s mute guard, so it lands inside the next reading —
        // the only place a reconnect is detectable.
        reconnect_at.store(mic.frames_now() + u64::from(mic.rate) * 5, Ordering::Relaxed);
        at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("c", 10.0)], &["c"], &["a", "b"]).await;

        let s = wait_for(&m, "the position to restart", |s| s.chain.as_ref().is_some_and(|c| c.restarts >= 1)).await;
        let warn = s.warnings.iter().find(|w| w.kind == WarningKind::MicReconnected).expect("the user has to be told");
        assert!(warn.message.contains("discarded"), "{}", warn.message);
        assert!(warn.message.contains("earlier positions are not affected"), "{}", warn.message);
        assert!(warn.message.contains("re-measures its overlaps"), "the reason they survive has to be in the sentence: {}", warn.message);

        let s = wait_chain(&m).await;
        let chain = chain_of(&s);
        assert_eq!(chain.steps.len(), 2, "the position was retaken, not lost: {}", chain.prompt);
        assert_eq!(applied(&s, "a"), 5, "position 1 came through untouched");
        assert_eq!(applied(&s, "c"), 15);
        // Nothing from the discarded attempt survived into the readings the step solved.
        assert_eq!(s.observations.len(), before + 6, "3 members × 2 passes for position 2, and nothing left over");
        // The two positions are in **different** frames, and that is fine precisely
        // because no arrival is ever compared across them.
        assert_eq!(chain.steps[0].grid_epoch, 0);
        assert_eq!(chain.steps[1].grid_epoch, 1, "the post-reconnect epoch");
        // Every reading a step solved shares that step's epoch — the seam the epoch check
        // in `arrivals_of` refuses is never reached.
        for step in &chain.steps {
            let names: Vec<&String> = step.members.iter().chain(step.overlaps.iter().map(|o| &o.node_name)).collect();
            let mine: Vec<&MemberObservation> =
                s.observations.iter().filter(|o| names.contains(&&o.node_name) && o.grid_epoch == step.grid_epoch).collect();
            assert_eq!(mine.len(), names.len() * MEASURE_PASSES, "step {} is missing readings in its own epoch", step.index);
        }
    }

    /// §2.4.2, through the chain: a sendspin-only run is aligned to the member that needed
    /// the **most** delay — its earliest arrival — which takes advance 0 while everyone
    /// else is advanced to meet it. The renormalisation is the interval solver, not a
    /// subtraction, and this is the case most likely to regress.
    #[test]
    fn a_sendspin_only_chain_aligns_to_its_earliest_member_after_renormalising() {
        let members = [member("a"), member("b"), member("c")];
        // A floor that has ratcheted a long way up, which is exactly what §1.1 warns
        // about: every step could only add.
        let provisional: HashMap<String, f64> =
            [("a".to_string(), 100.0), ("b".to_string(), 105.0), ("c".to_string(), 120.0)].into_iter().collect();
        let p = solve_chain(&ChainSolveInput {
            timing: Timing::real(),
            members: &members,
            provisional: &provisional,
            current_delays: &HashMap::new(),
            send_ahead: &SendAheadContext::default(),
            steps: &[],
            observations: &[],
        })
        .expect("accepted");
        let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
        // 'c' needed the most delay, i.e. it arrived earliest: advance 0, and the others
        // are advanced to meet it.
        assert_eq!(by("c").new_delay_ms, 0);
        assert_eq!(by("b").new_delay_ms, 15);
        assert_eq!(by("a").new_delay_ms, 20);
        assert_eq!(p.reference, "c");
        assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
        // The point of the renormalisation: nobody carries the 100 ms floor the chain
        // accumulated, and the relative geometry is untouched.
        assert_eq!(p.largest_knob_ms, 20);
        assert_eq!(p.members.iter().map(|m| m.new_delay_ms).min(), Some(0));
        assert!((p.spread_ms - 20.0).abs() < 1e-9, "the ratchet that was removed: {}", p.spread_ms);
        assert_eq!(by("a").new_delay_ms - by("b").new_delay_ms, 5, "'a' needed 5 ms less delay than 'b', so it ends 5 ms less advanced");
    }

    /// The renormalisation cannot be "subtract the minimum" once the polarities are mixed
    /// (plan §2.4.2), so it is the interval solver — and the solver's answer is a common
    /// shift chosen inside the intersection, which lands both knobs at the same value here.
    #[test]
    fn a_mixed_polarity_chain_is_renormalised_by_the_interval_solver() {
        let members = [member("spk"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
        let provisional: HashMap<String, f64> = [("spk".to_string(), 20.0), ("ap2-dev-x".to_string(), 0.0)].into_iter().collect();
        // The sendspin member needs headroom to be moved *later*: reducing an advance is
        // the only way to do it, so it has to have one to give up.
        let current: HashMap<String, u16> = [("spk".to_string(), 30u16)].into_iter().collect();
        let p = solve_chain(&ChainSolveInput {
            timing: Timing::real(),
            members: &members,
            provisional: &provisional,
            current_delays: &current,
            send_ahead: &SendAheadContext::default(),
            steps: &[],
            observations: &[],
        })
        .expect("accepted");
        let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
        assert_eq!(by("spk").new_delay_ms, 5, "the advance is reduced by 25, which delays it by 25");
        assert_eq!(by("ap2-dev-x").new_delay_ms, 5, "and the AP2 member is delayed by 5 — the difference is the 20 ms asked for");
        assert_eq!(p.largest_knob_ms, 5, "the target chosen is the one that keeps the largest knob smallest (§9.2)");

        // Without that headroom the chain is measurable but **not writable**, and the
        // refusal names both members rather than writing a best effort (§2.4.2).
        let r = solve_chain(&ChainSolveInput {
            timing: Timing::real(),
            members: &members,
            provisional: &provisional,
            current_delays: &HashMap::new(),
            send_ahead: &SendAheadContext::default(),
            steps: &[],
            observations: &[],
        })
        .expect_err("an advance-only member cannot be pushed later");
        assert_eq!(r.kind, RefusalKind::KnobRange);
        assert!(r.message.contains("spk") && r.message.contains("ap2-dev-x"), "{}", r.message);
    }

    /// §1.1's algebra on its own, with no capture, no relay and no runtime — including the
    /// two cases the orchestration cannot express as cleanly: three overlaps, and an
    /// aligned set that is already the latest thing at the new position.
    #[test]
    fn the_chain_step_algebra_takes_the_latest_arrival_as_the_target() {
        let aligned: HashMap<String, f64> = [("x".to_string(), 4.0), ("y".to_string(), 0.0), ("z".to_string(), 7.0)].into_iter().collect();
        // The aligned set reads 10 and 12 here (mean 11); the new member arrives at 30.
        let arrivals = [("new".to_string(), 30.0), ("x".to_string(), 10.0), ("y".to_string(), 12.0)];
        let overlaps = ["x".to_string(), "y".to_string()];
        let s = chain_step(&ChainStepInput { aligned: &aligned, arrivals: &arrivals, overlaps: &overlaps, tolerance_ms: 8.0 })
            .expect("2 ms apart is plausible");
        assert_eq!(s.target_ms, 30.0);
        assert_eq!(s.anchor_ms, Some(11.0));
        assert_eq!(s.delta_ms, 19.0);
        assert_eq!(s.provisional.get("new"), Some(&0.0), "the latest arrival needs no delay");
        assert_eq!(s.provisional.get("x"), Some(&23.0), "4 + Δ");
        assert_eq!(s.provisional.get("y"), Some(&19.0), "0 + Δ");
        assert_eq!(s.provisional.get("z"), Some(&26.0), "7 + Δ — never measured here, moved anyway");
        assert_eq!(s.disagreement_ms, Some(2.0));
        assert_eq!(s.joint_error_ms, Some(1.0), "the anchor is the mean, so it can be out by half the disagreement");
        assert_eq!(s.confidence, OverlapConfidence::Checked);

        // The other direction: the aligned set is already the latest thing here, so it
        // does not move at all and the new members are delayed to it.
        let arrivals = [("new".to_string(), 5.0), ("x".to_string(), 20.0), ("y".to_string(), 20.0)];
        let s = chain_step(&ChainStepInput { aligned: &aligned, arrivals: &arrivals, overlaps: &overlaps, tolerance_ms: 8.0 })
            .expect("accepted");
        assert_eq!(s.delta_ms, 0.0);
        assert_eq!(s.provisional.get("new"), Some(&15.0));
        assert_eq!(s.provisional.get("z"), None, "with no Δ the aligned set is not touched at all");

        // A later position with no overlap has nothing tying it to anything.
        let arrivals = [("new".to_string(), 5.0)];
        let r = chain_step(&ChainStepInput { aligned: &aligned, arrivals: &arrivals, overlaps: &[], tolerance_ms: 8.0 })
            .expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::OverlapMissing);
        assert!(r.message.contains("mutually meaningless"), "{}", r.message);
    }

    /// The chain's error statement, which is the whole honesty budget of the feature.
    #[test]
    fn the_accumulated_error_is_the_joints_and_is_withheld_when_a_joint_is_unmeasurable() {
        let step = |index: usize, confidence: OverlapConfidence, joint: Option<f64>| ChainStep {
            index,
            members: vec![format!("m{index}")],
            overlaps: match confidence {
                OverlapConfidence::Origin => Vec::new(),
                _ => vec![ChainOverlap { node_name: "ov".into(), arrival_ms: 0.0, applied_ms: 0.0 }],
            },
            confidence,
            disagreement_ms: joint.map(|j| j * 2.0),
            worst_pair: None,
            tolerance_ms: OVERLAP_AGREEMENT_TOL_MS,
            anchor_ms: None,
            delta_ms: 0.0,
            target_ms: 0.0,
            spread_ms: 0.0,
            drift_ppm: 0.0,
            joint_error_ms: joint,
            grid_epoch: 0,
            checks: Checks {
                transitivity: transitivity(&[], &Timing::real(), TRANSITIVITY_TOL_MS),
                repeatability: None,
                merged_peak: MergedPeakCheck::seam(),
                closure: None,
            },
            note: String::new(),
        };
        // One position is not a chain: no joints, so nothing accumulates.
        let one = chain_error(&[step(1, OverlapConfidence::Origin, None)]);
        assert!(one.bounded && one.joint_ms == Some(0.0));
        assert!(one.message.contains("no joints"), "{}", one.message);

        // Two checked joints compose additively — the shifts are applied one on top of
        // the other, so the worst case is the sum.
        let checked = chain_error(&[
            step(1, OverlapConfidence::Origin, None),
            step(2, OverlapConfidence::Checked, Some(1.5)),
            step(3, OverlapConfidence::Checked, Some(2.0)),
        ]);
        assert!(checked.bounded);
        assert_eq!(checked.joint_ms, Some(3.5));
        assert!(checked.message.contains("3.5 ms"), "{}", checked.message);
        // …and it still says what it is *not* measuring.
        assert!(checked.message.contains("§5.6"), "{}", checked.message);
        assert!(checked.message.contains("between* regions"), "{}", checked.message);

        // One unchecked joint anywhere and there is no total, rather than a total with a
        // hole in it.
        let single = chain_error(&[
            step(1, OverlapConfidence::Origin, None),
            step(2, OverlapConfidence::Checked, Some(1.5)),
            step(3, OverlapConfidence::Single, None),
        ]);
        assert!(!single.bounded);
        assert_eq!(single.joint_ms, None);
        assert!(single.message.contains("position 3"), "{}", single.message);
        assert!(single.message.contains("would be worse than none"), "{}", single.message);
    }

    /// The out-of-order cases, all of which are states the user can act on — so they are
    /// refusals with a sentence, never a 500 and never silence (plan §11).
    #[tokio::test(start_paused = true)]
    async fn the_chain_refuses_calls_that_do_not_match_where_it_is() {
        let m = manager();
        let r = m.position(vec!["a".into()], Vec::new()).expect_err("idle refuses");
        assert_eq!(r.kind, RefusalKind::ChainOutOfOrder);

        let rig = chain_rig(&["a", "b", "c"]);
        let (arrivals, relay, writer) = (rig.arrivals.clone(), rig.relay.clone(), rig.writer.clone());
        m.start(rig.deps).await.expect("started");
        wait_chain(&m).await;

        // A speaker the run is not holding.
        let r = m.position(vec!["ghost".into(), "a".into()], Vec::new()).expect_err("must refuse");
        assert_eq!(r.member.as_deref(), Some("ghost"));
        // An overlap at the first position, where nothing is aligned yet.
        let r = m.position(vec!["a".into(), "b".into()], vec!["c".into()]).expect_err("must refuse");
        assert!(r.message.contains("first position"), "{}", r.message);
        // One speaker on its own has nothing to be aligned *to*.
        let r = m.position(vec!["a".into()], Vec::new()).expect_err("must refuse");
        assert!(r.message.contains("at least two speakers"), "{}", r.message);
        // Finishing with speakers still unaligned.
        let r = m.finish().expect_err("must refuse");
        assert!(r.message.contains("not been aligned"), "{}", r.message);
        // A near-field call on a chained run points at the right endpoint.
        let r = m.arrival("a".into(), None).expect_err("must refuse");
        assert!(r.message.contains("/api/align/measure/position"), "{}", r.message);

        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
        // A second tap while the position is being measured.
        let r = m.position(vec!["c".into()], vec!["a".into()]).expect_err("must refuse");
        assert!(r.message.contains("busy"), "{}", r.message);
        wait_chain(&m).await;

        // Re-aligning a speaker that is already aligned, and using one that is not as an
        // overlap: the two halves of the same mistake, each named correctly.
        let r = m.position(vec!["a".into(), "c".into()], vec!["b".into()]).expect_err("must refuse");
        assert_eq!(r.member.as_deref(), Some("a"));
        assert!(r.message.contains("Offer it as an *overlap*"), "{}", r.message);
        let r = m.position(vec!["c".into()], vec!["c".into()]).expect_err("must refuse");
        assert!(r.message.contains("named twice"), "{}", r.message);

        // Abandoning mid-chain gives the provisional delays back and writes nothing.
        assert!(relay.applied_ms("a") > 0.0);
        let after = m.abandon();
        assert_eq!(after.phase, Phase::Idle);
        assert!(after.chain.is_none(), "abandoning clears the chain with the run");
        assert_eq!(relay.applied_ms("a"), 0.0, "a closed tab must not leave a delay line applied (plan §1.1.1)");
        assert!(after.message.contains("provisional"), "{}", after.message);
        assert!(writer.writes.lock_recover().is_empty());
    }

    /// A run that is not chained takes no positions, and says why rather than accepting
    /// one and doing nothing with it.
    #[tokio::test(start_paused = true)]
    async fn an_unchained_run_is_not_a_chain_and_says_so() {
        let rig = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::SweetSpot, 0.0);
        let m = manager();
        m.start(rig.deps).await.expect("started");
        let r = m.position(vec!["a".into(), "b".into()], Vec::new()).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::ChainOutOfOrder);
        assert!(r.message.contains("`chain: true`"), "{}", r.message);
        let s = wait_terminal(&m).await;
        assert_eq!(s.phase, Phase::Proposed, "the ordinary path is untouched: {}", s.message);
        assert!(s.chain.is_none(), "an unchained run has no chain state");

        // And a near-field walk is refused for its own reason: it needs no overlaps at all.
        let walk = Rig::new(&[("a", 0.0), ("b", 6.0)], Mode::NearField, 0.0);
        m.abandon();
        m.start(walk.deps).await.expect("started");
        let r = m.position(vec!["a".into(), "b".into()], Vec::new()).expect_err("must refuse");
        assert!(r.message.contains("one continuous capture"), "{}", r.message);
    }

    /// A chain over a pw-sink member: the mode admits every member kind, and the floor
    /// that cannot be written below is the solver's problem rather than the chain's
    /// (plan §1.1.2 item 4).
    #[tokio::test(start_paused = true)]
    async fn a_chain_can_include_a_pw_sink_member() {
        let rig = chain_rig_of(&[("a", MemberKind::Sendspin), ("b", MemberKind::Sendspin), ("pwsink-dev-host", MemberKind::PwSink)]);
        let arrivals = rig.arrivals.clone();
        let m = manager();
        m.start(rig.deps).await.expect("started");
        at_position(&m, &arrivals, &[("a", 0.0), ("b", 5.0)], &["a", "b"], &[]).await;
        wait_chain(&m).await;
        at_position(&m, &arrivals, &[("a", 20.0), ("b", 25.0), ("pwsink-dev-host", 10.0)], &["pwsink-dev-host"], &["a", "b"]).await;
        wait_chain(&m).await;
        m.finish().expect("finish accepted");
        let s = wait_terminal(&m).await;

        // The pw-sink member's playout delay cannot go below three packet times, and that
        // floor is what pins the whole group's target — a refusal or a shifted target, but
        // never a value nobody can write.
        match s.phase {
            Phase::Proposed => {
                let p = s.proposal.clone().expect("a proposal");
                let sink = proposed(&s, "pwsink-dev-host");
                assert_eq!(sink.polarity, KnobPolarity::Delay);
                assert!(sink.new_delay_ms >= crate::sync_settings::PWSINK_JITTER_MIN_MS, "{sink:?}");
                assert!(p.blocked.is_none(), "{:?}", p.blocked);
            }
            Phase::Refused => {
                let r = s.refusal.clone().expect("a refusal");
                assert_eq!(r.kind, RefusalKind::KnobRange, "{}", r.message);
            }
            other => panic!("unexpected phase {other:?}: {}", s.message),
        }
    }

    // ------------------------------------------------------- closure unit tests

    fn walk_obs(name: &str, centre: f64, phase_a: f64) -> MemberObservation {
        obs(name, 0, centre, phase_a, 0.0)
    }

    /// The tolerance is a **rate** bound, not a magnitude one, and that is the whole
    /// discrimination: a speaker that moved shows up as a large error over a short
    /// walk, while clock drift is bounded in ppm however long the walk was.
    #[test]
    fn the_closure_tolerance_is_a_drift_rate_and_not_a_fixed_number_of_milliseconds() {
        let t = Timing::real();
        // A 40 s walk: 200 ppm buys 8 ms, so 5 ms of creep is credible…
        let ok = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 20.0, 305.0), &t);
        assert!((ok.span_s - 40.0).abs() < 1e-9, "span {}", ok.span_s);
        assert!((ok.tolerance_ms - 8.0).abs() < 1e-9, "tolerance {}", ok.tolerance_ms);
        assert!((ok.drift_ppm - 125.0).abs() < 1e-6, "ppm {}", ok.drift_ppm);
        assert!(ok.passed);
        // …and 12 ms over the same 40 s is not, because no pair of clocks does 300 ppm.
        let bad = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 20.0, 312.0), &t);
        assert!(!bad.passed, "{bad:?}");
        assert!(bad.drift_ppm > MAX_CLOSURE_DRIFT_PPM);
        // The *same* 12 ms over a five-minute walk is credible, and the check says so
        // rather than pretending to a resolution it does not have.
        let slow = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 150.0, 312.0), &t);
        assert!(slow.passed, "{slow:?}");
        assert!(slow.tolerance_ms > 12.0);
        // Sign is kept: which way the clocks ran decides which way every member moves.
        let back = closure_report(&walk_obs("a", 0.0, 305.0), &walk_obs("a", 20.0, 300.0), &t);
        assert!((back.error_ms + 5.0).abs() < 1e-9, "error {}", back.error_ms);
        assert!(back.passed && back.drift_ppm < 0.0);
        // A short walk still gets the floor rather than a sub-millisecond verdict the
        // measurement cannot support.
        let quick = closure_report(&walk_obs("a", 0.0, 300.0), &walk_obs("a", 2.0, 302.0), &t);
        assert!((quick.tolerance_ms - MIN_CLOSURE_TOL_MS).abs() < 1e-9, "tolerance {}", quick.tolerance_ms);
        assert!(quick.passed);
        // Two readings with no time between them have no baseline at all.
        let flat = closure_report(&walk_obs("a", 5.0, 300.0), &walk_obs("a", 5.0, 300.0), &t);
        assert!(!flat.passed, "a closure with no span cannot establish anything");
        assert!(flat.drift_ppm.is_infinite());
    }

    /// The claim [`ClosureReport`] rests on: feeding the closure observation to
    /// [`fit_drift`] *is* the drift fit — the pooled slope reduces to
    /// `error / span`, and each member's correction is proportional to when in the
    /// walk it was measured.
    #[test]
    fn the_closure_observation_is_what_makes_the_drift_fit_possible() {
        // A walk: a at period 0, b at 5, c at 10, then back to a at 15. The mic clock
        // runs 0.4 ms fast per period, so every reading creeps.
        let slope = 0.4;
        let o = [
            walk_obs("a", 0.0, 300.0),
            walk_obs("b", 5.0, 306.0 + 5.0 * slope),
            walk_obs("c", 10.0, 311.0 + 10.0 * slope),
            walk_obs("a", 15.0, 300.0 + 15.0 * slope),
        ];
        // One pass with no closure: nothing has two readings, so there is no slope and
        // the creep would be written into the speakers (plan §5.3).
        let unclosed = fit_drift(&o[..3], 2000.0, |x| x.m.phase_a_ms);
        assert!(!unclosed.fitted, "a single pass has no time baseline");

        let fit = fit_drift(&o, 2000.0, |x| x.m.phase_a_ms);
        assert!(fit.fitted, "the closure is the second reading the fit needs");
        assert!((fit.slope_ms_per_period - slope).abs() < 1e-9, "slope {}", fit.slope_ms_per_period);
        // …and the offsets are the true wire delays, creep removed.
        let base = fit.offsets["a"];
        assert!((fit.offsets["b"] - base - 6.0).abs() < 1e-9, "b {}", fit.offsets["b"] - base);
        assert!((fit.offsets["c"] - base - 11.0).abs() < 1e-9, "c {}", fit.offsets["c"] - base);
        // The closure error and the slope are two views of one number.
        let c = closure_report(&o[0], &o[3], &Timing::real());
        assert!((c.error_ms - slope * 15.0).abs() < 1e-9, "error {}", c.error_ms);
        assert!((fit.slope_ms_per_period - c.error_ms / c.span_periods).abs() < 1e-12);
    }

    /// Near-field arrivals go through the same §2.4.2 solver as everything else, which
    /// is the point — near field changes how arrivals are *acquired*, not what is done
    /// with them.
    #[test]
    fn a_walks_arrivals_feed_the_interval_solver_unchanged() {
        let members = [member("a"), member("b"), member("c")];
        // Walk order a, b, c, closing back on a. No drift.
        let o = [walk_obs("a", 0.0, 300.0), walk_obs("b", 5.0, 312.0), walk_obs("c", 10.0, 305.0), walk_obs("a", 15.0, 300.0)];
        let current: HashMap<String, u16> = HashMap::new();
        let ctx = SendAheadContext::default();
        let closure = closure_report(&o[0], &o[3], &Timing::real());
        assert!(closure.passed);
        let p = solve(&SolveInput {
            timing: Timing::real(),
            members: &members,
            observations: &o,
            current_delays: &current,
            send_ahead: &ctx,
            closure: Some(closure),
        })
        .expect("accepted");
        // A sendspin-only group aligns to its EARLIEST member (§2.4.1's inversion),
        // whatever order the user walked in.
        assert_eq!(p.reference, "a");
        let by = |n: &str| p.members.iter().find(|m| m.node_name == n).unwrap().clone();
        assert_eq!((by("a").new_delay_ms, by("b").new_delay_ms, by("c").new_delay_ms), (0, 12, 5));
        assert!(p.members.iter().all(|m| m.polarity == KnobPolarity::Advance));
        assert!(p.blocked.is_none());
        assert!(p.checks.closure.is_some_and(|c| c.passed));
        assert!(p.checks.repeatability.is_none());

        // A mixed group is still refused when the two ranges cannot meet — near field
        // does not change the knobs, only where they were measured from.
        let mixed = [member("s"), SessionMember { node_name: "ap2-dev-x".into(), kind: MemberKind::Airplay2 }];
        let o = [walk_obs("s", 0.0, 300.0), walk_obs("ap2-dev-x", 5.0, 320.0), walk_obs("s", 10.0, 300.0)];
        let closure = closure_report(&o[0], &o[2], &Timing::real());
        let r = solve(&SolveInput {
            timing: Timing::real(),
            members: &mixed,
            observations: &o,
            current_delays: &current,
            send_ahead: &ctx,
            closure: Some(closure),
        })
        .expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::KnobRange);
    }

    // ------------------------------- relay ≡ device equivalence (W21), end to end

    /// The provisional delay line as the experiment drives it: a **real**
    /// [`crate::align::relay_delay::RelayDelay`] for the value arithmetic, the cap and — the
    /// part that matters here — the real priming state, plus a stand-in for the relay
    /// thread, because a line only fills from audio that actually flows and there is no
    /// PipeWire graph in a unit test.
    ///
    /// The pushed audio is silence: this fake's job is the line's *state*, and the
    /// arrival shift a delay produces is injected separately ([`EquivPhysics`]) so a
    /// test can inject one that disagrees with it.
    struct FakeRelay {
        rd: crate::align::relay_delay::RelayDelay,
        /// Frames the stand-in relay thread pushes per status poll. 480 (10 ms) is
        /// deliberately less than the 20 ms step, so priming takes more than one poll and
        /// the wait is a tested path rather than a no-op.
        frames_per_poll: usize,
        buf: Mutex<Vec<u8>>,
    }

    impl FakeRelay {
        fn new() -> Self {
            Self { rd: crate::align::relay_delay::RelayDelay::new(), frames_per_poll: 480, buf: Mutex::new(Vec::new()) }
        }

        /// What the line is applying right now, in ms — read *without* pumping, because
        /// the arrival physics reads it once per capture window.
        fn applied_ms(&self, output: &str) -> f64 {
            self.rd.status(output).map(|s| crate::align::relay_delay::us_for_frames(s.delay_frames, s.rate) as f64 / 1000.0).unwrap_or(0.0)
        }

        fn pump(&self, output: &str) {
            let mut buf = self.buf.lock_recover();
            let src = vec![0u8; self.frames_per_poll * 4];
            let _ = self.rd.delay_into(output, crate::align::relay_delay::PcmFormat::new(48_000, 2), &src, &mut buf);
        }
    }

    impl RelayControl for FakeRelay {
        fn set_delay_ms(&self, output: &str, delay_ms: u16) -> Result<(), String> {
            self.rd.set_delay_us(output, u64::from(delay_ms) * 1_000).map(|_| ()).map_err(|e| e.to_string())
        }

        fn status(&self, output: &str) -> Option<crate::align::relay_delay::DelayStatus> {
            self.pump(output); // the relay thread ran between two polls
            self.rd.status(output)
        }

        fn clear(&self, output: &str) {
            self.rd.clear(output);
        }
    }

    /// The physics the experiment exists to discover, injected: how far a provisional
    /// delay moves this speaker, how far and **which way** its knob does, and what a
    /// reconnect does on its own.
    struct EquivPhysics {
        member: String,
        relay: Arc<FakeRelay>,
        writer: Arc<FakeWriter>,
        baseline_knob: u16,
        inject: EquivInject,
    }

    /// Everything the W21 tests vary. The defaults are a speaker that behaves exactly as
    /// plan §2.4.1 and §1.1.1 say it should.
    #[derive(Debug, Clone, Copy)]
    struct EquivInject {
        /// ms of arrival shift per ms of provisional delay. `1.0` = the delay line does
        /// what it says.
        relay_per_ms: f64,
        /// ms of arrival shift per ms the knob is **raised**. `-1.0` is §2.4.1's advance,
        /// `+1.0` a delay, `-1.15` a 15 % scale error, `0.0` a knob nobody honours.
        device_per_ms: f64,
        /// A constant this speaker's arrival gains the moment its knob is written at all
        /// — the ε of §1.1.2 item 3, and the constant a two-point device arm must cancel.
        reconnect_eps_ms: f64,
        /// Mic-vs-audio clock drift, ms of phase per second (0.1 = 100 ppm, §5.4.1's
        /// realistic phone).
        drift_ms_per_s: f64,
        /// The speaker goes silent once this many writes have landed (plan §2.3.2's
        /// wedged member).
        silent_after_writes: Option<usize>,
        /// Shorten **only** the gate timeouts. Every other quantity stays production —
        /// see the test that uses it for why.
        short_timeouts: bool,
    }

    impl Default for EquivInject {
        fn default() -> Self {
            Self {
                relay_per_ms: 1.0,
                device_per_ms: -1.0,
                reconnect_eps_ms: 0.0,
                drift_ms_per_s: 0.0,
                silent_after_writes: None,
                short_timeouts: false,
            }
        }
    }

    impl ArrivalShift for EquivPhysics {
        fn shift_ms(&self, member: &str) -> f64 {
            if member != self.member {
                return 0.0;
            }
            let knob = self.writer.last(member).unwrap_or(self.baseline_knob);
            let raised = f64::from(knob) - f64::from(self.baseline_knob);
            let reconnected = self.writer.count(member) > 0;
            self.relay.applied_ms(member) * self.inject.relay_per_ms
                + raised * self.inject.device_per_ms
                + if reconnected { self.inject.reconnect_eps_ms } else { 0.0 }
        }

        fn silent(&self, member: &str) -> bool {
            member == self.member && self.inject.silent_after_writes.is_some_and(|n| self.writer.count(member) >= n)
        }
    }

    struct EquivRig {
        deps: EquivalenceDeps,
        relay: Arc<FakeRelay>,
        writer: Arc<FakeWriter>,
    }

    /// Assemble the experiment over one group. `members` carries each member's kind and
    /// its **current** knob value; `target` is the speaker whose physics is injected —
    /// which is the one the planner is expected to pick.
    fn equiv_rig(members: &[(&str, MemberKind, u16)], target: &str, inject: EquivInject) -> EquivRig {
        let timing = match inject.short_timeouts {
            true => {
                Timing { gate_settle_timeout: Duration::from_secs(20), gate_reconnect_timeout: Duration::from_secs(30), ..Timing::real() }
            }
            false => Timing::real(),
        };
        let soloed = Arc::new(Mutex::new(None));
        let session_members: Vec<SessionMember> =
            members.iter().map(|(n, k, _)| SessionMember { node_name: (*n).to_string(), kind: *k }).collect();
        // Deliberately not zero: an advance of 20 ms on a speaker arriving at 0 ms would
        // wrap the pattern, and this experiment is about the *shift*, not the arrival.
        let arrivals: Arc<Mutex<HashMap<String, f64>>> =
            Arc::new(Mutex::new(members.iter().enumerate().map(|(i, (n, _, _))| ((*n).to_string(), 30.0 + i as f64 * 4.0)).collect()));
        let current_delays: HashMap<String, u16> = members.iter().map(|(n, _, ms)| ((*n).to_string(), *ms)).collect();
        let session = Arc::new(FakeSession {
            members: session_members,
            soloed: soloed.clone(),
            active: Arc::new(AtomicBool::new(true)),
            interference: Arc::new(Mutex::new(Vec::new())),
            levels: Arc::new(Mutex::new(HashMap::new())),
        });
        let relay = Arc::new(FakeRelay::new());
        let writer = Arc::new(FakeWriter::default());
        let physics = Arc::new(EquivPhysics {
            member: target.to_string(),
            relay: relay.clone(),
            writer: writer.clone(),
            baseline_knob: current_delays.get(target).copied().unwrap_or(0),
            inject,
        });
        let mic = Arc::new(FakeMic {
            rate: 48_000,
            pattern_ms: timing.pattern_ms,
            soloed,
            arrivals,
            start: Instant::now(),
            frames: AtomicU64::new(0),
            connected: Arc::new(AtomicBool::new(true)),
            drift_ms_per_s: inject.drift_ms_per_s,
            reconnect_at: Arc::new(AtomicU64::new(0)),
            shift: physics,
        });
        let deps = EquivalenceDeps {
            base: MeasureDeps {
                mode: Mode::SweetSpot,
                chained: false,
                link_to: Vec::new(),
                session,
                mic,
                writer: writer.clone(),
                relay: relay.clone(),
                current_delays,
                send_ahead: SendAheadContext::default(),
                timing,
            },
            member: None,
        };
        EquivRig { deps, relay, writer }
    }

    /// Run the whole experiment, restore included, and hand back its final status.
    async fn run_equiv(rig: EquivRig) -> (EquivalenceStatus, Arc<FakeWriter>, Arc<FakeRelay>) {
        let EquivRig { deps, relay, writer } = rig;
        let st = EquivState::new();
        let cancel = Arc::new(AtomicBool::new(false));
        drive_equivalence(deps, st.clone(), cancel).await;
        (st.status(), writer, relay)
    }

    fn report_of(s: &EquivalenceStatus) -> EquivalenceReport {
        s.report.clone().unwrap_or_else(|| panic!("a report; phase {:?}, message {}", s.phase, s.message))
    }

    /// The headline case: both arms measured on one speaker, and the answer is a number
    /// with an uncertainty rather than a boolean.
    #[tokio::test(start_paused = true)]
    async fn both_arms_are_measured_and_the_result_is_a_number_with_a_bound() {
        let rig = equiv_rig(&[("ap2-dev-other", MemberKind::Airplay2, 0), ("spk", MemberKind::Sendspin, 0)], "spk", EquivInject::default());
        let (s, writer, relay) = run_equiv(rig).await;
        assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
        assert_eq!(s.steps_done, EQUIV_STEPS);
        let r = report_of(&s);

        // The relay arm: a 20 ms line moved it 20 ms later, and the line reports the
        // sample-exact figure rather than what was asked for.
        assert_eq!(r.relay.applied_ms, 20.0);
        assert!((r.relay.shift_ms - 20.0).abs() < 0.2, "relay arm: {} ms", r.relay.shift_ms);
        assert!(r.relay.writes.is_empty(), "the relay arm must cost no reconnect at all");

        // The device arm: a 20 ms advance moved it 20 ms EARLIER (§2.4.1), which is a
        // negative shift and a positive equivalent delay.
        assert!((r.device.shift_ms + 20.0).abs() < 0.2, "device arm: {} ms", r.device.shift_ms);
        assert!((r.device_equivalent_delay_ms - 20.0).abs() < 0.2);
        assert_eq!(r.polarity_assumed, KnobPolarity::Advance);
        assert_eq!(r.polarity_observed, Some(KnobPolarity::Advance), "the firmware's sign is confirmed, not assumed");

        // …and the comparison is reported as a bounded claim.
        assert_eq!(r.verdict, EquivalenceVerdict::WithinResolution);
        assert!(r.discrepancy_ms.abs() < 0.5, "{:+} ms", r.discrepancy_ms);
        assert!(r.uncertainty_ms > 0.0 && r.uncertainty_ms < 0.5, "1σ = {} ms", r.uncertainty_ms);
        assert!(r.resolution_ms >= EQUIV_MIN_MEANINGFUL_MS, "the claim is never finer than the knob's own granularity");
        assert!(r.headline.contains("no difference beyond"), "{}", r.headline);
        assert!(r.headline.contains("one speaker of one transport"), "the scope travels with the claim: {}", r.headline);
        assert!((r.scale.expect("a scale") - 1.0).abs() < 0.05, "scale {:?}", r.scale);

        // Three writes: from → to → from. The last one leaves the knob where the user
        // had it, so the happy path needs no restoring write.
        assert_eq!(writer.writes.lock_recover().clone(), vec![("spk".into(), 0), ("spk".into(), 20), ("spk".into(), 0)]);
        assert_eq!(r.reconnects, 3);
        assert_eq!(r.plan.member, "spk", "the sendspin member is chosen, and only one member is used");
        assert!(r.plan.why_member.contains("only *advance*"), "{}", r.plan.why_member);
        assert!(r.plan.why_step.contains("one wire-codec frame"), "{}", r.plan.why_step);

        // State restored: no line left applied, knob back where it started.
        let restore = s.restore.expect("a restore report");
        assert!(restore.relay_cleared && restore.failures.is_empty(), "{restore:?}");
        assert!(!restore.knob_rewritten, "the step's own last write already put it back");
        assert_eq!(restore.knob_left_at_ms, Some(0));
        assert_eq!(relay.applied_ms("spk"), 0.0, "the provisional delay is not the user's and must not outlive the run");

        // What it cannot tell you travels with the numbers.
        assert_eq!(r.cannot_tell.len(), 6);
        assert!(r.cannot_tell.iter().any(|c| c.contains("ONE speaker of ONE transport")));
        assert!(r.cannot_tell.iter().any(|c| c.contains("cannot see a *constant* difference")));
        // A clean run has nothing to caveat: the line did what it said, and every write
        // reported that it was reconnecting the speaker.
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert!(r.device.writes.len() == 3 && r.device.writes.iter().all(|m| m.contains("reconnecting")), "{:?}", r.device.writes);
    }

    /// The interesting failure: the knob moves the speaker by a *different amount* than
    /// the delay line. It must come out as both numbers and a factor — and nothing may
    /// quietly divide by it.
    #[tokio::test(start_paused = true)]
    async fn a_scale_disagreement_is_reported_with_both_numbers_and_never_applied() {
        let inject = EquivInject { device_per_ms: -1.15, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
        let (s, writer, _relay) = run_equiv(rig).await;
        assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
        let r = report_of(&s);
        assert_eq!(r.verdict, EquivalenceVerdict::ScaleDisagrees);
        assert!((r.relay.shift_ms - 20.0).abs() < 0.2, "the relay arm is the reference: {} ms", r.relay.shift_ms);
        assert!((r.device_equivalent_delay_ms - 23.0).abs() < 0.3, "device arm: {} ms", r.device_equivalent_delay_ms);
        assert!((r.discrepancy_ms - 3.0).abs() < 0.3, "{:+} ms", r.discrepancy_ms);
        assert!(r.discrepancy_ms.abs() > r.resolution_ms, "3 ms must clear the resolution, or this test proves nothing");
        assert!((r.scale.expect("a scale") - 1.15).abs() < 0.02, "scale {:?}", r.scale);
        // Both numbers, in the sentence a user reads.
        assert!(r.headline.contains("NOT interchangeable"), "{}", r.headline);
        assert!(r.headline.contains("20.0") && r.headline.contains("23."), "{}", r.headline);
        // The correction is stated and disowned in the same breath.
        assert!(r.implied_correction.contains("NOT applied"), "{}", r.implied_correction);
        assert!(r.implied_correction.contains("0.870"), "the factor is spelled out: {}", r.implied_correction);
        // Nothing was written except the experiment's own three steps: a discrepancy is a
        // finding, not a repair.
        assert_eq!(writer.writes.lock_recover().len(), 3);
        assert_eq!(s.restore.expect("a restore").knob_left_at_ms, Some(0));
    }

    /// The finding §2.4.1 says would be far more serious than an offset: the firmware
    /// disagreeing with the polarity the solver assumes.
    #[tokio::test(start_paused = true)]
    async fn a_knob_that_moves_the_sound_the_wrong_way_is_called_a_sign_inversion() {
        // +1.0: raising the knob makes the speaker play LATER, i.e. it is a delay, not
        // the advance the solver models.
        let inject = EquivInject { device_per_ms: 1.0, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
        let (s, _writer, _relay) = run_equiv(rig).await;
        assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
        let r = report_of(&s);
        assert_eq!(r.verdict, EquivalenceVerdict::SignInverted);
        assert_eq!(r.polarity_assumed, KnobPolarity::Advance);
        assert_eq!(r.polarity_observed, Some(KnobPolarity::Delay), "the measurement, not the assumption, decides");
        assert!((r.device.shift_ms - 20.0).abs() < 0.2, "{} ms", r.device.shift_ms);
        // Expressed as a delay-equivalent it is *negative*, which is what makes every
        // proposal for this kind inverted rather than merely offset.
        assert!((r.device_equivalent_delay_ms + 20.0).abs() < 0.2);
        assert!((r.discrepancy_ms + 40.0).abs() < 0.5, "{:+} ms", r.discrepancy_ms);
        assert!(r.headline.contains("WRONG WAY") && r.headline.contains("inverted"), "{}", r.headline);
        assert!(r.headline.contains("do not write"), "{}", r.headline);
        assert!(r.implied_correction.contains("knob_of"), "the fix is a code change, not a factor: {}", r.implied_correction);
        // The relay arm is still reported, so the reader can see which half moved.
        assert!((r.relay.shift_ms - 20.0).abs() < 0.2);
    }

    /// Plan §1.1.2 item 3, tested: a reconnect shifts this speaker by a constant that a
    /// one-reconnect comparison would have charged to the knob. The two-point device arm
    /// cancels it — and therefore cannot measure a constant at all, which the report
    /// says out loud.
    #[tokio::test(start_paused = true)]
    async fn a_constant_reconnect_offset_cancels_and_is_reported_as_epsilon() {
        let inject = EquivInject { reconnect_eps_ms: 5.0, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
        let (s, _writer, _relay) = run_equiv(rig).await;
        assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
        let r = report_of(&s);

        // The naive experiment §1.1.1 budgeted — "relay N, no reconnect" against
        // "device N, after one reconnect" — would have been 5 ms out, and here is the
        // evidence: the same speaker at the same knob value, either side of one write.
        assert!(
            (r.device.baseline_before_ms - r.relay.baseline_after_ms).abs() > 4.0,
            "the reconnect really did move it: {} → {}",
            r.relay.baseline_after_ms,
            r.device.baseline_before_ms
        );
        assert!((r.reconnect_epsilon_ms - 5.0).abs() < 0.3, "ε = {} ms", r.reconnect_epsilon_ms);

        // …and the bracketed difference is unmoved by it.
        assert_eq!(r.verdict, EquivalenceVerdict::WithinResolution);
        assert!(r.discrepancy_ms.abs() < 0.5, "{:+} ms", r.discrepancy_ms);
        assert!(r.reconnect_variation_ms < 0.5, "two identical reconnects landed the same: {} ms", r.reconnect_variation_ms);
        assert!(
            r.cannot_tell.iter().any(|c| c.contains("cancels any constant")),
            "the price of item 3 is that a constant is invisible, and that has to be stated"
        );
    }

    /// The correction §1.1.2 item 3 did *not* budget for: two reconnects are tens of
    /// seconds apart, and a 100 ppm phone clock creeps millimetres of a millisecond per
    /// second — 6 ms over a device arm, against a 20 ms step. Bracketing each arm
    /// removes it; without the brackets this measurement would report a 30 % scale error
    /// that does not exist.
    #[tokio::test(start_paused = true)]
    async fn clock_drift_across_the_reconnects_is_bracketed_out() {
        let inject = EquivInject { drift_ms_per_s: 0.1, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
        let (s, _writer, _relay) = run_equiv(rig).await;
        assert_eq!(s.phase, EquivPhase::Done, "{}", s.message);
        let r = report_of(&s);

        // The drift is real and large enough to matter: the device arm's two identical
        // readings are milliseconds apart, and it is measured as ~100 ppm on the arm that
        // has no reconnects in it.
        assert!(r.relay.span_s > 15.0, "the relay bracket has to span something: {} s", r.relay.span_s);
        assert!((r.relay.drift_ppm - 100.0).abs() < 20.0, "{} ppm", r.relay.drift_ppm);
        assert!(
            r.device.baseline_disagreement_ms.abs() > 2.0,
            "the device arm's baselines must disagree, or the bracket is not being tested: {} ms",
            r.device.baseline_disagreement_ms
        );
        // What the drift is *not* is a scale error, and the brackets are why.
        assert_eq!(r.verdict, EquivalenceVerdict::WithinResolution, "{}", r.headline);
        assert!(r.discrepancy_ms.abs() < 0.6, "{:+} ms", r.discrepancy_ms);
        assert!(r.reconnect_variation_ms < 1.0, "drift explained the disagreement: {} ms", r.reconnect_variation_ms);
    }

    /// A knob nothing honours is its own finding, and it is not "equivalent".
    #[tokio::test(start_paused = true)]
    async fn a_knob_the_device_ignores_is_named_rather_than_averaged_away() {
        let inject = EquivInject { device_per_ms: 0.0, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
        let (s, _writer, _relay) = run_equiv(rig).await;
        let r = report_of(&s);
        assert_eq!(r.verdict, EquivalenceVerdict::KnobHadNoEffect);
        assert_eq!(r.polarity_observed, None, "no shift means no direction — that is not a sign, it is an absence");
        assert!(r.headline.contains("did not act on it"), "{}", r.headline);
        assert!(r.implied_correction.contains("no factor turns zero into a delay"), "{}", r.implied_correction);
        // The relay arm still worked, which is what makes this a device finding.
        assert!((r.relay.shift_ms - 20.0).abs() < 0.2);
    }

    /// The other half's failure, and it must not be charged to the device: if the delay
    /// line produces no shift, the *provisional* half of the deferred-write scheme is
    /// broken and the knob is beside the point.
    #[tokio::test(start_paused = true)]
    async fn a_delay_line_that_does_nothing_is_blamed_on_the_line_not_the_knob() {
        let inject = EquivInject { relay_per_ms: 0.0, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
        let (s, _writer, _relay) = run_equiv(rig).await;
        let r = report_of(&s);
        assert_eq!(r.verdict, EquivalenceVerdict::RelayLineHadNoEffect);
        assert!(r.relay.shift_ms.abs() < 0.5, "{} ms", r.relay.shift_ms);
        // The knob was fine, and is still reported — but no factor is offered, because
        // there is nothing to divide by.
        assert!((r.device_equivalent_delay_ms - 20.0).abs() < 0.3);
        assert_eq!(r.scale, None, "a NaN here would break the status serialisation, never mind the arithmetic");
        assert!(r.headline.contains("*delay line* is what failed"), "{}", r.headline);
        assert!(r.implied_correction.contains("fix the delay line"), "{}", r.implied_correction);
        // And the whole status still serialises, which is the reason `scale` is optional.
        serde_json::to_string(&s).expect("the status must serialise whatever the verdict");
    }

    /// A gate that never locks must refuse, not guess. Here the speaker is wedged from
    /// the start (plan §2.3.2), so the very first reading fails — before a single
    /// reconnect has been spent, which is why the relay arm goes first.
    ///
    /// Only the gate *timeouts* are shortened: the test has to synthesise every sample
    /// of the wait, and 45 s of silence at 48 kHz proves nothing that 20 s does not.
    #[tokio::test(start_paused = true)]
    async fn a_gate_that_never_locks_refuses_before_a_reconnect_is_spent() {
        let inject = EquivInject { silent_after_writes: Some(0), short_timeouts: true, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 0)], "spk", inject);
        let (s, writer, relay) = run_equiv(rig).await;
        assert_eq!(s.phase, EquivPhase::Refused);
        let refusal = s.refusal.expect("a refusal");
        assert_eq!(refusal.kind, RefusalKind::GateTimeout);
        assert_eq!(refusal.member.as_deref(), Some("spk"));
        assert!(refusal.message.contains("no tone from this speaker"), "{}", refusal.message);
        assert!(s.report.is_none(), "a refused experiment must not report a verdict");
        assert!(writer.writes.lock_recover().is_empty(), "the relay arm is first precisely so a failure costs no reconnect");
        assert_eq!(relay.applied_ms("spk"), 0.0);
        let restore = s.restore.expect("a restore report even on refusal");
        assert!(restore.relay_cleared && restore.failures.is_empty());
        assert_eq!(restore.knob_left_at_ms, None, "no knob was ever written");
    }

    /// The same, but after the device arm has already written the step: the knob is at a
    /// value the user did not choose, so the refusal has to pay for one more reconnect
    /// to put it back.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_after_the_step_was_written_puts_the_knob_back() {
        // Silent once two writes have landed, i.e. exactly when the stepped value is on
        // the device.
        let inject = EquivInject { silent_after_writes: Some(2), short_timeouts: true, ..EquivInject::default() };
        let rig = equiv_rig(&[("spk", MemberKind::Sendspin, 7)], "spk", inject);
        let (s, writer, relay) = run_equiv(rig).await;
        assert_eq!(s.phase, EquivPhase::Refused);
        assert_eq!(s.refusal.expect("a refusal").kind, RefusalKind::GateTimeout);
        assert!(s.report.is_none());
        let restore = s.restore.expect("a restore report");
        assert!(restore.knob_rewritten, "the run stopped with the step applied, so it owes a write back");
        assert_eq!(restore.knob_left_at_ms, Some(7), "back to the value the user had, not to zero");
        assert!(restore.failures.is_empty(), "{restore:?}");
        assert!(restore.message.contains("one more reconnect"), "the extra cost is stated: {}", restore.message);
        // 7 → 27 → (refused) → 7.
        assert_eq!(writer.writes.lock_recover().clone(), vec![("spk".into(), 7), ("spk".into(), 27), ("spk".into(), 7)]);
        assert_eq!(relay.applied_ms("spk"), 0.0);
    }

    /// Abandoning is not a licence to leave a speaker 20 ms out: the run still finishes
    /// its restore, and the status says where the knob ended up.
    #[tokio::test(start_paused = true)]
    async fn abandoning_still_puts_the_borrowed_delay_back() {
        let rig =
            equiv_rig(&[("ap2-dev-other", MemberKind::Airplay2, 0), ("spk", MemberKind::Sendspin, 12)], "spk", EquivInject::default());
        let EquivRig { deps, relay, writer } = rig;
        let m = EquivalenceManager { st: EquivState::new() };
        m.start(deps).await.expect("the experiment must start");

        // Wait until the stepped value is actually on the device, so what is being tested
        // is a cancellation that owes a write back.
        for _ in 0..40_000u32 {
            if writer.last("spk") == Some(32) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(writer.last("spk"), Some(32), "the run never reached the stepped write");
        let cancelling = m.abandon();
        assert!(cancelling.message.contains("putting the borrowed delay back"), "{}", cancelling.message);

        let s = loop {
            let s = m.status();
            if s.phase == EquivPhase::Refused && s.restore.is_some() {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(s.refusal.expect("a refusal").kind, RefusalKind::Cancelled);
        assert!(s.report.is_none(), "an abandoned experiment has no verdict");
        let restore = s.restore.expect("a restore report");
        assert!(restore.knob_rewritten && restore.failures.is_empty(), "{restore:?}");
        assert_eq!(restore.knob_left_at_ms, Some(12));
        assert_eq!(writer.last("spk"), Some(12), "the user's own value is what the speaker is left at");
        assert!(restore.relay_cleared);
        assert_eq!(relay.applied_ms("spk"), 0.0);
        assert!(s.message.contains("abandoned;"), "{}", s.message);
    }

    // ------------------------------------------- the plan (member and step choice)

    fn equiv_members(kinds: &[(&str, MemberKind)]) -> Vec<SessionMember> {
        kinds.iter().map(|(n, k)| SessionMember { node_name: (*n).to_string(), kind: *k }).collect()
    }

    /// The step is one wire-codec frame, and that is what set it — asserted against the
    /// codec rather than restated, so a codec change cannot quietly reintroduce
    /// §1.1.2 item 2's window-phase confound.
    #[test]
    fn the_step_is_exactly_one_wire_codec_frame() {
        assert_eq!(EQUIV_STEP_MS, 20);
        assert_eq!(usize::from(EQUIV_STEP_MS) * 48, crate::outputs::sendspin::codec::OPUS_FRAME_FRAMES);
        // And it dwarfs the estimator by the margins §5.4.1 measured.
        assert!(f64::from(EQUIV_STEP_MS) > 100.0 * 0.14, "100× the worst accepted delta error");
        assert!(f64::from(EQUIV_STEP_MS) > 10.0 * REPEATABILITY_TOL_MS);
    }

    #[test]
    fn the_member_is_chosen_by_transport_and_the_choice_is_explained() {
        let members =
            equiv_members(&[("ap2-dev-x", MemberKind::Airplay2), ("pwsink-dev-y", MemberKind::PwSink), ("spk", MemberKind::Sendspin)]);
        let current: HashMap<String, u16> = HashMap::new();
        let ctx = SendAheadContext::default();
        let p = plan_equivalence(&members, &current, &ctx, None).expect("a plan");
        assert_eq!(p.member, "spk", "sendspin first: it is the only advance, so it is the only place the sign can be confirmed");
        assert_eq!(p.delta_ms, i32::from(EQUIV_STEP_MS));
        assert_eq!((p.from_ms, p.to_ms), (0, EQUIV_STEP_MS));
        assert!(p.why_member.contains("2 other member(s) were not used"), "{}", p.why_member);
        assert!(p.why_member.contains("property of the transport"), "{}", p.why_member);

        // No sendspin member: AP2 next, and its live-push caveat is stated.
        let members = equiv_members(&[("pwsink-dev-y", MemberKind::PwSink), ("ap2-dev-x", MemberKind::Airplay2)]);
        let p = plan_equivalence(&members, &current, &ctx, None).expect("a plan");
        assert_eq!(p.member, "ap2-dev-x");
        assert!(p.why_member.contains("pushes it *live*"), "{}", p.why_member);

        // pw-sink last, and its baseline is the floor rather than zero.
        let members = equiv_members(&[("pwsink-dev-y", MemberKind::PwSink)]);
        let current: HashMap<String, u16> = [("pwsink-dev-y".to_string(), 0u16)].into_iter().collect();
        let p = plan_equivalence(&members, &current, &ctx, None).expect("a plan");
        assert_eq!(p.from_ms, crate::sync_settings::PWSINK_JITTER_MIN_MS, "a pw-sink knob cannot sit below its floor");
        assert_eq!(p.to_ms, crate::sync_settings::PWSINK_JITTER_MIN_MS + EQUIV_STEP_MS);
        assert_eq!(p.stored_ms, 0, "…but what the restore writes back is what was stored, not the floor it had to start from");

        // An explicit override is honoured; an unknown name is refused rather than
        // silently replaced by the automatic choice.
        let members = equiv_members(&[("spk", MemberKind::Sendspin), ("ap2-dev-x", MemberKind::Airplay2)]);
        let current: HashMap<String, u16> = HashMap::new();
        assert_eq!(plan_equivalence(&members, &current, &ctx, Some("ap2-dev-x")).expect("a plan").member, "ap2-dev-x");
        assert!(plan_equivalence(&members, &current, &ctx, Some("nope")).is_err());
        assert!(plan_equivalence(&[], &current, &ctx, None).is_err());
    }

    /// Plan §9.2 is an upper bound on the step, and it is checked against the real
    /// numbers: a step that lifted the group's send-ahead high-water mark would silence
    /// every speaker in the group to measure one of them. It refuses rather than
    /// shrinking the step, because a smaller step gives up the codec-frame property.
    #[test]
    fn a_step_that_would_lift_the_send_ahead_mark_is_refused_not_shrunk() {
        let members = equiv_members(&[("spk", MemberKind::Sendspin)]);
        let current: HashMap<String, u16> = [("spk".to_string(), 0u16)].into_iter().collect();
        // The mark is the max over members of `min_buffer + advance`, floored by the
        // group lead. With 200 ms of lead and a 190 ms buffer there are 10 ms of room —
        // not 20.
        let tight = SendAheadContext {
            floor_ms: 200,
            unreported_floor_ms: 40,
            min_buffer_ms: [("spk".to_string(), Some(190u32))].into_iter().collect(),
        };
        let r = plan_equivalence(&members, &current, &tight, None).expect_err("must refuse");
        assert_eq!(r.kind, RefusalKind::KnobRange);
        assert!(r.message.contains("high-water mark from 200 ms to 210 ms"), "{}", r.message);
        assert!(r.message.contains("silence the whole group"), "{}", r.message);

        // 40 ms of room: the same step now fits, and nothing is refused.
        let roomy = SendAheadContext { min_buffer_ms: [("spk".to_string(), Some(160u32))].into_iter().collect(), ..tight.clone() };
        let p = plan_equivalence(&members, &current, &roomy, None).expect("a plan");
        assert_eq!(p.to_ms, EQUIV_STEP_MS);

        // A delay-polarity knob never feeds that mark (§1.1.2's asymmetry), so the same
        // tight group is fine on an AP2 member.
        let ap2 = equiv_members(&[("ap2-dev-x", MemberKind::Airplay2)]);
        assert!(plan_equivalence(&ap2, &HashMap::new(), &tight, None).is_ok());
    }

    #[test]
    fn a_knob_with_no_headroom_steps_downwards_instead_of_refusing() {
        // An AP2 member already at its ceiling: the step goes the other way, and the
        // comparison normalises the sign back (see `equiv_compare`).
        let members = equiv_members(&[("ap2-dev-x", MemberKind::Airplay2)]);
        let max = crate::outputs::ap2::server::AP2_RENDER_DELAY_MAX_MS;
        let current: HashMap<String, u16> = [("ap2-dev-x".to_string(), max)].into_iter().collect();
        let p = plan_equivalence(&members, &current, &SendAheadContext::default(), None).expect("a plan");
        assert_eq!((p.from_ms, p.to_ms, p.delta_ms), (max, max - EQUIV_STEP_MS, -i32::from(EQUIV_STEP_MS)));
    }
}
