// ABOUTME: Wire encoders for the sendspin push path — PCM passthrough, Opus
// ABOUTME: (vendored libopus via opusic-sys) and FLAC (pure-Rust flacenc) — plus the
// ABOUTME: re-blocker that turns capture quanta into legal codec blocks.
//
// The spec requires a server to support all three codecs
// (`Sendspin/spec` `roles/player/v1.md`: "Servers must support all audio codecs:
// 'opus', 'flac', and 'pcm'"), and compressing the wire is what makes several
// speakers on one WiFi affordable: PCM 48 kHz/16-bit/stereo is ~1.5 Mbit/s per
// stream, Opus at 160 kbps is ~10× less.
//
// ## Where this runs, and what that forbids
//
// Encoding happens **on the `sendspin-relay` SCHED_FIFO thread**, inline in the
// capture→wire fan-out (sendspin_server.rs). That's the cheapest place — the PCM is
// already there, no extra hop or thread — but it means the steady path must not
// allocate, block, or take an unbounded amount of time. So every buffer here is
// allocated once and reused:
//
// - [`Reblocker`] owns one accumulator and consumes it with `copy_within`;
// - [`OpusEncoder`] keeps its i16 input and packet output buffers;
// - [`FlacEncoder`] keeps its `FrameBuf` and `ByteSink` and fills the former
//   straight from our S16LE bytes (`fill_le_bytes` — no intermediate i32 buffer).
//
// One allocation is *not* ours to remove: `flacenc::encode_fixed_size_frame`
// returns an owned `Frame`, so FLAC allocates inside the crate per block. It's
// bounded (one small tree per ~21 ms) and the alternative is forking flacenc's
// internals; Opus and PCM are allocation-free once warm.
//
// ## Per-device encoder state
//
// Opus and FLAC are predictive: an encoder's output depends on what it encoded
// before. Devices in a group normally receive byte-identical PCM, but a device
// being announced to gets `duck(music)+overlay` instead — so its audio *diverges*,
// and sharing one encoder across the group would put a discontinuity in everyone's
// stream at overlay start/end. Hence one encoder per member (sendspin_server.rs
// keys them by `client_id`), created lazily and dropped with the membership.
//
// ## Blocking
//
// The spec bounds a chunk to ≤150 ms and ≥15 ms; Opus additionally only accepts
// 2.5/5/10/20/40/60 ms frames. The capture quantum (~21.3 ms at 1024 frames) is
// legal for PCM and FLAC but not for Opus, so [`Reblocker`] re-cuts the stream to
// exactly [`OPUS_FRAME_FRAMES`] (20 ms — what sendspin-cpp's decoder sizes its
// buffer for) and hands the remainder to the next block. Because a group streams
// ONE format, the block size is a property of the group, so the timeline is stamped
// once per emitted block and every member stays sample-coincident.

use crate::sendspin_capture::{CHANNELS, SAMPLE_RATE};

/// Bytes per interleaved frame at the capture format (S16LE stereo).
const BYTES_PER_FRAME: usize = 2 * CHANNELS as usize;

/// Opus frame size in frames-per-channel: 20 ms at 48 kHz. One of Opus's legal
/// frame sizes, inside the spec's 15..=150 ms chunk bounds, and the size
/// sendspin-cpp's decoder pre-sizes its buffer for ("Opus packets are almost always
/// a single 20ms frame").
pub const OPUS_FRAME_FRAMES: usize = SAMPLE_RATE as usize / 50;

/// FLAC block size in frames-per-channel: 1024 = 21.3 ms at 48 kHz, a conventional
/// FLAC block size that also sits inside the spec's chunk bounds.
pub const FLAC_BLOCK_FRAMES: usize = 1024;

/// Target Opus bitrate for 48 kHz stereo music. High enough to be transparent for
/// casual listening, ~10× smaller than PCM.
const OPUS_BITRATE: i32 = 160_000;

/// Opus encoder complexity (0-10). 5 keeps the per-block CPU predictable on a
/// 4-core Pi — this runs on the RT relay thread, where a long tail matters more
/// than a few kbps of efficiency.
const OPUS_COMPLEXITY: i32 = 5;

