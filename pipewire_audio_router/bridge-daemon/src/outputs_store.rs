//! Persistent, runtime-managed set of RAOP outputs — the source of truth for
//! what the daemon loads, replacing the old static `pipewire.conf.d`
//! generation.
//!
//! Managed live via the CRUD API (api.rs) and persisted to a JSON file under
//! `/data` so it survives restarts. There is no `options.json` seeding: the
//! store starts empty on a fresh install and is populated entirely through the
//! API / web UI (and, for RAOP, mDNS auto-discovery).

use crate::config::RaopOutputConfig;
use crate::raop::raop_node_name;
use std::path::{Path, PathBuf};

pub struct OutputsStore {
    path: PathBuf,
    outputs: Vec<RaopOutputConfig>,
}

impl OutputsStore {
    /// Load the store from `path`, or start empty if it doesn't exist yet (the
    /// file is created on the first `add`).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading outputs store {}: {e}", path.display()))?;
            let outputs: Vec<RaopOutputConfig> = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("parsing outputs store {}: {e}", path.display()))?;
            Ok(Self { path: path.to_path_buf(), outputs })
        } else {
            Ok(Self { path: path.to_path_buf(), outputs: Vec::new() })
        }
    }

    pub fn list(&self) -> &[RaopOutputConfig] {
        &self.outputs
    }

    /// Whether an output with the given RAOP node name is already stored.
    pub fn contains(&self, node_name: &str) -> bool {
        self.outputs.iter().any(|o| raop_node_name(&o.name) == node_name)
    }

    /// Add an output and persist. Errors if one with the same node name (i.e.
    /// the same slugified display name) already exists — node names must be
    /// unique, they key everything downstream (registry lookup, links, HA
    /// entity ids).
    pub fn add(&mut self, output: RaopOutputConfig) -> anyhow::Result<()> {
        let node_name = raop_node_name(&output.name);
        if self.contains(&node_name) {
            anyhow::bail!("an output named '{}' (node {node_name}) already exists", output.name);
        }
        self.outputs.push(output);
        self.persist()
    }

    /// Remove the output with the given node name and persist. Returns the
    /// removed config, or `None` if there was no such output.
    pub fn remove(&mut self, node_name: &str) -> anyhow::Result<Option<RaopOutputConfig>> {
        match self.outputs.iter().position(|o| raop_node_name(&o.name) == node_name) {
            Some(i) => {
                let removed = self.outputs.remove(i);
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
        let json = serde_json::to_string_pretty(&self.outputs)?;
        std::fs::write(&self.path, json)
            .map_err(|e| anyhow::anyhow!("writing outputs store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RaopEncryption, RaopOutputConfig};

    fn output(name: &str, ip: &str) -> RaopOutputConfig {
        RaopOutputConfig {
            name: name.to_string(),
            ip: ip.to_string(),
            port: 7000,
            encryption: RaopEncryption::AuthSetup,
        }
    }

    #[test]
    fn starts_empty_when_no_file_then_persists_across_reload() {
        let dir = std::env::temp_dir().join(format!("outputs-store-test-{}", std::process::id()));
        let path = dir.join("raop-outputs.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = OutputsStore::load(&path).unwrap();
        assert!(store.list().is_empty(), "fresh install starts empty — no seeding");
        store.add(output("Pioneer VSX-934", "192.168.178.35")).unwrap();

        let reloaded = OutputsStore::load(&path).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].name, "Pioneer VSX-934");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_is_deduped_by_node_name_and_remove_round_trips() {
        let dir = std::env::temp_dir().join(format!("outputs-store-test-add-{}", std::process::id()));
        let path = dir.join("raop-outputs.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = OutputsStore::load(&path).unwrap();
        store.add(output("Kitchen", "192.168.1.10")).unwrap();
        assert!(store.contains("raop-out-kitchen"));

        // Same slug -> rejected.
        assert!(store.add(output("kitchen", "192.168.1.11")).is_err());
        assert_eq!(store.list().len(), 1);

        // Persisted across a reload.
        let reloaded = OutputsStore::load(&path).unwrap();
        assert!(reloaded.contains("raop-out-kitchen"));

        let removed = store.remove("raop-out-kitchen").unwrap();
        assert!(removed.is_some());
        assert!(store.list().is_empty());
        assert!(store.remove("raop-out-kitchen").unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
