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
// no-ops for sendspin per-device.
//
// There is a second, independent producer of ducking: a **duck hold** (see
// `DuckHold`), an open-ended lease that attenuates one output's music with no clip
// of its own. That is what voice ducking needs — an assistant speaking through its
// own speaker gives the router nothing to play, only music to get out of the way —
// and it composes with an announcement overlay by taking the stronger of the two
// gains. Holds live in their own map so no overlay bookkeeping path (the progress
// watchdog, the finished-drain, `stop`) can see them.
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

/// Identifies one **duck hold** — an open-ended request to keep an output's music
/// ducked with no clip of its own (voice_duck: a voice assistant is talking in
/// that room and speaks through its *own* speaker, so the router has nothing to
/// play, only music to get out of the way).
pub type DuckHoldId = u64;

/// Default lease length for a duck hold. The holder is expected to renew well
/// inside this; the TTL exists so a holder that dies mid-turn (Home Assistant
/// restarting, the network dropping, the integration reloading) cannot leave
/// music ducked forever.
pub const DUCK_HOLD_TTL: Duration = Duration::from_secs(30);

/// One live duck hold on one output.
struct DuckHold {
    id: DuckHoldId,
    /// Music gain (0.0–1.0) while this hold is live.
    level: f32,
    /// Lease deadline; extended by [`OverlayMixer::renew_duck`], enforced by
    /// [`OverlayMixer::expire_ducks`].
    expires: Instant,
}

/// Per-output overlay slots. One process-global instance shared by every
/// per-device server and the API (announcements are addressed by output name).
#[derive(Default)]
pub struct OverlayMixer {
    slots: Mutex<HashMap<String, Overlay>>,
    /// Live duck holds per output, newest last. Deliberately a **separate** map
    /// from `slots`: a hold has no clip and no cursor, so it must never be
    /// reachable by the progress watchdog ([`Self::reap_stalled`]) or the
    /// finished-drain — keeping it out of `Overlay` buys that structurally
    /// instead of by a flag check on every path. A hold and an announcement
    /// overlay can be live on the same output at once; the mix takes the
    /// strongest (lowest) gain of the two, so a doorbell stays audible over
    /// already-ducked music.
    ///
    /// Holds are keyed by output name and outlive any relay, so a hold placed on
    /// an output nothing is streaming simply applies later, when music starts.
    ducks: Mutex<HashMap<String, Vec<DuckHold>>>,
    next_duck_id: Mutex<DuckHoldId>,
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

    /// Start a duck hold on every output in `targets` and return its id. One id
    /// covers all of them, so a single release/renew ends or extends the whole
    /// turn. `level` is the music gain (0.0–1.0); `ttl` is the lease.
    ///
    /// A hold on an output with nothing playing is inaudible and harmless — that
    /// is deliberate, so the caller need not know which speakers are live, and
    /// music that *starts* mid-turn comes up already ducked.
    ///
    /// A hold on an output held for **alignment** is reported to the holder
    /// (`align_group`), because it is the interferer nothing else can see: a duck
    /// hold has no clip and no occupancy, so the announce arbiter's reservation
    /// never hears about it, and an assistant turn would otherwise attenuate a
    /// calibration tone and be diagnosed as an unstable hand (plan §12.3, §2.3.2).
    /// Reporting here rather than at the API handler is deliberate: this is the one
    /// place a hold can be born, so the report cannot be forgotten by a future
    /// caller.
    pub fn start_duck(&self, targets: &[String], level: f32, ttl: Duration) -> DuckHoldId {
        let id = {
            let mut next = self.next_duck_id.lock().unwrap();
            *next += 1;
            *next
        };
        let expires = Instant::now() + ttl;
        let level = level.clamp(0.0, 1.0);
        {
            let mut ducks = self.ducks.lock().unwrap();
            for output in targets {
                ducks.entry(output.clone()).or_default().push(DuckHold { id, level, expires });
            }
        }
        // Outside the `ducks` lock: the registry takes its own, and no lock order
        // between the two is worth establishing for a report.
        let holds = crate::align::group::registry();
        for output in targets {
            holds.note(output, crate::align::group::InterferenceCause::DuckHold { hold: id });
        }
        id
    }

