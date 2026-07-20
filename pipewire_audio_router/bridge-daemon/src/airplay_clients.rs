//! Persistent registry of AirPlay senders that have connected to the receive
//! source (airplay_source.rs). Drives the Sources-tab "who's/who was connected"
//! list, and is the data model the later priority/ban/force-disconnect controls
//! will hang off (nothing here decides policy yet — it only records identity +
//! liveness).
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
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct ClientsFile {
    #[serde(default)]
    clients: Vec<AirplayClient>,
}

pub struct AirplayClientRegistry {
    path: PathBuf,
    clients: Vec<AirplayClient>,
}

/// Shared handle held by AppState (API reads) and the AirPlay Handler (callback
/// writes).
pub type SharedAirplayClients = Arc<Mutex<AirplayClientRegistry>>;

impl AirplayClientRegistry {
    /// Load from `path`, or start empty if it doesn't exist yet (written on the
    /// first mutation). Mirrors sources_store.rs. All clients load
    /// `connected = false`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let clients = if path.exists() {
            let raw =
                std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading airplay clients {}: {e}", path.display()))?;
            let file: ClientsFile =
                serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing airplay clients {}: {e}", path.display()))?;
            file.clients
        } else {
            Vec::new()
        };
        Ok(Self { path: path.to_path_buf(), clients })
    }

    pub fn shared(self) -> SharedAirplayClients {
        Arc::new(Mutex::new(self))
    }

    /// Snapshot for the API, most-recently-connected first.
    pub fn list(&self) -> Vec<AirplayClient> {
        let mut out = self.clients.clone();
        out.sort_by(|a, b| b.last_connected.cmp(&a.last_connected));
        out
    }

    /// A connection opened from `addr`. We only know the IP here; the name (if
    /// any) follows via [`set_name`]. Attributes it to the existing record for
    /// that IP, or creates one.
    pub fn mark_connected(&mut self, addr: &str) {
        let now = now_secs();
        if let Some(c) = self.clients.iter_mut().find(|c| c.addr == addr) {
            c.last_connected = now;
            c.connected = true;
        } else {
            self.clients.push(AirplayClient {
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

    /// The sender at `addr` identified itself as `name`. Name has precedence: if
    /// another record already carries this name, fold the current IP-keyed
    /// connection into it (keeping the oldest `first_seen`, newest
    /// `last_connected`); otherwise just stamp the name onto the current record.
    pub fn set_name(&mut self, addr: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        // The record this live connection is currently attributed to.
        let Some(cur_idx) = self.clients.iter().position(|c| c.addr == addr && c.connected) else {
            return;
        };
        // An existing (different) record already known by this name → merge.
        if let Some(named_idx) = self
            .clients
            .iter()
            .position(|c| c.name.as_deref().is_some_and(|n| same_name(n, name)))
        {
            if named_idx != cur_idx {
                let cur = self.clients.remove(cur_idx);
                // Removing shifts indices; re-find the named record.
                let named = self.clients.iter_mut().find(|c| c.name.as_deref().is_some_and(|n| same_name(n, name))).unwrap();
                named.addr = cur.addr;
                named.connected = true;
                named.first_seen = named.first_seen.min(cur.first_seen);
                named.last_connected = named.last_connected.max(cur.last_connected);
                self.persist();
                return;
            }
        }
        self.clients[cur_idx].name = Some(name.to_string());
        self.persist();
    }

    /// The connection from `addr` closed.
    pub fn mark_disconnected(&mut self, addr: &str) {
        let mut changed = false;
        for c in self.clients.iter_mut().filter(|c| c.addr == addr && c.connected) {
            c.connected = false;
            changed = true;
        }
        if changed {
            self.persist();
        }
    }

    /// Whether a sender identified by `addr` and/or `name` is banned. Name has
    /// precedence but either match bans (a banned name blocks even from a new
    /// IP; a banned IP blocks even before a name is known).
    pub fn is_banned(&self, addr: &str, name: Option<&str>) -> bool {
        self.clients.iter().filter(|c| c.banned).any(|c| {
            c.addr == addr || name.is_some_and(|n| c.name.as_deref().is_some_and(|cn| same_name(cn, n)))
        })
    }

    /// Set/clear the ban on the client with this [`AirplayClient::key`]. Returns
    /// whether a client matched. Banning a currently-connected client takes
    /// effect on its next session (it isn't evicted here).
    pub fn set_banned(&mut self, key: &str, banned: bool) -> bool {
        let mut found = false;
        for c in self.clients.iter_mut().filter(|c| c.key() == key) {
            c.banned = banned;
            found = true;
        }
        if found {
            self.persist();
        }
        found
    }

    /// Set the takeover priority of the client with this key. Returns whether a
    /// client matched.
    pub fn set_priority(&mut self, key: &str, priority: i32) -> bool {
        let mut found = false;
        for c in self.clients.iter_mut().filter(|c| c.key() == key) {
            c.priority = priority;
            found = true;
        }
        if found {
            self.persist();
        }
        found
    }

    /// The stored priority of the client identified by `addr`/`name` (name has
    /// precedence), or 0 if we've never seen it.
    pub fn priority_of(&self, addr: &str, name: Option<&str>) -> i32 {
        self.clients
            .iter()
            .find(|c| name.is_some_and(|n| c.name.as_deref().is_some_and(|cn| same_name(cn, n))) || c.addr == addr)
            .map(|c| c.priority)
            .unwrap_or(0)
    }

    /// The most recent IP of the connected client with this key, for
    /// force-disconnect. `None` if unknown or not currently connected.
    pub fn connected_addr(&self, key: &str) -> Option<String> {
        self.clients.iter().find(|c| c.key() == key && c.connected).map(|c| c.addr.clone())
    }

    /// Clear all live-connection flags — used when the receiver (re)starts, so a
    /// stale "connected" can't survive a restart that skipped a disconnect.
    pub fn reset_connected(&mut self) {
        for c in self.clients.iter_mut() {
            c.connected = false;
        }
    }

    /// Forget a remembered client by its [`AirplayClient::key`]. Refuses to
    /// forget a currently-connected client (there'd be nothing to forget — it's
    /// live). Returns whether a client was removed.
    pub fn forget(&mut self, key: &str) -> bool {
        let before = self.clients.len();
        self.clients.retain(|c| c.connected || c.key() != key);
        let removed = self.clients.len() != before;
        if removed {
            self.persist();
        }
        removed
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&ClientsFile { clients: self.clients.clone() }) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!("failed to persist airplay clients {}: {e}", self.path.display());
                }
            }
            Err(e) => tracing::warn!("failed to serialize airplay clients: {e}"),
        }
    }
}

