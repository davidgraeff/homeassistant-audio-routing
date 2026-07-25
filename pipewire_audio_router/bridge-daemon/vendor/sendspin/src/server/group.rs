// ABOUTME: Multi-client synchronized playback group
// ABOUTME: Every member receives identical audio bytes with identical timestamps, so each client's clock-sync offset alone yields sample-accurate multi-room sync

use crate::error::Error;
use crate::protocol::messages::{PlayerCommand, StreamPlayerConfig};
use crate::server::binary::encode_audio_frame;
use crate::server::connection::{AudioEnqueue, ServerSender};
use crate::server::timeline::SharedTimeline;
use crate::sync::raw_clock::Clock;
use futures_util::future::join_all;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Bytes;

// Re-exported here for source compatibility; the constant now lives with the
// timeline it parameterizes.
pub use crate::server::timeline::DEFAULT_SEND_AHEAD_US;

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
/// [`Group::push_encoded`], instead of calling [`Group::push_audio`] per group
/// (which would advance the shared timeline once per group).
///
/// v1 scope: one shared PCM format for the whole group — no per-client
/// transcoding, so a member that can't take the group's format is a v1
/// limitation, not silently-wrong audio. No late-join catch-up (a client
/// added mid-stream just gets `stream/start` and audio from that point
/// forward) and no historical buffer replay.
pub struct Group {
    timeline: Arc<SharedTimeline>,
    members: Mutex<HashMap<String, ServerSender>>,
}

