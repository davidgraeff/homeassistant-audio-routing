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
//! capture ─▶ overlay_mixer::mix_into(node, …) ─▶ relay_delay::delay_into(node, …) ─▶ encoder/sender
//! ```
//!
//! (`sendspin_server.rs`, `ap2_server.rs`, `pwsink_server.rs`; for sendspin this sits
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
//! `align/calibrate.rs` resolves which mechanism each member gets, per output.
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
//! orchestration is W21, in `align/measure.rs` (`plan_equivalence`, `EquivalenceReport`);
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
//!    (`sync_settings::PWSINK_JITTER_MIN_MS` = 15 ms), while this line is sample-accurate
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
//!   960-frame Opus blocks (`sendspin_codec::Reblocker`) and this line sits upstream of
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
//! (`sendspin_server::required_send_ahead_us`) and so can force a group-wide restart
//! when it crosses the high-water mark (plan §9.2) — the provisional delay never does,
//! which is a reason the final write can behave differently from the walk that preceded
//! it, and a reason to check the mark *before* writing.

// W13 delivers this library plus the three relay hooks; the measurement state machine
// and the HTTP surface that drive it (plan §11) land separately, so the setters and
// inspectors have no in-tree caller yet. Same convention pwsink_server.rs used while its
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
    // `align/calibrate.rs` decides per output which mechanism a member gets.

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
    /// keep using `src` — the same contract as `overlay_mixer::mix_into`, so the two
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
mod tests {
    use super::*;

    const F48: PcmFormat = PcmFormat::new(48_000, 2);
    const FRAME: usize = 4;