/// Convenience for the callback path: lock and record a connect.
pub fn on_connected(clients: &SharedAirplayClients, addr: &str) {
    clients.lock_recover().mark_connected(addr);
}

/// Convenience for the callback path: lock and record a name.
pub fn on_named(clients: &SharedAirplayClients, addr: &str, name: &str) {
    clients.lock_recover().set_name(addr, name);
}

/// Convenience for the callback path: lock and record a disconnect.
pub fn on_disconnected(clients: &SharedAirplayClients, addr: &str) {
    clients.lock_recover().mark_disconnected(addr);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("airplay-clients-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn connect_then_name_stamps_the_record() {
        let path = temp_path("stamp");
        let _ = std::fs::remove_file(&path);
        let mut r = AirplayClientRegistry::load(&path).unwrap();
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
        let mut r = AirplayClientRegistry::load(&path).unwrap();
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
        let mut r = AirplayClientRegistry::load(&path).unwrap();
        r.mark_connected("10.0.0.2");
        r.set_name("10.0.0.2", "Kitchen Mac");
        // Can't forget while connected.
        assert!(!r.forget("Kitchen Mac"));
        r.mark_disconnected("10.0.0.2");
        assert!(r.forget("Kitchen Mac"));
        // Persisted empty across reload.
        assert!(AirplayClientRegistry::load(&path).unwrap().list().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ip_only_client_is_kept_and_keyed_by_ip() {
        let path = temp_path("iponly");
        let _ = std::fs::remove_file(&path);
        let mut r = AirplayClientRegistry::load(&path).unwrap();
        r.mark_connected("172.16.0.4");
        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, None);
        assert_eq!(list[0].key(), "172.16.0.4");
        // connected flag does not survive a reload.
        r.mark_disconnected("172.16.0.4");
        assert!(!AirplayClientRegistry::load(&path).unwrap().list()[0].connected);
        let _ = std::fs::remove_file(&path);
    }
}
