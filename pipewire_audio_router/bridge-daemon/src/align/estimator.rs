//! Arrival-time estimator for microphone-assisted speaker alignment
//! (docs/mic-alignment-plan.md §5).
//!
//! Consumes a mono PCM stream captured by the user's phone and returns, per
//! measurement *channel*, the arrival phase of that channel's tone burst within
//! the calibration pattern — in milliseconds, with an uncertainty.
//!
//! Everything here is a **relative** time inside one continuous microphone
//! stream: the estimator never learns when a burst was emitted, only where its
//! arrivals sit relative to each other and to the pattern period (plan §3). That
//! is what removes any need to synchronise the phone's clock with the daemon.
//!
//! Pure DSP, no I/O and no daemon state — so it is fully testable against
//! synthetic signals, which is how the accuracy claims below are held honest.
//!
//! ## The grid origin is arbitrary — this is not a bug
//!
//! Arrivals are reduced to a phase inside the pattern period using
//! `frame_index mod period_frames`, where `frame_index` is whatever index the
//! ingest hands us (`align_mic::MicWindow::first_frame` — frames since the phone
//! connected). The estimator therefore does **not** know, and does not need to
//! know, where the calibration pattern actually starts in the mic stream: the
//! grid is offset from the true pattern start by some unknown constant.
//!
//! That constant is common to every channel and every period, so it cancels in
//! every quantity anyone consumes: channel-to-channel differences
//! ([`Estimate::delta_ms`]) and the drift slope. Reading a single
//! [`ChannelEstimate::phase_ms`] as "the time the burst was emitted" is the only
//! way to get this wrong; it is a phase on an arbitrary-but-consistent grid.
//!
//! ## Detector
//!
//! Per channel: complex heterodyne to baseband, **two cascaded boxcar
//! integrators** of `L/2` frames each (`L` = the burst length in frames) →
//! magnitude envelope → local maxima → parabolic interpolation on the three
//! envelope samples around the winner. Heterodyne + boxcar is algebraically the
//! same single-bin sliding DFT as a Goertzel (plan §5.2), written in the
//! numerically safe direction:
//!
//! * A boxcar running sum is unconditionally stable and **exactly linear
//!   phase**, so its group delay is the same constant for *every* channel
//!   regardless of centre frequency. A resonator (Goertzel proper) or a one-pole
//!   lowpass has a frequency-dependent, asymmetric delay, which is a
//!   *differential* bias between channels — exactly the error this module exists
//!   to measure. Plan §6.2 worries about this for the per-device gating filters;
//!   it applies to the analysis filter just as much.
//!
//! Why *two* stages rather than the obvious one boxcar of `L` frames matched to
//! the burst — this was measured, not assumed:
//!
//! * A single length-`L` boxcar is the matched filter, and its correlation peak
//!   against a Hann burst is *quartically* flat (the Hann window's edges
//!   contribute almost nothing as the window slides off), so the peak position
//!   is very badly conditioned.
//! * On top of that flat plateau sits a ripple at twice the centre frequency —
//!   the negative-frequency image, which only cancels while the window is fully
//!   inside the burst. The ripple crests become local maxima, and the picked peak
//!   *hops between crests* under noise: measured per-period spread of ±0.5 ms
//!   and a 0.15 ms bias in the A↔B difference at a modest SNR.
//! * Cascading two `L/2` boxcars is a triangular window of the same total span.
//!   Its stopband is the boxcar's squared, which buries the image ripple, and its
//!   correlation peak has real curvature. Same measurement: per-period spread
//!   ±0.04 ms, difference error 0.013 ms — a 20× improvement for one extra
//!   running sum per sample.
//!
//! Cost is ~15 flops + one `sqrt` per sample per channel, and two f64 delay lines
//! of `L/2` complex samples (~6 kB at 48 kHz) — it runs comfortably on a tokio
//! task, off the RT threads.
//!
//! ## Known blind spot (feeds the W9 decision)
//!
//! A reflection arriving **within the analysis window** of the direct sound
//! merges into one peak, and the merged peak's position is pulled towards the
//! reflection. The refusal rules cannot see this: the peak is single (so the
//! second-peak ratio is large), strong (so the SNR is high) and *deterministic*
//! (so the standard error is tiny — it is the same wrong answer every period).
//! Measured with a 0.9× reflection at +1…+5 ms: a silent 0.4–1.7 ms bias,
//! reported as `Accepted`. With the reflection louder than the direct sound the
//! estimator locks onto the reflection outright (+5 ms). Diffuse reverb is
//! benign by comparison (a full decaying tail cost 0.14 ms), and a reflection
//! *outside* the window is caught properly as
//! [`RejectReason::AmbiguousPeak`].
//!
//! This is the one failure this signal design cannot detect, and it is the
//! argument for plan §5.1/W9 (a 200–300 ms chirp with a matched filter has the
//! resolution to separate arrivals a few milliseconds apart). Until then, plan
//! §10's transitivity check is the cross-check that can expose it, because a
//! per-speaker bias breaks transitivity while a shared one does not.
//!
//! ## Aggregation
//!
//! Per pattern period the estimator keeps one refined peak position, a peak SNR
//! (peak vs. the median envelope away from the peak) and the ratio of the largest
//! to the second-largest well-separated peak. Across periods it takes a
//! **circular** mean (phases wrap at the period, an arithmetic mean of 1.999 s
//! and 0.001 s is meaningless) and then fits a straight line against the period
//! index. The intercept is the phase estimate; the **slope is mic-vs-audio clock
//! drift**, reported in ppm because a long session (near-field walking, plan §1)
//! needs it both as a health signal and as a correction.
//!
//! ## Refusing
//!
//! Writing a delay from a bad measurement silently degrades a system the user had
//! aligned by ear, so the estimator must be willing to say no (plan §5.5). Every
//! rejection carries a machine-readable [`RejectReason`] *and* a sentence the UI
//! can show; see the threshold constants below for what each number means.
//!
//! ## Handoff
//!
//! W1 (`align_mic`) produces contiguous mono `f32` blocks with the frame index of
//! the first sample; feed them straight in:
//!
//! ```ignore
//! let mut est = Estimator::new(EstimatorConfig::click_track(w.sample_rate))?;
//! if w.gap { est.note_gap(); }
//! est.push_block(w.first_frame, &w.samples);
//! let report = est.estimate();
//! ```
//!
//! No dependency in either direction: this module is pure DSP and knows nothing
//! about sockets, and the ingest knows nothing about filters.

