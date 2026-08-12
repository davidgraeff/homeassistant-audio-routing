//! Microphone ingest for the alignment session (docs/mic-alignment-plan.md §4).
//!
//! The user's phone captures audio in the browser and streams it here over a
//! binary WebSocket; this module owns the socket, validates the stream, and hands
//! contiguous mono PCM to the estimator (`align_estimator`).
//!
//! The stream must be **gapless** to be usable: every measurement is a relative
//! time within one continuous capture (plan §3), so a dropped chunk shifts
//! everything after it. Chunks therefore carry a sequence number and a gap
//! invalidates the window in progress rather than silently corrupting the result.
//!
//! ## Wire protocol (`GET /api/align/mic/ws`)
//!
//! 1. Client → server, one **text** frame: `{"sampleRate":48000,"channelCount":1}`
//!    (snake_case keys accepted too). 48 kHz and 44.1 kHz are both valid — iOS
//!    hands out 44.1 kHz `AudioContext`s and resampling in the browser would only
//!    add an unknown group delay to the thing being measured.
//! 2. Server → client, one **text** frame: `{"type":"ready","sample_rate":48000,
//!    "capacity_frames":480000}`, or `{"type":"error","reason":…}` followed by a
//!    close frame carrying the same reason.
//! 3. Client → server, **binary** frames:
//!    `[u32 LE sequence number][Int16 LE mono samples…]` — ~20 ms per frame.
//!    The sequence number starts anywhere and must increment by exactly 1; any
//!    other step is a gap.
//!
//! ## One socket at a time
//!
//! Two concurrent captures would interleave into garbage, so a second connection
//! is **rejected** with a close reason rather than silently accepted. Closing the
//! socket marks the ingest disconnected but touches nothing else: the alignment
//! session (`align/calibrate/mod.rs`) survives, because the user may just be switching
//! modes, and its own 15 min safety timeout still governs teardown.
//!
//! ## What the consumer gets
//!
//! [`MicIngest`] is a process-global, cloneable handle ([`shared`]). Its
//! consumer-facing surface is deliberately tiny, and is the W3 hand-off contract:
//! mono `f32` samples as a contiguous window ([`MicIngest::window`] /
//! [`MicIngest::window_from`]), the running frame index of the window's first
//! sample, the capture rate, and the two quality flags a measurement must respect
//! — a sequence gap inside the window, and clipping inside the window (plan §5.5
//! refuses to measure on either).

use crate::util::locks::LockRecover;
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

/// How much recent audio the ring holds, in seconds.
///
/// The calibration pattern is a 2 s loop (`align/calibrate/mod.rs` `PATTERN_SECS`) and the
/// estimator averages over "several loop periods" (plan §5.3), so a window worth
/// keeping is a handful of periods long. 10 s = 5 pattern periods: enough for the
/// estimator to average, and enough slack that a consumer polling at a lazy 1 Hz
/// still finds the window it asked for. At 48 kHz mono `f32` that is 1.92 MB —
/// allocated once when a socket connects, never grown afterwards.
const RING_SECS: usize = 10;

/// Accepted capture rates. 48 kHz is the norm; 44.1 kHz is what iOS gives.
const RATES: [u32; 2] = [48_000, 44_100];

/// Longest binary frame accepted, in bytes. A 20 ms block at 48 kHz is 964 B;
/// this leaves two orders of magnitude of headroom while still bounding what one
/// frame can make us allocate.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Peak decay applied per received block (~20 ms), so the UI meter falls at a
/// readable speed instead of flickering: ×0.85 per block ≈ −20 dB in 300 ms.
const PEAK_DECAY: f32 = 0.85;

/// The client's opening frame. Both key styles are accepted — the browser sends
/// camelCase (it is reading `AudioContext.sampleRate`), the rest of this API is
/// snake_case, and there is no value in making that a failure mode.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicHello {
    #[serde(alias = "sample_rate")]
    pub sample_rate: u32,
    #[serde(alias = "channel_count", default = "one")]
    pub channel_count: u16,
}

fn one() -> u16 {
    1
}

