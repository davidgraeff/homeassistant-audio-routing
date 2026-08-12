//! The near-field walk (W8a): measure each speaker with the microphone at the
//! speaker, not at the listening position.
//!
//! The daemon cannot see where the phone is, so the walk is driven by the user:
//! stand at a speaker, measure, move to the next. That buys arrivals with almost no
//! room in them — the wire delay rather than the wire plus a reflection — at the cost
//! of a clock that drifts across the walk. [`run_walk`] closes the loop by returning
//! to the first speaker, which is what makes the drift measurable and removable, and
//! refuses the whole walk when the closure is implausible.
//!
//! The prompts are part of the mechanism, not decoration: a walk is a sequence of
//! instructions to a person, and a wrong one produces a wrong measurement.

use super::*;

/// Near field states its own scope rather than letting the user assume the flattering
/// reading. One walk is internally coherent and nothing more.
pub(crate) const WALK_SCOPE_NOTE: &str =
    "the speakers in this walk are aligned to each other, and to nothing else: this result is not related to any \
     set aligned in an earlier session, even where the two share a speaker. Linking two walks through a shared speaker is not \
     implemented (it needs the multi-position chaining machinery), so anything that must sound coherent has to be walked in one \
     session.";

/// Plan §12.2: near field folds the level into each arrival, so there is no level
/// *learning* phase to skip — the level is only meaningful at the speaker, and the
/// risk there inverts from too-quiet to clipping.
pub(crate) const WALK_LEVEL_NOTE: &str =
    "each speaker is measured at the level set for it when you arrived (POST /api/align/audible while you stand \
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
pub(crate) async fn run_walk(
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
pub(crate) fn set_walk(inner: &Arc<Mutex<Inner>>, walk: WalkProgress) {
    let mut g = inner.lock_recover();
    g.walk = Some(walk);
    g.bump();
}

/// The sentence the user reads while the walk waits for them.
pub(crate) fn walk_prompt(purpose: WalkPurpose, next: WalkAction, measured: &[String], remaining: &[String], restarts: u32) -> String {
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
pub(crate) fn closure_prompt(c: &ClosureReport) -> String {
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
