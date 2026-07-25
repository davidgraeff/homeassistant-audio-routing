//! General, daemon-wide app settings — the "Settings" page's backing store.
//!
//! Mirrors the other `/data` stores (sources_store.rs, sync_settings.rs): no
//! `options.json` seeding, every field has a `serde(default)` so an older/newer
//! config file still loads, and the file is authoritative once written (created
//! on the first mutation, or seeded on a fresh install — see main.rs).
//!
//! What lives here are genuinely *global* knobs that don't belong to a single
//! output/source (those stay per-entity in outputs_store/sources_store) and
//! aren't sync-specific (the group lead + per-device delays stay in
//! sync_settings.rs):
//!
//! - **`default_duck`** — the level surviving sources are ducked to for an
//!   announce that doesn't specify one. The production announce path (HA's
//!   integration) always passes its own value, so this only affects callers
//!   that omit it — the Diagnostics test tool, mainly.
//! - **`discovery_enabled`** — runtime mDNS on/off, applied live by the
//!   discovery supervisor (discovery_supervisor.rs). Disabling stops
//!   discovering *new* devices; already-present ones age out normally (RAOP via
//!   the absent-grace, sendspin via liveness). The initial value is seeded from
//!   `BRIDGE_DISCOVERY` on a fresh install (main.rs), then this is authoritative.
//! - **`default_raop_latency_ms`** — stamped onto a newly-added RAOP output
//!   that doesn't specify its own latency, instead of leaving it at the module
//!   default (1500 ms). `None` = keep the module default.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Ducked-but-audible default: sources are ducked, not silenced, while an
/// announce plays (api.rs).
pub const DEFAULT_DUCK: f32 = 0.25;

fn default_duck() -> f32 {
    DEFAULT_DUCK
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    #[serde(default = "default_duck")]
    default_duck: f32,
    /// Runtime mDNS discovery on/off. `serde(default)` via `default_true` so old
    /// config files (which predate this) come up with discovery on.
    #[serde(default = "default_true")]
    discovery_enabled: bool,
    /// Default RAOP receiver latency (ms) for newly-added outputs; `None` keeps
    /// the PipeWire module default (1500 ms).
    #[serde(default)]
    default_raop_latency_ms: Option<u16>,
    /// Whether sendspin devices apply a static-delay change to the *running*
    /// stream. Current ESPHome firmware does NOT (it reads the delay only at
    /// stream start), so by default a delay change restarts the group stream —
    /// like a RAOP sink reload. Flip this on for future firmware that honors a
    /// live `SetStaticDelay`, to skip the restart. `serde(default)` = false.
    #[serde(default)]
    sendspin_delay_live: bool,
    /// Whether the HA integration should additionally expose every individual
    /// output as its own `media_player` entity. By default the integration
    /// creates one entity per music group and per announcement group; turning
    /// this on adds a per-output entity for directly addressing a single
    /// speaker regardless of its group. `serde(default)` = false.
    #[serde(default)]
    expose_outputs_as_media_players: bool,
    /// **Experimental (O-B):** run each sync group's sendspin devices as
    /// *per-device senders* sharing one timeline (sync_group + `start_server_per_device`)
    /// instead of one shared `Group`. Sync-preserving (validated in spike S1); the
    /// foundation for per-device duck/overlay. `serde(default)` = false.
    #[serde(default)]
    per_device_sendspin_senders: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { default_duck: DEFAULT_DUCK, discovery_enabled: true, default_raop_latency_ms: None, sendspin_delay_live: false, expose_outputs_as_media_players: false, per_device_sendspin_senders: false }
    }
}

pub struct SettingsStore {
    path: PathBuf,
    settings: Settings,
}

