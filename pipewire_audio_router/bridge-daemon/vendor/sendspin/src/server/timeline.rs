// ABOUTME: SharedTimeline — the group's audio timeline (clock domain, anchor,
// ABOUTME: per-chunk advancement), extracted from Group so that several
// ABOUTME: independent per-device senders can stamp *identical* timestamps.

use crate::protocol::messages::StreamPlayerConfig;
use crate::sync::raw_clock::Clock;
use std::sync::{Arc, Mutex};

/// How far ahead of "now" (in this timeline's clock domain) audio is anchored,
/// by default. Must comfortably exceed a client's own startup buffering latency,
/// or it'll receive chunks whose intended playback time has already passed.
pub const DEFAULT_SEND_AHEAD_US: i64 = 250_000;

/// The single source of truth for a synchronized group's playback timeline.
///
/// Sample-accurate multi-room sync only requires that, for a given chunk of
/// audio, every member is handed the *same* `server/time`-domain timestamp:
/// each member then converts that one timestamp into its own clock domain and
/// schedules playback locally. A [`SharedTimeline`] owns that timestamp
/// stream, decoupled from *who* the audio is sent to.
///
/// Today a single [`crate::server::Group`] owns one timeline and fans one frame
/// to every member — sync is automatic. Splitting playback into one
/// independently-addressable sender per device (so a device can be ducked,
/// overlaid, or routed on its own) would give each sender its own timeline, and
/// they'd anchor at different instants and drift. Sharing **one**
/// `Arc<SharedTimeline>` across those senders removes that failure mode: the
/// timeline is stamped exactly once per chunk ([`SharedTimeline::stamp`]) and
/// the resulting timestamp is handed to every sender, so chunk *N* carries an
/// identical `ts` for all members regardless of per-sender callback ordering —
/// the same correctness property as a single group, with per-device addressing.
pub struct SharedTimeline {
    clock: Arc<dyn Clock>,
    send_ahead_us: i64,
    state: Mutex<TimelineState>,
}

#[derive(Debug)]
struct TimelineState {
    /// The format currently streaming, used to derive each chunk's duration.
    /// `None` before the first `start`/after `clear`.
    config: Option<StreamPlayerConfig>,
    /// Timestamp (this timeline's clock domain) to stamp on the next chunk.
    /// `None` until the first stamp after a start/clear (re)anchors.
    next_ts_us: Option<i64>,
    /// Carry for the sub-microsecond part of a chunk's duration (numerator over
    /// the sample rate), so advancing the timeline doesn't accumulate drift.
    residue: i64,
}

