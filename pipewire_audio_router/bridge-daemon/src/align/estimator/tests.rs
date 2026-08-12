//! Tests for arrival estimation from a mic window.

use super::*;

// ---------------------------------------------------------------- harness
//
// A local synthetic-capture generator. Deliberately *not* built on
// `calibrate::click_wav()`: these tests must exercise the estimator alone,
// with fractional delays, noise, reflections and clock drift that the real
// click track cannot express — and without a WAV container in the way.

const RATE_48: u32 = 48_000;
const RATE_44: u32 = 44_100;

/// One burst to render: where it starts inside each pattern period (ms,
/// fractional), at what centre frequency and amplitude.
#[derive(Clone, Copy)]
struct Burst {
    onset_ms: f64,
    hz: f64,
    amp: f64,
}

/// Deterministic xorshift64* + Box–Muller — seeded so a failure is always
/// reproducible.
struct Noise {
    s: u64,
    spare: Option<f64>,
}

impl Noise {
    fn new(seed: u64) -> Self {
        Self { s: seed | 1, spare: None }
    }

    fn uniform(&mut self) -> f64 {
        self.s ^= self.s >> 12;
        self.s ^= self.s << 25;
        self.s ^= self.s >> 27;
        let v = self.s.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let u1 = self.uniform().max(1e-12);
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        self.spare = Some(r * (TAU * u2).sin());
        r * (TAU * u2).cos()
    }
}

/// Add one Hann-enveloped burst at a **fractional** frame position, by
/// sampling the continuous burst function — this is what makes sub-sample
/// accuracy claims testable at all.
fn add_burst(buf: &mut [f32], rate: f64, start: f64, hz: f64, amp: f64, len: usize) {
    let i0 = start.floor() as i64;
    let frac = start - i0 as f64;
    for k in 0..=len {
        let t = k as f64 - frac; // frames since the burst's true onset
        if t < 0.0 || t > len as f64 {
            continue;
        }
        let env = 0.5 - 0.5 * (TAU * t / len as f64).cos();
        let s = amp * env * (TAU * hz * t / rate).sin();
        let idx = i0 + k as i64;
        if idx >= 0 && (idx as usize) < buf.len() {
            buf[idx as usize] += s as f32;
        }
    }
}

/// Render `periods` pattern periods containing `bursts`, plus optional white
/// noise of the given RMS.
fn render(rate: u32, periods: usize, bursts: &[Burst], noise_rms: f64, seed: u64) -> Vec<f32> {
    let r = f64::from(rate);
    let period_frames = (PATTERN_SECS * r).round() as usize;
    let len = (BURST_MS / 1000.0 * r).round() as usize;
    let mut buf = vec![0.0f32; periods * period_frames];
    for p in 0..periods {
        for b in bursts {
            let start = (p * period_frames) as f64 + b.onset_ms / 1000.0 * r;
            add_burst(&mut buf, r, start, b.hz, b.amp, len);
        }
    }
    if noise_rms > 0.0 {
        let mut n = Noise::new(seed);
        for s in &mut buf {
            *s += (noise_rms * n.gauss()) as f32;
        }
    }
    buf
}

/// Linear-interpolation resample standing in for a mic clock that runs fast
/// by `ppm`: the capture emits `1 + ppm/1e6` frames per nominal frame, so the
/// 2 s pattern occupies more than `period_frames` mic frames and every
/// arrival lands progressively later on the estimator's fixed grid.
fn resample_ppm(input: &[f32], ppm: f64) -> Vec<f32> {
    let ratio = 1.0 + ppm * 1e-6;
    let out_len = (input.len() as f64 * ratio) as usize;
    (0..out_len)
        .map(|n| {
            let t = n as f64 / ratio;
            let i = t.floor() as usize;
            let f = (t - i as f64) as f32;
            let a = input.get(i).copied().unwrap_or(0.0);
            let b = input.get(i + 1).copied().unwrap_or(0.0);
            a + (b - a) * f
        })
        .collect()
}

/// Feed a capture in ~20 ms blocks, the way `align_mic` will (plan §4.3), so
/// the per-block oscillator re-anchoring is exercised.
fn feed(est: &mut Estimator, samples: &[f32], first_frame: u64) {
    let block = est.config().sample_rate as usize / 50;
    for (i, chunk) in samples.chunks(block).enumerate() {
        est.push_block(first_frame + (i * block) as u64, chunk);
    }
}

