//! Process-global **pw-sink liveness registry** — the handshake-driven "is a
//! receiver actually streaming?" state for each routed pw-sink target.
//!
//! Unlike sendspin/AP2 (where the daemon dials the receiver, so a failed connect
//! is observable directly), a pw-sink target's `module-rtp-session` *initiates*
//! the AppleMIDI handshake to the session the daemon advertises. So "present on
//! mDNS" (pw_target_discovery.rs) is not the same as "connected and playing" —
//! the truth lives in each [`crate::applemidi_sender::AppleMidiSender`]'s
//! `status()`. The per-group sender task (pwsink_server.rs) polls that status and
//! publishes it here, keyed by output node name; the API (`/api/outputs`) reads
//! it so the UI can show a target as present-but-not-yet-connected vs streaming.
//!
//! Mirrors overlay_mixer.rs's global-singleton shape (a `OnceLock` behind a
//! mutex): cheap, lock-guarded, no per-call allocation.
//!
//! A status *change* also nudges the routing-matrix WebSocket (via the notifier
//! main.rs installs, like sendspin_volume.rs's): the matrix reports this as each
//! output's `streaming`, and the graph decides from it whether a wire is really
//! carrying audio — so a handshake completing has to push a frame, not wait for
//! some unrelated registry event.

use crate::pw_thread::ChangeNotifier;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// One target's live handshake status.
#[derive(Clone, Copy, Debug, Default)]
pub struct PwSinkStatus {
    /// A receiver has completed the AppleMIDI handshake and is being streamed to.
    pub established: bool,
    /// Number of receivers currently in an established session with this target's
    /// advertised pw-sink session. Populated for diagnostics/logging; the API
    /// currently surfaces only `established`.
    #[allow(dead_code)]
    pub peer_count: usize,
}

#[derive(Default)]
pub struct PwSinkLiveness {
    map: Mutex<BTreeMap<String, PwSinkStatus>>,
    /// Pushes a routing-matrix frame when `established` flips. Installed once at
    /// startup; `None` in tests and before wiring.
    notifier: Mutex<Option<ChangeNotifier>>,
}

impl PwSinkLiveness {
    /// The process-global registry.
    pub fn global() -> &'static PwSinkLiveness {
        static L: OnceLock<PwSinkLiveness> = OnceLock::new();
        L.get_or_init(PwSinkLiveness::default)
    }

    /// Install the notifier nudged when a target's `established` changes.
    pub fn set_change_notifier(&self, changes: ChangeNotifier) {
        *self.notifier.lock().unwrap() = Some(changes);
    }

    /// Publish (or update) a target's status. The status task polls every second,
    /// so only a *change* notifies — otherwise this would push a matrix frame per
    /// target per second.
    pub fn set(&self, node_name: &str, status: PwSinkStatus) {
        let prev = self.map.lock().unwrap().insert(node_name.to_string(), status);
        if prev.map(|p| p.established) != Some(status.established) {
            self.notify_changed();
        }
    }

    /// Forget a target (on group teardown / sender stop).
    pub fn remove(&self, node_name: &str) {
        if let Some(prev) = self.map.lock().unwrap().remove(node_name) {
            if prev.established {
                self.notify_changed();
            }
        }
    }

    fn notify_changed(&self) {
        if let Some(changes) = self.notifier.lock().unwrap().as_ref() {
            let _ = changes.send(());
        }
    }

    /// Current status for a target, or `None` if no sender is running for it
    /// (the API reports that as present-but-not-connected).
    pub fn get(&self, node_name: &str) -> Option<PwSinkStatus> {
        self.map.lock().unwrap().get(node_name).copied()
    }
}
