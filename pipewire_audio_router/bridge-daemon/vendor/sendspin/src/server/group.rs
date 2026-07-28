// ABOUTME: Multi-client synchronized playback group
// ABOUTME: Every member receives identical audio bytes with identical timestamps, so each client's clock-sync offset alone yields sample-accurate multi-room sync

use crate::error::Error;
use crate::protocol::messages::{PlayerCommand, StreamPlayerConfig};
use crate::server::binary::{encode_audio_frame, AudioFrame};
use crate::server::connection::{AudioEnqueue, QueuedControl, ServerSender};
use crate::server::timeline::SharedTimeline;
use crate::sync::raw_clock::Clock;
use futures_util::future::join_all;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

/// Marker for a group that owns its timeline and may therefore re-anchor or clear
/// it. See [`Group`].
#[derive(Debug)]
pub struct OwnsTimeline;

/// Marker for a group that shares its timeline with others. See [`Group`].
///
/// Such a group deliberately has no `start_stream`/`end_stream`/`push_audio`:
/// re-anchoring or clearing a shared timeline from inside one group desyncs every
/// other group sharing it, and stamping it per group advances it once per group
/// instead of once per chunk. The coordinator that owns the timeline drives those;
/// a group only announces the stream to its own members via
/// [`Group::broadcast_stream_start`] / [`Group::broadcast_stream_end`].
#[derive(Debug)]
pub struct SharesTimeline;

/// A synchronized playback group.
///
/// The server-side synchronization trick is simple and doesn't require
/// knowing any client's individual clock: every member is sent the *same*
/// audio bytes tagged with the *same* `server/time`-domain timestamp. Each
/// client independently converts that timestamp into its own clock domain
/// (via the offset/drift it tracks from `client/time`/`server/time`
/// exchanges) and schedules local playback there — so two members with
/// converged clock-sync play the same chunk at the same wall-clock instant
/// without the server ever comparing their clocks to each other.
///
/// The timestamp stream lives in a [`SharedTimeline`]. A group created with
/// [`Group::new`] owns its own timeline (the classic single-group case: sync is
/// automatic). Several groups created with [`Group::with_timeline`] over one
/// `Arc<SharedTimeline>` share a single timeline — the building block for
/// per-device senders that must stay phase-locked while being addressed
/// independently (duck/overlay/route one member without the others). In that
/// mode the caller stamps the timeline **once** per chunk
/// ([`SharedTimeline::stamp`]) and delivers the result to each group via
/// [`Group::push_at`], instead of calling [`Group::push_audio`] per group
/// (which would advance the shared timeline once per group).
///
/// Which of those two modes a group is in is part of its **type**, so the mistake
/// cannot be made: `start_stream`, `end_stream`, `clear_stream` and `push_audio`
/// exist only on `Group<`[`OwnsTimeline`]`>`, because each of them mutates the
/// timeline and would desync every other group sharing it. A
/// `Group<`[`SharesTimeline`]`>` has [`Group::broadcast_stream_start`] /
/// [`Group::broadcast_stream_end`] / [`Group::push_at`] instead, and the
/// coordinator drives the timeline itself.
///
/// v1 scope: one shared PCM format for the whole group — no per-client
/// transcoding, so a member that can't take the group's format is a v1
/// limitation, not silently-wrong audio. No late-join catch-up (a client
/// added mid-stream just gets `stream/start` and audio from that point
/// forward) and no historical buffer replay.
#[derive(Debug)]
pub struct Group<Timeline = OwnsTimeline> {
    timeline: Arc<SharedTimeline>,
    /// The group's members — and, deliberately, its **ordering point**.
    ///
    /// Every frame this group queues for a member, control or audio, is queued
    /// while this lock is held: `stream/start`/`stream/end` in the lifecycle
    /// calls, audio in [`Group::push_at`]. Queueing is synchronous
    /// (`ServerSender::queue_*` / `enqueue_audio` never await), so holding one
    /// lock across it is enough to give every member the same, valid order — and
    /// no `await` ever happens while it's held, which a `std::sync::Mutex` makes
    /// hard to get wrong by accident.
    ///
    /// Lock order is `members` before the timeline's own lock, never the
    /// reverse.
    members: Mutex<HashMap<String, ServerSender>>,
    /// Which of the two method sets above this group has. Zero-sized.
    _timeline: PhantomData<Timeline>,
}

