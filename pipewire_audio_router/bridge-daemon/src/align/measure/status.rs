//! What a run reports: every type the API serializes and the UI renders.
//!
//! [`MeasureStatus`] is the whole picture — phase, per-member progress, the
//! [`Proposal`] with its [`Checks`], the [`Verification`] after a write, and any
//! [`Refusal`] or [`Warning`]. [`RefusalKind`] and [`WarningKind`] are the
//! vocabulary: each variant is a sentence the user can act on, which is why there
//! are so many of them and why none is a bare error code.
//!
//! These are a wire format. The frontend's `measure.svelte.ts` and `types.ts` are
//! written against these field names, so renaming one here is an API change that
//! `npm run check` will not catch.

use super::*;

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
    pub(crate) fn is_walk(self) -> bool {
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
    pub(crate) fn is_terminal(self) -> bool {
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

    pub(crate) fn for_member(kind: RefusalKind, member: &str, message: impl Into<String>) -> Self {
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
    pub(crate) fn new(kind: WarningKind, message: impl Into<String>) -> Self {
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
    pub(crate) fn failure_advice(splits: &[MemberSplit], all_calibrated: bool) -> String {
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
    pub(crate) fn seam() -> Self {
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