impl SettingsStore {
    /// Load from `path`, or start with defaults if it doesn't exist yet (created
    /// on the first mutation).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading settings store {}: {e}", path.display()))?;
            let settings: Settings =
                serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing settings store {}: {e}", path.display()))?;
            Ok(Self { path: path.to_path_buf(), settings })
        } else {
            Ok(Self { path: path.to_path_buf(), settings: Settings::default() })
        }
    }

    pub fn default_duck(&self) -> f32 {
        self.settings.default_duck
    }

    /// Set the announce default duck level (clamped to 0.0–1.0) and persist.
    pub fn set_default_duck(&mut self, duck: f32) -> anyhow::Result<()> {
        self.settings.default_duck = duck.clamp(0.0, 1.0);
        self.persist()
    }

    pub fn discovery_enabled(&self) -> bool {
        self.settings.discovery_enabled
    }

    /// Set the runtime discovery flag and persist.
    pub fn set_discovery_enabled(&mut self, enabled: bool) -> anyhow::Result<()> {
        self.settings.discovery_enabled = enabled;
        self.persist()
    }

    pub fn default_raop_latency_ms(&self) -> Option<u16> {
        self.settings.default_raop_latency_ms
    }

    /// Set (or clear, with `None`) the default RAOP latency for new outputs.
    pub fn set_default_raop_latency_ms(&mut self, ms: Option<u16>) -> anyhow::Result<()> {
        self.settings.default_raop_latency_ms = ms;
        self.persist()
    }

    pub fn sendspin_delay_live(&self) -> bool {
        self.settings.sendspin_delay_live
    }

    /// Set whether sendspin delay changes apply live (no stream restart).
    pub fn set_sendspin_delay_live(&mut self, live: bool) -> anyhow::Result<()> {
        self.settings.sendspin_delay_live = live;
        self.persist()
    }

    pub fn expose_outputs_as_media_players(&self) -> bool {
        self.settings.expose_outputs_as_media_players
    }

    /// Set whether the HA integration also exposes a per-output media_player.
    pub fn set_expose_outputs_as_media_players(&mut self, expose: bool) -> anyhow::Result<()> {
        self.settings.expose_outputs_as_media_players = expose;
        self.persist()
    }

    pub fn per_device_sendspin_senders(&self) -> bool {
        self.settings.per_device_sendspin_senders
    }

    /// Set the experimental per-device-senders mode for sync groups.
    pub fn set_per_device_sendspin_senders(&mut self, on: bool) -> anyhow::Result<()> {
        self.settings.per_device_sendspin_senders = on;
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.settings)?;
        std::fs::write(&self.path, json).map_err(|e| anyhow::anyhow!("writing settings store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

/// Shared handle used across the API and (for the discovery flag) startup.
pub type SharedSettings = std::sync::Arc<std::sync::Mutex<SettingsStore>>;

/// Read the experimental per-device-senders flag from the shared settings
/// (poison-safe; defaults to false if the lock is poisoned). Mirrors
/// `sync_settings::group_lead_us` so the reconcile task can read it each tick.
pub fn per_device_senders(settings: &SharedSettings) -> bool {
    settings.lock().map(|s| s.per_device_sendspin_senders()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("settings-store-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn defaults_when_absent() {
        let path = temp_path("empty");
        let _ = std::fs::remove_file(&path);
        let store = SettingsStore::load(&path).unwrap();
        assert_eq!(store.default_duck(), DEFAULT_DUCK);
        assert!(store.discovery_enabled());
        assert_eq!(store.default_raop_latency_ms(), None);
        assert!(!store.sendspin_delay_live());
        assert!(!store.expose_outputs_as_media_players());
        assert!(!store.per_device_sendspin_senders());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_clamps_and_persists_across_reload() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut store = SettingsStore::load(&path).unwrap();
        store.set_default_duck(1.5).unwrap(); // clamps to 1.0
        store.set_discovery_enabled(false).unwrap();
        store.set_default_raop_latency_ms(Some(400)).unwrap();
        store.set_sendspin_delay_live(true).unwrap();
        store.set_expose_outputs_as_media_players(true).unwrap();
        store.set_per_device_sendspin_senders(true).unwrap();

        let reloaded = SettingsStore::load(&path).unwrap();
        assert_eq!(reloaded.default_duck(), 1.0);
        assert!(!reloaded.discovery_enabled());
        assert_eq!(reloaded.default_raop_latency_ms(), Some(400));
        assert!(reloaded.sendspin_delay_live());
        assert!(reloaded.expose_outputs_as_media_players());
        assert!(reloaded.per_device_sendspin_senders());
        let _ = std::fs::remove_file(&path);
    }
}
