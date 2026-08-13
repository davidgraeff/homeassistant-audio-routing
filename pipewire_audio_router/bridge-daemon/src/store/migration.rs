//! One-time migration: **drop-raop 2026-07**.
//!
//! The AirPlay-1/RAOP *output* path (`raop-out-<slug>`) was removed (Phase 6);
//! each physical receiver is now reached via its AirPlay-2 output
//! (`ap2-dev-<slug>`). Both node names derive from `slugify(<mDNS instance
//! label>)`, and in the tested fleet a receiver advertises the same label on
//! `_raop._tcp` and `_airplay._tcp`, so the slug is stable across the switch.
//!
//! This shim rewrites any persisted `raop-out-<slug>` reference in the routing
//! intent (routing.json) and the named music/announcement groups (groups.json)
//! to `ap2-dev-<slug>` **before** the reconcilers first read them — otherwise a
//! link saved under the old output path would dangle (grey, no audio). It runs
//! at startup, is **idempotent** (a boot that finds no `raop-out-*` does
//! nothing), and only ever *rewrites* — it never invents links. Every rewrite is
//! logged, and a resulting `ap2-dev-*` that no discovered device later matches
//! stays a harmless grey link the user can re-link once.
//!
//! Two other kinds of stale state need no code here: the old RAOP output store
//! (`/data/raop-outputs.json`) is simply left/ignored (AP2 devices are
//! discovered, not stored), and the stale `sync-settings.raop_latency` /
//! `settings.default_raop_latency_ms` keys are dropped automatically by serde on
//! the next save (their struct fields no longer exist).
//!
//! Idempotent + cheap, so it can be deleted once the deployment has booted once.

use crate::store::groups::GroupsStore;
use crate::store::routing::RoutingStore;
use crate::util::node_names::{slugify, AP2_DEV_PREFIX};
use std::path::Path;

/// The removed RAOP output node-name prefix (was `raop::RAOP_NODE_PREFIX`).
const RAOP_OUT_PREFIX: &str = "raop-out-";

/// Rewrite `raop-out-<slug>` → `ap2-dev-<slug>`, or return `None` if `name`
/// isn't a RAOP output node. The slug is re-`slugify`'d defensively so the
/// result matches `ap2_discovery`'s node-name convention exactly.
fn rewrite(name: &str) -> Option<String> {
    name.strip_prefix(RAOP_OUT_PREFIX).map(|slug| format!("{AP2_DEV_PREFIX}{}", slugify(slug)))
}

/// Run the one-time drop-raop rewrite over the routing + groups stores.
/// Best-effort: a load/persist error is logged, never fatal (a fresh install
/// with no files, or a store that can't be read, just leaves the graph as-is).
pub fn migrate_raop_prefixes(routing_path: &Path, groups_path: &Path) {
    migrate_routing(routing_path);
    migrate_groups(groups_path);
}

fn migrate_routing(routing_path: &Path) {
    if !routing_path.exists() {
        return;
    }
    let mut store = match RoutingStore::load(routing_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("drop-raop migration: could not load routing store {}: {e}", routing_path.display());
            return;
        }
    };
    // Snapshot the links that need rewriting (source, old_output, new_output).
    let rewrites: Vec<(String, String, String)> =
        store.links().filter_map(|l| rewrite(&l.output).map(|new| (l.source.clone(), l.output.clone(), new))).collect();
    if rewrites.is_empty() {
        return;
    }
    for (source, old_output, new_output) in rewrites {
        // Remove the RAOP link, add the AP2 one (both persist). add() is a no-op
        // if the AP2 link already exists (idempotent re-run).
        if let Err(e) = store.remove(&source, &old_output) {
            tracing::warn!("drop-raop migration: failed to drop '{source}' → '{old_output}': {e}");
            continue;
        }
        if let Err(e) = store.add(&source, &new_output) {
            tracing::warn!("drop-raop migration: failed to add '{source}' → '{new_output}': {e}");
            continue;
        }
        tracing::info!("drop-raop migration: rewrote routing link '{source}' → '{old_output}' ⇒ '{new_output}'");
    }
}

fn migrate_groups(groups_path: &Path) {
    if !groups_path.exists() {
        return;
    }
    let mut store = match GroupsStore::load(groups_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("drop-raop migration: could not load groups store {}: {e}", groups_path.display());
            return;
        }
    };

    // Music groups: rewrite member lists that contain a raop-out-* output. Per
    // preset, since that is where membership lives — a file this old has only the
    // `Default` preset the groups store just migrated it into, but a stale member
    // in any other preset would come back when that preset is activated.
    let preset_ids: Vec<String> = store.presets().iter().map(|p| p.id.clone()).collect();
    let music_rewrites: Vec<(String, String, Vec<String>)> = preset_ids
        .iter()
        .flat_map(|preset| {
            store.music_in(preset).into_iter().filter_map(move |g| {
                let has_raop = g.members.iter().any(|m| m.starts_with(RAOP_OUT_PREFIX));
                has_raop
                    .then(|| (preset.clone(), g.id.clone(), g.members.iter().map(|m| rewrite(m).unwrap_or_else(|| m.clone())).collect()))
            })
        })
        .collect();
    for (preset, id, members) in music_rewrites {
        match store.update_music(&id, None, Some(members), Some(&preset)) {
            Ok(g) => tracing::info!("drop-raop migration: rewrote music group '{id}' members in preset '{preset}' ⇒ {:?}", g.members),
            Err(e) => tracing::warn!("drop-raop migration: failed to rewrite music group '{id}' in preset '{preset}': {e}"),
        }
    }

    // Announcement groups: same, over their target lists.
    let ann_rewrites: Vec<(String, Vec<String>)> = store
        .announcement()
        .iter()
        .filter_map(|g| {
            let has_raop = g.targets.iter().any(|t| t.starts_with(RAOP_OUT_PREFIX));
            has_raop.then(|| (g.id.clone(), g.targets.iter().map(|t| rewrite(t).unwrap_or_else(|| t.clone())).collect()))
        })
        .collect();
    for (id, targets) in ann_rewrites {
        match store.update_announcement(&id, None, Some(targets), None, None) {
            Ok(g) => tracing::info!("drop-raop migration: rewrote announcement group '{id}' targets ⇒ {:?}", g.targets),
            Err(e) => tracing::warn!("drop-raop migration: failed to rewrite announcement group '{id}': {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_maps_raop_out_to_ap2_dev_keeping_slug() {
        assert_eq!(rewrite("raop-out-dusche").as_deref(), Some("ap2-dev-dusche"));
        assert_eq!(rewrite("raop-out-pioneer_vsx_934_f11b89").as_deref(), Some("ap2-dev-pioneer_vsx_934_f11b89"));
    }

    #[test]
    fn rewrite_ignores_non_raop_outputs() {
        assert_eq!(rewrite("ap2-dev-dusche"), None);
        assert_eq!(rewrite("sendspin-dev-kitchen"), None);
        assert_eq!(rewrite("airplay-in"), None);
    }
}
