//! Persistent latency/sync tuning knobs for the grouping reconciler
//! (sync_group.rs) — the user-facing dials for "make the group as snappy as
//! possible, but keep everyone playing together".
//!
//! Two things live here, both keyed by stable name so they survive restarts,
//! node reloads and device churn (same rationale as routing_store.rs):
//!
//! - **`group_lead_ms`** — the sendspin group's presentation lead
//!   ([`sendspin::server::Group::with_send_ahead_us`]). It's how far ahead of
//!   "now" audio is scheduled; every member must be able to buffer within it.
//!   Raise it so a slower member (e.g. a RAOP receiver sharing the group's
//!   anchor, or a distant speaker) can still play the same instant; lower it
//!   for snappier start. One value for the whole daemon — the protocol itself
//!   only supports one lead per group.
//! - **`sendspin_delays`** — a per-device *static* delay in ms
//!   (`PlayerCommandType::SetStaticDelay`), for trimming an individual speaker
//!   that's consistently early/late relative to the rest of its group. Applied
//!   in-band by the sendspin server on (re)connect (sendspin_volume.rs).
//! - **`ap2_latency`** — the AirPlay-2 per-output render delay in ms (the PT=87
//!   anchor shift; ap2_server.rs), keyed by AP2 node name. It's retuned LIVE on
//!   the running stream (ap2_control → SetRenderDelay), and used as the initial
//!   delay on the next (membership/rate) reconnect. `None` = the sender's
//!   built-in default (1500 ms).
//!
//! Mirrors the other `/data` stores: no `options.json` seeding, the file is
//! authoritative and created on first mutation; a missing file means defaults.

use crate::locks::LockRecover;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Default sendspin group lead — the sendspin protocol's own default
/// ([`sendspin::server::group::DEFAULT_SEND_AHEAD_US`] = 250 000 µs).
pub const DEFAULT_GROUP_LEAD_MS: u32 = 250;

fn default_group_lead_ms() -> u32 {
    DEFAULT_GROUP_LEAD_MS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncConfig {
    #[serde(default = "default_group_lead_ms")]
    group_lead_ms: u32,
    /// Per-sendspin-device static delay (ms), keyed by virtual device node name.
    #[serde(default)]
    sendspin_delays: BTreeMap<String, u16>,
    /// Per-AP2-output render delay (ms), keyed by AP2 node name.
    #[serde(default)]
    ap2_latency: BTreeMap<String, u16>,
    /// Per-AP2-output wire sample-rate MODE (user choice), keyed by AP2 node name.
    /// Absent ⇒ `Auto`. `Auto` negotiates 48 kHz and falls back to 44.1 kHz;
    /// `Fixed44100` forces the AirPlay-standard 44.1 kHz (for receivers that
    /// misbehave at 48 kHz).
    #[serde(default)]
    ap2_rate_mode: BTreeMap<String, Ap2RateMode>,
    /// Per-sendspin-device wire codec choice, keyed by sendspin node name. Absent
    /// ⇒ [`SendspinCodec::Auto`].
    #[serde(default)]
    sendspin_codec: BTreeMap<String, SendspinCodec>,
    /// Per-AP2-device LEARNED capability cache (Hz), keyed by AP2 node name: the
    /// last successfully-negotiated rate, or 44100 if a 48 kHz SETUP was rejected.
    /// Absent ⇒ untested (Auto optimistically tries 48 kHz). Persisted so we don't
    /// re-probe a known-44.1k-only receiver on every connect.
    #[serde(default)]
    ap2_rate_cap: BTreeMap<String, u32>,
}

/// Per-AP2-output sample-rate mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ap2RateMode {
    /// Negotiate 48 kHz, fall back to 44.1 kHz on rejection (default).
    Auto,
    /// Force 44.1 kHz (AirPlay standard).
    Fixed44100,
}

impl Default for Ap2RateMode {
    fn default() -> Self {
        Self::Auto
    }
}

/// Per-sendspin-output wire codec choice.
///
/// Sendspin negotiates: a device advertises the `{codec, rate, depth, channels}`
/// combinations it decodes, and one stream serves a whole group — so the *effective*
/// codec is this choice narrowed by what the daemon can encode and what every member
/// of the group supports (`sendspin_server::resolve_codec`). An unusable choice falls
/// back to PCM rather than sending a stream nothing can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendspinCodec {
    /// Best available: Opus when the daemon can encode it and every member decodes
    /// it (≈10× less WiFi airtime), else PCM. The default.
    Auto,
    /// Force uncompressed PCM — zero encode/decode cost, ~1.5 Mbit/s per stream.
    Pcm,
    /// Force Opus (lossy, ~10-15× smaller).
    Opus,
    /// Force FLAC (lossless, ~40-50% smaller). Never chosen by `Auto`: it's a
    /// deliberate "lossless, and I'll pay the bandwidth" decision.
    Flac,
}

