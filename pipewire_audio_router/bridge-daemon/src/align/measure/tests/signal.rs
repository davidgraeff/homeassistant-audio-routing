//! The pre-flight signal check, which grades a capture before a run commits to it.
//!
//! Fixtures and fakes live in [`super::harness`].

use super::super::*;
use super::harness::*;

#[test]
fn signal_check_grades_the_level_by_the_worst_channel() {
    // A quiet room: both tones well clear of the floor.
    let good = signal_check_window(&signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.000_5), 2000.0);
    assert_eq!(good.verdict, SignalVerdict::Good, "{}", good.message);
    assert_eq!(good.channels.len(), 2, "both click-track tones must be graded");
    // The verdict follows the *worst* channel, never an average.
    let worst = good.channels.iter().map(|c| c.peak_snr_db).fold(f64::INFINITY, f64::min);
    assert!((good.worst_peak_snr_db.unwrap() - worst).abs() < 1e-9);

    // Loud speaker, loud room: measurable but without margin. Recorded because
    // it pins the offset W2 measured between *broadband* SNR (burst peak over
    // noise RMS, here ≈ −6 dB) and the *reported* peak SNR, which the matched
    // filter's processing gain lifts by roughly 24 dB — so a capture that looks
    // hopeless by ear can still measure, and the meter cannot tell you that.
    let tight = signal_check_window(&signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.5), 2000.0);
    assert_eq!(tight.verdict, SignalVerdict::Marginal, "{}", tight.message);
    assert!(tight.message.contains("tight"), "{}", tight.message);

    // A speaker far too quiet for the room: refused rather than attempted, so
    // no delay is ever written from it.
    let bad = signal_check_window(&signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.005, 0.05), 2000.0);
    assert_eq!(bad.verdict, SignalVerdict::TooQuiet, "{}", bad.message);
    assert!(bad.message.contains("Too quiet"), "{}", bad.message);
}

/// Plan §12.2: the green indicator has to beat the measurement gate, and it does
/// so on a shorter window (the ordering itself is a compile-time assertion next
/// to the two constants). What is tested here is that the shorter window still
/// *grades* — a faster verdict that is wrong would be worse than a slow one.
#[test]
fn the_preflight_grades_on_a_shorter_window_than_the_gate() {
    // A window that is not aligned to the analysis grid, which is the live case:
    // the partial period at each end is dropped, leaving one whole one.
    let period_frames = 2000.0 / 1000.0 * 48_000.0;
    let offset = (period_frames / 3.0) as u64;
    let good = signal_check_window(&signal_window_at(offset, 48_000, 2000.0, PREFLIGHT_PERIODS, 0.15, 0.000_5), 2000.0);
    assert_eq!(good.verdict, SignalVerdict::Good, "{}", good.message);
    assert_eq!(good.periods, 1, "two periods of audio yield exactly one analysed period");
    assert_eq!(good.channels.len(), 2, "both tones are still graded");
    // One period has no line fit, so there is no phase — which the pre-flight does
    // not need (plan §12.2: a rough SNR, not a phase).
    assert!(good.channels.iter().all(|c| c.phase_ms == 0.0));

    let bad = signal_check_window(&signal_window_at(offset, 48_000, 2000.0, PREFLIGHT_PERIODS, 0.005, 0.05), 2000.0);
    assert_eq!(bad.verdict, SignalVerdict::TooQuiet, "{}", bad.message);
    assert!(bad.message.contains("Too quiet"), "{}", bad.message);
}

/// The corner the short window creates: a window that happens to sit exactly on
/// the analysis grid has a partial period at each end and no whole one between
/// them. The estimator then reports 0 dB from an empty median, which must not be
/// shown as "far too quiet" — it would send the user to turn the speakers up for
/// no reason.
///
/// It must not be shown as "still collecting audio" either, which is what it said
/// until 2026-08-13: the window is *full*, so nothing arrives by waiting. The level
/// that was in it goes in the sentence instead, because that is the number which says
/// whether the microphone heard anything at all.
#[test]
fn a_preflight_window_with_no_complete_period_says_so_rather_than_too_quiet() {
    let aligned = signal_check_window(&signal_window_at(0, 48_000, 2000.0, PREFLIGHT_PERIODS, 0.15, 0.000_5), 2000.0);
    assert_eq!(aligned.periods, 0, "this is the case the guard exists for");
    assert_eq!(aligned.verdict, SignalVerdict::Unusable, "{}", aligned.message);
    assert!(!aligned.message.contains("quiet"), "a loud capture must never be called quiet: {}", aligned.message);
    assert!(!aligned.message.contains("Still collecting"), "the window is full — waiting cannot help: {}", aligned.message);
    assert!(aligned.message.contains("dBFS"), "the level it did hear is the actionable part: {}", aligned.message);
    assert!(aligned.capture_peak_dbfs.is_some_and(|db| db > -20.0), "a loud window reports a loud peak: {:?}", aligned.capture_peak_dbfs);
}

