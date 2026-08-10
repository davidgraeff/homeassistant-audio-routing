//! Persistent, runtime-managed config for the daemon's input sources.
//!
//! Historically this stored *exactly one* AirPlay-receive source (a name +
//! three knobs) and *exactly one* Bluetooth-bridge RTP source. It is now a
//! **keyed collection** of sources of two kinds (AirPlay + RTP), so the user can
//! add/remove more than one of either type at runtime, each independently
//! routable. Mirrors outputs_store.rs: no `options.json` seeding — starts empty
//! on a fresh install, then the `/data` file is authoritative and everything is
//! managed live via the API (api.rs).
//!
//! Persisted shape (`sources.json`):
//!
//! ```jsonc
//! { "sources": [
//!   { "id": "airplay-in", "label": "Living Room", "kind": "airplay",
//!     "latency_msec": 100, "auth_setup": false, "prevent_takeover": true, "port": 5000 },
//!   { "id": "bt-bridge-rtp", "label": "Bluetooth Bridge", "kind": "rtp",
//!     "port": 46000, "latency_msec": 200, "source_addr": "0.0.0.0",
//!     "ignore_ssrc": true, "rate": 48000 }
//! ] }
//! ```
//!
//! Presence in the list = enabled (no separate flag), matching the old `Option`
//! semantics. On load, a legacy single-field file is migrated to this shape and
//! rewritten (idempotent, deletable-after-one-boot).
//!
//! During the multi-source refactor (Phases 2-4) the rest of the crate still
//! talks to the old singular API; the **back-compat shims** at the bottom of
//! `impl SourcesStore` implement those exact signatures over the collection
//! using the two legacy ids, so nothing else has to change yet.
//!
//! (Sendspin is not configured here: devices are auto-discovered
//! (sendspin_discovery.rs) and grouped from the routing intent
//! (sync_group.rs), so there's nothing per-output to persist.)

use crate::airplay_source::DEFAULT_AIRPLAY_LATENCY_MSEC;
use crate::rtp_source::{DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_PORT, DEFAULT_RTP_RATE, DEFAULT_RTP_SOURCE_ADDR};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Stable id = slug, also the base of the PipeWire node name and the routing
/// key. Immutable once created.
pub type SourceId = String;

/// The two kinds of input source. Serialized `snake_case` (`"airplay"`/`"rtp"`)
/// — it is the `kind` discriminator tag on the wire.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Airplay,
    Rtp,
}

/// AirPlay-receive source config. Gathers the four formerly-scattered
/// `airplay_*` fields into one struct; the serde defaults match the old
/// per-field defaults so a config written by an older daemon still loads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AirplaySourceConfig {
    /// AirPlay producer jitter-buffer target, ms.
    #[serde(default = "default_airplay_latency_msec")]
    pub latency_msec: u32,
    /// Whether to also advertise the MFi auth-setup encryption mode (`et=0,4`)
    /// so encryption-requiring senders can connect. Off by default (the
    /// PipeWire-safe unencrypted path).
    #[serde(default)]
    pub auth_setup: bool,
    /// When true, a new AirPlay sender is refused while another is already
    /// streaming (anti-takeover); when false, the legacy last-wins behavior.
    /// Defaults to `true` so old config files come up protected.
    #[serde(default = "default_true")]
    pub prevent_takeover: bool,
    /// RTSP port the `RaopServer` binds. Allocated on add (base 5000, first
    /// free among AirPlay sources), persisted so it is stable across restarts.
    /// `0` = "allocate on next load".
    #[serde(default)]
    pub port: u16,
}

impl Default for AirplaySourceConfig {
    fn default() -> Self {
        Self { latency_msec: DEFAULT_AIRPLAY_LATENCY_MSEC, auth_setup: false, prevent_takeover: true, port: 0 }
    }
}

