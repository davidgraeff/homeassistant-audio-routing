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
// One push loop for all three alignment status sockets — see that module's docs for
// why a fourth copy of it must not appear.
use crate::align::status_ws::status_socket;
use crate::align::transcript;
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

/// Tolerance for the cross-band transitivity check between **uncalibrated** members,
/// in ms. See [`transitivity`] for why it cannot be tightened to the estimator's own
/// precision (~0.05 ms): a loudspeaker's crossover genuinely delays 1.5 kHz and 3 kHz
/// differently, and that legitimate difference is indistinguishable from the
/// reflection bias this check is hunting.
///
/// **Unchanged at 3.0 despite a real run failing it at 3.45 ms** (a Home Assistant
/// Voice PE against an ESPHome satellite, phone next to both, 2026-08-12). Raising it
/// would have blunted the one instrument this design has against §5.6, so the
/// hardware difference is *measured out* instead —
/// [`CALIBRATED_TRANSITIVITY_TOL_MS`], `sync_settings::BandSplit`.
pub const TRANSITIVITY_TOL_MS: f64 = 3.0;

/// Tolerance for the cross-band check between two members whose **own band split has
/// been calibrated** (`sync_settings::BandSplit`), in ms.
///
/// Half of [`TRANSITIVITY_TOL_MS`], and tighter on purpose. Once each speaker's own
/// crossover split is subtracted, the confound that forced 3 ms is gone and what is
/// left in a residual difference is: the codec's frequency-dependent contribution
/// (plan §2.3.1 — real, unmeasured, and shared by members on the same wire codec, so
/// it largely cancels in a *difference*), the estimator's ~0.05 ms, and the thing this
/// check exists to find. §5.6's measured reflection biases were 0.89–1.72 ms, so
/// 1.5 ms catches the documented sizes while leaving the codec room.
///
/// This is the direction §5.6.1 asks for: a calibrated group gets a **sharper**
/// instrument, not a looser one. It is also why calibration is opt-in — the user who
/// measures their speakers gets the sharper check, and a false refusal there is
/// answered by re-measuring from another position, which a crossover cannot fake.
pub const CALIBRATED_TRANSITIVITY_TOL_MS: f64 = 1.5;

/// Largest band split that can plausibly be a *crossover* rather than a reflection or
/// a mis-locked peak, in ms — the ceiling on what [`MeasureManager::calibrate_split`]
/// will store.
///
/// §10.2 puts a crossover at "a millisecond or two"; 5 ms is generous room on top of
/// that for the codec (§2.3.1). Beyond it, the far more likely explanations are the
/// estimator locking onto a reflection (§5.6 measured +5.2 ms and called it excellent)
/// or the phone not actually being at the speaker — neither of which is a constant
/// worth subtracting from every future run.
pub const MAX_PLAUSIBLE_SPLIT_MS: f64 = 5.0;

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
    /// `pub(crate)` because the API layer refuses in the same vocabulary: persisting a
    /// calibration or reading a transcript can fail there, and a client must get the
    /// same shape back as for a refusal raised inside a run.
    pub(crate) fn new(kind: RefusalKind, message: impl Into<String>) -> Self {
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
    /// The pair that decided the verdict: the one furthest *past* its own tolerance
    /// when the check fails, and otherwise the one closest to it.
    pub worst_pair: Option<(String, String)>,
    /// That pair's disagreement, in ms, after each member's calibrated split was
    /// subtracted from its measured one.
    pub worst_ms: f64,
    /// The tolerance actually applied to [`Self::worst_pair`] — narrower when both of
    /// its members are calibrated ([`CALIBRATED_TRANSITIVITY_TOL_MS`]).
    pub tolerance_ms: f64,
    pub passed: bool,
    /// Every member's split, measured and residual (plan §5.6.1's W22 data: the
    /// distribution of these is the evidence that decides whether W9 is needed).
    pub splits: Vec<MemberSplit>,
    /// Whether *every* member in the check had a calibrated split. Decides both the
    /// tolerance and what the refusal is allowed to claim.
    pub all_calibrated: bool,
    /// What to do about a failure, in one sentence. Depends on calibration, because
    /// the decisive next step does: for uncalibrated members a genuine hardware
    /// difference is the leading explanation and re-measuring from another position
    /// separates it from a reflection.
    pub advice: String,
    /// Plain-language statement of the blind spot this check does *not* close.
    pub caveat: &'static str,
}

/// One member's cross-band split, and what was subtracted from it.
///
/// Reported for **every** member on every run, whether or not the check passed: a
/// calibration that is applied silently is a calibration that can be wrong without
/// anyone noticing, and this is also exactly the distribution plan §5.6.1 wants read
/// off a real run.
#[derive(Debug, Clone, Serialize)]
pub struct MemberSplit {
    pub node_name: String,
    /// `phase_B − phase_A − half a period`, averaged over this member's readings.
    pub measured_ms: f64,
    /// The stored per-output calibration that was subtracted, if any
    /// (`sync_settings::BandSplit::split_ms`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibrated_ms: Option<f64>,
    /// What the check actually compares: measured − calibrated (= measured when
    /// uncalibrated).
    pub residual_ms: f64,
}

