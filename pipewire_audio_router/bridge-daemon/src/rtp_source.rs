//! RTP source module concerns: the fixed node name and the SPA-JSON `args`
//! string for the one `libpipewire-module-rtp-source` instance that receives
//! the Bluetooth bridge firmware's audio stream (firmware/bt-bridge/).
//!
//! Loaded into the bridge daemon's *own* PipeWire context at runtime (see
//! `pw_control::module` / pw_thread.rs), exactly like a RAOP sink — enable/disable and
//! re-point the listen port live via `/api/sources`, no restart. Unlike the
//! AirPlay-receive source (a `shairport-sync` subprocess), this is a native
//! PipeWire module, so it goes through `PwCommand::Load`/`Unload`, not the
//! process supervisor.
//!
//! Everything except the listen port, jitter buffer and **sample rate** is
//! fixed to match exactly what the firmware sends: **native-endian `S16LE`**
//! (NOT RFC 3551's big-endian `L16`), stereo, and `sess.ignore-ssrc = true`
//! because the firmware picks a new random SSRC every boot. The rate is
//! configurable (default **48000**) so the whole path can stay at 48 kHz —
//! the router graph runs at 48 kHz, so a 48 kHz sender avoids a resample on
//! both the bridge (down to the old fixed 44100) *and* here (back up to 48000).
//! The rate must match what the sender transmits. See
//! firmware/bt-bridge/README.md and docs/decisions.md "Bluetooth bridge box".

use crate::locks::LockRecover;
use crate::pw_thread::{PwCommand, PwCommandSender, SharedState};
use crate::sources_store::{RtpSourceConfig, SourceConfig, SourceEntry, LEGACY_RTP_ID};
use std::fmt::Write as _;
use tokio::sync::oneshot;

/// The PipeWire module that provides an RTP source.
pub const RTP_SOURCE_MODULE_NAME: &str = "libpipewire-module-rtp-source";

/// Default UDP port the source listens on — matches the example in
/// firmware/bt-bridge/README.md. The firmware's target port is itself
/// HA-configurable, so this is only a sane default; the real value is stored
/// per install (sources_store.rs) and settable via the API.
pub const DEFAULT_RTP_PORT: u16 = 46000;

/// Default receiver-side jitter buffer target, in milliseconds. The module's
/// own default is 100 ms, which is too tight for this sender: the ESP32 bridges
/// classic-BT A2DP and WiFi on one 2.4 GHz radio, so RTP egress arrives in
/// bursts (DTIM/coex-aligned) rather than paced. A 200 ms buffer absorbs that
/// jitter and stops the underruns heard as stutter, at the cost of ~100 ms
/// extra latency — imperceptible for this one-way "phone → whole-home audio"
/// path. This is only a sane default: like the port, the real value is stored
/// per install (sources_store.rs) and settable via the API, so a weak-signal
/// install can trade more latency for fewer dropouts. See
/// firmware/bt-bridge/README.md and docs/decisions.md.
pub const DEFAULT_RTP_LATENCY_MSEC: u32 = 200;

/// Default `source.ip`: `0.0.0.0` = plain unicast listener (bind all local
/// interfaces). Set instead to an IPv4 **multicast group** (e.g. `239.255.42.42`)
/// to have several receivers each join the group and share one firmware stream:
/// `module-rtp-source` calls IP_ADD_MEMBERSHIP when `source.ip` is a multicast
/// address. Point the firmware's `PipeWire RTP Host` at the same group. Stored
/// per install (sources_store.rs) and settable via the API.
pub const DEFAULT_RTP_SOURCE_ADDR: &str = "0.0.0.0";

/// Default `sess.ignore-ssrc`. `true` (module analogue of "accept all senders")
/// keeps every packet reaching the port regardless of its SSRC — required for
/// firmware that picks a **random SSRC per boot** (see docs/decisions.md
/// "Fix 3"). With firmware that sends a **stable** MAC-derived SSRC, set this
/// `false` to have the receiver latch onto the first SSRC and reject every
/// other sender — the "Only one client" mode that stops a stray/second sender
/// from interleaving into (corrupting) the stream. Defaults to `true` so
/// installs with not-yet-reflashed bridges don't go silent on reboot. Stored
/// per install (sources_store.rs), settable via API.
pub const DEFAULT_RTP_IGNORE_SSRC: bool = true;

/// Default sample rate. **48000** so the path stays at 48 kHz end-to-end (the
/// router graph's rate), avoiding a needless resample. Historically this was
/// fixed at 44100 to match the first firmware; senders that still transmit
/// 44100 (e.g. an ESP32 whose A2DP SBC decoder settled on 44.1 kHz) set this
/// back to 44100 via the API so the receiver rate matches the wire. Stored per
/// install (sources_store.rs), settable via the API.
pub const DEFAULT_RTP_RATE: u32 = 48000;

