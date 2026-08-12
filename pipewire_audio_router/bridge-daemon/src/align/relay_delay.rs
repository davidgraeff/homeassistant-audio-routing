//! Per-device **provisional** delay and **calibration mute** for alignment
//! (docs/mic-alignment-plan.md §1.1.1, §12.3.2).
//!
//! Two per-device effects, both applied at the same relay hook and both alignment-only:
//! a delay line (below) and a mute (see "The calibration mute"). They live together
//! because they share that hook, their per-device state, their lifetime and their
//! restore obligation — a third independent pass over the three relays would cost the
//! same RT budget again for no gain.
//!
//! A real delay write costs a device reconnect — tens of seconds (plan §2.3) — so a
//! multi-position alignment run cannot afford to write the knobs at every step. It
//! applies its delays here instead, in the daemon, and writes the real knobs once at
//! the end.
//!
//! The mechanism is a **delay line**, not per-device tone synthesis: a per-device ring
//! buffer read at an offset of *d* samples emits older content against the relay's
//! unchanged timestamp schedule, so the device renders that content later — which is
//! exactly a delay.
//!
//! Two properties are the reason for this shape rather than the timeline-keyed
//! synthesis rejected in plan §6.3:
//!
//! * **Transport-agnostic.** A delay line needs no presentation timestamp and no
//!   timeline anchor; it only buffers. So it works identically for sendspin, AP2 and
//!   pw-sink, which is precisely what §6.3 could not do.
//! * **Sample-accurate despite block-granular relays.** Reading a ring at an offset is
//!   indifferent to block boundaries, so the delay is not quantised to the wire codec's
//!   frame size (20 ms for Opus).
//!
//! Nothing here is persisted: a daemon restart drops every provisional delay and leaves
//! the user's stored configuration untouched.
//!
//! ## Where it hooks in, and in which order
//!
//! Beside the existing per-device announcement overlay, and **after** it:
//!
//! ```text
//! capture ─▶ outputs::overlay_mixer::mix_into(node, …) ─▶ relay_delay::delay_into(node, …) ─▶ encoder/sender
//! ```
//!
//! (`outputs/sendspin/server.rs`, `outputs/ap2/server.rs`, `outputs/pwsink/server.rs`; for sendspin this sits
//! *before* the per-member encoder, because the ring stores PCM.)
//!
//! The order is forced by what this module is standing in for. The real knob it
//! emulates — sendspin `static_delay_ms`, AP2 render delay, pw-sink playout delay
//! (plan §2.4) — is applied by the *device*, at the very end of the chain, and
//! therefore delays **everything** that device renders, announcements included. Putting
//! the delay line downstream of the overlay mix reproduces that; putting it upstream
//! would delay music but not the announcement, so the provisional state and the final
//! written state would differ in a way the run cannot see.
//!
//! ## The calibration mute (plan §12.3.2, W17)
//!
//! Alignment implements audibility as **mute**: exactly the members it names are
//! audible and every other one is silent, so a per-member arrival can be attributed at
//! all (plan §12.2). sendspin and AP2 have an in-band mute, and a pw-sink host can
//! usually be muted out of band by its agent (`pwsink_agent`) — but neither is
//! universal: an agent can be disconnected, a host's sink can have *no volume lever at
//! all*, and a future output kind may have nothing of its own either. A member that
//! cannot be silenced keeps emitting the calibration click straight through every other
//! member's solo, and that is a measurement hazard rather than a cosmetic one: the mic
//! then hears two arrivals, which either refuse as `AmbiguousPeak` (so **every** member
//! of the step fails, not just the unmutable one) or merge into one pulled peak, i.e.
//! plan §5.6's silent bias by construction.
//!
//! So this module also owns a **per-device calibration mute**, applied at the same hook
//! as the delay line and therefore just as transport-agnostic: a muted output's block is
//! replaced with exact silence of the same length. It is the universal fallback, not the
//! preferred mechanism — where a device or its host can mute itself that is better,
//! because the stream keeps flowing and the receiver's jitter buffer never re-anchors.
//! `align/calibrate/mod.rs` resolves which mechanism each member gets, per output.
//!
//! Two properties make it safe to compose with everything else here:
//!
//! * **The delay line keeps advancing while muted.** The block is written into the ring
//!   and the read pointer moves exactly as it would have; only the *emitted* bytes are
//!   zeroed. So unmuting resumes at the sample continuous playback would have reached —
//!   no jump forward, no replay of stale audio, nothing the estimator could measure as a
//!   real offset. (Muting does not re-prime, either.)
//! * **It sits downstream of the announcement overlay**, like the delay, so a mute
//!   silences duck+overlay too — exactly what a device-side mute does. The overlay's
//!   cursor still advances, so a barge-in that wins against the hold still *finishes*
//!   and is still reported as interference (`align_group::Interference`); it is simply
//!   not heard on a member the run is keeping silent.
//!
//! A mute and a delay are independent state with independent lifetimes on the same
//! output: clearing one never clears the other, and an output with only a mute costs no
//! ring at all.
//!
//! One consequence worth knowing: an overlay's cursor is advanced by `mix_into` at the
//! relay's *consumption* rate, so with a delay of *d* the announce scheduler learns the
//! clip "finished" up to *d* earlier than the speaker actually renders its tail. That is
//! not new — the send-ahead lead (~250 ms for sendspin) already puts the daemon's notion
//! of "sent" ahead of rendering — the delay only extends it by *d*, bounded by
//! [`MAX_DELAY_MS`].
//!
//! ## Priming: the first *d* samples are silence
//!
//! A newly set delay has no history to read, so the output is **silence** until the ring
//! has accumulated *d* samples. Silence is chosen over the alternatives because it is
//! the only one that cannot be mistaken for content: reading the un-primed ring would
//! emit either zeros anyway (a fresh ring) or, after a wrap, audio from *d* seconds of
//! nowhere — a backwards jump that a periodic click loop (plan §3) would alias into a
//! plausible-looking peak at the wrong phase. A gap is honest; a shifted repeat is not.
//!
//! **This makes priming an orchestration precondition, not a detail.** A measurement
//! taken while a line is priming sees a dropout, and the estimator's amplitude/spread
//! checks would report it as something else entirely (compare plan §2.3.2 on
//! misattributed intermittency). Wait for [`RelayDelay::status`]'s
//! `primed`/`prime_remaining_us` — at most [`MAX_DELAY_MS`] — before measuring.
//!
//! A **delay increase** re-primes only the increment (existing history is kept, so only
//! the newly exposed pre-history is silent); a **delay decrease** skips forward over
//! *(old − new)* samples of content, which is inherent to shortening a delay — and
//! clearing a delay is the same thing with *new = 0*, so it costs one skip. Both are
//! sample-exact from the moment they take effect.
//!
//! ## Bound and memory cost
//!
//! [`MAX_DELAY_MS`] = 1000 ms. Larger is rejected rather than clamped, so a caller
//! cannot silently measure against a delay it did not ask for. The bound is generous for
//! the job: the chain's ratcheting floor (plan §1.1) accumulates room-scale path
//! differences (~3 ms/m) plus per-step corrections, and it is renormalised to zero at
//! the end anyway.
//!
//! A line's ring is sized once, for the bound, at 48 kHz stereo S16LE:
//! `48 000 × 4 B = 192 000 B` for the delay plus [`HEADROOM_FRAMES`] (8192 frames,
//! 32 768 B) so any plausible capture quantum fits behind the read pointer —
//! **≈ 220 KiB per delayed output** (≈ 1.7 MiB for eight). Outputs with no delay have no
//! line and cost nothing. Sizing for the bound rather than the current value is what
//! keeps a delay *change* free of reallocation on the RT thread.
//!
//! ## RT-safety
//!
//! These relays run on SCHED_FIFO threads (`set_relay_realtime_priority`) and the
//! surrounding code is allocation-free once warm (pooled capture buffers, a reused
//! `mix_buf`, per-member encoders). This module holds to the same rule:
//!
//! * **No delay and no mute anywhere** — the overwhelmingly common case — costs one
//!   relaxed atomic load per device per block ([`RelayDelay::delay_into`] returns before
//!   touching the mutex or the map). A router that never runs an alignment pays nothing.
//! * **With a delay set**, the hot path is: one mutex lock, one `&str` hash lookup (no
//!   allocation — `HashMap<String, _>` is queried by `&str`), and 2–4 `memcpy`s. No
//!   per-sample loop, no timekeeping, no logging.
//! * **With a mute set**, the same lock and lookup plus one `memset` of the block (and
//!   the delay line's memcpys when it also has a delay, because the ring must keep
//!   advancing). No allocation once the caller's `delay_buf` has reached its steady
//!   capacity, which it does on the first block — `clear()` keeps the capacity and
//!   `resize` into it only writes zeros.
//! * **Allocation happens off this thread**: the ring is allocated by
//!   [`RelayDelay::set_delay_us`] (an API/measurement-thread call), sized for the bound
//!   at 48 kHz stereo, so neither a delay change nor a rate change reallocates. The only
//!   way the RT thread can allocate is a format that needs *more* bytes per second than
//!   48 kHz stereo (nothing today does) or a capture block larger than
//!   [`HEADROOM_FRAMES`]; both grow the ring once and re-prime, and both are logged.
//! * The reused output `Vec` (`delay_buf` at each call site) reaches its steady capacity
//!   on the first block, exactly like `mix_buf`.
//!
//! Because the active-line count is read `Relaxed`, a delay set on one thread can take
//! effect one block (≤ ~21 ms) later on a relay thread. That is invisible next to the
//! priming wait it is followed by.
//!
//! ## Seam: retiring the relay-vs-device equivalence assumption (plan §1.1.1)
//!
//! The plan's scheme rests on "relay-side delay *d* and device-side static delay *d*
//! produce the same audible shift", to be retired by measuring once. This module
//! provides the relay arm — [`RelayDelay::set_delay_us`] plus [`RelayDelay::status`] for
//! the exact applied sample count — and deliberately does not orchestrate it. The
//! orchestration is W21, in `align/measure/mod.rs` (`plan_equivalence`, `EquivalenceReport`);
//! read its section header for what the experiment turned out to measure, which is not
//! what §1.1.1 expected:
//!
//! * §1.1.1 asks for "a per-transport constant to correct for". A constant is exactly
//!   what item 1 below makes **invisible**: a difference of two post-reconnect readings
//!   cancels it. What the experiment can measure is the **scale** (does a knob change of
//!   *N* move the sound by *N*?) and the **sign** — and those are the two that matter,
//!   because a constant is a common shift *within* a transport kind and a common shift
//!   is free (plan §2.4.2), while a scale error leaves every member wrong by
//!   `(g−1)·d_i`, which is not.
//! * Two reconnects are not enough either, for a reason item 1 does not mention: they
//!   are tens of seconds apart (plan §2.3) and the mic-vs-audio clock runs at up to
//!   ~100 ppm (§5.4.1), i.e. ~6 ms of phase creep against a step that §9.2's send-ahead
//!   mark keeps down to 20 ms. So each arm is bracketed — baseline, changed, baseline —
//!   and the device arm costs **three** writes.
//!
//! Two things the orchestration must account for, both established by reading the code
//! rather than by measurement:
//!
//! 1. **The device arm needs two reconnects, not one.** Writing the real knob forces a
//!    reconnect, and a reconnect restarts the stream, the device's clock-sync and its
//!    buffer fill — any of which may shift that device's rendering offset by some ε
//!    independent of the delay. Comparing "relay *d*, no reconnect" against "device *d*,
//!    after a reconnect" measures *d ± (difference) ± ε* and cannot separate the two. The
//!    device arm must be a difference of two *post-reconnect* measurements (write 0,
//!    reconnect, measure; write *N*, reconnect, measure). W21 reports the ε it stumbles
//!    over on the way across (`EquivalenceReport::reconnect_epsilon_ms`), which is the
//!    first number this design has for it rather than an argument.
//! 2. **What is written back is not what was applied.** The knobs are integer
//!    milliseconds and pw-sink's has a hard floor of three packet times
//!    (`routing::sync_settings::PWSINK_JITTER_MIN_MS` = 15 ms), while this line is sample-accurate
//!    (20.8 µs at 48 kHz). So the provisional value is *strictly more expressive* than
//!    the final one: rounding costs up to 0.5 ms per member, and a sub-15 ms pw-sink
//!    delay cannot be written at all. Independent of any equivalence question, the
//!    write-back is where sub-millisecond alignment is lost.
//!
//! 3. **The sendspin knob may have the opposite sign.** This line can only *delay*.
//!    sendspin's `static_delay_ms` is, in the reference client, an output-latency
//!    *compensation*: `synced_player.rs` anchors the cursor `+delay` into the server
//!    timeline and schedules it at `server_to_local(cursor) − delay`
//!    (`clock.rs::server_to_local_instant_with_latency`), i.e. it emits each sample
//!    *earlier* so the amp/speaker latency lands it on time — and this daemon's own
//!    `required_send_ahead_us` agrees, adding each member's `static_delay_ms` to the
//!    group lead because "the device plays that much earlier". If the ESPHome firmware
//!    follows that reading, plan §2.4's "additive only — you can delay a speaker, never
//!    advance it" and §9.1's "reference = the latest arrival" are inverted for sendspin
//!    (while AP2 render delay and pw-sink playout really are delays, so a mixed group
//!    would hold knobs of *both* signs). The write-back is still expressible — with an
//!    advance-only knob, `advance_i = max_j(d_j) − d_i` reproduces the relative geometry
//!    of provisional delays `d_i`, since a common shift changes nothing — but the numbers
//!    are inverted, not offset by a constant. **Have the equivalence experiment record
//!    the sign, not just the magnitude**; it settles this in the one reconnect it was
//!    already going to spend. (§2.4.1 has since settled it from `sendspin-cpp` itself —
//!    the firmware subtracts the static delay, so it *is* an advance — and W21's device
//!    arm is the independent confirmation, which reports a sign disagreement as its own
//!    verdict rather than as a large offset.)
//!
//! And one mechanism by which the two are **not** identical, which is inherent and
//! cannot be calibrated away by a per-transport constant (W21 sidesteps it by stepping
//! exactly one Opus frame, which leaves the window phase untouched — a real alignment
//! delay is an arbitrary number of milliseconds and does not have that luxury):
//!
//! * **A relay-side delay moves content relative to the codec's frame grid; a
//!   device-side delay does not.** The sendspin relay re-cuts the stream into fixed
//!   960-frame Opus blocks (`outputs::sendspin::codec::Reblocker`) and this line sits upstream of
//!   that, so a delay that is not a multiple of 20 ms changes *where inside an MDCT
//!   window* a measurement transient falls. The decoded signal is still delayed by
//!   exactly *d*, but Opus's transient smearing is window-position dependent, so the
//!   estimator's peak position can move by a fraction of a frame that the device knob —
//!   which shifts rendering with the content-to-frame phase untouched — would not
//!   produce. Plan §2.3.1 already records that sub-millisecond peak *position* through
//!   Opus is unverified; this is the specific mechanism, and it is sendspin-with-Opus
//!   only (PCM and L16 are exact, FLAC is lossless so its shifted frame boundaries
//!   change nothing in the decoded samples).
//!
//! What is *not* affected, checked against the code rather than assumed: this line emits
//! exactly as many bytes as it consumes, one output block per input block, at the same
//! instant. So the send-ahead lead and the receivers' jitter buffers see an unchanged
//! schedule — `SharedTimeline::stamp(pcm_len)` (sendspin submodule
//! `server/timeline.rs`) derives the presentation timestamp from the block *length* and
//! the clock alone and is called once per block *before* the per-device fan-out;
//! `applemidi_sender`'s RTP timestamp is a pure frame counter (`ts += frames_per_pkt`)
//! and its backlog is measured in queued samples; the AP2 sender likewise timestamps
//! what it is fed. None of them can observe that the content is older. The one
//! asymmetry, and it belongs to the *device* side, is that a real sendspin
//! `static_delay_ms` is folded into the group's required lead
//! (`outputs::sendspin::server::required_send_ahead_us`) and so can force a group-wide restart
//! when it crosses the high-water mark (plan §9.2) — the provisional delay never does,
//! which is a reason the final write can behave differently from the walk that preceded
//! it, and a reason to check the mark *before* writing.

