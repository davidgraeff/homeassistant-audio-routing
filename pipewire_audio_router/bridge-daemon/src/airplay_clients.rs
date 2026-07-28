//! Persistent registry of AirPlay senders that have connected to the receive
//! sources (airplay_source.rs). Drives the Sources-tab "who's/who was connected"
//! list and the priority/ban/force-disconnect controls (name has precedence,
//! liveness rebuilt from callbacks).
//!
//! **Per-receiver (Phase 4).** With multiple concurrent AirPlay receivers, the
//! connection list, ban list, priorities, and anti-takeover state are keyed by
//! **source id**: one backing file (`airplay_clients.json`) holds a `sources`
//! map of `id -> [client...]`, and each running receiver + each API call
//! operates on a per-source *view* ([`AirplayClientRegistry`]) obtained from the
//! shared [`AirplayClientStore`]. The whole thing is behind one `Mutex`, so the
//! (frequent) connect/disconnect writes stay consistent across receivers.
//!
//! Identity, and why **name has precedence**: a TCP connect only gives us the
//! peer IP (`mark_connected`); the sender's friendly name (e.g. "David's
//! iPhone") arrives a beat later in the RTSP handshake (`set_name`). A phone's
//! IP changes with DHCP, its name doesn't — so once we've learned a name we key
//! the client on it and treat the IP as merely the last-seen address. Clients
//! we've only ever seen by IP are keyed on the IP until a name shows up, at
//! which point any earlier name-keyed record for the same sender is merged in.
//!
//! `connected` is live-only: it's `#[serde(skip)]`, so a reload starts with
//! everything disconnected (nothing is streaming at boot) and the flag is
//! rebuilt from the connect/disconnect callbacks.

use crate::locks::LockRecover;
use crate::sources_store::{SourceId, LEGACY_AIRPLAY_ID};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix seconds now, or 0 if the clock is before the epoch (never, in practice).
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// One remembered AirPlay sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AirplayClient {
    /// Friendly name the sender advertised, once we've learned one. `None` until
    /// then (client is keyed on `addr` in the meantime).
    #[serde(default)]
    pub name: Option<String>,
    /// Most recent IP address this client connected from.
    pub addr: String,
    /// Unix seconds of the first time this client was ever seen.
    pub first_seen: u64,
    /// Unix seconds of this client's most recent connection.
    pub last_connected: u64,
    /// Banned: refuse this client's future sessions (enforced at RTSP SETUP via
    /// `authorize_session`). Persisted — a ban outlives restarts. Does not evict
    /// an already-streaming session (that's force-disconnect).
    #[serde(default)]
    pub banned: bool,
    /// Priority for takeover arbitration: a connecting client with a strictly
    /// higher priority than the current one takes the session over; otherwise
    /// the anti-takeover policy decides. Default 0. Persisted.
    #[serde(default)]
    pub priority: i32,
    /// Connected right now. Live-only — never persisted (rebuilt from callbacks).
    #[serde(skip)]
    pub connected: bool,
}

impl AirplayClient {
    /// Stable key for the UI / forget calls: the name if known (it has
    /// precedence and outlives the IP), else the IP.
    pub fn key(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.addr)
    }
}

/// Case-insensitive, trimmed name comparison — senders aren't guaranteed to be
/// byte-identical across connections.
fn same_name(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Persisted per-source client list. Wrapped in an object so the file shape can
/// grow per-source fields later without another migration.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SourceClients {
    #[serde(default)]
    clients: Vec<AirplayClient>,
}

/// New (v2) on-disk shape: `{ "sources": { "<id>": { "clients": [...] } } }`.
/// The `sources` key is required (no serde default) so a legacy v1 file — which
/// has a top-level `clients` array and no `sources` — fails to parse here and
/// falls through to the v1 migration path in [`load_by_source`].
#[derive(Debug, Deserialize)]
struct ClientsFileV2 {
    sources: BTreeMap<SourceId, SourceClients>,
}

/// Legacy (v1) shape: a single flat `clients` array (the singular-receiver era).
/// Migrated to `sources[LEGACY_AIRPLAY_ID]` on load.
#[derive(Debug, Deserialize)]
struct ClientsFileV1 {
    clients: Vec<AirplayClient>,
}

/// Serialize shape for [`ClientsDb::persist`] (owned, so it can borrow the map).
#[derive(Debug, Serialize)]
struct ClientsFileOut<'a> {
    sources: BTreeMap<&'a SourceId, SourceClientsOut<'a>>,
}

