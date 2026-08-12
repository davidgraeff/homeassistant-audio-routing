//! Which discovered outputs the user has actually **adopted** — the gate
//! between "we found a speaker on the network" and "this speaker is part of the
//! system".
//!
//! Discovery (outputs/sendspin/discovery.rs, outputs/ap2/discovery.rs, outputs/pwsink/discovery.rs)
//! finds every compatible device on the LAN, which used to make each one
//! immediately routable *and* — with the `expose_outputs_as_media_players`
//! toggle on — spawn a Home Assistant `media_player` for it. On a normal home
//! network that means a neighbour's HomePod, a guest phone's AirPlay target or a
//! colleague's laptop running `module-rtp-session` all silently join the routing
//! matrix and HA's entity registry. So discovery is now only an *offer*: a
//! device stays inert until the user adds it.
//!
//! Three states, and the file only stores the two non-default ones:
//!
//! - **discovered** (default, not in this file) — visible on the Outputs page
//!   under "Discovered", with connection details and test playback so the user
//!   can identify it before committing. Not in the routing matrix, gets no
//!   media_player, and the group reconciler ignores it, so no audio is ever
//!   sent to it (the on-demand *test* tone/announcement path is the one
//!   deliberate exception — that's how you tell which speaker this is).
//! - **adopted** — a normal output: routable, exposed to HA, tunable.
//! - **ignored** — hidden from "Discovered" behind the page's "show ignored"
//!   checkbox, so a network full of foreign devices stays out of the way.
//!
//! Keyed by the same stable `node_name` as the routing intent
//! (store/routing.rs), so adoption survives a restart, a device dropping off
//! the network, and mDNS re-resolution. Adopting is *not* remembered per
//! device-kind: the name prefix (`sendspin-dev-`, `ap2-dev-`, `pwsink-dev-`)
//! already carries that.
//!
//! Mirrors the other `/data` stores (store/routing.rs, store/settings.rs): no
//! `options.json` seeding, every field `serde(default)` so an older/newer file
//! still loads, and the file is authoritative once written (created on the
//! first mutation). **No migration on upgrade** — an existing install comes up
//! with nothing adopted and its saved routing intact but dormant, so the user
//! confirms each device once and inherits their routing the moment they do.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Shortest name a user may give an output (see [`OutputsStore::set_name`]).
pub const MIN_NAME_CHARS: usize = 3;

/// What the user has decided about a discovered output. Serialized in
/// `/api/outputs` (and the discovered listing) as the lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    /// Found by discovery, no decision yet: inert, offered on the Outputs page.
    Discovered,
    /// Added by the user: routable, exposed to HA, tunable.
    Adopted,
    /// Explicitly dismissed: hidden unless "show ignored" is on.
    Ignored,
}

impl OutputState {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputState::Discovered => "discovered",
            OutputState::Adopted => "adopted",
            OutputState::Ignored => "ignored",
        }
    }
}

/// On-disk shape. Two name sets rather than a map so the file stays readable
/// and diffable, and an entry can only be in one of them (enforced by the
/// mutators below).
#[derive(Debug, Default, Serialize, Deserialize)]
struct OutputsConfig {
    #[serde(default)]
    adopted: BTreeSet<String>,
    #[serde(default)]
    ignored: BTreeSet<String>,
    /// User-chosen display names, keyed by `node_name`. Absent = the name
    /// discovery reported (mDNS) or, failing that, the one derived from the node
    /// name. Deliberately *not* touched by the state mutators below: a label is
    /// not a verdict, so un-ignoring a device — or re-adding one you removed and
    /// the network offered again — keeps the name you gave it.
    #[serde(default)]
    names: BTreeMap<String, String>,
}

pub struct OutputsStore {
    path: PathBuf,
    config: OutputsConfig,
}

