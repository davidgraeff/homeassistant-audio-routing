//! Persistent latency/sync tuning knobs for the grouping reconciler
//! (sync_group.rs) — the user-facing dials for "make the group as snappy as
//! possible, but keep everyone playing together".
//!
//! Two things live here, both keyed by stable name so they survive restarts,
//! node reloads and device churn (same rationale as routing_store.rs):
//!
//! - **`group_lead_ms`** — the sendspin group's presentation lead
//!   ([`sendspin::server::Group::with_send_ahead_us`]). It's how far ahead of
//!   "now" audio is scheduled; every member must be able to buffer within it.
//!   Raise it so a slower member (e.g. a RAOP receiver sharing the group's
//!   anchor, or a distant speaker) can still play the same instant; lower it
//!   for snappier start. One value for the whole daemon — the protocol itself
//!   only supports one lead per group.
//! - **`sendspin_delays`** — a per-device *static* delay in ms
//!   (`PlayerCommandType::SetStaticDelay`), for trimming an individual speaker
//!   that's consistently early/late relative to the rest of its group. Applied
//!   in-band by the sendspin server on (re)connect (sendspin_volume.rs).
//! - **`raop_latency`** — the RAOP counterpart of `sendspin_delays`: a per-output
//!   `raop.latency.ms` in ms, keyed by RAOP node name. It's a module-load
//!   argument (not a live push), so it's applied when the `raop-sink` is loaded —
//!   for **both** manually-added and mDNS-discovered receivers, keyed by the same
//!   stable node name so a device's calibration survives it going offline and
//!   being rediscovered. RAOP's own default is a hefty 1500 ms; lower it toward a
//!   group's other members for a snappier start.
//!
//! Mirrors the other `/data` stores: no `options.json` seeding, the file is
//! authoritative and created on first mutation; a missing file means defaults.

use crate::locks::LockRecover;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Default sendspin group lead — the sendspin protocol's own default
/// ([`sendspin::server::group::DEFAULT_SEND_AHEAD_US`] = 250 000 µs).
pub const DEFAULT_GROUP_LEAD_MS: u32 = 250;

fn default_group_lead_ms() -> u32 {
    DEFAULT_GROUP_LEAD_MS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncConfig {
    #[serde(default = "default_group_lead_ms")]
    group_lead_ms: u32,
    /// Per-sendspin-device static delay (ms), keyed by virtual device node name.
    #[serde(default)]
    sendspin_delays: BTreeMap<String, u16>,
    /// Per-RAOP-output `raop.latency.ms` (ms), keyed by RAOP node name.
    #[serde(default)]
    raop_latency: BTreeMap<String, u16>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { group_lead_ms: DEFAULT_GROUP_LEAD_MS, sendspin_delays: BTreeMap::new(), raop_latency: BTreeMap::new() }
    }
}

pub struct SyncSettings {
    path: PathBuf,
    config: SyncConfig,
}

impl SyncSettings {
    /// Load from `path`, or start with defaults if it doesn't exist yet (created
    /// on the first mutation).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading sync settings {}: {e}", path.display()))?;
            let config: SyncConfig =
                serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing sync settings {}: {e}", path.display()))?;
            Ok(Self { path: path.to_path_buf(), config })
        } else {
            Ok(Self { path: path.to_path_buf(), config: SyncConfig::default() })
        }
    }

    pub fn group_lead_ms(&self) -> u32 {
        self.config.group_lead_ms
    }

    /// The group lead in microseconds, ready for [`sendspin::server::Group::with_send_ahead_us`].
    pub fn group_lead_us(&self) -> i64 {
        i64::from(self.config.group_lead_ms) * 1000
    }

    pub fn set_group_lead_ms(&mut self, ms: u32) -> anyhow::Result<()> {
        self.config.group_lead_ms = ms;
        self.persist()
    }

    /// Desired per-device static delays (ms) by node name.
    pub fn sendspin_delays(&self) -> BTreeMap<String, u16> {
        self.config.sendspin_delays.clone()
    }

    /// Set (or clear, when `ms` is 0) a device's static delay and persist.
    pub fn set_sendspin_delay(&mut self, node_name: &str, ms: u16) -> anyhow::Result<()> {
        if ms == 0 {
            self.config.sendspin_delays.remove(node_name);
        } else {
            self.config.sendspin_delays.insert(node_name.to_string(), ms);
        }
        self.persist()
    }

    /// The configured `raop.latency.ms` for a RAOP node, if any (`None` = the
    /// module default).
    pub fn raop_latency(&self, node_name: &str) -> Option<u16> {
        self.config.raop_latency.get(node_name).copied()
    }

    /// Desired per-output RAOP latencies (ms) by node name.
    pub fn raop_latencies(&self) -> BTreeMap<String, u16> {
        self.config.raop_latency.clone()
    }

    /// Set (or clear, when `ms` is `None`) a RAOP output's latency and persist.
    pub fn set_raop_latency(&mut self, node_name: &str, ms: Option<u16>) -> anyhow::Result<()> {
        match ms {
            None => {
                self.config.raop_latency.remove(node_name);
            }
            Some(ms) => {
                self.config.raop_latency.insert(node_name.to_string(), ms);
            }
        }
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, json).map_err(|e| anyhow::anyhow!("writing sync settings {}: {e}", self.path.display()))?;
        Ok(())
    }
}

/// Shared handle used across the API and the reconcile task.
pub type SharedSyncSettings = Arc<Mutex<SyncSettings>>;

/// Convenience: lock and read the current group lead in microseconds.
pub fn group_lead_us(settings: &SharedSyncSettings) -> i64 {
    settings.lock_recover().group_lead_us()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sync-settings-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn defaults_when_absent_and_persists_across_reload() {
        let path = temp_path("basic");
        let _ = std::fs::remove_file(&path);
        let mut s = SyncSettings::load(&path).unwrap();
        assert_eq!(s.group_lead_ms(), DEFAULT_GROUP_LEAD_MS);
        assert_eq!(s.group_lead_us(), 250_000);
        s.set_group_lead_ms(600).unwrap();
        s.set_sendspin_delay("sendspin-dev-kitchen", 40).unwrap();

        let reloaded = SyncSettings::load(&path).unwrap();
        assert_eq!(reloaded.group_lead_ms(), 600);
        assert_eq!(reloaded.sendspin_delays().get("sendspin-dev-kitchen").copied(), Some(40));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zero_delay_clears_entry() {
        let path = temp_path("clear");
        let _ = std::fs::remove_file(&path);
        let mut s = SyncSettings::load(&path).unwrap();
        s.set_sendspin_delay("sendspin-dev-bath", 30).unwrap();
        assert_eq!(s.sendspin_delays().len(), 1);
        s.set_sendspin_delay("sendspin-dev-bath", 0).unwrap();
        assert!(s.sendspin_delays().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