/// The field case that made the message above wrong (2026-08-13): a capture that is
/// connected, gapless, and carrying **nothing**. No period can ever close, so the old
/// code invited the user to wait indefinitely for audio that was never coming.
///
/// It is `Silent` rather than `TooQuiet` because the remedies are disjoint: nothing is
/// playing on the soloed speaker, or the input is muted, or the device gates its own
/// silence to exact zeros — and "turn the speakers up" answers none of them.
#[test]
fn a_capture_carrying_nothing_is_silent_not_still_collecting() {
    let frames = (2000.0 / 1000.0 * 48_000.0 * PREFLIGHT_PERIODS as f64) as usize;
    let silence =
        crate::align::mic::MicWindow { samples: vec![0.0; frames], first_frame: 12_345, sample_rate: 48_000, gap: false, clipped: false };
    let check = signal_check_window(&silence, 2000.0);
    assert_eq!(check.verdict, SignalVerdict::Silent, "{}", check.message);
    assert_eq!(check.periods, 0);
    assert!(check.capture_peak_dbfs.is_none(), "there is no dB value for digital silence");
    assert!(check.message.contains("soloed"), "the message has to name what to check: {}", check.message);
    assert!(!check.message.contains("Still collecting"), "{}", check.message);
    assert!(!check.message.to_lowercase().contains("turn the speakers up"), "{}", check.message);

    // A stuck DC offset is the same fault for the same reason — a constant signal has
    // no local maximum whatever its value — so it must not read as a loud capture.
    let dc = crate::align::mic::MicWindow { samples: vec![0.5; frames], first_frame: 0, sample_rate: 48_000, gap: false, clipped: false };
    let check = signal_check_window(&dc, 2000.0);
    assert_eq!(check.verdict, SignalVerdict::Silent, "{}", check.message);
    assert!(check.capture_peak_dbfs.is_some_and(|db| (db + 6.0).abs() < 0.1), "{:?}", check.capture_peak_dbfs);
}

/// Clipping is decided over the mic's own trailing window, which is wider than the 4 s
/// the pre-flight analyses. Without that, one broken level produced three different
/// diagnoses in as many polls depending on where a rail sample happened to land.
#[test]
fn recent_clipping_outranks_a_level_verdict_but_not_a_more_specific_one() {
    let good = signal_check_window(&signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.000_5), 2000.0);
    assert_eq!(good.verdict, SignalVerdict::Good);

    let clipped = with_recent_clipping(good.clone(), 7, 5);
    assert_eq!(clipped.verdict, SignalVerdict::Unusable, "{}", clipped.message);
    assert!(clipped.clipped);
    assert_eq!(clipped.recent_clip_count, 7);
    assert!(clipped.message.contains("7 sample"), "it quotes what it counted: {}", clipped.message);
    assert!(clipped.message.contains("last 5 s"), "and over what span: {}", clipped.message);
    assert!(clipped.message.contains("down"), "the action must be the right one: {}", clipped.message);

    // No recent clipping leaves the verdict alone, and only records the zero.
    let kept = with_recent_clipping(good.clone(), 0, 5);
    assert_eq!(kept.verdict, SignalVerdict::Good);
    assert_eq!(kept.recent_clip_count, 0);

    // A silent capture stays silent: a clip four seconds ago is the *previous* state,
    // and "it clipped" would send the user to turn down a speaker that is now mute.
    let frames = (2000.0 / 1000.0 * 48_000.0 * PREFLIGHT_PERIODS as f64) as usize;
    let silence =
        crate::align::mic::MicWindow { samples: vec![0.0; frames], first_frame: 0, sample_rate: 48_000, gap: false, clipped: false };
    let silent = with_recent_clipping(signal_check_window(&silence, 2000.0), 3, 5);
    assert_eq!(silent.verdict, SignalVerdict::Silent, "{}", silent.message);
    assert_eq!(silent.recent_clip_count, 3, "still reported, just not the headline");
}

#[test]
fn signal_check_refuses_clipped_and_gapped_captures_before_grading() {
    let mut w = signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.000_5);
    w.clipped = true;
    let clipped = signal_check_window(&w, 2000.0);
    assert_eq!(clipped.verdict, SignalVerdict::Unusable);
    // The action must be the correct one: turning up cannot fix clipping.
    assert!(clipped.message.contains("down"), "{}", clipped.message);

    let mut w = signal_window(48_000, 2000.0, GATE_MIN_PERIODS, 0.15, 0.000_5);
    w.gap = false;
    w.gap = true;
    let gapped = signal_check_window(&w, 2000.0);
    assert_eq!(gapped.verdict, SignalVerdict::Unusable);
    assert!(gapped.message.contains("lost"), "{}", gapped.message);
}