impl TransitivityCheck {
    /// The sentence a refusal appends. Named on the check rather than written out at
    /// each of the three refusal sites so they cannot drift apart.
    fn failure_advice(splits: &[MemberSplit], all_calibrated: bool) -> String {
        if all_calibrated {
            return "both speakers' own crossover splits are calibrated and have been subtracted, so this disagreement is not the \
                    hardware: it is an early reflection pulling one arrival (plan §5.6), or the wire codec smearing the two bands \
                    differently (§2.3.1). Move the phone away from walls and hard surfaces and measure again — a reflection moves \
                    with the phone, and nothing else here does."
                .to_string();
        }
        let uncalibrated: Vec<&str> = splits.iter().filter(|s| s.calibrated_ms.is_none()).map(|s| s.node_name.as_str()).collect();
        format!(
            "two things produce this and they cannot be told apart from one position. If these are **different speaker models** the \
             likely cause is genuine hardware: a crossover delays 1.5 kHz and 3 kHz differently, by a millisecond or two, and \
             differently per model — which is not an error and would not spoil an alignment. The other cause is an early reflection \
             pulling one arrival, which would. **The decisive test is to measure again from a different position**: a crossover split \
             is a fixed property of the speaker and reads the same everywhere, a reflection-induced one changes with the geometry. If \
             it reads the same, calibrate the speakers' own splits (POST /api/align/measure/split, phone held at each speaker) — that \
             subtracts the hardware and makes this check sharper rather than looser. Uncalibrated here: {}.",
            match uncalibrated.is_empty() {
                true => "none".to_string(),
                false => uncalibrated.join(", "),
            }
        )
    }
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
    /// This run's transcript id (`align/transcript.rs`), when one is being recorded:
    /// `GET /api/align/measure/log?run=<id>` is then the whole run as one document.
    /// `None` means no run has started here, or transcripts are disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_run: Option<String>,
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
    /// Silence every member without releasing the hold or stopping the click track.
    ///
    /// What a **parked** run owes the room. The hold has to stay (`apply` measures
    /// through it and writes into it), but the tick/tack must not keep looping while
    /// the user reads a proposal, and it must fall silent even if the tab is closed —
    /// so this is the daemon's decision, not an empty audible set posted by the panel
    /// (plan §12.3.2's principle: the party holding the numbers decides). Resuming
    /// needs no counterpart: every measuring step begins with [`Self::solo`].
    fn silence(&self) -> Fut<'_, Result<(), String>>;
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