    /// Extend a hold's lease by `ttl` from now. Returns `false` if the id is
    /// unknown (already released or expired) so the caller can start a new one
    /// rather than silently ducking nothing.
    pub fn renew_duck(&self, id: DuckHoldId, ttl: Duration) -> bool {
        let expires = Instant::now() + ttl;
        let mut ducks = self.ducks.lock().unwrap();
        let mut found = false;
        for holds in ducks.values_mut() {
            for h in holds.iter_mut().filter(|h| h.id == id) {
                h.expires = expires;
                found = true;
            }
        }
        found
    }

    /// Release a hold, returning the outputs it covered (empty ⇒ unknown id, i.e.
    /// already released or expired). The caller re-asserts each returned output's
    /// duck state, since an agent-backed host is told an absolute depth rather
    /// than ref-counting for us.
    pub fn release_duck(&self, id: DuckHoldId) -> Vec<String> {
        let mut ducks = self.ducks.lock().unwrap();
        let mut affected = Vec::new();
        for (output, holds) in ducks.iter_mut() {
            let before = holds.len();
            holds.retain(|h| h.id != id);
            if holds.len() != before {
                affected.push(output.clone());
            }
        }
        ducks.retain(|_, holds| !holds.is_empty());
        affected.sort();
        affected
    }

    /// Drop every hold whose lease has run out, as `(id, outputs it covered)` —
    /// one entry per expired hold, so the caller can log it once and re-assert
    /// those outputs. Driven from the announce tick: the safety net for a holder
    /// that stopped renewing without releasing.
    pub fn expire_ducks(&self) -> Vec<(DuckHoldId, Vec<String>)> {
        let now = Instant::now();
        let mut expired: Vec<(DuckHoldId, Vec<String>)> = Vec::new();
        let mut ducks = self.ducks.lock().unwrap();
        for (output, holds) in ducks.iter_mut() {
            holds.retain(|h| {
                if h.expires <= now {
                    match expired.iter_mut().find(|(id, _)| *id == h.id) {
                        Some((_, outs)) => outs.push(output.clone()),
                        None => expired.push((h.id, vec![output.clone()])),
                    }
                    false
                } else {
                    true
                }
            });
        }
        ducks.retain(|_, holds| !holds.is_empty());
        for (_, outs) in &mut expired {
            outs.sort();
        }
        expired
    }

    /// The duck gain in force on `output` right now — the strongest of every live
    /// hold and any announcement overlay's own duck — or `None` when its music
    /// plays untouched. This is exactly what [`Self::mix_into`] applies, exposed
    /// because an agent-backed pw-sink host is told an *absolute* depth for the
    /// foreign audio on its sink (`pwsink_agent`), so whoever changes one duck
    /// has to re-assert the aggregate rather than assume ref-counting.
    pub fn effective_duck(&self, output: &str) -> Option<f32> {
        let hold = Self::duck_gain(&self.ducks.lock().unwrap(), output);
        let overlay = self.slots.lock().unwrap().get(output).map(|ov| ov.duck);
        match (hold, overlay) {
            (Some(h), Some(o)) => Some(h.min(o)),
            (h, o) => h.or(o),
        }
    }

    /// Live holds as `(output, id, level)`, sorted by output — for `GET /api/duck`
    /// and for debugging "why is this quiet?".
    pub fn duck_holds(&self) -> Vec<(String, DuckHoldId, f32)> {
        let ducks = self.ducks.lock().unwrap();
        let mut v: Vec<(String, DuckHoldId, f32)> =
            ducks.iter().flat_map(|(o, hs)| hs.iter().map(move |h| (o.clone(), h.id, h.level))).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        v
    }

    /// The strongest (lowest) duck gain held on `output`, or `None` if none is.
    fn duck_gain(ducks: &HashMap<String, Vec<DuckHold>>, output: &str) -> Option<f32> {
        let holds = ducks.get(output)?;
        holds.iter().map(|h| h.level).fold(None, |acc: Option<f32>, l| Some(acc.map_or(l, |a| a.min(l))))
    }