#![allow(dead_code)] // consumed by the orchestration in W3 (plan §14); unit-tested now.

use serde::Serialize;
use std::collections::VecDeque;
use std::f64::consts::TAU;

/// Full pattern period. Mirrors `calibrate::PATTERN_SECS` — the click track is
/// one A burst and one B burst per 2 s loop.
pub const PATTERN_SECS: f64 = 2.0;
/// Burst length, mirroring `calibrate::CLICK_MS`. Also the integrator length, so
/// changing it changes the detector's resolution: an 8 ms Hann burst has ~250 Hz
/// null-to-null main lobe and gives sub-millisecond peak positions (plan §5.1).
pub const BURST_MS: f64 = 8.0;
/// Centre frequency of the existing click track's "A" burst
/// (`calibrate::FREQ_A`).
pub const CLICK_A_HZ: f64 = 3000.0;
/// Centre frequency of the existing click track's "B" burst
/// (`calibrate::FREQ_B`).
pub const CLICK_B_HZ: f64 = 1500.0;
/// Channel label for the A burst.
pub const CLICK_A_LABEL: &str = "a";
/// Channel label for the B burst.
pub const CLICK_B_LABEL: &str = "b";

/// Plan §5.5: below this the peak is not reliably the direct arrival, and it is
/// also the level-learning phase's target (plan §7), so a channel under it means
/// "the learning phase failed", not "the room is hopeless".
///
/// Measured against synthetic noise, this threshold is well placed: at a
/// reported peak SNR of 15 dB the A↔B difference is still good to 0.15 ms, and
/// 3 dB below it the estimator collapses outright (tens of ms). The margin is on
/// the right side of the cliff.
pub const MIN_PEAK_SNR_DB: f64 = 15.0;
/// Plan §5.5: the largest peak must stand clear of the runner-up, or we cannot
/// tell which arrival is the direct sound. 1.4× ≈ 3 dB — enough to survive a
/// strong single reflection, tight enough to catch a genuine two-peak ambiguity
/// (a whole-period slip, or two speakers still audible in one channel).
pub const MIN_SECOND_PEAK_RATIO: f64 = 1.4;
/// Plan §5.5: the delay knobs are integer milliseconds (plan §2.4), so an
/// estimate whose own standard error exceeds 1 ms cannot inform the write. In
/// practice this is what catches the user moving the phone mid-measurement.
pub const MAX_STD_ERROR_MS: f64 = 1.0;
/// Fewer than three usable periods and there is nothing to separate noise from
/// drift: the line fit needs three points to have any residual at all.
pub const MIN_PERIODS_USED: usize = 3;
/// A period whose peak barely rises above its *own* floor did not contain a
/// detectable burst — the burst straddled the grid boundary, or a mute had not
/// settled yet. Such periods are dropped from the fit (they would contribute
/// pure noise) but still count towards the reported SNR, so a capture that is
/// *entirely* like this is rejected as [`RejectReason::LowSnr`] rather than
/// disappearing as "too few periods". Deliberately below
/// [`MIN_PEAK_SNR_DB`]: this gate exists to discard non-detections, not to
/// enforce quality.
pub const PERIOD_SNR_GATE_DB: f64 = 10.0;

/// Each integrator stage is `burst_frames / INTEG_DIVISOR` long, so the cascade
/// spans roughly the burst. See the module docs for why the length matters.
const INTEG_DIVISOR: usize = 2;