    /// A stream whose every frame is identifiable: frame `i` is `(i, i)` as two i16s.
    fn frames(from: usize, count: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(count * FRAME);
        for i in from..from + count {
            let s = (i % 30_000) as i16;
            v.extend_from_slice(&s.to_le_bytes());
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    fn silence(count: usize) -> Vec<u8> {
        vec![0u8; count * FRAME]
    }

    /// Push `blocks` blocks of `block_frames` of the identifiable stream through `rd`,
    /// returning the concatenated output (undelayed blocks pass through, as the relays
    /// would do with the `false` return).
    fn run(rd: &RelayDelay, output: &str, block_frames: usize, blocks: usize, start_frame: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut got = Vec::new();
        for b in 0..blocks {
            let src = frames(start_frame + b * block_frames, block_frames);
            let delayed = rd.delay_into(output, F48, &src, &mut buf);
            got.extend_from_slice(if delayed { &buf } else { &src });
        }
        got
    }

    #[test]
    fn no_delay_is_a_true_passthrough_and_costs_no_lookup() {
        let rd = RelayDelay::new();
        assert!(!rd.any_active());
        let mut out = vec![0xAAu8; 8]; // must be left untouched
        assert!(!rd.delay_into("out-a", F48, &frames(0, 64), &mut out));
        assert_eq!(out, vec![0xAAu8; 8]);
        // A line on ANOTHER output must not delay this one.
        rd.set_delay_us("out-b", 5_000).unwrap();
        assert!(rd.any_active());
        assert!(!rd.delay_into("out-a", F48, &frames(0, 64), &mut out));
        assert_eq!(out, vec![0xAAu8; 8]);
        assert!(rd.clear("out-b"));
        assert!(!rd.any_active());
    }

    #[test]
    fn delay_is_sample_accurate_and_not_quantised_to_the_block_size() {
        // 1001 frames is deliberately NOT a multiple of the 960-frame Opus block the
        // sendspin relay re-cuts to, nor of the block size used here. This is the
        // property plan §1.1.1 turns on.
        const BLOCK: usize = 960;
        const D: usize = 1001;
        let rd = RelayDelay::new();
        let us = us_for_frames(D as u64, 48_000);
        rd.set_delay_us("spk", us).unwrap();
        assert_eq!(rd.status("spk").unwrap().delay_frames, D as u64, "µs↔frames round trip must be exact");

        let got = run(&rd, "spk", BLOCK, 6, 0);
        let mut want = silence(D);
        want.extend_from_slice(&frames(0, BLOCK * 6 - D));
        assert_eq!(got, want, "output must be the input shifted by exactly {D} frames");
    }

    #[test]
    fn delay_survives_ring_wraparound() {
        // Enough audio to wrap the (MAX-sized) ring several times.
        const BLOCK: usize = 1024;
        const D: usize = 7_777;
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", us_for_frames(D as u64, 48_000)).unwrap();
        let ring_frames = rd.status("spk").unwrap().ring_bytes / FRAME;
        let blocks = (ring_frames * 3) / BLOCK;
        let got = run(&rd, "spk", BLOCK, blocks, 0);
        let mut want = silence(D);
        want.extend_from_slice(&frames(0, BLOCK * blocks - D));
        assert_eq!(got.len(), want.len());
        assert_eq!(got, want, "content must stay exact across {} ring wraps", 3);
    }

    #[test]
    fn priming_emits_silence_then_content_with_no_discontinuity() {
        const BLOCK: usize = 480;
        const D: usize = 1_200; // 2.5 blocks: priming ends mid-block
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", us_for_frames(D as u64, 48_000)).unwrap();

        let st = rd.status("spk").unwrap();
        assert!(!st.primed);
        assert_eq!(st.prime_remaining_us, us_for_frames(D as u64, 48_000));

        let got = run(&rd, "spk", BLOCK, 2, 0);
        assert_eq!(got, silence(BLOCK * 2), "before {D} frames have flowed the output is silence");
        assert!(!rd.status("spk").unwrap().primed);

        // Third block straddles the boundary: 240 frames of silence, then frame 0.
        let mut buf = Vec::new();
        let src = frames(2 * BLOCK, BLOCK);
        assert!(rd.delay_into("spk", F48, &src, &mut buf));
        let mut want = silence(D - 2 * BLOCK);
        want.extend_from_slice(&frames(0, BLOCK - (D - 2 * BLOCK)));
        assert_eq!(buf, want);
        let st = rd.status("spk").unwrap();
        assert!(st.primed);
        assert_eq!(st.prime_remaining_us, 0);
    }

    #[test]
    fn increasing_the_delay_reprimes_only_the_increment() {
        const BLOCK: usize = 960;
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", us_for_frames(100, 48_000)).unwrap();
        // Prime and run past it.
        let _ = run(&rd, "spk", BLOCK, 4, 0);
        assert!(rd.status("spk").unwrap().primed);

        // 100 → 5000 frames: 4900 frames of history are missing, so that much silence
        // comes out, and then content resumes exactly 5000 frames behind.
        rd.set_delay_us("spk", us_for_frames(5_000, 48_000)).unwrap();
        assert!(!rd.status("spk").unwrap().primed);
        let got = run(&rd, "spk", BLOCK, 8, BLOCK * 4);
        // History available at the change: 4*960 = 3840 frames (frames 0..3839).
        // First output frame wanted is (3840 + 1) - 5000 → pre-history ⇒ silence until
        // the wanted index reaches 0, i.e. for 5000 - 3840 = 1160 frames.
        let mut want = silence(1_160);
        want.extend_from_slice(&frames(0, BLOCK * 8 - 1_160));
        assert_eq!(got, want);
        assert!(rd.status("spk").unwrap().primed);
    }

    #[test]
    fn decreasing_the_delay_skips_forward_and_stays_exact() {
        const BLOCK: usize = 960;
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", us_for_frames(2_000, 48_000)).unwrap();
        let _ = run(&rd, "spk", BLOCK, 4, 0); // 3840 frames in, primed
        rd.set_delay_us("spk", us_for_frames(500, 48_000)).unwrap();
        assert!(rd.status("spk").unwrap().primed, "a shorter delay needs no new history");
        // Next block covers input frames 3840..4799; at delay 500 the output is
        // 3341..4300 — a jump forward over the 1500 frames the old delay was holding.
        let mut buf = Vec::new();
        let src = frames(4 * BLOCK, BLOCK);
        assert!(rd.delay_into("spk", F48, &src, &mut buf));
        assert_eq!(buf, frames(4 * BLOCK - 500, BLOCK));
    }

    #[test]
    fn the_cap_holds_and_a_rejection_leaves_the_previous_delay_alone() {
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", 20_000).unwrap();
        assert!(rd.set_delay_us("spk", MAX_DELAY_US + 1).is_err());
        assert_eq!(rd.delay_us("spk"), Some(20_000), "a rejected value must not disturb the applied one");
        assert!(rd.set_delay_ms("spk", MAX_DELAY_MS as f64 + 0.001).is_err());
        assert!(rd.set_delay_ms("spk", -1.0).is_err());
        assert!(rd.set_delay_ms("spk", f64::NAN).is_err());
        // The bound is what makes the ring a fixed cost: 1000 ms + headroom at 48 kHz
        // stereo.
        let ring = rd.status("spk").unwrap().ring_bytes;
        assert_eq!(ring, (48_000 + HEADROOM_FRAMES as usize) * FRAME);
        assert_eq!(ring, 224_768, "documented per-output memory cost (~220 KiB)");
        // At the cap itself the line still works.
        rd.set_delay_us("spk", MAX_DELAY_US).unwrap();
        assert_eq!(rd.status("spk").unwrap().delay_frames, 48_000);
        let mut buf = Vec::new();
        assert!(rd.delay_into("spk", F48, &frames(0, 1024), &mut buf));
        assert_eq!(buf, silence(1024));
    }

    #[test]
    fn zero_clears_the_line_and_a_sub_frame_delay_is_a_passthrough() {
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", 5_000).unwrap();
        assert_eq!(rd.set_delay_us("spk", 0).unwrap(), 0);
        assert_eq!(rd.delay_us("spk"), None);
        assert!(!rd.any_active());
        // 10 µs at 48 kHz rounds to 0 frames: the line exists but must not corrupt audio.
        rd.set_delay_us("spk", 10).unwrap();
        let mut buf = Vec::new();
        assert!(!rd.delay_into("spk", F48, &frames(0, 64), &mut buf));
    }

    #[test]
    fn composes_with_an_active_announcement_overlay_in_the_relay_order() {
        // Mirrors the call sites: mix_into first, then delay_into on its result — so the
        // delay applies to duck(music)+overlay, exactly as a device-side knob would.
        let output = "relay-delay-compose-test";
        let mixer = crate::overlay_mixer::OverlayMixer::global();
        const BLOCK: usize = 480;
        const D: usize = 700;
        let rd = RelayDelay::new();
        rd.set_delay_us(output, us_for_frames(D as u64, 48_000)).unwrap();

        // A long enough clip that it stays active for the whole test.
        let clip = frames(9_000, BLOCK * 8);
        mixer.start(output, 1, clip.clone(), 0.5);

        let mut mix_buf = Vec::new();
        let mut delay_buf = Vec::new();
        let mut mixed_stream = Vec::new();
        let mut out_stream = Vec::new();
        for b in 0..6 {
            let src = frames(b * BLOCK, BLOCK);
            let overlaid = mixer.mix_into(output, &src, &mut mix_buf);
            assert!(overlaid, "the overlay must be active for this test to mean anything");
            mixed_stream.extend_from_slice(&mix_buf);
            let delayed = rd.delay_into(output, F48, &mix_buf, &mut delay_buf);
            assert!(delayed);
            out_stream.extend_from_slice(&delay_buf);
        }
        mixer.stop(output);
        let _ = mixer.take_finished();

        // The delayed stream is the *mixed* stream shifted by D — i.e. the announcement
        // is delayed too, and nothing about the mix is disturbed.
        let mut want = silence(D);
        want.extend_from_slice(&mixed_stream[..mixed_stream.len() - D * FRAME]);
        assert_eq!(out_stream, want);
        // Sanity: the mix really did change the audio (so we aren't comparing music).
        assert_ne!(mixed_stream[..BLOCK * FRAME], frames(0, BLOCK)[..]);
    }

    #[test]
    fn a_rate_change_reprimes_and_keeps_the_delay_in_time_not_samples() {
        // An AP2 group renegotiating 48 → 44.1 kHz: the ring's contents are a different
        // rate, so history is dropped, but the *time* delay is preserved.
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", 10_000).unwrap(); // 10 ms
        let _ = run(&rd, "spk", 960, 4, 0);
        assert_eq!(rd.status("spk").unwrap().delay_frames, 480);
        assert!(rd.status("spk").unwrap().primed);

        let f441 = PcmFormat::new(44_100, 2);
        let mut buf = Vec::new();
        assert!(rd.delay_into("spk", f441, &frames(0, 100), &mut buf));
        let st = rd.status("spk").unwrap();
        assert_eq!(st.rate, 44_100);
        assert_eq!(st.delay_frames, 441, "10 ms at 44.1 kHz");
        assert!(!st.primed, "history from the old rate must not be reused — it re-primes from scratch");
        assert_eq!(st.prime_remaining_us, us_for_frames(441 - 100, 44_100));
        assert_eq!(buf, silence(100));
        // No reallocation on the way down in rate: the ring was sized for 48 kHz.
        assert_eq!(st.ring_bytes, (48_000 + HEADROOM_FRAMES as usize) * FRAME);
    }

    #[test]
    fn a_block_that_is_not_whole_frames_passes_through_undelayed() {
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", 5_000).unwrap();
        let mut buf = Vec::new();
        let mut src = frames(0, 64);
        src.push(0x7f); // 257 bytes: not a whole stereo frame
        assert!(!rd.delay_into("spk", F48, &src, &mut buf));
        assert!(!rd.delay_into("spk", F48, &[], &mut buf));
    }

    #[test]
    fn set_all_replaces_the_whole_set_atomically() {
        let rd = RelayDelay::new();
        rd.set_delay_us("a", 1_000).unwrap();
        rd.set_delay_us("b", 2_000).unwrap();
        rd.set_all_us([("b", 3_000), ("c", 4_000)]).unwrap();
        let snap: Vec<(String, u64)> = rd.snapshot().into_iter().map(|(o, s)| (o, s.delay_us)).collect();
        assert_eq!(snap, vec![("b".to_string(), 3_000), ("c".to_string(), 4_000)], "'a' must be gone, 'b' updated");

        // A rejected member leaves the previous set untouched.
        assert!(rd.set_all_us([("b", 1), ("d", MAX_DELAY_US + 1)]).is_err());
        let snap: Vec<(String, u64)> = rd.snapshot().into_iter().map(|(o, s)| (o, s.delay_us)).collect();
        assert_eq!(snap, vec![("b".to_string(), 3_000), ("c".to_string(), 4_000)]);

        // A zero in a batch means "no delay on that output".
        rd.set_all_us([("b", 0), ("c", 5_000)]).unwrap();
        assert_eq!(rd.delay_us("b"), None);
        assert_eq!(rd.clear_all(), 1);
        assert!(!rd.any_active());
    }

    #[test]
    fn all_primed_is_the_measurement_gate() {
        let rd = RelayDelay::new();
        assert!(rd.all_primed(), "no lines ⇒ nothing to wait for");
        rd.set_delay_us("a", 10_000).unwrap();
        rd.set_delay_us("b", 1_000).unwrap();
        assert!(!rd.all_primed());
        let _ = run(&rd, "b", 960, 2, 0); // 'b' primes (48 frames)
        assert!(rd.status("b").unwrap().primed);
        assert!(!rd.all_primed(), "'a' still hasn't seen audio");
        let _ = run(&rd, "a", 960, 1, 0); // 960 > 480 frames
        assert!(rd.all_primed());
    }

    // ---- calibration mute (W17) ---------------------------------------------------

    #[test]
    fn a_calibration_mute_is_exact_silence_and_the_idle_path_stays_free() {
        let rd = RelayDelay::new();
        let mut out = vec![0xAAu8; 8]; // must be left untouched while nothing applies
        assert!(!rd.delay_into("spk", F48, &frames(0, 64), &mut out));
        assert_eq!(out, vec![0xAAu8; 8], "no effects ⇒ one atomic load and nothing else");

        // A mute on ANOTHER output opens the gate but must not silence this one.
        assert!(!rd.set_muted("other", true), "there was no mute before");
        assert!(rd.any_active() && rd.any_muted());
        assert!(!rd.delay_into("spk", F48, &frames(0, 64), &mut out));
        assert_eq!(out, vec![0xAAu8; 8]);

        // Muted: exact silence, one block out per block in, and no ring at all.
        rd.set_muted("spk", true);
        let src = frames(0, 64);
        assert!(rd.delay_into("spk", F48, &src, &mut out));
        assert_eq!(out, silence(64), "every sample is zero — silence, not attenuation");
        let st = rd.status("spk").unwrap();
        assert!(st.muted);
        assert_eq!((st.delay_us, st.ring_bytes, st.primed), (0, 0, true), "a mute-only output costs no memory and has nothing to prime");
        assert_eq!(rd.snapshot().iter().filter(|(_, s)| s.muted).count(), 2);

        // Unmuting drops the entry, so the RT gate closes again.
        assert!(rd.set_muted("spk", false), "the previous state is reported");
        assert!(!rd.delay_into("spk", F48, &src, &mut out));
        rd.set_muted("other", false);
        assert!(!rd.any_active(), "nothing left ⇒ the hot path is free again");
        assert!(rd.status("spk").is_none());
    }

    /// The property that makes the mute safe to use mid-run: the delay line keeps
    /// consuming and advancing while muted, so unmuting resumes at the sample continuous
    /// playback would have reached. Asserted on **samples** against an identical,
    /// never-muted line — a stalled ring would replay the muted stretch and a skipped one
    /// would jump over it, and the estimator would measure either as a real offset.
    #[test]
    fn the_ring_keeps_advancing_while_muted_so_unmuting_is_continuous() {
        const BLOCK: usize = 480;
        const D: usize = 1_200;
        let us = us_for_frames(D as u64, 48_000);
        let reference = RelayDelay::new();
        let muted = RelayDelay::new();
        reference.set_delay_us("spk", us).unwrap();
        muted.set_delay_us("spk", us).unwrap();

        let (mut buf_ref, mut buf_mut) = (Vec::new(), Vec::new());
        for blk in 0..12 {
            if blk == 4 {
                muted.set_muted("spk", true);
            }
            if blk == 7 {
                assert!(muted.set_muted("spk", false));
            }
            let src = frames(blk * BLOCK, BLOCK);
            assert!(reference.delay_into("spk", F48, &src, &mut buf_ref));
            assert!(muted.delay_into("spk", F48, &src, &mut buf_mut));
            if (4..7).contains(&blk) {
                assert_eq!(buf_mut, silence(BLOCK), "block {blk} must be silent");
            } else {
                assert_eq!(buf_mut, buf_ref, "block {blk} must be exactly the never-muted stream");
            }
        }
        // The first block after the unmute, spelled out: input frame 7*480 read 1200
        // frames back, i.e. the audio the muted stretch was consuming — not a repeat of
        // it, and not a jump past it.
        let mut buf = Vec::new();
        assert!(muted.delay_into("spk", F48, &frames(12 * BLOCK, BLOCK), &mut buf));
        assert_eq!(buf, frames(12 * BLOCK - D, BLOCK));
        // And the delay is untouched: a mute neither re-primes nor changes the offset.
        let st = muted.status("spk").unwrap();
        assert!(st.primed && !st.muted);
        assert_eq!(st.delay_frames, D as u64);
    }

    /// A mute, an announcement overlay and a delay offset, all live at once — the state a
    /// barge-in during a solo actually produces.
    #[test]
    fn a_mute_composes_with_an_overlay_and_a_delay_at_the_same_time() {
        let output = "relay-delay-mute-compose-test";
        let mixer = crate::overlay_mixer::OverlayMixer::global();
        const BLOCK: usize = 480;
        const D: usize = 700;
        let rd = RelayDelay::new();
        rd.set_delay_us(output, us_for_frames(D as u64, 48_000)).unwrap();
        mixer.start(output, 1, frames(9_000, BLOCK * 16), 0.5);

        let (mut mix_buf, mut delay_buf) = (Vec::new(), Vec::new());
        let (mut mixed, mut got) = (Vec::new(), Vec::new());
        for blk in 0..10 {
            if blk == 3 {
                rd.set_muted(output, true);
            }
            if blk == 6 {
                rd.set_muted(output, false);
            }
            let src = frames(blk * BLOCK, BLOCK);
            assert!(mixer.mix_into(output, &src, &mut mix_buf), "the overlay must stay active for this to mean anything");
            mixed.extend_from_slice(&mix_buf);
            assert!(rd.delay_into(output, F48, &mix_buf, &mut delay_buf));
            got.extend_from_slice(&delay_buf);
        }
        mixer.stop(output);
        let _ = mixer.take_finished();

        // Expected: duck(music)+overlay, shifted by D, with the muted blocks zeroed. So
        // the mute silences the announcement too — exactly what a device-side mute does —
        // while the delay is unaffected in both directions.
        let mut want = silence(D);
        want.extend_from_slice(&mixed[..mixed.len() - D * FRAME]);
        for blk in 3..6 {
            want[blk * BLOCK * FRAME..(blk + 1) * BLOCK * FRAME].fill(0);
        }
        assert_eq!(got, want);
        // Nothing was lost inside the muted window: the ring kept advancing, so the
        // block after the unmute carries real (mixed) audio.
        assert_ne!(got[6 * BLOCK * FRAME..7 * BLOCK * FRAME], silence(BLOCK)[..]);
    }

    #[test]
    fn a_mute_and_a_delay_are_independent_state_on_the_same_output() {
        let rd = RelayDelay::new();
        rd.set_delay_us("spk", 5_000).unwrap();
        rd.set_muted("spk", true);

        // Clearing the delay must NOT unmute: a silenced member becoming audible again as
        // a side effect of a renormalisation is the hazard this mechanism exists for.
        assert!(rd.clear("spk"));
        assert!(rd.is_muted("spk") && rd.any_active());
        assert_eq!(rd.delay_us("spk"), None);
        assert_eq!(rd.status("spk").unwrap().ring_bytes, 0, "the ring goes with the delay");

        // Nor do the batch delay setters.
        rd.set_delay_us("spk", 5_000).unwrap();
        rd.set_all_us([("other", 1_000)]).unwrap();
        assert!(rd.is_muted("spk"), "set_all_us replaces the delay set, not the mute set");
        assert_eq!(rd.delay_us("spk"), None);
        rd.set_delay_us("spk", 5_000).unwrap();
        assert_eq!(rd.clear_all(), 2, "two delays cleared");
        assert!(rd.is_muted("spk"), "…and the mute survived that too");

        // The other way round: a mute change leaves the line and its history alone.
        rd.set_delay_us("spk", us_for_frames(480, 48_000)).unwrap();
        let _ = run(&rd, "spk", 960, 2, 0);
        assert!(rd.status("spk").unwrap().primed);
        rd.set_muted("spk", true);
        assert!(rd.status("spk").unwrap().primed, "muting must not re-prime");
        assert!(rd.set_muted("spk", false));
        assert!(rd.status("spk").unwrap().primed);
        assert_eq!(rd.delay_us("spk"), Some(us_for_frames(480, 48_000)));
    }

    #[test]
    fn mute_batches_are_scoped_and_every_clearer_is_a_removal() {
        let rd = RelayDelay::new();
        // One solo = one batch = one lock, so no relay block sees half of it.
        assert_eq!(rd.set_mutes([("a", true), ("b", true), ("c", false)]), 2);
        assert_eq!(rd.muted_outputs(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(rd.set_mutes([("a", true), ("b", true)]), 0, "idempotent");

        // The next position's solo moves the mutes and touches nothing else.
        rd.set_muted("elsewhere", true);
        assert_eq!(rd.set_mutes([("a", false), ("b", true), ("c", true)]), 2);
        assert_eq!(rd.muted_outputs(), vec!["b".to_string(), "c".to_string(), "elsewhere".to_string()]);

        // Teardown, scoped: a stale hold releasing late can only touch its own outputs.
        assert_eq!(rd.unmute_all(["b", "c"]), 2);
        assert_eq!(rd.muted_outputs(), vec!["elsewhere".to_string()]);
        assert_eq!(rd.unmute_all(["b", "c"]), 0, "idempotent, and never a panic");

        // The abort of last resort.
        assert_eq!(rd.clear_mutes(), 1);
        assert!(!rd.any_muted() && !rd.any_active());
        assert_eq!(rd.clear_mutes(), 0);

        // A delay on an output whose mute is cleared keeps its entry and its ring.
        rd.set_delay_us("d", 4_000).unwrap();
        rd.set_muted("d", true);
        assert_eq!(rd.clear_mutes(), 1);
        assert_eq!(rd.delay_us("d"), Some(4_000));
        assert!(rd.any_active());
    }

    #[test]
    fn frame_conversion_rounds_to_nearest_and_round_trips() {
        assert_eq!(frames_for(0, 48_000), 0);
        assert_eq!(frames_for(20_833, 48_000), 1_000); // 20.833 ms
        assert_eq!(frames_for(10, 48_000), 0); // under half a frame
        assert_eq!(frames_for(11, 48_000), 1); // over half a frame (20.83 µs)
        for frames in [1u64, 7, 959, 960, 961, 1_001, 47_999, 48_000] {
            assert_eq!(frames_for(us_for_frames(frames, 48_000), 48_000), frames, "48k round trip for {frames}");
        }
        for frames in [1u64, 7, 441, 1_001, 44_100] {
            assert_eq!(frames_for(us_for_frames(frames, 44_100), 44_100), frames, "44.1k round trip for {frames}");
        }
        assert_eq!(F48.frame_bytes(), 4);
    }
}