impl MicHello {
    /// Rejects anything the ingest cannot honour. Mono only: the estimator works
    /// on one channel and downmixing here would hide a mis-configured capture.
    fn validate(&self) -> Result<(), String> {
        if !RATES.contains(&self.sample_rate) {
            return Err(format!("unsupported sample rate {} (expected 48000 or 44100)", self.sample_rate));
        }
        if self.channel_count != 1 {
            return Err(format!("expected mono capture, got {} channels", self.channel_count));
        }
        Ok(())
    }
}

/// Ingest status for the UI's level meter and pre-flight readout
/// (`GET /api/align/mic`).
#[derive(Debug, Clone, Serialize)]
pub struct MicStatus {
    /// A capture socket is open right now.
    pub connected: bool,
    /// Rate the current (or last) capture declared; 0 before the first hello.
    pub sample_rate: u32,
    /// Mono frames accepted since the current capture connected.
    pub frames_received: u64,
    /// Binary blocks accepted since the current capture connected.
    pub blocks_received: u64,
    /// Sequence discontinuities seen since the current capture connected. Any
    /// window spanning one is unusable (plan §3).
    pub gap_count: u64,
    /// Decaying peak level, 0.0–1.0, for the meter.
    pub peak: f32,
    /// Whether *any* sample has hit full scale since the current capture
    /// connected. Sticky on purpose: plan §7 refuses to measure on a clipped
    /// capture, so the user has to see it happened even if it has stopped.
    pub clipped: bool,
    /// Samples at/beyond full scale, counted.
    pub clip_count: u64,
    /// Frames currently retrievable as a window.
    pub buffered_frames: usize,
    /// Ring capacity in frames (0 before the first hello).
    pub capacity_frames: usize,
}

/// A contiguous mono window plus everything a measurement needs to judge it.
///
/// `first_frame` is an index into the *received* frame stream, not a wall clock
/// and not a count of frames the phone captured: frames lost to a gap are never
/// counted, because `gap` already invalidates any window that spans one, which
/// makes the missing count irrelevant (and unknowable — a gap tells us how many
/// *blocks* were lost, not how many frames were in them).
#[derive(Debug, Clone)]
#[allow(dead_code)] // the fields are the W3 hand-off contract; nothing in-tree reads them yet (plan §14)
pub struct MicWindow {
    /// Mono samples, oldest first, in −1.0..1.0.
    pub samples: Vec<f32>,
    /// Frame index of `samples[0]` since the capture connected.
    pub first_frame: u64,
    /// Rate the capture declared.
    pub sample_rate: u32,
    /// A sequence gap landed inside this window — do not measure it (plan §5.5).
    pub gap: bool,
    /// A sample inside this window hit full scale — do not measure it (plan §7).
    pub clipped: bool,
}

/// Fixed-capacity ring of recent mono audio, with absolute frame accounting.
struct Ring {
    buf: Vec<f32>,
    /// Where the next sample goes.
    write: usize,
    /// How much of `buf` holds real audio (`<= buf.len()`).
    filled: usize,
    /// Frames ever written — the index one past the newest sample.
    total: u64,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self { buf: vec![0.0; capacity], write: 0, filled: 0, total: 0 }
    }

    fn push(&mut self, samples: &[f32]) {
        let cap = self.buf.len();
        if cap == 0 {
            return;
        }
        // A block longer than the ring can only leave its tail behind, and
        // trimming here keeps the copy below a single pass.
        let src = if samples.len() > cap { &samples[samples.len() - cap..] } else { samples };
        let first = (cap - self.write).min(src.len());
        self.buf[self.write..self.write + first].copy_from_slice(&src[..first]);
        if first < src.len() {
            self.buf[..src.len() - first].copy_from_slice(&src[first..]);
        }
        self.write = (self.write + src.len()) % cap;
        self.filled = (self.filled + src.len()).min(cap);
        self.total += samples.len() as u64;
    }

    /// Frame index of the oldest sample still retrievable.
    fn oldest(&self) -> u64 {
        self.total - self.filled as u64
    }

    /// `len` samples starting at absolute frame `first`, or `None` if that range
    /// has already been overwritten or has not been captured yet.
    fn read(&self, first: u64, len: usize) -> Option<Vec<f32>> {
        if len == 0 || first < self.oldest() || first + len as u64 > self.total {
            return None;
        }
        let back = (self.total - first) as usize; // samples from `first` to the head
        let start = (self.write + self.buf.len() - back) % self.buf.len();
        let mut out = Vec::with_capacity(len);
        let first_run = (self.buf.len() - start).min(len);
        out.extend_from_slice(&self.buf[start..start + first_run]);
        if first_run < len {
            out.extend_from_slice(&self.buf[..len - first_run]);
        }
        Some(out)
    }
}