/// Upper bound for one encoded Opus packet. libopus recommends ≤4000 bytes for a
/// single frame; the buffer is allocated once at this size and reused.
const OPUS_MAX_PACKET: usize = 4000;

/// Re-cuts the capture stream into fixed-size blocks for the wire codec.
///
/// PCM passes chunks through unchanged (`block_frames == None`); a compressed codec
/// gets exactly its block size, with the remainder carried to the next call. The
/// accumulator is allocated once: [`Self::consume`] shifts the tail down with
/// `copy_within` instead of reallocating.
pub struct Reblocker {
    buf: Vec<u8>,
    block_bytes: Option<usize>,
}

impl Reblocker {
    /// `block_frames = None` ⇒ passthrough (PCM).
    pub fn new(block_frames: Option<usize>) -> Self {
        let block_bytes = block_frames.map(|f| f * BYTES_PER_FRAME);
        // Two blocks of headroom absorbs a capture quantum that straddles a block
        // boundary, so the steady path never grows the Vec.
        let cap = block_bytes.map(|b| b * 2).unwrap_or(8 * 1024);
        Self { buf: Vec::with_capacity(cap), block_bytes }
    }

    /// Append a captured chunk.
    pub fn push(&mut self, pcm: &[u8]) {
        self.buf.extend_from_slice(pcm);
    }

    /// Byte length of the next complete block, or `None` when more audio is needed.
    /// Passthrough mode reports whatever is buffered.
    pub fn ready(&self) -> Option<usize> {
        match self.block_bytes {
            Some(b) if self.buf.len() >= b => Some(b),
            Some(_) => None,
            None if self.buf.is_empty() => None,
            None => Some(self.buf.len()),
        }
    }

    /// The block reported by [`Self::ready`].
    pub fn block(&self, len: usize) -> &[u8] {
        &self.buf[..len]
    }

    /// Drop the first `len` bytes, keeping the tail for the next block.
    pub fn consume(&mut self, len: usize) {
        let rest = self.buf.len() - len;
        self.buf.copy_within(len.., 0);
        self.buf.truncate(rest);
    }
}

/// One member's wire encoder.
pub enum Encoder {
    /// Uncompressed: the wire payload *is* the PCM, so there's nothing to do.
    Pcm,
    Opus(OpusEncoder),
    Flac(FlacEncoder),
}

impl Encoder {
    /// Build the encoder for `codec`, or `None` if it isn't one we can produce
    /// (the caller then falls back to PCM rather than sending undecodable audio).
    pub fn new(codec: &str) -> Option<Self> {
        match codec {
            "pcm" => Some(Self::Pcm),
            "opus" => OpusEncoder::new().map(Self::Opus),
            "flac" => FlacEncoder::new().map(Self::Flac),
            _ => None,
        }
    }

    /// The block size (frames per channel) `codec` needs, or `None` for
    /// passthrough. Drives [`Reblocker::new`].
    pub fn block_frames(codec: &str) -> Option<usize> {
        match codec {
            "opus" => Some(OPUS_FRAME_FRAMES),
            "flac" => Some(FLAC_BLOCK_FRAMES),
            _ => None,
        }
    }

    /// Encode one block of S16LE interleaved PCM, returning the wire payload.
    ///
    /// For [`Self::Pcm`] that's `pcm` itself (no copy). Returns `None` if the
    /// encoder failed — the caller drops the chunk rather than sending garbage,
    /// exactly as it would drop a chunk for a dead member.
    pub fn encode<'a>(&'a mut self, pcm: &'a [u8]) -> Option<&'a [u8]> {
        match self {
            Self::Pcm => Some(pcm),
            Self::Opus(e) => e.encode(pcm),
            Self::Flac(e) => e.encode(pcm),
        }
    }
}

/// libopus encoder (vendored through `opusic-sys`, so there's no system libopus to
/// install in the add-on image).
pub struct OpusEncoder {
    st: *mut opusic_sys::OpusEncoder,
    /// Algorithmic delay (`OPUS_GET_LOOKAHEAD`) in µs — see [`codec_delay_us`].
    lookahead_us: i64,
    /// Reused S16 input scratch (the wire carries interleaved i16; libopus wants a
    /// `*const i16`, and we must not assume the capture bytes are aligned).
    pcm: Vec<i16>,
    /// Reused packet buffer.
    out: Vec<u8>,
}

