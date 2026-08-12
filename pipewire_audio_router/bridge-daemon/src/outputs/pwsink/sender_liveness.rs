//! Process-global **pw-sink liveness registry** — the handshake-driven "is a
//! receiver actually streaming?" state for each routed pw-sink target.
//!
//! Unlike sendspin/AP2 (where the daemon dials the receiver, so a failed connect
//! is observable directly), a pw-sink target's `module-rtp-session` *initiates*
//! the AppleMIDI handshake to the session the daemon advertises. So "present on
//! mDNS" (outputs/pwsink/discovery.rs) is not the same as "connected and playing" —
//! the truth lives in each [`crate::outputs::pwsink::applemidi::AppleMidiSender`]'s
//! `status()`. The per-group sender task (outputs/pwsink/server.rs) polls that status and
//! publishes it here, keyed by output node name; the API (`/api/outputs`) reads
//! it so the UI can show a target as present-but-not-yet-connected vs streaming.
//!
//! Mirrors outputs/overlay_mixer.rs's global-singleton shape (a `OnceLock` behind a
//! mutex): cheap, lock-guarded, no per-call allocation.
//!
//! A status *change* also nudges the routing-matrix WebSocket (via the notifier
//! main.rs installs, like outputs/sendspin/volume.rs's): the matrix reports this as each
//! output's `streaming`, and the graph decides from it whether a wire is really
//! carrying audio — so a handshake completing has to push a frame, not wait for
//! some unrelated registry event.

use crate::pw::thread::ChangeNotifier;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// Asks one target's receiver to rebuild its receive side — in practice a re-sent
/// `welcome` over that host's agent socket, which the agent answers by reloading
/// `module-rtp-session` (docs/receiver-agent.md §7.4).
///
/// A hook rather than a direct call because this module is reached from the sender's
/// own poll task, which knows nothing about agents; `main.rs` installs the one
/// implementation, exactly as it installs the change notifier above.
type RebuildHook = Box<dyn Fn(&str) + Send + Sync>;

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
    /// How a target's receiver is asked to re-handshake. `None` in tests, and in the
    /// spike binaries that start a sender without the agent half.
    rebuild: Mutex<Option<RebuildHook>>,
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

    /// Install the hook that asks a target's receiver to rebuild (§7.4).
    pub fn set_rebuild_hook(&self, hook: RebuildHook) {
        *self.rebuild.lock().unwrap() = Some(hook);
    }

    /// Ask this target's receiver to re-handshake with the session now advertised for
    /// it.
    ///
    /// **Why the daemon has to ask.** Stock `module-rtp-session` invites *once*, when it
    /// resolves the advertised service — and a resolve does not happen again for a
    /// service the resolver already knows, which after our restart is the *stale*
    /// record for a socket that no longer exists. So every fresh `AppleMidiSender` —
    /// at add-on start, and on every group rebuild (a routing change, an alignment hold,
    /// a retune) — waits for an invitation that will never come, and the target reads
    /// "not connected" indefinitely while its agent looks perfectly healthy. Measured
    /// live 2026-08-12: reloading the receiver took `established` from false to true
    /// within a second, on a session that had been dead for 40 minutes.
    ///
    /// The daemon cannot fix this from its own side: it has nothing on the target to
    /// poke (§1.1) and the handshake is receiver-initiated by construction. Asking the
    /// agent to reload its module is the only lever, and it is free — a target with no
    /// established peer is carrying no audio, so there is nothing to interrupt.
    pub fn request_rebuild(&self, node_name: &str) {
        let hook = self.rebuild.lock().unwrap();
        match hook.as_ref() {
            Some(hook) => hook(node_name),
            None => tracing::debug!("pw-sink: no rebuild hook installed; '{node_name}' must re-handshake on its own"),
        }
    }

    /// Current status for a target, or `None` if no sender is running for it
    /// (the API reports that as present-but-not-connected).
    pub fn get(&self, node_name: &str) -> Option<PwSinkStatus> {
        self.map.lock().unwrap().get(node_name).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // On a local instance, never `global()`: these install a hook, and the singleton is
    // shared with every other test in the process.
    #[test]
    fn a_rebuild_request_reaches_the_installed_hook() {
        let liveness = PwSinkLiveness::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = asked.clone();
        liveness.set_rebuild_hook(Box::new(move |node| sink.lock().unwrap().push(node.to_string())));
        liveness.request_rebuild("pwsink-dev-desk_david");
        liveness.request_rebuild("pwsink-dev-desk_david");
        assert_eq!(asked.lock().unwrap().len(), 2, "each ask is passed on; the caller owns the pacing");
        assert_eq!(asked.lock().unwrap()[0], "pwsink-dev-desk_david");
    }

    #[test]
    fn a_rebuild_request_with_no_hook_is_not_fatal() {
        // The spike binaries start a sender with no agent half at all, and a test can
        // reach `global()` before main.rs has wired anything.
        PwSinkLiveness::default().request_rebuild("pwsink-dev-desk_david");
    }
}
