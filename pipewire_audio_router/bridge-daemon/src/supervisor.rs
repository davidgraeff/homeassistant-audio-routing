//! Supervises the external child processes the daemon owns: `shairport-sync`
//! (the AirPlay-receive source) and `sendspin-adapter.py` (one per sendspin
//! output). Unlike RAOP outputs — which are native PipeWire modules loaded
//! in-process (pw_module.rs) — these are separate programs with no in-daemon
//! equivalent, so the daemon spawns them from its persisted config
//! (sources_store.rs) at startup and on API changes, and kills them on removal
//! or shutdown. This replaces `run.sh`'s boot-time `runtime-plan` spawn loop
//! (the design intended all along — see sendspin-adapter/adapter.py, Section
//! 5.5).
//!
//! A crashed child is reported as not-running rather than auto-restarted or
//! taken as fatal — consistent with RAOP outputs' `nofail` stance ("one bad
//! component mustn't take everything down"); the user can re-enable it via the
//! API. The daemon exiting still restarts the container (run.sh `wait -n`).

use std::collections::HashMap;
use tokio::process::{Child, Command};

/// What to run for one supervised process.
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// Owns the live child processes, keyed by a stable id (`"airplay"` for the
/// source, the sendspin node name for each adapter).
#[derive(Default)]
pub struct Supervisor {
    children: HashMap<String, Child>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re)spawns the process under `key`, killing any existing one first so a
    /// config change (e.g. a renamed AirPlay source) cleanly replaces it.
    pub async fn respawn(&mut self, key: &str, spec: &ProcessSpec) -> Result<(), String> {
        self.stop(key).await;
        let child = Command::new(&spec.program)
            .args(&spec.args)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", spec.program))?;
        self.children.insert(key.to_string(), child);
        Ok(())
    }

    /// Kills and reaps the process under `key` if present. Idempotent.
    pub async fn stop(&mut self, key: &str) {
        if let Some(mut child) = self.children.remove(key) {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// Kills and reaps every supervised process — used on daemon shutdown.
    pub async fn stop_all(&mut self) {
        for key in self.children.keys().cloned().collect::<Vec<_>>() {
            self.stop(&key).await;
        }
    }

    /// Best-effort liveness for `key`: whether we hold a child for it that
    /// hasn't exited. Reaps and forgets one that has exited.
    pub fn is_running(&mut self, key: &str) -> bool {
        match self.children.get_mut(key) {
            Some(child) => match child.try_wait() {
                Ok(None) => true,
                _ => {
                    self.children.remove(key);
                    false
                }
            },
            None => false,
        }
    }
}
