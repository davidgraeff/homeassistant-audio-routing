//! Persistent, runtime-managed config for the AirPlay-receive source name (the
//! embedded native receiver, airplay_source.rs) and the Bluetooth-bridge RTP
//! source. Mirrors outputs_store.rs: no `options.json` seeding — starts empty
//! on a fresh install, then the `/data` file is authoritative and everything is
//! managed live via the API (api.rs).
//!
//! (Sendspin is no longer configured here: devices are auto-discovered
//! (sendspin_discovery.rs) and grouped from the routing intent
//! (sendspin_group.rs), so there's nothing per-output to persist.)

use crate::rtp_source::DEFAULT_RTP_PORT;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The single RTP source (Bluetooth bridge firmware target). Its presence in
/// the store means "enabled"; the only knob is the UDP port it listens on (the
/// rest of the wire format is fixed by the firmware — see rtp_source.rs).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RtpSourceConfig {
    #[serde(default = "default_rtp_port")]
    pub port: u16,
}

fn default_rtp_port() -> u16 {
    DEFAULT_RTP_PORT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourcesConfig {
    /// AirPlay-receive service name; `None` (or empty) = disabled.
    #[serde(default)]
    airplay_source_name: Option<String>,
    /// RTP source (Bluetooth bridge firmware target); `None` = disabled.
    #[serde(default)]
    rtp_source: Option<RtpSourceConfig>,
}

pub struct SourcesStore {
    path: PathBuf,
    config: SourcesConfig,
}

/// Empty/whitespace AirPlay name means "disabled" everywhere — normalize to
/// `None` so the rest of the code only deals with `Some(real name)`.
fn normalize_name(name: Option<String>) -> Option<String> {
    name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl SourcesStore {
    /// Load from `path`, or start empty if it doesn't exist yet (the file is
    /// created on the first mutation). No `options.json` seeding.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading sources store {}: {e}", path.display()))?;
            let config: SourcesConfig = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("parsing sources store {}: {e}", path.display()))?;
            Ok(Self { path: path.to_path_buf(), config })
        } else {
            let config = SourcesConfig { airplay_source_name: None, rtp_source: None };
            Ok(Self { path: path.to_path_buf(), config })
        }
    }

    pub fn airplay_source_name(&self) -> Option<&str> {
        self.config.airplay_source_name.as_deref()
    }

    /// Set (or clear, with `None`/empty) the AirPlay source name and persist.
    pub fn set_airplay_source_name(&mut self, name: Option<String>) -> anyhow::Result<()> {
        self.config.airplay_source_name = normalize_name(name);
        self.persist()
    }

    /// The stored RTP source config, or `None` when the source is disabled.
    pub fn rtp_source(&self) -> Option<RtpSourceConfig> {
        self.config.rtp_source
    }

    /// Set (or clear, with `None`) the RTP source config and persist.
    pub fn set_rtp_source(&mut self, cfg: Option<RtpSourceConfig>) -> anyhow::Result<()> {
        self.config.rtp_source = cfg;
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, json)
            .map_err(|e| anyhow::anyhow!("writing sources store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sources-store-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn starts_empty_when_no_file() {
        let path = temp_path("empty");
        let _ = std::fs::remove_file(&path);
        let store = SourcesStore::load(&path).unwrap();
        assert_eq!(store.airplay_source_name(), None);
        assert_eq!(store.rtp_source(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn airplay_set_normalizes_and_persists_across_reload() {
        let path = temp_path("airplay");
        let _ = std::fs::remove_file(&path);
        let mut store = SourcesStore::load(&path).unwrap();
        store.set_airplay_source_name(Some("  Living Room ".to_string())).unwrap();
        assert_eq!(store.airplay_source_name(), Some("Living Room")); // trimmed
        // Persisted across a reload.
        assert_eq!(SourcesStore::load(&path).unwrap().airplay_source_name(), Some("Living Room"));
        // Empty/whitespace disables.
        store.set_airplay_source_name(Some("   ".to_string())).unwrap();
        assert_eq!(store.airplay_source_name(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rtp_source_set_clear_and_persist_across_reload() {
        let path = temp_path("rtp");
        let _ = std::fs::remove_file(&path);
        let mut store = SourcesStore::load(&path).unwrap();
        assert_eq!(store.rtp_source(), None); // disabled by default
        store.set_rtp_source(Some(RtpSourceConfig { port: 46000 })).unwrap();
        assert_eq!(store.rtp_source().map(|c| c.port), Some(46000));
        // Persisted across a reload.
        assert_eq!(SourcesStore::load(&path).unwrap().rtp_source().map(|c| c.port), Some(46000));
        // Cleared = disabled.
        store.set_rtp_source(None).unwrap();
        assert_eq!(store.rtp_source(), None);
        let _ = std::fs::remove_file(&path);
    }
}