// W13 delivers this library plus the three relay hooks; the measurement state machine
// and the HTTP surface that drive it (plan §11) land separately, so the setters and
// inspectors have no in-tree caller yet. Same convention outputs/pwsink/server.rs used while its
// reconciler wiring was pending.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// Largest provisional delay this module accepts, in milliseconds. See the module
/// docs: rejected rather than clamped, and the reason the ring can be sized once.
pub const MAX_DELAY_MS: u32 = 1_000;

/// [`MAX_DELAY_MS`] in microseconds.
pub const MAX_DELAY_US: u64 = MAX_DELAY_MS as u64 * 1_000;

/// Ring headroom in frames, on top of the delay itself: the read pointer sits
/// `delay` behind the write pointer, so the ring must also hold one whole input block
/// (8192 frames = 170 ms at 48 kHz — far above any capture quantum; the sendspin spec
/// caps a chunk at 150 ms, and PipeWire quanta are 1–2 k frames).
pub const HEADROOM_FRAMES: u64 = 8_192;

/// The rate a ring is sized for, so a delay change never reallocates on the RT thread.
/// The highest rate the relays run (a 44.1 kHz AP2 group needs *fewer* frames for the
/// same delay, so its ring is comfortably large).
const SIZING_RATE: u32 = 48_000;