/// Everything one capture session owns. Replaced wholesale on connect, so a
/// reconnect never inherits the previous capture's counters or audio.
struct Capture {
    sample_rate: u32,
    ring: Ring,
    /// Sequence number the next block must carry; `None` until the first block.
    expect_seq: Option<u32>,
    blocks: u64,
    gaps: u64,
    /// Frame indices at which a discontinuity landed (the first frame *after*
    /// each gap), so a window can be told whether it spans one.
    gap_frames: Vec<u64>,
    /// Frame indices of clipped samples, same purpose. Both lists are pruned to
    /// what the ring can still return, so neither grows without bound.
    clip_frames: Vec<u64>,
    clip_count: u64,
    clipped: bool,
    peak: f32,
}

impl Capture {
    fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            ring: Ring::new(sample_rate as usize * RING_SECS),
            expect_seq: None,
            blocks: 0,
            gaps: 0,
            gap_frames: Vec::new(),
            clip_frames: Vec::new(),
            clip_count: 0,
            clipped: false,
            peak: 0.0,
        }
    }

    /// Ingest one binary frame. `Err` is a protocol violation and closes the
    /// socket; a sequence gap is *not* an error (the stream stays usable, only
    /// windows spanning the gap do not).
    fn on_block(&mut self, frame: &[u8]) -> Result<(), String> {
        if frame.len() < 4 {
            return Err(format!("binary frame shorter than the 4-byte sequence number ({} bytes)", frame.len()));
        }
        if frame.len() > MAX_FRAME_BYTES {
            return Err(format!("binary frame too large ({} bytes)", frame.len()));
        }
        let payload = &frame[4..];
        if !payload.len().is_multiple_of(2) {
            return Err(format!("payload is not whole Int16 samples ({} bytes)", payload.len()));
        }
        let seq = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        // Wrapping compare, so a capture long enough to overflow the counter
        // (~2.7 years at 20 ms/block) does not report a spurious gap.
        if let Some(expected) = self.expect_seq {
            if seq != expected {
                self.gaps += 1;
                self.gap_frames.push(self.ring.total);
            }
        }
        self.expect_seq = Some(seq.wrapping_add(1));
        self.blocks += 1;

        let base = self.ring.total;
        let mut samples = Vec::with_capacity(payload.len() / 2);
        let mut block_peak = 0.0f32;
        // Length already checked, so the remainder is empty by construction.
        let (pairs, _) = payload.as_chunks::<2>();
        for (i, pair) in pairs.iter().enumerate() {
            let raw = i16::from_le_bytes(*pair);
            // Full scale is ±i16::MAX/MIN. With AGC off nothing manages the mic's
            // headroom (plan §4.2), so a clipped sample is broadband and corrupts
            // every measurement channel at once (plan §7) — hence counted, not
            // smoothed away.
            if raw == i16::MAX || raw == i16::MIN {
                self.clip_count += 1;
                self.clipped = true;
                self.clip_frames.push(base + i as u64);
            }
            let s = i16_to_f32(raw);
            block_peak = block_peak.max(s.abs());
            samples.push(s);
        }
        self.peak = block_peak.max(self.peak * PEAK_DECAY);
        self.ring.push(&samples);
        self.prune_marks();
        Ok(())
    }

    /// Drop gap/clip marks the ring can no longer return audio for.
    fn prune_marks(&mut self) {
        let oldest = self.ring.oldest();
        self.gap_frames.retain(|f| *f >= oldest);
        self.clip_frames.retain(|f| *f >= oldest);
    }

    fn window(&self, first: u64, len: usize) -> Option<MicWindow> {
        let samples = self.ring.read(first, len)?;
        let end = first + len as u64;
        Some(MicWindow {
            samples,
            first_frame: first,
            sample_rate: self.sample_rate,
            // A gap mark sits on the first frame *after* the discontinuity, so a
            // window that begins exactly there is itself contiguous — only a mark
            // strictly inside the window breaks it. Being stricter would discard
            // the first good window after every drop for no gain.
            gap: self.gap_frames.iter().any(|f| *f > first && *f < end),
            // A clip mark, by contrast, is a sample *in* the window.
            clipped: self.clip_frames.iter().any(|f| *f >= first && *f < end),
        })
    }
}

