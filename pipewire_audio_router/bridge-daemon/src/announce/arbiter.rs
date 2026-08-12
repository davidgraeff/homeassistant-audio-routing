// ABOUTME: Announcement-group scheduler — the *pure* decision logic for how
// ABOUTME: announcements share outputs: queue-by-default, per-announcement
// ABOUTME: barge-in and TTL, decoupled from the audio path so it's unit-tested
// ABOUTME: ahead of O-B/O-A/O-E integration.
//
// Decisions this encodes (from the two-tier MG/AG discussion):
//   * Announcements are **atomic clips** (the daemon holds the WAV bytes) and
//     always play whole, from the start, or not at all — never "resume remaining
//     parts". The router may therefore queue a clip and play it later.
//   * **Busy policy (default queue, reject opt-in):** if an announcement's target
//     outputs are busy, it is *queued* by default (played when they free, ordered
//     by priority then arrival). `on_busy = Reject` instead returns a busy
//     admission so the producer (Home Assistant) can reschedule.
//   * **Urgency (`barge_in`, default off):** a normal announcement queue-jumps
//     (waits for the current clip to finish). A `barge_in` announcement preempts
//     the overlapping clip immediately; the preempted clip is **re-queued whole**
//     (replayed from the start later) if still within its TTL, else dropped.
//   * **TTL:** `None` = wait indefinitely; `Some(0)` = play-now-or-drop;
//     `Some(ms)` = drop if it can't start within ms (a stale "timer done" played
//     a minute late is worse than not played).
//   * **Concurrency:** announcements with disjoint target sets play at once. An
//     announcement occupies its target outputs exclusively; a multi-output
//     announcement waits until *all* its targets are free, then plays on all.
//   * **Ducking** is reference-counted by occupancy: an output ducks its music
//     while an announcement occupies it, and un-ducks when none does.
//   * **Reservations (plan §12.3):** something that is *not* an announcement can
//     claim outputs exclusively — today an alignment session's temporary group
//     (align/group.rs). A reservation counts as occupancy for admission, so an
//     ordinary announcement queues (or is rejected per `OnBusy`) instead of
//     playing over a calibration tone. It deliberately does **not** stop a
//     `barge_in`: nobody wants a fire alarm suppressed by a calibration, so the
//     admission order stays `if !overlaps { start } else if barge_in { preempt }
//     else { on_busy }` and the reservation *holder is told* instead
//     ([`ReservationHit`]), which is what lets it discard the affected member's
//     measurement with a reason naming the cause.
//
// The scheduler is pure and time is injected (`now_ms`) so it's deterministic in
// tests; the daemon passes real milliseconds. Wiring the emitted [`Action`]s to
// real per-output ducking + overlay is the audio path's job (sendspin: O-A/O-B;
// AP2 does it in-process), which lands after O-B.

#![allow(dead_code)] // wired into the audio path after O-B; unit-tested now.

use std::collections::{BTreeSet, HashMap};

/// Identifies one in-flight announcement (one play of a clip/TTS to an AG's
/// targets). The same id fans across every target output.
pub type AnnouncementId = u64;

/// An output's stable node name (e.g. `ap2-dev-dusche`, `sendspin-dev-…`).
pub type Output = String;

/// Identifies one **reservation**: an exclusive claim on a set of outputs held by
/// something that is not an announcement (today an alignment session's temporary
/// group — see `align/group.rs`).
pub type ReservationId = u64;

/// What to do when the requested outputs are busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnBusy {
    /// Hold the clip and play it when the outputs free (default).
    Queue,
    /// Don't hold it; report busy so the producer can reschedule.
    Reject,
}

/// A request to play one announcement to a set of outputs.
#[derive(Debug, Clone)]
pub struct Request {
    pub id: AnnouncementId,
    /// Higher wins: orders the queue and decides barge-in eligibility.
    pub priority: i32,
    /// The announcement group's target outputs (played together).
    pub targets: Vec<Output>,
    pub on_busy: OnBusy,
    /// Preempt an overlapping clip immediately (preempted one replayed whole).
    pub barge_in: bool,
    /// Max wait before it must start: `None` = forever, `Some(0)` =
    /// play-now-or-drop, `Some(ms)` = drop if not started within ms.
    pub ttl_ms: Option<u64>,
}