/// A single RTP source (Bluetooth bridge firmware target). Two knobs: the UDP
/// `port` it listens on (must match the firmware's target) and the jitter-buffer
/// `latency_msec` (traded up on weak-signal installs to ride out dropped
/// packets). The rest of the wire format is fixed by the firmware — see
/// rtp_source.rs.
///
/// `latency_msec` has a `serde(default)` so a config file written by an older
/// daemon (port only) still loads, defaulting to the sane 200 ms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RtpSourceConfig {
    #[serde(default = "default_rtp_port")]
    pub port: u16,
    #[serde(default = "default_rtp_latency_msec")]
    pub latency_msec: u32,
    /// `source.ip`: `0.0.0.0` for unicast, or a multicast group to share one
    /// firmware stream across receivers. `serde(default)` for old config files.
    #[serde(default = "default_rtp_source_addr")]
    pub source_addr: String,
    /// `sess.ignore-ssrc`. `true` (default) accepts packets from any sender on
    /// the port; `false` latches onto the first SSRC and rejects the rest — the
    /// "Only one client" mode that stops a stray/second sender from corrupting
    /// the stream (needs a firmware with a stable SSRC). `serde(default)` keeps
    /// old config files — and installs with not-yet-reflashed bridges — on the
    /// safe `true`. See rtp_source.rs.
    #[serde(default = "default_rtp_ignore_ssrc")]
    pub ignore_ssrc: bool,
    /// `audio.rate` the receiver decodes at — must match the sender's wire rate.
    /// Defaults to 48000 (stay 48 kHz end-to-end, no resample); set 44100 for a
    /// sender that still transmits 44.1 kHz. `serde(default)` keeps old config
    /// files (which had no rate) loading — they default to 48000, which matches
    /// the updated Pi bridge; a still-44100 sender is a one-time re-save.
    #[serde(default = "default_rtp_rate")]
    pub rate: u32,
}

fn default_rtp_port() -> u16 {
    DEFAULT_RTP_PORT
}

fn default_rtp_rate() -> u32 {
    DEFAULT_RTP_RATE
}

fn default_rtp_latency_msec() -> u32 {
    DEFAULT_RTP_LATENCY_MSEC
}

fn default_rtp_source_addr() -> String {
    DEFAULT_RTP_SOURCE_ADDR.to_string()
}

fn default_rtp_ignore_ssrc() -> bool {
    DEFAULT_RTP_IGNORE_SSRC
}

fn default_airplay_latency_msec() -> u32 {
    DEFAULT_AIRPLAY_LATENCY_MSEC
}

fn default_true() -> bool {
    true
}

/// Per-kind config, internally tagged by `kind`. Flattened into [`SourceEntry`]
/// so an entry on the wire is a flat object: `{id, label, kind, <config...>}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    Airplay(AirplaySourceConfig),
    Rtp(RtpSourceConfig),
}

impl SourceConfig {
    /// The discriminator for this config.
    pub fn kind(&self) -> SourceKind {
        match self {
            SourceConfig::Airplay(_) => SourceKind::Airplay,
            SourceConfig::Rtp(_) => SourceKind::Rtp,
        }
    }
}

/// One persisted source: a stable `id`, a user-facing `label`, and the
/// kind-specific `config` (flattened, so `kind` and the config fields sit at the
/// top level of the object).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceEntry {
    pub id: SourceId,
    pub label: String,
    #[serde(flatten)]
    pub config: SourceConfig,
}

impl SourceEntry {
    /// The kind of this entry.
    pub fn kind(&self) -> SourceKind {
        self.config.kind()
    }

    /// The PipeWire node name this source presents as.
    pub fn node_name(&self) -> String {
        source_node_name(self.kind(), &self.id)
    }
}

/// The id assigned to the migrated single AirPlay source, kept equal to the old
/// fixed node name so existing `routing.json` links keep resolving.
pub const LEGACY_AIRPLAY_ID: &str = "airplay-in";
/// The id assigned to the migrated single RTP source (as above).
pub const LEGACY_RTP_ID: &str = "bt-bridge-rtp";

/// Base RTSP port for AirPlay receivers; instances are allocated upward from
/// here (first free).
const AIRPLAY_PORT_BASE: u16 = 5000;

/// Map a source `(kind, id)` to its PipeWire node name.
///
/// AirPlay → `airplay-in-<id>`, RTP → `rtp-in-<id>`, EXCEPT the two legacy ids
/// map to the bare legacy names (`airplay-in` / `bt-bridge-rtp`) so existing
/// routing links resolve unchanged.
pub fn source_node_name(kind: SourceKind, id: &str) -> String {
    match kind {
        SourceKind::Airplay if id == LEGACY_AIRPLAY_ID => LEGACY_AIRPLAY_ID.to_string(),
        SourceKind::Rtp if id == LEGACY_RTP_ID => LEGACY_RTP_ID.to_string(),
        SourceKind::Airplay => format!("airplay-in-{id}"),
        SourceKind::Rtp => format!("rtp-in-{id}"),
    }
}