/// How many pattern periods of history the estimator keeps per channel. 512
/// periods is ~17 min at a 2 s pattern — longer than the session timeout — and
/// costs a few tens of kB. Older periods are dropped so a forgotten session
/// cannot grow without bound.
const MAX_RETAINED_PERIODS: usize = 512;
/// Local maxima tracked per period. Only the best and the best well-separated
/// runner-up are consumed; a handful of slots is enough for those two to survive
/// the noise peaks competing for space.
const MAX_CANDIDATES: usize = 8;
/// Envelope samples kept per period to estimate the noise floor by median.
/// Decimating to ~2 k samples bounds the per-period cost at a few tens of kB and
/// a 2 k-element sort, and the floor is a broad statistic — it does not need
/// every sample.
const FLOOR_SAMPLES: u64 = 2048;
/// Reported SNR is capped here: a synthetic (or genuinely silent) capture has a
/// floor of exactly zero, and `inf` does not serialise to JSON.
const MAX_SNR_DB: f64 = 120.0;
/// Same idea for the peak ratio when there is no second peak at all.
const MAX_PEAK_RATIO: f64 = 1000.0;

/// One measurement channel: a label the caller can map back to a group member,
/// and the centre frequency of that member's burst.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelSpec {
    pub label: String,
    pub center_hz: f64,
}

impl ChannelSpec {
    pub fn new(label: impl Into<String>, center_hz: f64) -> Self {
        Self { label: label.into(), center_hz }
    }
}

/// Everything the estimator needs to know about the signal it is looking for.
#[derive(Debug, Clone, Serialize)]
pub struct EstimatorConfig {
    /// Capture rate. 48 kHz and 44.1 kHz are both real (iOS gives 44.1).
    pub sample_rate: u32,
    pub pattern_secs: f64,
    pub burst_ms: f64,
    pub channels: Vec<ChannelSpec>,
}

impl EstimatorConfig {
    /// The two channels of the *existing* click track (`calibrate::click_wav`):
    /// A at 3 kHz, B at 1.5 kHz. Note that today both speakers emit both bursts
    /// (plan §2.2), so these two channels are a *frequency* split, not a
    /// per-speaker split — useful for validating the estimator and for the
    /// merged-peak check (plan §10.3). Per-member channels arrive with the
    /// parallel excitation in W7.
    pub fn click_track(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            pattern_secs: PATTERN_SECS,
            burst_ms: BURST_MS,
            channels: vec![ChannelSpec::new(CLICK_A_LABEL, CLICK_A_HZ), ChannelSpec::new(CLICK_B_LABEL, CLICK_B_HZ)],
        }
    }
}

/// Why an estimate must not be used. Machine-readable half of the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Peak SNR below [`MIN_PEAK_SNR_DB`].
    LowSnr,
    /// Second-largest peak too close to the largest ([`MIN_SECOND_PEAK_RATIO`]).
    AmbiguousPeak,
    /// Standard error across periods above [`MAX_STD_ERROR_MS`].
    UnstablePhase,
    /// A sample at or beyond full scale reached the estimator.
    Clipped,
    /// The caller reported a dropped chunk ([`Estimator::note_gap`]).
    SequenceGap,
    /// Fewer than [`MIN_PERIODS_USED`] usable pattern periods.
    TooFewPeriods,
}

/// Whether an estimate may be used, and if not, why — in both machine and human
/// form (plan §5.5: "it didn't work" is not an acceptable thing to show a user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Quality {
    Accepted,
    Rejected { reason: RejectReason, message: String },
}

impl Quality {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Quality::Accepted)
    }

    pub fn reason(&self) -> Option<RejectReason> {
        match self {
            Quality::Accepted => None,
            Quality::Rejected { reason, .. } => Some(*reason),
        }
    }

    fn reject(reason: RejectReason, message: impl Into<String>) -> Self {
        Quality::Rejected { reason, message: message.into() }
    }
}

/// One channel's result.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelEstimate {
    pub label: String,
    pub center_hz: f64,
    /// Arrival phase inside the pattern period, in ms, on the estimator's
    /// arbitrary-but-consistent grid (see the module docs). Only differences
    /// between channels are meaningful.
    pub phase_ms: f64,
    /// Standard error of `phase_ms` from the spread of the per-period arrivals
    /// around the fitted line (so drift is removed before the spread is
    /// measured).
    pub std_error_ms: f64,
    /// Peak envelope over the median envelope away from the peak, in dB; median
    /// across periods.
    pub peak_snr_db: f64,
    /// Largest peak ÷ largest *well-separated* other peak; median across
    /// periods. 1.0 means "cannot tell which arrival is the direct sound".
    pub second_peak_ratio: f64,
    /// Mic-vs-audio clock drift from the slope of phase against period index.
    /// Positive = the mic clock runs fast (the pattern appears longer than
    /// nominal in mic frames). Diagnostic, and the correction a long session
    /// needs (plan §3).
    pub drift_ppm: f64,
    /// Periods that went into the fit.
    pub periods_used: usize,
    pub quality: Quality,
}

