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
}

impl PwSinkLiveness {
    /// The process-global registry.
    pub fn global() -> &'static PwSinkLiveness {
        static L: OnceLock<PwSinkLiveness> = OnceLock::new();
        L.get_or_init(PwSinkLiveness::default)
    }

    /// Publish (or update) a target's status.
    pub fn set(&self, node_name: &str, status: PwSinkStatus) {
        self.map.lock().unwrap().insert(node_name.to_string(), status);
    }

    /// Forget a target (on group teardown / sender stop).
    pub fn remove(&self, node_name: &str) {
        self.map.lock().unwrap().remove(node_name);
    }

    /// Current status for a target, or `None` if no sender is running for it
    /// (the API reports that as present-but-not-connected).
    pub fn get(&self, node_name: &str) -> Option<PwSinkStatus> {
        self.map.lock().unwrap().get(node_name).copied()
    }
}