#[derive(Debug, Serialize)]
struct SourceClientsOut<'a> {
    clients: &'a [AirplayClient],
}

/// Backing store: the whole `airplay_clients.json`, one client list per source
/// id. All mutation goes through here (behind the [`AirplayClientStore`] mutex)
/// and rewrites the file atomically.
struct ClientsDb {
    path: PathBuf,
    by_source: BTreeMap<SourceId, Vec<AirplayClient>>,
}

/// Parse the file at `path` into the per-source map, tolerating an
/// empty/truncated or corrupt file (this file is rewritten on every connect/
/// disconnect, so an interrupted write can truncate it) and migrating a legacy
/// v1 flat `clients` array under [`LEGACY_AIRPLAY_ID`].
fn load_by_source(path: &Path) -> anyhow::Result<BTreeMap<SourceId, Vec<AirplayClient>>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading airplay clients {}: {e}", path.display()))?;
    if raw.trim().is_empty() {
        tracing::warn!("airplay clients {} is empty; starting with none", path.display());
        return Ok(BTreeMap::new());
    }
    // Try the NEW per-source shape first.
    if let Ok(v2) = serde_json::from_str::<ClientsFileV2>(&raw) {
        return Ok(v2.sources.into_iter().map(|(id, sc)| (id, sc.clients)).collect());
    }
    // LEGACY flat shape → migrate the whole list under the legacy AirPlay id
    // (the singular receiver's node name / routing key), so old remembered
    // clients keep their bans/priorities on the migrated source.
    if let Ok(v1) = serde_json::from_str::<ClientsFileV1>(&raw) {
        let mut by_source = BTreeMap::new();
        if !v1.clients.is_empty() {
            by_source.insert(LEGACY_AIRPLAY_ID.to_string(), v1.clients);
        }
        return Ok(by_source);
    }
    tracing::warn!("airplay clients {} is corrupt; starting with none", path.display());
    Ok(BTreeMap::new())
}

impl ClientsDb {
    /// A snapshot of one source's clients, most-recently-connected first.
    fn list(&self, sid: &str) -> Vec<AirplayClient> {
        let mut out = self.by_source.get(sid).cloned().unwrap_or_default();
        out.sort_by(|a, b| b.last_connected.cmp(&a.last_connected));
        out
    }

    fn mark_connected(&mut self, sid: &str, addr: &str) {
        let now = now_secs();
        let clients = self.by_source.entry(sid.to_string()).or_default();
        if let Some(c) = clients.iter_mut().find(|c| c.addr == addr) {
            c.last_connected = now;
            c.connected = true;
        } else {
            clients.push(AirplayClient {
                name: None,
                addr: addr.to_string(),
                first_seen: now,
                last_connected: now,
                banned: false,
                priority: 0,
                connected: true,
            });
        }
        self.persist();
    }

    fn set_name(&mut self, sid: &str, addr: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let clients = self.by_source.entry(sid.to_string()).or_default();
        // The record this live connection is currently attributed to.
        let Some(cur_idx) = clients.iter().position(|c| c.addr == addr && c.connected) else {
            return;
        };
        // An existing (different) record already known by this name → merge.
        if let Some(named_idx) = clients.iter().position(|c| c.name.as_deref().is_some_and(|n| same_name(n, name))) {
            if named_idx != cur_idx {
                let cur = clients.remove(cur_idx);
                // Removing shifts indices; re-find the named record.
                let named = clients.iter_mut().find(|c| c.name.as_deref().is_some_and(|n| same_name(n, name))).unwrap();
                named.addr = cur.addr;
                named.connected = true;
                named.first_seen = named.first_seen.min(cur.first_seen);
                named.last_connected = named.last_connected.max(cur.last_connected);
                self.persist();
                return;
            }
        }
        clients[cur_idx].name = Some(name.to_string());
        self.persist();
    }

    fn mark_disconnected(&mut self, sid: &str, addr: &str) {
        let Some(clients) = self.by_source.get_mut(sid) else { return };
        let mut changed = false;
        for c in clients.iter_mut().filter(|c| c.addr == addr && c.connected) {
            c.connected = false;
            changed = true;
        }
        if changed {
            self.persist();
        }
    }

    fn is_banned(&self, sid: &str, addr: &str, name: Option<&str>) -> bool {
        let Some(clients) = self.by_source.get(sid) else { return false };
        clients
            .iter()
            .filter(|c| c.banned)
            .any(|c| c.addr == addr || name.is_some_and(|n| c.name.as_deref().is_some_and(|cn| same_name(cn, n))))
    }