/// The estimator's output for one measurement window.
#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub sample_rate: u32,
    pub pattern_ms: f64,
    pub channels: Vec<ChannelEstimate>,
    /// Complete pattern periods observed (the partial first and in-progress last
    /// period are not counted).
    pub periods_seen: usize,
    /// Samples at or beyond full scale since the last [`Estimator::reset`].
    pub clipped_samples: u64,
    /// Sequence gaps reported or detected since the last [`Estimator::reset`].
    pub gaps: u32,
    /// The worst channel verdict — what the orchestration should branch on.
    pub quality: Quality,
}

impl Estimate {
    pub fn channel(&self, label: &str) -> Option<&ChannelEstimate> {
        self.channels.iter().find(|c| c.label == label)
    }

    pub fn accepted(&self) -> bool {
        self.quality.is_accepted()
    }

    /// `to`'s arrival minus `from`'s, in ms, wrapped into ±half a pattern period
    /// — i.e. "how much later than `from` does `to` arrive". This is the only
    /// quantity the arbitrary grid origin cancels out of, and the one the solver
    /// (W5) consumes. `None` if either label is unknown.
    pub fn delta_ms(&self, from: &str, to: &str) -> Option<f64> {
        let a = self.channel(from)?;
        let b = self.channel(to)?;
        let half = self.pattern_ms / 2.0;
        let mut d = b.phase_ms - a.phase_ms;
        while d > half {
            d -= self.pattern_ms;
        }
        while d < -half {
            d += self.pattern_ms;
        }
        Some(d)
    }
}

/// Frame-domain geometry derived once from the config.
struct Geom {
    rate: f64,
    period_frames: u64,
    burst_frames: usize,
    /// Length of each of the two cascaded integrator stages.
    integ_frames: usize,
    /// Frames from the newest sample back to the effective analysis window's
    /// centre, plus half a burst: subtracting it turns a peak position into the
    /// burst's onset.
    onset_offset: f64,
    /// Minimum separation for two peaks to count as distinct, and the radius
    /// excluded around the peak when measuring the noise floor. A single burst's
    /// envelope spans the burst plus the analysis window, so anything closer than
    /// that is the same arrival smeared, not a second one — and, per the module
    /// docs, a reflection inside it is a bias this module cannot detect.
    guard_frames: u64,
    /// Envelope decimation for the floor estimate.
    floor_step: u64,
}

/// One refined arrival, plus the quality numbers that come from its own period.
#[derive(Debug, Clone, Copy)]
struct PeriodObs {
    /// Absolute period index on the arbitrary grid.
    p: u64,
    /// Refined arrival, in frames, inside `[0, period_frames)`.
    phase: f64,
    snr_db: f64,
    ratio: f64,
    /// Did this period actually contain a detectable burst
    /// ([`PERIOD_SNR_GATE_DB`])?
    usable: bool,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    /// Offset inside the period, in frames.
    offset: u64,
    /// Envelope value at the local maximum, and its two neighbours (taken from
    /// the *continuous* envelope, so a peak next to a period boundary still gets
    /// both neighbours).
    value: f64,
    prev: f64,
    next: f64,
}

/// Accumulator for the period currently being observed.
struct PeriodAcc {
    p: u64,
    /// Envelope samples attributed to this period; a period is only usable once
    /// this reaches `period_frames` (which rules out the partial first period
    /// after warm-up and the in-progress last one).
    covered: u64,
    candidates: Vec<Candidate>,
    floor: Vec<(u64, f64)>,
}

impl PeriodAcc {
    fn new(p: u64) -> Self {
        Self { p, covered: 0, candidates: Vec::with_capacity(MAX_CANDIDATES), floor: Vec::new() }
    }

    fn offer(&mut self, c: Candidate, g: &Geom) {
        // Merge candidates that are the same arrival (within the guard), keeping
        // the taller one — otherwise the main lobe's own ripple would occupy
        // every slot and hide the real runner-up.
        if let Some(existing) = self.candidates.iter_mut().find(|e| circ_dist(e.offset, c.offset, g.period_frames) <= g.guard_frames) {
            if c.value > existing.value {
                *existing = c;
            }
            return;
        }
        if self.candidates.len() < MAX_CANDIDATES {
            self.candidates.push(c);
            return;
        }
        let (i, min) = self
            .candidates
            .iter()
            .enumerate()
            .fold((0usize, f64::INFINITY), |(bi, bv), (i, e)| if e.value < bv { (i, e.value) } else { (bi, bv) });
        if c.value > min {
            self.candidates[i] = c;
        }
    }