    fn silence(&self) -> Fut<'_, Result<(), String>> {
        Box::pin(async move {
            // The session's own set-based audibility with an empty set. Its levels,
            // its hold and its player thread are untouched, so `apply`'s verification
            // walk can still measure through exactly the same session.
            crate::align::calibrate::AlignManager::silence(self).await.map(|_| ())
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
/// Implemented in `api/measure.rs` **on top of the existing endpoint handlers** rather
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
    /// Each member's persisted band-split calibration in ms, keyed by node name
    /// (`sync_settings::BandSplit::split_ms`). Subtracted before the cross-band check
    /// compares members, so a mixed-model group is not refused for its hardware
    /// (plan §10.2). Empty is the uncalibrated case and behaves exactly as before.
    pub band_splits: BandSplits,
    /// Where this run's forensic transcript goes (`align/transcript.rs`). A run opens
    /// one file here and appends to it; `Transcripts::disabled()` records nothing,
    /// which is what a unit test and a daemon without `/data` both get.
    pub transcript: Arc<crate::align::transcript::Transcripts>,
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
/// **The confound, and what is now done about it.** A loudspeaker crossover
/// genuinely delays 1.5 kHz and 3 kHz differently — often by a millisecond or two, and
/// differently for different models. That forced a 3 ms tolerance, and a real
/// mixed-model run still failed it: 3.45 ms between a Home Assistant Voice PE and an
/// ESPHome satellite with the phone next to both (2026-08-12). Transitivity **blocks
/// the write**, so that group could not be aligned at all.
///
/// The fix is not a wider tolerance. A crossover split is a *fixed property of the
/// speaker*, so unlike a reflection it can be **measured once and subtracted**:
/// `calibration` carries each output's own split (`sync_settings::BandSplit`, measured
/// at close range by [`MeasureManager::calibrate_split`]) and this compares the
/// **residuals**. Two consequences, both intended:
///
/// * an uncalibrated member behaves exactly as before, at [`TRANSITIVITY_TOL_MS`];
/// * a pair that is calibrated on *both* sides is held to
///   [`CALIBRATED_TRANSITIVITY_TOL_MS`], because the legitimate difference has been
///   removed — so the check becomes **more** sensitive to §5.6's reflections rather
///   than blunter, which is what §5.6.1 wants from this data.
///
/// A pass is still not proof that §5.6 did not happen: a reflection biasing *both*
/// bands equally, or biasing every member alike, cancels here. Only W9 (chirp +
/// matched filter) resolves the direct arrival from an early reflection properly.
pub fn transitivity(obs: &[MemberObservation], timing: &Timing, tolerance_ms: f64, calibration: &BandSplits) -> TransitivityCheck {
    let mut by_member: HashMap<&str, Vec<f64>> = HashMap::new();
    for o in obs {
        by_member.entry(o.node_name.as_str()).or_default().push(member_split_ms(&o.m, timing));
    }
    let mut splits: Vec<MemberSplit> = Vec::new();
    for o in obs {
        if splits.iter().any(|s| s.node_name == o.node_name) {
            continue;
        }
        let v = &by_member[o.node_name.as_str()];
        let measured_ms = v.iter().sum::<f64>() / v.len() as f64;
        let calibrated_ms = calibration.get(&o.node_name).copied();
        splits.push(MemberSplit {
            node_name: o.node_name.clone(),
            measured_ms,
            calibrated_ms,
            residual_ms: measured_ms - calibrated_ms.unwrap_or(0.0),
        });
    }
    let all_calibrated = !splits.is_empty() && splits.iter().all(|s| s.calibrated_ms.is_some());

    // The decisive pair is the one furthest past *its own* tolerance, not the one with
    // the largest raw disagreement: with mixed calibration those can be different
    // pairs, and reporting a passing pair's number next to a failing verdict would be
    // incoherent.
    let mut worst: Option<(f64, f64, (String, String))> = None;
    for (i, a) in splits.iter().enumerate() {
        for b in splits.iter().skip(i + 1) {
            let both = a.calibrated_ms.is_some() && b.calibrated_ms.is_some();
            let tol = match both {
                true => tolerance_ms.min(CALIBRATED_TRANSITIVITY_TOL_MS),
                false => tolerance_ms,
            };
            let d = (a.residual_ms - b.residual_ms).abs();
            if worst.as_ref().is_none_or(|(wd, wt, _)| d - tol > wd - wt) {
                worst = Some((d, tol, (a.node_name.clone(), b.node_name.clone())));
            }
        }
    }
    let (worst_ms, applied_tol, worst_pair) = match worst {
        Some((d, t, pair)) => (d, t, Some(pair)),
        None => (0.0, tolerance_ms, None),
    };
    TransitivityCheck {
        worst_pair,
        worst_ms,
        tolerance_ms: applied_tol,
        passed: worst_ms <= applied_tol,
        advice: TransitivityCheck::failure_advice(&splits, all_calibrated),
        splits,
        all_calibrated,
        caveat: "measured as cross-band agreement, the only independent pairing a single mic position offers; a pass does \
                 not rule out an early-reflection bias shared by both bands, and an uncalibrated loudspeaker crossover can \
                 fail it legitimately (plan §5.6). Each member's own split can be calibrated out, which tightens it",
    }
}

/// One member's cross-band split: how much later its 1.5 kHz burst arrived than the
/// half-period the click track puts between the two bursts (plan §5.6.1).
///
/// The reflection signature, and — measured at close range, where reflections are
/// negligible — the speaker's own crossover constant. One expression, used by the
/// transitivity check and by the calibration alike, so the stored constant and the
/// number it is subtracted from can never be computed two different ways.
pub fn member_split_ms(m: &MemberMeasurement, timing: &Timing) -> f64 {
    wrap_sym(m.phase_b_ms - m.phase_a_ms - timing.nominal_ab_ms(), timing.pattern_ms)
}

/// Per-output band-split calibrations, by node name, in ms (the values of
/// `sync_settings::BandSplit`). Empty = nothing calibrated, which is how every check
/// that consumes it behaved before the calibration existed.
pub type BandSplits = HashMap<String, f64>;

/// The empty calibration set, for call sites that legitimately have none (a chain's
/// aggregate placeholder, and the unit tests of the checks themselves).
pub fn no_band_splits() -> &'static BandSplits {
    static EMPTY: OnceLock<BandSplits> = OnceLock::new();
    EMPTY.get_or_init(BandSplits::new)
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
    /// Each member's calibrated band split, subtracted before the cross-band check
    /// compares them (plan §10.2). [`no_band_splits`] for "nothing calibrated", which
    /// is what every uncalibrated group runs as.
    pub band_splits: &'a BandSplits,
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
        transitivity: transitivity(input.observations, &input.timing, TRANSITIVITY_TOL_MS, input.band_splits),
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
                "the two test tones disagree by {:.2} ms about how far apart '{a}' and '{b}' are (limit {:.1} ms), so the measured \
                 offset is not purely the electrical one and nothing is written. {}",
                checks.transitivity.worst_ms, checks.transitivity.tolerance_ms, checks.transitivity.advice
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
            .unwrap_or_else(|| transitivity(&[], &input.timing, TRANSITIVITY_TOL_MS, no_band_splits())),
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
    /// The session this run is driving, kept here for the same reason [`Self::relay`]
    /// is: **`abandon` has to be able to silence the room**. A cancelled run's task may
    /// never get another chance to, and a closed tab must still stop the click track
    /// (it keeps looping otherwise, which is what a real run got complained about).
    /// `None` means no run ever started against this state — which is exactly the
    /// by-ear case, and why a manual session is never silenced from here.
    session: Option<Arc<dyn SessionControl>>,
    /// This run's transcript (`align/transcript.rs`). Always present — a run with no
    /// `/data` gets a disabled log rather than an `Option` every recording site would
    /// have to unwrap.
    log: Arc<crate::align::transcript::RunLog>,
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
            session: None,
            log: crate::align::transcript::RunLog::disabled(),
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
            log_run: match self.log.id() {
                "" => None,
                id => Some(id.to_string()),
            },
            elapsed_s: self.started.map(|s| s.elapsed().as_secs()).unwrap_or(0),
        }
    }