fn estimate_of(rate: u32, samples: &[f32]) -> Estimate {
    let mut est = Estimator::new(EstimatorConfig::click_track(rate)).unwrap();
    feed(&mut est, samples, 0);
    est.estimate()
}

/// The A/B pair used by most tests: B lands 17.3 ms after A.
fn ab_pair(a_ms: f64, delay_ms: f64, amp: f64) -> [Burst; 2] {
    [Burst { onset_ms: a_ms, hz: CLICK_A_HZ, amp }, Burst { onset_ms: a_ms + delay_ms, hz: CLICK_B_HZ, amp }]
}

// ------------------------------------------------------------------ tests

/// The headline claim: a known injected delay comes back.
///
/// Achieved (noiseless, 12 periods): **delta error +0.005 ms** at 48 kHz, and
/// the *absolute* phase of A lands 0.002 ms from its true onset — which also
/// confirms the `onset_offset` window-delay convention. The 0.05 ms tolerance
/// asserted below is ~10× the observed error; the residual is
/// parabolic-interpolation bias on a peak that is not exactly a parabola, not
/// noise (every period returns the same answer to ±0.5 frames).
#[test]
fn recovers_a_known_injected_delay() {
    let sig = render(RATE_48, 12, &ab_pair(300.0, 17.3, 0.25), 0.0, 1);
    let est = estimate_of(RATE_48, &sig);
    assert!(est.accepted(), "expected an accepted estimate, got {:?}", est.quality);
    let d = est.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
    assert!((d - 17.3).abs() < 0.05, "delta {d} ms, expected 17.3");
    let a = est.channel(CLICK_A_LABEL).unwrap();
    assert!((a.phase_ms - 300.0).abs() < 0.05, "A phase {} ms, expected ~300", a.phase_ms);
    assert!(a.std_error_ms < 0.05, "std error {} ms", a.std_error_ms);
    assert!(a.peak_snr_db > 60.0, "noiseless SNR {} dB", a.peak_snr_db);
    // 12 rendered periods → the first is eaten by filter warm-up and the last
    // is still in progress (it closes when the next one starts).
    assert_eq!(a.periods_used, 10);
}

/// 44.1 kHz is not hypothetical — it is what iOS gives (plan §4.3).
/// Achieved: delta error +0.012 ms; the coarser frame is visible but
/// irrelevant against a 1 ms knob.
#[test]
fn recovers_the_same_delay_at_44100() {
    let sig = render(RATE_44, 12, &ab_pair(300.0, 17.3, 0.25), 0.0, 2);
    let est = estimate_of(RATE_44, &sig);
    assert!(est.accepted(), "{:?}", est.quality);
    let d = est.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
    assert!((d - 17.3).abs() < 0.05, "delta {d} ms at 44.1 kHz");
    let a = est.channel(CLICK_A_LABEL).unwrap();
    assert!((a.phase_ms - 300.0).abs() < 0.05, "A phase {} ms at 44.1 kHz", a.phase_ms);
    assert!((est.pattern_ms - 2000.0).abs() < 0.01);
}

