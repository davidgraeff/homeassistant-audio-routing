//! Per-device volume control for sendspin (multi-room) speakers.
//!
//! Unlike AirPlay outputs — real PipeWire `raop-sink` nodes whose volume the
//! matrix drives through `node_id` — sendspin devices are *virtual* outputs fed
//! by a shared group sink (sync_group.rs), so there's no per-device
//! PipeWire volume to set. Sendspin instead carries volume in-band: the server
//! sends a `server/command` player `Volume` message to a specific client
//! (`ServerSender::send_player_command`).
//!
//! This registry is keyed by each device's virtual **node name**
//! (`sendspin-dev-<slug>`), the same identity the routing matrix and the API
//! use. The connection's own `client_id` is an opaque MAC on ESPHome devices
//! and does NOT match the advertised name, so the caller (sendspin_server.rs)
//! resolves the node name from the dialed mDNS fullname before registering —
//! see `ClientEvent::Connected.fullname` (our sendspin-rs patch). Desired
//! volumes persist across (re)connects and are re-applied on register.

use sendspin::protocol::messages::{PlayerCommand, PlayerCommandType};
use sendspin::server::ServerSender;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared handle to the volume control (API + every group server hold a clone).
/// A device with no entry in `desired` is at full scale (100) — the UI shows
/// sliders at 100 for those.
pub type SharedSendspinControl = Arc<Mutex<SendspinControl>>;

#[derive(Default)]
pub struct SendspinControl {
    /// Live connections, keyed by virtual device node name.
    senders: HashMap<String, ServerSender>,
    /// Desired volume (0–100) per virtual device node name; absent = default.
    /// This is also the *reported* volume: a device turning its physical knob
    /// updates this entry via [`Self::note_reported_volume`], so it tracks the
    /// device's real level rather than only what the UI last set.
    desired: HashMap<String, u8>,
    /// Desired mute state per virtual device node name; absent = unmuted. Like
    /// `desired`, this is also the *reported* mute: a device muting itself
    /// updates it via [`Self::note_reported_mute`]. Re-applied on reconnect.
    /// Independent of volume — uses the protocol's dedicated `Mute` command, so
    /// unmuting restores the prior level (see [`mute_cmd`]).
    desired_muted: HashMap<String, bool>,
    /// Fired whenever `desired` changes so the routing-matrix WebSocket rebuilds
    /// and pushes a fresh snapshot (routing.rs) — the device→UI half of volume
    /// sync, without the UI having to poll. Injected once at startup from main.rs
    /// (the same broadcast every other change source nudges); `None` until then
    /// (e.g. in unit tests), where mutations simply don't notify.
    change_notifier: Option<crate::pw_thread::ChangeNotifier>,
    /// Desired per-device *static delay* (ms), keyed by virtual device node
    /// name; absent = no extra delay. Unlike volume this IS persisted (across
    /// restarts) by sync_settings.rs and seeded back in via [`Self::seed_delays`]
    /// — a calibrated offset is useless if it resets. It's the per-client half of
    /// group sync: trim one speaker that's consistently early/late relative to
    /// the rest of its group.
    desired_delay: HashMap<String, u16>,
}

fn volume_cmd(volume: u8) -> PlayerCommand {
    PlayerCommand { command: PlayerCommandType::Volume, volume: Some(volume.min(100)), mute: None, static_delay_ms: None }
}

fn delay_cmd(ms: u16) -> PlayerCommand {
    PlayerCommand { command: PlayerCommandType::SetStaticDelay, volume: None, mute: None, static_delay_ms: Some(ms.min(5000)) }
}

fn mute_cmd(muted: bool) -> PlayerCommand {
    PlayerCommand { command: PlayerCommandType::Mute, volume: None, mute: Some(muted), static_delay_ms: None }
}

/// Construct an empty control wrapped for sharing.
pub fn shared() -> SharedSendspinControl {
    Arc::new(Mutex::new(SendspinControl::default()))
}

