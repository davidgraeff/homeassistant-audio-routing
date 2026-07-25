// ABOUTME: Per-output announcement overlay mixer for the per-device-senders path.
// ABOUTME: While an overlay is active on an output, that device's frame becomes
// ABOUTME: duck(music)+overlay; its groupmates keep plain music — the per-speaker
// ABOUTME: announcement/duck capability (AG delivery for sendspin).
//
// The per-device capture loop (`sendspin_server::start_server_per_device`) drives
// every device's single-member `Group` from one PCM source. For a device with an
// active overlay it calls [`OverlayMixer::mix`] to get a per-device frame
// (ducked music + the next slice of the announcement clip) instead of the plain
// music chunk; the shared timeline still stamps one timestamp, so music stays
// sample-accurate across the group while one speaker carries the announcement.
//
// Ducking is *implicit* here: it happens inside the mix only while an overlay is
// active on that output, so the scheduler's DuckMusic/UnduckMusic actions are
// no-ops for sendspin per-device (RAOP per-output duck is separate, O-E).
//
// Audio format is fixed to the capture format (`sendspin_capture`): S16LE, 48 kHz,
// stereo. Overlay clip PCM must already be in that format.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One active overlay on an output.
struct Overlay {
    id: u64,
    /// Announcement PCM (S16LE/48k/stereo), consumed as the music streams.
    pcm: Vec<u8>,
    cursor: usize,
    /// Music duck gain (0.0–1.0) applied while this overlay plays.
    duck: f32,
}

/// Per-output overlay slots. One process-global instance shared by every
/// per-device server and the API (announcements are addressed by output name).
#[derive(Default)]
pub struct OverlayMixer {
    slots: Mutex<HashMap<String, Overlay>>,
    /// Overlays that reached the end of their clip since the last drain, so the
    /// caller can tell the scheduler the announcement finished on that output.
    finished: Mutex<Vec<(String, u64)>>,
}

impl OverlayMixer {
    /// The process-global mixer.
    pub fn global() -> &'static OverlayMixer {
        static M: OnceLock<OverlayMixer> = OnceLock::new();
        M.get_or_init(OverlayMixer::default)
    }

    /// Start (or replace) an overlay on `output`.
    pub fn start(&self, output: &str, id: u64, pcm: Vec<u8>, duck: f32) {
        self.slots.lock().unwrap().insert(
            output.to_string(),
            Overlay { id, pcm, cursor: 0, duck: duck.clamp(0.0, 1.0) },
        );
    }

    /// Stop the overlay on `output` (if any); returns its id.
    pub fn stop(&self, output: &str) -> Option<u64> {
        self.slots.lock().unwrap().remove(output).map(|o| o.id)
    }

    /// Whether an overlay is currently active on `output`.
    #[allow(dead_code)] // used by the AnnounceScheduler wiring (step 3b)
    pub fn is_active(&self, output: &str) -> bool {
        self.slots.lock().unwrap().contains_key(output)
    }

    /// Mix one music chunk for `output`. If an overlay is active, returns
    /// `duck(music) + overlay` for the next `music.len()` bytes (padding the
    /// final chunk with silence), advancing the overlay; when the clip is
    /// exhausted the slot is removed and recorded in [`Self::take_finished`].
    /// Returns `None` if no overlay is active (caller sends plain music).
    pub fn mix(&self, output: &str, music: &[u8]) -> Option<Vec<u8>> {
        let mut slots = self.slots.lock().unwrap();
        let ov = slots.get_mut(output)?;

        // Overlay slice matching this music chunk, zero-padded if the clip ends.
        let remaining = &ov.pcm[ov.cursor.min(ov.pcm.len())..];
        let take = remaining.len().min(music.len());
        let overlay_chunk = &remaining[..take];
        let mixed = mix_s16le(music, overlay_chunk, ov.duck);
        ov.cursor += take;

        let done = ov.cursor >= ov.pcm.len();
        if done {
            let id = ov.id;
            slots.remove(output);
            self.finished.lock().unwrap().push((output.to_string(), id));
        }
        Some(mixed)
    }

    /// Drain the outputs whose overlay finished since the last call.
    pub fn take_finished(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut *self.finished.lock().unwrap())
    }
}