    fn warn(&mut self, w: Warning) {
        if !self.warnings.iter().any(|e| e.kind == w.kind) {
            // Inside the de-duplication, so the transcript carries each warning kind
            // once, in the order the run raised it — the same set the status shows, with
            // the timestamps the status cannot keep.
            self.log.record(transcript::Event::new("warning", w.message.clone()).detail(&w));
            self.warnings.push(w);
            self.bump();
        }
    }

    /// Append one line to this run's transcript (`align/transcript.rs`).
    ///
    /// A method on the state rather than a free function because the log's lifetime is
    /// the run's: a `start` opens it, `abandon` replaces it, and every recording site
    /// already holds this lock for the state change the event describes — so the two
    /// cannot disagree about what happened.
    fn record(&self, ev: transcript::Event) {
        self.log.record(ev);
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
    /// A band-split calibration ([`MeasureManager::calibrate_split`]) is in progress.
    ///
    /// Not part of [`Inner`] deliberately: `start` *replaces* `Inner`, and this flag has
    /// to be visible across that replacement, because the hazard it guards is exactly
    /// "a run started while one speaker was being calibrated" — both solo through the
    /// same session, and the loser measures whatever the winner made audible.
    split_busy: Arc<AtomicBool>,
}

impl MeasureManager {
    fn with_inner(inner: Arc<Mutex<Inner>>) -> Self {
        Self { inner, split_busy: Arc::new(AtomicBool::new(false)) }
    }
}

/// Clears [`MeasureManager::split_busy`] on every exit path, including the refusals
/// after the flag is taken.
struct SplitGuard<'a>(&'a AtomicBool);

impl Drop for SplitGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// One output's measured band split, as [`MeasureManager::calibrate_split`] reports it
/// and as `api/measure.rs` persists it (`sync_settings::BandSplit`).
#[derive(Debug, Clone, Serialize)]
pub struct SplitCalibration {
    pub node_name: String,
    /// `phase_B − phase_A − ½ period`, in ms: how much later this speaker renders
    /// 1.5 kHz than 3 kHz.
    pub split_ms: f64,
    pub std_error_ms: f64,
    pub peak_snr_db: f64,
    pub second_peak_ratio: f64,
    /// The playback level it was measured at.
    pub level: u8,
    /// What the number does and does not establish — shown next to it, because a
    /// calibration that looks authoritative is worse than one that explains itself.
    pub note: &'static str,
}

/// The process-wide orchestrator, in the same shape `align_mic` uses — it is a
/// single resource (one mic, one session, one group) with nothing per-request to
/// thread through `AppState`.
pub fn shared() -> &'static MeasureManager {
    static M: OnceLock<MeasureManager> = OnceLock::new();
    M.get_or_init(|| MeasureManager::with_inner(Arc::new(Mutex::new(Inner::idle()))))
}

impl MeasureManager {
    pub fn status(&self) -> MeasureStatus {
        self.inner.lock_recover().status()
    }

