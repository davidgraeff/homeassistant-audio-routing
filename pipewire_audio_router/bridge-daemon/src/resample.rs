// ABOUTME: Minimal linear resampler + channel normalizer to the capture format
// ABOUTME: (48 kHz, stereo, S16LE) so an announce clip can be mixed sample-for-
// ABOUTME: sample into the per-device overlay (overlay_mixer.rs).
//
// Linear interpolation is low-fidelity but entirely adequate for short speech/
// chime announcements, and it adds no dependency. If higher quality is ever
// wanted for music-grade overlays, swap this for a windowed-sinc resampler.

/// The capture/overlay target rate (matches `sendspin_capture::SAMPLE_RATE`).
pub const TARGET_RATE: u32 = 48_000;

/// Convert interleaved S16LE PCM at `src_rate`/`src_channels` to 48 kHz stereo
/// S16LE. Mono is duplicated to both channels; >2 channels keep the first two.
pub fn to_48k_stereo_s16le(pcm: &[u8], src_rate: u32, src_channels: u16) -> Vec<u8> {
    let ch = src_channels as usize;
    if ch == 0 || src_rate == 0 || pcm.len() < 2 {
        return Vec::new();
    }
    let frames = pcm.len() / 2 / ch;
    let at = |frame: usize, c: usize| -> i16 {
        let idx = (frame * ch + c) * 2;
        i16::from_le_bytes([pcm[idx], pcm[idx + 1]])
    };
    let right_ch = if ch > 1 { 1 } else { 0 };
    let left: Vec<i16> = (0..frames).map(|f| at(f, 0)).collect();
    let right: Vec<i16> = (0..frames).map(|f| at(f, right_ch)).collect();

    let out_frames = if src_rate == TARGET_RATE { frames } else { ((frames as u64) * TARGET_RATE as u64 / src_rate as u64) as usize };
    let ratio = src_rate as f64 / TARGET_RATE as f64;

    let interp = |chan: &[i16], of: usize| -> i16 {
        if src_rate == TARGET_RATE {
            return chan.get(of).copied().unwrap_or(0);
        }
        let pos = of as f64 * ratio;
        let i = pos.floor() as usize;
        let frac = pos - i as f64;
        let a = chan.get(i).copied().unwrap_or(0) as f64;
        let b = chan.get(i + 1).copied().unwrap_or(a as i16) as f64;
        (a + (b - a) * frac).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
    };

    let mut out = Vec::with_capacity(out_frames * 4);
    for of in 0..out_frames {
        out.extend_from_slice(&interp(&left, of).to_le_bytes());
        out.extend_from_slice(&interp(&right, of).to_le_bytes());
    }
    out
}

/// Resample interleaved 48 kHz **stereo** S16LE to `dst_rate` stereo S16LE (linear).
/// Passthrough when `dst_rate == 48000`. Used to rate-match an announce overlay
/// (always produced at 48 kHz) to a group's capture rate — e.g. 48k→44.1k for a
/// receiver that only does 44.1 kHz. One-shot at announcement start, not per chunk.
pub fn from_48k_stereo_to(pcm: &[u8], dst_rate: u32) -> Vec<u8> {
    if dst_rate == 0 || dst_rate == TARGET_RATE || pcm.len() < 4 {
        return pcm.to_vec();
    }
    let frames = pcm.len() / 4; // stereo S16 = 4 bytes/frame
    let at = |frame: usize, c: usize| -> i16 {
        let idx = frame * 4 + c * 2;
        i16::from_le_bytes([pcm[idx], pcm[idx + 1]])
    };
    let left: Vec<i16> = (0..frames).map(|f| at(f, 0)).collect();
    let right: Vec<i16> = (0..frames).map(|f| at(f, 1)).collect();
    let out_frames = ((frames as u64) * dst_rate as u64 / TARGET_RATE as u64) as usize;
    let ratio = TARGET_RATE as f64 / dst_rate as f64;
    let interp = |chan: &[i16], of: usize| -> i16 {
        let pos = of as f64 * ratio;
        let i = pos.floor() as usize;
        let frac = pos - i as f64;
        let a = chan.get(i).copied().unwrap_or(0) as f64;
        let b = chan.get(i + 1).copied().unwrap_or(a as i16) as f64;
        (a + (b - a) * frac).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
    };
    let mut out = Vec::with_capacity(out_frames * 4);
    for of in 0..out_frames {
        out.extend_from_slice(&interp(&left, of).to_le_bytes());
        out.extend_from_slice(&interp(&right, of).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16(v: &[i16]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
    fn to_i16(b: &[u8]) -> Vec<i16> {
        b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
    }

    #[test]
    fn mono_48k_is_duplicated_to_stereo_same_length() {
        let out = to_48k_stereo_s16le(&s16(&[100, 200, 300]), 48_000, 1);
        assert_eq!(to_i16(&out), vec![100, 100, 200, 200, 300, 300]);
    }

    #[test]
    fn stereo_48k_passes_through() {
        let out = to_48k_stereo_s16le(&s16(&[10, 20, 30, 40]), 48_000, 2);
        assert_eq!(to_i16(&out), vec![10, 20, 30, 40]);
    }

    #[test]
    fn upsamples_24k_to_48k_roughly_doubling_frames() {
        // 4 mono frames @ 24k → ~8 stereo frames @ 48k.
        let out = to_48k_stereo_s16le(&s16(&[0, 100, 200, 300]), 24_000, 1);
        let frames = out.len() / 4;
        assert_eq!(frames, 8);
        // First output frame == first input sample (pos 0).
        assert_eq!(to_i16(&out[0..4]), vec![0, 0]);
    }

    #[test]
    fn keeps_first_two_of_multichannel() {
        // 1 frame, 3 channels [L=10, R=20, C=30] → stereo [10,20].
        let out = to_48k_stereo_s16le(&s16(&[10, 20, 30]), 48_000, 3);
        assert_eq!(to_i16(&out), vec![10, 20]);
    }
}