/// Where the estimator stops being trustworthy.
///
/// SNR here is *broadband*: 20·log10(burst peak ÷ noise RMS). The detector's
/// in-band gain puts the *reported* `peak_snr_db` about 17 dB above it.
/// Achieved delta errors, 20 periods, fixed seed, 48 kHz:
///
/// | broadband SNR | reported peak SNR | delta error | verdict |
/// |---|---|---|---|
/// |  30 dB | 47 dB | +0.014 ms | accepted |
/// |  20 dB | 37 dB | +0.020 ms | accepted |
/// |  15 dB | 32 dB | +0.024 ms | accepted |
/// |  10 dB | 27 dB | +0.029 ms | accepted |
/// |   5 dB | 22 dB | +0.012 ms | accepted |
/// |   0 dB | 18 dB | +0.065 ms | accepted |
/// |  −3 dB | 15 dB | +0.147 ms | **refused** (low SNR) |
/// |  −6 dB | 12 dB | −19.5 ms  | **refused** (low SNR) |
/// | −10 dB | 11 dB | −99 ms    | **refused** (low SNR) |
///
/// So noise is not the limit anywhere near the levels a phone mic in a quiet
/// room sees: the estimator stays inside 0.07 ms right down to a broadband
/// SNR of 0 dB, and the cliff between "good to 0.15 ms" and "meaningless"
/// spans about 3 dB — with the refusal threshold on the safe side of it.
/// Reverb, not noise, is the real risk (see `survives_synthetic_reverb` and
/// `an_early_reflection_biases_silently`).
#[test]
fn accuracy_degrades_gracefully_with_noise() {
    // Low enough that even −6 dB SNR noise cannot reach full scale, which
    // would trip the clipping rule instead of the one under test.
    let amp = 0.05;
    for (snr_db, tol) in [(30.0, 0.05), (20.0, 0.05), (15.0, 0.05), (10.0, 0.05), (5.0, 0.05), (0.0, 0.10)] {
        let noise = amp / 10f64.powf(snr_db / 20.0);
        let sig = render(RATE_48, 20, &ab_pair(300.0, 17.3, amp), noise, 7);
        let est = estimate_of(RATE_48, &sig);
        assert_eq!(est.clipped_samples, 0, "the test signal itself must not clip at {snr_db} dB");
        assert!(est.accepted(), "{snr_db} dB SNR should still be measurable: {:?}", est.quality);
        let d = est.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
        assert!((d - 17.3).abs() < tol, "at {snr_db} dB broadband SNR delta was {d} ms (tolerance {tol})");
    }
    // …and three dB further down it is refused rather than believed.
    let sig = render(RATE_48, 20, &ab_pair(300.0, 17.3, amp), amp * 2.0, 7);
    let est = estimate_of(RATE_48, &sig);
    assert_eq!(est.quality.reason(), Some(RejectReason::LowSnr), "{:?}", est.quality);
}

/// Pure noise must be refused, not fitted. The largest of ~250 independent
/// envelope samples sits only ~9 dB over the median, so `LowSnr` fires.
#[test]
fn pure_noise_is_refused_as_low_snr() {
    let sig = render(RATE_48, 8, &[], 0.05, 11);
    let est = estimate_of(RATE_48, &sig);
    assert_eq!(est.quality.reason(), Some(RejectReason::LowSnr), "{:?}", est.quality);
    let a = est.channel(CLICK_A_LABEL).unwrap();
    assert!(a.peak_snr_db < MIN_PEAK_SNR_DB, "noise-only SNR {} dB", a.peak_snr_db);
    // The message is what the user sees; it must name the problem.
    let Quality::Rejected { message, .. } = &est.quality else { panic!() };
    assert!(message.contains("noise floor"), "{message}");
}

/// A real room: the direct arrival plus a decaying train of reflections.
///
/// Outcome (recorded, since either finding it or refusing was acceptable):
/// the estimator **still finds the direct arrival and accepts**. With
/// reflections at +7/+13/+23/+37/+53/+71 ms at −4 to −19 dB, the delta error
/// is 0.14 ms, the absolute phases are off by +0.04 ms (A) and −0.10 ms (B),
/// and the second-peak ratio is 2.2 — the direct sound stays clear of the
/// runner-up. Making every reflection 30 % louder gives 0.16 ms and a ratio
/// of 1.66, i.e. still accepted and still accurate.
///
/// The A and B errors have opposite signs because a reflection interferes
/// with the direct sound differently at each channel's frequency (7 ms is 21
/// whole cycles at 3 kHz but 10.5 at 1.5 kHz — constructive for one,
/// destructive for the other). That per-channel bias, not the noise floor, is
/// what ultimately limits this signal design; see
/// `an_early_reflection_biases_silently` for where it stops being harmless.
#[test]
fn survives_synthetic_reverb() {
    let direct = 0.25;
    let mut bursts = ab_pair(300.0, 17.3, direct).to_vec();
    for (dt, gain) in [(7.0, 0.63), (13.0, 0.45), (23.0, 0.32), (37.0, 0.22), (53.0, 0.16), (71.0, 0.11)] {
        bursts.push(Burst { onset_ms: 300.0 + dt, hz: CLICK_A_HZ, amp: direct * gain });
        bursts.push(Burst { onset_ms: 300.0 + 17.3 + dt, hz: CLICK_B_HZ, amp: direct * gain });
    }
    let sig = render(RATE_48, 16, &bursts, 0.002, 23);
    let est = estimate_of(RATE_48, &sig);
    let a = est.channel(CLICK_A_LABEL).unwrap();
    let b = est.channel(CLICK_B_LABEL).unwrap();
    assert!(est.accepted(), "{:?}", est.quality);
    let d = est.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
    assert!((d - 17.3).abs() < 0.5, "reverb pulled the delta to {d} ms (ratio {})", a.second_peak_ratio);
    // The direct arrival, not a reflection: both absolute phases stay put.
    assert!((a.phase_ms - 300.0).abs() < 0.3, "A {} ms", a.phase_ms);
    assert!((b.phase_ms - 317.3).abs() < 0.3, "B {} ms", b.phase_ms);
    assert!(a.second_peak_ratio > MIN_SECOND_PEAK_RATIO);
}