/// The channel count a ring is sized for (every relay captures stereo:
/// `pw::capture::CHANNELS`).
const SIZING_CHANNELS: u16 = 2;

/// The interleaved PCM format a relay is feeding this line, so the delay can be
/// converted from time to samples without this module having to track per-output rates.
/// Sample format is always S16LE (the capture format shared by all three relays).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u16,
}

impl PcmFormat {
    pub const fn new(rate: u32, channels: u16) -> Self {
        Self { rate, channels }
    }

    /// Bytes per interleaved frame (S16LE).
    pub const fn frame_bytes(&self) -> usize {
        2 * self.channels as usize
    }
}

/// Delay in whole frames for `delay_us` at `rate`, rounded to the **nearest** frame.
///
/// Rounding, not truncation: the estimator's output is a fractional millisecond, and
/// truncating would bias every delay in the same direction — a systematic error is worse
/// than a ±0.5-sample one, and it accumulates along the chain (plan §1.1).
pub const fn frames_for(delay_us: u64, rate: u32) -> u64 {
    (delay_us * rate as u64 + 500_000) / 1_000_000
}

/// Inverse of [`frames_for`]: the microsecond value that lands on `frames` at `rate`.
/// Exposed so a caller that thinks in samples (tests, the equivalence experiment) can
/// express an exact sample count.
pub const fn us_for_frames(frames: u64, rate: u32) -> u64 {
    (frames * 1_000_000 + (rate as u64) / 2) / rate as u64
}

