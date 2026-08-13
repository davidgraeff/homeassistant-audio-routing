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
//! - **Presets** — a named grouping of the whole house: which speakers sit in
//!   which MG, and what each MG plays. See docs/music-group-presets-plan.md.
//!
//! The split that makes presets work (plan §2): an MG stored here is an
//! **identity** (`id` + `name`) and nothing more; the *membership* lives in each
//! preset. An MG identity is a Home Assistant entity (`<entry>_mg_<id>`), so it
//! has to outlive every preset switch — if a preset owned the group list,
//! switching would create and destroy `media_player` entities.
//!
//! Readers are unaffected: [`GroupsStore::music`] hands out `MusicGroup`s whose
//! `members` are the *active* preset's, which is the shape `/api/groups/music`
//! always returned. Presets are additive surface, not a migration of the read
//! path.
//!
//! Mirrors the other `/data` stores (settings_store/routing_store): `serde`
//! defaults so an older/newer file still loads, file authoritative once written.

use crate::util::node_names::slugify;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn default_duck() -> f32 {
    0.25
}

/// The preset every install has, cannot delete, and falls back to.
pub const DEFAULT_PRESET_ID: &str = "default";
const DEFAULT_PRESET_NAME: &str = "Default";

/// A music group as every reader wants it: identity plus the members it has in
/// the preset being asked about (the active one, for [`GroupsStore::music`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MusicGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
}

/// A music group as *stored*: identity only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct MusicGroupIdentity {
    id: String,
    name: String,
    /// Where membership lived before presets existed. Read once by the migration
    /// in [`GroupsStore::load`] and never written again (hence
    /// `skip_serializing_if`), so a `groups.json` from an older daemon upgrades
    /// on first boot instead of coming up with empty groups.
    #[serde(rename = "members", default, skip_serializing_if = "Vec::is_empty")]
    legacy_members: Vec<String>,
}

/// One music group's slot in one preset: who is in it, and what it plays.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PresetGroup {
    #[serde(default)]
    pub members: Vec<String>,
    /// Source node name to put the members on when this preset is activated.
    /// `None` means **leave their links alone** — a preset does not touch what it
    /// does not mention (plan §4.1). Kept current by the write-through in
    /// `api/groups.rs` while the preset is active.
    #[serde(default)]
    pub source: Option<String>,
}

/// A named grouping of the house. Groups it doesn't mention are simply empty in
/// it; the identities still exist (and still have their HA entities).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub groups: BTreeMap<String, PresetGroup>,
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

fn default_active() -> String {
    DEFAULT_PRESET_ID.to_string()
}

#[derive(Debug, Serialize, Deserialize)]
struct GroupsConfig {
    #[serde(default)]
    music: Vec<MusicGroupIdentity>,
    #[serde(default)]
    presets: Vec<Preset>,
    #[serde(default = "default_active")]
    active_preset: String,
    #[serde(default)]
    announcement: Vec<AnnouncementGroup>,
}

