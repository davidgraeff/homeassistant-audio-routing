//! The pre-flight signal check: is the microphone hearing the test pattern well
//! enough for a run to be worth starting?
//!
//! Grades a short capture — shorter than the gate's window, because this answers
//! "point the phone better" while the user is still holding it — on level, clipping
//! and whether a complete pattern period even arrived. Level is graded by the
//! *worst* channel: a phone lying face-down has one usable mic and one against the
//! table, and averaging hides exactly that.
//!
//! A verdict here refuses before a run spends a device reconnect, which is the
//! expensive thing about starting one at all.

use super::*;

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
pub(crate) const PREFLIGHT_PERIODS: usize = 2;

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
    pub(crate) fn unusable(message: impl Into<String>, sample_rate: u32) -> Self {
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