impl<Timeline> Group<Timeline> {
    /// The member map, recovering from a poisoned lock rather than propagating the
    /// panic.
    ///
    /// A panic while this lock is held would otherwise poison it for the life of
    /// the process, and the next push would panic on the caller's thread — which is
    /// typically a dedicated real-time audio thread nobody joins, so the output
    /// simply goes silent with nothing surfaced. The guarded state does not justify
    /// that: it is a `HashMap` of senders, and the worst a half-finished mutation
    /// can leave behind is a member that gets pruned on its next failed enqueue.
    fn members(&self) -> std::sync::MutexGuard<'_, HashMap<String, ServerSender>> {
        self.members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Group<OwnsTimeline> {
    /// Create an empty group that owns a fresh timeline in `clock`'s domain —
    /// pass the same clock the [`crate::server::ServerListener`] that accepted
    /// these connections was built with, so timestamps here are in the same
    /// domain as the `server/time` replies members already trust.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            timeline: Arc::new(SharedTimeline::new(clock)),
            members: Mutex::new(HashMap::new()),
            _timeline: PhantomData,
        }
    }

    /// Start (or restart, e.g. after a format change) the shared stream for
    /// every current member, and re-anchor the audio timeline.
    ///
    /// `stream/start` is queued for every member while the member lock is held,
    /// so audio pushed concurrently is ordered either wholly before it (and then
    /// discarded as belonging to the previous stream) or wholly after it. The
    /// client never observes audio *between* the re-anchor and the
    /// `stream/start`.
    ///
    /// Only exists on a timeline-owning group. For a shared timeline, call
    /// [`SharedTimeline::set_config`] once and [`Self::broadcast_stream_start`]
    /// per group — re-anchoring from inside one group would desync the rest.
    pub async fn start_stream(&self, config: StreamPlayerConfig) {
        let queued = {
            let members = self.members();
            self.timeline.set_config(config.clone());
            Self::queue_each(&members, |sender| sender.queue_stream_start(config.clone()))
        };
        self.settle(queued).await;
    }

    /// End the shared stream for every current member and reset the timeline.
    ///
    /// Ordered against concurrent pushes exactly as [`Self::start_stream`] is,
    /// but the other way round: audio pushed before `stream/end` was queued goes
    /// out *first*, since ending a stream means "after everything I sent" —
    /// overtaking it would truncate the tail. Audio pushed concurrently *after*
    /// lands after the `stream/end`, which is the caller's own race, not this
    /// method's.
    ///
    /// Only exists on a timeline-owning group. For a shared timeline, use
    /// [`Self::broadcast_stream_end`], plus [`SharedTimeline::clear_config`] once
    /// the *last* group is done — clearing it from inside one group would strand
    /// the others without a config.
    pub async fn end_stream(&self) {
        let queued = {
            let members = self.members();
            self.timeline.clear_config();
            Self::queue_each(&members, |sender| sender.queue_stream_end())
        };
        self.settle(queued).await;
    }

    /// Push one PCM chunk to every member: stamp the timeline once, then fan the
    /// identical frame out. Returns that timestamp. Enqueue is non-blocking, so
    /// one slow member never delays the others; a member whose connection has
    /// died is pruned.
    ///
    /// Only exists on a timeline-owning group — when several groups share one
    /// timeline, stamping per group would advance it once per group instead of
    /// once per chunk. Stamp it yourself and use [`Self::push_at`].
    pub fn push_audio(&self, pcm: &[u8]) -> i64 {
        self.push_audio_impl(pcm)
    }

    /// Ask every member to discard buffered-but-unplayed audio (e.g. after a
    /// seek) without ending the stream, and reset the timeline anchor to match.
    pub async fn clear_stream(&self) {
        self.timeline.reset();
        self.broadcast_stream_clear().await;
    }

    /// Override the default send-ahead lead time; rebuilds the timeline with the
    /// new lead. Only exists on a timeline-owning group — for a shared timeline,
    /// set the lead on the [`SharedTimeline`] itself before sharing it.
    pub fn with_send_ahead_us(self, send_ahead_us: i64) -> Self {
        let clock = self.timeline.clock();
        Self {
            timeline: Arc::new(SharedTimeline::new(clock).with_send_ahead_us(send_ahead_us)),
            members: self.members,
            _timeline: PhantomData,
        }
    }
}

