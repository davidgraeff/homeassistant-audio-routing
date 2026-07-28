//! Per-device volume/mute control for AirPlay-2 senders.
//!
//! Like sendspin devices (sendspin_volume.rs), AP2 receivers are *virtual*
//! outputs (`ap2-dev-<slug>`) with no PipeWire node volume — volume is carried
//! in-band, here as an RTSP `SET_PARAMETER` the sender sends to the receiver
//! (`airplay_client::Connection::set_volume`, 0.0–1.0 → dB). Two differences
//! from sendspin:
//!
//! * The live `Connection` needs `&mut` to send and lives inside the group's
//!   sender task (ap2_server.rs), so this registry can't hold it directly — it
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

use crate::pw_thread::ChangeNotifier;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Shared handle to the AP2 volume control (API + routing snapshot + every group
/// task hold a clone).
pub type SharedAp2Control = Arc<Mutex<Ap2Control>>;

/// A command to a running AP2 group task (ap2_server.rs), targeting one receiver
/// by its virtual node name. Only volume: mute is expressed as volume `0.0` by
/// [`Ap2Control`] before it reaches the task.
#[derive(Debug, Clone)]
pub enum Ap2Command {
    SetVolume { node_name: String, volume: f32 },
    /// Change the render delay (PT=87 anchor offset) on the LIVE stream, without a
    /// reconnect — the streamer reads it per packet, so a UI change takes effect
    /// mid-stream. Avoids the session churn a group restart caused (which could leave
    /// a receiver silent).
    SetRenderDelay { node_name: String, ms: u16 },
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

    async fn send(&self, node_name: &str, volume: f32) {
        if let Some(tx) = self.senders.get(node_name) {
            let cmd = Ap2Command::SetVolume { node_name: node_name.to_string(), volume };
            if let Err(e) = tx.send(cmd).await {
                tracing::warn!("ap2 volume command for '{node_name}' dropped: {e}");
            }
        }
    }

    /// Change a device's render delay LIVE (no reconnect) if it's streaming. Returns
    /// true if a command reached the group task. The value is also persisted by the
    /// caller (sync_settings) so a later reconnect uses it as the initial delay.
    pub async fn set_render_delay(&self, node_name: &str, ms: u16) -> bool {
        if let Some(tx) = self.senders.get(node_name) {
            let cmd = Ap2Command::SetRenderDelay { node_name: node_name.to_string(), ms };
            if tx.send(cmd).await.is_ok() {
                return true;
            }
        }
        false
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
    pub async fn register(&mut self, node_name: String, tx: mpsc::Sender<Ap2Command>) {
        self.senders.insert(node_name.clone(), tx);
        if self.user_set.contains(&node_name) {
            let effective = self.effective_volume(&node_name);
            self.send(&node_name, effective).await;
        }
    }

    /// Drop a device whose group task is gone (desired state kept for reconnect).
    pub fn unregister(&mut self, node_name: &str) {
        self.senders.remove(node_name);
    }

    /// Set a device's desired volume (0.0–1.0). Applies live unless muted (then it
    /// takes effect on unmute). Returns true if a command reached a live task.
    pub async fn set_volume(&mut self, node_name: &str, volume: f32) -> bool {
        let volume = volume.clamp(0.0, 1.0);
        // Explicit user intent: remember it so it survives our own reconnects.
        self.user_set.insert(node_name.to_string());
        let changed = self.desired_volume.insert(node_name.to_string(), volume) != Some(volume);
        if changed {
            self.notify_changed();
        }
        let muted = self.desired_muted.get(node_name).copied().unwrap_or(false);
        if !muted && self.senders.contains_key(node_name) {
            self.send(node_name, volume).await;
            return true;
        }
        false
    }

    /// Set a device's desired mute. Sends volume `0.0` (mute) or the stored
    /// desired volume (unmute). Returns true if a command reached a live task.
    pub async fn set_muted(&mut self, node_name: &str, muted: bool) -> bool {
        let changed = self.desired_muted.insert(node_name.to_string(), muted) != Some(muted);
        if changed {
            self.notify_changed();
        }
        if self.senders.contains_key(node_name) {
            self.send(node_name, self.effective_volume(node_name)).await;
            return true;
        }
        false
    }

    /// Record a **receiver-reported** volume (0.0–1.0), parsed from the AP2 event
    /// channel (ap2_server.rs → `Connection::volume_events`) — e.g. a user turning
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

    #[tokio::test]
    async fn volume_persists_without_a_live_task() {
        let mut c = Ap2Control::default();
        assert!(!c.set_volume("ap2-dev-yamaha", 0.5).await);
        assert_eq!(c.volumes().get("ap2-dev-yamaha").copied(), Some(0.5));
    }

    #[tokio::test]
    async fn mute_sends_zero_then_unmute_restores_desired() {
        let mut c = Ap2Control::default();
        let (tx, mut rx) = mpsc::channel(8);
        c.set_volume("ap2-dev-yamaha", 0.6).await; // stored, no task yet
        c.register("ap2-dev-yamaha".to_string(), tx).await; // applies 0.6
        c.set_muted("ap2-dev-yamaha", true).await; // sends 0.0
        c.set_muted("ap2-dev-yamaha", false).await; // sends 0.6 again

        let mut seen = Vec::new();
        while let Ok(Ap2Command::SetVolume { volume, .. }) = rx.try_recv() {
            seen.push(volume);
        }
        assert_eq!(seen, vec![0.6, 0.0, 0.6]);
    }
}