/// Mix a music chunk with an overlay chunk (both S16LE): `music*duck + overlay`,
/// saturating to i16. `overlay` may be shorter than `music` (treated as trailing
/// silence); output length matches `music`.
fn mix_s16le(music: &[u8], overlay: &[u8], duck: f32) -> Vec<u8> {
    let n = music.len() / 2;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let m = i16::from_le_bytes([music[2 * i], music[2 * i + 1]]) as f32;
        let o = if 2 * i + 1 < overlay.len() {
            i16::from_le_bytes([overlay[2 * i], overlay[2 * i + 1]]) as f32
        } else {
            0.0
        };
        let mixed = (m * duck + o).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        out.extend_from_slice(&mixed.to_le_bytes());
    }
    out
}

/// Generate a stereo S16LE test tone at the capture format (48 kHz), for the
/// overlay spike: `seconds` of a `freq` Hz sine at `amplitude` (0.0–1.0).
pub fn test_tone(seconds: f32, freq: f32, amplitude: f32) -> Vec<u8> {
    let rate = crate::sendspin_capture::SAMPLE_RATE as f32;
    let frames = (rate * seconds.max(0.0)) as usize;
    let amp = amplitude.clamp(0.0, 1.0) * i16::MAX as f32;
    let mut v = Vec::with_capacity(frames * 4);
    let two_pi_f = std::f32::consts::TAU * freq;
    for i in 0..frames {
        let s = (two_pi_f * (i as f32 / rate)).sin() * amp;
        let val = (s as i16).to_le_bytes();
        v.extend_from_slice(&val); // L
        v.extend_from_slice(&val); // R
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
    fn to_i16(bytes: &[u8]) -> Vec<i16> {
        bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
    }

    #[test]
    fn ducks_music_and_adds_overlay() {
        // music=[1000,2000], overlay=[100,100], duck=0.5 → [600, 1100]
        let out = mix_s16le(&s16(&[1000, 2000]), &s16(&[100, 100]), 0.5);
        assert_eq!(to_i16(&out), vec![600, 1100]);
    }

    #[test]
    fn saturates_instead_of_wrapping() {
        // 30000*1.0 + 10000 = 40000 → clamp to i16::MAX (32767).
        let out = mix_s16le(&s16(&[30000]), &s16(&[10000]), 1.0);
        assert_eq!(to_i16(&out), vec![i16::MAX]);
    }

    #[test]
    fn shorter_overlay_is_silence_padded() {
        // overlay only covers the first sample; second is music*duck only.
        let out = mix_s16le(&s16(&[1000, 2000]), &s16(&[500]), 0.5);
        assert_eq!(to_i16(&out), vec![1000, 1000]);
    }

    #[test]
    fn mix_advances_cursor_and_finishes_when_exhausted() {
        let m = OverlayMixer::default();
        // 2 samples of overlay; music chunk is 1 sample → two mix calls.
        m.start("k", 7, s16(&[100, 200]), 1.0);
        let a = m.mix("k", &s16(&[1000])).unwrap();
        assert_eq!(to_i16(&a), vec![1100]); // 1000*1.0 + 100
        assert!(m.is_active("k"), "still active after first chunk");
        let b = m.mix("k", &s16(&[1000])).unwrap();
        assert_eq!(to_i16(&b), vec![1200]); // 1000*1.0 + 200
        assert!(!m.is_active("k"), "exhausted → slot removed");
        assert_eq!(m.take_finished(), vec![("k".to_string(), 7)]);
        assert_eq!(m.take_finished(), vec![], "drained");
    }

    #[test]
    fn no_overlay_returns_none() {
        let m = OverlayMixer::default();
        assert!(m.mix("k", &s16(&[1000])).is_none());
    }

    #[test]
    fn stop_removes_the_overlay() {
        let m = OverlayMixer::default();
        m.start("k", 1, s16(&[1, 2, 3, 4]), 1.0);
        assert_eq!(m.stop("k"), Some(1));
        assert!(!m.is_active("k"));
        assert_eq!(m.stop("k"), None);
    }

    #[test]
    fn test_tone_has_expected_length_and_format() {
        // 0.5s @ 48kHz stereo S16 = 24000 frames * 4 bytes.
        let t = test_tone(0.5, 440.0, 0.3);
        assert_eq!(t.len(), 24_000 * 4);
    }
}