impl Default for GroupsConfig {
    fn default() -> Self {
        Self { music: Vec::new(), presets: Vec::new(), active_preset: default_active(), announcement: Vec::new() }
    }
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
        let mut store = Self { path: path.to_path_buf(), config };
        // The shape upgrade runs here, not in store/migration.rs: it is this
        // struct's own file format, and it has to have happened before any reader
        // sees the store. Idempotent — a file that already has presets is left
        // alone, so this is not worth a version field.
        if store.migrate_to_presets() {
            if let Err(e) = store.persist() {
                // In-memory is already upgraded, so the daemon runs correctly and
                // retries the write on the next mutation.
                tracing::warn!("groups store: could not persist the preset migration: {e}");
            }
        }
        Ok(store)
    }

    /// Give the store the `Default` preset every install has, moving pre-presets
    /// memberships (`music[].members`) into it. Returns whether anything changed.
    fn migrate_to_presets(&mut self) -> bool {
        let mut changed = false;
        if !self.config.presets.iter().any(|p| p.id == DEFAULT_PRESET_ID) {
            let groups = self
                .config
                .music
                .iter()
                .filter(|g| !g.legacy_members.is_empty())
                .map(|g| (g.id.clone(), PresetGroup { members: g.legacy_members.clone(), source: None }))
                .collect::<BTreeMap<_, _>>();
            if !groups.is_empty() {
                tracing::info!("groups store: moved {} music group(s) into the '{DEFAULT_PRESET_NAME}' preset", groups.len());
            }
            // First, so `Default` leads the chip bar.
            self.config.presets.insert(0, Preset { id: DEFAULT_PRESET_ID.to_string(), name: DEFAULT_PRESET_NAME.to_string(), groups });
            changed = true;
        }
        for g in &mut self.config.music {
            changed |= !g.legacy_members.is_empty();
            g.legacy_members.clear();
        }
        if !self.config.presets.iter().any(|p| p.id == self.config.active_preset) {
            self.config.active_preset = default_active();
            changed = true;
        }
        changed
    }

    // ---- presets ---------------------------------------------------------

    pub fn presets(&self) -> &[Preset] {
        &self.config.presets
    }

    pub fn active_preset(&self) -> &str {
        &self.config.active_preset
    }

    /// The preset a membership edit applies to: the named one, or the active one.
    /// Errors on a name that isn't a preset rather than silently editing the
    /// active grouping — a stale preset id in a request must not move speakers.
    fn resolve_preset(&self, preset: Option<&str>) -> anyhow::Result<String> {
        match preset {
            None => Ok(self.config.active_preset.clone()),
            Some(id) if self.config.presets.iter().any(|p| p.id == id) => Ok(id.to_string()),
            Some(id) => anyhow::bail!("no preset '{id}'"),
        }
    }

    fn preset(&self, id: &str) -> Option<&Preset> {
        self.config.presets.iter().find(|p| p.id == id)
    }

    fn preset_mut(&mut self, id: &str) -> anyhow::Result<&mut Preset> {
        self.config.presets.iter_mut().find(|p| p.id == id).ok_or_else(|| anyhow::anyhow!("no preset '{id}'"))
    }

    /// A new preset, optionally copying another's grouping. `copy_from` is what
    /// the UI passes by default (the selected preset): a variant is nearly always
    /// an edit of an existing grouping, not a blank slate.
    pub fn create_preset(&mut self, name: &str, copy_from: Option<&str>) -> anyhow::Result<Preset> {
        let groups = match copy_from {
            None => BTreeMap::new(),
            Some(src) => self.preset(src).ok_or_else(|| anyhow::anyhow!("no preset '{src}' to copy"))?.groups.clone(),
        };
        let preset = Preset { id: self.unique_preset_id(name), name: name.to_string(), groups };
        self.config.presets.push(preset.clone());
        self.persist()?;
        Ok(preset)
    }

    pub fn rename_preset(&mut self, id: &str, name: &str) -> anyhow::Result<Preset> {
        let p = self.preset_mut(id)?;
        p.name = name.to_string();
        let out = p.clone();
        self.persist()?;
        Ok(out)
    }

    /// Delete a preset. `Default` is refused (it is the fallback, and the state
    /// the "work with presets" switch returns the house to). Deleting the active
    /// one leaves `Default` active — the caller then applies it, exactly as for
    /// an explicit activation.
    pub fn delete_preset(&mut self, id: &str) -> anyhow::Result<()> {
        if id == DEFAULT_PRESET_ID {
            anyhow::bail!("the '{DEFAULT_PRESET_NAME}' preset cannot be deleted");
        }
        let before = self.config.presets.len();
        self.config.presets.retain(|p| p.id != id);
        if self.config.presets.len() == before {
            anyhow::bail!("no preset '{id}'");
        }
        if self.config.active_preset == id {
            self.config.active_preset = default_active();
        }
        self.persist()
    }

    /// Make `id` the active preset and return the routing plan to apply: one
    /// entry per group *with members*, holding the source it should play —
    /// `None` meaning "leave these members' links alone" (plan §4.1). Applying it
    /// is the API layer's job, in one pass, so the reconciler sees one
    /// transition.
    pub fn activate_preset(&mut self, id: &str) -> anyhow::Result<Vec<(Vec<String>, Option<String>)>> {
        let preset = self.preset(id).ok_or_else(|| anyhow::anyhow!("no preset '{id}'"))?;
        let plan =
            preset.groups.values().filter(|g| !g.members.is_empty()).map(|g| (g.members.clone(), g.source.clone())).collect::<Vec<_>>();
        self.config.active_preset = id.to_string();
        self.persist()?;
        Ok(plan)
    }

    /// Record what a music group plays in the **active** preset — the write-through
    /// that lets a preset restore the music and not just the grouping (plan §4.2).
    /// A group with no slot in the active preset gets none: routing a group that
    /// this preset doesn't contain is a routing edit, not a preset edit.
    pub fn note_source(&mut self, group_id: &str, source: Option<&str>) -> anyhow::Result<()> {
        let active = self.config.active_preset.clone();
        let Ok(preset) = self.preset_mut(&active) else { return Ok(()) };
        let Some(slot) = preset.groups.get_mut(group_id) else { return Ok(()) };
        let next = source.map(|s| s.to_string());
        if slot.source == next {
            return Ok(());
        }
        slot.source = next;
        self.persist()
    }

    // ---- music groups ----------------------------------------------------

    /// Every music group with the members it has in the **active** preset — the
    /// shape every reader (HA entities, the card, the routing endpoints) wants.
    pub fn music(&self) -> Vec<MusicGroup> {
        self.music_in(&self.config.active_preset)
    }

    /// Every music group with the members it has in `preset`; a group with no
    /// slot there is listed with no members (its identity, and its Home Assistant
    /// entity, exist regardless).
    pub fn music_in(&self, preset: &str) -> Vec<MusicGroup> {
        let slots = self.preset(preset).map(|p| &p.groups);
        self.config
            .music
            .iter()
            .map(|g| MusicGroup {
                id: g.id.clone(),
                name: g.name.clone(),
                members: slots.and_then(|s| s.get(&g.id)).map(|s| s.members.clone()).unwrap_or_default(),
            })
            .collect()
    }

    pub fn announcement(&self) -> &[AnnouncementGroup] {
        &self.config.announcement
    }

    pub fn announcement_by_id(&self, id: &str) -> Option<&AnnouncementGroup> {
        self.config.announcement.iter().find(|g| g.id == id)
    }

    /// Which MG **in this preset** (if any) already contains any of `members`,
    /// excluding the group `editing` (so updating a group's own membership doesn't
    /// self-conflict). Exclusivity is per preset, which is the point of the
    /// feature: `Bath` may hold a speaker in one preset while `Everywhere` holds
    /// it in another, and neither edit has to make room for the other.
    fn exclusivity_conflict(&self, preset: &str, members: &[String], editing: Option<&str>) -> Option<(String, String)> {
        let slots = self.preset(preset)?;
        let name_of = |id: &str| self.config.music.iter().find(|g| g.id == id).map(|g| g.name.clone()).unwrap_or_else(|| id.to_string());
        for (id, slot) in &slots.groups {
            if editing == Some(id.as_str()) {
                continue;
            }
            if let Some(m) = members.iter().find(|m| slot.members.contains(m)) {
                return Some((m.clone(), name_of(id)));
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

    /// Preset ids live in their own namespace (their own endpoint, never mixed
    /// with a group id in a request), so this doesn't consult the group ids.
    fn unique_preset_id(&self, name: &str) -> String {
        let base = {
            let s = slugify(name);
            if s.is_empty() {
                "preset".to_string()
            } else {
                s
            }
        };
        let taken = |id: &str| self.config.presets.iter().any(|p| p.id == id);
        if !taken(&base) {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap()
    }

    /// Create a music group identity, with `members` in `preset` (default: the
    /// active one). The identity is global — a group created while editing the
    /// party layout still gets its Home Assistant entity, and can be filled in
    /// any other preset.
    pub fn create_music(&mut self, name: &str, members: Vec<String>, preset: Option<&str>) -> anyhow::Result<MusicGroup> {
        let preset = self.resolve_preset(preset)?;
        if let Some((m, other)) = self.exclusivity_conflict(&preset, &members, None) {
            anyhow::bail!("output '{m}' is already in music group '{other}' (an output can be in only one music group)");
        }
        let id = self.unique_id(name);
        self.config.music.push(MusicGroupIdentity { id: id.clone(), name: name.to_string(), legacy_members: Vec::new() });
        self.preset_mut(&preset)?.groups.insert(id.clone(), PresetGroup { members: members.clone(), source: None });
        self.persist()?;
        Ok(MusicGroup { id, name: name.to_string(), members })
    }

    /// Rename a group (global — it is the identity) and/or set its members in
    /// `preset` (default: the active one).
    pub fn update_music(
        &mut self,
        id: &str,
        name: Option<String>,
        members: Option<Vec<String>>,
        preset: Option<&str>,
    ) -> anyhow::Result<MusicGroup> {
        let preset = self.resolve_preset(preset)?;
        if !self.config.music.iter().any(|g| g.id == id) {
            anyhow::bail!("no music group '{id}'");
        }
        if let Some(ref m) = members {
            if let Some((mem, other)) = self.exclusivity_conflict(&preset, m, Some(id)) {
                anyhow::bail!("output '{mem}' is already in music group '{other}'");
            }
        }
        if let Some(n) = name {
            if let Some(g) = self.config.music.iter_mut().find(|g| g.id == id) {
                g.name = n;
            }
        }
        if let Some(m) = members {
            self.preset_mut(&preset)?.groups.entry(id.to_string()).or_default().members = m;
        }
        self.persist()?;
        // The caller asked about `preset`, so answer about `preset` — for the
        // active one (the common case) that is the same thing as `music()`.
        self.music_in(&preset).into_iter().find(|g| g.id == id).ok_or_else(|| anyhow::anyhow!("no music group '{id}'"))
    }

    /// Delete a music group identity, and its membership in **every** preset: the
    /// group is gone from the house, not just from the grouping in force.
    pub fn delete_music(&mut self, id: &str) -> anyhow::Result<()> {
        let before = self.config.music.len();
        self.config.music.retain(|g| g.id != id);
        if self.config.music.len() == before {
            anyhow::bail!("no music group '{id}'");
        }
        for p in &mut self.config.presets {
            p.groups.remove(id);
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

    /// Drop `node_name` from every music group's members **in every preset** and
    /// from every announcement group's targets — used when an output is removed or
    /// ignored on the Outputs page, so a stale member can't silently re-route it
    /// if it's ever added again. Every preset, because a speaker left behind in an
    /// inactive one would come back the moment that preset is activated. The
    /// groups themselves are kept even if they end up empty (a named, empty group
    /// is a valid thing to refill; deleting the user's group because its last
    /// speaker went away would be a surprise). Returns whether anything changed;
    /// persists only then.
    pub fn remove_output(&mut self, node_name: &str) -> anyhow::Result<bool> {
        let mut changed = false;
        for p in &mut self.config.presets {
            for slot in p.groups.values_mut() {
                let before = slot.members.len();
                slot.members.retain(|m| m != node_name);
                changed |= slot.members.len() != before;
            }
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
        s.create_music("Downstairs", v(&["kitchen", "hall"]), None).unwrap();
        // A second MG reusing 'kitchen' is rejected.
        let err = s.create_music("Kitchen Only", v(&["kitchen"]), None).unwrap_err();
        assert!(err.to_string().contains("already in music group"), "got: {err}");
        // A disjoint MG is fine.
        assert!(s.create_music("Bedroom", v(&["bedroom"]), None).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_music_can_keep_its_own_members() {
        let path = temp("selfedit");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let mg = s.create_music("Zone", v(&["a", "b"]), None).unwrap();
        // Re-saving the same/overlapping members for the SAME group must not conflict.
        assert!(s.update_music(&mg.id, None, Some(v(&["a", "b", "c"])), None).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    // ---- presets ---------------------------------------------------------

    #[test]
    fn a_fresh_store_has_exactly_the_default_preset_active() {
        let path = temp("fresh");
        let _ = std::fs::remove_file(&path);
        let s = GroupsStore::load(&path).unwrap();
        assert_eq!(s.presets().len(), 1);
        assert_eq!(s.active_preset(), DEFAULT_PRESET_ID);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pre_presets_file_migrates_its_members_into_default_and_settles() {
        let path = temp("migrate");
        let _ = std::fs::remove_file(&path);
        // Exactly what an older daemon wrote: members on the group itself.
        std::fs::write(&path, r#"{"music":[{"id":"kitchen_zone","name":"Kitchen","members":["a","b"]}],"announcement":[]}"#).unwrap();

        let s = GroupsStore::load(&path).unwrap();
        assert_eq!(s.active_preset(), DEFAULT_PRESET_ID);
        assert_eq!(s.music(), vec![MusicGroup { id: "kitchen_zone".into(), name: "Kitchen".into(), members: v(&["a", "b"]) }]);

        // Persisted, and loading the upgraded file is a no-op (no second Default,
        // no lost members) — the migration has to be idempotent because it runs
        // on every boot.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("\"members\": [\n        \"a\""), "legacy members should not be written back:\n{raw}");
        let mut again = GroupsStore::load(&path).unwrap();
        assert_eq!(again.presets().len(), 1);
        assert_eq!(again.music()[0].members, v(&["a", "b"]));
        assert!(!again.migrate_to_presets(), "second migration must change nothing");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn one_speaker_can_be_in_different_groups_in_two_presets() {
        let path = temp("perpreset");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let bath = s.create_music("Bath", v(&["dusche"]), None).unwrap();
        let all = s.create_music("Everywhere", v(&[]), None).unwrap();
        let party = s.create_preset("House party", None).unwrap();

        // In `party`, Everywhere takes the speaker Bath holds in Default — no
        // emptying of Bath required, which is the whole point.
        s.update_music(&all.id, None, Some(v(&["dusche", "kitchen"])), Some(&party.id)).unwrap();
        // …and it is still Bath's in the active (Default) preset.
        let active = s.music();
        assert_eq!(active.iter().find(|g| g.id == bath.id).unwrap().members, v(&["dusche"]));
        assert!(active.iter().find(|g| g.id == all.id).unwrap().members.is_empty());
        // Within one preset it is still exclusive.
        let err = s.update_music(&bath.id, None, Some(v(&["dusche"])), Some(&party.id)).unwrap_err();
        assert!(err.to_string().contains("already in music group"), "got: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn activating_a_preset_switches_the_members_and_returns_its_routing_plan() {
        let path = temp("activate");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let kitchen = s.create_music("Kitchen", v(&["a", "b"]), None).unwrap();
        let all = s.create_music("Everywhere", v(&[]), None).unwrap();
        s.note_source(&kitchen.id, Some("airplay-in")).unwrap();
        let party = s.create_preset("House party", None).unwrap();
        s.update_music(&all.id, None, Some(v(&["a", "b", "c"])), Some(&party.id)).unwrap();

        let plan = s.activate_preset(&party.id).unwrap();
        assert_eq!(s.active_preset(), party.id);
        // Only groups with members are in the plan, and this one has no source yet.
        assert_eq!(plan, vec![(v(&["a", "b", "c"]), None)]);
        assert_eq!(s.music().iter().find(|g| g.id == all.id).unwrap().members, v(&["a", "b", "c"]));
        assert!(s.music().iter().find(|g| g.id == kitchen.id).unwrap().members.is_empty());

        // Back to Default: the plan restores the grouping *and* the source that
        // was noted while it was active.
        let plan = s.activate_preset(DEFAULT_PRESET_ID).unwrap();
        assert_eq!(plan, vec![(v(&["a", "b"]), Some("airplay-in".to_string()))]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn note_source_only_touches_the_active_preset() {
        let path = temp("notesrc");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let kitchen = s.create_music("Kitchen", v(&["a"]), None).unwrap();
        let party = s.create_preset("House party", Some(DEFAULT_PRESET_ID)).unwrap();
        s.note_source(&kitchen.id, Some("airplay-in")).unwrap();

        assert_eq!(s.preset(DEFAULT_PRESET_ID).unwrap().groups[&kitchen.id].source.as_deref(), Some("airplay-in"));
        assert_eq!(s.preset(&party.id).unwrap().groups[&kitchen.id].source, None);
        // Un-routing records the absence, so an activation stops the music too.
        s.note_source(&kitchen.id, None).unwrap();
        assert_eq!(s.preset(DEFAULT_PRESET_ID).unwrap().groups[&kitchen.id].source, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_copied_preset_is_independent() {
        let path = temp("copy");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let g = s.create_music("Kitchen", v(&["a"]), None).unwrap();
        let copy = s.create_preset("House party", Some(DEFAULT_PRESET_ID)).unwrap();
        assert_eq!(copy.groups[&g.id].members, v(&["a"]));

        s.update_music(&g.id, None, Some(v(&["a", "b"])), Some(&copy.id)).unwrap();
        assert_eq!(s.music_in(DEFAULT_PRESET_ID)[0].members, v(&["a"]));
        assert_eq!(s.music_in(&copy.id)[0].members, v(&["a", "b"]));
        // A rename is the identity, so it is preset-independent.
        s.update_music(&g.id, Some("Cooking".into()), None, Some(&copy.id)).unwrap();
        assert_eq!(s.music_in(DEFAULT_PRESET_ID)[0].name, "Cooking");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_cannot_be_deleted_and_deleting_the_active_preset_falls_back_to_it() {
        let path = temp("delpreset");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let party = s.create_preset("House party", None).unwrap();
        s.activate_preset(&party.id).unwrap();

        assert!(s.delete_preset(DEFAULT_PRESET_ID).unwrap_err().to_string().contains("cannot be deleted"));
        s.delete_preset(&party.id).unwrap();
        assert_eq!(s.active_preset(), DEFAULT_PRESET_ID);
        // An active preset that vanished from the file is repaired on load, too.
        let reloaded = GroupsStore::load(&path).unwrap();
        assert_eq!(reloaded.active_preset(), DEFAULT_PRESET_ID);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deleting_a_music_group_removes_it_from_every_preset() {
        let path = temp("delgroup");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let g = s.create_music("Kitchen", v(&["a"]), None).unwrap();
        let party = s.create_preset("House party", Some(DEFAULT_PRESET_ID)).unwrap();
        s.delete_music(&g.id).unwrap();
        assert!(s.preset(&party.id).unwrap().groups.is_empty());
        assert!(s.music_in(&party.id).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_preset_scope_is_refused_rather_than_editing_the_active_one() {
        let path = temp("badscope");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let g = s.create_music("Kitchen", v(&["a"]), None).unwrap();
        assert!(s.update_music(&g.id, None, Some(v(&["b"])), Some("nope")).is_err());
        assert_eq!(s.music()[0].members, v(&["a"]), "the active grouping must be untouched");
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
        let mg = s.create_music("Downstairs", v(&["kitchen", "hall"]), None).unwrap();
        let solo = s.create_music("Kitchen Only", v(&["kitchen-2"]), None).unwrap();
        // A second preset holding the same speaker: it has to be swept as well,
        // or activating that preset would bring the removed output back.
        let party = s.create_preset("House party", Some(DEFAULT_PRESET_ID)).unwrap();
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
        // The inactive preset was swept too — both removed speakers are gone from
        // its copy, and `hall` (which was never removed) is still there.
        let in_party = reloaded.music_in(&party.id);
        assert_eq!(in_party.iter().find(|g| g.id == mg.id).unwrap().members, v(&["hall"]));
        assert!(in_party.iter().find(|g| g.id == solo.id).unwrap().members.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ids_are_unique_slugs_and_persist_across_reload() {
        let path = temp("persist");
        let _ = std::fs::remove_file(&path);
        let mut s = GroupsStore::load(&path).unwrap();
        let a = s.create_music("Living Room", v(&["x"]), None).unwrap();
        let b = s.create_music("Living Room", v(&["y"]), None).unwrap();
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