impl SendspinControl {
    /// Wire up the change-notifier (main.rs, once at startup) so `desired`
    /// mutations push a fresh routing-matrix snapshot to watching UIs.
    pub fn set_change_notifier(&mut self, changes: crate::pw_thread::ChangeNotifier) {
        self.change_notifier = Some(changes);
    }

    fn notify_changed(&self) {
        if let Some(changes) = &self.change_notifier {
            let _ = changes.send(());
        }
    }

    /// Record a **device-reported** current volume (from an inbound `client/state`
    /// player update — see sendspin_server.rs) as the new desired level, WITHOUT
    /// echoing a `Volume` command back to the device. Echoing would fight a user
    /// turning the physical knob, and re-applying our own just-sent value on the
    /// device's confirming state emit would loop. This is the device→UI half of
    /// volume sync: the UI reads [`Self::volumes`] over the routing WebSocket, so
    /// a physical change now surfaces there. No-op (and no push) when the value is
    /// unchanged, so a device's steady-state re-emits don't spam the matrix.
    pub fn note_reported_volume(&mut self, node_name: &str, volume: u8) {
        let volume = volume.min(100);
        if self.desired.get(node_name) == Some(&volume) {
            return;
        }
        tracing::debug!("sendspin device '{node_name}' reported volume {volume}");
        self.desired.insert(node_name.to_string(), volume);
        self.notify_changed();
    }

    /// Register a freshly-connected device (by its virtual node name) and
    /// (re)apply its stored volume + static delay so a reconnect restores what
    /// the user set.
    pub async fn register(&mut self, node_name: String, sender: ServerSender) {
        tracing::info!("sendspin device connected: {node_name}");
        if let Some(&vol) = self.desired.get(&node_name) {
            if let Err(e) = sender.send_player_command(volume_cmd(vol)).await {
                tracing::warn!("failed to apply stored volume {vol} to '{node_name}': {e}");
            }
        }
        if let Some(&ms) = self.desired_delay.get(&node_name) {
            if let Err(e) = sender.send_player_command(delay_cmd(ms)).await {
                tracing::warn!("failed to apply stored delay {ms}ms to '{node_name}': {e}");
            }
        }
        if let Some(&muted) = self.desired_muted.get(&node_name) {
            if let Err(e) = sender.send_player_command(mute_cmd(muted)).await {
                tracing::warn!("failed to apply stored mute {muted} to '{node_name}': {e}");
            }
        }
        self.senders.insert(node_name, sender);
    }

    /// Seed desired per-device delays at startup from the persisted sync
    /// settings (sync_settings.rs), so they re-apply as devices connect. Existing
    /// live entries are left untouched (a startup-only merge).
    pub fn seed_delays(&mut self, delays: HashMap<String, u16>) {
        for (node_name, ms) in delays {
            self.desired_delay.entry(node_name).or_insert(ms);
        }
    }

    /// Set a device's desired static delay (ms) and push it to the device if
    /// connected. `0` clears it. Returns true if it reached a live device.
    pub async fn set_delay(&mut self, node_name: &str, ms: u16) -> bool {
        if ms == 0 {
            self.desired_delay.remove(node_name);
        } else {
            self.desired_delay.insert(node_name.to_string(), ms.min(5000));
        }
        if let Some(sender) = self.senders.get(node_name) {
            match sender.send_player_command(delay_cmd(ms)).await {
                Ok(()) => return true,
                Err(e) => tracing::warn!("failed to set delay for '{node_name}': {e}"),
            }
        }
        false
    }

    /// Snapshot of the desired per-device delays by node name (for the UI).
    pub fn delays(&self) -> HashMap<String, u16> {
        self.desired_delay.clone()
    }

    /// Drop a disconnected device (its desired volume is kept for reconnect).
    pub fn unregister(&mut self, node_name: &str) {
        self.senders.remove(node_name);
    }