impl Default for SendspinCodec {
    fn default() -> Self {
        Self::Auto
    }
}

impl SendspinCodec {
    /// The codec name this mode pins, or `None` for [`Self::Auto`].
    pub fn explicit_codec(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Pcm => Some("pcm"),
            Self::Opus => Some("opus"),
            Self::Flac => Some("flac"),
        }
    }

    /// The wire/API name of this mode (`"auto"`, `"pcm"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pcm => "pcm",
            Self::Opus => "opus",
            Self::Flac => "flac",
        }
    }

    /// Parse an API/wire name; `None` if it isn't a known mode.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "pcm" => Some(Self::Pcm),
            "opus" => Some(Self::Opus),
            "flac" => Some(Self::Flac),
            _ => None,
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            group_lead_ms: DEFAULT_GROUP_LEAD_MS,
            sendspin_delays: BTreeMap::new(),
            ap2_latency: BTreeMap::new(),
            ap2_rate_mode: BTreeMap::new(),
            sendspin_codec: BTreeMap::new(),
            ap2_rate_cap: BTreeMap::new(),
        }
    }
}

pub struct SyncSettings {
    path: PathBuf,
    config: SyncConfig,
}

impl SyncSettings {
    /// Load from `path`, or start with defaults if it doesn't exist yet (created
    /// on the first mutation).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading sync settings {}: {e}", path.display()))?;
            let config: SyncConfig =
                serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing sync settings {}: {e}", path.display()))?;
            Ok(Self { path: path.to_path_buf(), config })
        } else {
            Ok(Self { path: path.to_path_buf(), config: SyncConfig::default() })
        }
    }

    pub fn group_lead_ms(&self) -> u32 {
        self.config.group_lead_ms
    }

    /// The group lead in microseconds, ready for [`sendspin::server::Group::with_send_ahead_us`].
    pub fn group_lead_us(&self) -> i64 {
        i64::from(self.config.group_lead_ms) * 1000
    }

    pub fn set_group_lead_ms(&mut self, ms: u32) -> anyhow::Result<()> {
        self.config.group_lead_ms = ms;
        self.persist()
    }

    /// Desired per-device static delays (ms) by node name.
    pub fn sendspin_delays(&self) -> BTreeMap<String, u16> {
        self.config.sendspin_delays.clone()
    }

    /// Set (or clear, when `ms` is 0) a device's static delay and persist.
    pub fn set_sendspin_delay(&mut self, node_name: &str, ms: u16) -> anyhow::Result<()> {
        if ms == 0 {
            self.config.sendspin_delays.remove(node_name);
        } else {
            self.config.sendspin_delays.insert(node_name.to_string(), ms);
        }
        self.persist()
    }

    /// The configured render delay (ms) for an AP2 node, if any (`None` = the
    /// sender's built-in default).
    pub fn ap2_latency(&self, node_name: &str) -> Option<u16> {
        self.config.ap2_latency.get(node_name).copied()
    }

    /// Desired per-output AP2 render delays (ms) by node name.
    pub fn ap2_latencies(&self) -> BTreeMap<String, u16> {
        self.config.ap2_latency.clone()
    }

    /// Set (or clear, when `ms` is `None`) an AP2 output's render delay and persist.
    pub fn set_ap2_latency(&mut self, node_name: &str, ms: Option<u16>) -> anyhow::Result<()> {
        match ms {
            None => {
                self.config.ap2_latency.remove(node_name);
            }
            Some(ms) => {
                self.config.ap2_latency.insert(node_name.to_string(), ms);
            }
        }
        self.persist()
    }

    // ---- AP2 sample rate (mode + learned cache) --------------------------

    /// The user-chosen wire codec for a sendspin output (default
    /// [`SendspinCodec::Auto`]).
    pub fn sendspin_codec(&self, node_name: &str) -> SendspinCodec {
        self.config.sendspin_codec.get(node_name).copied().unwrap_or_default()
    }

    /// Every stored sendspin codec choice, by node name (for the API listing).
    pub fn sendspin_codecs(&self) -> BTreeMap<String, SendspinCodec> {
        self.config.sendspin_codec.clone()
    }

    /// Set (and persist) a sendspin output's codec choice. `Auto` clears the entry so
    /// the default keeps following any future change to what `Auto` means.
    pub fn set_sendspin_codec(&mut self, node_name: &str, codec: SendspinCodec) -> anyhow::Result<()> {
        if codec == SendspinCodec::Auto {
            self.config.sendspin_codec.remove(node_name);
        } else {
            self.config.sendspin_codec.insert(node_name.to_string(), codec);
        }
        self.persist()
    }

    /// The user-chosen rate mode for an AP2 output (default [`Ap2RateMode::Auto`]).
    pub fn ap2_rate_mode(&self, node_name: &str) -> Ap2RateMode {
        self.config.ap2_rate_mode.get(node_name).copied().unwrap_or_default()
    }

    /// All explicitly-set AP2 rate modes (for the UI/outputs listing).
    pub fn ap2_rate_modes(&self) -> BTreeMap<String, Ap2RateMode> {
        self.config.ap2_rate_mode.clone()
    }

    /// Set an AP2 output's rate mode and persist. Setting `Auto` also clears any
    /// learned cap so the next connect re-probes 48 kHz.
    pub fn set_ap2_rate_mode(&mut self, node_name: &str, mode: Ap2RateMode) -> anyhow::Result<()> {
        self.config.ap2_rate_mode.insert(node_name.to_string(), mode);
        if mode == Ap2RateMode::Auto {
            self.config.ap2_rate_cap.remove(node_name);
        }
        self.persist()
    }

    /// Record the outcome of a rate negotiation (48000 = 48 kHz worked, 44100 =
    /// 48 kHz was rejected) so we don't re-probe. Persisted. No-op if unchanged.
    pub fn set_ap2_rate_cap(&mut self, node_name: &str, rate: u32) -> anyhow::Result<()> {
        if self.config.ap2_rate_cap.get(node_name) == Some(&rate) {
            return Ok(());
        }
        self.config.ap2_rate_cap.insert(node_name.to_string(), rate);
        self.persist()
    }

    /// The effective wire rate for one AP2 output: `Fixed44100` ⇒ 44100; `Auto` ⇒
    /// the learned cap (44100 if a prior 48 kHz SETUP was rejected), else 48000
    /// (optimistic — untested Auto devices try 48 kHz first).
    pub fn ap2_effective_rate(&self, node_name: &str) -> u32 {
        match self.ap2_rate_mode(node_name) {
            Ap2RateMode::Fixed44100 => 44_100,
            Ap2RateMode::Auto => self.config.ap2_rate_cap.get(node_name).copied().unwrap_or(48_000),
        }
    }

    /// The capture/wire rate for a whole AP2 group: 48000 iff EVERY member's
    /// effective rate is 48000, else 44100 (one capture serves the group, so any
    /// 44.1k member pulls the group to 44.1 kHz).
    pub fn ap2_group_rate<'a>(&self, members: impl IntoIterator<Item = &'a str>) -> u32 {
        if members.into_iter().all(|n| self.ap2_effective_rate(n) == 48_000) {
            48_000
        } else {
            44_100
        }
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, json).map_err(|e| anyhow::anyhow!("writing sync settings {}: {e}", self.path.display()))?;
        Ok(())
    }
}