    /// A receiver that fires whenever [`Self::status`] would return something new
    /// (plan §11: progress is pushed, not polled). Survives `abandon`/`start`,
    /// which replace the state but carry the notifier across.
    #[allow(dead_code)] // used by `measure_ws`, whose route api/measure.rs owns
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
        // Opened before the state is replaced and before the run task exists, so the
        // very first thing in the file is what the run was asked to do. Retention runs
        // here too (`Transcripts::begin`), which is why the disk high-water mark is
        // `MAX_RUNS` files and not one more.
        let log = deps.transcript.begin();
        let status = {
            let mut inner = self.inner.lock_recover();
            // The notifier outlives the state it reports on, so an open `measure_ws`
            // sees the new run rather than being silently disconnected.
            *inner = Inner::idle_watching(inner.changes.clone());
            inner.log = log;
            inner.session = Some(deps.session.clone());
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
            inner.record(
                transcript::Event::new(
                    "run_started",
                    format!(
                        "{:?} run over {} member(s){}",
                        deps.mode,
                        inner.members.len(),
                        match chained {
                            true => ", chained across positions",
                            false => "",
                        }
                    ),
                )
                .detail(&serde_json::json!({
                    "mode": deps.mode,
                    "chained": chained,
                    "sources": inner.sources,
                    "sample_rate": inner.sample_rate,
                    "session_level": session.level,
                    // The knobs the proposal will be a delta from — without these the
                    // written values in this file cannot be interpreted later.
                    "members": inner.members.iter().map(|m| serde_json::json!({
                        "node_name": m.node_name,
                        "kind": m.kind,
                        "current_delay_ms": m.current_delay_ms,
                        "band_split_calibration_ms": deps.band_splits.get(&m.node_name),
                    })).collect::<Vec<_>>(),
                })),
            );
            inner.bump();
            inner.status()
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let outcome = run_measure(&deps, &inner, &cancel, cmd_rx).await;
            finish(&deps, &inner, &cancel, outcome).await;
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
            // The run resumes on the same transcript: apply is the second half of one
            // run, and splitting it would put the writes in a different file from the
            // proposal they came from.
            inner.record(transcript::Event::new("apply", "the user applied the proposal").detail(&proposal));
            inner.bump();
            inner.status()
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let outcome = run_apply(&deps, &inner, &cancel, &proposal, cmd_rx).await;
            finish(&deps, &inner, &cancel, outcome).await;
        });
        Ok(status)
    }

    /// `POST /api/align/measure/split` — measure **one** output's own cross-band split
    /// at close range, so the transitivity check can subtract it (plan §10.2).
    ///
    /// ## Why this exists, and why it is a separate operation
    ///
    /// §10.2's check compares members' `split_i = phase_B − phase_A − ½ period`. A
    /// loudspeaker's crossover contributes to that, differently per model, which is why
    /// the tolerance had to be 3 ms — and a real mixed-model group still failed at
    /// 3.45 ms with the phone next to both speakers (2026-08-12), which *blocks the
    /// write*, so the group could not be aligned at all. A crossover split is a fixed
    /// property of the speaker, so unlike a reflection it can be measured once and
    /// subtracted. That is what this does.
    ///
    /// ## What makes the reading a calibration rather than another measurement
    ///
    /// Only the **distance**: the user holds the phone at the speaker, where the direct
    /// sound dominates any reflection by far more than the 0.9× that produced §5.6's
    /// biases — the same premise near-field mode rests on, and the same one it cannot
    /// verify. So this is refused rather than stored when the number is implausibly
    /// large for a crossover ([`MAX_PLAUSIBLE_SPLIT_MS`]), which is the one form of
    /// "you were not at the speaker" that is detectable from here.
    ///
    /// Runs inline (one solo, one gate, one reading — on the order of fifteen seconds)
    /// and refuses while a measurement run is live, because both drive the same
    /// session's audibility.
    pub async fn calibrate_split(&self, deps: MeasureDeps, node_name: String, level: Option<u8>) -> Result<SplitCalibration, Refusal> {
        {
            let inner = self.inner.lock_recover();
            if inner.running {
                return Err(Refusal::new(
                    RefusalKind::Internal,
                    "a measurement is running; a band-split calibration solos one speaker on its own and would fight it. Finish or \
                     abandon the run first.",
                ));
            }
        }
        if self.split_busy.swap(true, Ordering::SeqCst) {
            return Err(Refusal::new(RefusalKind::Internal, "a band-split calibration is already running"));
        }
        // Named rather than `_`, so it lives to the end of the call and every refusal
        // below clears the flag on its way out.
        let _done = SplitGuard(&self.split_busy);
        let session = deps.session.snapshot().await;
        if !session.active {
            return Err(Refusal::new(
                RefusalKind::NoSession,
                "no alignment session is running, so nothing is playing to measure. Start a session holding this speaker, stand next to \
                 it, and calibrate from there.",
            ));
        }
        let Some(member) = session.members.iter().find(|m| m.node_name == node_name).cloned() else {
            return Err(Refusal::for_member(
                RefusalKind::Internal,
                &node_name,
                format!("'{node_name}' is not a member of the running alignment session, so it cannot be calibrated"),
            ));
        };
        let mic = deps.mic.status();
        if !mic.connected {
            return Err(Refusal::new(
                RefusalKind::MicMissing,
                "no microphone capture is connected. Open the alignment panel on the phone, start the microphone, and hold it at the \
                 speaker.",
            ));
        }
        // A throwaway state: this is not a run, so it must not overwrite the status a
        // finished run has parked in (its proposal is still applicable). The gate, the
        // estimator and the interference handling are the real ones.
        let scratch = Arc::new(Mutex::new(Inner::idle()));
        scratch.lock_recover().sample_rate = mic.sample_rate;
        let cancel = AtomicBool::new(false);
        let level = level.unwrap_or_else(|| session.level_for(&node_name));
        let cfg = GateConfig::mute_settle(&deps.timing);
        let o = match measure_member(&deps, &scratch, &cancel, &member, level, cfg, 0, 0, mic.sample_rate).await {
            Ok(o) => o,
            Err(StepError::Refuse(r)) | Err(StepError::RestartSet(r)) => return Err(r),
        };
        let split_ms = member_split_ms(&o.m, &deps.timing);
        if split_ms.abs() > MAX_PLAUSIBLE_SPLIT_MS {
            return Err(Refusal::for_member(
                RefusalKind::Estimator,
                &node_name,
                format!(
                    "'{node_name}' read a cross-band split of {split_ms:.2} ms, which is too large to be a crossover (limit {:.1} ms). A \
                     crossover delays the two tones by a millisecond or two; this size is what an early reflection looks like when the \
                     estimator locks onto it, or what happens when the phone is not actually at the speaker. Nothing was stored — hold \
                     the phone within a hand's width of the driver, away from the wall behind it, and try again.",
                    MAX_PLAUSIBLE_SPLIT_MS
                ),
            ));
        }
        // Appended to whatever run is parked in the state, when there is one: a
        // calibration is nearly always the answer to a refusal, and the refusal's own
        // transcript is where someone investigating it will be looking.
        self.inner.lock_recover().record(
            transcript::Event::for_member("split_calibrated", &node_name, format!("'{node_name}' band split {split_ms:.2} ms"))
                .detail(&serde_json::json!({ "split_ms": split_ms, "observation": o, "level": level })),
        );
        Ok(SplitCalibration {
            node_name,
            split_ms,
            std_error_ms: o.m.std_error_ms,
            peak_snr_db: o.m.peak_snr_db,
            second_peak_ratio: o.m.second_peak_ratio,
            level,
            note: "measured at close range, where the direct sound dominates a reflection — that premise is yours to keep and nothing \
                   here can check it. Stored per output and subtracted from this speaker's split before the cross-band check compares it \
                   with the others (plan §10.2), which also tightens that check for calibrated pairs.",
        })
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
    ///
    /// Async only because of the silencing: a cancelled run's task will never solo
    /// anything again, so this is the last chance to stop the click track, and a closed
    /// tab must not leave a room ticking. The session itself is **not** stopped — the
    /// by-ear panel and `revert` both still need it.
    pub async fn abandon(&self) -> MeasureStatus {
        let (status, session) = self.abandon_state();
        // Only a session this manager actually drove is silenced. A by-ear session that
        // no run ever touched is left exactly as the user set it up — two speakers
        // audible while they nudge one of them is the entire point of that path.
        if let Some(session) = session {
            if let Err(e) = session.silence().await {
                tracing::info!("alignment: could not silence the abandoned group: {e}");
            }
        }
        status
    }

    fn abandon_state(&self) -> (MeasureStatus, Option<Arc<dyn SessionControl>>) {
        let mut inner = self.inner.lock_recover();
        inner.cancel.store(true, Ordering::Relaxed);
        let session = inner.session.clone();
        // `abandoned` rather than a second `run_finished`: a run parked on a proposal has
        // already written its verdict, and abandoning it is a further event rather than a
        // correction of that one.
        inner.record(transcript::Event::new("abandoned", "the run was abandoned by the user").detail(&serde_json::json!({
            "phase": "cancelled",
            "provisional_lines": inner.provisional.len(),
            "written": inner.written,
        })));
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
        (inner.status(), session)
    }
}