/// **Known blind spot, recorded deliberately** (plan §5.5 does not cover it,
/// and it is the argument for W9).
///
/// A reflection that lands inside the analysis window merges with the direct
/// arrival into a single peak, and the merged peak is pulled. Nothing in the
/// verdict notices: the peak is strong, single, *and* identical every period,
/// so SNR, second-peak ratio and standard error all look excellent. Measured
/// with a 0.9× reflection on channel B only: −1.72 ms at +1 ms, +0.89 ms at
/// +2 ms, −0.93 ms at +3 ms, −0.40 ms at +5 ms, −0.001 ms at +8 ms — all
/// reported as accepted with a standard error under 0.01 ms. A reflection
/// *louder* than the direct sound (1.4×, +5 ms) moves the answer by +5.2 ms:
/// the estimator has locked onto the reflection and is completely confident.
///
/// The saving grace is the guard distance: once the reflection is further out
/// than the window it becomes a separate peak and is refused as ambiguous
/// (verified below at +13 ms).
#[test]
fn an_early_reflection_biases_silently() {
    let measure = |dt: f64, gain: f64| {
        let mut bursts = ab_pair(300.0, 17.3, 0.25).to_vec();
        bursts.push(Burst { onset_ms: 317.3 + dt, hz: CLICK_B_HZ, amp: 0.25 * gain });
        let sig = render(RATE_48, 14, &bursts, 0.001, 29);
        estimate_of(RATE_48, &sig)
    };

    // Inside the window: biased, and confidently accepted.
    let inside = measure(3.0, 0.9);
    assert!(inside.accepted(), "{:?}", inside.quality);
    let b = inside.channel(CLICK_B_LABEL).unwrap();
    let err = b.phase_ms - 317.3;
    assert!(err.abs() > 0.5, "expected a bias from the merged reflection, got {err} ms");
    assert!(err.abs() < 2.0, "…but a bounded one, got {err} ms");
    assert!(b.std_error_ms < 0.05, "the bias is deterministic, so the SE cannot see it: {}", b.std_error_ms);
    assert!(b.second_peak_ratio > 10.0, "…and it is one merged peak: {}", b.second_peak_ratio);

    // Outside the window: correctly refused instead of silently biased.
    let outside = measure(13.0, 0.9);
    assert_eq!(outside.quality.reason(), Some(RejectReason::AmbiguousPeak), "{:?}", outside.quality);
    // The phase itself is still the direct arrival — it is the *confidence*
    // that is missing, which is exactly what the refusal communicates.
    assert!((outside.channel(CLICK_B_LABEL).unwrap().phase_ms - 317.3).abs() < 0.1);
}

/// Clock drift is recovered, not silently absorbed into the phase.
/// Achieved: +100 ppm injected → +100.0 ppm reported (both channels), and
/// the delta is still right to 0.02 ms.
#[test]
fn recovers_clock_drift() {
    let nominal = render(RATE_48, 24, &ab_pair(900.0, 17.3, 0.25), 0.0, 31);
    let drifted = resample_ppm(&nominal, 100.0);
    let est = estimate_of(RATE_48, &drifted);
    assert!(est.accepted(), "{:?}", est.quality);
    for label in [CLICK_A_LABEL, CLICK_B_LABEL] {
        let c = est.channel(label).unwrap();
        assert!((c.drift_ppm - 100.0).abs() < 5.0, "channel {label} drift {} ppm, expected +100", c.drift_ppm);
    }
    let d = est.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
    assert!((d - 17.3).abs() < 0.1, "delta {d} ms under drift");
    // A drift-free capture must report ~0, or the slope would be an artefact.
    let clean = estimate_of(RATE_48, &nominal);
    assert!(clean.channel(CLICK_A_LABEL).unwrap().drift_ppm.abs() < 1.0);
}