/// Shared handle used across the API and the reconcile task.
pub type SharedSyncSettings = Arc<Mutex<SyncSettings>>;

/// Convenience: lock and read the current group lead in microseconds.
pub fn group_lead_us(settings: &SharedSyncSettings) -> i64 {
    settings.lock_recover().group_lead_us()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sync-settings-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn defaults_when_absent_and_persists_across_reload() {
        let path = temp_path("basic");
        let _ = std::fs::remove_file(&path);
        let mut s = SyncSettings::load(&path).unwrap();
        assert_eq!(s.group_lead_ms(), DEFAULT_GROUP_LEAD_MS);
        assert_eq!(s.group_lead_us(), 250_000);
        s.set_group_lead_ms(600).unwrap();
        s.set_sendspin_delay("sendspin-dev-kitchen", 40).unwrap();
        s.set_ap2_latency("ap2-dev-dusche", Some(800)).unwrap();

        let reloaded = SyncSettings::load(&path).unwrap();
        assert_eq!(reloaded.group_lead_ms(), 600);
        assert_eq!(reloaded.sendspin_delays().get("sendspin-dev-kitchen").copied(), Some(40));
        assert_eq!(reloaded.ap2_latency("ap2-dev-dusche"), Some(800));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zero_delay_clears_entry() {
        let path = temp_path("clear");
        let _ = std::fs::remove_file(&path);
        let mut s = SyncSettings::load(&path).unwrap();
        s.set_sendspin_delay("sendspin-dev-bath", 30).unwrap();
        assert_eq!(s.sendspin_delays().len(), 1);
        s.set_sendspin_delay("sendspin-dev-bath", 0).unwrap();
        assert!(s.sendspin_delays().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