impl std::fmt::Debug for SharedTimeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn Clock>` isn't Debug, and the clock's identity is not what a
        // reader of this wants anyway.
        f.debug_struct("SharedTimeline")
            .field("send_ahead_us", &self.send_ahead_us)
            .field("state", &*self.state())
            .finish()
    }
}

impl SharedTimeline {
    /// The timeline state, recovering from a poisoned lock rather than propagating
    /// the panic — see the equivalent on `Group`. The state is an `Option<config>`
    /// plus two integers, and the only damage a half-finished mutation can do is a
    /// stale anchor, which the re-anchor branch in [`Self::stamp`] heals on the next
    /// chunk. Killing the audio path to protect that would be a bad trade.
    fn state(&self) -> std::sync::MutexGuard<'_, TimelineState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SharedTimeline {
    /// Create a timeline in `clock`'s domain. Pass the same clock the
    /// [`crate::server::ServerListener`] that accepted these connections was
    /// built with, so timestamps here match the `server/time` replies members
    /// already trust. On Linux `DefaultClock` reads `CLOCK_MONOTONIC_RAW`
    /// directly, so two `DefaultClock` instances share one process-wide
    /// timebase — but passing the *same* `Arc` is the intended, portable way to
    /// guarantee a shared domain across senders.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            send_ahead_us: DEFAULT_SEND_AHEAD_US,
            state: Mutex::new(TimelineState {
                config: None,
                next_ts_us: None,
                residue: 0,
            }),
        }
    }

    /// Override the default send-ahead lead time (builder style).
    pub fn with_send_ahead_us(mut self, send_ahead_us: i64) -> Self {
        self.send_ahead_us = send_ahead_us;
        self
    }

    /// The configured presentation lead.
    pub fn send_ahead_us(&self) -> i64 {
        self.send_ahead_us
    }

    /// The shared clock, so a caller building a parallel sender can hand the
    /// exact same clock domain to its connections.
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// Set the streaming format and re-anchor the timeline. Call when a stream
    /// starts (or its format changes).
    pub fn set_config(&self, config: StreamPlayerConfig) {
        let mut state = self.state();
        state.config = Some(config);
        state.next_ts_us = None;
        state.residue = 0;
    }

    /// The format currently streaming, if any — used to (re)issue `stream/start`
    /// to a late-joining member.
    pub fn config(&self) -> Option<StreamPlayerConfig> {
        self.state().config.clone()
    }

    /// Clear the streaming format and reset the timeline (stream ended).
    pub fn clear_config(&self) {
        let mut state = self.state();
        state.config = None;
        state.next_ts_us = None;
        state.residue = 0;
    }

    /// Reset the anchor without touching the format (e.g. after a clear/seek).
    pub fn reset(&self) {
        let mut state = self.state();
        state.next_ts_us = None;
        state.residue = 0;
    }

    /// Advance the timeline by one chunk and return that chunk's timestamp.
    ///
    /// **Call exactly once per chunk**, then hand the returned `ts` to every
    /// sender for that chunk — that is what keeps members coincident. The
    /// timestamp comes from an anchored timeline, not `now + lead` per call, so
    /// pushing faster or slower than real time doesn't shift playback: the
    /// first stamp after a start/clear anchors at `now + send_ahead_us`, and
    /// each stamp advances by the chunk's own duration (derived from the
    /// format). If stamps fall behind — the timeline would schedule a chunk too
    /// close to now — it re-anchors forward. Half the lead is the low-water
    /// mark, giving hysteresis so steady real-time pacing doesn't re-anchor
    /// every chunk.
    pub fn stamp(&self, pcm_len: usize) -> i64 {
        // Read the clock before taking the lock. `now` only feeds the re-anchor
        // comparison below, so a slightly staler reading can only make that
        // decision more conservative — and on some targets this is a syscall, which
        // has no business inside a lock a real-time producer contends for.
        let now = self.clock.now_micros();
        let mut state = self.state();

        let ts = match state.next_ts_us {
            Some(t) if t >= now + self.send_ahead_us / 2 => t,
            _ => now + self.send_ahead_us,
        };

        // Advance the timeline by this chunk's exact duration, carrying the
        // fractional-microsecond remainder so it doesn't drift.
        let advanced = match &state.config {
            Some(cfg) => {
                let bytes_per_sample = (cfg.channels as usize) * (cfg.bit_depth as usize / 8);
                if bytes_per_sample > 0 && cfg.sample_rate > 0 {
                    let samples = (pcm_len / bytes_per_sample) as i64;
                    let total = samples * 1_000_000 + state.residue;
                    let rate = cfg.sample_rate as i64;
                    state.residue = total % rate;
                    ts + total / rate
                } else {
                    ts
                }
            }
            None => ts,
        };
        state.next_ts_us = Some(advanced);
        ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::raw_clock::DefaultClock;

    fn pcm_config() -> StreamPlayerConfig {
        StreamPlayerConfig {
            codec: "pcm".to_string(),
            sample_rate: 48000,
            channels: 2,
            bit_depth: 16,
            codec_header: None,
        }
    }

    #[test]
    fn advances_by_exact_chunk_duration_with_residue_carry() {
        let tl = SharedTimeline::new(Arc::new(DefaultClock::default()));
        tl.set_config(pcm_config());
        // 4 bytes/frame @ 48kHz. 192 bytes = 48 frames = exactly 1000µs.
        let t0 = tl.stamp(192);
        let t1 = tl.stamp(192);
        assert_eq!(t1 - t0, 1000, "each 48-frame chunk is exactly 1ms");

        // A chunk that isn't a whole number of microseconds must carry the
        // remainder so it doesn't drift: 4 frames @ 48kHz = 83.333µs. Over three
        // chunks the individual advances are 83, 83, 84 — the carried residue
        // lands on the third advance so twelve frames span exactly 250µs.
        let tl = SharedTimeline::new(Arc::new(DefaultClock::default()));
        tl.set_config(pcm_config());
        let a = tl.stamp(16); // 4 frames
        let b = tl.stamp(16);
        let c = tl.stamp(16);
        let d = tl.stamp(16);
        assert_eq!(b - a, 83);
        assert_eq!(c - b, 83);
        assert_eq!(d - c, 84, "the residue carry lands on the third advance");
        assert_eq!(d - a, 250, "12 frames @ 48kHz is exactly 250µs, no drift");
    }

    #[test]
    fn two_readers_of_one_timeline_agree_when_stamped_once_per_chunk() {
        // One stamp() per chunk is the single source of truth: the caller fans
        // that one timestamp out to every sender.
        let tl = Arc::new(SharedTimeline::new(Arc::new(DefaultClock::default())));
        tl.set_config(pcm_config());
        let a = Arc::clone(&tl);
        let b = Arc::clone(&tl);
        // Same Arc → same next_ts progression. Stamp once per chunk, share ts.
        let ts0 = tl.stamp(192);
        // Both "senders" would encode with ts0 for chunk 0 — trivially identical.
        assert!(Arc::ptr_eq(&a, &b));
        let ts1 = tl.stamp(192);
        assert_eq!(ts1 - ts0, 1000);
    }
}