/// A side effect the audio path must carry out on one output. The scheduler is
/// pure — it only decides these; ducking + per-output overlay is the audio path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Begin ducking this output's music (it just became occupied).
    DuckMusic(Output),
    /// Restore this output's music (no announcement occupies it anymore).
    UnduckMusic(Output),
    /// Start playing this announcement's clip (overlay) on this output.
    StartAnnouncement(Output, AnnouncementId),
    /// Stop this announcement's clip on this output (completed or preempted).
    StopAnnouncement(Output, AnnouncementId),
}

/// The result of admitting a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Started now on all its targets.
    Playing,
    /// Held; will play when its targets free. `position` is 0-based in the queue.
    Queued { position: usize },
    /// Not admitted.
    Rejected(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Targets busy and `on_busy = Reject`.
    Busy,
    /// Targets busy, `on_busy = Queue`, but `ttl = Some(0)` (play-now-or-drop).
    Stale,
    /// The queue is at capacity.
    QueueFull,
}

/// Why a previously-accepted announcement was dropped (for logging).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Its TTL elapsed before it could (re)start.
    Stale,
}

/// A barge-in announcement that took over outputs someone had **reserved**.
///
/// The load-bearing half of the reservation feature (plan §12.3): the reservation
/// is not what stops the announcement — the *report* is what stops the holder from
/// trusting audio it no longer controls. Without it an alignment run sees the
/// doorbell as unstable amplitude and tells the user to hold the phone still.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationHit {
    pub reservation: ReservationId,
    /// The reserved outputs the announcement is taking over (sorted, unique).
    pub outputs: Vec<Output>,
    /// The announcement that won.
    pub by: AnnouncementId,
}

/// Side effects plus any drops, from an operation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Effects {
    /// Ordered actions to apply (stable order: by output, stop→duck→start→unduck).
    pub actions: Vec<Action>,
    /// Announcements removed without playing/finishing (e.g. TTL), for logging.
    pub dropped: Vec<(AnnouncementId, DropReason)>,
    /// Reservations a barge-in just played over — the holder must be told (the
    /// caller forwards these; see `announce/mod.rs`).
    pub reservations_hit: Vec<ReservationHit>,
}

/// An exclusive claim on outputs by a non-announcement holder (see
/// [`ReservationId`]). Counts as occupancy for admission, emits no actions of its
/// own — the holder is already producing whatever those outputs play.
#[derive(Debug, Clone)]
struct Reservation {
    id: ReservationId,
    outputs: BTreeSet<Output>,
}

#[derive(Debug, Clone)]
struct Entry {
    req: Request,
    /// Absolute deadline (ms) by which it must start, or `None` for no limit.
    deadline: Option<u64>,
    /// Arrival order tiebreaker (stable across re-queue on preemption).
    seq: u64,
}

/// The announcement scheduler. One per daemon; the caller drives it with real
/// millisecond timestamps and applies the returned [`Action`]s.
#[derive(Debug)]
pub struct AnnounceScheduler {
    active: Vec<Entry>,
    queue: Vec<Entry>,
    /// Non-announcement exclusive claims (plan §12.3). Kept separate from
    /// `active` so no action-diffing path can mistake one for a clip: a
    /// reservation has no id in [`occupancy`](Self::occupancy), so it never emits
    /// duck/start/stop.
    reservations: Vec<Reservation>,
    max_queue: usize,
    next_seq: u64,
}

impl Default for AnnounceScheduler {
    fn default() -> Self {
        Self::with_max_queue(16)
    }
}