    /// Turn a complete period into an observation. `None` if the period is
    /// incomplete (warm-up / in progress) or contained no local maximum at all
    /// (mathematically dead silence).
    fn close(mut self, g: &Geom) -> Option<PeriodObs> {
        if self.covered < g.period_frames || self.candidates.is_empty() {
            return None;
        }
        self.candidates.sort_unstable_by(|a, b| b.value.total_cmp(&a.value));
        let peak = self.candidates[0];

        // Parabolic interpolation over the three envelope samples around the
        // peak: sub-sample resolution, ~4 flops (plan §5.2).
        let denom = peak.prev - 2.0 * peak.value + peak.next;
        let delta = if denom < 0.0 { (0.5 * (peak.prev - peak.next) / denom).clamp(-1.0, 1.0) } else { 0.0 };

        // The envelope peaks when the analysis window's centre sits on the
        // burst's centre, so the burst's onset is `onset_offset` frames earlier.
        // That shift is the same constant for every channel (the integrator is
        // linear phase), so it cancels in differences even if this reasoning
        // were off by a sample.
        let onset = peak.offset as f64 + delta - g.onset_offset;
        let phase = onset.rem_euclid(g.period_frames as f64);

        let second = self
            .candidates
            .iter()
            .skip(1)
            .find(|c| circ_dist(c.offset, peak.offset, g.period_frames) > g.guard_frames)
            .map_or(0.0, |c| c.value);
        let ratio = if second > 0.0 { (peak.value / second).min(MAX_PEAK_RATIO) } else { MAX_PEAK_RATIO };

        let mut floor: Vec<f64> =
            self.floor.iter().filter(|(o, _)| circ_dist(*o, peak.offset, g.period_frames) > g.guard_frames).map(|(_, v)| *v).collect();
        let med = median(&mut floor);
        let snr_db = if med > 0.0 && peak.value > 0.0 { (20.0 * (peak.value / med).log10()).min(MAX_SNR_DB) } else { MAX_SNR_DB };

        Some(PeriodObs { p: self.p, phase, snr_db, ratio, usable: snr_db >= PERIOD_SNR_GATE_DB })
    }
}

/// Per-channel filter state and observation history.
struct ChannelState {
    spec: ChannelSpec,
    /// Heterodyne oscillator, `e^{+jωn}`, re-anchored on every block's absolute
    /// frame index so it can never drift out of step with the frame grid.
    osc_re: f64,
    osc_im: f64,
    /// Complex baseband delay lines for the two cascaded boxcar stages.
    hist1_re: Vec<f64>,
    hist1_im: Vec<f64>,
    hist2_re: Vec<f64>,
    hist2_im: Vec<f64>,
    pos: usize,
    sum1_re: f64,
    sum1_im: f64,
    sum2_re: f64,
    sum2_im: f64,
    filled: usize,
    /// Envelope at `n`, `n-1`, `n-2` (`e0` newest).
    e0: f64,
    e1: f64,
    e2: f64,
    taps: u8,
    acc: Option<PeriodAcc>,
    obs: VecDeque<PeriodObs>,
}

impl ChannelState {
    fn new(spec: ChannelSpec, g: &Geom) -> Self {
        Self {
            spec,
            osc_re: 1.0,
            osc_im: 0.0,
            hist1_re: vec![0.0; g.integ_frames],
            hist1_im: vec![0.0; g.integ_frames],
            hist2_re: vec![0.0; g.integ_frames],
            hist2_im: vec![0.0; g.integ_frames],
            pos: 0,
            sum1_re: 0.0,
            sum1_im: 0.0,
            sum2_re: 0.0,
            sum2_im: 0.0,
            filled: 0,
            e0: 0.0,
            e1: 0.0,
            e2: 0.0,
            taps: 0,
            acc: None,
            obs: VecDeque::new(),
        }
    }

    /// Drop the filter state and the period in progress. Used on a gap: the
    /// integrator would otherwise smear samples from both sides of the
    /// discontinuity into one envelope value.
    fn discontinuity(&mut self) {
        for v in self.hist1_re.iter_mut().chain(&mut self.hist1_im).chain(&mut self.hist2_re).chain(&mut self.hist2_im) {
            *v = 0.0;
        }
        self.pos = 0;
        self.sum1_re = 0.0;
        self.sum1_im = 0.0;
        self.sum2_re = 0.0;
        self.sum2_im = 0.0;
        self.filled = 0;
        self.taps = 0;
        self.acc = None;
    }