/// The SPA-JSON `args` object for the rtp-source module, ready to pass as the
/// `args` string to `pw_context_load_module` (the braces-wrapped form the
/// module's own `pw_properties_new_string(args)` parses). `stream.props` is a
/// nested object so the received audio lands on a node with the right
/// `media.class`/`node.name`. Only the listen port and jitter-buffer latency
/// vary; see the module docs for why the format/rate/channels are fixed.
pub fn rtp_source_module_args(node_name: &str, port: u16, latency_msec: u32, source_addr: &str, ignore_ssrc: bool, rate: u32) -> String {
    let mut a = String::new();
    a.push_str("{ ");
    write!(a, "source.ip = \"{source_addr}\" ").unwrap();
    write!(a, "source.port = {port} ").unwrap();
    a.push_str("sess.media = \"audio\" ");
    write!(a, "sess.ignore-ssrc = {ignore_ssrc} ").unwrap();
    write!(a, "sess.latency.msec = {latency_msec} ").unwrap();
    a.push_str("audio.format = \"S16LE\" ");
    write!(a, "audio.rate = {rate} ").unwrap();
    a.push_str("audio.channels = 2 ");
    write!(a, "stream.props = {{ media.class = \"Audio/Source\" node.name = \"{node_name}\" }} ").unwrap();
    a.push('}');
    a
}

/// Whether a PipeWire node name belongs to an RTP-receive source this daemon
/// manages: the bare legacy name ([`LEGACY_RTP_ID`]) or the `rtp-in-<id>` scheme
/// [`crate::sources_store::source_node_name`] mints for every non-legacy RTP
/// source. Used to spot orphaned modules (a source that was removed) in the
/// registry so [`reconcile`] can unload them.
pub fn is_rtp_source_node(node_name: &str) -> bool {
    node_name == LEGACY_RTP_ID || node_name.starts_with("rtp-in-")
}

/// Unload the module registered under `node_name`. Idempotent — `Unload` is a
/// no-op if nothing is loaded there — so this is safe to call unconditionally.
/// A failed send just means the PipeWire thread is gone (daemon shutting down).
pub async fn unload(pw_cmd: &PwCommandSender, node_name: &str) {
    let (tx, rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::Unload { node_name: node_name.to_string(), reply: tx }).is_ok() {
        let _ = rx.await;
    }
}

/// (Re)load the rtp-source module for `node_name` from `cfg`. Unloads any
/// existing instance first so a re-enable or a config change is a clean reload
/// — `Load` errors if a module is already registered under the node name, and
/// `Unload` is idempotent, so this is safe whether or not one is loaded.
pub async fn reload(pw_cmd: &PwCommandSender, node_name: &str, cfg: &RtpSourceConfig) -> Result<(), String> {
    unload(pw_cmd, node_name).await;

    let args = rtp_source_module_args(node_name, cfg.port, cfg.latency_msec, &cfg.source_addr, cfg.ignore_ssrc, cfg.rate);
    let (tx, rx) = oneshot::channel();
    if pw_cmd
        .send(PwCommand::Load { node_name: node_name.to_string(), module_name: RTP_SOURCE_MODULE_NAME.to_string(), args, reply: tx })
        .is_err()
    {
        return Err("PipeWire thread is not running".to_string());
    }
    match rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("no reply from PipeWire thread".to_string()),
    }
}

/// Reconcile the loaded rtp-source modules against the stored sources: ensure
/// one module is loaded (under `source_node_name(Rtp, id)`) for every RTP
/// [`SourceEntry`] with that entry's config, and unload any RTP module whose
/// source was removed.
///
/// `pw_thread` keys `Load`/`Unload` by node name, so the registry snapshot
/// (`pw`) is our view of what is currently loaded: any node matching the RTP
/// naming scheme ([`is_rtp_source_node`]) that no longer has a desired source is
/// an orphan and gets unloaded. Each desired source is then (re)loaded from its
/// current config, which is a clean unload→load — so calling `reconcile`
/// repeatedly converges to the stored set (idempotent). Non-RTP entries are
/// ignored (handled by their own lifecycle).
pub async fn reconcile(sources: &[SourceEntry], pw_cmd: &PwCommandSender, pw: &SharedState) {
    // Desired: (node name, config) for every RTP entry.
    let desired: Vec<(String, RtpSourceConfig)> = sources
        .iter()
        .filter_map(|e| match &e.config {
            SourceConfig::Rtp(cfg) => Some((e.node_name(), cfg.clone())),
            SourceConfig::Airplay(_) => None,
        })
        .collect();

    // Unload orphans: RTP nodes present in the registry with no desired source.
    let orphans: Vec<String> = {
        let reg = pw.lock_recover();
        let mut names: Vec<String> = reg
            .nodes
            .values()
            .map(|n| n.node_name.clone())
            .filter(|n| is_rtp_source_node(n) && !desired.iter().any(|(d, _)| d == n))
            .collect();
        names.sort();
        names.dedup();
        names
    };
    for name in orphans {
        tracing::info!("rtp reconcile: unloading removed source '{name}'");
        unload(pw_cmd, &name).await;
    }

    // (Re)load every desired source from its current config.
    for (node_name, cfg) in &desired {
        match reload(pw_cmd, node_name, cfg).await {
            Ok(()) => tracing::info!(
                "rtp reconcile: loaded '{node_name}' on {}:{} ({} ms jitter buffer)",
                cfg.source_addr,
                cfg.port,
                cfg.latency_msec
            ),
            Err(e) => tracing::warn!("rtp reconcile: failed to load '{node_name}': {e}"),
        }
    }
}