    /// Set a device's desired volume and push it to the device if connected.
    /// Returns true if it reached a live device (false = stored for reconnect).
    pub async fn set_volume(&mut self, node_name: &str, volume: u8) -> bool {
        let volume = volume.min(100);
        let changed = self.desired.insert(node_name.to_string(), volume) != Some(volume);
        if changed {
            // Push to any other watching UI (the routing WS); the caller already
            // has the value optimistically, so this is for the second client.
            self.notify_changed();
        }
        if let Some(sender) = self.senders.get(node_name) {
            match sender.send_player_command(volume_cmd(volume)).await {
                Ok(()) => return true,
                Err(e) => tracing::warn!("failed to set volume for '{node_name}': {e}"),
            }
        }
        false
    }

    /// Snapshot of the desired volumes by node name (for the UI sliders).
    pub fn volumes(&self) -> HashMap<String, u8> {
        self.desired.clone()
    }

    /// Set a device's desired mute state (persisted, re-applied on reconnect) and
    /// push it via the dedicated `Mute` command — so unmuting restores the prior
    /// volume. Returns true if it reached a live device. Unlike the transient
    /// [`Self::set_mute`] (alignment wizard), this is the user-facing mute the UI
    /// toggle drives and the matrix reflects.
    pub async fn set_muted(&mut self, node_name: &str, muted: bool) -> bool {
        let changed = self.desired_muted.insert(node_name.to_string(), muted) != Some(muted);
        if changed {
            self.notify_changed();
        }
        if let Some(sender) = self.senders.get(node_name) {
            match sender.send_player_command(mute_cmd(muted)).await {
                Ok(()) => return true,
                Err(e) => tracing::warn!("failed to set mute for '{node_name}': {e}"),
            }
        }
        false
    }

    /// Record a **device-reported** mute state (from an inbound `client/state`
    /// player update) without echoing a command back — the device→UI half of
    /// mute sync, mirroring [`Self::note_reported_volume`]. No-op when unchanged.
    pub fn note_reported_mute(&mut self, node_name: &str, muted: bool) {
        if self.desired_muted.get(node_name) == Some(&muted) {
            return;
        }
        tracing::debug!("sendspin device '{node_name}' reported muted={muted}");
        self.desired_muted.insert(node_name.to_string(), muted);
        self.notify_changed();
    }

    /// Snapshot of the desired mute state by node name (for the UI).
    pub fn mutes(&self) -> HashMap<String, bool> {
        self.desired_muted.clone()
    }

    /// Push a **transient** mute/unmute to a device if connected, using the
    /// protocol's dedicated `Mute` command (not volume 0 — conforming devices
    /// keep volume and mute independent, and some ignore a 0 volume). Does NOT
    /// touch the stored desired volume, so unmuting restores the prior level.
    /// Used by the alignment wizard to solo the reference + target speaker.
    /// Returns true if it reached a live device.
    pub async fn set_mute(&self, node_name: &str, muted: bool) -> bool {
        if let Some(sender) = self.senders.get(node_name) {
            match sender.send_player_command(mute_cmd(muted)).await {
                Ok(()) => return true,
                Err(e) => tracing::warn!("failed to set mute for '{node_name}': {e}"),
            }
        }
        false
    }

    /// Whether a device currently has a live server connection — the
    /// connection-driven liveness signal (sendspin_liveness.rs). A connected
    /// device is unambiguously present without needing an active probe.
    pub fn is_connected(&self, node_name: &str) -> bool {
        self.senders.contains_key(node_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_volume_persists_desired_even_with_no_live_device() {
        let mut c = SendspinControl::default();
        assert!(!c.set_volume("sendspin-dev-kitchen", 40).await);
        assert_eq!(c.volumes().get("sendspin-dev-kitchen").copied(), Some(40));
    }

    #[test]
    fn note_reported_volume_updates_desired_and_clamps() {
        let mut c = SendspinControl::default();
        c.note_reported_volume("sendspin-dev-kitchen", 42);
        assert_eq!(c.volumes().get("sendspin-dev-kitchen").copied(), Some(42));
        // A device→UI report is the same channel the UI reads, so the reported
        // level becomes the desired level; out-of-range is clamped like set_volume.
        c.note_reported_volume("sendspin-dev-kitchen", 200);
        assert_eq!(c.volumes().get("sendspin-dev-kitchen").copied(), Some(100));
    }
}
