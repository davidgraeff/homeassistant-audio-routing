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
    /// **The capture carries no signal at all** — not a quiet one, an absent one.
    /// Its own verdict rather than a flavour of [`Self::TooQuiet`] because the
    /// remedies are disjoint: "turn it up" is the answer to too quiet and the wrong
    /// answer here, where nothing is playing on the soloed speaker, the input is
    /// muted, or the device is gating its own silence to exact zeros.
    Silent,
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
    /// Peak level of the analysed window in dBFS, or `None` when no window was
    /// analysed. Reported for every verdict, because it is the one number that
    /// separates "the microphone hears nothing" from "the estimator did not find the
    /// pattern in what it heard" — and a refusal that quotes no number leaves the
    /// user guessing which of the two they are looking at.
    pub capture_peak_dbfs: Option<f64>,
    /// Clipped samples in the capture's recent window
    /// ([`crate::align::mic::MicStatus::recent_clip_count`]), which is wider than the
    /// analysed window; see [`with_recent_clipping`].
    pub recent_clip_count: u64,
    pub channels: Vec<SignalChannel>,
}

impl SignalCheck {
    pub(crate) fn unusable(message: impl Into<String>, sample_rate: u32) -> Self {
        Self::refused(SignalVerdict::Unusable, message, sample_rate, None)
    }

    /// A refusal that carries the level it was refusing, for the branches that
    /// analysed a window and can therefore quote one.
    fn refused(verdict: SignalVerdict, message: impl Into<String>, sample_rate: u32, peak_dbfs: Option<f64>) -> Self {
        Self {
            verdict,
            message: message.into(),
            sample_rate,
            periods: 0,
            gap: false,
            clipped: false,
            worst_peak_snr_db: None,
            capture_peak_dbfs: peak_dbfs,
            recent_clip_count: 0,
            channels: Vec::new(),
        }
    }
}

/// The analysed window's peak as dBFS, or `None` for a window with no signal at all
/// (log of zero). Full scale is 0 dBFS.
fn peak_dbfs(peak: f32) -> Option<f64> {
    (peak > 0.0).then(|| 20.0 * f64::from(peak).log10())
}

/// How the peak reads in a sentence, including the case where there is nothing to
/// report a number for.
fn peak_note(peak: f32) -> String {
    match peak_dbfs(peak) {
        Some(db) => format!("peak {db:.0} dBFS"),
        None => "every sample is exactly zero".to_string(),
    }
}

/// The same for a **constant** window, where the peak alone would read as a
/// contradiction ("delivering silence — peak −6 dBFS"): what matters is that nothing
/// in it varies, which is a stuck input rather than a quiet room.
fn flat_note(peak: f32) -> String {
    match peak_dbfs(peak) {
        Some(db) => format!("not one sample varies from the last, at a level of {db:.0} dBFS — a stuck input"),
        None => "every sample is exactly zero".to_string(),
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
        // The window verdict, then the capture's own recent clipping over it — see
        // `with_recent_clipping` for why the wider span decides.
        Some(w) => with_recent_clipping(signal_check_window(&w, pattern_ms), status.recent_clip_count, status.clip_window_secs),
        // The only branch that is *genuinely* still collecting: the capture has not
        // delivered the two periods this analyses yet. Every refusal past this point
        // holds a full window, so telling the user to wait would be a lie — which is
        // exactly what "no complete test-pattern period has been captured yet" was
        // doing (2026-08-13: it stood for minutes on a capture that was full, gapless
        // and carrying nothing).
        None => SignalCheck::unusable(
            format!(
                "Still collecting audio — {PREFLIGHT_PERIODS} pattern periods ({:.0} s) are needed before the level can be judged.",
                pattern_ms / 1000.0 * PREFLIGHT_PERIODS as f64
            ),
            rate,
        ),
    }
}

/// Fold the capture's **recent** clipping into a window verdict (plan §7: one clipped
/// block is broadband, so it corrupts every measurement channel at once).
///
/// [`signal_check_window`] can only see rail samples inside the window it analysed, so
/// a capture that clips on every click flapped between three different diagnoses —
/// "clipping", "no complete period", and even "good" — depending on where the 4 s
/// window happened to land between polls. Observed on hardware 2026-08-13, and it sent
/// the user after the wrong thing twice. The mic's own window
/// ([`crate::align::mic::CLIP_WINDOW_SECS`]) is wider than the analysed one on purpose,
/// so this covers the gap between polls.
///
/// Clipping outranks a *level* verdict because no level read off a clipped capture
/// means anything. It does not outrank [`SignalVerdict::Silent`] or a gap: those say
/// the reading is not about the level at all, and both are more specific than "it
/// clipped a few seconds ago".
pub(crate) fn with_recent_clipping(check: SignalCheck, recent_clips: u64, window_secs: u32) -> SignalCheck {
    let check = SignalCheck { recent_clip_count: recent_clips, ..check };
    if recent_clips == 0 || check.gap || check.verdict == SignalVerdict::Silent {
        return check;
    }
    SignalCheck {
        verdict: SignalVerdict::Unusable,
        clipped: true,
        message: format!(
            "The capture is clipping — {recent_clips} sample(s) hit full scale in the last {window_secs} s. \
             Turn the playback level down; no amount of loudness fixes a clipped microphone."
        ),
        ..check
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
    // Level facts about the window itself, in the sample domain. Two things need
    // them: the report (a refusal that quotes no number is not actionable), and the
    // branch below that has to tell "nothing is playing" from "the pattern did not
    // close". `flat` is the exact condition the estimator's candidate search fails on
    // — a constant signal has no strict local maximum anywhere, whatever its value,
    // so digital silence and a stuck DC offset both land there.
    let peak = w.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let flat = w.samples.first().is_some_and(|first| w.samples.iter().all(|s| s == first));
    let window_secs = w.samples.len() as f64 / f64::from(w.sample_rate.max(1));

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
    //
    // **What this branch actually means, corrected 2026-08-13.** `PeriodAcc::close`
    // drops a period for one of *two* reasons — it was not covered end to end (the
    // grid case above), or it "contained no local maximum at all (mathematically dead
    // silence)". The second is overwhelmingly the common one in the field, and it is
    // not a transient: a capture delivering exact zeros — nothing playing on the
    // soloed speaker, a muted input, or a device that gates its own silence — stays
    // here for as long as the user is willing to stare at it. Reporting both as
    // "still collecting audio" told them to wait for something that could not arrive.
    // So the level of the window decides which sentence they get, and the sentence
    // carries the number.
    if estimate.periods_seen == 0 {
        let (verdict, message) = if flat {
            (
                SignalVerdict::Silent,
                format!(
                    "The microphone is delivering no signal — {} over the whole {:.0} s window, so there is no click in it to \
                     measure. Check that a speaker is soloed and that it is one you can hear from where the microphone is, and \
                     that the input is not muted or gating its own silence.",
                    flat_note(peak),
                    window_secs
                ),
            )
        } else {
            (
                SignalVerdict::Unusable,
                format!(
                    "Audio is arriving ({}) but no complete test-pattern period closed in the last {:.0} s. If that persists, \
                     what the microphone hears is not this click track — check which speaker is soloed.",
                    peak_note(peak),
                    window_secs
                ),
            )
        };
        return SignalCheck::refused(verdict, message, w.sample_rate, peak_dbfs(peak));
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
        capture_peak_dbfs: peak_dbfs(peak),
        recent_clip_count: 0,
        channels,
    }
}