impl AnnounceScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_queue(max_queue: usize) -> Self {
        Self { active: Vec::new(), queue: Vec::new(), reservations: Vec::new(), max_queue, next_seq: 0 }
    }

    /// Claim `outputs` exclusively for `id` (plan §12.3). Replaces any previous
    /// claim under the same id, so re-reserving is how a holder changes its set.
    /// An empty set releases it.
    ///
    /// Emits nothing: a reservation is not a clip, so there is no duck to start —
    /// the holder is already producing what those outputs play. What it changes is
    /// *admission*: while held, an ordinary announcement to one of these outputs
    /// queues (or is rejected per `OnBusy`), and a queued one cannot start.
    pub fn reserve(&mut self, id: ReservationId, outputs: impl IntoIterator<Item = Output>) {
        let outputs: BTreeSet<Output> = outputs.into_iter().collect();
        self.reservations.retain(|r| r.id != id);
        if !outputs.is_empty() {
            self.reservations.push(Reservation { id, outputs });
        }
    }

    /// Drop `id`'s claim and let anything that was waiting on those outputs start.
    /// Idempotent — releasing an unknown id just settles.
    pub fn release_reservation(&mut self, id: ReservationId, now_ms: u64) -> Effects {
        let before = self.occupancy();
        let mut dropped = Vec::new();
        self.reservations.retain(|r| r.id != id);
        self.settle(now_ms, &mut dropped);
        let after = self.occupancy();
        Effects { actions: diff_actions(&before, &after), dropped, reservations_hit: Vec::new() }
    }

    /// Every currently reserved output (sorted).
    pub fn reserved_outputs(&self) -> BTreeSet<Output> {
        self.reservations.iter().flat_map(|r| r.outputs.iter().cloned()).collect()
    }

    /// Is `output` claimed by any reservation?
    pub fn is_reserved(&self, output: &str) -> bool {
        self.reservations.iter().any(|r| r.outputs.contains(output))
    }

    /// Every output an announcement is currently playing on **or waiting for**.
    /// The audio path keeps a transport it opened on demand alive for these: a
    /// queued clip has no overlay slot yet, so the mixer alone can't tell that the
    /// output still has something coming.
    ///
    /// Reservations are deliberately **not** included: a reservation is not a clip
    /// waiting for a transport — its holder owns the output's audio path already
    /// (an alignment hold is routed into its own group), so listing it here would
    /// keep an on-demand announce session leased for nothing.
    pub fn outputs_in_flight(&self) -> BTreeSet<Output> {
        self.active.iter().chain(self.queue.iter()).flat_map(|e| e.req.targets.iter().cloned()).collect()
    }

    /// Outputs currently occupied by a playing announcement → its id.
    fn occupancy(&self) -> HashMap<Output, AnnouncementId> {
        let mut m = HashMap::new();
        for e in &self.active {
            for o in &e.req.targets {
                m.insert(o.clone(), e.req.id);
            }
        }
        m
    }

    fn deadline_for(req: &Request, now_ms: u64) -> Option<u64> {
        req.ttl_ms.map(|t| now_ms.saturating_add(t))
    }

    /// Admit a request. Returns its admission and the side effects (including any
    /// clips that started/were preempted as a result, and TTL drops).
    pub fn begin(&mut self, req: Request, now_ms: u64) -> (Admission, Effects) {
        let before = self.occupancy();
        let mut dropped = Vec::new();
        let req_id = req.id;

        // "Busy" is announcement occupancy **plus** reservations (plan §12.3) — an
        // aligning output used to be invisible here, which is exactly why an
        // announcement took the immediate-start path straight over a calibration.
        let overlaps = req.targets.iter().any(|t| before.contains_key(t) || self.is_reserved(t));
        let mut reservations_hit = Vec::new();

        enum Path {
            Started,
            Queued,
            Rejected(RejectReason),
        }

        let path = if !overlaps {
            // All targets free → play now.
            self.push_active(req, now_ms);
            Path::Started
        } else if req.barge_in {
            // A barge-in wins over a reservation too — it just has to say so, so the
            // holder can throw away whatever it was measuring on those outputs.
            reservations_hit = self.hits_for(&req);
            // Preempt every announcement overlapping our targets; re-queue each
            // whole (if still within TTL) so it replays from the start later.
            let victims: Vec<AnnouncementId> =
                self.active.iter().filter(|e| e.req.targets.iter().any(|o| req.targets.contains(o))).map(|e| e.req.id).collect();
            for vid in victims {
                if let Some(entry) = self.remove_active(vid) {
                    if entry.deadline.is_some_and(|d| now_ms >= d) {
                        dropped.push((entry.req.id, DropReason::Stale));
                    } else {
                        self.requeue(entry);
                    }
                }
            }
            self.push_active(req, now_ms);
            Path::Started
        } else {
            match req.on_busy {
                OnBusy::Reject => Path::Rejected(RejectReason::Busy),
                OnBusy::Queue if req.ttl_ms == Some(0) => Path::Rejected(RejectReason::Stale),
                OnBusy::Queue if self.queue.len() >= self.max_queue => Path::Rejected(RejectReason::QueueFull),
                OnBusy::Queue => {
                    let deadline = Self::deadline_for(&req, now_ms);
                    let seq = self.take_seq();
                    self.enqueue(Entry { req, deadline, seq });
                    Path::Queued
                }
            }
        };

        // Starting/preempting can free outputs; let eligible queued clips start.
        self.settle(now_ms, &mut dropped);
        let after = self.occupancy();
        let actions = diff_actions(&before, &after);

        let admission = match path {
            Path::Started => Admission::Playing,
            Path::Rejected(r) => Admission::Rejected(r),
            Path::Queued => {
                if self.is_active(req_id) {
                    // settle started it (a same-op preemption freed its targets).
                    Admission::Playing
                } else {
                    let position = self.queue.iter().position(|e| e.req.id == req_id).unwrap_or(0);
                    Admission::Queued { position }
                }
            }
        };

        (admission, Effects { actions, dropped, reservations_hit })
    }

    /// A playing announcement finished (its clip ended) or was cancelled.
    pub fn complete(&mut self, id: AnnouncementId, now_ms: u64) -> Effects {
        let before = self.occupancy();
        let mut dropped = Vec::new();
        self.remove_active(id);
        self.settle(now_ms, &mut dropped);
        let after = self.occupancy();
        Effects { actions: diff_actions(&before, &after), dropped, reservations_hit: Vec::new() }
    }

    /// Advance time: drop stale queued announcements and start any now-eligible
    /// ones. Call periodically (or when a TTL might have elapsed).
    pub fn tick(&mut self, now_ms: u64) -> Effects {
        let before = self.occupancy();
        let mut dropped = Vec::new();
        self.settle(now_ms, &mut dropped);
        let after = self.occupancy();
        Effects { actions: diff_actions(&before, &after), dropped, reservations_hit: Vec::new() }
    }

    /// Outputs currently ducked (an announcement occupies them).
    pub fn ducked_outputs(&self) -> Vec<Output> {
        let mut v: Vec<Output> = self.occupancy().into_keys().collect();
        v.sort();
        v
    }

    /// Is `id` currently playing?
    pub fn is_active(&self, id: AnnouncementId) -> bool {
        self.active.iter().any(|e| e.req.id == id)
    }

    // --- internals (mutate active/queue only; never emit actions) ---

    /// Which reservations `req`'s targets overlap, as the report the holder gets.
    fn hits_for(&self, req: &Request) -> Vec<ReservationHit> {
        self.reservations
            .iter()
            .filter_map(|r| {
                let outputs: BTreeSet<Output> = req.targets.iter().filter(|t| r.outputs.contains(*t)).cloned().collect();
                (!outputs.is_empty()).then(|| ReservationHit { reservation: r.id, outputs: outputs.into_iter().collect(), by: req.id })
            })
            .collect()
    }

    fn take_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    fn push_active(&mut self, req: Request, now_ms: u64) {
        let deadline = Self::deadline_for(&req, now_ms);
        let seq = self.take_seq();
        self.active.push(Entry { req, deadline, seq });
    }

    fn remove_active(&mut self, id: AnnouncementId) -> Option<Entry> {
        if let Some(i) = self.active.iter().position(|e| e.req.id == id) {
            Some(self.active.remove(i))
        } else {
            None
        }
    }

    fn enqueue(&mut self, entry: Entry) {
        self.queue.push(entry);
        self.sort_queue();
    }

    /// Re-queue a preempted entry, preserving its original seq/deadline.
    fn requeue(&mut self, entry: Entry) {
        self.queue.push(entry);
        self.sort_queue();
    }

    /// Highest priority first, then earliest arrival (smallest seq).
    fn sort_queue(&mut self) {
        self.queue.sort_by(|a, b| b.req.priority.cmp(&a.req.priority).then(a.seq.cmp(&b.seq)));
    }

    /// Drop expired queued entries, then repeatedly start the best queued entry
    /// whose targets are all currently free, until none can start.
    fn settle(&mut self, now_ms: u64, dropped: &mut Vec<(AnnouncementId, DropReason)>) {
        // Drop stale (deadline passed) queued entries — they never played.
        self.queue.retain(|e| {
            if e.deadline.is_some_and(|d| now_ms >= d) {
                dropped.push((e.req.id, DropReason::Stale));
                false
            } else {
                true
            }
        });

        // Start eligible entries in priority/arrival order. Occupancy only grows
        // as we start clips, so a single ordered sweep with rechecks suffices.
        // Reservations are constant across the sweep, so they're read once — and a
        // reserved output blocks a queued start exactly like a playing clip does,
        // which is what makes "queue, don't play over the calibration" hold for a
        // clip admitted *before* the reservation existed.
        let reserved = self.reserved_outputs();
        loop {
            let occ = self.occupancy();
            let pick = self.queue.iter().position(|e| e.req.targets.iter().all(|o| !occ.contains_key(o) && !reserved.contains(o)));
            match pick {
                Some(i) => {
                    let entry = self.queue.remove(i);
                    self.active.push(entry);
                }
                None => break,
            }
        }
    }
}

