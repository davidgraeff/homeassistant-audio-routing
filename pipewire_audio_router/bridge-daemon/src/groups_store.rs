//! Persisted **named groups** — the data model for the two-tier grouping design
//! (see docs/architecture-limitations-and-options.md):
//!
//! - **Music groups (MG)** — a named set of outputs that play the same stream in
//!   sync. **Exclusive:** an output belongs to at most one MG (validated here).
//!   The routable unit: routing a source to an MG routes it to all its members
//!   (the routing target is polymorphic — output | MG — so this layers onto the
//!   matrix without hardcoding "row == MG"). Reconciler integration is staged.
//! - **Announcement groups (AG)** — a named, reusable set of target outputs an
//!   announcement plays to, with a `priority` and `duck` level. Overlaps MGs and
//!   other AGs freely (no exclusivity). Addresses `/api/announce` by name.
//!
//! Mirrors the other `/data` stores (settings_store/routing_store): `serde`
//! defaults so an older/newer file still loads, file authoritative once written.

use crate::util::node_names::slugify;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn default_duck() -> f32 {
    0.25
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MusicGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnnouncementGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_duck")]
    pub duck: f32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GroupsConfig {
    #[serde(default)]
    music: Vec<MusicGroup>,
    #[serde(default)]
    announcement: Vec<AnnouncementGroup>,
}

pub struct GroupsStore {
    path: PathBuf,
    config: GroupsConfig,
}

pub type SharedGroupsStore = Arc<Mutex<GroupsStore>>;

impl GroupsStore {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let config = if path.exists() {
            let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading groups store {}: {e}", path.display()))?;
            serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing groups store {}: {e}", path.display()))?
        } else {
            GroupsConfig::default()
        };
        Ok(Self { path: path.to_path_buf(), config })
    }

    pub fn music(&self) -> &[MusicGroup] {
        &self.config.music
    }

    pub fn announcement(&self) -> &[AnnouncementGroup] {
        &self.config.announcement
    }

    pub fn announcement_by_id(&self, id: &str) -> Option<&AnnouncementGroup> {
        self.config.announcement.iter().find(|g| g.id == id)
    }

    /// Which MG (if any) already contains any of `members`, excluding the group
    /// `editing` (so updating a group's own membership doesn't self-conflict).
    fn exclusivity_conflict(&self, members: &[String], editing: Option<&str>) -> Option<(String, String)> {
        for mg in &self.config.music {
            if editing == Some(mg.id.as_str()) {
                continue;
            }
            if let Some(m) = members.iter().find(|m| mg.members.contains(m)) {
                return Some((m.clone(), mg.name.clone()));
            }
        }
        None
    }

    fn unique_id(&self, name: &str) -> String {
        let base = {
            let s = slugify(name);
            if s.is_empty() {
                "group".to_string()
            } else {
                s
            }
        };
        let taken = |id: &str| self.config.music.iter().any(|g| g.id == id) || self.config.announcement.iter().any(|g| g.id == id);
        if !taken(&base) {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap()
    }

    pub fn create_music(&mut self, name: &str, members: Vec<String>) -> anyhow::Result<MusicGroup> {
        if let Some((m, other)) = self.exclusivity_conflict(&members, None) {
            anyhow::bail!("output '{m}' is already in music group '{other}' (an output can be in only one music group)");
        }
        let mg = MusicGroup { id: self.unique_id(name), name: name.to_string(), members };
        self.config.music.push(mg.clone());
        self.persist()?;
        Ok(mg)
    }

    pub fn update_music(&mut self, id: &str, name: Option<String>, members: Option<Vec<String>>) -> anyhow::Result<MusicGroup> {
        if let Some(ref m) = members {
            if let Some((mem, other)) = self.exclusivity_conflict(m, Some(id)) {
                anyhow::bail!("output '{mem}' is already in music group '{other}'");
            }
        }
        let mg = self.config.music.iter_mut().find(|g| g.id == id).ok_or_else(|| anyhow::anyhow!("no music group '{id}'"))?;
        if let Some(n) = name {
            mg.name = n;
        }
        if let Some(m) = members {
            mg.members = m;
        }
        let out = mg.clone();
        self.persist()?;
        Ok(out)
    }

    pub fn delete_music(&mut self, id: &str) -> anyhow::Result<()> {
        let before = self.config.music.len();
        self.config.music.retain(|g| g.id != id);
        if self.config.music.len() == before {
            anyhow::bail!("no music group '{id}'");
        }
        self.persist()
    }

    pub fn create_announcement(&mut self, name: &str, targets: Vec<String>, priority: i32, duck: f32) -> anyhow::Result<AnnouncementGroup> {
        let ag = AnnouncementGroup { id: self.unique_id(name), name: name.to_string(), targets, priority, duck: duck.clamp(0.0, 1.0) };
        self.config.announcement.push(ag.clone());
        self.persist()?;
        Ok(ag)
    }

    pub fn update_announcement(
        &mut self,
        id: &str,
        name: Option<String>,
        targets: Option<Vec<String>>,
        priority: Option<i32>,
        duck: Option<f32>,
    ) -> anyhow::Result<AnnouncementGroup> {
        let ag = self.config.announcement.iter_mut().find(|g| g.id == id).ok_or_else(|| anyhow::anyhow!("no announcement group '{id}'"))?;
        if let Some(n) = name {
            ag.name = n;
        }
        if let Some(t) = targets {
            ag.targets = t;
        }
        if let Some(p) = priority {
            ag.priority = p;
        }
        if let Some(d) = duck {
            ag.duck = d.clamp(0.0, 1.0);
        }
        let out = ag.clone();
        self.persist()?;
        Ok(out)
    }

    pub fn delete_announcement(&mut self, id: &str) -> anyhow::Result<()> {
        let before = self.config.announcement.len();
        self.config.announcement.retain(|g| g.id != id);
        if self.config.announcement.len() == before {
            anyhow::bail!("no announcement group '{id}'");
        }
        self.persist()
    }

    /// Drop `node_name` from every music group's members and every announcement
    /// group's targets — used when an output is removed or ignored on the Outputs
    /// page, so a stale member can't silently re-route it if it's ever added
    /// again. The groups themselves are kept even if they end up empty (a named,
    /// empty group is a valid thing to refill; deleting the user's group because
    /// its last speaker went away would be a surprise). Returns whether anything
    /// changed; persists only then.
    pub fn remove_output(&mut self, node_name: &str) -> anyhow::Result<bool> {
        let mut changed = false;
        for g in &mut self.config.music {
            let before = g.members.len();
            g.members.retain(|m| m != node_name);
            changed |= g.members.len() != before;
        }
        for g in &mut self.config.announcement {
            let before = g.targets.len();
            g.targets.retain(|t| t != node_name);
            changed |= g.targets.len() != before;
        }
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, json).map_err(|e| anyhow::anyhow!("writing groups store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("groups-store-{tag}-{}.json", std::process::id()))
    }
    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn music_group_membership_is_exclusive() {
        let path = temp("excl");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        s.create_music("Downstairs", v(&["kitchen", "hall"])).unwrap();
        // A second MG reusing 'kitchen' is rejected.
        let err = s.create_music("Kitchen Only", v(&["kitchen"])).unwrap_err();
        assert!(err.to_string().contains("already in music group"), "got: {err}");
        // A disjoint MG is fine.
        assert!(s.create_music("Bedroom", v(&["bedroom"])).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_music_can_keep_its_own_members() {
        let path = temp("selfedit");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let mg = s.create_music("Zone", v(&["a", "b"])).unwrap();
        // Re-saving the same/overlapping members for the SAME group must not conflict.
        assert!(s.update_music(&mg.id, None, Some(v(&["a", "b", "c"]))).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn announcement_groups_overlap_freely_and_default_duck() {
        let path = temp("ag");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let a = s.create_announcement("Downstairs", v(&["kitchen", "hall"]), 0, 0.25).unwrap();
        // Overlapping targets across AGs are allowed.
        let b = s.create_announcement("Everywhere", v(&["kitchen", "bedroom"]), 10, 0.3).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(s.announcement_by_id(&b.id).unwrap().priority, 10);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_output_strips_it_from_every_group_but_keeps_the_groups() {
        let path = temp("rmout");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let mg = s.create_music("Downstairs", v(&["kitchen", "hall"])).unwrap();
        let solo = s.create_music("Kitchen Only", v(&["kitchen-2"])).unwrap();
        let ag = s.create_announcement("Everywhere", v(&["kitchen", "bedroom"]), 0, 0.25).unwrap();

        assert!(s.remove_output("kitchen").unwrap());
        assert!(!s.remove_output("kitchen").unwrap(), "second removal is a no-op");
        assert_eq!(s.music().iter().find(|g| g.id == mg.id).unwrap().members, v(&["hall"]));
        assert_eq!(s.announcement_by_id(&ag.id).unwrap().targets, v(&["bedroom"]));
        // A near-miss name is untouched, and emptying a group doesn't delete it.
        assert_eq!(s.music().iter().find(|g| g.id == solo.id).unwrap().members, v(&["kitchen-2"]));
        s.remove_output("kitchen-2").unwrap();
        let reloaded = GroupsStore::load(&path).unwrap();
        assert_eq!(reloaded.music().len(), 2);
        assert!(reloaded.music().iter().find(|g| g.id == solo.id).unwrap().members.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ids_are_unique_slugs_and_persist_across_reload() {
        let path = temp("persist");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let a = s.create_music("Living Room", v(&["x"])).unwrap();
        let b = s.create_music("Living Room", v(&["y"])).unwrap();
        // slugify() lowercases and replaces non-alphanumerics with '_'.
        assert_eq!(a.id, "living_room");
        assert_eq!(b.id, "living_room-2");

        let reloaded = GroupsStore::load(&path).unwrap();
        assert_eq!(reloaded.music().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_unknown_errors() {
        let path = temp("del");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        assert!(s.delete_music("nope").is_err());
        let _ = std::fs::remove_file(&path);
    }
}
