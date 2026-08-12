//! The relay-vs-device equivalence experiment (W21): does a delay applied in the
//! daemon's relay measure the same as the device's own knob?
//!
//! It matters because a run applies *provisional* delays in the relay
//! (`align/relay_delay.rs`) to avoid paying a device reconnect at every step, then
//! writes the real knobs once at the end. That substitution is only sound if the two
//! move the sound by the same amount, and this measures both arms on one member and
//! reports the difference with a bound — or refuses, naming which arm it could not
//! trust: a sign inversion, a knob the device ignores, a delay line that does
//! nothing.
//!
//! Self-contained on purpose: its own manager, its own state machine, its own two
//! endpoints and WebSocket, and it borrows a knob it always puts back — including
//! when it is abandoned. It shares only the gate and the estimator with a run.
//!
//! The experiment is the *instrument* for a measurement that has not been taken
//! yet: it is answered by W22, live acceptance on real speakers
//! (docs/mic-alignment-plan.md §14.3). Do not gate it out of the build that W22
//! would run on.

use super::*;

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
pub(crate) const EQUIV_PRIME_TIMEOUT: Duration = Duration::from_secs(10);

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
/// `api/measure.rs`'s handler pushes it *live* to the running stream — so its "reconnect" may
/// not happen at all, which makes it a poor probe of the ε item 3 is about.
///
/// **pw-sink last.** Its knob is floored at `PWSINK_JITTER_MIN_MS`, so its baseline
/// cannot be a device zero, and a write reloads the receiving module (an audible gap of
/// its own).
pub(crate) fn equiv_kind_rank(kind: MemberKind) -> u8 {
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
                 is no sign question here — and `api/measure.rs` pushes it *live* to the running stream, so the two baselines may not be separated \
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

pub(crate) struct EquivInner {
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
    pub(crate) fn idle() -> Self {
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
pub(crate) struct EquivState {
    inner: Arc<Mutex<EquivInner>>,
    scratch: Arc<Mutex<Inner>>,
    changes: Arc<tokio::sync::watch::Sender<u64>>,
}

impl EquivState {
    pub(crate) fn new() -> Self {
        let changes = Arc::new(tokio::sync::watch::channel(0).0);
        Self {
            inner: Arc::new(Mutex::new(EquivInner::idle())),
            scratch: Arc::new(Mutex::new(Inner::idle_watching(changes.clone()))),
            changes,
        }
    }

    pub(crate) fn bump(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub(crate) fn status(&self) -> EquivalenceStatus {
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

    pub(crate) fn set(&self, phase: EquivPhase, message: impl Into<String>) {
        {
            let mut g = self.inner.lock_recover();
            g.phase = phase;
            g.message = message.into();
        }
        self.bump();
    }

    pub(crate) fn say(&self, message: impl Into<String>) {
        self.inner.lock_recover().message = message.into();
        self.bump();
    }
}

/// The relay-vs-device equivalence experiment: one at a time, process-wide.
pub struct EquivalenceManager {
    pub(crate) st: EquivState,
}

/// The process-wide experiment, for the same reason [`shared`] is process-wide: one
/// mic, one session, one group.
// Used by the API handlers that own the routes, which `api/measure.rs` has yet to add.
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
    #[allow(dead_code)] // used by `equivalence_ws`, whose route api/measure.rs owns
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.st.changes.subscribe()
    }

    /// `POST /api/align/equivalence` — start the experiment.
    ///
    /// Refuses up front on everything knowable without playing anything: a measurement
    /// run in flight (both would solo the same session), no alignment session, no
    /// microphone, no knob headroom.
    #[allow(dead_code)] // wired by api/measure.rs, which owns the router
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
    #[allow(dead_code)] // wired by api/measure.rs, which owns the router
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
pub(crate) struct EquivApplied {
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
pub(crate) async fn drive_equivalence(deps: EquivalenceDeps, st: EquivState, cancel: Arc<AtomicBool>) {
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
pub(crate) async fn restore_equivalence(deps: &EquivalenceDeps, st: &EquivState, applied: &EquivApplied) -> RestoreReport {
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
pub(crate) async fn equivalence_body(
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
pub(crate) async fn equiv_write(
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
pub(crate) async fn equiv_wait_primed(deps: &EquivalenceDeps, st: &EquivState, cancel: &AtomicBool, output: &str) -> Result<f64, Refusal> {
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
pub(crate) async fn equiv_read(
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
pub(crate) fn equiv_phases(o: &MemberObservation) -> [f64; 2] {
    [o.m.phase_a_ms, o.m.phase_b_ms]
}

/// The shift between two readings, per band and then averaged, wrapped the short way.
pub(crate) fn equiv_shift(from: &MemberObservation, to: &MemberObservation, pattern_ms: f64) -> f64 {
    let (from, to) = (equiv_phases(from), equiv_phases(to));
    (wrap_sym(to[0] - from[0], pattern_ms) + wrap_sym(to[1] - from[1], pattern_ms)) / 2.0
}

/// A bracketed arm: the shift of the middle reading against the **mean** of the two
/// baselines, which cancels a linear drift between them exactly.
pub(crate) fn equiv_bracket(b1: &MemberObservation, mid: &MemberObservation, b2: &MemberObservation, pattern_ms: f64) -> (f64, f64) {
    let mut per_band = [0.0f64; 2];
    for (i, band) in per_band.iter_mut().enumerate() {
        let s1 = wrap_sym(equiv_phases(mid)[i] - equiv_phases(b1)[i], pattern_ms);
        let s2 = wrap_sym(equiv_phases(mid)[i] - equiv_phases(b2)[i], pattern_ms);
        *band = (s1 + s2) / 2.0;
    }
    ((per_band[0] + per_band[1]) / 2.0, (per_band[0] - per_band[1]).abs())
}

#[allow(clippy::too_many_arguments)] // one arm's worth of context
pub(crate) fn equiv_arm(
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
pub(crate) fn equiv_compare(
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
pub(crate) const EQUIV_CANNOT_TELL: [&str; 6] = [
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
/// moving. Registered in `api/measure.rs`, which owns the router.
#[allow(dead_code)] // the route belongs to api/measure.rs
pub async fn equivalence_ws(ws: axum::extract::ws::WebSocketUpgrade) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| {
        let m = equivalence();
        status_socket(socket, m.subscribe(), || Box::pin(async { serde_json::to_string(&m.status()).ok() }))
    })
}
