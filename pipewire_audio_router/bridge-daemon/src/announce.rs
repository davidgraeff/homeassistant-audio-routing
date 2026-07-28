// ABOUTME: Announce coordinator — ties the AnnounceScheduler (concurrency policy:
// ABOUTME: queue / barge-in / TTL) to the OverlayMixer (per-device duck+overlay),
// ABOUTME: turning a decoded clip + target outputs into scheduled per-speaker
// ABOUTME: announcements. This is the AG delivery path for sendspin devices on
// ABOUTME: per-device senders.
//
// Flow: `announce()` decodes are done by the caller (api.rs) — it hands us
// 48k/S16/stereo PCM + targets + policy. We assign an id, ask the scheduler to
// admit it, and translate the scheduler's per-output actions into OverlayMixer
// start/stop calls. A periodic `poll()` (driven from main.rs) feeds finished
// overlays back to `scheduler.complete` (starting the next queued clip / ending
// the duck) and ticks TTLs.
//
// Ducking is implicit in the overlay mix for sendspin per-device, so the
// scheduler's Duck/Unduck actions are no-ops here (RAOP per-output duck is O-E).

use crate::announce_arbiter::{Action, Admission, AnnounceScheduler, AnnouncementId, Effects, OnBusy, Request};
use crate::overlay_mixer::OverlayMixer;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Monotonic milliseconds since first use (for the scheduler's time base).
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

struct Clip {
    pcm: Arc<Vec<u8>>,
    duck: f32,
}

struct Inner {
    sched: AnnounceScheduler,
    /// Clip PCM per in-flight announcement, kept so a queued or barge-preempted
    /// clip can (re)start whole later.
    clips: HashMap<AnnouncementId, Clip>,
    next_id: u64,
}

impl Inner {
    /// Translate scheduler effects into OverlayMixer calls.
    fn apply(&mut self, effects: Effects) {
        let mixer = OverlayMixer::global();
        for action in effects.actions {
            match action {
                Action::StartAnnouncement(output, id) => {
                    if let Some(clip) = self.clips.get(&id) {
                        mixer.start(&output, id, (*clip.pcm).clone(), clip.duck);
                    }
                }
                Action::StopAnnouncement(output, _id) => {
                    mixer.stop(&output);
                }
                // Duck is implicit in the mix for sendspin per-device.
                Action::DuckMusic(_) | Action::UnduckMusic(_) => {}
            }
        }
        for (id, _reason) in effects.dropped {
            self.clips.remove(&id);
        }
    }
}

/// Process-global coordinator (announcements are addressed by output name across
/// whichever per-device server owns each device).
pub struct AnnounceCoordinator {
    inner: Mutex<Inner>,
}

impl AnnounceCoordinator {
    pub fn global() -> &'static Self {
        static C: OnceLock<AnnounceCoordinator> = OnceLock::new();
        C.get_or_init(|| AnnounceCoordinator {
            inner: Mutex::new(Inner { sched: AnnounceScheduler::new(), clips: HashMap::new(), next_id: 1 }),
        })
    }

    /// Admit an announcement (clip already decoded to 48k/S16/stereo). Returns
    /// the scheduler's admission (`Playing` / `Queued{pos}` / `Rejected`).
    #[allow(clippy::too_many_arguments)]
    pub fn announce(
        &self,
        targets: Vec<String>,
        pcm: Vec<u8>,
        duck: f32,
        priority: i32,
        on_busy: OnBusy,
        barge_in: bool,
        ttl_ms: Option<u64>,
    ) -> Admission {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.clips.insert(id, Clip { pcm: Arc::new(pcm), duck });
        let (admission, effects) =
            inner.sched.begin(Request { id, priority, targets, on_busy, barge_in, ttl_ms }, now);
        inner.apply(effects);
        if matches!(admission, Admission::Rejected(_)) {
            inner.clips.remove(&id);
        }
        admission
    }

    /// Outputs with an announcement playing or queued — see
    /// [`AnnounceScheduler::outputs_in_flight`]. Read by the on-demand transport
    /// lease (sync_group.rs) so a session isn't handed back before its clip's turn.
    pub fn outputs_in_flight(&self) -> std::collections::BTreeSet<String> {
        self.inner.lock().unwrap().sched.outputs_in_flight()
    }

    /// Periodic tick: complete finished overlays (start next queued / end duck)
    /// and expire stale queued announcements. Driven from main.rs.
    pub fn poll(&self) {
        let now = now_ms();
        let finished = OverlayMixer::global().take_finished();
        let mut inner = self.inner.lock().unwrap();
        let mut seen = HashSet::new();
        for (_output, id) in finished {
            // A multi-output announcement reports finished on each output; one
            // complete() per id.
            if seen.insert(id) {
                let eff = inner.sched.complete(id, now);
                inner.apply(eff);
                inner.clips.remove(&id);
            }
        }
        let eff = inner.sched.tick(now);
        inner.apply(eff);
    }
}
