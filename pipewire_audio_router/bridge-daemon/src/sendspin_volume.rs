//! Per-device volume control for sendspin (multi-room) speakers.
//!
//! Unlike AirPlay outputs — real PipeWire `raop-sink` nodes whose volume the
//! matrix drives through `node_id` — sendspin devices are *virtual* outputs fed
//! by a shared group sink (sendspin_group.rs), so there's no per-device
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
    desired: HashMap<String, u8>,
}

fn volume_cmd(volume: u8) -> PlayerCommand {
    PlayerCommand { command: PlayerCommandType::Volume, volume: Some(volume.min(100)), mute: None, static_delay_ms: None }
}

/// Construct an empty control wrapped for sharing.
pub fn shared() -> SharedSendspinControl {
    Arc::new(Mutex::new(SendspinControl::default()))
}

impl SendspinControl {
    /// Register a freshly-connected device (by its virtual node name) and
    /// (re)apply its stored volume so a reconnect restores what the user set.
    pub async fn register(&mut self, node_name: String, sender: ServerSender) {
        tracing::info!("sendspin device connected: {node_name}");
        if let Some(&vol) = self.desired.get(&node_name) {
            if let Err(e) = sender.send_player_command(volume_cmd(vol)).await {
                tracing::warn!("failed to apply stored volume {vol} to '{node_name}': {e}");
            }
        }
        self.senders.insert(node_name, sender);
    }

    /// Drop a disconnected device (its desired volume is kept for reconnect).
    pub fn unregister(&mut self, node_name: &str) {
        self.senders.remove(node_name);
    }

    /// Set a device's desired volume and push it to the device if connected.
    /// Returns true if it reached a live device (false = stored for reconnect).
    pub async fn set_volume(&mut self, node_name: &str, volume: u8) -> bool {
        let volume = volume.min(100);
        self.desired.insert(node_name.to_string(), volume);
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
}
