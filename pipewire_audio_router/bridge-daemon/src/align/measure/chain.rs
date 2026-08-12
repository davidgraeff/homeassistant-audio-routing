//! Multi-position chaining (W12): one measurement per listening spot, joined into
//! one set of delays.
//!
//! A single position aligns what the microphone can hear from there. Chaining walks
//! the room: each position is measured on its own, then joined to the previous one
//! through the members they **share** — [`chain_step`] renormalises the new
//! position's arrivals onto the running set through that overlap, and refuses when
//! the overlap's members disagree, because a bad joint would silently move every
//! speaker measured so far.
//!
//! [`chain_error`] is the accumulated uncertainty: it is the joints, not the
//! individual measurements, and it is withheld rather than guessed when a joint
//! cannot be measured. A chain writes the real knobs exactly once, at the end
//! ([`apply_provisional`] holds the intermediate delays in the relay).

use super::*;

/// Plan §1.1's honesty clause, stated on every chained run rather than inferred from
/// the numbers.
pub(crate) const CHAIN_SCOPE_NOTE: &str =
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
pub(crate) fn chain_note(
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
pub(crate) async fn run_chain(
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
pub(crate) struct AcceptedStep {
    step: ChainStep,
    /// The new provisional delay for every member this position touches — the step's own
    /// speakers, plus the whole already-aligned set when Δ > 0.
    provisional: HashMap<String, f64>,
    observations: Vec<MemberObservation>,
}

#[allow(clippy::too_many_arguments)] // one position's worth of context; a struct would only move the list
pub(crate) fn chain_solve_step(
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
pub(crate) fn chain_step_retryable(kind: RefusalKind) -> bool {
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
pub(crate) async fn apply_provisional(
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
pub(crate) fn chain_progress(
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

pub(crate) fn set_chain(inner: &Arc<Mutex<Inner>>, chain: ChainProgress) {
    let mut g = inner.lock_recover();
    g.chain = Some(chain);
    g.bump();
}

/// The sentence the user reads while the chain waits for them to move.
pub(crate) fn chain_prompt(next: ChainAction, remaining: &[String], done: usize) -> String {
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
