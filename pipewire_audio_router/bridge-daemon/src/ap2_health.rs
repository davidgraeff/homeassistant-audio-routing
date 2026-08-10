//! Why an AirPlay-2 output isn't playing — the one place the UI can read it from.
//!
//! Before this existed, a receiver that refused every connection produced nothing a
//! user could see: `/api/outputs` showed it `present: true`, the matrix drew it
//! green, and the only trace was a `WARN` line in the daemon log. The two places
//! that *learn* a receiver is unusable are far apart — [`crate::ap2_liveness`] (the
//! periodic probe) and [`crate::ap2_server`] (a failed connect inside a group's
//! spawned task) — and neither is on a path that already reaches the API state.
//! Rather than thread a handle through `sync_group`'s three `ap2_server::start`
//! call sites, both report here, mirroring [`crate::overlay_mixer`]'s
//! process-global.
//!
//! Deliberately not persisted and not part of `Ap2Device`: this is a runtime
//! verdict about *now*, while `Ap2Device` holds mDNS facts. It is cleared the
//! moment the receiver works again.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Last known fault per AP2 output node name. Absent = nothing wrong (or nothing
/// tried yet).
#[derive(Default)]
pub struct Ap2Health {
    faults: Mutex<HashMap<String, String>>,
}

impl Ap2Health {
    /// The process-global registry.
    pub fn global() -> &'static Ap2Health {
        static H: OnceLock<Ap2Health> = OnceLock::new();
        H.get_or_init(Ap2Health::default)
    }

    /// Record why `node_name` is unusable. Returns `true` if this changed the
    /// stored message, so the caller can decide whether to nudge the UI (and avoid
    /// re-logging the same fault every probe tick).
    pub fn set(&self, node_name: &str, message: impl Into<String>) -> bool {
        let message = message.into();
        let mut f = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        match f.get(node_name) {
            Some(existing) if *existing == message => false,
            _ => {
                f.insert(node_name.to_string(), message);
                true
            }
        }
    }

    /// Forget any fault for `node_name` (it works again). Returns `true` if there
    /// was one to clear.
    pub fn clear(&self, node_name: &str) -> bool {
        self.faults.lock().unwrap_or_else(|e| e.into_inner()).remove(node_name).is_some()
    }

    /// The current fault message for `node_name`, if any.
    pub fn get(&self, node_name: &str) -> Option<String> {
        self.faults.lock().unwrap_or_else(|e| e.into_inner()).get(node_name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses its own instance, not `global()`, so tests don't share state.
    #[test]
    fn set_clear_and_change_detection() {
        let h = Ap2Health::default();
        assert!(h.get("a").is_none());
        assert!(h.set("a", "wedged"));
        // Same message again is not a change — callers use this to log/notify once.
        assert!(!h.set("a", "wedged"));
        assert!(h.set("a", "unreachable"));
        assert_eq!(h.get("a").unwrap(), "unreachable");
        assert!(h.clear("a"));
        assert!(!h.clear("a"));
        assert!(h.get("a").is_none());
    }
}