    fn set_banned(&mut self, sid: &str, key: &str, banned: bool) -> bool {
        let Some(clients) = self.by_source.get_mut(sid) else { return false };
        let mut found = false;
        for c in clients.iter_mut().filter(|c| c.key() == key) {
            c.banned = banned;
            found = true;
        }
        if found {
            self.persist();
        }
        found
    }

    fn set_priority(&mut self, sid: &str, key: &str, priority: i32) -> bool {
        let Some(clients) = self.by_source.get_mut(sid) else { return false };
        let mut found = false;
        for c in clients.iter_mut().filter(|c| c.key() == key) {
            c.priority = priority;
            found = true;
        }
        if found {
            self.persist();
        }
        found
    }

    fn priority_of(&self, sid: &str, addr: &str, name: Option<&str>) -> i32 {
        let Some(clients) = self.by_source.get(sid) else { return 0 };
        clients
            .iter()
            .find(|c| name.is_some_and(|n| c.name.as_deref().is_some_and(|cn| same_name(cn, n))) || c.addr == addr)
            .map(|c| c.priority)
            .unwrap_or(0)
    }

    fn connected_addr(&self, sid: &str, key: &str) -> Option<String> {
        self.by_source.get(sid)?.iter().find(|c| c.key() == key && c.connected).map(|c| c.addr.clone())
    }

    fn reset_connected(&mut self, sid: &str) {
        if let Some(clients) = self.by_source.get_mut(sid) {
            for c in clients.iter_mut() {
                c.connected = false;
            }
        }
    }

    fn forget(&mut self, sid: &str, key: &str) -> bool {
        let Some(clients) = self.by_source.get_mut(sid) else { return false };
        let before = clients.len();
        clients.retain(|c| c.connected || c.key() != key);
        let removed = clients.len() != before;
        if removed {
            self.persist();
        }
        removed
    }

    fn total_clients(&self) -> usize {
        self.by_source.values().map(|v| v.len()).sum()
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let out = ClientsFileOut {
            sources: self.by_source.iter().map(|(id, clients)| (id, SourceClientsOut { clients })).collect(),
        };
        match serde_json::to_string_pretty(&out) {
            Ok(json) => {
                // Atomic write: write a temp file then rename over the target. rename()
                // is atomic on the same filesystem, so a crash/restart mid-write can't
                // leave the real file truncated to 0 bytes (which crash-looped the daemon
                // on the next boot — this file is rewritten on every connect/disconnect).
                let tmp = self.path.with_extension("json.tmp");
                let res = std::fs::write(&tmp, json.as_bytes()).and_then(|_| std::fs::rename(&tmp, &self.path));
                if let Err(e) = res {
                    tracing::warn!("failed to persist airplay clients {}: {e}", self.path.display());
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            Err(e) => tracing::warn!("failed to serialize airplay clients: {e}"),
        }
    }
}

/// The whole `airplay_clients.json`, shared behind one mutex. Held by `AppState`
/// (API reads/writes for `/api/sources/{id}/clients/*`) and used by `main.rs` /
/// the AirPlay reconciler to hand each running receiver its own per-source
/// [`AirplayClientRegistry`]. Clone-cheap (an `Arc`).
#[derive(Clone)]
pub struct AirplayClientStore {
    db: Arc<Mutex<ClientsDb>>,
}

impl AirplayClientStore {
    /// Load from `path`, migrating a legacy flat file to the per-source shape.
    /// Starts empty if the file doesn't exist yet (written on the first
    /// mutation). All clients load `connected = false`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let by_source = load_by_source(path)?;
        Ok(Self { db: Arc::new(Mutex::new(ClientsDb { path: path.to_path_buf(), by_source })) })
    }

    /// A per-source view onto the store, for one AirPlay receiver / one API id.
    pub fn registry(&self, source_id: &str) -> SharedAirplayClients {
        AirplayClientRegistry { db: self.db.clone(), source_id: source_id.to_string() }
    }

    /// Total remembered clients across all sources (for the boot log).
    pub fn total_clients(&self) -> usize {
        self.db.lock_recover().total_clients()
    }
}

/// A per-source handle onto the shared store: everything a single receiver's
/// callbacks and a single source's API routes need. Clone-cheap; mutation is
/// serialized by the store's inner mutex, so all methods take `&self`.
#[derive(Clone)]
pub struct AirplayClientRegistry {
    db: Arc<Mutex<ClientsDb>>,
    source_id: SourceId,
}

/// Shared handle held by a running receiver's `Handler` (callback writes) and by
/// the API (reads/mutations) for one source id. Per-receiver as of Phase 4.
pub type SharedAirplayClients = AirplayClientRegistry;

impl AirplayClientRegistry {
    /// Snapshot for the API, most-recently-connected first.
    pub fn list(&self) -> Vec<AirplayClient> {
        self.db.lock_recover().list(&self.source_id)
    }