/// Two equally strong arrivals in one channel: which is the direct sound is
/// unknowable, so refuse.
#[test]
fn an_ambiguous_double_peak_is_refused() {
    let mut bursts = ab_pair(300.0, 17.3, 0.25).to_vec();
    // A second A burst 400 ms away, same level — e.g. a member slipped by a
    // whole click, or two speakers still audible in the same band.
    bursts.push(Burst { onset_ms: 700.0, hz: CLICK_A_HZ, amp: 0.25 });
    let sig = render(RATE_48, 10, &bursts, 0.001, 41);
    let est = estimate_of(RATE_48, &sig);
    assert_eq!(est.quality.reason(), Some(RejectReason::AmbiguousPeak), "{:?}", est.quality);
    let a = est.channel(CLICK_A_LABEL).unwrap();
    assert!(a.second_peak_ratio < MIN_SECOND_PEAK_RATIO, "ratio {}", a.second_peak_ratio);
    // B, with a single arrival, is individually fine — the *set* is not.
    assert!(est.channel(CLICK_B_LABEL).unwrap().quality.is_accepted());
}

/// One full-scale sample poisons every channel (clipping is broadband), so
/// the whole window goes.
#[test]
fn a_clipped_capture_is_refused() {
    let mut sig = render(RATE_48, 8, &ab_pair(300.0, 17.3, 0.25), 0.0, 51);
    sig[5_000] = 1.0;
    sig[5_001] = -1.0;
    let est = estimate_of(RATE_48, &sig);
    assert_eq!(est.quality.reason(), Some(RejectReason::Clipped), "{:?}", est.quality);
    assert_eq!(est.clipped_samples, 2);
    // Clipping outranks everything else: even the healthy B channel is out.
    assert!(!est.channel(CLICK_B_LABEL).unwrap().quality.is_accepted());
}

/// A caller-signalled gap invalidates the window even though the ingest's
/// frame index stays contiguous across it.
#[test]
fn a_sequence_gap_is_refused() {
    let sig = render(RATE_48, 8, &ab_pair(300.0, 17.3, 0.25), 0.0, 61);
    let mut est = Estimator::new(EstimatorConfig::click_track(RATE_48)).unwrap();
    let half = sig.len() / 2;
    feed(&mut est, &sig[..half], 0);
    est.note_gap();
    feed(&mut est, &sig[half..], half as u64);
    let out = est.estimate();
    assert_eq!(out.quality.reason(), Some(RejectReason::SequenceGap), "{:?}", out.quality);
    assert_eq!(out.gaps, 1);
    // …and a reset makes the estimator usable again.
    est.reset();
    feed(&mut est, &sig, 0);
    assert!(est.estimate().accepted());
}

/// A discontinuous `first_frame` is a gap too, without anyone saying so.
#[test]
fn a_frame_index_jump_is_detected_as_a_gap() {
    let sig = render(RATE_48, 6, &ab_pair(300.0, 17.3, 0.25), 0.0, 63);
    let mut est = Estimator::new(EstimatorConfig::click_track(RATE_48)).unwrap();
    est.push_block(0, &sig[..1000]);
    est.push_block(9_999, &sig[1000..]);
    assert_eq!(est.estimate().gaps, 1);
}

/// Too short to fit a line through.
#[test]
fn too_few_periods_is_refused() {
    let sig = render(RATE_48, 3, &ab_pair(300.0, 17.3, 0.25), 0.0, 71);
    let est = estimate_of(RATE_48, &sig);
    assert_eq!(est.quality.reason(), Some(RejectReason::TooFewPeriods), "{:?}", est.quality);
    assert!(est.periods_seen < MIN_PERIODS_USED);
}

