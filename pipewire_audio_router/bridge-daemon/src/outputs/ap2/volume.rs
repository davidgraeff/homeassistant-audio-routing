//! Per-device volume/mute control for AirPlay-2 senders.
//!
//! Like sendspin devices (outputs/sendspin/volume.rs), AP2 receivers are *virtual*
//! outputs (`ap2-dev-<slug>`) with no PipeWire node volume — volume is carried
//! in-band, here as an RTSP `SET_PARAMETER` the sender sends to the receiver
//! (`airplay_client::Connection::set_volume`, 0.0–1.0 → dB). Two differences
//! from sendspin:
//!
//! * The live `Connection` needs `&mut` to send and lives inside the group's
//!   sender task (outputs/ap2/server.rs), so this registry can't hold it directly — it
//!   holds an mpsc *command channel* to that task and the task owns the
//!   connections. Commands carry the device node name so the task picks the
//!   right one.
//! * AirPlay has no dedicated mute; mute is volume `0.0` (−144 dB). This layer
//!   keeps the desired volume separate from the mute flag and only ever sends a
//!   single `SetVolume` — mute→`0.0`, unmute→the stored desired — so the task
//!   stays trivial and unmuting restores the prior level.
//!
//! There is currently no device→UI feedback (the vendored sender doesn't parse
//! the receiver's event channel), so the matrix shows the desired/last-set level
//! rather than a physical change made on the receiver itself.
//!
//! # Lock discipline: nothing here may await
//!
//! Every mutator on [`Ap2Control`] is **synchronous**, and that is deliberate. The
//! control lives behind an async `Mutex`, and its readers are the routing matrix and
//! the outputs listing — i.e. `/api/outputs`, `/api/outputs/discovered`,
//! `/api/routing` and the reconciler. Any `.await` reached while the guard is held
//! parks all of them behind it for the duration.
//!
//! Making the writes sync means a caller *cannot* hold the guard across an await even
//! by accident, so the usual `control.lock().await.set_volume(..)` one-liner is safe
//! here. `SendspinControl` needs the opposite treatment (`PendingCommands`, built
//! under the guard and applied after dropping it) because its writes are real network
//! I/O; ours only hand a command to a local task, so removing the await is both
//! simpler and stronger than relocating it. See [`Ap2Control::try_queue`].

