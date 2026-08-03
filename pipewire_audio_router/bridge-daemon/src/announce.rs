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
//
// One exception: a pw-sink target is a whole *host*, which may be playing music of
// its own that isn't in our stream at all. Our overlay duck cannot reach that, so
// start/stop on a `pwsink-dev-*` output is mirrored to that host's agent, which
// attenuates the foreign streams on its sink (pwsink_agent::duck_output —
// docs/receiver-agent-plan.md §11 P3).

use crate::announce_arbiter::{Action, Admission, AnnounceScheduler, AnnouncementId, Effects, OnBusy, Request};
use crate::overlay_mixer::OverlayMixer;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Monotonic milliseconds since first use (for the scheduler's time base).
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

struct Clip {
    pcm: Arc<Vec<u8>>,
    duck: f32,
    /// Stall grace handed to the mixer on every (re)start of this clip — longer
    /// when a target's transport is being opened on demand (see
    /// `overlay_mixer::OVERLAY_ONDEMAND_GRACE`). Kept on the clip so a queued or
    /// barge-preempted announcement gets the same grace when it finally starts.
    grace: Duration,
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
                        mixer.start_with_grace(&output, id, (*clip.pcm).clone(), clip.duck, clip.grace);
                        // No-op unless `output` is an agent-backed host.
                        crate::pwsink_agent::duck_output(&output, clip.duck);
                    }
                }
                Action::StopAnnouncement(output, _id) => {
                    mixer.stop(&output);
                    crate::pwsink_agent::unduck_output(&output);
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
    ///
    /// `grace` is how long the clip may sit unconsumed on an output before the
    /// stall watchdog releases it — pass `overlay_mixer::OVERLAY_ONDEMAND_GRACE`
    /// when a target's sender is still being connected (an unrouted AP2 receiver).
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
        grace: Duration,
    ) -> Admission {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.clips.insert(id, Clip { pcm: Arc::new(pcm), duck, grace });
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

    /// Periodic tick: complete finished overlays (start next queued / end duck),
    /// release overlays nothing is consuming, and expire stale queued
    /// announcements. Driven from main.rs.
    pub fn poll(&self) {
        let now = now_ms();
        let mixer = OverlayMixer::global();
        let mut done = mixer.take_finished();
        // Overlays no sender consumed (an output with no live transport, or one
        // whose sender died mid-clip). Treated exactly like a finish so the
        // scheduler frees the output — otherwise it stays occupied by a clip that
        // can never end, and every later announcement to it queues forever.
        for (output, id) in mixer.reap_stalled() {
            tracing::warn!(
                "announce {id} on '{output}': nothing consumed its audio (no per-device sender streaming this output); releasing it"
            );
            done.push((output, id));
        }
        let mut inner = self.inner.lock().unwrap();
        let mut seen = HashSet::new();
        for (_output, id) in done {
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