/// What one output's delay line is currently doing — for the API/measurement layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelayStatus {
    /// The requested delay, as set.
    pub delay_us: u64,
    /// The delay actually applied, in frames at [`Self::rate`] — the exact quantity to
    /// compare against a device knob in the equivalence experiment.
    pub delay_frames: u64,
    /// Sample rate of the relay feeding this line (the rate `delay_frames` is at).
    /// Before the first block arrives this is the assumed 48 kHz the ring was sized for;
    /// the first call from a relay replaces it with the real one.
    pub rate: u32,
    /// Channels of the relay feeding this line (assumed stereo until the first block).
    pub channels: u16,
    /// Whether the ring holds a full delay's worth of history, i.e. whether the output
    /// is now entirely real content. Measure only when this is true.
    pub primed: bool,
    /// How much more audio must flow before `primed` becomes true.
    pub prime_remaining_us: u64,
    /// Ring size in bytes (the per-output memory cost). `0` for an output that only
    /// carries a calibration mute — no delay, so no ring.
    pub ring_bytes: usize,
    /// Whether this output is **calibration-muted** at the relay hook: its blocks leave
    /// as exact silence while the delay line (if any) keeps advancing. Independent of
    /// the delay in both directions.
    pub muted: bool,
}

/// One output's delay line: a byte ring holding the recent past of that output's PCM,
/// read `delay` bytes behind the write pointer.
struct DelayLine {
    delay_us: u64,
    /// Format the ring's contents are in. A change (an AP2 group renegotiating
    /// 44.1 ↔ 48 kHz) invalidates the history, so it re-primes.
    fmt: PcmFormat,
    ring: Vec<u8>,
    /// Byte offset the next write starts at.
    write: usize,
    /// Bytes of real history behind `write`, saturating at `ring.len()`. Reset by a
    /// format change or a regrow; *not* reset by a delay change, so an increase
    /// re-primes only the increment.
    valid: usize,
    /// One-shot log guards, so a pathological block size cannot spam the RT thread.
    warned_grow: bool,
    warned_misaligned: bool,
}

