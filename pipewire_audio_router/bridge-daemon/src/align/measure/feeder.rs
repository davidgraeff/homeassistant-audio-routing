//! The pump between the microphone and the estimator.
//!
//! [`Feeder`] pulls analysis windows off the [`MicFeed`], hands each to the gate,
//! and reports what came back — including the two cases that are not measurements:
//! the mic went away, or the caller cancelled. Small on purpose; it exists so the
//! run's own code is not interleaved with window bookkeeping.

use super::*;

// ---------------------------------------------------------------- feeder

/// Pulls the mic ring into the estimator, contiguously, and keeps the per-period
/// peaks the gate needs.
///
/// Positioned at the *head* of the capture when it is armed: everything older
/// belongs to a state the run has already left behind (a different solo, a
/// pre-write delay), and feeding it would put the disturbance inside the window
/// the gate is about to judge.
pub(crate) struct Feeder<'m> {
    mic: &'m dyn MicFeed,
    rate: u32,
    period_frames: u64,
    /// Next frame index to read.
    next: u64,
    /// First pattern period fully inside the window.
    first_period: u64,
    /// Complete periods pulled so far.
    completed: u64,
    peaks: HashMap<u64, f32>,
    /// Ingest counters at arm time, so "did it happen inside *this* window" is a
    /// comparison rather than a guess.
    base_gaps: u64,
    base_clips: u64,
    seen_gaps: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Pulled {
    pub(crate) disconnected: bool,
    pub(crate) reconnected: bool,
    pub(crate) gap: bool,
    pub(crate) clipped: bool,
    pub(crate) new_periods: u64,
}

impl<'m> Feeder<'m> {
    pub(crate) fn new(mic: &'m dyn MicFeed, rate: u32, pattern_ms: f64) -> Self {
        let period_frames = (pattern_ms / 1000.0 * f64::from(rate)).round() as u64;
        let mut f = Feeder {
            mic,
            rate,
            period_frames: period_frames.max(1),
            next: 0,
            first_period: 0,
            completed: 0,
            peaks: HashMap::new(),
            base_gaps: 0,
            base_clips: 0,
            seen_gaps: 0,
        };
        f.arm();
        f
    }

    /// Drop everything buffered and start a fresh window at the capture head.
    pub(crate) fn arm(&mut self) {
        let st = self.mic.status();
        self.next = st.frames_received;
        // The period the head sits inside is already partly gone, so the first
        // period this window can *complete* is the next one.
        self.first_period = self.next / self.period_frames + 1;
        self.completed = 0;
        self.peaks.clear();
        self.base_gaps = st.gap_count;
        self.base_clips = st.clip_count;
        self.seen_gaps = st.gap_count;
    }

    pub(crate) fn pull(&mut self, est: &mut Estimator) -> Pulled {
        let mut out = Pulled::default();
        let st = self.mic.status();
        if !st.connected {
            out.disconnected = true;
            return out;
        }
        if st.sample_rate != self.rate || st.frames_received < self.next {
            out.reconnected = true;
            return out;
        }
        // Both counters are per-capture and monotonic, so an increase since the
        // window was armed means it happened inside the window. Sticky until the
        // window is re-armed, which matches the estimator's own latched verdict.
        out.gap = st.gap_count > self.base_gaps;
        out.clipped = st.clip_count > self.base_clips;
        if st.gap_count > self.seen_gaps {
            est.note_gap();
            self.seen_gaps = st.gap_count;
        }

        let mut avail = st.frames_received - self.next;
        if avail > st.capacity_frames as u64 {
            // The run fell so far behind that the ring recycled: the missing audio
            // is unrecoverable, so treat it as a gap and re-arm rather than
            // stitching two non-adjacent stretches together.
            est.note_gap();
            out.gap = true;
            self.arm();
            return out;
        }
        while avail > 0 {
            let len = avail.min(CHUNK_FRAMES as u64) as usize;
            let Some(w) = self.mic.window_from(self.next, len) else {
                // Overwritten between the status read and the window read.
                est.note_gap();
                out.gap = true;
                self.arm();
                return out;
            };
            for (i, s) in w.samples.iter().enumerate() {
                let p = (self.next + i as u64) / self.period_frames;
                let e = self.peaks.entry(p).or_insert(0.0);
                *e = e.max(s.abs());
            }
            est.push_block(w.first_frame, &w.samples);
            self.next += len as u64;
            avail -= len as u64;
        }
        let completed = (self.next / self.period_frames).saturating_sub(self.first_period);
        out.new_periods = completed.saturating_sub(self.completed);
        self.completed = completed;
        out
    }

    /// Peak of the most recently completed period.
    pub(crate) fn last_peak(&mut self) -> f32 {
        let p = self.first_period + self.completed.saturating_sub(1);
        let peak = self.peaks.get(&p).copied().unwrap_or(0.0);
        self.peaks.retain(|k, _| *k >= p);
        peak
    }

    /// Pattern-period index at the centre of the accumulated window — the same
    /// origin convention the estimator reports its intercept on.
    pub(crate) fn period_centre(&self) -> f64 {
        self.first_period as f64 + (self.completed.max(1) - 1) as f64 / 2.0
    }
}
