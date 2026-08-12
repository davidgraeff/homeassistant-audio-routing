//! The interval model: which knob each member exposes, and the one target value
//! that satisfies all of them at once.
//!
//! Each member's transport gives it a different knob — a sendspin device takes a
//! *send-ahead advance*, an AP2 receiver a *render delay*, a pw-sink target a
//! *playout delay* — with different signs, floors and ceilings. [`knob_of`] states
//! which, [`MemberInterval`] turns a member's measured arrival plus its knob into
//! the range of group arrival times it can reach, and [`choose_target`] finds the
//! one time every member can reach, or a [`Refusal`] naming the two that cannot
//! meet.
//!
//! Pure arithmetic: no session, no I/O, no state. The sign conventions live here
//! and nowhere else, which is the point — an advance and a delay move the sound in
//! opposite directions, and that is exactly the kind of mistake a reader should be
//! able to check in one file.

use super::*;

/// Largest sendspin advance the API accepts (`api/measure.rs`'s `delay_ms.min(5000)`,
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
    pub(crate) fn noun(self) -> &'static str {
        match self {
            KnobPolarity::Advance => "advance",
            KnobPolarity::Delay => "delay",
        }
    }

    /// Which way raising the knob moves the sound.
    pub(crate) fn direction(self) -> &'static str {
        match self {
            KnobPolarity::Advance => "earlier",
            KnobPolarity::Delay => "later",
        }
    }

    pub(crate) fn opposite(self) -> Self {
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
        // will accept (`routing::sync_settings::PWSINK_JITTER_MIN_MS`).
        MemberKind::PwSink => Knob {
            polarity: KnobPolarity::Delay,
            min_ms: crate::routing::sync_settings::PWSINK_JITTER_MIN_MS,
            max_ms: crate::routing::sync_settings::PWSINK_JITTER_MAX_MS,
        },
    }
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
    pub(crate) fn new(node_name: String, kind: MemberKind, current_ms: u16, arrival_ms: f64) -> Self {
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
    pub(crate) fn knob_for(&self, target: f64) -> f64 {
        match self.knob.polarity {
            KnobPolarity::Advance => self.base_ms - target,
            KnobPolarity::Delay => target - self.base_ms,
        }
    }

    /// "advance 12 ms (was 0 ms) — 12 ms earlier than it plays now".
    pub(crate) fn describe(&self, new_ms: u16) -> String {
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
pub(crate) fn intervals_for(
    arrivals: &[(String, f64)],
    members: &[SessionMember],
    current_delays: &HashMap<String, u16>,
) -> Vec<MemberInterval> {
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
pub(crate) fn infeasible(floor: &MemberInterval, ceiling: &MemberInterval) -> Refusal {
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
