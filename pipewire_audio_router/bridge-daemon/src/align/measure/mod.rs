//! Measurement orchestration for microphone-assisted alignment
//! (docs/mic-alignment-plan.md §8).
//!
//! Drives the state machine that turns a microphone capture into a set of
//! per-member delay corrections: arm → learn → measure → solve → write → settle →
//! verify. Owns the binding between an alignment session (`align/calibrate/mod.rs`) and the
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

pub(crate) struct Inner {
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
mod chain;
mod deps;
mod equivalence;
mod feeder;
mod gate;
mod knobs;
mod run;
mod signal;
mod solve;
mod status;
mod walk;

// Re-exported so the code and its tests keep addressing these by name: the
// boundaries here organise the file, they are not an interface.
pub(crate) use chain::*;
pub(crate) use deps::*;
pub(crate) use equivalence::*;
pub(crate) use feeder::*;
pub(crate) use gate::*;
pub(crate) use knobs::*;
pub(crate) use run::*;
pub(crate) use signal::*;
pub(crate) use solve::*;
pub(crate) use status::*;
pub(crate) use walk::*;

#[cfg(test)]
mod tests;