/// One `i16` sample as `f32` in −1.0..1.0. Divides by `i16::MAX` so full-scale
/// positive is exactly 1.0 (`i16::MIN` therefore lands a hair past −1.0, which
/// the clip counter has already flagged).
fn i16_to_f32(v: i16) -> f32 {
    f32::from(v) / f32::from(i16::MAX)
}

/// The mic ingest: the socket owner and the window provider.
///
/// A process-global singleton reached with [`shared`], in the shape
/// `outputs/overlay_mixer.rs` / `outputs/pwsink/sender_liveness.rs` already use — it is a single
/// hardware-ish resource with one socket, so there is nothing per-request to
/// thread through `AppState`, and W3 can reach it without a new plumbing layer.
pub struct MicIngest {
    /// `None` between captures; `Some` from hello until the socket closes. The
    /// last capture's data is kept after disconnect so the UI's final poll still
    /// shows what happened.
    capture: Mutex<Option<Capture>>,
    /// Whether a socket currently holds the ingest. Separate from `capture` so a
    /// disconnect can leave the audio readable while refusing new connections'
    /// interleaving.
    connected: Mutex<bool>,
}

/// The process-wide ingest handle.
pub fn shared() -> &'static MicIngest {
    static M: OnceLock<MicIngest> = OnceLock::new();
    M.get_or_init(|| MicIngest { capture: Mutex::new(None), connected: Mutex::new(false) })
}

/// Held for the lifetime of an accepted socket; releases the ingest on drop, so
/// every exit path (clean close, error, task cancellation) frees it.
struct IngestGuard;

impl Drop for IngestGuard {
    fn drop(&mut self) {
        *shared().connected.lock_recover() = false;
        tracing::info!("mic ingest disconnected (alignment session untouched)");
    }
}

impl MicIngest {
    /// Claim the ingest for one socket. `None` if another socket already holds
    /// it — two mic streams would interleave into garbage.
    fn acquire(&self) -> Option<IngestGuard> {
        let mut connected = self.connected.lock_recover();
        if *connected {
            return None;
        }
        *connected = true;
        Some(IngestGuard)
    }

    /// Start a capture: allocates the ring and clears every counter, so a
    /// reconnect never mixes two captures' audio or statistics.
    fn begin(&self, hello: MicHello) {
        *self.capture.lock_recover() = Some(Capture::new(hello.sample_rate));
    }

    fn on_block(&self, frame: &[u8]) -> Result<(), String> {
        match self.capture.lock_recover().as_mut() {
            Some(c) => c.on_block(frame),
            None => Err("audio before the hello frame".to_string()),
        }
    }

    pub fn status(&self) -> MicStatus {
        let connected = *self.connected.lock_recover();
        let capture = self.capture.lock_recover();
        let Some(c) = capture.as_ref() else {
            return MicStatus {
                connected,
                sample_rate: 0,
                frames_received: 0,
                blocks_received: 0,
                gap_count: 0,
                peak: 0.0,
                clipped: false,
                clip_count: 0,
                buffered_frames: 0,
                capacity_frames: 0,
            };
        };
        MicStatus {
            connected,
            sample_rate: c.sample_rate,
            frames_received: c.ring.total,
            blocks_received: c.blocks,
            gap_count: c.gaps,
            peak: c.peak,
            clipped: c.clipped,
            clip_count: c.clip_count,
            buffered_frames: c.ring.filled,
            capacity_frames: c.ring.buf.len(),
        }
    }

