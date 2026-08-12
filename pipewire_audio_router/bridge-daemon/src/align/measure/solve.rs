//! Observations in, proposal out — the arithmetic of a run, with no state and no
//! I/O.
//!
//! [`fit_drift`] separates a clock difference common to every member from the
//! per-member arrival offsets that are the actual measurement. [`transitivity`] and
//! [`repeatability`] are the two checks that can veto a write: the first hunts an
//! early reflection biasing one speaker by comparing across frequency bands, the
//! second catches a phone that moved between passes. [`solve`] assembles a
//! [`Proposal`], and [`residual`] states how well the chosen target was actually met.
//!
//! Every refusal here names the two members that could not be reconciled, because
//! "no solution" is not something a user can act on.

use super::*;

// ---------------------------------------------------------------- solve (§9)

/// Wrap a difference into ±half a period.
pub(crate) fn wrap_sym(d: f64, period: f64) -> f64 {
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
pub(crate) struct Arrivals {
    pub(crate) fit: DriftFit,
    /// Earliest member at 0, in `order`.
    pub(crate) linear: Vec<(String, f64)>,
    pub(crate) spread_ms: f64,
    /// Where the reported per-member drift corrections are quoted from: the earliest
    /// reading in the set. A common shift cancels in every difference, so the choice is
    /// presentational — but it has to be *stated*, because "0.4 ms of drift" means
    /// nothing without saying since when.
    pub(crate) drift_origin: f64,
}

pub(crate) fn arrivals_of(observations: &[MemberObservation], order: &[String], timing: &Timing) -> Result<Arrivals, Refusal> {
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
    pub(crate) fn drift_correction(&self, observations: &[MemberObservation], name: &str) -> f64 {
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
pub(crate) fn worst_std_errors(observations: &[MemberObservation]) -> HashMap<String, f64> {
    let mut m: HashMap<String, f64> = HashMap::new();
    for o in observations {
        let e = m.entry(o.node_name.clone()).or_insert(0.0);
        *e = e.max(o.m.std_error_ms);
    }
    m
}

/// The knob half of a proposal: turn a solved target into per-member writes, name the
/// member left at the smallest knob, and refuse a value the rounding pushed out of range.
pub(crate) struct ProposedKnobs {
    pub(crate) members: Vec<ProposedDelay>,
    pub(crate) reference: String,
    pub(crate) largest_knob_ms: u16,
    /// Only the [`KnobPolarity::Advance`] members, before and after — the two sets the
    /// §9.2 high-water check compares.
    pub(crate) current_advances: HashMap<String, u16>,
    pub(crate) proposed_advances: HashMap<String, u16>,
}

pub(crate) fn propose_knobs(
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
pub(crate) fn send_ahead_warning(
    ctx: &SendAheadContext,
    current: &HashMap<String, u16>,
    proposed: &HashMap<String, u16>,
) -> Option<Warning> {
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
