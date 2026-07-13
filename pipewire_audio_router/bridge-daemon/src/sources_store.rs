//! Persistent, runtime-managed config for the daemon-supervised AirPlay-receive
//! source (`shairport-sync`, via `supervisor.rs`) and the sendspin outputs (an
//! embedded native server per output, via `sendspin_server.rs` — no subprocess
//! at all, see docs/decisions.md). Mirrors outputs_store.rs: no `options.json`
//! seeding — starts empty on a fresh install, then the `/data` file is
//! authoritative and everything is managed live via the API (api.rs).

use crate::config::{slugify, SENDSPIN_NODE_PREFIX};
use crate::rtp_source::DEFAULT_RTP_PORT;
use crate::supervisor::ProcessSpec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Supervisor key for the single AirPlay-receive source.
pub const AIRPLAY_KEY: &str = "airplay";

const SHAIRPORT_SYNC_BIN: &str = "shairport-sync";
const DEFAULT_SENDSPIN_BASE_PORT: u16 = 8927;

/// The PipeWire node name for a sendspin output (also its native server
/// handle's map key — see sendspin_server.rs).
pub fn sendspin_node_name(name: &str) -> String {
    format!("{SENDSPIN_NODE_PREFIX}{}", slugify(name))
}

/// How to spawn `shairport-sync` for an AirPlay-receive source of the given
/// advertised name.
pub fn airplay_spec(name: &str) -> ProcessSpec {
    ProcessSpec {
        program: SHAIRPORT_SYNC_BIN.to_string(),
        args: vec!["-a".to_string(), name.to_string()],
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendspinOutput {
    pub name: String,
    pub port: u16,
}

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

impl SendspinOutput {
    pub fn node_name(&self) -> String {
        sendspin_node_name(&self.name)
    }
}

fn default_base_port() -> u16 {
    DEFAULT_SENDSPIN_BASE_PORT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourcesConfig {
    /// AirPlay-receive service name; `None` (or empty) = disabled.
    #[serde(default)]
    airplay_source_name: Option<String>,
    #[serde(default)]
    sendspin_outputs: Vec<SendspinOutput>,
    /// Base port new sendspin outputs are allocated from.
    #[serde(default = "default_base_port")]
    sendspin_base_port: u16,
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
            let config = SourcesConfig {
                airplay_source_name: None,
                sendspin_outputs: Vec::new(),
                sendspin_base_port: default_base_port(),
                rtp_source: None,
            };
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

    pub fn sendspin_outputs(&self) -> &[SendspinOutput] {
        &self.config.sendspin_outputs
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

    pub fn contains_sendspin(&self, node_name: &str) -> bool {
        self.config.sendspin_outputs.iter().any(|o| o.node_name() == node_name)
    }

    /// Add a sendspin output (deduped by node name), allocating the lowest free
    /// port at/above the base. Persists and returns the created output.
    pub fn add_sendspin(&mut self, name: &str) -> anyhow::Result<SendspinOutput> {
        let node_name = sendspin_node_name(name);
        if self.contains_sendspin(&node_name) {
            anyhow::bail!("a sendspin output named '{name}' (node {node_name}) already exists");
        }
        let used: std::collections::HashSet<u16> = self.config.sendspin_outputs.iter().map(|o| o.port).collect();
        let base = if self.config.sendspin_base_port == 0 { default_base_port() } else { self.config.sendspin_base_port };
        let mut port = base;
        while used.contains(&port) {
            port += 1;
        }
        let output = SendspinOutput { name: name.to_string(), port };
        self.config.sendspin_outputs.push(output.clone());
        self.persist()?;
        Ok(output)
    }

    /// Remove a sendspin output by node name. Returns the removed config, or
    /// `None` if there was no such output.
    pub fn remove_sendspin(&mut self, node_name: &str) -> anyhow::Result<Option<SendspinOutput>> {
        match self.config.sendspin_outputs.iter().position(|o| o.node_name() == node_name) {
            Some(i) => {
                let removed = self.config.sendspin_outputs.remove(i);
                self.persist()?;
                Ok(Some(removed))
            }
            None => Ok(None),
        }
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
        assert!(store.sendspin_outputs().is_empty());
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

    #[test]
    fn sendspin_add_dedupes_and_allocates_ports() {
        let path = temp_path("sendspin");
        let _ = std::fs::remove_file(&path);
        let mut store = SourcesStore::load(&path).unwrap();
        let a = store.add_sendspin("Bedroom").unwrap();
        let b = store.add_sendspin("Bath").unwrap();
        assert_eq!(a.port, 8927);
        assert_eq!(b.port, 8928);
        assert!(store.add_sendspin("bedroom").is_err()); // same slug
        assert!(store.remove_sendspin("sendspin-out-bedroom").unwrap().is_some());
        assert!(store.remove_sendspin("sendspin-out-bedroom").unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }
}