    /// The most recent `frames` samples, or `None` if that much has not been
    /// captured (yet). The W3 entry point for "measure what just played".
    #[allow(dead_code)] // consumed by the orchestration in W3 (plan §14)
    pub fn window(&self, frames: usize) -> Option<MicWindow> {
        let capture = self.capture.lock_recover();
        let c = capture.as_ref()?;
        let first = c.ring.total.checked_sub(frames as u64)?;
        c.window(first, frames)
    }

    /// `frames` samples starting at an exact frame index — how a caller re-reads
    /// the window it already knows the position of (loop-phase tracking, plan
    /// §6.1), or extends one it has been following.
    #[allow(dead_code)] // consumed by the orchestration in W3 (plan §14)
    pub fn window_from(&self, first_frame: u64, frames: usize) -> Option<MicWindow> {
        let capture = self.capture.lock_recover();
        capture.as_ref()?.window(first_frame, frames)
    }
}

/// `GET /api/align/mic/ws` — the binary mic ingest (plan §4.3).
pub async fn mic_ws(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

/// `GET /api/align/mic` — ingest status for the UI meter and pre-flight.
pub async fn mic_status() -> Json<MicStatus> {
    Json(shared().status())
}

async fn handle_socket(mut socket: WebSocket) {
    let ingest = shared();
    let Some(_guard) = ingest.acquire() else {
        // Rejected loudly: a silently-accepted second capture would interleave
        // with the first and produce plausible-looking nonsense.
        refuse(&mut socket, "a microphone capture is already connected").await;
        return;
    };

    // The hello frame first, and only the hello frame — audio before it has no
    // rate to be interpreted at.
    let hello = match socket.recv().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<MicHello>(&text).map_err(|e| format!("bad hello frame: {e}")),
        Some(Ok(_)) => Err("expected a JSON hello frame first".to_string()),
        Some(Err(e)) => {
            tracing::debug!("mic ingest socket failed before hello: {e}");
            return;
        }
        None => return,
    };
    let hello = match hello.and_then(|h| h.validate().map(|()| h)) {
        Ok(h) => h,
        Err(reason) => {
            refuse(&mut socket, &reason).await;
            return;
        }
    };

    ingest.begin(hello);
    let capacity = hello.sample_rate as usize * RING_SECS;
    tracing::info!("mic ingest connected: {} Hz mono, {RING_SECS}s ring ({capacity} frames)", hello.sample_rate);
    let ready = serde_json::json!({ "type": "ready", "sample_rate": hello.sample_rate, "capacity_frames": capacity });
    if socket.send(Message::Text(ready.to_string().into())).await.is_err() {
        return;
    }

    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(Message::Binary(data)) => {
                if let Err(reason) = ingest.on_block(&data) {
                    tracing::warn!("mic ingest protocol error: {reason}");
                    refuse(&mut socket, &reason).await;
                    return;
                }
            }
            // Text after the hello is reserved for future control messages;
            // ignoring it keeps an older daemon usable with a newer client.
            Ok(Message::Text(_)) | Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => break,
            Err(e) => {
                tracing::debug!("mic ingest socket error: {e}");
                break;
            }
        }
    }
    // `_guard` drops here: ingest free, alignment session untouched.
}