// The encoder is created and used on one thread (the relay); the raw pointer is
// only ever dereferenced there. `Send` lets it live in a map the relay closure owns.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    fn new() -> Option<Self> {
        let mut err = 0i32;
        // SAFETY: valid rate/channel/application arguments; `err` is written by the
        // callee. A null return with `err != OPUS_OK` is handled below.
        let st =
            unsafe { opusic_sys::opus_encoder_create(SAMPLE_RATE as i32, CHANNELS as i32, opusic_sys::OPUS_APPLICATION_AUDIO, &mut err) };
        if st.is_null() || err != opusic_sys::OPUS_OK {
            tracing::warn!("opus: encoder_create failed (err {err})");
            return None;
        }
        // SAFETY: `st` is a live encoder; both CTLs take one i32 by value.
        unsafe {
            opusic_sys::opus_encoder_ctl(st, opusic_sys::OPUS_SET_BITRATE_REQUEST, OPUS_BITRATE);
            opusic_sys::opus_encoder_ctl(st, opusic_sys::OPUS_SET_COMPLEXITY_REQUEST, OPUS_COMPLEXITY);
        }
        // Algorithmic delay: the decoder's output lags the encoder's input by this
        // much, and sendspin has no pre-skip field to declare it — so the *sender*
        // has to account for it (see `codec_delay_us`).
        let mut lookahead: i32 = 0;
        // SAFETY: `st` is live; the getter CTL writes through the &mut i32.
        unsafe { opusic_sys::opus_encoder_ctl(st, opusic_sys::OPUS_GET_LOOKAHEAD_REQUEST, &mut lookahead) };
        let lookahead_us = i64::from(lookahead) * 1_000_000 / i64::from(SAMPLE_RATE);
        tracing::info!(
            "opus: encoder ready ({} Hz, {} ch, {} bps, complexity {OPUS_COMPLEXITY}, lookahead {lookahead} samples = {lookahead_us} µs)",
            SAMPLE_RATE,
            CHANNELS,
            OPUS_BITRATE
        );
        Some(Self { st, lookahead_us, pcm: Vec::with_capacity(OPUS_FRAME_FRAMES * CHANNELS as usize), out: vec![0u8; OPUS_MAX_PACKET] })
    }

    fn encode(&mut self, block: &[u8]) -> Option<&[u8]> {
        let frames = block.len() / BYTES_PER_FRAME;
        if frames != OPUS_FRAME_FRAMES {
            // The re-blocker guarantees this; a mismatch would be a wiring bug, and
            // libopus would reject the frame size anyway.
            tracing::warn!("opus: block of {frames} frames, expected {OPUS_FRAME_FRAMES}");
            return None;
        }
        self.pcm.clear();
        self.pcm.extend(block.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        // SAFETY: `st` is live; `pcm` holds exactly frames×channels samples and
        // `out` has `OPUS_MAX_PACKET` bytes of capacity, both of which we pass as
        // the matching lengths.
        let n = unsafe {
            opusic_sys::opus_encode(self.st, self.pcm.as_ptr(), OPUS_FRAME_FRAMES as i32, self.out.as_mut_ptr(), self.out.len() as i32)
        };
        if n < 0 {
            tracing::warn!("opus: encode failed (error {n})");
            return None;
        }
        Some(&self.out[..n as usize])
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: `st` came from `opus_encoder_create` and is dropped exactly once.
        unsafe { opusic_sys::opus_encoder_destroy(self.st) };
    }
}

/// Pure-Rust FLAC encoder (`flacenc`), one fixed-size frame per block.
pub struct FlacEncoder {
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    stream_info: flacenc::component::StreamInfo,
    /// Reused per-channel sample buffer, filled straight from our S16LE bytes.
    framebuf: flacenc::source::FrameBuf,
    /// Reused output sink (`clear`ed per block, keeps its capacity).
    sink: flacenc::bitsink::ByteSink,
    /// FLAC frame counter — a live stream just keeps counting.
    frame_number: usize,
}