use crate::pw::thread::ChangeNotifier;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Shared handle to the AP2 volume control (API + routing snapshot + every group
/// task hold a clone).
pub type SharedAp2Control = Arc<Mutex<Ap2Control>>;

/// Depth of the per-group command channel (here → the group task in
/// outputs/ap2/server.rs). Volume/mute changes are rare and tiny, so a small buffer is
/// ample for a *draining* task; reaching it means the task is not draining, which
/// [`Ap2Control::try_queue`] reports rather than waits out.
pub const AP2_CMD_DEPTH: usize = 32;

/// A command to a running AP2 group task (outputs/ap2/server.rs), targeting one receiver
/// by its virtual node name. Only volume: mute is expressed as volume `0.0` by
/// [`Ap2Control`] before it reaches the task.
#[derive(Debug, Clone)]
pub enum Ap2Command {
    SetVolume {
        node_name: String,
        volume: f32,
    },
    /// Change the render delay (PT=87 anchor offset) on the LIVE stream, without a
    /// reconnect — the streamer reads it per packet, so a UI change takes effect
    /// mid-stream. Avoids the session churn a group restart caused (which could leave
    /// a receiver silent).
    SetRenderDelay {
        node_name: String,
        ms: u16,
    },
}

#[derive(Default)]
pub struct Ap2Control {
    /// Command sink per connected device node name → the group task that owns its
    /// `Connection`. Absent = not currently streaming (desired state is kept for
    /// the next connect). Several devices in one group share the same sender.
    senders: HashMap<String, mpsc::Sender<Ap2Command>>,
    /// Known volume (0.0–1.0) per device node name — from a device *read*
    /// (`note_reported_volume`) or a *user* set (`set_volume`). **Absent = unknown**
    /// (we haven't read it and the user hasn't set it): the UI shows no/zero level
    /// rather than a made-up one. We never invent a value to push to the receiver
    /// on connect — the receiver's own volume is authoritative.
    desired_volume: HashMap<String, f32>,
    /// Node names the *user* explicitly set via the UI (vs a device-reported read).
    /// Only these are re-applied on our own reconnects (a rate/membership restart);
    /// a first connect with no user intent never pushes a volume to the receiver.
    user_set: std::collections::HashSet<String>,
    /// Desired mute per device node name; absent = unmuted.
    desired_muted: HashMap<String, bool>,
    /// Nudges the routing-matrix WebSocket to rebuild+push on any change, so the
    /// UI slider reflects a set without polling (mirrors SendspinControl).
    change_notifier: Option<ChangeNotifier>,
}

/// Construct an empty control wrapped for sharing.
pub fn shared() -> SharedAp2Control {
    Arc::new(Mutex::new(Ap2Control::default()))
}

impl Ap2Control {
    pub fn set_change_notifier(&mut self, changes: ChangeNotifier) {
        self.change_notifier = Some(changes);
    }

    fn notify_changed(&self) {
        if let Some(changes) = &self.change_notifier {
            let _ = changes.send(());
        }
    }

    /// Nudge the reconciler (e.g. after an AP2 rate downgrade cached a new
    /// capability, so the group is re-evaluated and restarted at 44.1 kHz).
    pub fn notify_reconcile(&self) {
        self.notify_changed();
    }

    /// Node names whose AP2 sender is connected and streaming *right now* (it
    /// registered its command channel). Distinguishes a receiver actually being fed
    /// from one merely routed — a group's `ap2_members` includes receivers whose
    /// session failed or is still pairing. The announce path uses this to decide
    /// whether an overlay dropped on an output will really be consumed.
    pub fn connected(&self) -> std::collections::HashSet<String> {
        self.senders.keys().cloned().collect()
    }

    /// The level to send when we DO send (mute/unmute, or re-applying user intent):
    /// `0.0` while muted, else the known volume, else `0.0` (unknown → silent, never
    /// a made-up level). Note `register` does NOT call this on a first connect — the
    /// device's own volume is left untouched there.
    fn effective_volume(&self, node_name: &str) -> f32 {
        if self.desired_muted.get(node_name).copied().unwrap_or(false) {
            0.0
        } else {
            self.desired_volume.get(node_name).copied().unwrap_or(0.0)
        }
    }

    /// Hand one command to the device's group task. **Never awaits and never
    /// blocks** — see [`Self::try_queue`] for why that is load-bearing rather than a
    /// style choice.
    fn send(&self, node_name: &str, volume: f32) -> bool {
        self.try_queue(node_name, Ap2Command::SetVolume { node_name: node_name.to_string(), volume })
    }

    /// Queue a command for the group task that owns `node_name`'s `Connection`,
    /// dropping it (with a warning) rather than waiting for room.
    ///
    /// **This must never become an `await`.** Every caller holds the `Ap2Control`
    /// guard — an async `Mutex` — while calling in, so a `send().await` that parks on
    /// a full channel holds that guard for as long as it parks. Everything that reads
    /// the same guard then parks behind it: `/api/outputs`, `/api/outputs/discovered`,
    /// `/api/routing` and the reconciler. That is a whole-daemon deadlock reached from
    /// a volume write, and it is not hypothetical — it took the web UI down on
    /// 2026-08-12, because a group task stops draining this channel for as long as it
    /// is mid-connect on an unresponsive receiver, and the alignment session writes a
    /// level plus a mute per member per position (easily the channel's depth).
    ///
    /// `try_send` inverts the failure: a wedged group task loses control writes and
    /// says so in the log, instead of taking the UI with it. Nothing is lost from our
    /// *model* — the desired level/mute stays recorded here, the caller is told the
    /// write did not land, and `register` re-applies a user-set level on the next
    /// connect. Contrast `SendspinControl`, which keeps its sends `async` behind
    /// `PendingCommands` because there the send is a real network write with a
    /// timeout; here it is only a handoff to a local task, so the honest fix is to
    /// remove the await rather than to relocate it.
    fn try_queue(&self, node_name: &str, cmd: Ap2Command) -> bool {
        let Some(tx) = self.senders.get(node_name) else {
            return false;
        };
        match tx.try_send(cmd) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    "AP2: '{node_name}' has {} unprocessed control commands — dropping this one. \
                     Its sender task is not draining (mid-connect on a slow receiver, or wedged); \
                     the level/mute is still recorded and re-applies on its next connect.",
                    AP2_CMD_DEPTH
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("AP2: '{node_name}' sender task is gone; control command dropped");
                false
            }
        }
    }

    /// Change a device's render delay LIVE (no reconnect) if it's streaming. Returns
    /// true if a command reached the group task. The value is also persisted by the
    /// caller (sync_settings) so a later reconnect uses it as the initial delay.
    pub fn set_render_delay(&self, node_name: &str, ms: u16) -> bool {
        self.try_queue(node_name, Ap2Command::SetRenderDelay { node_name: node_name.to_string(), ms })
    }

    /// Register a freshly-connected device's group-task command channel.
    ///
    /// Does NOT push a volume for a device the user hasn't touched — the receiver's
    /// own volume is authoritative and must be respected (imposing one would blast a
    /// powerful amp, since AirPlay volume is sender-authoritative dB and a fresh
    /// connect otherwise defaulted to 0 dB = max). Only if the user explicitly set a
    /// level THIS session (`user_set`) do we re-apply it across our own restarts
    /// (rate/membership reconnects), so their intent isn't lost. The real device
    /// volume is instead learned by a read after connect (`note_reported_volume`).
    pub fn register(&mut self, node_name: String, tx: mpsc::Sender<Ap2Command>) {
        self.senders.insert(node_name.clone(), tx);
        if self.user_set.contains(&node_name) {
            let effective = self.effective_volume(&node_name);
            self.send(&node_name, effective);
        }
        // The sender set IS this receiver's session state (`connected`), which the
        // routing matrix reports as `streaming` — push a frame so the graph stops
        // showing the wire as waiting the moment the session comes up.
        self.notify_changed();
    }

    /// Drop a device whose group task is gone (desired state kept for reconnect).
    pub fn unregister(&mut self, node_name: &str) {
        self.senders.remove(node_name);
        self.notify_changed();
    }

    /// Set a device's desired volume (0.0–1.0). Applies live unless muted (then it
    /// takes effect on unmute). Returns true if a command reached a live task —
    /// `false` also covers "its task is not draining", so the caller reports what
    /// actually happened rather than assuming a live sender consumed it.
    pub fn set_volume(&mut self, node_name: &str, volume: f32) -> bool {
        let volume = volume.clamp(0.0, 1.0);
        // Explicit user intent: remember it so it survives our own reconnects.
        self.user_set.insert(node_name.to_string());
        let changed = self.desired_volume.insert(node_name.to_string(), volume) != Some(volume);
        if changed {
            self.notify_changed();
        }
        let muted = self.desired_muted.get(node_name).copied().unwrap_or(false);
        if !muted {
            return self.send(node_name, volume);
        }
        false
    }

    /// Like [`Self::set_volume`], but **without claiming user intent** — for a level
    /// the *daemon* is imposing temporarily rather than one the user chose.
    ///
    /// The alignment session drives every audible member's level for its duration
    /// (docs/mic-alignment-plan.md §12.2) and puts the original back at teardown. Going
    /// through `set_volume` for that would leave a mark teardown cannot erase: it adds
    /// the node to `user_set`, and `register` re-applies a `user_set` level on every
    /// later reconnect — so a calibration level of 20 % would come back as if the user
    /// had asked for it, on an amplifier, forever. `desired_volume` is still set,
    /// because `set_muted(false)` re-sends it and the calibration level has to survive
    /// an unmute *within* the session.
    ///
    /// Pair it with [`Self::forget_volume`] when the pre-session level was unknown.
    pub fn set_volume_transient(&mut self, node_name: &str, volume: f32) -> bool {
        let volume = volume.clamp(0.0, 1.0);
        let changed = self.desired_volume.insert(node_name.to_string(), volume) != Some(volume);
        if changed {
            self.notify_changed();
        }
        let muted = self.desired_muted.get(node_name).copied().unwrap_or(false);
        if !muted {
            return self.send(node_name, volume);
        }
        false
    }

    /// Forget any desired level for a device, and any claim that the user chose it.
    ///
    /// The restore counterpart for a member whose level was **unknown** before a
    /// session drove it: there is no value to put back, and writing an invented one is
    /// the one thing that must not happen (AP2 level is device-authoritative). Dropping
    /// the entry returns the receiver to "we do not know and will not impose", which is
    /// the state it was in. Sends nothing — the receiver keeps whatever it is playing at.
    pub fn forget_volume(&mut self, node_name: &str) {
        let had = self.desired_volume.remove(node_name).is_some() | self.user_set.remove(node_name);
        if had {
            self.notify_changed();
        }
    }

    /// Set a device's desired mute. Sends volume `0.0` (mute) or the stored
    /// desired volume (unmute). Returns true if a command reached a live task.
    pub fn set_muted(&mut self, node_name: &str, muted: bool) -> bool {
        let changed = self.desired_muted.insert(node_name.to_string(), muted) != Some(muted);
        if changed {
            self.notify_changed();
        }
        self.send(node_name, self.effective_volume(node_name))
    }

    /// Record a **receiver-reported** volume (0.0–1.0), parsed from the AP2 event
    /// channel (outputs/ap2/server.rs → `Connection::volume_events`) — e.g. a user turning
    /// the AVR's own knob. Updates the stored level so the UI reflects it, WITHOUT
    /// sending anything back to the receiver (that would fight the physical
    /// control). No-op when unchanged, or while muted (we send ~0 when muted, so a
    /// reported ~0 must not clobber the pre-mute level we restore on unmute).
    pub fn note_reported_volume(&mut self, node_name: &str, volume: f32) {
        if self.desired_muted.get(node_name).copied().unwrap_or(false) {
            return;
        }
        let volume = volume.clamp(0.0, 1.0);
        if self.desired_volume.get(node_name) == Some(&volume) {
            return;
        }
        tracing::debug!("ap2 receiver '{node_name}' reported volume {volume:.3}");
        self.desired_volume.insert(node_name.to_string(), volume);
        self.notify_changed();
    }

    /// Snapshot of desired volumes (0.0–1.0) by node name, for the routing matrix.
    pub fn volumes(&self) -> HashMap<String, f32> {
        self.desired_volume.clone()
    }

    /// Snapshot of desired mute states by node name, for the routing matrix.
    pub fn mutes(&self) -> HashMap<String, bool> {
        self.desired_muted.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alignment session drives a level and then puts it back. Going through
    /// `set_volume` would leave the node in `user_set`, so a later reconnect would
    /// re-impose the calibration level on an amplifier as if the user had chosen it —
    /// a side effect no teardown can reach. The transient path must not do that.
    #[tokio::test]
    async fn a_transient_level_does_not_claim_user_intent() {
        let mut c = Ap2Control::default();
        c.set_volume_transient("ap2-dev-yamaha", 0.2);
        // Still the desired level, because `set_muted(false)` re-sends it mid-session.
        assert_eq!(c.volumes().get("ap2-dev-yamaha").copied(), Some(0.2));
        assert!(!c.user_set.contains("ap2-dev-yamaha"), "a daemon-imposed level is not user intent");

        // Whereas an explicit user set does claim it, and must keep doing so.
        c.set_volume("ap2-dev-pioneer", 0.4);
        assert!(c.user_set.contains("ap2-dev-pioneer"));
    }

    /// The restore path for a member whose level was unknown before the session: there
    /// is nothing to put back, and inventing one is what must not happen.
    #[tokio::test]
    async fn forgetting_a_level_returns_the_receiver_to_unknown() {
        let mut c = Ap2Control::default();
        c.set_volume_transient("ap2-dev-yamaha", 0.2);
        c.forget_volume("ap2-dev-yamaha");
        assert_eq!(c.volumes().get("ap2-dev-yamaha"), None, "unknown again, not 0.0");
        assert!(!c.user_set.contains("ap2-dev-yamaha"));
        // Idempotent: teardown runs on paths that may already have cleaned up.
        c.forget_volume("ap2-dev-yamaha");
    }

    #[tokio::test]
    async fn volume_persists_without_a_live_task() {
        let mut c = Ap2Control::default();
        assert!(!c.set_volume("ap2-dev-yamaha", 0.5));
        assert_eq!(c.volumes().get("ap2-dev-yamaha").copied(), Some(0.5));
    }

    #[tokio::test]
    async fn mute_sends_zero_then_unmute_restores_desired() {
        let mut c = Ap2Control::default();
        let (tx, mut rx) = mpsc::channel(8);
        c.set_volume("ap2-dev-yamaha", 0.6); // stored, no task yet
        c.register("ap2-dev-yamaha".to_string(), tx); // applies 0.6
        c.set_muted("ap2-dev-yamaha", true); // sends 0.0
        c.set_muted("ap2-dev-yamaha", false); // sends 0.6 again

        let mut seen = Vec::new();
        while let Ok(Ap2Command::SetVolume { volume, .. }) = rx.try_recv() {
            seen.push(volume);
        }
        assert_eq!(seen, vec![0.6, 0.0, 0.6]);
    }

    /// The regression that matters most in this file: a group task that has stopped
    /// draining its command channel must not be able to stall a writer.
    ///
    /// This is the 2026-08-12 daemon deadlock in miniature. The receiver end is kept
    /// alive but never read, so the channel fills; every further write must return
    /// promptly and report `false` rather than parking. It parked before, and because
    /// every caller writes while holding the `Ap2Control` guard, that parked
    /// `/api/outputs`, `/api/outputs/discovered`, `/api/routing` and the reconciler for
    /// as long as the receiver stayed wedged — which was indefinitely.
    ///
    /// `tokio::time::timeout` with a paused clock is the assertion: if any of these
    /// writes ever awaits again, the timeout resolves first and the test fails instead
    /// of hanging the suite.
    #[tokio::test(start_paused = true)]
    async fn a_task_that_stopped_draining_cannot_stall_a_writer() {
        let mut c = Ap2Control::default();
        // `_rx` is deliberately never read: this is a task stuck mid-connect.
        let (tx, _rx) = mpsc::channel(AP2_CMD_DEPTH);
        c.register("ap2-dev-wedged".to_string(), tx);

        let fill = async {
            for i in 0..AP2_CMD_DEPTH {
                assert!(c.set_volume("ap2-dev-wedged", i as f32 / AP2_CMD_DEPTH as f32), "queue has room at {i}");
            }
            // Past the channel's depth the command is dropped and reported, not awaited.
            assert!(!c.set_volume("ap2-dev-wedged", 0.9), "a full queue reports the write did not land");
            assert!(!c.set_muted("ap2-dev-wedged", true), "and so does a mute");
            assert!(!c.set_render_delay("ap2-dev-wedged", 250), "and a render-delay change");
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), fill).await.expect("writing to a non-draining task must never block");

        // The desired state is still recorded, so nothing is lost from our model —
        // `register` re-applies a user-set level on the device's next connect.
        assert_eq!(c.volumes().get("ap2-dev-wedged").copied(), Some(0.9));
        assert_eq!(c.mutes().get("ap2-dev-wedged").copied(), Some(true));
    }

    /// A device whose task is gone reports honestly too — same contract as a full
    /// queue, different cause, and the path a torn-down group leaves behind until
    /// `unregister` runs.
    #[tokio::test(start_paused = true)]
    async fn a_closed_channel_reports_instead_of_blocking() {
        let mut c = Ap2Control::default();
        let (tx, rx) = mpsc::channel(AP2_CMD_DEPTH);
        c.register("ap2-dev-gone".to_string(), tx);
        drop(rx);

        let write = async { assert!(!c.set_volume("ap2-dev-gone", 0.5), "a dead task cannot be reached") };
        tokio::time::timeout(std::time::Duration::from_secs(30), write).await.expect("a closed channel must not block");
    }
}
