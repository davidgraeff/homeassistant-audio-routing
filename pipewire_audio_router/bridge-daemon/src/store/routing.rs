//! Persistent routing *intent*: the `(source, output)` links the user
//! configured, keyed by STABLE `node_name`.
//!
//! Raw PipeWire links are tied to ephemeral node ids and (even with
//! `object.linger`) don't survive a container restart or a node reload — so a
//! device that disappears and comes back loses its routing. This store records
//! the *desired* routing by stable name instead; the reconciler (routing.rs)
//! applies it to the live graph whenever both endpoints are present, and
//! reapplies automatically when an entity reappears. An intent link whose
//! endpoint isn't currently in the graph is "offline" — retained here, shown
//! grayed in the UI, and re-linked the moment the node returns.
//!
//! Mirrors the other `/data` stores (sources.rs): no `options.json`
//! seeding, starts empty, the file is authoritative and created on first
//! mutation.

use crate::util::locks::LockRecover;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One desired link, by stable node names. Ordered so the on-disk file is
/// deterministic (BTreeSet) and dedup is free.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoutingLink {
    /// Source node name (e.g. `"airplay-in"`, `"bt-bridge-rtp"`).
    pub source: String,
    /// Output node name (e.g. `"ap2-dev-dusche"`, `"sendspin-dev-kitchen"`).
    pub output: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RoutingConfig {
    #[serde(default)]
    links: BTreeSet<RoutingLink>,
}

pub struct RoutingStore {
    path: PathBuf,
    config: RoutingConfig,
}

impl RoutingStore {
    /// Load from `path`, or start empty if it doesn't exist yet (created on the
    /// first mutation).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading routing store {}: {e}", path.display()))?;
            let config: RoutingConfig =
                serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing routing store {}: {e}", path.display()))?;
            Ok(Self { path: path.to_path_buf(), config })
        } else {
            Ok(Self { path: path.to_path_buf(), config: RoutingConfig::default() })
        }
    }

    pub fn links(&self) -> impl Iterator<Item = &RoutingLink> {
        self.config.links.iter()
    }

    pub fn contains(&self, source: &str, output: &str) -> bool {
        self.config.links.contains(&RoutingLink { source: source.to_string(), output: output.to_string() })
    }

    /// Record a desired link (idempotent). Persists only if it was new.
    pub fn add(&mut self, source: &str, output: &str) -> anyhow::Result<()> {
        if self.config.links.insert(RoutingLink { source: source.to_string(), output: output.to_string() }) {
            self.persist()?;
        }
        Ok(())
    }

    /// Drop a desired link (idempotent). Persists only if it existed.
    pub fn remove(&mut self, source: &str, output: &str) -> anyhow::Result<()> {
        if self.config.links.remove(&RoutingLink { source: source.to_string(), output: output.to_string() }) {
            self.persist()?;
        }
        Ok(())
    }

    /// Forget every link that references `node_name` on either side — used when
    /// an output or source is removed entirely, so its intent doesn't linger as
    /// a phantom offline entity. Persists if anything changed.
    pub fn remove_entity(&mut self, node_name: &str) -> anyhow::Result<()> {
        let before = self.config.links.len();
        self.config.links.retain(|l| l.source != node_name && l.output != node_name);
        if self.config.links.len() != before {
            self.persist()?;
        }
        Ok(())
    }

    /// Output node names that appear in at least one intent link — used to
    /// surface "offline but configured" outputs the graph doesn't (yet) have.
    pub fn referenced_outputs(&self) -> BTreeSet<String> {
        self.config.links.iter().map(|l| l.output.clone()).collect()
    }

    /// Source node names referenced in intent links (counterpart of
    /// [`Self::referenced_outputs`], for offline sources).
    pub fn referenced_sources(&self) -> BTreeSet<String> {
        self.config.links.iter().map(|l| l.source.clone()).collect()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, json).map_err(|e| anyhow::anyhow!("writing routing store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

/// Shared handle used across the API and the reconcile task.
pub type SharedRouting = Arc<Mutex<RoutingStore>>;

/// Convenience: lock and snapshot the current intent links.
pub fn snapshot(routing: &SharedRouting) -> Vec<RoutingLink> {
    routing.lock_recover().links().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("routing-store-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn add_dedupes_and_persists_across_reload() {
        let path = temp_path("add");
        let _ = std::fs::remove_file(&path);
        let mut store = RoutingStore::load(&path).unwrap();
        store.add("airplay-in", "ap2-dev-dusche").unwrap();
        store.add("airplay-in", "ap2-dev-dusche").unwrap(); // dup, no-op
        assert!(store.contains("airplay-in", "ap2-dev-dusche"));
        assert_eq!(RoutingStore::load(&path).unwrap().links().count(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_entity_drops_all_refs() {
        let path = temp_path("entity");
        let _ = std::fs::remove_file(&path);
        let mut store = RoutingStore::load(&path).unwrap();
        store.add("airplay-in", "ap2-dev-dusche").unwrap();
        store.add("bt-bridge-rtp", "ap2-dev-dusche").unwrap();
        store.add("airplay-in", "ap2-dev-pioneer").unwrap();
        store.remove_entity("ap2-dev-dusche").unwrap();
        assert!(!store.contains("airplay-in", "ap2-dev-dusche"));
        assert!(!store.contains("bt-bridge-rtp", "ap2-dev-dusche"));
        assert!(store.contains("airplay-in", "ap2-dev-pioneer"));
        let _ = std::fs::remove_file(&path);
    }
}