impl FlacEncoder {
    fn new() -> Option<Self> {
        use flacenc::error::Verify;
        let config = match flacenc::config::Encoder::default().into_verified() {
            Ok(c) => c,
            Err((_, e)) => {
                tracing::warn!("flac: encoder config rejected: {e:?}");
                return None;
            }
        };
        let stream_info = match flacenc::component::StreamInfo::new(SAMPLE_RATE as usize, CHANNELS as usize, 16) {
            Ok(si) => si,
            Err(e) => {
                tracing::warn!("flac: stream info rejected: {e:?}");
                return None;
            }
        };
        let framebuf = match flacenc::source::FrameBuf::with_size(CHANNELS as usize, FLAC_BLOCK_FRAMES) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!("flac: frame buffer rejected: {e:?}");
                return None;
            }
        };
        tracing::info!("flac: encoder ready ({} Hz, {} ch, 16-bit, {FLAC_BLOCK_FRAMES}-frame blocks)", SAMPLE_RATE, CHANNELS);
        Some(Self { config, stream_info, framebuf, sink: flacenc::bitsink::ByteSink::new(), frame_number: 0 })
    }

    fn encode(&mut self, block: &[u8]) -> Option<&[u8]> {
        use flacenc::component::BitRepr;
        use flacenc::source::Fill;

        // `Fill` takes our capture bytes directly — no intermediate i32 buffer.
        if self.framebuf.fill_le_bytes(block, 2).is_err() {
            tracing::warn!("flac: could not fill frame buffer from a {}-byte block", block.len());
            return None;
        }
        // NOTE: this returns an owned `Frame` — the one allocation on this path we
        // don't control (see the module docs).
        let frame = match flacenc::encode_fixed_size_frame(&self.config, &self.framebuf, self.frame_number, &self.stream_info) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("flac: encode failed: {e:?}");
                return None;
            }
        };
        self.sink.clear();
        if let Err(e) = frame.write(&mut self.sink) {
            tracing::warn!("flac: writing the frame failed: {e:?}");
            return None;
        }
        self.frame_number = self.frame_number.wrapping_add(1) & 0x7fff_ffff; // spec: 31-bit
        Some(self.sink.as_slice())
    }
}

/// The smallest send-ahead a `codec` is safe with on real hardware, in µs — our own
/// floor, for when the device doesn't state one.
///
/// The protocol has players report `min_buffer_ms`, and that always wins when present.
/// But a player is not obliged to send it (the Voice PE / satellite1 firmware reports
/// nothing), and without it a user can configure a lead that a *compressed* stream
/// can't survive: the client has to receive, decode and schedule each chunk before its
/// play time, and decoding is the part that needs headroom on an MCU.
///
/// The numbers are measured on this project's hardware:
/// - **PCM** and **FLAC**: 0 — no floor imposed. FLAC decode is cheap integer work,
///   so a floor would only add latency.
/// - **Opus**: [`DEFAULT_OPUS_FLOOR_MS`], and configurable
///   ([`crate::sync_settings::SyncSettings::opus_floor_ms`]);
///   [`opus_floor_lower_bound_ms`] is how low the value may go.
///
/// A device that states its own `min_buffer_ms` overrides this in both directions.
pub fn min_send_ahead_us(codec: &str, opus_floor_ms: u32) -> i64 {
    match codec {
        "opus" => i64::from(opus_floor_ms) * 1000,
        _ => 0,
    }
}

/// Shipped Opus send-ahead floor: **40 ms**, two [`OPUS_FRAME_FRAMES`] blocks.
///
/// Measured on this project's hardware (Voice PE and satellite1 over 2.4 GHz WiFi):
/// Opus plays cleanly at this lead. It covers one block of encoder output plus the
/// WiFi hop, the MCU's decode and its scheduling.
///
/// Tunable per install ([`crate::sync_settings::SyncSettings::opus_floor_ms`]), since
/// the network half of that budget belongs to the site rather than to the codec: a
/// congested band spends more of it on retransmissions.
pub const DEFAULT_OPUS_FLOOR_MS: u32 = 40;