    fn process(&mut self, first_frame: u64, samples: &[f32], g: &Geom) {
        let cycles_per_frame = self.spec.center_hz / g.rate;
        // Anchor the oscillator phase on the absolute frame index. `fract()` on
        // the product keeps ~1e-9 cycles of precision even after hours of
        // frames, and re-anchoring per block means the per-sample rotation below
        // can never accumulate error across a session.
        let phase = TAU * (cycles_per_frame * first_frame as f64).fract();
        self.osc_re = phase.cos();
        self.osc_im = phase.sin();
        let dw = TAU * cycles_per_frame;
        let (dre, dim) = (dw.cos(), dw.sin());
        let w = g.integ_frames;
        let norm = 1.0 / w as f64;

        for (k, &raw) in samples.iter().enumerate() {
            let n = first_frame + k as u64;
            // A non-finite sample would poison the running sum forever, so it is
            // treated as silence here; `push_block` has already counted it as
            // clipping, which rejects the window anyway.
            let x = if raw.is_finite() { f64::from(raw) } else { 0.0 };

            // Mix to baseband: z = x · conj(osc).
            let zr = x * self.osc_re;
            let zi = -x * self.osc_im;
            // Rotate, with one Newton step towards |osc| = 1 (3 flops) so the
            // rotation stays on the unit circle for arbitrarily long blocks.
            let (r, i) = (self.osc_re * dre - self.osc_im * dim, self.osc_re * dim + self.osc_im * dre);
            let corr = 1.5 - 0.5 * (r * r + i * i);
            self.osc_re = r * corr;
            self.osc_im = i * corr;

            // Two cascaded boxcar integrators: += new, -= the one leaving the
            // window, twice. Both stages run off one `pos` because they are the
            // same length.
            let old1_r = self.hist1_re[self.pos];
            let old1_i = self.hist1_im[self.pos];
            self.hist1_re[self.pos] = zr;
            self.hist1_im[self.pos] = zi;
            self.sum1_re += zr - old1_r;
            self.sum1_im += zi - old1_i;
            let y_r = self.sum1_re * norm;
            let y_i = self.sum1_im * norm;
            let old2_r = self.hist2_re[self.pos];
            let old2_i = self.hist2_im[self.pos];
            self.hist2_re[self.pos] = y_r;
            self.hist2_im[self.pos] = y_i;
            self.sum2_re += y_r - old2_r;
            self.sum2_im += y_i - old2_i;
            self.pos += 1;
            if self.pos == w {
                self.pos = 0;
            }
            if self.filled < 2 * w {
                self.filled += 1;
                if self.filled < 2 * w {
                    continue; // cascade not yet full — no envelope value here
                }
            }
            let env = (self.sum2_re * self.sum2_re + self.sum2_im * self.sum2_im).sqrt() * norm;

            self.e2 = self.e1;
            self.e1 = self.e0;
            self.e0 = env;
            if self.taps < 3 {
                self.taps += 1;
                if self.taps < 3 {
                    continue;
                }
            }
            // The middle tap is the one that can be a local maximum.
            self.observe(n - 1, self.e2, self.e1, self.e0, g);
        }
    }

    fn observe(&mut self, m: u64, prev: f64, mid: f64, next: f64, g: &Geom) {
        let p = m / g.period_frames;
        let offset = m % g.period_frames;
        if self.acc.as_ref().is_none_or(|a| a.p != p) {
            if let Some(done) = self.acc.take() {
                if let Some(o) = done.close(g) {
                    if self.obs.len() == MAX_RETAINED_PERIODS {
                        self.obs.pop_front();
                    }
                    self.obs.push_back(o);
                }
            }
            self.acc = Some(PeriodAcc::new(p));
        }
        let acc = self.acc.as_mut().expect("just set");
        acc.covered += 1;
        if m.is_multiple_of(g.floor_step) {
            acc.floor.push((offset, mid));
        }
        // Strict on the rising side: a constant run (digital silence) is not a
        // peak, and skipping it keeps the candidate scan off the hot path.
        if mid > prev && mid >= next {
            acc.offer(Candidate { offset, value: mid, prev, next }, g);
        }
    }
}

/// Streaming arrival-time estimator. Feed contiguous blocks, ask for an
/// [`Estimate`] whenever you like — it is a pure read of the accumulated
/// periods, so polling it costs nothing and does not disturb the stream.
pub struct Estimator {
    config: EstimatorConfig,
    geom: Geom,
    channels: Vec<ChannelState>,
    next_frame: Option<u64>,
    clipped_samples: u64,
    gaps: u32,
}

impl Estimator {
    pub fn new(config: EstimatorConfig) -> Result<Self, String> {
        if config.sample_rate < 8_000 {
            return Err(format!("sample rate {} is too low to measure an {BURST_MS} ms burst", config.sample_rate));
        }
        let rate = f64::from(config.sample_rate);
        if !(config.pattern_secs.is_finite() && config.pattern_secs > 0.0) {
            return Err(format!("pattern period {} s is not a positive duration", config.pattern_secs));
        }
        if !(config.burst_ms.is_finite() && config.burst_ms > 0.0) {
            return Err(format!("burst length {} ms is not a positive duration", config.burst_ms));
        }
        let burst_frames = (config.burst_ms / 1000.0 * rate).round() as usize;
        if burst_frames < 8 {
            return Err(format!("a {} ms burst is only {burst_frames} frames at {} Hz", config.burst_ms, config.sample_rate));
        }
        let period_frames = (config.pattern_secs * rate).round() as u64;
        if period_frames < 4 * burst_frames as u64 {
            return Err(format!("pattern period {} s is too short for a {} ms burst", config.pattern_secs, config.burst_ms));
        }
        if config.channels.is_empty() {
            return Err("at least one measurement channel is required".to_string());
        }
        for c in &config.channels {
            if !(c.center_hz.is_finite() && c.center_hz > 0.0) || c.center_hz > rate * 0.45 {
                return Err(format!("channel '{}' centre {} Hz is not measurable at {} Hz", c.label, c.center_hz, config.sample_rate));
            }
            if config.channels.iter().filter(|o| o.label == c.label).count() > 1 {
                return Err(format!("duplicate channel label '{}'", c.label));
            }
        }
        let integ_frames = (burst_frames / INTEG_DIVISOR).max(4);
        let geom = Geom {
            rate,
            period_frames,
            burst_frames,
            integ_frames,
            onset_offset: (integ_frames as f64 - 1.0) + burst_frames as f64 / 2.0,
            guard_frames: ((burst_frames + integ_frames) as u64).min(period_frames / 4),
            floor_step: (period_frames / FLOOR_SAMPLES).max(1),
        };
        let channels = config.channels.iter().cloned().map(|s| ChannelState::new(s, &geom)).collect();
        Ok(Self { config, geom, channels, next_frame: None, clipped_samples: 0, gaps: 0 })
    }