/// Tell the client why it is being closed, in a reason string it can show. The
/// close frame carries it too, since a browser only sees `CloseEvent.reason` when
/// the handshake never finished.
async fn refuse(socket: &mut WebSocket, reason: &str) {
    tracing::warn!("mic ingest refused: {reason}");
    let err = serde_json::json!({ "type": "error", "reason": reason });
    let _ = socket.send(Message::Text(err.to_string().into())).await;
    // 123 bytes is the protocol's limit on a close reason.
    let clipped: String = reason.chars().take(100).collect();
    let _ = socket.send(Message::Close(Some(CloseFrame { code: close_code::POLICY, reason: clipped.into() }))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One binary frame: sequence number + Int16LE payload.
    fn block(seq: u32, samples: &[i16]) -> Vec<u8> {
        let mut v = seq.to_le_bytes().to_vec();
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// A capture with a tiny ring, so wraparound is reachable in a test.
    fn capture(frames: usize) -> Capture {
        let mut c = Capture::new(48_000);
        c.ring = Ring::new(frames);
        c
    }

    #[test]
    fn hello_accepts_both_key_styles_and_both_rates() {
        let a: MicHello = serde_json::from_str(r#"{"sampleRate":48000,"channelCount":1}"#).unwrap();
        assert_eq!((a.sample_rate, a.channel_count), (48_000, 1));
        let b: MicHello = serde_json::from_str(r#"{"sample_rate":44100,"channel_count":1}"#).unwrap();
        assert_eq!((b.sample_rate, b.channel_count), (44_100, 1));
        // channelCount is optional and defaults to mono.
        let c: MicHello = serde_json::from_str(r#"{"sampleRate":44100}"#).unwrap();
        assert_eq!(c.channel_count, 1);
        assert!(a.validate().is_ok() && b.validate().is_ok());
    }

    #[test]
    fn hello_rejects_unsupported_rate_and_multichannel() {
        let stereo = MicHello { sample_rate: 48_000, channel_count: 2 };
        assert!(stereo.validate().unwrap_err().contains("mono"));
        let odd_rate = MicHello { sample_rate: 16_000, channel_count: 1 };
        assert!(odd_rate.validate().unwrap_err().contains("sample rate"));
    }

    #[test]
    fn i16_conversion_hits_the_rails_exactly() {
        assert_eq!(i16_to_f32(0), 0.0);
        assert_eq!(i16_to_f32(i16::MAX), 1.0);
        assert!((i16_to_f32(i16::MIN) + 1.0).abs() < 1e-4);
        assert!((i16_to_f32(16_384) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn consecutive_sequence_numbers_are_gapless() {
        let mut c = capture(4_800);
        for seq in 10..20 {
            c.on_block(&block(seq, &[100, -100])).unwrap();
        }
        assert_eq!(c.gaps, 0);
        assert_eq!(c.blocks, 10);
        assert_eq!(c.ring.total, 20);
        let w = c.window(0, 20).unwrap();
        assert!(!w.gap && !w.clipped);
        assert_eq!(w.first_frame, 0);
        assert_eq!(w.sample_rate, 48_000);
    }

    #[test]
    fn a_skipped_sequence_number_invalidates_only_the_windows_spanning_it() {
        let mut c = capture(4_800);
        c.on_block(&block(0, &[1, 2, 3, 4])).unwrap();
        c.on_block(&block(1, &[5, 6, 7, 8])).unwrap();
        c.on_block(&block(5, &[9, 10, 11, 12])).unwrap(); // 2..4 lost
        assert_eq!(c.gaps, 1);
        // The gap is marked at the first frame after it, i.e. frame 8: windows
        // before it and windows starting at it are contiguous, one straddling it
        // is not.
        assert!(!c.window(0, 8).unwrap().gap);
        assert!(c.window(4, 8).unwrap().gap);
        assert!(!c.window(8, 4).unwrap().gap);
        // A repeated sequence number is a gap too — it is simply not consecutive.
        c.on_block(&block(5, &[13, 14])).unwrap();
        assert_eq!(c.gaps, 2);
    }

    #[test]
    fn sequence_wraparound_is_not_a_gap() {
        let mut c = capture(4_800);
        c.on_block(&block(u32::MAX, &[1, 2])).unwrap();
        c.on_block(&block(0, &[3, 4])).unwrap();
        assert_eq!(c.gaps, 0);
    }

    #[test]
    fn clipping_is_counted_stuck_and_locatable() {
        let mut c = capture(4_800);
        c.on_block(&block(0, &[0, 10])).unwrap();
        c.on_block(&block(1, &[i16::MAX, i16::MIN])).unwrap();
        c.on_block(&block(2, &[0, 0])).unwrap();
        assert_eq!(c.clip_count, 2);
        assert!(c.clipped, "the flag is sticky: plan §7 refuses to measure a capture that clipped");
        assert!(c.window(2, 2).unwrap().clipped);
        assert!(!c.window(0, 2).unwrap().clipped);
        assert!(!c.window(4, 2).unwrap().clipped);
    }

    #[test]
    fn peak_tracks_the_loudest_recent_sample_and_decays() {
        let mut c = capture(4_800);
        c.on_block(&block(0, &[16_384, -16_384])).unwrap();
        assert!((c.peak - 0.5).abs() < 1e-3);
        c.on_block(&block(1, &[0, 0])).unwrap();
        assert!((c.peak - 0.5 * PEAK_DECAY).abs() < 1e-3, "peak = {}", c.peak);
    }

    #[test]
    fn malformed_frames_are_protocol_errors() {
        let mut c = capture(4_800);
        assert!(c.on_block(&[1, 2, 3]).unwrap_err().contains("shorter than"));
        assert!(c.on_block(&[0, 0, 0, 0, 7]).unwrap_err().contains("Int16"));
        assert!(c.on_block(&vec![0u8; MAX_FRAME_BYTES + 2]).unwrap_err().contains("too large"));
        // An empty payload is legal (a keep-alive block) and moves nothing.
        c.on_block(&block(0, &[])).unwrap();
        assert_eq!(c.ring.total, 0);
    }

    #[test]
    fn ring_wraps_and_drops_only_the_oldest() {
        let mut r = Ring::new(8);
        r.push(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(r.read(0, 5).unwrap(), vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        // Wraps: frames 0..2 fall off, 2..10 survive.
        r.push(&[6.0, 7.0, 8.0, 9.0, 10.0]);
        assert_eq!(r.total, 10);
        assert_eq!(r.oldest(), 2);
        assert_eq!(r.read(2, 8).unwrap(), vec![3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
        // A window straddling the physical wrap point reads contiguously.
        assert_eq!(r.read(6, 4).unwrap(), vec![7.0, 8.0, 9.0, 10.0]);
        assert!(r.read(1, 4).is_none(), "overwritten audio must be refused, not silently shifted");
        assert!(r.read(8, 4).is_none(), "the future must be refused too");
        assert!(r.read(0, 0).is_none());
    }

    #[test]
    fn a_block_longer_than_the_ring_keeps_its_tail_and_the_frame_count() {
        let mut r = Ring::new(4);
        r.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(r.total, 6, "frame accounting counts everything received");
        assert_eq!(r.oldest(), 2);
        assert_eq!(r.read(2, 4).unwrap(), vec![3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn marks_are_pruned_with_the_audio_they_refer_to() {
        let mut c = capture(4);
        c.on_block(&block(0, &[i16::MAX, 0])).unwrap();
        c.on_block(&block(9, &[0, 0])).unwrap(); // gap at frame 2
        assert_eq!((c.clip_frames.len(), c.gap_frames.len()), (1, 1));
        c.on_block(&block(10, &[0, 0])).unwrap();
        c.on_block(&block(11, &[0, 0])).unwrap();
        assert!(c.clip_frames.is_empty() && c.gap_frames.is_empty(), "marks must not outlive the ring");
    }

    #[test]
    fn window_needs_the_frames_to_exist() {
        let mut c = capture(4_800);
        c.on_block(&block(0, &[1, 2, 3, 4])).unwrap();
        assert!(c.window(0, 5).is_none());
        assert!(c.window(0, 4).is_some());
    }

    #[test]
    fn only_one_socket_may_hold_the_ingest() {
        // The global handle, so this also proves `shared()` hands out the same one.
        let first = shared().acquire().expect("a free ingest must be claimable");
        assert!(shared().acquire().is_none(), "a second capture would interleave into garbage");
        assert!(shared().status().connected);
        drop(first);
        assert!(!shared().status().connected, "closing marks the ingest disconnected");
        let second = shared().acquire();
        assert!(second.is_some(), "the next capture may connect");
    }
}