impl Group {
    /// Create an empty group that owns a fresh timeline in `clock`'s domain —
    /// pass the same clock the [`crate::server::ServerListener`] that accepted
    /// these connections was built with, so timestamps here are in the same
    /// domain as the `server/time` replies members already trust.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_timeline(Arc::new(SharedTimeline::new(clock)))
    }

    /// Create an empty group that shares an existing [`SharedTimeline`] with
    /// other groups/senders. All groups sharing one timeline emit identical
    /// timestamps for the same chunk — see the type docs for the stamp-once
    /// contract.
    pub fn with_timeline(timeline: Arc<SharedTimeline>) -> Self {
        Self {
            timeline,
            members: Mutex::new(HashMap::new()),
        }
    }

    /// Override the default send-ahead lead time. Only valid on a group that
    /// owns its (as-yet-unshared) timeline, i.e. straight after [`Group::new`];
    /// it rebuilds the timeline with the new lead.
    pub fn with_send_ahead_us(self, send_ahead_us: i64) -> Self {
        let clock = self.timeline.clock();
        Self {
            timeline: Arc::new(SharedTimeline::new(clock).with_send_ahead_us(send_ahead_us)),
            members: self.members,
        }
    }

    /// The timeline backing this group, so a caller can share it across
    /// per-device senders (`Group::with_timeline(group.timeline())`) and stamp
    /// it once per chunk.
    pub fn timeline(&self) -> Arc<SharedTimeline> {
        Arc::clone(&self.timeline)
    }

    /// Client IDs of every current member.
    pub fn member_ids(&self) -> Vec<String> {
        self.members.lock().unwrap().keys().cloned().collect()
    }

    /// Number of current members.
    pub fn len(&self) -> usize {
        self.members.lock().unwrap().len()
    }

    /// Whether the group has no members.
    pub fn is_empty(&self) -> bool {
        self.members.lock().unwrap().is_empty()
    }

    /// Add a member. If a stream is already active for this group, starts
    /// it for the new member too (matching the group's already-negotiated
    /// format) — but does not replay any audio already delivered to
    /// existing members (no late-join catch-up in v1, see the type docs).
    pub async fn add_member(
        &self,
        client_id: impl Into<String>,
        sender: ServerSender,
    ) -> Result<(), Error> {
        if let Some(cfg) = self.timeline.config() {
            sender.send_stream_start(cfg).await?;
        }
        self.members.lock().unwrap().insert(client_id.into(), sender);
        Ok(())
    }

    /// Remove a member, if present. The caller is responsible for actually
    /// disconnecting it (e.g. via [`crate::server::ServerConnection::disconnect`]) —
    /// this only stops future broadcasts from reaching it.
    pub fn remove_member(&self, client_id: &str) -> Option<ServerSender> {
        self.members.lock().unwrap().remove(client_id)
    }

    /// Start (or restart, e.g. after a format change) the shared stream for
    /// every current member, and re-anchor the audio timeline.
    pub async fn start_stream(&self, config: StreamPlayerConfig) {
        self.timeline.set_config(config.clone());
        self.broadcast_control(|sender| {
            let config = config.clone();
            async move { sender.send_stream_start(config).await }
        })
        .await;
    }

    /// Push one PCM chunk to every member: stamp the timeline once, then fan
    /// the identical frame out. Returns that timestamp. Use this for a group
    /// that owns its timeline. When several groups share one timeline, stamp it
    /// yourself once per chunk and call [`Group::push_encoded`] per group so the
    /// timeline advances only once. Enqueue is non-blocking, so one slow member
    /// never delays the others; a member whose connection has died is pruned.
    pub fn push_audio(&self, pcm: &[u8]) -> i64 {
        let ts = self.timeline.stamp(pcm.len());
        self.push_encoded(ts, pcm);
        ts
    }

    /// Fan one PCM chunk out to every member at a caller-supplied timestamp,
    /// **without** advancing the timeline. This is the shared-timeline path:
    /// the caller stamps a shared [`SharedTimeline`] once and delivers that one
    /// `ts` to each group/sender, so every member's chunk-N carries an
    /// identical timestamp. (For a group that owns its timeline, prefer
    /// [`Group::push_audio`], which stamps and fans in one call.)
    pub fn push_encoded(&self, ts: i64, pcm: &[u8]) {
        // Encode once; fan the same frame out to every member as cheap refcount
        // clones. Prune members whose connection has died.
        let frame: Bytes = encode_audio_frame(ts, pcm).into();
        let mut members = self.members.lock().unwrap();
        let mut dead = Vec::new();
        for (id, sender) in members.iter() {
            match sender.enqueue_audio(frame.clone()) {
                Ok(AudioEnqueue::Sent) => {}
                Ok(AudioEnqueue::Evicted) => {
                    log::trace!("group member {id} audio backlog full, dropping chunk")
                }
                Err(_) => dead.push(id.clone()),
            }
        }
        for id in dead {
            log::warn!("dropping dead group member {id}");
            members.remove(&id);
        }
    }

    /// Broadcast a player command (volume, mute, static delay) to every
    /// member.
    pub async fn send_player_command(&self, command: PlayerCommand) {
        self.broadcast_control(|sender| {
            let command = command.clone();
            async move { sender.send_player_command(command).await }
        })
        .await;
    }

    /// End the shared stream for every current member and reset the timeline.
    pub async fn end_stream(&self) {
        self.timeline.clear_config();
        self.broadcast_control(|sender| async move { sender.send_stream_end().await })
            .await;
    }

    /// Run an awaiting control send `f` concurrently against every member — a
    /// slow member never blocks delivery to the others — then drop any member
    /// `f` failed against (its writer task is gone; the failure is permanent).
    async fn broadcast_control<F, Fut>(&self, f: F)
    where
        F: Fn(ServerSender) -> Fut,
        Fut: std::future::Future<Output = Result<(), Error>>,
    {
        let members: Vec<(String, ServerSender)> = {
            let members = self.members.lock().unwrap();
            members
                .iter()
                .map(|(id, sender)| (id.clone(), sender.clone()))
                .collect()
        };
        let results = join_all(members.into_iter().map(|(id, sender)| {
            let fut = f(sender);
            async move { (id, fut.await) }
        }))
        .await;
        let mut members = self.members.lock().unwrap();
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
}