    pub fn config(&self) -> &EstimatorConfig {
        &self.config
    }

    /// Feed one contiguous block. `first_frame` is the running frame index of
    /// `samples[0]` in the capture stream; a value that does not continue the
    /// previous block is itself treated as a gap.
    pub fn push_block(&mut self, first_frame: u64, samples: &[f32]) {
        if self.next_frame.is_some_and(|next| next != first_frame) {
            self.note_gap();
        }
        self.next_frame = Some(first_frame + samples.len() as u64);
        for &s in samples {
            if !s.is_finite() || s.abs() >= 1.0 {
                self.clipped_samples += 1;
            }
        }
        for c in &mut self.channels {
            c.process(first_frame, samples, &self.geom);
        }
    }

    /// Tell the estimator a chunk was lost (plan §4.3: the ingest bumps the
    /// sequence number rather than buffering to catch up). The window in progress
    /// is discarded and the whole measurement is rejected with
    /// [`RejectReason::SequenceGap`] — the caller's move is to [`Self::reset`]
    /// and start a fresh window, which is exactly plan §3's "a gap invalidates
    /// the window in progress".
    ///
    /// Needed as an explicit call because the ingest's frame index counts
    /// *received* frames, so it stays contiguous across a gap and the
    /// discontinuity is not visible in `first_frame`.
    pub fn note_gap(&mut self) {
        self.gaps = self.gaps.saturating_add(1);
        for c in &mut self.channels {
            c.discontinuity();
        }
    }

    /// Drop all accumulated history, keeping the configuration. Start of a new
    /// measurement window.
    pub fn reset(&mut self) {
        for c in &mut self.channels {
            c.discontinuity();
            c.obs.clear();
        }
        self.next_frame = None;
        self.clipped_samples = 0;
        self.gaps = 0;
    }

    /// Complete pattern periods currently held (max across channels).
    pub fn periods_complete(&self) -> usize {
        self.channels.iter().map(|c| c.obs.len()).max().unwrap_or(0)
    }

    pub fn estimate(&self) -> Estimate {
        // A single abscissa origin, shared by every channel, so the intercepts
        // are directly comparable even when the channels kept different
        // periods. Centring it on the window also minimises the intercept's own
        // standard error.
        let mut lo = u64::MAX;
        let mut hi = 0u64;
        for c in &self.channels {
            if let (Some(f), Some(l)) = (c.obs.front(), c.obs.back()) {
                lo = lo.min(f.p);
                hi = hi.max(l.p);
            }
        }
        let centre = if lo == u64::MAX { 0.0 } else { (lo as f64 + hi as f64) / 2.0 };

        let channels: Vec<ChannelEstimate> = self.channels.iter().map(|c| self.aggregate(c, centre)).collect();
        // Worst verdict wins: the orchestration needs one thing to branch on,
        // and any rejected channel makes the whole set unusable.
        let quality = channels.iter().find(|c| !c.quality.is_accepted()).map_or(Quality::Accepted, |c| c.quality.clone());
        Estimate {
            sample_rate: self.config.sample_rate,
            pattern_ms: self.geom.period_frames as f64 / self.geom.rate * 1000.0,
            channels,
            periods_seen: self.periods_complete(),
            clipped_samples: self.clipped_samples,
            gaps: self.gaps,
            quality,
        }
    }