impl OutputsStore {
    /// Load from `path`, or start empty if it doesn't exist yet (created on the
    /// first mutation). Empty = nothing adopted, which is the safe default: a
    /// fresh install offers every device instead of enrolling it.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading outputs store {}: {e}", path.display()))?;
            let config: OutputsConfig =
                serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing outputs store {}: {e}", path.display()))?;
            Ok(Self { path: path.to_path_buf(), config })
        } else {
            Ok(Self { path: path.to_path_buf(), config: OutputsConfig::default() })
        }
    }

    pub fn state(&self, node_name: &str) -> OutputState {
        if self.config.adopted.contains(node_name) {
            OutputState::Adopted
        } else if self.config.ignored.contains(node_name) {
            OutputState::Ignored
        } else {
            OutputState::Discovered
        }
    }

    pub fn is_adopted(&self, node_name: &str) -> bool {
        self.config.adopted.contains(node_name)
    }

    /// The adopted set — the authoritative "these are our outputs" list, used to
    /// keep offline-but-adopted outputs listed and to gate the routing matrix.
    pub fn adopted(&self) -> &BTreeSet<String> {
        &self.config.adopted
    }

    /// Add an output: it becomes routable and (with the toggle on) an HA
    /// media_player. Clears any ignore. Idempotent; persists only on change.
    pub fn adopt(&mut self, node_name: &str) -> anyhow::Result<()> {
        let changed = self.config.adopted.insert(node_name.to_string()) | self.config.ignored.remove(node_name);
        if changed {
            self.persist()?;
        }
        Ok(())
    }

    /// Dismiss an output: hidden from the discovered list unless "show ignored"
    /// is on. Clears any adoption — ignoring is the stronger form of removing,
    /// so the caller pairs this with forgetting routing/group membership.
    pub fn ignore(&mut self, node_name: &str) -> anyhow::Result<()> {
        let changed = self.config.ignored.insert(node_name.to_string()) | self.config.adopted.remove(node_name);
        if changed {
            self.persist()?;
        }
        Ok(())
    }

    /// The user's name for this output, if they gave it one.
    pub fn name(&self, node_name: &str) -> Option<&str> {
        self.config.names.get(node_name).map(String::as_str)
    }

    /// Every user-chosen name, for the callers that resolve a whole listing at
    /// once (the outputs API, the routing matrix) rather than one output.
    pub fn names(&self) -> &BTreeMap<String, String> {
        &self.config.names
    }

    /// Rename an output, or (with `None`) drop the override so it goes back to
    /// the name discovery reports.
    ///
    /// The length rule lives here rather than in the API handler because this is
    /// what persists: a one-or-two-character name is almost always a slip, and
    /// it becomes an HA entity name and a routing-graph label, where there is no
    /// room to notice the mistake. Idempotent; persists only on change.
    pub fn set_name(&mut self, node_name: &str, name: Option<&str>) -> anyhow::Result<()> {
        let changed = match name {
            Some(n) => {
                let trimmed = n.trim();
                if trimmed.chars().count() < MIN_NAME_CHARS {
                    anyhow::bail!("a name needs at least {MIN_NAME_CHARS} characters");
                }
                self.config.names.insert(node_name.to_string(), trimmed.to_string()).as_deref() != Some(trimmed)
            }
            None => self.config.names.remove(node_name).is_some(),
        };
        if changed {
            self.persist()?;
        }
        Ok(())
    }

    /// Back to undecided — the state a removed output lands in, so a device
    /// that's still on the network reappears under "Discovered" and can be
    /// added again. Idempotent; persists only on change.
    pub fn reset(&mut self, node_name: &str) -> anyhow::Result<()> {
        let changed = self.config.adopted.remove(node_name) | self.config.ignored.remove(node_name);
        if changed {
            self.persist()?;
        }
        Ok(())
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, json).map_err(|e| anyhow::anyhow!("writing outputs store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

/// Shared handle used across the API, the routing matrix and the group
/// reconciler (which all need the same adoption verdict).
pub type SharedOutputs = Arc<Mutex<OutputsStore>>;

/// Convenience: lock and snapshot the adopted set. Used on the hot-ish paths
/// (matrix snapshot, reconcile pass) so no guard is held across an await.
pub fn adopted_snapshot(outputs: &SharedOutputs) -> BTreeSet<String> {
    use crate::util::locks::LockRecover;
    outputs.lock_recover().adopted().clone()
}

/// Convenience: lock and snapshot the user-chosen names, for the same reason as
/// [`adopted_snapshot`] — the matrix build holds no guard across an await.
pub fn names_snapshot(outputs: &SharedOutputs) -> BTreeMap<String, String> {
    use crate::util::locks::LockRecover;
    outputs.lock_recover().names().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("outputs-store-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn fresh_install_adopts_nothing() {
        let path = temp_path("empty");
        let _ = std::fs::remove_file(&path);
        let store = OutputsStore::load(&path).unwrap();
        assert_eq!(store.state("sendspin-dev-kitchen"), OutputState::Discovered);
        assert!(!store.is_adopted("sendspin-dev-kitchen"));
        assert!(store.adopted().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn states_are_exclusive_and_survive_reload() {
        let path = temp_path("exclusive");
        let _ = std::fs::remove_file(&path);
        let mut store = OutputsStore::load(&path).unwrap();
        store.adopt("ap2-dev-dusche").unwrap();
        store.adopt("ap2-dev-dusche").unwrap(); // idempotent
        store.ignore("ap2-dev-neighbour").unwrap();
        // Ignoring an adopted output un-adopts it (and vice versa).
        store.ignore("ap2-dev-dusche").unwrap();
        assert_eq!(store.state("ap2-dev-dusche"), OutputState::Ignored);
        store.adopt("ap2-dev-dusche").unwrap();
        assert_eq!(store.state("ap2-dev-dusche"), OutputState::Adopted);

        let reloaded = OutputsStore::load(&path).unwrap();
        assert_eq!(reloaded.state("ap2-dev-dusche"), OutputState::Adopted);
        assert_eq!(reloaded.state("ap2-dev-neighbour"), OutputState::Ignored);
        assert_eq!(reloaded.adopted().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reset_returns_an_output_to_the_discovered_offer() {
        let path = temp_path("reset");
        let _ = std::fs::remove_file(&path);
        let mut store = OutputsStore::load(&path).unwrap();
        store.adopt("pwsink-dev-desk").unwrap();
        store.reset("pwsink-dev-desk").unwrap();
        assert_eq!(store.state("pwsink-dev-desk"), OutputState::Discovered);
        assert_eq!(OutputsStore::load(&path).unwrap().state("pwsink-dev-desk"), OutputState::Discovered);
        let _ = std::fs::remove_file(&path);
    }

    /// A rename is trimmed, survives a reload, and is independent of the
    /// adopted/ignored verdict — including across the reset a remove or an
    /// un-ignore performs.
    #[test]
    fn names_are_trimmed_persisted_and_outlive_the_verdict() {
        let path = temp_path("names");
        let _ = std::fs::remove_file(&path);
        let mut store = OutputsStore::load(&path).unwrap();
        assert_eq!(store.name("ap2-dev-dusche"), None);
        store.set_name("ap2-dev-dusche", Some("  Shower  ")).unwrap();
        assert_eq!(store.name("ap2-dev-dusche"), Some("Shower"));
        store.adopt("ap2-dev-dusche").unwrap();
        store.reset("ap2-dev-dusche").unwrap();
        assert_eq!(store.name("ap2-dev-dusche"), Some("Shower"), "a rename is a label, not a verdict");

        // Too short (after trimming) is refused, and refusing changes nothing.
        assert!(store.set_name("ap2-dev-dusche", Some(" ab ")).is_err());
        assert_eq!(store.name("ap2-dev-dusche"), Some("Shower"));

        assert_eq!(OutputsStore::load(&path).unwrap().name("ap2-dev-dusche"), Some("Shower"));
        store.set_name("ap2-dev-dusche", None).unwrap();
        assert_eq!(OutputsStore::load(&path).unwrap().name("ap2-dev-dusche"), None);
        let _ = std::fs::remove_file(&path);
    }

    /// An unknown/legacy key in the file must not stop the daemon booting — same
    /// forward/backward tolerance as the other /data stores.
    #[test]
    fn unknown_keys_are_tolerated() {
        let path = temp_path("legacy");
        std::fs::write(&path, r#"{"outputs":[{"name":"raop-out-old"}],"adopted":["sendspin-dev-kitchen"]}"#).unwrap();
        let store = OutputsStore::load(&path).unwrap();
        assert!(store.is_adopted("sendspin-dev-kitchen"));
        let _ = std::fs::remove_file(&path);
    }
}