/// Node name of the old single RTP source, kept as a fixture for the args tests.
#[cfg(test)]
const RTP_SOURCE_NODE_NAME: &str = "bt-bridge-rtp";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_args_carry_the_firmware_wire_format() {
        let args = rtp_source_module_args(RTP_SOURCE_NODE_NAME, 46000, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_RATE);
        // Braces so the module's pw_properties_new_string parses it as an object.
        assert!(args.starts_with("{ ") && args.ends_with('}'));
        assert!(args.contains("source.ip = \"0.0.0.0\""));
        assert!(args.contains("source.port = 46000"));
        // The three settings that must match the firmware exactly.
        assert!(args.contains("sess.ignore-ssrc = true"));
        // Jitter buffer widened past the module default to absorb the
        // ESP32's bursty BT+WiFi coexistence egress (stutter fix).
        assert!(args.contains("sess.latency.msec = 200"));
        assert!(args.contains("audio.format = \"S16LE\""));
        // Default is now 48 kHz (stay 48 kHz end-to-end, no resample).
        assert!(args.contains("audio.rate = 48000"));
        assert!(args.contains("audio.channels = 2"));
        // Nested stream.props with the routable source node.
        assert!(args.contains("stream.props = { media.class = \"Audio/Source\""));
        assert!(args.contains("node.name = \"bt-bridge-rtp\""));
    }

    #[test]
    fn module_args_honor_a_custom_port() {
        let args = rtp_source_module_args(RTP_SOURCE_NODE_NAME, 47100, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_RATE);
        assert!(args.contains("source.port = 47100"));
    }

    #[test]
    fn module_args_honor_a_custom_latency() {
        let args = rtp_source_module_args(RTP_SOURCE_NODE_NAME, 46000, 350, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_RATE);
        assert!(args.contains("sess.latency.msec = 350"));
    }

    #[test]
    fn module_args_honor_a_multicast_source_address() {
        let args = rtp_source_module_args(RTP_SOURCE_NODE_NAME, 46000, DEFAULT_RTP_LATENCY_MSEC, "239.255.42.42", DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_RATE);
        assert!(args.contains("source.ip = \"239.255.42.42\""));
    }

    #[test]
    fn module_args_honor_ignore_ssrc_false() {
        // "Only one client": the receiver latches onto the first SSRC and drops
        // foreign senders. Requires a firmware with a stable (MAC-derived) SSRC.
        let args = rtp_source_module_args(RTP_SOURCE_NODE_NAME, 46000, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, false, DEFAULT_RTP_RATE);
        assert!(args.contains("sess.ignore-ssrc = false"));
    }

    #[test]
    fn module_args_honor_a_custom_rate() {
        // A sender still transmitting 44.1 kHz (e.g. an ESP32 whose SBC decoder
        // settled there) — the receiver rate must match the wire.
        let args = rtp_source_module_args(RTP_SOURCE_NODE_NAME, 46000, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC, 44100);
        assert!(args.contains("audio.rate = 44100"));
    }

    #[test]
    fn module_args_honor_a_custom_node_name() {
        // A non-legacy RTP source presents under its `rtp-in-<id>` node name.
        let args = rtp_source_module_args("rtp-in-garage", 46000, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_RATE);
        assert!(args.contains("node.name = \"rtp-in-garage\""));
    }

    #[test]
    fn rtp_source_node_name_recognition() {
        // The legacy bare name and every `rtp-in-<id>` name are ours; other
        // sources (airplay, sendspin, raop targets) are not.
        assert!(is_rtp_source_node(RTP_SOURCE_NODE_NAME)); // "bt-bridge-rtp"
        assert!(is_rtp_source_node("rtp-in-garage"));
        assert!(is_rtp_source_node("rtp-in-anything-else"));
        assert!(!is_rtp_source_node("airplay-in"));
        assert!(!is_rtp_source_node("airplay-in-kitchen"));
        assert!(!is_rtp_source_node("sendspin-dev-aabbcc"));
    }
}