/// New persisted shape: a flat list of sources. Presence = enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourcesConfig {
    sources: Vec<SourceEntry>,
}

/// Legacy single-field shape, parsed only as a migration fallback in [`load`].
#[derive(Debug, Clone, Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    airplay_source_name: Option<String>,
    #[serde(default = "default_airplay_latency_msec")]
    airplay_latency_msec: u32,
    #[serde(default)]
    airplay_auth_setup: bool,
    #[serde(default = "default_true")]
    airplay_prevent_takeover: bool,
    #[serde(default)]
    rtp_source: Option<RtpSourceConfig>,
}

pub struct SourcesStore {
    path: PathBuf,
    config: SourcesConfig,
}

/// Turn a user-facing label into a URL/node-name-safe slug: lowercase ASCII
/// alphanumerics, other runs collapsed to a single `-`, no leading/trailing `-`.
/// An empty result falls back to `"source"`.
fn slugify(label: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        out.push_str("source");
    }
    out
}

/// Empty/whitespace AirPlay name means "disabled" everywhere — normalize to
/// `None` so the rest of the code only deals with `Some(real name)`.
fn normalize_name(name: Option<String>) -> Option<String> {
    name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl SourcesStore {
    /// Load from `path`, migrating a legacy single-field file to the new
    /// collection shape (and rewriting it) if needed. Starts empty if the file
    /// doesn't exist yet (the file is created on the first mutation). No
    /// `options.json` seeding.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self { path: path.to_path_buf(), config: SourcesConfig { sources: Vec::new() } });
        }

        let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading sources store {}: {e}", path.display()))?;

        // Try the NEW shape first. The `sources` field is required (no serde
        // default), so a legacy file — which has no `sources` key — fails here
        // and falls through to the migration path below.
        if let Ok(config) = serde_json::from_str::<SourcesConfig>(&raw) {
            let mut store = Self { path: path.to_path_buf(), config };
            // Fill in any ports left at 0 (`allocate on next load`); only touch
            // the file if something actually changed.
            if store.allocate_airplay_ports() {
                store.persist()?;
            }
            return Ok(store);
        }

        // LEGACY shape → convert to one entry per configured singular source,
        // preserving the old node-name ids so routing links keep resolving.
        let legacy: LegacyConfig =
            serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing sources store {}: {e}", path.display()))?;

        let mut sources = Vec::new();
        if let Some(name) = normalize_name(legacy.airplay_source_name) {
            sources.push(SourceEntry {
                id: LEGACY_AIRPLAY_ID.to_string(),
                label: name,
                config: SourceConfig::Airplay(AirplaySourceConfig {
                    latency_msec: legacy.airplay_latency_msec,
                    auth_setup: legacy.airplay_auth_setup,
                    prevent_takeover: legacy.airplay_prevent_takeover,
                    port: 0, // allocated just below
                }),
            });
        }
        if let Some(rtp) = legacy.rtp_source {
            sources.push(SourceEntry {
                id: LEGACY_RTP_ID.to_string(),
                label: "Bluetooth Bridge".to_string(),
                config: SourceConfig::Rtp(rtp),
            });
        }

        let mut store = Self { path: path.to_path_buf(), config: SourcesConfig { sources } };
        store.allocate_airplay_ports();
        store.persist()?; // rewrite in the new shape (idempotent on next boot)
        Ok(store)
    }

    /// All sources, sorted by label (case-insensitive, id as tiebreak).
    pub fn list(&self) -> Vec<SourceEntry> {
        let mut out = self.config.sources.clone();
        out.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// Look up a source by id.
    pub fn get(&self, id: &str) -> Option<SourceEntry> {
        self.config.sources.iter().find(|e| e.id == id).cloned()
    }

    /// Add a source: slugify `label` → unique id (collision-suffixed), allocate
    /// an AirPlay RTSP port if unset, validate RTP port uniqueness, persist, and
    /// return the created entry.
    pub fn add(&mut self, label: String, mut config: SourceConfig) -> anyhow::Result<SourceEntry> {
        let id = self.unique_id(&slugify(&label));
        match &mut config {
            SourceConfig::Airplay(a) => {
                if a.port == 0 {
                    a.port = self.first_free_airplay_port(None);
                }
            }
            SourceConfig::Rtp(r) => {
                if let Some(other) = self.rtp_port_owner(r.port, None) {
                    anyhow::bail!("RTP port {} is already used by source '{other}'", r.port);
                }
            }
        }
        let entry = SourceEntry { id, label, config };
        self.config.sources.push(entry.clone());
        self.persist()?;
        Ok(entry)
    }

    /// Update a source's `label` and/or `config`. The `id` and `kind` are
    /// immutable: changing kind (a config of a different kind) is an error.
    pub fn update(&mut self, id: &str, label: Option<String>, config: Option<SourceConfig>) -> anyhow::Result<SourceEntry> {
        let idx = self.config.sources.iter().position(|e| e.id == id).ok_or_else(|| anyhow::anyhow!("no source with id '{id}'"))?;

        if let Some(new_cfg) = &config {
            let cur_kind = self.config.sources[idx].kind();
            if new_cfg.kind() != cur_kind {
                anyhow::bail!("cannot change the kind of source '{id}' (id and kind are immutable)");
            }
            match new_cfg {
                SourceConfig::Rtp(r) => {
                    if let Some(other) = self.rtp_port_owner(r.port, Some(id)) {
                        anyhow::bail!("RTP port {} is already used by source '{other}'", r.port);
                    }
                }
                SourceConfig::Airplay(_) => {}
            }
        }

        if let Some(label) = label {
            self.config.sources[idx].label = label;
        }
        if let Some(mut new_cfg) = config {
            if let SourceConfig::Airplay(a) = &mut new_cfg {
                if a.port == 0 {
                    a.port = self.first_free_airplay_port(Some(id));
                }
            }
            self.config.sources[idx].config = new_cfg;
        }

        let entry = self.config.sources[idx].clone();
        self.persist()?;
        Ok(entry)
    }

    /// Remove a source by id. Returns whether an entry was actually removed.
    pub fn remove(&mut self, id: &str) -> anyhow::Result<bool> {
        let before = self.config.sources.len();
        self.config.sources.retain(|e| e.id != id);
        let removed = self.config.sources.len() != before;
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    // --- internal helpers -------------------------------------------------

    /// A slug guaranteed not to collide with an existing id (suffix `-2`, `-3`,
    /// … as needed).
    fn unique_id(&self, base: &str) -> SourceId {
        if !self.id_taken(base) {
            return base.to_string();
        }
        let mut n = 2;
        loop {
            let candidate = format!("{base}-{n}");
            if !self.id_taken(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    fn id_taken(&self, id: &str) -> bool {
        self.config.sources.iter().any(|e| e.id == id)
    }

    /// Lowest free RTSP port at or above the base, considering all AirPlay
    /// entries (except `exclude_id`) with a non-zero port.
    fn first_free_airplay_port(&self, exclude_id: Option<&str>) -> u16 {
        let mut port = AIRPLAY_PORT_BASE;
        while self.airplay_port_in_use(port, exclude_id) {
            port += 1;
        }
        port
    }

    fn airplay_port_in_use(&self, port: u16, exclude_id: Option<&str>) -> bool {
        self.config
            .sources
            .iter()
            .any(|e| Some(e.id.as_str()) != exclude_id && matches!(&e.config, SourceConfig::Airplay(a) if a.port != 0 && a.port == port))
    }

    /// The id of an RTP source (other than `exclude_id`) already listening on
    /// `port`, if any.
    fn rtp_port_owner(&self, port: u16, exclude_id: Option<&str>) -> Option<String> {
        self.config
            .sources
            .iter()
            .find(|e| Some(e.id.as_str()) != exclude_id && matches!(&e.config, SourceConfig::Rtp(r) if r.port == port))
            .map(|e| e.id.clone())
    }

    /// Assign a port to every AirPlay entry left at `0`. Returns whether any
    /// entry changed (so the caller can decide whether to rewrite the file).
    fn allocate_airplay_ports(&mut self) -> bool {
        let mut changed = false;
        loop {
            let idx = self.config.sources.iter().position(|e| matches!(&e.config, SourceConfig::Airplay(a) if a.port == 0));
            let Some(idx) = idx else { break };
            let port = self.first_free_airplay_port(None);
            if let SourceConfig::Airplay(a) = &mut self.config.sources[idx].config {
                a.port = port;
            }
            changed = true;
        }
        changed
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, json).map_err(|e| anyhow::anyhow!("writing sources store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sources-store-{tag}-{}-{:?}.json", std::process::id(), std::thread::current().id()))
    }

    fn sample_rtp(port: u16) -> RtpSourceConfig {
        RtpSourceConfig { port, latency_msec: 200, source_addr: "0.0.0.0".to_string(), ignore_ssrc: true, rate: 48000 }
    }

    #[test]
    fn starts_empty_when_no_file() {
        let path = temp_path("empty");
        let _ = std::fs::remove_file(&path);
        let store = SourcesStore::load(&path).unwrap();
        assert!(store.list().is_empty());
        // Missing file starts empty and is NOT written until a mutation.
        assert!(!path.exists());
        let _ = std::fs::remove_file(&path);
    }

    // --- migration ---------------------------------------------------------

    #[test]
    fn migrates_legacy_shape_both_kinds_and_rewrites() {
        let path = temp_path("migrate");
        let _ = std::fs::remove_file(&path);
        // Write a legacy single-field file (both kinds present).
        let legacy = r#"{
            "airplay_source_name": "  Kitchen  ",
            "airplay_latency_msec": 150,
            "airplay_auth_setup": true,
            "airplay_prevent_takeover": false,
            "rtp_source": { "port": 46000, "latency_msec": 250, "source_addr": "239.255.42.42", "ignore_ssrc": false, "rate": 44100 }
        }"#;
        std::fs::write(&path, legacy).unwrap();

        let store = SourcesStore::load(&path).unwrap();
        // Migrated entries carry the legacy values; ids are the legacy ones
        // (routing keys preserved) and node names are bare.
        let airplay = store.get(LEGACY_AIRPLAY_ID).unwrap();
        assert_eq!(airplay.label, "Kitchen"); // trimmed
        assert_eq!(airplay.kind(), SourceKind::Airplay);
        assert_eq!(airplay.node_name(), "airplay-in");
        if let SourceConfig::Airplay(a) = &airplay.config {
            assert_eq!(a.latency_msec, 150);
            assert!(a.auth_setup);
            assert!(!a.prevent_takeover);
            assert_eq!(a.port, 5000); // allocated from the base
        } else {
            panic!("expected airplay config");
        }
        let rtp = store.get(LEGACY_RTP_ID).unwrap();
        assert_eq!(rtp.node_name(), "bt-bridge-rtp");
        assert_eq!(
            rtp.config,
            SourceConfig::Rtp(RtpSourceConfig {
                port: 46000,
                latency_msec: 250,
                source_addr: "239.255.42.42".to_string(),
                ignore_ssrc: false,
                rate: 44100
            })
        );

        // The file was rewritten in the NEW shape and reloads idempotently.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"sources\""));
        let reloaded = SourcesStore::load(&path).unwrap();
        assert_eq!(reloaded.list(), store.list());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrates_legacy_airplay_only_no_rtp() {
        let path = temp_path("migrate-ap-only");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, r#"{ "airplay_source_name": "Den" }"#).unwrap();
        let store = SourcesStore::load(&path).unwrap();
        assert_eq!(store.list().len(), 1);
        assert!(store.get(LEGACY_RTP_ID).is_none()); // no rtp migrated
        let airplay = store.get(LEGACY_AIRPLAY_ID).unwrap();
        assert_eq!(airplay.label, "Den");
        if let SourceConfig::Airplay(a) = &airplay.config {
            assert_eq!(a.latency_msec, DEFAULT_AIRPLAY_LATENCY_MSEC);
            assert!(a.prevent_takeover); // default_true preserved
        } else {
            panic!("expected airplay config");
        }
        let _ = std::fs::remove_file(&path);
    }

    // --- collection CRUD ---------------------------------------------------

    #[test]
    fn add_get_list_update_remove() {
        let path = temp_path("crud");
        let _ = std::fs::remove_file(&path);
        let mut store = SourcesStore::load(&path).unwrap();

        let ap = store.add("Kitchen AirPlay".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();
        assert_eq!(ap.id, "kitchen-airplay");
        assert_eq!(ap.node_name(), "airplay-in-kitchen-airplay");
        let rtp = store.add("Garage Bridge".to_string(), SourceConfig::Rtp(sample_rtp(47000))).unwrap();
        assert_eq!(rtp.id, "garage-bridge");
        assert_eq!(rtp.node_name(), "rtp-in-garage-bridge");

        // get
        assert_eq!(store.get("kitchen-airplay").map(|e| e.label), Some("Kitchen AirPlay".to_string()));
        assert!(store.get("nope").is_none());

        // list sorted by label
        let labels: Vec<_> = store.list().into_iter().map(|e| e.label).collect();
        assert_eq!(labels, vec!["Garage Bridge".to_string(), "Kitchen AirPlay".to_string()]);

        // update label + config (same kind)
        let updated = store.update("garage-bridge", Some("Garage RTP".to_string()), Some(SourceConfig::Rtp(sample_rtp(47001)))).unwrap();
        assert_eq!(updated.label, "Garage RTP");
        if let SourceConfig::Rtp(r) = &updated.config {
            assert_eq!(r.port, 47001);
        } else {
            panic!();
        }

        // update rejects a kind change
        let err = store.update("garage-bridge", None, Some(SourceConfig::Airplay(AirplaySourceConfig::default()))).unwrap_err();
        assert!(err.to_string().contains("kind"));

        // update of a missing id errors
        assert!(store.update("ghost", Some("x".to_string()), None).is_err());

        // persists across reload
        let reloaded = SourcesStore::load(&path).unwrap();
        assert_eq!(reloaded.list().len(), 2);

        // remove
        assert!(store.remove("kitchen-airplay").unwrap());
        assert!(!store.remove("kitchen-airplay").unwrap()); // already gone
        assert_eq!(store.list().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn slug_collision_suffixing() {
        let path = temp_path("slug");
        let _ = std::fs::remove_file(&path);
        let mut store = SourcesStore::load(&path).unwrap();
        let a = store.add("Kitchen".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();
        let b = store.add("Kitchen".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();
        let c = store.add("kitchen!!!".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();
        assert_eq!(a.id, "kitchen");
        assert_eq!(b.id, "kitchen-2");
        assert_eq!(c.id, "kitchen-3");
        // A label with no slug-able chars falls back to "source".
        let d = store.add("!!!".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();
        assert_eq!(d.id, "source");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn airplay_port_allocation_walks_up_from_base() {
        let path = temp_path("ports");
        let _ = std::fs::remove_file(&path);
        let mut store = SourcesStore::load(&path).unwrap();
        let a = store.add("A".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();
        let b = store.add("B".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();
        // Explicit port is respected and skipped by the allocator.
        let c = store.add("C".to_string(), SourceConfig::Airplay(AirplaySourceConfig { port: 5001, ..Default::default() })).unwrap();
        let d = store.add("D".to_string(), SourceConfig::Airplay(AirplaySourceConfig::default())).unwrap();

        let port = |e: &SourceEntry| match &e.config {
            SourceConfig::Airplay(a) => a.port,
            _ => panic!(),
        };
        assert_eq!(port(&a), 5000);
        assert_eq!(port(&b), 5001);
        assert_eq!(port(&c), 5001); // explicit (collides, but explicit ports aren't validated)
        assert_eq!(port(&d), 5002); // first free above the two allocated + the explicit 5001
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rtp_port_collision_is_rejected() {
        let path = temp_path("rtp-collision");
        let _ = std::fs::remove_file(&path);
        let mut store = SourcesStore::load(&path).unwrap();
        store.add("Bridge A".to_string(), SourceConfig::Rtp(sample_rtp(46000))).unwrap();
        // add: same port rejected
        let err = store.add("Bridge B".to_string(), SourceConfig::Rtp(sample_rtp(46000))).unwrap_err();
        assert!(err.to_string().contains("46000"));
        // different port ok
        store.add("Bridge B".to_string(), SourceConfig::Rtp(sample_rtp(46001))).unwrap();
        // update onto an occupied port rejected
        let err = store.update("bridge-b", None, Some(SourceConfig::Rtp(sample_rtp(46000)))).unwrap_err();
        assert!(err.to_string().contains("46000"));
        // updating a source to its own current port is fine
        store.update("bridge-b", None, Some(SourceConfig::Rtp(sample_rtp(46001)))).unwrap();
        assert_eq!(store.list().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn source_node_name_legacy_vs_regular() {
        assert_eq!(source_node_name(SourceKind::Airplay, LEGACY_AIRPLAY_ID), "airplay-in");
        assert_eq!(source_node_name(SourceKind::Rtp, LEGACY_RTP_ID), "bt-bridge-rtp");
        assert_eq!(source_node_name(SourceKind::Airplay, "kitchen"), "airplay-in-kitchen");
        assert_eq!(source_node_name(SourceKind::Rtp, "garage"), "rtp-in-garage");
    }
}