impl DelayLine {
    fn new(delay_us: u64) -> Self {
        let fmt = PcmFormat::new(SIZING_RATE, SIZING_CHANNELS);
        // Sized for the BOUND, not for `delay_us`: a later change (up to MAX) then costs
        // no allocation, which is what keeps the RT path clean. Allocated here, on the
        // caller's (non-RT) thread.
        let bytes = Self::ring_bytes_for(MAX_DELAY_US, fmt);
        Self { delay_us, fmt, ring: vec![0u8; bytes], write: 0, valid: 0, warned_grow: false, warned_misaligned: false }
    }

    fn ring_bytes_for(delay_us: u64, fmt: PcmFormat) -> usize {
        (frames_for(delay_us, fmt.rate) + HEADROOM_FRAMES) as usize * fmt.frame_bytes()
    }

    fn delay_bytes(&self, fmt: PcmFormat) -> usize {
        frames_for(self.delay_us, fmt.rate) as usize * fmt.frame_bytes()
    }

    /// Drop the history and adopt `fmt` (and, if `min_bytes` exceeds the ring, a bigger
    /// ring). The only allocating path reachable from the relay thread; see the module
    /// docs on when that can happen.
    fn reset_for(&mut self, output: &str, fmt: PcmFormat, min_bytes: usize) {
        if self.ring.len() < min_bytes {
            if !self.warned_grow {
                self.warned_grow = true;
                tracing::warn!(
                    "relay_delay '{output}': growing the delay ring from {} to {min_bytes} B — a capture block \
                     exceeded the {HEADROOM_FRAMES}-frame headroom or the format needs more than {SIZING_RATE} Hz \
                     stereo. Allocated on the relay thread (once); audio re-primes.",
                    self.ring.len()
                );
            }
            self.ring = vec![0u8; min_bytes];
        }
        self.fmt = fmt;
        self.write = 0;
        self.valid = 0;
    }

    /// Emit `src` delayed by this line's delay into `out` (cleared and reused).
    /// Returns false when the block was left alone (see the guards); `out` is untouched
    /// in that case.
    fn process(&mut self, output: &str, fmt: PcmFormat, src: &[u8], out: &mut Vec<u8>) -> bool {
        let frame_bytes = fmt.frame_bytes();
        if src.is_empty() || frame_bytes == 0 {
            return false;
        }
        if !src.len().is_multiple_of(frame_bytes) {
            // A partial frame would shift the channel phase of everything after it
            // (L↔R swap), which is a far worse bug than not delaying. Never seen from
            // these relays — they hand over whole quanta — so it is a guard, not a path.
            if !self.warned_misaligned {
                self.warned_misaligned = true;
                tracing::warn!(
                    "relay_delay '{output}': block of {} B is not a whole number of {frame_bytes}-byte frames — \
                     passing it through undelayed rather than risking a channel shift",
                    src.len()
                );
            }
            return false;
        }
        if self.fmt != fmt {
            let min = Self::ring_bytes_for(MAX_DELAY_US, fmt).max(src.len() + self.delay_bytes(fmt));
            self.reset_for(output, fmt, min);
        }
        let delay_bytes = self.delay_bytes(fmt);
        if delay_bytes == 0 {
            return false; // rounds to less than a frame: a true passthrough.
        }
        // The read pointer sits `delay_bytes` behind the write pointer and the window is
        // `src.len()` long, so the ring must hold both.
        if self.ring.len() < src.len() + delay_bytes {
            let min = src.len() + delay_bytes + (HEADROOM_FRAMES as usize * frame_bytes);
            self.reset_for(output, fmt, min);
        }
        let cap = self.ring.len();
        let n = src.len();

        // Write this block into the ring (1–2 memcpys).
        let head = (cap - self.write).min(n);
        self.ring[self.write..self.write + head].copy_from_slice(&src[..head]);
        if n > head {
            self.ring[..n - head].copy_from_slice(&src[head..]);
        }
        self.write = (self.write + n) % cap;
        self.valid = (self.valid + n).min(cap);

        // The window we want is the `n` bytes ending `delay_bytes` behind the newest
        // sample. Whatever part of it predates our history is silence (priming).
        let missing = (n + delay_bytes).saturating_sub(self.valid).min(n);
        let want = n - missing;
        out.clear();
        out.reserve(n);
        if missing > 0 {
            out.resize(missing, 0);
        }
        if want > 0 {
            let start = (self.write + cap - delay_bytes - want) % cap;
            let head = (cap - start).min(want);
            out.extend_from_slice(&self.ring[start..start + head]);
            if want > head {
                out.extend_from_slice(&self.ring[..want - head]);
            }
        }
        true
    }

    fn status(&self, muted: bool) -> DelayStatus {
        let delay_bytes = self.delay_bytes(self.fmt);
        let short = delay_bytes.saturating_sub(self.valid);
        DelayStatus {
            delay_us: self.delay_us,
            delay_frames: frames_for(self.delay_us, self.fmt.rate),
            rate: self.fmt.rate,
            channels: self.fmt.channels,
            primed: short == 0,
            prime_remaining_us: us_for_frames((short / self.fmt.frame_bytes()) as u64, self.fmt.rate),
            ring_bytes: self.ring.len(),
            muted,
        }
    }
}