    /// Mix one music chunk for `output` **into a caller-provided buffer** (reused
    /// across chunks by the sendspin relay, so the per-chunk mix does no
    /// allocation on the RT relay thread). Returns `true` when it wrote something,
    /// `false` (leaving `out` untouched) when the caller should send plain music.
    ///
    /// Three cases, in order of how loud the result is:
    /// * an announcement overlay is active → `duck(music) + overlay` for the next
    ///   `music.len()` bytes (padding the final chunk with silence), advancing the
    ///   overlay; when the clip is exhausted the slot is removed and recorded in
    ///   [`Self::take_finished`]. If a duck hold is *also* live, the stronger
    ///   (lower) of the two gains applies — the clip itself is untouched;
    /// * only a duck hold is live → `duck(music)`, no cursor to advance and
    ///   nothing to finish (its lifecycle is the lease, not progress);
    /// * neither → `false`.
    pub fn mix_into(&self, output: &str, music: &[u8], out: &mut Vec<u8>) -> bool {
        let mut slots = self.slots.lock().unwrap();
        let hold_gain = Self::duck_gain(&self.ducks.lock().unwrap(), output);
        let Some(ov) = slots.get_mut(output) else {
            // Duck-only: attenuate the music, leave every overlay bookkeeping
            // path alone.
            let Some(gain) = hold_gain else { return false };
            mix_s16le_into(music, &[], gain, out);
            return true;
        };

        // Overlay slice matching this music chunk, zero-padded if the clip ends.
        let remaining = &ov.pcm[ov.cursor.min(ov.pcm.len())..];
        let take = remaining.len().min(music.len());
        let overlay_chunk = &remaining[..take];
        let duck = hold_gain.map_or(ov.duck, |g| g.min(ov.duck));
        mix_s16le_into(music, overlay_chunk, duck, out);
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
        bytes.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect()
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

    // --- duck holds (voice ducking) ---

    fn o(s: &str) -> String {
        s.to_string()
    }
    fn out(s: &str) -> Vec<String> {
        vec![o(s)]
    }
    const TTL: Duration = Duration::from_secs(30);

    #[test]
    fn a_duck_hold_alone_attenuates_the_music() {
        let m = OverlayMixer::default();
        assert!(m.mix("k", &s16(&[1000])).is_none(), "no hold yet → plain music");
        m.start_duck(&out("k"), 0.25, TTL);
        // Ducked, and nothing else: no overlay started, nothing to finish.
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000, 2000])).unwrap()), vec![250, 500]);
        assert!(!m.is_active("k"), "a hold is not an overlay slot");
        assert_eq!(m.take_finished(), vec![]);
    }

    #[test]
    fn releasing_the_hold_restores_full_gain() {
        let m = OverlayMixer::default();
        let id = m.start_duck(&out("k"), 0.5, TTL);
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![500]);
        assert!(!m.release_duck(id).is_empty());
        assert!(m.mix("k", &s16(&[1000])).is_none(), "released → plain music again");
        assert!(m.release_duck(id).is_empty(), "second release is a no-op");
    }

    #[test]
    fn one_hold_id_covers_every_target_and_releases_them_together() {
        let m = OverlayMixer::default();
        let id = m.start_duck(&[o("k"), o("bath")], 0.5, TTL);
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![500]);
        assert_eq!(to_i16(&m.mix("bath", &s16(&[1000])).unwrap()), vec![500]);
        m.release_duck(id);
        assert!(m.mix("k", &s16(&[1000])).is_none());
        assert!(m.mix("bath", &s16(&[1000])).is_none());
    }

    #[test]
    fn overlapping_holds_take_the_strongest_and_refcount() {
        let m = OverlayMixer::default();
        let quiet = m.start_duck(&out("k"), 0.2, TTL);
        let mild = m.start_duck(&out("k"), 0.8, TTL);
        // Strongest (lowest) gain wins while both are live.
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![200]);
        // Dropping the stronger one leaves the milder duck — NOT full volume.
        m.release_duck(quiet);
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![800]);
        m.release_duck(mild);
        assert!(m.mix("k", &s16(&[1000])).is_none());
    }

    #[test]
    fn a_hold_and_an_announcement_compose_at_the_stronger_duck() {
        let m = OverlayMixer::default();
        // Announcement ducks to 0.5; a voice turn wants 0.2 on the same output.
        m.start("k", 7, s16(&[100, 100]), 0.5);
        m.start_duck(&out("k"), 0.2, TTL);
        // music*0.2 + overlay — the clip itself is never attenuated.
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![300]);
        // The overlay still completes normally...
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![300]);
        assert_eq!(m.take_finished(), vec![("k".to_string(), 7)]);
        // ...and the hold outlives it, still ducking.
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![200]);
    }

    #[test]
    fn an_announcements_own_duck_still_applies_with_no_hold() {
        let m = OverlayMixer::default();
        m.start("k", 1, s16(&[0]), 0.5);
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![500]);
    }

    #[test]
    fn an_expired_lease_un_ducks_on_its_own() {
        let m = OverlayMixer::default();
        let id = m.start_duck(&out("k"), 0.25, Duration::ZERO);
        assert_eq!(m.expire_ducks(), vec![(id, vec![o("k")])], "lease already up");
        assert!(m.mix("k", &s16(&[1000])).is_none(), "music back at full level");
        assert!(m.expire_ducks().is_empty(), "nothing left to expire");
        // A live lease is never expired.
        m.start_duck(&out("k"), 0.25, Duration::from_secs(60));
        assert!(m.expire_ducks().is_empty());
    }

    #[test]
    fn renewing_keeps_a_hold_alive_and_reports_unknown_ids() {
        let m = OverlayMixer::default();
        let id = m.start_duck(&out("k"), 0.25, Duration::ZERO);
        assert!(m.renew_duck(id, Duration::from_secs(60)));
        assert!(m.expire_ducks().is_empty(), "renewed before the tick saw it");
        assert!(m.mix("k", &s16(&[1000])).is_some());
        assert!(!m.renew_duck(999, TTL), "unknown id → caller starts a new hold");
    }

    #[test]
    fn a_hold_survives_with_no_relay_and_applies_once_one_starts() {
        let m = OverlayMixer::default();
        // Nothing is streaming this output: no mix calls at all, and the stall
        // watchdog (an overlay concept) must not touch the hold.
        let id = m.start_duck(&out("k"), 0.25, TTL);
        assert_eq!(m.reap_stalled(), vec![]);
        assert_eq!(m.duck_holds(), vec![("k".to_string(), id, 0.25)]);
        // Music starts later → it comes up already ducked.
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![250]);
    }

    #[test]
    fn holds_are_invisible_to_the_overlay_watchdog_and_stop() {
        let m = OverlayMixer::default();
        m.start_duck(&out("k"), 0.25, TTL);
        m.start_with_grace("k", 9, s16(&[100]), 1.0, Duration::ZERO);
        // Reaping a stalled overlay leaves the hold in place.
        assert_eq!(m.reap_stalled(), vec![("k".to_string(), 9)]);
        assert_eq!(m.duck_holds().len(), 1);
        assert!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()) == vec![250]);
        // Same for an explicit overlay stop.
        m.start("k", 10, s16(&[0, 0]), 1.0);
        assert_eq!(m.stop("k"), Some(10));
        assert_eq!(m.duck_holds().len(), 1);
    }

    #[test]
    fn effective_duck_is_the_aggregate_an_agent_host_must_be_told() {
        let m = OverlayMixer::default();
        assert_eq!(m.effective_duck("k"), None, "nothing ducking → agent un-ducked");
        // Hold only, overlay only, and both: always the strongest.
        let hold = m.start_duck(&out("k"), 0.4, TTL);
        assert_eq!(m.effective_duck("k"), Some(0.4));
        m.start("k", 1, s16(&[0, 0]), 0.2);
        assert_eq!(m.effective_duck("k"), Some(0.2));
        // The announcement ending must NOT read as "un-duck the host" while the
        // voice hold is still live — that's the clobbering this exists to prevent.
        m.stop("k");
        assert_eq!(m.effective_duck("k"), Some(0.4));
        assert_eq!(m.release_duck(hold), vec![o("k")]);
        assert_eq!(m.effective_duck("k"), None);
    }

    #[test]
    fn expiry_reports_every_output_a_hold_covered() {
        let m = OverlayMixer::default();
        // One hold over two outputs → one entry naming both, so the caller can
        // re-assert each agent host exactly once.
        let id = m.start_duck(&[o("bath"), o("k")], 0.25, Duration::ZERO);
        assert_eq!(m.expire_ducks(), vec![(id, vec![o("bath"), o("k")])]);
    }

    #[test]
    fn duck_level_is_clamped() {
        let m = OverlayMixer::default();
        m.start_duck(&out("k"), -1.0, TTL);
        assert_eq!(to_i16(&m.mix("k", &s16(&[1000])).unwrap()), vec![0], "clamped to silence, not negated");
    }

    #[test]
    fn test_tone_has_expected_length_and_format() {
        // 0.5s @ 48kHz stereo S16 = 24000 frames * 4 bytes.
        let t = test_tone(0.5, 440.0, 0.3);
        assert_eq!(t.len(), 24_000 * 4);
    }
}