impl Group<SharesTimeline> {
    /// Create an empty group that shares an existing [`SharedTimeline`] with
    /// other groups/senders. All groups sharing one timeline emit identical
    /// timestamps for the same chunk — see the type docs for the stamp-once
    /// contract, and [`SharesTimeline`] for which methods such a group has.
    pub fn with_timeline(timeline: Arc<SharedTimeline>) -> Self {
        Self {
            timeline,
            members: Mutex::new(HashMap::new()),
            _timeline: PhantomData,
        }
    }
}

impl<Timeline> Group<Timeline> {
    /// The timeline backing this group, so a caller can share it across
    /// per-device senders (`Group::with_timeline(group.timeline())`) and stamp
    /// it once per chunk.
    pub fn timeline(&self) -> Arc<SharedTimeline> {
        Arc::clone(&self.timeline)
    }

    /// Client IDs of every current member.
    pub fn member_ids(&self) -> Vec<String> {
        self.members().keys().cloned().collect()
    }

    /// Number of current members.
    pub fn len(&self) -> usize {
        self.members().len()
    }

    /// Whether the group has no members.
    pub fn is_empty(&self) -> bool {
        self.members().is_empty()
    }

    /// Add a member. If a stream is already active for this group, starts
    /// it for the new member too (matching the group's already-negotiated
    /// format) — but does not replay any audio already delivered to
    /// existing members (no late-join catch-up in v1, see the type docs).
    ///
    /// The new member's `stream/start` is queued *and* the member inserted under
    /// one lock, so a concurrent [`Self::push_at`] can't slip audio in
    /// ahead of it. If the `stream/start` write then fails, the member is
    /// removed again and the error returned.
    pub async fn add_member(
        &self,
        client_id: impl Into<String>,
        sender: ServerSender,
    ) -> Result<(), Error> {
        let client_id = client_id.into();
        let queued = {
            let mut members = self.members();
            let queued = self
                .timeline
                .config()
                .map(|cfg| sender.queue_stream_start(cfg));
            members.insert(client_id.clone(), sender);
            queued
        };
        if let Some(queued) = queued {
            if let Err(e) = queued.written().await {
                self.members().remove(&client_id);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Remove a member, if present. The caller is responsible for actually
    /// disconnecting it (e.g. via [`crate::server::ServerConnection::disconnect`]) —
    /// this only stops future broadcasts from reaching it.
    pub fn remove_member(&self, client_id: &str) -> Option<ServerSender> {
        self.members().remove(client_id)
    }

    /// Send `stream/start` to every current member **without** touching the
    /// timeline — the shared-timeline counterpart to [`Self::start_stream`].
    ///
    /// Use when several groups share one [`SharedTimeline`]: set the config once
    /// on the timeline ([`SharedTimeline::set_config`]), then start each group.
    /// Safe on an owned timeline too, where it simply means "re-announce the
    /// stream without re-anchoring".
    pub async fn broadcast_stream_start(&self, config: StreamPlayerConfig) {
        self.broadcast(|sender| sender.queue_stream_start(config.clone()))
            .await;
    }

    /// Send `stream/end` to every current member **without** touching the
    /// timeline — the shared-timeline counterpart to [`Self::end_stream`].
    pub async fn broadcast_stream_end(&self) {
        self.broadcast(|sender| sender.queue_stream_end()).await;
    }

    /// Ask every member to discard buffered-but-unplayed audio (e.g. after a
    /// seek) without ending the stream. Audio still queued for a member is
    /// dropped rather than written after the `stream/clear`. Does not touch the
    /// timeline — see `Group::clear_stream` on a timeline-owning group.
    pub async fn broadcast_stream_clear(&self) {
        self.broadcast(|sender| sender.queue_stream_clear()).await;
    }

    /// Push one PCM chunk to every member: stamp the timeline once, then fan
    /// the identical frame out. Returns that timestamp. Use this for a group
    /// that owns its timeline. When several groups share one timeline, stamp it
    /// yourself once per chunk and call [`Group::push_at`] per group so the
    /// timeline advances only once. Enqueue is non-blocking, so one slow member
    /// never delays the others; a member whose connection has died is pruned.
    pub(crate) fn push_audio_impl(&self, pcm: &[u8]) -> i64 {
        // Stamp under the member lock so this chunk's timestamp and its enqueue
        // are one atomic step relative to a concurrent lifecycle transition —
        // otherwise a chunk could be stamped against the old timeline and
        // enqueued after a `stream/end`.
        let mut members = self.members();
        let ts = self.timeline.stamp(pcm.len());
        Self::fan_out(&mut members, ts, pcm);
        ts
    }

    /// Fan one PCM chunk out to every member at a caller-supplied timestamp,
    /// **without** advancing the timeline. (Named for what is pre-supplied — the
    /// timestamp; the encoding happens here.) This is the shared-timeline path:
    /// the caller stamps a shared [`SharedTimeline`] once and delivers that one
    /// `ts` to each group/sender, so every member's chunk-N carries an
    /// identical timestamp. (For a group that owns its timeline, prefer
    /// [`Group::push_audio`], which stamps and fans in one call.)
    pub fn push_at(&self, ts: i64, pcm: &[u8]) {
        // Encode *before* taking the lock. The frame is a pure function of
        // (ts, pcm) and observable to nobody, so this does not weaken the ordering
        // guarantee — the enqueue still happens under the lock, synchronously —
        // but it keeps an allocation and a full payload copy out of a critical
        // section that a SCHED_FIFO producer may be waiting on.
        let frame = encode_audio_frame(ts, pcm);
        let mut members = self.members();
        Self::fan_out_frame(&mut members, frame);
    }

    /// Encode, then fan out. Only used where the timestamp is produced under the
    /// same lock ([`Self::push_audio`]); [`Self::push_at`] encodes outside it.
    fn fan_out(members: &mut HashMap<String, ServerSender>, ts: i64, pcm: &[u8]) {
        Self::fan_out_frame(members, encode_audio_frame(ts, pcm));
    }

    /// Fan one already-encoded frame out to every member as cheap refcount clones,
    /// and prune members whose connection has died.
    fn fan_out_frame(members: &mut HashMap<String, ServerSender>, frame: AudioFrame) {
        let mut dead = Vec::new();
        // Hand the buffer to the *last* member rather than cloning for it. With a
        // single member — the per-device topology this exists to serve — that means
        // no clone at all, which avoids `Bytes` promoting to its shared
        // representation and saves a whole allocation per chunk.
        let last = members.len().saturating_sub(1);
        let mut frame = Some(frame);
        for (idx, (id, sender)) in members.iter().enumerate() {
            let this = if idx == last {
                frame.take().expect("one frame handed out per member")
            } else {
                frame.clone().expect("frame is owned until the last member")
            };
            match sender.queue_audio(this) {
                AudioEnqueue::Queued => {}
                AudioEnqueue::Dropped => {
                    log::trace!("group member {id} audio backlog full, dropping chunk")
                }
                AudioEnqueue::Disconnected => dead.push(id.clone()),
            }
        }
        for id in dead {
            log::warn!("dropping dead group member {id}");
            members.remove(&id);
        }
    }

    /// Broadcast a player command (volume, mute, static delay) to every
    /// member. Player commands are independent of the stream, so they overtake
    /// queued audio instead of waiting behind it.
    pub async fn send_player_command(&self, command: PlayerCommand) {
        self.broadcast(|sender| sender.queue_player_command(command.clone()))
            .await;
    }

    /// Queue one control frame per member under the member lock, then await the
    /// writes with the lock released.
    ///
    /// Every lifecycle broadcast goes through here so the lock discipline that makes
    /// frame order authoritative — queue synchronously while holding it, never await
    /// under it — exists in exactly one place.
    async fn broadcast(&self, queue: impl Fn(&ServerSender) -> QueuedControl) {
        let queued = {
            let members = self.members();
            Self::queue_each(&members, queue)
        };
        self.settle(queued).await;
    }

    /// Queue one control frame per member, synchronously, in member order. The
    /// caller holds the member lock across this — that's what makes the
    /// resulting order authoritative.
    fn queue_each(
        members: &HashMap<String, ServerSender>,
        queue: impl Fn(&ServerSender) -> QueuedControl,
    ) -> Vec<(String, QueuedControl)> {
        members
            .iter()
            .map(|(id, sender)| (id.clone(), queue(sender)))
            .collect()
    }

    /// Await already-queued control frames concurrently — a slow member never
    /// delays learning about the others — then drop any member whose write
    /// failed (its writer task is gone or its socket stalled past the write
    /// timeout; either way the failure is permanent).
    async fn settle(&self, queued: Vec<(String, QueuedControl)>) {
        let results = join_all(
            queued
                .into_iter()
                .map(|(id, queued)| async move { (id, queued.written().await) }),
        )
        .await;
        let mut members = self.members();
        for (id, result) in results {
            if let Err(e) = result {
                log::warn!("dropping group member {id}: {e}");
                members.remove(&id);
            }
        }
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
    fn new_group_is_empty() {
        let group = Group::new(Arc::new(DefaultClock::default()));
        assert!(group.is_empty());
        assert_eq!(group.len(), 0);
        assert_eq!(group.member_ids().len(), 0);
    }

    #[test]
    fn groups_can_share_one_timeline() {
        let a = Group::new(Arc::new(DefaultClock::default()));
        let b = Group::with_timeline(a.timeline());
        assert!(Arc::ptr_eq(&a.timeline(), &b.timeline()));
    }

    /// A shared timeline belongs to whoever coordinates the senders, not to any one
    /// group: re-anchoring or clearing it from a single group would desync every
    /// other group sharing it. That is enforced by the type — `Group<SharesTimeline>`
    /// has no `start_stream`/`end_stream`/`clear_stream`/`push_audio` at all, so the
    /// following does not compile:
    ///
    /// ```compile_fail
    /// # use sendspin::server::{Group, SharedTimeline};
    /// # use std::sync::Arc;
    /// # let clock = Arc::new(sendspin::DefaultClock::default());
    /// let shared = Arc::new(SharedTimeline::new(clock));
    /// let group = Group::with_timeline(shared);
    /// group.push_audio(&[0u8; 8]); // no such method on a shared timeline
    /// ```
    ///
    /// What remains testable at runtime is that an owner really does drive the
    /// timeline, and that a sharer can start its own members without touching it.
    #[tokio::test]
    async fn an_owner_drives_the_timeline_and_a_sharer_leaves_it_alone() {
        let owner = Group::new(Arc::new(DefaultClock::default()));
        let sharer = Group::with_timeline(owner.timeline());

        assert!(owner.timeline().config().is_none());
        owner.start_stream(pcm_config()).await;
        assert!(owner.timeline().config().is_some());

        // The sharer announces the stream to its own members and leaves the
        // timeline exactly as the owner set it.
        sharer.broadcast_stream_start(pcm_config()).await;
        assert!(owner.timeline().config().is_some());

        owner.end_stream().await;
        assert!(owner.timeline().config().is_none());
    }
}