/// Everything this module applies to **one** output at the relay hook: a provisional
/// delay, a calibration mute, or both.
///
/// One entry per affected output rather than a map per effect, so the hot path takes one
/// lock and does one lookup no matter how many effects are live, and so the
/// no-alignment gate stays a single atomic load. The entry exists while *either* effect
/// does and is dropped when both are gone, which is what keeps that gate honest.
#[derive(Default)]
struct DeviceEffect {
    /// `None` for an output that is only muted — no delay, so no ring is allocated.
    delay: Option<DelayLine>,
    /// Calibration mute (plan §12.3.2): emit exact silence, keep the ring advancing.
    muted: bool,
}

impl DeviceEffect {
    fn with_delay(delay_us: u64) -> Self {
        Self { delay: Some(DelayLine::new(delay_us)), muted: false }
    }

    /// Neither effect left ⇒ the entry can go.
    fn is_empty(&self) -> bool {
        self.delay.is_none() && !self.muted
    }

    fn set_delay(&mut self, delay_us: u64) {
        match self.delay.as_mut() {
            // Keep the ring and its history: an increase re-primes only the increment,
            // a decrease takes effect immediately.
            Some(line) => line.delay_us = delay_us,
            None => self.delay = Some(DelayLine::new(delay_us)),
        }
    }

    /// **RT hot path.** Apply this output's effects to `src`, into `out`.
    fn process(&mut self, output: &str, fmt: PcmFormat, src: &[u8], out: &mut Vec<u8>) -> bool {
        // The delay line runs *first and unconditionally*, even while muted: the ring has
        // to consume this block and move its read pointer exactly as it would have, or
        // unmuting would jump forward over the muted stretch (or replay it), which the
        // estimator would read as a real offset. Only the emitted bytes are then zeroed.
        let delayed = match self.delay.as_mut() {
            Some(line) => line.process(output, fmt, src, out),
            None => false,
        };
        if !self.muted {
            return delayed;
        }
        if src.is_empty() {
            return false;
        }
        // Exact silence, one block out per block in — so the sender's cadence, its
        // timestamps and its backlog are as untouched as they are by the delay. `clear`
        // keeps the capacity the caller's buffer already reached, so this is a memset.
        // Applied even on the paths the delay line refuses (a partial frame, a sub-frame
        // delay): zero-filling cannot shift a channel phase, and staying audible is the
        // one outcome that ruins the measurement.
        out.clear();
        out.resize(src.len(), 0);
        true
    }

    fn status(&self) -> DelayStatus {
        match &self.delay {
            Some(line) => line.status(self.muted),
            // A mute-only output: no delay, nothing to prime, no ring.
            None => DelayStatus {
                delay_us: 0,
                delay_frames: 0,
                rate: SIZING_RATE,
                channels: SIZING_CHANNELS,
                primed: true,
                prime_remaining_us: 0,
                ring_bytes: 0,
                muted: self.muted,
            },
        }
    }
}

/// Per-output relay effects — provisional delay lines and calibration mutes. One
/// process-global instance shared by every per-device relay and by whatever drives an
/// alignment run; outputs are addressed by node name, exactly like `overlay_mixer`'s
/// slots.
#[derive(Default)]
pub struct RelayDelay {
    /// Number of affected outputs (a delay, a mute, or both), so the idle hot path never
    /// takes the mutex. Written under `lines`; read `Relaxed` (see the module docs on the
    /// one-block lag).
    active: AtomicUsize,
    lines: Mutex<HashMap<String, DeviceEffect>>,
}

impl RelayDelay {
    /// The effect map, recovering from a poisoned mutex rather than propagating the
    /// panic. Deliberate: the mute clearers below are **teardown** steps, and a teardown
    /// step that can panic is a teardown step that can skip the ones after it. The
    /// critical sections are memcpys and map operations, so an inner panic would have to
    /// come from somewhere else entirely, and continuing with the map is strictly better
    /// than leaving a member silenced for as long as the daemon runs.
    fn lines(&self) -> std::sync::MutexGuard<'_, HashMap<String, DeviceEffect>> {
        self.lines.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Republish the gate the hot path reads. Must be called under `lines` by every
    /// mutator, which is why they all end with it.
    fn publish(&self, lines: &HashMap<String, DeviceEffect>) {
        self.active.store(lines.len(), Ordering::Relaxed);
    }