    fn aggregate(&self, c: &ChannelState, centre: f64) -> ChannelEstimate {
        let g = &self.geom;
        let to_ms = 1000.0 / g.rate;
        let label = c.spec.label.clone();
        let all: Vec<PeriodObs> = c.obs.iter().copied().collect();
        let used: Vec<PeriodObs> = all.iter().copied().filter(|o| o.usable).collect();

        // SNR/ratio are reported from the periods that saw a burst, falling back
        // to all of them so a capture with *no* detections still reports how bad
        // it was (and is rejected as LowSnr, not TooFewPeriods).
        let stats = if used.is_empty() { &all } else { &used };
        let peak_snr_db = median(&mut stats.iter().map(|o| o.snr_db).collect::<Vec<_>>());
        let second_peak_ratio = median(&mut stats.iter().map(|o| o.ratio).collect::<Vec<_>>());

        let mut out = ChannelEstimate {
            label,
            center_hz: c.spec.center_hz,
            phase_ms: 0.0,
            std_error_ms: 0.0,
            peak_snr_db,
            second_peak_ratio,
            drift_ppm: 0.0,
            periods_used: used.len(),
            quality: Quality::Accepted,
        };

        if used.len() >= MIN_PERIODS_USED {
            let period = g.period_frames as f64;
            // Circular mean: phases wrap at the period, so the average of 1.999 s
            // and 0.001 s must be 0.0 s, not 1.0 s.
            let (mut sx, mut sy) = (0.0, 0.0);
            for o in &used {
                let a = TAU * o.phase / period;
                sx += a.cos();
                sy += a.sin();
            }
            let mean = (sy.atan2(sx) / TAU * period).rem_euclid(period);
            // Unwrap around that mean, then fit a line. Valid as long as the
            // total drift across the window stays under half a period: at 100 ppm
            // the full 512-period history drifts ~100 ms against a 1000 ms limit,
            // so there is an order of magnitude of headroom.
            let xs: Vec<f64> = used.iter().map(|o| o.p as f64 - centre).collect();
            let ys: Vec<f64> = used.iter().map(|o| mean + wrap_sym(o.phase - mean, period)).collect();
            let n = used.len() as f64;
            let xbar = xs.iter().sum::<f64>() / n;
            let ybar = ys.iter().sum::<f64>() / n;
            let sxx: f64 = xs.iter().map(|x| (x - xbar) * (x - xbar)).sum();
            let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - xbar) * (y - ybar)).sum();
            let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
            let intercept = ybar - slope * xbar;
            let ss: f64 = xs.iter().zip(&ys).map(|(x, y)| (y - (intercept + slope * x)).powi(2)).sum();
            let sigma = (ss / (n - 2.0)).max(0.0).sqrt();
            let se = if sxx > 0.0 { sigma * (1.0 / n + xbar * xbar / sxx).sqrt() } else { sigma / n.sqrt() };

            out.phase_ms = intercept.rem_euclid(period) * to_ms;
            out.std_error_ms = se * to_ms;
            out.drift_ppm = slope / period * 1e6;
        }

        out.quality = self.verdict(&out);
        out
    }

    /// Plan §5.5, in priority order: the things that make the numbers meaningless
    /// come before the things that make them merely imprecise.
    fn verdict(&self, e: &ChannelEstimate) -> Quality {
        let l = &e.label;
        if self.clipped_samples > 0 {
            return Quality::reject(
                RejectReason::Clipped,
                format!(
                    "the microphone clipped ({} samples at full scale) — clipping is broadband, so it corrupts every measurement channel, \
                     not just the loud speaker's. Lower the playback level or move the phone back, then measure again.",
                    self.clipped_samples
                ),
            );
        }
        if self.gaps > 0 {
            return Quality::reject(
                RejectReason::SequenceGap,
                format!(
                    "the microphone stream dropped {} block(s), so arrival times after the gap are shifted by an unknown amount. \
                     Keep the phone's screen on and the browser tab in front, then measure again.",
                    self.gaps
                ),
            );
        }
        if e.periods_used < MIN_PERIODS_USED {
            return Quality::reject(
                RejectReason::TooFewPeriods,
                format!(
                    "only {} usable pattern period(s) for the {l} tone — need {MIN_PERIODS_USED}. Keep recording for a few more seconds.",
                    e.periods_used
                ),
            );
        }
        if e.peak_snr_db < MIN_PEAK_SNR_DB {
            return Quality::reject(
                RejectReason::LowSnr,
                format!(
                    "the {l} tone is only {:.1} dB above the room's noise floor (need {MIN_PEAK_SNR_DB:.0} dB). \
                     Move the phone closer to the speakers, raise the calibration level, or quieten the room.",
                    e.peak_snr_db
                ),
            );
        }
        if e.second_peak_ratio < MIN_SECOND_PEAK_RATIO {
            return Quality::reject(
                RejectReason::AmbiguousPeak,
                format!(
                    "two arrivals of the {l} tone are within {:.1}× of each other, so which one is the direct sound is a guess \
                     (a strong reflection, or another speaker still audible in this band). Move the phone away from walls, \
                     or align this pair by ear.",
                    e.second_peak_ratio
                ),
            );
        }
        if e.std_error_ms > MAX_STD_ERROR_MS {
            return Quality::reject(
                RejectReason::UnstablePhase,
                format!(
                    "the {l} tone's arrival moved by ±{:.2} ms between pattern repeats (limit {MAX_STD_ERROR_MS:.1} ms). \
                     Hold the phone still — put it down if you can — and measure again.",
                    e.std_error_ms
                ),
            );
        }
        Quality::Accepted
    }
}

/// Shortest distance between two offsets on a circular period.
fn circ_dist(a: u64, b: u64, period: u64) -> u64 {
    let d = a.abs_diff(b);
    d.min(period - d)
}

/// Wrap a difference into ±half a period.
fn wrap_sym(d: f64, period: f64) -> f64 {
    d - period * (d / period).round()
}

/// Median (upper of the two middles for even counts). 0.0 for an empty slice.
fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable_by(f64::total_cmp);
    v[v.len() / 2]
}

#[cfg(test)]
mod tests;