/// The lowest Opus floor that can mean anything, in ms — **the block size**.
///
/// The encoder emits nothing until it has a whole [`OPUS_FRAME_FRAMES`] block (20 ms at
/// 48 kHz, the size sendspin-cpp's decoder is built around). Audio captured at `C`
/// therefore leaves here no earlier than `C + 20 ms`, so a 20 ms send-ahead has it
/// arriving exactly when it is due to play, leaving no window for the network, the
/// decode or the scheduling. The API clamps here; a workable value is above it — see
/// [`DEFAULT_OPUS_FLOOR_MS`].
pub fn opus_floor_lower_bound_ms(codec: &str) -> u32 {
    (Encoder::block_frames(codec).unwrap_or(0) * 1000 / SAMPLE_RATE as usize) as u32
}

/// How long after its timestamp a `codec`-encoded chunk would actually be *heard*,
/// in µs — the encoder's algorithmic delay.
///
/// Opus is predictive and looks ahead, so a decoded packet lags the PCM that went in
/// by `OPUS_GET_LOOKAHEAD` samples (~6.5 ms at 48 kHz). A file container declares
/// that as Opus **pre-skip** and the decoder drops those samples; sendspin has no
/// such field (PCM/Opus headers are synthesized client-side from `stream/start`), so
/// nothing on the wire tells the player to compensate. Left uncorrected, every Opus
/// chunk plays ~6.5 ms after the instant its timestamp asks for — a *permanent* error
/// far outside the spec's ±1 ms accuracy floor, which the player's correction loop
/// then fights forever. The sender therefore shifts its timestamps back by this
/// amount, which is the sender-side equivalent of pre-skip.
///
/// PCM and FLAC have no such delay (FLAC is block-based and lossless: block N decodes
/// to exactly the samples of block N), so they return 0 — which is also why they were
/// unaffected.
pub fn codec_delay_us(codec: &str) -> i64 {
    match codec {
        // Deterministic for a given rate/channel/application, so a throwaway encoder
        // is the cheapest honest way to ask libopus rather than hardcoding 6.5 ms.
        "opus" => OpusEncoder::new().map(|e| e.lookahead_us).unwrap_or(0),
        _ => 0,
    }
}

/// The `codec_header` a `stream/start` must carry for `codec`, base64 as the spec
/// requires — `None` when the codec needs none.
///
/// Only FLAC does: sendspin-cpp feeds this blob straight into micro-flac and expects
/// it to reach `FLAC_DECODER_HEADER_READY`, i.e. a real FLAC stream header. PCM and
/// Opus carry their parameters in the `stream/start` format fields, which the client
/// turns into a synthetic header itself (`CHUNK_TYPE_{PCM,OPUS}_DUMMY_HEADER`).
pub fn codec_header_base64(codec: &str) -> Option<String> {
    (codec == "flac").then(|| base64_encode(&flac_stream_header()))
}

/// `fLaC` + a last-metadata-block STREAMINFO describing our live stream.
///
/// Hand-assembled rather than taken from flacenc: the header has to go out with
/// `stream/start`, *before* any frame exists, and the values are fully known up
/// front (fixed block size, no total sample count). `min/max frame size = 0` and
/// `total samples = 0` are the spec's "unknown", which is what a live stream is;
/// the MD5 is left zero (also "unset") since there's no complete stream to digest.
fn flac_stream_header() -> Vec<u8> {
    let mut si = [0u8; 34];
    let bs = FLAC_BLOCK_FRAMES as u16;
    si[0..2].copy_from_slice(&bs.to_be_bytes()); // min block size
    si[2..4].copy_from_slice(&bs.to_be_bytes()); // max block size
                                                 // si[4..7] min frame size, si[7..10] max frame size — 0 = unknown.
                                                 // Then a packed field: 20 bits sample rate, 3 bits channels-1, 5 bits depth-1,
                                                 // 36 bits total samples (0 = unknown).
    let rate = SAMPLE_RATE;
    let channels = u32::from(CHANNELS) - 1;
    let depth = 16u32 - 1;
    si[10] = (rate >> 12) as u8;
    si[11] = (rate >> 4) as u8;
    si[12] = (((rate & 0xf) << 4) as u8) | ((channels as u8) << 1) | ((depth >> 4) as u8);
    // Low 4 bits of (depth-1), then the top 4 bits of total samples (0 = unknown).
    si[13] = ((depth & 0xf) as u8) << 4;
    // si[14..18] = the remaining 32 bits of total samples (0); si[18..34] = MD5 (0).

    let mut out = Vec::with_capacity(4 + 4 + si.len());
    out.extend_from_slice(b"fLaC");
    out.push(0x80); // last-metadata-block flag | block type 0 (STREAMINFO)
    out.extend_from_slice(&(si.len() as u32).to_be_bytes()[1..]); // 24-bit length
    out.extend_from_slice(&si);
    out
}