/// Park the state machine on a terminal state, and silence the room.
///
/// Skipped entirely when the run's own cancel flag is set: that flag is what
/// `abandon` raises, and each run owns a fresh one, so a run that was abandoned
/// (or superseded by a newer one) must not write its late verdict over the state
/// the user is now looking at — nor silence a session that state has moved on to.
async fn finish(deps: &MeasureDeps, inner: &Arc<Mutex<Inner>>, cancel: &AtomicBool, outcome: Result<Phase, Refusal>) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    park(inner, cancel, outcome);
    // Every state this lands on is one the user reads rather than listens to
    // (`Proposed`, `Done`, `Refused`), so nothing should still be audible — the hold
    // stays, the click track keeps looping off the one clock `apply` needs, and every
    // member is muted. Awaited here rather than spawned so that a client which
    // abandons or re-selects the moment it sees the terminal phase cannot race us into
    // re-silencing a session it has just taken over.
    if let Err(e) = deps.session.silence().await {
        // Not a refusal: the run's verdict is already recorded, and a session that
        // cannot be silenced is one that has usually gone away by itself.
        tracing::info!("alignment: could not silence the parked group: {e}");
        inner.lock_recover().record(transcript::Event::new("silence_failed", format!("could not silence the parked group: {e}")));
    }
}