/// Compute the ordered action list from an occupancy transition. For each output
/// (in stable, sorted order): a hand-over emits stop(old)+start(new); a newly
/// occupied output emits duck+start; a freed output emits stop+unduck.
fn diff_actions(before: &HashMap<Output, AnnouncementId>, after: &HashMap<Output, AnnouncementId>) -> Vec<Action> {
    let mut outputs: BTreeSet<&Output> = BTreeSet::new();
    outputs.extend(before.keys());
    outputs.extend(after.keys());

    let mut actions = Vec::new();
    for o in outputs {
        match (before.get(o), after.get(o)) {
            (None, Some(new)) => {
                actions.push(Action::DuckMusic(o.clone()));
                actions.push(Action::StartAnnouncement(o.clone(), *new));
            }
            (Some(old), None) => {
                actions.push(Action::StopAnnouncement(o.clone(), *old));
                actions.push(Action::UnduckMusic(o.clone()));
            }
            (Some(old), Some(new)) if old != new => {
                actions.push(Action::StopAnnouncement(o.clone(), *old));
                actions.push(Action::StartAnnouncement(o.clone(), *new));
            }
            _ => {}
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::*;

    fn req(id: AnnouncementId, priority: i32, targets: &[&str]) -> Request {
        Request {
            id,
            priority,
            targets: targets.iter().map(|s| s.to_string()).collect(),
            on_busy: OnBusy::Queue,
            barge_in: false,
            ttl_ms: None,
        }
    }
    fn o(s: &str) -> Output {
        s.to_string()
    }

    #[test]
    fn plays_immediately_when_free() {
        let mut s = AnnounceScheduler::new();
        let (adm, eff) = s.begin(req(1, 0, &["k"]), 0);
        assert_eq!(adm, Admission::Playing);
        assert_eq!(eff.actions, vec![DuckMusic(o("k")), StartAnnouncement(o("k"), 1)]);
        let eff = s.complete(1, 100);
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 1), UnduckMusic(o("k"))]);
    }

    #[test]
    fn disjoint_announcements_play_concurrently() {
        let mut s = AnnounceScheduler::new();
        assert_eq!(s.begin(req(1, 0, &["k"]), 0).0, Admission::Playing);
        // Different output → not busy → plays now, doesn't disturb #1.
        let (adm, eff) = s.begin(req(2, 0, &["bed"]), 1);
        assert_eq!(adm, Admission::Playing);
        assert_eq!(eff.actions, vec![DuckMusic(o("bed")), StartAnnouncement(o("bed"), 2)]);
        assert!(s.is_active(1) && s.is_active(2));
    }

    #[test]
    fn busy_queues_by_default_then_plays_on_completion() {
        let mut s = AnnounceScheduler::new();
        s.begin(req(1, 0, &["k"]), 0);
        let (adm, eff) = s.begin(req(2, 0, &["k"]), 10);
        assert_eq!(adm, Admission::Queued { position: 0 });
        assert!(eff.actions.is_empty(), "queued clip produces no actions yet");
        // #1 ends → #2 takes the output (hand-over: stop 1, start 2; stays ducked).
        let eff = s.complete(1, 100);
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 1), StartAnnouncement(o("k"), 2)]);
        assert!(s.is_active(2));
    }

    #[test]
    fn reject_opt_in_returns_busy_and_does_not_queue() {
        let mut s = AnnounceScheduler::new();
        s.begin(req(1, 0, &["k"]), 0);
        let mut r = req(2, 5, &["k"]);
        r.on_busy = OnBusy::Reject;
        let (adm, eff) = s.begin(r, 10);
        assert_eq!(adm, Admission::Rejected(RejectReason::Busy));
        assert!(eff.actions.is_empty());
        // The rejected one is not queued: completing #1 just un-ducks.
        let eff = s.complete(1, 20);
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 1), UnduckMusic(o("k"))]);
    }

    #[test]
    fn queue_orders_by_priority_then_arrival() {
        let mut s = AnnounceScheduler::new();
        s.begin(req(1, 0, &["k"]), 0); // playing
        s.begin(req(2, 0, &["k"]), 1); // queued (low)
        s.begin(req(3, 10, &["k"]), 2); // queued (high) → should play first
                                        // #1 done → highest-priority queued (#3) plays next.
        let eff = s.complete(1, 100);
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 1), StartAnnouncement(o("k"), 3)]);
        // #3 done → then the lower one (#2).
        let eff = s.complete(3, 200);
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 3), StartAnnouncement(o("k"), 2)]);
    }

    #[test]
    fn barge_in_preempts_and_replays_the_preempted_whole() {
        let mut s = AnnounceScheduler::new();
        s.begin(req(1, 0, &["k"]), 0); // reminder playing
        let mut door = req(2, 100, &["k"]);
        door.barge_in = true;
        let (adm, eff) = s.begin(door, 10); // doorbell barges in
        assert_eq!(adm, Admission::Playing);
        // Hand-over on k: stop the reminder, start the doorbell (stays ducked).
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 1), StartAnnouncement(o("k"), 2)]);
        // Doorbell ends → the preempted reminder replays WHOLE (from start).
        let eff = s.complete(2, 50);
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 2), StartAnnouncement(o("k"), 1)]);
        assert!(s.is_active(1));
    }

    #[test]
    fn stale_queued_announcement_is_dropped_not_played_late() {
        let mut s = AnnounceScheduler::new();
        s.begin(req(1, 0, &["k"]), 0); // playing, no ttl
        let mut timer = req(2, 0, &["k"]);
        timer.ttl_ms = Some(30); // must start within 30ms
        assert_eq!(s.begin(timer, 10).0, Admission::Queued { position: 0 });
        // #1 finishes only at t=100, past #2's deadline (10+30=40) → #2 dropped.
        let eff = s.complete(1, 100);
        assert_eq!(eff.actions, vec![StopAnnouncement(o("k"), 1), UnduckMusic(o("k"))]);
        assert_eq!(eff.dropped, vec![(2, DropReason::Stale)]);
        assert!(!s.is_active(2));
    }

    #[test]
    fn play_now_or_drop_ttl_zero_rejects_when_busy() {
        let mut s = AnnounceScheduler::new();
        s.begin(req(1, 0, &["k"]), 0);
        let mut r = req(2, 0, &["k"]);
        r.ttl_ms = Some(0);
        assert_eq!(s.begin(r, 10).0, Admission::Rejected(RejectReason::Stale));
    }

    #[test]
    fn multi_output_waits_for_all_targets_free() {
        let mut s = AnnounceScheduler::new();
        s.begin(req(1, 0, &["k"]), 0); // occupies kitchen
                                       // #2 targets kitchen+bedroom; kitchen busy → must wait (not start on bedroom).
        assert_eq!(s.begin(req(2, 0, &["k", "bed"]), 5).0, Admission::Queued { position: 0 });
        assert!(!s.is_active(2), "must not start on the free output alone");
        // kitchen frees → #2 now plays on BOTH outputs together. Actions are in
        // stable output order ("bed" < "k"): bedroom newly occupied (duck+start),
        // kitchen handed over from #1 to #2 (stop+start).
        let eff = s.complete(1, 100);
        assert_eq!(
            eff.actions,
            vec![DuckMusic(o("bed")), StartAnnouncement(o("bed"), 2), StopAnnouncement(o("k"), 1), StartAnnouncement(o("k"), 2),]
        );
        assert!(s.is_active(2));
    }

    // --- reservations (plan §12.3: an alignment hold) ---

    /// The bug §12.3 records: occupancy came only from in-flight announcements, so
    /// an aligning output was invisible and `begin` took the immediate-start path.
    #[test]
    fn a_reservation_makes_an_ordinary_announcement_queue() {
        let mut s = AnnounceScheduler::new();
        s.reserve(7, [o("k")]);
        assert!(s.is_reserved("k"));
        let (adm, eff) = s.begin(req(1, 0, &["k"]), 0);
        assert_eq!(adm, Admission::Queued { position: 0 });
        assert!(eff.actions.is_empty(), "nothing may play over the calibration tone");
        assert!(eff.reservations_hit.is_empty(), "a queued clip did not touch the reservation");
        // A tick while the hold stands must not let it slip through.
        assert!(s.tick(10).actions.is_empty());
        assert!(!s.is_active(1));
        // Releasing the hold is what starts it.
        let eff = s.release_reservation(7, 20);
        assert_eq!(eff.actions, vec![DuckMusic(o("k")), StartAnnouncement(o("k"), 1)]);
        assert!(s.is_active(1));
    }

    #[test]
    fn a_reservation_rejects_when_the_producer_asked_for_reject() {
        let mut s = AnnounceScheduler::new();
        s.reserve(7, [o("k")]);
        let mut r = req(1, 0, &["k"]);
        r.on_busy = OnBusy::Reject;
        assert_eq!(s.begin(r, 0).0, Admission::Rejected(RejectReason::Busy));
        // ...and play-now-or-drop is stale, not queued, exactly as for a busy output.
        let mut r = req(2, 0, &["k"]);
        r.ttl_ms = Some(0);
        assert_eq!(s.begin(r, 0).0, Admission::Rejected(RejectReason::Stale));
    }

    #[test]
    fn a_reservation_leaves_other_outputs_alone() {
        let mut s = AnnounceScheduler::new();
        s.reserve(7, [o("k")]);
        let (adm, eff) = s.begin(req(1, 0, &["bed"]), 0);
        assert_eq!(adm, Admission::Playing, "an unreserved output is untouched by the hold");
        assert_eq!(eff.actions, vec![DuckMusic(o("bed")), StartAnnouncement(o("bed"), 1)]);
        // A multi-output clip that includes a reserved output waits for ALL of them
        // (the existing all-or-nothing rule), so it must not start on the free one.
        assert_eq!(s.begin(req(2, 0, &["k", "bed2"]), 1).0, Admission::Queued { position: 0 });
        assert!(!s.is_active(2));
    }

    /// The fire-alarm case: barge-in still wins, and the holder is told which of its
    /// members were touched and by what.
    #[test]
    fn barge_in_beats_a_reservation_and_reports_the_hit() {
        let mut s = AnnounceScheduler::new();
        s.reserve(7, [o("k"), o("hall")]);
        let mut alarm = req(9, 100, &["k"]);
        alarm.barge_in = true;
        let (adm, eff) = s.begin(alarm, 5);
        assert_eq!(adm, Admission::Playing, "an alarm is never suppressed by a calibration");
        assert_eq!(eff.actions, vec![DuckMusic(o("k")), StartAnnouncement(o("k"), 9)]);
        assert_eq!(eff.reservations_hit, vec![ReservationHit { reservation: 7, outputs: vec![o("k")], by: 9 }]);
        // The hold is NOT dissolved by the hit — only the affected member's
        // measurement is; the other member is still exclusive.
        assert!(s.is_reserved("k") && s.is_reserved("hall"));
        assert_eq!(s.begin(req(1, 0, &["hall"]), 6).0, Admission::Queued { position: 0 });
    }

    #[test]
    fn a_hit_names_every_reserved_output_the_barge_in_covers() {
        let mut s = AnnounceScheduler::new();
        s.reserve(7, [o("k"), o("hall")]);
        s.reserve(8, [o("bath")]);
        let mut alarm = req(9, 100, &["hall", "k", "bath", "bed"]);
        alarm.barge_in = true;
        let eff = s.begin(alarm, 0).1;
        assert_eq!(
            eff.reservations_hit,
            vec![
                ReservationHit { reservation: 7, outputs: vec![o("hall"), o("k")], by: 9 },
                ReservationHit { reservation: 8, outputs: vec![o("bath")], by: 9 },
            ],
            "one entry per reservation, its own outputs only, sorted"
        );
    }

    #[test]
    fn reserving_again_replaces_the_set_and_an_empty_set_releases() {
        let mut s = AnnounceScheduler::new();
        s.reserve(7, [o("k"), o("hall")]);
        s.reserve(7, [o("hall")]); // the wizard narrowed its selection
        assert!(!s.is_reserved("k") && s.is_reserved("hall"));
        assert_eq!(s.reserved_outputs().into_iter().collect::<Vec<_>>(), vec![o("hall")]);
        s.reserve(7, []);
        assert!(s.reserved_outputs().is_empty());
        // Releasing an id that never existed is a no-op, not a panic.
        assert!(s.release_reservation(999, 0).actions.is_empty());
    }

    #[test]
    fn barge_in_frees_a_non_shared_output_for_a_waiting_clip() {
        let mut s = AnnounceScheduler::new();
        // #1 occupies kitchen+hall.
        s.begin(req(1, 0, &["k", "hall"]), 0);
        // #2 (queued) wants hall only, waiting behind #1.
        assert_eq!(s.begin(req(2, 0, &["hall"]), 1).0, Admission::Queued { position: 0 });
        // Doorbell barges in on kitchen only. #1 is preempted (it overlaps kitchen),
        // freeing BOTH kitchen and hall; #2 can now start on hall while the
        // doorbell plays kitchen; #1 re-queued whole.
        let mut door = req(3, 100, &["k"]);
        door.barge_in = true;
        let (adm, _eff) = s.begin(door, 10);
        assert_eq!(adm, Admission::Playing);
        assert!(s.is_active(3), "doorbell playing on kitchen");
        assert!(s.is_active(2), "waiting hall clip started when #1 was preempted");
        assert!(!s.is_active(1), "#1 preempted");
    }
}