    /// The process-wide instance the relays read.
    pub fn global() -> &'static RelayDelay {
        static INSTANCE: OnceLock<RelayDelay> = OnceLock::new();
        INSTANCE.get_or_init(RelayDelay::default)
    }

    /// A standalone instance (tests; the global is the one relays use).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any output currently has a relay effect at all — a provisional delay or a
    /// calibration mute. One relaxed atomic load: this is the gate the RT path reads.
    pub fn any_active(&self) -> bool {
        self.active.load(Ordering::Relaxed) != 0
    }

    /// Apply a provisional delay of `delay_us` to `output`, allocating its ring here (on
    /// the calling thread, never on the relay). `0` clears the line. Returns the delay
    /// stored.
    ///
    /// Errors — rather than clamping — above [`MAX_DELAY_US`], so a caller cannot end up
    /// measuring against a delay it did not ask for.
    pub fn set_delay_us(&self, output: &str, delay_us: u64) -> anyhow::Result<u64> {
        if delay_us > MAX_DELAY_US {
            anyhow::bail!(
                "provisional delay of {:.1} ms for '{output}' exceeds the {MAX_DELAY_MS} ms cap (relay_delay::MAX_DELAY_MS)",
                delay_us as f64 / 1000.0
            );
        }
        let mut lines = self.lines();
        if delay_us == 0 {
            // A calibration mute on the same output survives: the two are independent
            // state, and dropping the mute here would make a soloed-away member audible
            // again as a side effect of clearing a delay.
            let drop_entry = match lines.get_mut(output) {
                Some(effect) => {
                    effect.delay = None;
                    effect.is_empty()
                }
                None => false,
            };
            if drop_entry {
                lines.remove(output);
            }
        } else {
            match lines.get_mut(output) {
                Some(effect) => effect.set_delay(delay_us),
                None => {
                    lines.insert(output.to_string(), DeviceEffect::with_delay(delay_us));
                }
            }
        }
        self.publish(&lines);
        Ok(delay_us)
    }

    /// [`Self::set_delay_us`] in (fractional) milliseconds — the estimator's unit.
    pub fn set_delay_ms(&self, output: &str, delay_ms: f64) -> anyhow::Result<u64> {
        if !delay_ms.is_finite() || delay_ms < 0.0 {
            anyhow::bail!("provisional delay for '{output}' must be a finite, non-negative number of ms (got {delay_ms})");
        }
        self.set_delay_us(output, (delay_ms * 1000.0).round() as u64)
    }

    /// Replace the whole provisional set in one step: every output in `delays` gets that
    /// delay, every output *not* in it is cleared. Validated in full before anything is
    /// applied, so a rejected value leaves the previous set intact.
    ///
    /// This is the shape plan §1.1's renormalisation wants — "adjust, verify, walk on" is
    /// a new complete set, not a diff — and doing it under one lock means no relay block
    /// ever sees half of a re-normalised set.
    pub fn set_all_us<'a>(&self, delays: impl IntoIterator<Item = (&'a str, u64)>) -> anyhow::Result<()> {
        let wanted: Vec<(&str, u64)> = delays.into_iter().collect();
        for (output, us) in &wanted {
            if *us > MAX_DELAY_US {
                anyhow::bail!(
                    "provisional delay of {:.1} ms for '{output}' exceeds the {MAX_DELAY_MS} ms cap (relay_delay::MAX_DELAY_MS)",
                    *us as f64 / 1000.0
                );
            }
        }
        let mut lines = self.lines();
        lines.retain(|output, effect| {
            if !wanted.iter().any(|(o, us)| *o == output.as_str() && *us > 0) {
                effect.delay = None;
            }
            // A calibration mute is not part of this set and must outlive it.
            !effect.is_empty()
        });
        for (output, us) in wanted {
            if us == 0 {
                continue;
            }
            match lines.get_mut(output) {
                Some(effect) => effect.set_delay(us),
                None => {
                    lines.insert(output.to_string(), DeviceEffect::with_delay(us));
                }
            }
        }
        self.publish(&lines);
        Ok(())
    }

    /// Drop `output`'s provisional delay (returns whether there was one). A calibration
    /// mute on the same output is left alone — see [`Self::unmute_all`] for that.
    pub fn clear(&self, output: &str) -> bool {
        let mut lines = self.lines();
        let (had, drop_entry) = match lines.get_mut(output) {
            Some(effect) => (effect.delay.take().is_some(), effect.is_empty()),
            None => (false, false),
        };
        if drop_entry {
            lines.remove(output);
        }
        self.publish(&lines);
        had
    }

    /// Drop every provisional delay — the end of a run, or an abort. Returns how many
    /// there were. Calibration mutes are a separate obligation ([`Self::clear_mutes`]).
    pub fn clear_all(&self) -> usize {
        let mut lines = self.lines();
        let mut n = 0;
        lines.retain(|_, effect| {
            n += usize::from(effect.delay.take().is_some());
            !effect.is_empty()
        });
        self.publish(&lines);
        n
    }

    /// `output`'s provisional delay in microseconds, if it has one.
    pub fn delay_us(&self, output: &str) -> Option<u64> {
        self.lines().get(output).and_then(|e| e.delay.as_ref()).map(|l| l.delay_us)
    }

    /// What this module is doing to `output` — its delay, whether it is
    /// [primed](DelayStatus), and whether it is calibration-muted. `None` when the output
    /// has neither effect.
    pub fn status(&self, output: &str) -> Option<DelayStatus> {
        self.lines().get(output).map(DeviceEffect::status)
    }

    /// Every affected output, by name (sorted — this feeds UI/logs).
    pub fn snapshot(&self) -> Vec<(String, DelayStatus)> {
        let lines = self.lines();
        let mut v: Vec<(String, DelayStatus)> = lines.iter().map(|(o, e)| (o.clone(), e.status())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Whether every live line has primed — the gate an alignment run waits on before
    /// it measures (see the module docs on priming). A mute-only output has nothing to
    /// prime.
    pub fn all_primed(&self) -> bool {
        self.lines().values().all(|e| e.status().primed)
    }

    // ---- calibration mute (plan §12.3.2, W17) -----------------------------------
    //
    // The universal fallback for audibility: it needs no cooperation from the device or
    // its host, so it covers a member whose transport has no mute of its own, a pw-sink
    // host whose agent is gone or whose sink has no volume lever, and any future kind.
    // `align/calibrate/mod.rs` decides per output which mechanism a member gets.

    /// Silence `output` at the relay hook, or stop silencing it. Returns the previous
    /// state, so a caller can tell a change from a no-op.
    ///
    /// Independent of any delay on the same output in both directions: muting does not
    /// disturb (or re-prime) the delay line, and clearing the delay does not unmute.
    pub fn set_muted(&self, output: &str, muted: bool) -> bool {
        let mut lines = self.lines();
        let previous = Self::apply_mute(&mut lines, output, muted);
        self.publish(&lines);
        previous
    }

    /// One output's mute, under a held guard. Returns the previous state. The entry is
    /// created only when muting and dropped when nothing is left on it, so
    /// [`Self::any_active`] stays an exact answer.
    fn apply_mute(lines: &mut HashMap<String, DeviceEffect>, output: &str, muted: bool) -> bool {
        if muted {
            let effect = lines.entry(output.to_string()).or_default();
            return std::mem::replace(&mut effect.muted, true);
        }
        let (previous, drop_entry) = match lines.get_mut(output) {
            Some(effect) => (std::mem::replace(&mut effect.muted, false), effect.is_empty()),
            None => (false, false),
        };
        if drop_entry {
            lines.remove(output);
        }
        previous
    }

    /// Apply a batch of mute decisions — `(output, muted)` — under **one** lock, so no
    /// relay block ever sees half of a solo. Returns how many outputs changed.
    ///
    /// Deliberately **scoped**, not a total replace: it touches exactly the outputs named
    /// and leaves every other one alone. An alignment session drives it with its own
    /// members, which is what keeps one session (or one test) from silently unmuting
    /// another's.
    pub fn set_mutes<'a>(&self, decisions: impl IntoIterator<Item = (&'a str, bool)>) -> usize {
        let mut lines = self.lines();
        let mut changed = 0;
        for (output, muted) in decisions {
            changed += usize::from(Self::apply_mute(&mut lines, output, muted) != muted);
        }
        self.publish(&lines);
        changed
    }

    /// Whether `output` is calibration-muted here.
    pub fn is_muted(&self, output: &str) -> bool {
        self.lines().get(output).is_some_and(|e| e.muted)
    }

    /// Every calibration-muted output (sorted).
    pub fn muted_outputs(&self) -> Vec<String> {
        let lines = self.lines();
        let mut v: Vec<String> = lines.iter().filter(|(_, e)| e.muted).map(|(o, _)| o.clone()).collect();
        v.sort();
        v
    }

    /// Whether anything is calibration-muted (for reporting; the RT gate is
    /// [`Self::any_active`]).
    pub fn any_muted(&self) -> bool {
        self.lines().values().any(|e| e.muted)
    }

    /// **Teardown.** Drop the calibration mute on exactly these outputs, returning how
    /// many were muted.
    ///
    /// Scoped to the caller's own outputs on purpose: an alignment hold releases the
    /// mutes it took, and a *stale* hold releasing late can then only ever touch outputs
    /// it held itself. Infallible, idempotent, and a pure removal — so it is safe as one
    /// step of a teardown sequence that must not skip the steps after it.
    pub fn unmute_all<'a>(&self, outputs: impl IntoIterator<Item = &'a str>) -> usize {
        self.set_mutes(outputs.into_iter().map(|o| (o, false)))
    }

    /// Drop **every** calibration mute — the abort of last resort. Returns how many
    /// there were.
    pub fn clear_mutes(&self) -> usize {
        let mut lines = self.lines();
        let mut n = 0;
        lines.retain(|_, effect| {
            n += usize::from(std::mem::replace(&mut effect.muted, false));
            !effect.is_empty()
        });
        self.publish(&lines);
        n
    }

    /// **RT hot path.** Apply `output`'s relay effects — its provisional delay and/or its
    /// calibration mute — to `src`, writing the result into `out` (cleared and reused
    /// across blocks, like the relays' `mix_buf`).
    ///
    /// Returns `true` when `out` holds the audio to send, `false` when this output has
    /// neither effect (or the block could not be delayed safely) and the caller should
    /// keep using `src` — the same contract as `outputs::overlay_mixer::mix_into`, so the two
    /// compose by chaining. Call it **after** `mix_into` (see the module docs for why).
    ///
    /// The name predates the mute; the contract did not change, which is why the three
    /// relay call sites did not have to.
    ///
    /// `fmt` is the format of `src`, which the caller knows statically (48 kHz stereo for
    /// sendspin and pw-sink, the negotiated group rate for AP2); passing it here is what
    /// keeps this module free of a per-output rate registry.
    pub fn delay_into(&self, output: &str, fmt: PcmFormat, src: &[u8], out: &mut Vec<u8>) -> bool {
        // The whole point of the counter: no alignment running ⇒ no lock, no lookup.
        if self.active.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let mut lines = self.lines();
        match lines.get_mut(output) {
            Some(effect) => effect.process(output, fmt, src, out),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests;