/// A phone held in the hand: the arrival wanders period to period. The
/// point estimate may still look plausible, so the standard error is what
/// has to catch it.
#[test]
fn a_wandering_arrival_is_refused_as_unstable() {
    let r = f64::from(RATE_48);
    let period_frames = (PATTERN_SECS * r).round() as usize;
    let len = (BURST_MS / 1000.0 * r).round() as usize;
    let mut buf = vec![0.0f32; 14 * period_frames];
    let mut n = Noise::new(83);
    for p in 0..14 {
        // ±3 ms of hand movement, uncorrelated between periods.
        let jitter = 3.0 * n.gauss();
        for (hz, off) in [(CLICK_A_HZ, 0.0), (CLICK_B_HZ, 17.3)] {
            let start = (p * period_frames) as f64 + (300.0 + off + jitter) / 1000.0 * r;
            add_burst(&mut buf, r, start, hz, 0.25, len);
        }
    }
    let est = estimate_of(RATE_48, &buf);
    assert_eq!(est.quality.reason(), Some(RejectReason::UnstablePhase), "{:?}", est.quality);
    assert!(est.channel(CLICK_A_LABEL).unwrap().std_error_ms > MAX_STD_ERROR_MS);
}

/// The grid origin is arbitrary: shifting the frame index shifts every
/// reported phase but leaves the only consumed quantity — the difference —
/// untouched. This is the property the module docs promise.
#[test]
fn the_grid_origin_shifts_phases_but_not_differences() {
    let sig = render(RATE_48, 12, &ab_pair(300.0, 17.3, 0.25), 0.0, 91);
    let base = estimate_of(RATE_48, &sig);
    let mut shifted_est = Estimator::new(EstimatorConfig::click_track(RATE_48)).unwrap();
    feed(&mut shifted_est, &sig, 12_345);
    let shifted = shifted_est.estimate();
    assert!(shifted.accepted(), "{:?}", shifted.quality);
    let (a, b) = (base.channel(CLICK_A_LABEL).unwrap(), shifted.channel(CLICK_A_LABEL).unwrap());
    assert!((a.phase_ms - b.phase_ms).abs() > 1.0, "the origin should have moved the absolute phase");
    let d0 = base.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
    let d1 = shifted.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
    assert!((d0 - d1).abs() < 0.05, "difference must not depend on the origin: {d0} vs {d1}");
}

/// `delta_ms` takes the short way round the period, so a member that arrives
/// just *before* the reference reads as a small negative number rather than
/// ~2 s.
#[test]
fn delta_takes_the_short_way_round_the_period() {
    // B at 20 ms, A at 1990 ms: B arrives 30 ms after A, across the wrap.
    let bursts = [Burst { onset_ms: 1990.0, hz: CLICK_A_HZ, amp: 0.25 }, Burst { onset_ms: 20.0, hz: CLICK_B_HZ, amp: 0.25 }];
    let sig = render(RATE_48, 12, &bursts, 0.0, 97);
    let est = estimate_of(RATE_48, &sig);
    assert!(est.accepted(), "{:?}", est.quality);
    let d = est.delta_ms(CLICK_A_LABEL, CLICK_B_LABEL).unwrap();
    assert!((d - 30.0).abs() < 0.1, "delta {d} ms, expected +30 across the wrap");
    assert_eq!(est.delta_ms("nope", CLICK_B_LABEL), None);
}

/// Arbitrarily many channels, and the labels come back with the results.
#[test]
fn measures_more_than_two_channels() {
    // The plan's one-octave, harmonic-safe set for N ≤ 4 (§6.2).
    let plan = [("s1", 2000.0), ("s2", 2500.0), ("s3", 3050.0), ("s4", 3700.0)];
    let mut bursts = Vec::new();
    let mut expect = Vec::new();
    for (i, (label, hz)) in plan.iter().enumerate() {
        let onset = 200.0 + 300.0 * i as f64 + 0.7 * i as f64;
        bursts.push(Burst { onset_ms: onset, hz: *hz, amp: 0.2 });
        expect.push((*label, onset));
    }
    let sig = render(RATE_48, 12, &bursts, 0.001, 101);
    let cfg = EstimatorConfig {
        sample_rate: RATE_48,
        pattern_secs: PATTERN_SECS,
        burst_ms: BURST_MS,
        channels: plan.iter().map(|(l, hz)| ChannelSpec::new(*l, *hz)).collect(),
    };
    let mut est = Estimator::new(cfg).unwrap();
    feed(&mut est, &sig, 0);
    let out = est.estimate();
    assert!(out.accepted(), "{:?}", out.quality);
    assert_eq!(out.channels.len(), 4);
    for (label, onset) in expect {
        let c = out.channel(label).unwrap();
        assert!((c.phase_ms - onset).abs() < 0.2, "{label} phase {} ms, expected {onset}", c.phase_ms);
    }
    // Staggered slots, so each channel's own band is clean.
    assert!(out.channels.iter().all(|c| c.second_peak_ratio > MIN_SECOND_PEAK_RATIO));
}