    /// A connection opened from `addr`. We only know the IP here; the name (if
    /// any) follows via [`set_name`]. Attributes it to the existing record for
    /// that IP, or creates one.
    pub fn mark_connected(&self, addr: &str) {
        self.db.lock_recover().mark_connected(&self.source_id, addr);
    }

    /// The sender at `addr` identified itself as `name`. Name has precedence: if
    /// another record already carries this name, fold the current IP-keyed
    /// connection into it; otherwise stamp the name onto the current record.
    pub fn set_name(&self, addr: &str, name: &str) {
        self.db.lock_recover().set_name(&self.source_id, addr, name);
    }

    /// The connection from `addr` closed.
    pub fn mark_disconnected(&self, addr: &str) {
        self.db.lock_recover().mark_disconnected(&self.source_id, addr);
    }

    /// Whether a sender identified by `addr` and/or `name` is banned on this
    /// source (name has precedence, but either match bans).
    pub fn is_banned(&self, addr: &str, name: Option<&str>) -> bool {
        self.db.lock_recover().is_banned(&self.source_id, addr, name)
    }

    /// Set/clear the ban on the client with this [`AirplayClient::key`]. Returns
    /// whether a client matched.
    pub fn set_banned(&self, key: &str, banned: bool) -> bool {
        self.db.lock_recover().set_banned(&self.source_id, key, banned)
    }

    /// Set the takeover priority of the client with this key. Returns whether a
    /// client matched.
    pub fn set_priority(&self, key: &str, priority: i32) -> bool {
        self.db.lock_recover().set_priority(&self.source_id, key, priority)
    }

    /// The stored priority of the client identified by `addr`/`name` (name has
    /// precedence), or 0 if we've never seen it on this source.
    pub fn priority_of(&self, addr: &str, name: Option<&str>) -> i32 {
        self.db.lock_recover().priority_of(&self.source_id, addr, name)
    }

    /// The most recent IP of the connected client with this key, for
    /// force-disconnect. `None` if unknown or not currently connected.
    pub fn connected_addr(&self, key: &str) -> Option<String> {
        self.db.lock_recover().connected_addr(&self.source_id, key)
    }

    /// Clear all live-connection flags for this source — used when the receiver
    /// (re)starts, so a stale "connected" can't survive a restart that skipped a
    /// disconnect.
    pub fn reset_connected(&self) {
        self.db.lock_recover().reset_connected(&self.source_id);
    }

