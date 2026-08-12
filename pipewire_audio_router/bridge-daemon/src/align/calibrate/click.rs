//! The test signal: a click pattern, generated rather than shipped as an asset.
//!
//! One period is a click followed by silence, which is what makes an arrival
//! measurable — a continuous tone gives the estimator no edge to find. The rate and
//! the period are fixed here because every consumer has to agree on them: the
//! session plays this, the estimator correlates against it, and the gate counts
//! periods.

use super::*;

/// The same meeting point for a **pw-sink host's** scale (W20): a calibration level is an
/// integer 0–100, while the receiver agent speaks the cubic **0.0–1.0** its own
/// `HostState::volume`, `wpctl` and HA's `volume_level` all use
/// ([`crate::outputs::pwsink::agent::Agents::set_volume`]).
///
/// Written out for the same reason as [`ap2_level`]: a factor of 100 the wrong way clamps
/// at a rail, and a rail on someone's desktop speakers is either silence or full scale.
/// Arithmetically identical to [`ap2_level`] today and deliberately *not* shared with it —
/// they are two independent far-end contracts, and folding them into one function would
/// make a future change to either one silently change the other.
///
/// The restore direction again has no counterpart: teardown writes back the host's own
/// snapshotted `f32` ([`Session::saved_oob_levels`]), never a round trip through 0–100, so
/// putting a level back cannot move it by a rounding step.
///
/// The knob's *taper* is not this function's business: the host applies a cubic curve of
/// its own, which is precisely the unknown `align_levels::LEVEL_TAPER_NOTE` describes and
/// the level solver measures rather than models.
pub(crate) fn host_level(level: u8) -> f32 {
    f32::from(level.min(100)) / 100.0
}

/// Builds the alternating two-tone click WAV (S16LE stereo, one 2 s loop).
pub fn click_wav() -> Vec<u8> {
    let total = (PATTERN_SECS * RATE as f64) as usize; // frames
    let half = total / 2;
    let click_frames = (CLICK_MS / 1000.0 * RATE as f64) as usize;
    let mut pcm = Vec::with_capacity(total * CHANNELS as usize * 2);
    for i in 0..total {
        // Click A burst at the pattern start, click B burst at the half point.
        let s = if i < click_frames {
            click_sample(i, click_frames, FREQ_A)
        } else if i >= half && i < half + click_frames {
            click_sample(i - half, click_frames, FREQ_B)
        } else {
            0.0
        };
        let v = (s * CLICK_AMP * f64::from(i16::MAX)) as i16;
        let le = v.to_le_bytes();
        pcm.extend_from_slice(&le); // FL
        pcm.extend_from_slice(&le); // FR
    }
    crate::audio::wav::build_wav(&pcm, RATE, 16, CHANNELS)
}

/// One burst sample: a sine at `freq` under a Hann envelope over the `n`-sample
/// burst, so it starts/ends at zero (no pop that would itself smear timing).
pub(crate) fn click_sample(i: usize, n: usize, freq: f64) -> f64 {
    let t = i as f64 / f64::from(RATE);
    let env = 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos();
    (2.0 * PI * freq * t).sin() * env
}