#[test]
fn rejects_impossible_configurations() {
    let bad = |cfg: EstimatorConfig| Estimator::new(cfg).err().expect("should be rejected");
    let base = EstimatorConfig::click_track(RATE_48);
    assert!(bad(EstimatorConfig { sample_rate: 4_000, ..base.clone() }).contains("too low"));
    assert!(bad(EstimatorConfig { pattern_secs: 0.0, ..base.clone() }).contains("positive"));
    assert!(bad(EstimatorConfig { burst_ms: 0.05, ..base.clone() }).contains("frames"));
    assert!(bad(EstimatorConfig { channels: vec![], ..base.clone() }).contains("at least one"));
    // Above Nyquist for the declared rate.
    assert!(bad(EstimatorConfig { channels: vec![ChannelSpec::new("x", 30_000.0)], ..base.clone() }).contains("not measurable"));
    assert!(bad(EstimatorConfig { channels: vec![ChannelSpec::new("a", 1000.0), ChannelSpec::new("a", 2000.0)], ..base.clone() })
        .contains("duplicate"));
    // 8 ms at 44.1 kHz is 353 frames — fine.
    assert!(Estimator::new(EstimatorConfig::click_track(RATE_44)).is_ok());
}

/// The API types are what the UI will read (plan §11), so the wire shape is
/// part of the contract.
#[test]
fn serialises_snake_case_for_the_api() {
    let sig = render(RATE_48, 8, &ab_pair(300.0, 17.3, 0.25), 0.0, 103);
    let json = serde_json::to_value(estimate_of(RATE_48, &sig)).unwrap();
    assert_eq!(json["verdict"], serde_json::Value::Null); // quality is nested
    assert_eq!(json["quality"]["verdict"], "accepted");
    let c = &json["channels"][0];
    for key in ["label", "center_hz", "phase_ms", "std_error_ms", "peak_snr_db", "second_peak_ratio", "drift_ppm", "periods_used"] {
        assert!(!c[key].is_null(), "missing {key} in {c}");
    }
    let mut est = Estimator::new(EstimatorConfig::click_track(RATE_48)).unwrap();
    est.note_gap();
    let json = serde_json::to_value(est.estimate()).unwrap();
    assert_eq!(json["quality"]["verdict"], "rejected");
    assert_eq!(json["quality"]["reason"], "sequence_gap");
    assert!(json["quality"]["message"].as_str().unwrap().len() > 20);
}

#[test]
fn history_is_bounded_and_resettable() {
    let mut est = Estimator::new(EstimatorConfig::click_track(RATE_48)).unwrap();
    let sig = render(RATE_48, 6, &ab_pair(300.0, 17.3, 0.25), 0.0, 107);
    feed(&mut est, &sig, 0);
    // 6 rendered periods, less warm-up and the one still in progress.
    assert_eq!(est.periods_complete(), 4);
    est.reset();
    assert_eq!(est.periods_complete(), 0);
    assert!(!est.estimate().accepted());
}

#[test]
fn helpers_behave() {
    assert_eq!(circ_dist(1, 3, 100), 2);
    assert_eq!(circ_dist(99, 1, 100), 2);
    assert!((wrap_sym(1.9, 2.0) - -0.1).abs() < 1e-12);
    assert!((wrap_sym(-1.9, 2.0) - 0.1).abs() < 1e-12);
    assert_eq!(median(&mut []), 0.0);
    assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
}