    /// Forget a remembered client by its [`AirplayClient::key`]. Refuses to
    /// forget a currently-connected client. Returns whether one was removed.
    pub fn forget(&self, key: &str) -> bool {
        self.db.lock_recover().forget(&self.source_id, key)
    }
}

/// Convenience for the callback path: record a connect.
pub fn on_connected(clients: &SharedAirplayClients, addr: &str) {
    clients.mark_connected(addr);
}

/// Convenience for the callback path: record a name.
pub fn on_named(clients: &SharedAirplayClients, addr: &str, name: &str) {
    clients.set_name(addr, name);
}

/// Convenience for the callback path: record a disconnect.
pub fn on_disconnected(clients: &SharedAirplayClients, addr: &str) {
    clients.mark_disconnected(addr);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("airplay-clients-{tag}-{}-{:?}.json", std::process::id(), std::thread::current().id()))
    }

    #[test]
    fn connect_then_name_stamps_the_record() {
        let path = temp_path("stamp");
        let _ = std::fs::remove_file(&path);
        let store = AirplayClientStore::load(&path).unwrap();
        let r = store.registry(LEGACY_AIRPLAY_ID);
        r.mark_connected("192.168.1.5");
        r.set_name("192.168.1.5", "David's iPhone");
        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("David's iPhone"));
        assert!(list[0].connected);
        assert_eq!(list[0].key(), "David's iPhone");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn same_name_from_new_ip_merges_not_duplicates() {
        let path = temp_path("merge");
        let _ = std::fs::remove_file(&path);
        let store = AirplayClientStore::load(&path).unwrap();
        let r = store.registry(LEGACY_AIRPLAY_ID);
        // First session, learns the name, then disconnects.
        r.mark_connected("192.168.1.5");
        r.set_name("192.168.1.5", "David's iPhone");
        r.mark_disconnected("192.168.1.5");
        // Reconnects on a new DHCP lease under the same name.
        r.mark_connected("192.168.1.9");
        r.set_name("192.168.1.9", "David's iPhone");
        let list = r.list();
        assert_eq!(list.len(), 1, "same name → one client, not two");
        assert_eq!(list[0].addr, "192.168.1.9", "addr updated to latest");
        assert!(list[0].connected);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn forget_removes_disconnected_only_and_persists() {
        let path = temp_path("forget");
        let _ = std::fs::remove_file(&path);
        let store = AirplayClientStore::load(&path).unwrap();
        let r = store.registry(LEGACY_AIRPLAY_ID);
        r.mark_connected("10.0.0.2");
        r.set_name("10.0.0.2", "Kitchen Mac");
        // Can't forget while connected.
        assert!(!r.forget("Kitchen Mac"));
        r.mark_disconnected("10.0.0.2");
        assert!(r.forget("Kitchen Mac"));
        // Persisted empty across reload.
        assert!(AirplayClientStore::load(&path).unwrap().registry(LEGACY_AIRPLAY_ID).list().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ip_only_client_is_kept_and_keyed_by_ip() {
        let path = temp_path("iponly");
        let _ = std::fs::remove_file(&path);
        let store = AirplayClientStore::load(&path).unwrap();
        let r = store.registry(LEGACY_AIRPLAY_ID);
        r.mark_connected("172.16.0.4");
        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, None);
        assert_eq!(list[0].key(), "172.16.0.4");
        // connected flag does not survive a reload.
        r.mark_disconnected("172.16.0.4");
        assert!(!AirplayClientStore::load(&path).unwrap().registry(LEGACY_AIRPLAY_ID).list()[0].connected);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn registries_are_isolated_per_source() {
        // Two receivers keep independent client lists, bans and priorities in
        // the one backing file.
        let path = temp_path("per-source");
        let _ = std::fs::remove_file(&path);
        let store = AirplayClientStore::load(&path).unwrap();
        let kitchen = store.registry("kitchen");
        let office = store.registry("office");
        kitchen.mark_connected("192.168.1.5");
        kitchen.set_name("192.168.1.5", "David's iPhone");
        office.mark_connected("192.168.1.6");
        office.set_name("192.168.1.6", "Work Mac");
        assert_eq!(kitchen.list().len(), 1);
        assert_eq!(office.list().len(), 1);
        assert_eq!(kitchen.list()[0].key(), "David's iPhone");
        assert_eq!(office.list()[0].key(), "Work Mac");
        // A ban on one source doesn't affect the other.
        assert!(kitchen.set_banned("David's iPhone", true));
        assert!(kitchen.is_banned("192.168.1.5", Some("David's iPhone")));
        assert!(!office.is_banned("192.168.1.5", Some("David's iPhone")));
        // Reload keeps both sources separate.
        drop(store);
        let reloaded = AirplayClientStore::load(&path).unwrap();
        assert_eq!(reloaded.registry("kitchen").list().len(), 1);
        assert_eq!(reloaded.registry("office").list().len(), 1);
        assert!(reloaded.registry("kitchen").list()[0].banned);
        assert_eq!(reloaded.total_clients(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrates_legacy_flat_file_under_legacy_id() {
        // A v1 flat `{ "clients": [...] }` file loads under the legacy AirPlay id
        // so old bans/priorities survive, and is rewritten in the v2 shape.
        let path = temp_path("migrate-v1");
        let _ = std::fs::remove_file(&path);
        let v1 = r#"{ "clients": [
            { "name": "Old Phone", "addr": "192.168.1.2", "first_seen": 10, "last_connected": 20, "banned": true, "priority": 5 }
        ] }"#;
        std::fs::write(&path, v1).unwrap();
        let store = AirplayClientStore::load(&path).unwrap();
        let r = store.registry(LEGACY_AIRPLAY_ID);
        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key(), "Old Phone");
        assert!(list[0].banned);
        assert_eq!(list[0].priority, 5);
        assert!(!list[0].connected); // #[serde(skip)] → false after load
        // Rewrite it (any mutation) and confirm the new shape reloads.
        r.set_priority("Old Phone", 7);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"sources\""));
        let reloaded = AirplayClientStore::load(&path).unwrap();
        assert_eq!(reloaded.registry(LEGACY_AIRPLAY_ID).list()[0].priority, 7);
        let _ = std::fs::remove_file(&path);
    }
}