/// The state half of [`finish`], separate so the state lock is released before the
/// silencing await. Keeps its own cancel check: it is the one that guards the state.
fn park(inner: &Arc<Mutex<Inner>>, cancel: &AtomicBool, outcome: Result<Phase, Refusal>) {
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
            g.record(transcript::Event::new("run_finished", g.message.clone()).detail(&serde_json::json!({ "phase": phase })));
        }
        Err(refusal) => {
            g.record(refusal_event(&refusal));
            g.record(
                transcript::Event::new("run_finished", refusal.message.clone()).detail(&serde_json::json!({ "phase": Phase::Refused })),
            );
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

/// A refusal as one transcript line, carrying the whole refusal as its detail (the
/// kind, the member, and the estimator's own verdict where it came from there).
fn refusal_event(r: &Refusal) -> transcript::Event {
    match r.member.as_deref() {
        Some(m) => transcript::Event::for_member("refusal", m, r.message.clone()),
        None => transcript::Event::new("refusal", r.message.clone()),
    }
    .detail(r)
}

fn set_phase(inner: &Arc<Mutex<Inner>>, phase: Phase, message: impl Into<String>) {
    let mut g = inner.lock_recover();
    g.phase = phase;
    g.message = message.into();
    // Every phase transition in the run loop goes through here, so this is the
    // transcript's spine: the sequence of these lines *is* what the run did, and the
    // gaps between their timestamps are where it spent its minutes.
    g.record(transcript::Event::new("phase", g.message.clone()).detail(&serde_json::json!({ "phase": phase })));
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
        band_splits: &deps.band_splits,
        closure,
    })?;
    let blocked = proposal.blocked.clone();
    {
        let mut g = inner.lock_recover();
        for w in &proposal.warnings {
            g.warn(w.clone());
        }
        // The whole proposal, verbatim: the knobs, the checks with each member's
        // measured and calibrated band split, and the refusal that blocks it if one
        // does. This one line is what makes a run reconstructable afterwards.
        g.record(
            transcript::Event::new(
                "proposal",
                match proposal.blocked.as_ref() {
                    None => format!("proposed: reference '{}', spread {:.2} ms", proposal.reference, proposal.spread_ms),
                    Some(b) => format!("proposal blocked ({:?})", b.kind),
                },
            )
            .detail(&proposal),
        );
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
                g.record(
                    transcript::Event::new("set_restart", r.message.clone())
                        .detail(&serde_json::json!({ "attempt": restarts, "limit": MAX_SET_RESTARTS, "grid_epoch": epoch, "refusal": r })),
                );
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
        g.record(
            transcript::Event::new(
                "proposal",
                match proposal.blocked.as_ref() {
                    None => format!("proposed: reference '{}', {} position(s)", proposal.reference, steps.len()),
                    Some(b) => format!("chain proposal blocked ({:?})", b.kind),
                },
            )
            .detail(&proposal),
        );
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
        transitivity: transitivity(&observations, &deps.timing, TRANSITIVITY_TOL_MS, &deps.band_splits),
        repeatability: repeatability(&observations, &arrivals.fit, deps.timing.pattern_ms, REPEATABILITY_TOL_MS),
        merged_peak: MergedPeakCheck::seam(),
        closure: None,
    };
    if !checks.transitivity.passed {
        let (a, b) = checks.transitivity.worst_pair.clone().unwrap_or_default();
        return Err(Refusal::new(
            RefusalKind::Transitivity,
            format!(
                "at position {index} the two test tones disagree by {:.2} ms about how far apart '{a}' and '{b}' are (limit {:.1} ms), so \
                 the measured offset is not purely the electrical one. This position was not accepted; the speakers aligned at earlier \
                 positions are untouched. {}",
                checks.transitivity.worst_ms, checks.transitivity.tolerance_ms, checks.transitivity.advice
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
                let mut g = inner.lock_recover();
                // The endpoint's own reply, **verbatim**: it is what says whether the
                // device was reconnected to pick the value up, and that is the sentence
                // anyone reconstructing a run needs (plan §2.3).
                g.record(transcript::Event::for_member("write", &m.node_name, msg.clone()).detail(&serde_json::json!({
                    "kind": m.kind,
                    "from_ms": m.current_delay_ms,
                    "to_ms": m.new_delay_ms,
                    "reply": msg,
                })));
                g.mark_written(&m.node_name);
                drop(g);
                tracing::info!("alignment write: {msg}");
            }
            Err(e) => {
                inner
                    .lock_recover()
                    .record(transcript::Event::for_member("write_failed", &m.node_name, e.clone()).detail(
                        &serde_json::json!({ "kind": m.kind, "from_ms": m.current_delay_ms, "to_ms": m.new_delay_ms, "error": e }),
                    ));
                return Err(Refusal::for_member(
                    RefusalKind::WriteFailed,
                    &m.node_name,
                    format!("writing '{}''s delay failed: {e}. Use revert to restore the delays from before this session.", m.node_name),
                ));
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
    let trans = transitivity(&observations, &deps.timing, TRANSITIVITY_TOL_MS, &deps.band_splits);
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
        g.record(
            transcript::Event::new(
                "verification",
                format!(
                    "residual {:.2} ms (limit {:.1}), cross-band {:.2} ms (limit {:.1}): {}",
                    residual.worst_ms,
                    residual.tolerance_ms,
                    trans.worst_ms,
                    trans.tolerance_ms,
                    match passed {
                        true => "passed",
                        false => "failed",
                    }
                ),
            )
            .detail(&verification),
        );
        g.verification = Some(verification);
        g.bump();
    }
    if !trans.passed {
        let (a, b) = trans.worst_pair.clone().unwrap_or_default();
        return Err(Refusal::new(
            RefusalKind::Transitivity,
            format!(
                "after writing, the two test tones disagree by {:.2} ms about '{a}' vs '{b}' (limit {:.1} ms), so the \
                 delays that were written cannot be trusted — revert and measure again. {}",
                trans.worst_ms, trans.tolerance_ms, trans.advice
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
            {
                let mut g = inner.lock_recover();
                // Recorded per occurrence, unlike the warning, which de-duplicates by
                // kind: three doorbells during one run is a different story from one.
                g.record(transcript::Event::for_member("interference", i.member.clone(), i.reason.clone()).detail(&i));
                g.warn(Warning::new(WarningKind::Interference, i.reason.clone()));
            }
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
                inner.lock_recover().record(
                    transcript::Event::for_member("gate_failed", name, r.message.clone())
                        .detail(&serde_json::json!({ "refusal": r, "gate": step.progress })),
                );
                return Err(if r.kind == RefusalKind::MicReconnected { StepError::RestartSet(r) } else { StepError::Refuse(r) });
            }
            if step.restart {
                // The reason is the whole value of recording a restart: "acquiring" for
                // the tenth time and "the tone stopped again" are the same delay to the
                // user and completely different diagnoses.
                inner
                    .lock_recover()
                    .record(transcript::Event::for_member("gate_restart", name, step.progress.message.clone()).detail(&step.progress));
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
                let o = MemberObservation {
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
                };
                let split_ms = member_split_ms(&o.m, timing);
                let calibrated = deps.band_splits.get(name).copied();
                let mut g = inner.lock_recover();
                g.record(
                    transcript::Event::for_member(
                        "measurement",
                        name,
                        format!(
                            "accepted pass {} of '{name}': {:.2} ms at 3 kHz, SNR {:.1} dB, cross-band split {split_ms:.2} ms{}",
                            pass + 1,
                            o.m.phase_a_ms,
                            o.m.peak_snr_db,
                            // Wherever a calibration is *applied*, it is said out loud —
                            // a wrong calibration must be visible in the record rather
                            // than silently correcting the numbers beside it.
                            match calibrated {
                                Some(c) => format!(" (calibrated {c:.2} ms, residual {:.2} ms)", split_ms - c),
                                None => String::new(),
                            }
                        ),
                    )
                    .detail(&serde_json::json!({
                        "observation": o,
                        "gate": step.progress,
                        "level": level,
                        "split_ms": split_ms,
                        "band_split_calibration_ms": calibrated,
                        "residual_split_ms": calibrated.map(|c| split_ms - c),
                    })),
                );
                g.note(name, None);
                drop(g);
                return Ok(o);
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

/// `GET /api/align/measure/ws` — the run status, pushed.
///
/// One full [`MeasureStatus`] on connect, then one on every change. Same shape a
/// `GET /api/align/measure` poll returns, so a client can use either and the
/// socket is purely a latency improvement: a measurement spends most of its time
/// inside a gate whose *message* is the only thing moving, and polling that at any
/// useful rate is what §11 objected to.
///
/// Registered in `api/measure.rs`, which owns the router.
pub async fn measure_ws(ws: axum::extract::ws::WebSocketUpgrade) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| {
        let m = shared();
        // Subscribed *before* the first status is read, so a change that lands between
        // the two is a redundant push rather than a missed one.
        status_socket(socket, m.subscribe(), || Box::pin(async { serde_json::to_string(&m.status()).ok() }))
    })
}

// The run's pieces, each with a matching test module in `tests/`.
mod equivalence;
mod gate;
mod knobs;
mod signal;

// Re-exported so the code and its tests keep addressing these by name: the
// boundaries here organise the file, they are not an interface.
pub(crate) use equivalence::*;
pub(crate) use gate::*;
pub(crate) use knobs::*;
pub(crate) use signal::*;

#[cfg(test)]
mod tests;