/// Standard base64 (the spec's encoding for `codec_header`). Hand-rolled to avoid
/// pulling a dependency in for one 42-byte blob emitted once per stream start.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(TABLE[(n >> 18 & 0x3f) as usize] as char);
        out.push(TABLE[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6 & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm_frames(n: usize) -> Vec<u8> {
        // A quiet ramp — real-ish input, so the encoders actually do work.
        (0..n * CHANNELS as usize).flat_map(|i| ((i as i16).wrapping_mul(64)).to_le_bytes()).collect()
    }

    #[test]
    fn reblocker_passes_pcm_through_untouched() {
        let mut r = Reblocker::new(None);
        let chunk = pcm_frames(1024);
        r.push(&chunk);
        let len = r.ready().expect("a pushed chunk is immediately ready");
        assert_eq!(r.block(len), &chunk[..], "PCM must not be re-cut");
        r.consume(len);
        assert_eq!(r.ready(), None);
    }

    #[test]
    fn reblocker_cuts_fixed_blocks_and_carries_the_remainder() {
        let mut r = Reblocker::new(Some(OPUS_FRAME_FRAMES));
        // One capture quantum (1024 frames) is more than one 960-frame Opus block…
        r.push(&pcm_frames(1024));
        let len = r.ready().expect("960 frames are available");
        assert_eq!(len, OPUS_FRAME_FRAMES * BYTES_PER_FRAME);
        r.consume(len);
        // …and the 64-frame remainder waits for the next quantum instead of being
        // sent short or dropped.
        assert_eq!(r.ready(), None);
        r.push(&pcm_frames(1024));
        assert_eq!(r.ready(), Some(OPUS_FRAME_FRAMES * BYTES_PER_FRAME));
    }

    #[test]
    fn reblocker_does_not_grow_in_the_steady_state() {
        let mut r = Reblocker::new(Some(OPUS_FRAME_FRAMES));
        let cap = r.buf.capacity();
        for _ in 0..500 {
            r.push(&pcm_frames(1024));
            while let Some(len) = r.ready() {
                let _ = r.block(len);
                r.consume(len);
            }
        }
        assert_eq!(r.buf.capacity(), cap, "the accumulator must not reallocate on the RT path");
    }

    #[test]
    fn opus_encodes_a_block_to_a_much_smaller_packet() {
        let mut e = OpusEncoder::new().expect("libopus is vendored, so this must succeed");
        let block = pcm_frames(OPUS_FRAME_FRAMES);
        let packet = e.encode(&block).expect("a correctly-sized block encodes");
        assert!(!packet.is_empty());
        assert!(
            packet.len() < block.len() / 4,
            "160 kbps opus is ≈10× smaller than 1.5 Mbit/s PCM, got {} vs {}",
            packet.len(),
            block.len()
        );
        // Steady state: no reallocation across many blocks.
        let (pcm_cap, out_cap) = (e.pcm.capacity(), e.out.len());
        for _ in 0..100 {
            e.encode(&block).unwrap();
        }
        assert_eq!((e.pcm.capacity(), e.out.len()), (pcm_cap, out_cap));
    }

    #[test]
    fn opus_rejects_a_wrongly_sized_block() {
        // Guards the re-blocker contract: libopus only accepts its legal frame
        // sizes, so a mis-cut block must be dropped, not sent.
        let mut e = OpusEncoder::new().unwrap();
        assert!(e.encode(&pcm_frames(OPUS_FRAME_FRAMES + 1)).is_none());
    }

    #[test]
    fn flac_encodes_a_block_losslessly_smaller_than_pcm() {
        let mut e = FlacEncoder::new().expect("flacenc is pure Rust, so this must succeed");
        let block = pcm_frames(FLAC_BLOCK_FRAMES);
        let frame = e.encode(&block).expect("a full block encodes");
        // FLAC frames start with the 14-bit sync code 0b11111111111110.
        assert_eq!(frame[0], 0xff, "not a FLAC frame header");
        assert_eq!(frame[1] & 0xfc, 0xf8, "not a FLAC frame header");
        assert!(frame.len() < block.len(), "lossless should still beat raw PCM on a ramp");
        // Frame numbers advance so a decoder sees a continuous stream.
        assert_eq!(e.frame_number, 1);
        e.encode(&block).unwrap();
        assert_eq!(e.frame_number, 2);
    }

    #[test]
    fn only_opus_reports_an_encoder_delay_and_it_matches_libopus() {
        // PCM/FLAC decode block N to exactly block N's samples — no delay, which is
        // why they never showed the sync error Opus did.
        assert_eq!(codec_delay_us("pcm"), 0);
        assert_eq!(codec_delay_us("flac"), 0);
        // libopus at 48 kHz reports 312 samples (6.5 ms) for this configuration; assert
        // the shape rather than the exact number so a libopus update doesn't fail here.
        let opus = codec_delay_us("opus");
        assert!((3_000..=15_000).contains(&opus), "implausible opus lookahead: {opus} µs");
        let e = OpusEncoder::new().unwrap();
        assert_eq!(opus, e.lookahead_us, "the group-level constant must be what the encoder reports");
    }

    #[test]
    fn only_flac_gets_a_codec_header_and_it_is_a_real_flac_stream_header() {
        assert_eq!(codec_header_base64("pcm"), None);
        assert_eq!(codec_header_base64("opus"), None, "the client synthesizes Opus's header from stream/start");
        let raw = flac_stream_header();
        assert_eq!(&raw[..4], b"fLaC");
        assert_eq!(raw[4], 0x80, "STREAMINFO must be flagged as the last metadata block");
        assert_eq!(&raw[5..8], &[0, 0, 34], "STREAMINFO is 34 bytes");
        assert_eq!(raw.len(), 42);
        // Block size, sample rate, channels and depth must round-trip out of the
        // packed bit fields — a decoder that misreads these plays noise.
        assert_eq!(u16::from_be_bytes([raw[8], raw[9]]), FLAC_BLOCK_FRAMES as u16);
        assert_eq!(u16::from_be_bytes([raw[10], raw[11]]), FLAC_BLOCK_FRAMES as u16);
        let si = &raw[8..];
        let rate = (u32::from(si[10]) << 12) | (u32::from(si[11]) << 4) | (u32::from(si[12]) >> 4);
        assert_eq!(rate, SAMPLE_RATE);
        assert_eq!(((si[12] >> 1) & 0x07) + 1, CHANNELS as u8);
        let depth = (((u32::from(si[12]) & 0x01) << 4) | (u32::from(si[13]) >> 4)) + 1;
        assert_eq!(depth, 16);
        // And it must be valid base64 of exactly that blob.
        assert_eq!(codec_header_base64("flac"), Some(base64_encode(&raw)));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fLaC"), "ZkxhQw==");
    }

    #[test]
    fn block_size_choice_stays_inside_the_specs_chunk_bounds() {
        // "A server MUST NOT send an audio chunk longer than 150 ms, and SHOULD NOT
        // send one shorter than 15 ms" (roles/player/v1.md).
        for frames in [OPUS_FRAME_FRAMES, FLAC_BLOCK_FRAMES] {
            let ms = frames as f64 * 1000.0 / f64::from(SAMPLE_RATE);
            assert!((15.0..=150.0).contains(&ms), "{frames} frames = {ms} ms is outside the spec bounds");
        }
    }
}
