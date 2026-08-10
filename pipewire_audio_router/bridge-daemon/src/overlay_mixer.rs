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
// Because only a running per-device relay advances an overlay, a slot on an output
// nothing is streaming would never finish — and would hold that output "occupied"
// in the announce scheduler forever. [`OverlayMixer::reap_stalled`] is the
// watchdog for exactly that (see architecture.md §5.3).
//
// Audio format is fixed to the capture format (`sendspin_capture`): S16LE, 48 kHz,
// stereo. Overlay clip PCM must already be in that format.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long an overlay may make **no progress** before [`OverlayMixer::reap_stalled`]
/// drops it. An overlay is only consumed by a running per-device relay
/// (sendspin_server / ap2_server / pwsink_server); if the targeted output has no
/// live sender, nothing ever advances its cursor and — without this — the slot
/// would sit there forever, holding the output "occupied" in the announce
/// scheduler so every later announcement to it queues behind a clip that can
/// never finish. This is the default grace for an output that is *supposed* to
/// have a live transport already.
pub const OVERLAY_STALL_GRACE: Duration = Duration::from_secs(5);

/// Stall grace for an output whose transport is being opened **on demand** (an
/// unrouted AP2 receiver: pair + SETUP + RECORD + stream start, retried once).
/// The clip legitimately makes no progress until that session is up, so it needs
/// to outlast a full connect (`AP2_CONNECT_TIMEOUT` × attempts + backoff).
pub const OVERLAY_ONDEMAND_GRACE: Duration = Duration::from_secs(40);

/// One active overlay on an output.
struct Overlay {
    id: u64,
    /// Announcement PCM (S16LE/48k/stereo), consumed as the music streams.
    pcm: Vec<u8>,
    cursor: usize,
    /// Music duck gain (0.0–1.0) applied while this overlay plays.
    duck: f32,
    /// How long this overlay may make no progress before it's reaped.
    grace: Duration,
    /// Progress watchdog, driven by the *reaper* (not the RT relay): the cursor
    /// value last seen by [`OverlayMixer::reap_stalled`] and when it last moved.
    /// Keeping the clock read out of `mix_into` keeps the RT relay path free of
    /// per-chunk timekeeping — the cursor is already mutex-protected, so the
    /// reaper can sample it for free.
    watch_cursor: usize,
    watch_since: Instant,
}

/// Per-output overlay slots. One process-global instance shared by every
/// per-device server and the API (announcements are addressed by output name).
#[derive(Default)]
pub struct OverlayMixer {
    slots: Mutex<HashMap<String, Overlay>>,
    /// Overlays that reached the end of their clip since the last drain, so the
    /// caller can tell the scheduler the announcement finished on that output.
    finished: Mutex<Vec<(String, u64)>>,
    /// Per-output capture rate (Hz), published by the AP2 sender when it starts a
    /// group. Overlay clips arrive at 48 kHz; `start` resamples each to the target
    /// output's rate so `mix_into` (which adds sample-for-sample on the RT relay)
    /// always sees music and overlay at the same rate. Absent ⇒ 48 kHz (sendspin,
    /// and AP2 groups running at 48 kHz — the common case, no resampling).
    output_rate: Mutex<HashMap<String, u32>>,
}

impl OverlayMixer {
    /// The process-global mixer.
    pub fn global() -> &'static OverlayMixer {
        static M: OnceLock<OverlayMixer> = OnceLock::new();
        M.get_or_init(OverlayMixer::default)
    }

    /// Start (or replace) an overlay on `output`. `pcm` is 48 kHz stereo S16LE; if
    /// the output's capture runs at a different rate (a 44.1 kHz AP2 group), the
    /// clip is resampled once here so it mixes sample-for-sample with the music.
    pub fn start(&self, output: &str, id: u64, pcm: Vec<u8>, duck: f32) {
        self.start_with_grace(output, id, pcm, duck, OVERLAY_STALL_GRACE);
    }

    /// [`Self::start`] with an explicit stall `grace` — how long the clip may make
    /// no progress before [`Self::reap_stalled`] drops it. Callers that first open
    /// a transport on demand pass [`OVERLAY_ONDEMAND_GRACE`] so the connect has
    /// time to complete before the watchdog fires.
    pub fn start_with_grace(&self, output: &str, id: u64, pcm: Vec<u8>, duck: f32, grace: Duration) {
        let rate = self.output_rate.lock().unwrap().get(output).copied().unwrap_or(48_000);
        let pcm = crate::resample::from_48k_stereo_to(&pcm, rate);
        self.slots.lock().unwrap().insert(
            output.to_string(),
            Overlay { id, pcm, cursor: 0, duck: duck.clamp(0.0, 1.0), grace, watch_cursor: 0, watch_since: Instant::now() },
        );
    }

    /// Publish an output's current capture rate (Hz) so `start` can rate-match its
    /// overlay clips. Called by the AP2 sender when a group (re)starts.
    pub fn set_output_rate(&self, output: &str, rate: u32) {
        self.output_rate.lock().unwrap().insert(output.to_string(), rate);
    }

    /// Forget an output's rate (back to the 48 kHz default) — on AP2 teardown.
    pub fn clear_output_rate(&self, output: &str) {
        self.output_rate.lock().unwrap().remove(output);
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

    /// Mix one music chunk for `output` **into a caller-provided buffer** (reused
    /// across chunks by the sendspin relay, so the per-chunk mix does no
    /// allocation on the RT relay thread). If an overlay is active, writes
    /// `duck(music) + overlay` for the next `music.len()` bytes (padding the
    /// final chunk with silence) into `out`, advances the overlay, and returns
    /// `true`; when the clip is exhausted the slot is removed and recorded in
    /// [`Self::take_finished`]. Returns `false` (leaving `out` untouched) if no
    /// overlay is active — the caller sends plain music.
    pub fn mix_into(&self, output: &str, music: &[u8], out: &mut Vec<u8>) -> bool {
        let mut slots = self.slots.lock().unwrap();
        let Some(ov) = slots.get_mut(output) else {
            return false;
        };

        // Overlay slice matching this music chunk, zero-padded if the clip ends.
        let remaining = &ov.pcm[ov.cursor.min(ov.pcm.len())..];
        let take = remaining.len().min(music.len());
        let overlay_chunk = &remaining[..take];
        mix_s16le_into(music, overlay_chunk, ov.duck, out);
        ov.cursor += take;

        let done = ov.cursor >= ov.pcm.len();
        if done {
            let id = ov.id;
            slots.remove(output);
            self.finished.lock().unwrap().push((output.to_string(), id));
        }
        true
    }

    /// Allocating convenience wrapper over [`Self::mix_into`] — returns `None`
    /// when no overlay is active. Used by tests; the hot path uses `mix_into`.
    #[cfg(test)]
    pub fn mix(&self, output: &str, music: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.mix_into(output, music, &mut out).then_some(out)
    }

    /// Drain the outputs whose overlay finished since the last call.
    pub fn take_finished(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut *self.finished.lock().unwrap())
    }

    /// Drop every overlay that has made no progress for longer than its grace and
    /// return them as `(output, id)` — the safety net for an output nothing is
    /// streaming (no per-device sender consuming it, or one that died mid-clip).
    /// The caller (announce.rs) treats a reaped overlay like a finished one, so the
    /// scheduler releases the output instead of holding it occupied forever.
    ///
    /// Called from the announce tick, so "no progress" is sampled at that cadence:
    /// each call advances the watchdog when the cursor moved, and only fires once
    /// the cursor has been frozen for the whole grace window.
    pub fn reap_stalled(&self) -> Vec<(String, u64)> {
        let mut slots = self.slots.lock().unwrap();
        let now = Instant::now();
        let mut reaped = Vec::new();
        slots.retain(|output, ov| {
            if ov.cursor != ov.watch_cursor {
                ov.watch_cursor = ov.cursor;
                ov.watch_since = now;
                return true;
            }
            if now.duration_since(ov.watch_since) <= ov.grace {
                return true;
            }
            reaped.push((output.clone(), ov.id));
            false
        });
        reaped
    }
}

