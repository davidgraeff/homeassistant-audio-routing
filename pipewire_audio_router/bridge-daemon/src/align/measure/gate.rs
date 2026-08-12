//! The lock gate: deciding when a microphone window is a measurement rather than a
//! guess.
//!
//! [`Gate`] watches successive windows and locks only once the estimator agrees
//! across enough consecutive stable periods, so a single lucky window cannot
//! propose a delay. When it will not lock it says *why* in the user's terms
//! ([`GateReason`]) — too quiet, clipped, a gap in the capture, an unstable
//! amplitude, the AEC's monotonic decay signature, interference from the
//! announcement, a phone that moved — because "it did not lock" is not something a
//! user can act on.
//!
//! One entry point, [`Gate::observe`], and no I/O: the caller feeds it windows.

use super::*;

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

    pub(crate) fn progress(&self, locked: bool, periods: usize) -> GateProgress {
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

    pub(crate) fn restart(&mut self, reason: GateReason, message: impl Into<String>, elapsed: Duration) -> GateStep {
        self.peaks.clear();
        self.restarts += 1;
        self.last = Some((reason, message.into()));
        let progress = self.progress(false, 0);
        GateStep { locked: false, restart: true, failed: self.timeout(elapsed, reason), progress }
    }

    pub(crate) fn waiting(&mut self, reason: GateReason, message: impl Into<String>, periods: usize, elapsed: Duration) -> GateStep {
        self.last = Some((reason, message.into()));
        let progress = self.progress(false, periods);
        GateStep { locked: false, restart: false, failed: self.timeout(elapsed, reason), progress }
    }

    /// The timeout is checked on every non-locked sample, and reports the reason
    /// the gate was *last* waiting on rather than a bare "timed out".
    pub(crate) fn timeout(&self, elapsed: Duration, reason: GateReason) -> Option<Refusal> {
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
pub(crate) fn amplitude_spread_db(peaks: &[f32]) -> Option<f64> {
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
pub(crate) fn monotonic_decay_db(peaks: &[f32]) -> Option<f64> {
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