/// Mix a music chunk with an overlay chunk (both S16LE) into `out`:
/// `music*duck + overlay`, saturating to i16. `overlay` may be shorter than
/// `music` (treated as trailing silence); output length matches `music`. `out`
/// is cleared first and reused, so a caller looping over chunks allocates at
/// most once (capacity is retained).
fn mix_s16le_into(music: &[u8], overlay: &[u8], duck: f32, out: &mut Vec<u8>) {
    let n = music.len() / 2;
    out.clear();
    out.reserve(n * 2);
    for i in 0..n {
        let m = i16::from_le_bytes([music[2 * i], music[2 * i + 1]]) as f32;
        let o = if 2 * i + 1 < overlay.len() { i16::from_le_bytes([overlay[2 * i], overlay[2 * i + 1]]) as f32 } else { 0.0 };
        let mixed = (m * duck + o).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        out.extend_from_slice(&mixed.to_le_bytes());
    }
}

/// Allocating convenience wrapper over [`mix_s16le_into`], used by tests.
#[cfg(test)]
fn mix_s16le(music: &[u8], overlay: &[u8], duck: f32) -> Vec<u8> {
    let mut out = Vec::new();
    mix_s16le_into(music, overlay, duck, &mut out);
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
    fn reaps_an_overlay_nothing_consumes() {
        let m = OverlayMixer::default();
        // Zero grace: the very first reap sees a frozen cursor and drops it.
        m.start_with_grace("k", 9, s16(&[100, 200]), 1.0, Duration::ZERO);
        assert_eq!(m.reap_stalled(), vec![("k".to_string(), 9)]);
        assert!(!m.is_active("k"), "reaped slot is gone");
        assert_eq!(m.reap_stalled(), vec![], "nothing left to reap");
        // Reaping is NOT a finish — the caller distinguishes them for logging.
        assert_eq!(m.take_finished(), vec![]);
    }

    #[test]
    fn progress_resets_the_stall_watchdog() {
        let m = OverlayMixer::default();
        m.start_with_grace("k", 9, s16(&[100, 200, 300, 400]), 1.0, Duration::ZERO);
        // A consumed chunk moved the cursor, so this reap only re-arms the
        // watchdog — even at zero grace, a *progressing* overlay is never dropped.
        m.mix("k", &s16(&[1000])).unwrap();
        assert_eq!(m.reap_stalled(), vec![], "cursor moved → not stalled");
        assert!(m.is_active("k"));
        // Progress then stops → the next reap (grace 0) drops it.
        assert_eq!(m.reap_stalled(), vec![("k".to_string(), 9)]);
    }

    #[test]
    fn a_long_grace_survives_a_stalled_reap() {
        let m = OverlayMixer::default();
        m.start_with_grace("k", 9, s16(&[100]), 1.0, Duration::from_secs(60));
        assert_eq!(m.reap_stalled(), vec![], "still inside its grace window");
        assert!(m.is_active("k"));
    }

    #[test]
    fn test_tone_has_expected_length_and_format() {
        // 0.5s @ 48kHz stereo S16 = 24000 frames * 4 bytes.
        let t = test_tone(0.5, 440.0, 0.3);
        assert_eq!(t.len(), 24_000 * 4);
    }
}
