//! RTP source module concerns: the fixed node name and the SPA-JSON `args`
//! string for the one `libpipewire-module-rtp-source` instance that receives
//! the Bluetooth bridge firmware's audio stream (firmware/bt-bridge/).
//!
//! Loaded into the bridge daemon's *own* PipeWire context at runtime (see
//! pw_module.rs / pw_thread.rs), exactly like a RAOP sink — enable/disable and
//! re-point the listen port live via `/api/source/rtp`, no restart. Unlike the
//! AirPlay-receive source (a `shairport-sync` subprocess), this is a native
//! PipeWire module, so it goes through `PwCommand::Load`/`Unload`, not the
//! process supervisor.
//!
//! Everything except the listen port is fixed to match exactly what the
//! firmware sends: **native-endian `S16LE`** (NOT RFC 3551's big-endian `L16`),
//! 44100 Hz stereo, and `sess.ignore-ssrc = true` because the firmware picks a
//! new random SSRC every boot. See firmware/bt-bridge/README.md and
//! docs/decisions.md "Bluetooth bridge box" for why those settings matter.

use std::fmt::Write as _;

/// The stable PipeWire node name the received audio appears under. Fixed (not
/// derived from user input) — there is only ever one RTP source, and routing.rs
/// classifies any node with a non-monitor output port as a source, so the node
/// shows up in the routing matrix automatically once loaded, with no
/// special-casing of this name anywhere.
pub const RTP_SOURCE_NODE_NAME: &str = "bt-bridge-rtp";

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

/// The SPA-JSON `args` object for the rtp-source module, ready to pass as the
/// `args` string to `pw_context_load_module` (the braces-wrapped form the
/// module's own `pw_properties_new_string(args)` parses). `stream.props` is a
/// nested object so the received audio lands on a node with the right
/// `media.class`/`node.name`. Only the listen port and jitter-buffer latency
/// vary; see the module docs for why the format/rate/channels are fixed.
pub fn rtp_source_module_args(port: u16, latency_msec: u32, source_addr: &str, ignore_ssrc: bool) -> String {
    let mut a = String::new();
    a.push_str("{ ");
    write!(a, "source.ip = \"{source_addr}\" ").unwrap();
    write!(a, "source.port = {port} ").unwrap();
    a.push_str("sess.media = \"audio\" ");
    write!(a, "sess.ignore-ssrc = {ignore_ssrc} ").unwrap();
    write!(a, "sess.latency.msec = {latency_msec} ").unwrap();
    a.push_str("audio.format = \"S16LE\" ");
    a.push_str("audio.rate = 44100 ");
    a.push_str("audio.channels = 2 ");
    write!(a, "stream.props = {{ media.class = \"Audio/Source\" node.name = \"{RTP_SOURCE_NODE_NAME}\" }} ").unwrap();
    a.push('}');
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_args_carry_the_firmware_wire_format() {
        let args = rtp_source_module_args(46000, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC);
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
        assert!(args.contains("audio.rate = 44100"));
        assert!(args.contains("audio.channels = 2"));
        // Nested stream.props with the routable source node.
        assert!(args.contains("stream.props = { media.class = \"Audio/Source\""));
        assert!(args.contains("node.name = \"bt-bridge-rtp\""));
    }

    #[test]
    fn module_args_honor_a_custom_port() {
        let args = rtp_source_module_args(47100, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC);
        assert!(args.contains("source.port = 47100"));
    }

    #[test]
    fn module_args_honor_a_custom_latency() {
        let args = rtp_source_module_args(46000, 350, DEFAULT_RTP_SOURCE_ADDR, DEFAULT_RTP_IGNORE_SSRC);
        assert!(args.contains("sess.latency.msec = 350"));
    }

    #[test]
    fn module_args_honor_a_multicast_source_address() {
        let args = rtp_source_module_args(46000, DEFAULT_RTP_LATENCY_MSEC, "239.255.42.42", DEFAULT_RTP_IGNORE_SSRC);
        assert!(args.contains("source.ip = \"239.255.42.42\""));
    }

    #[test]
    fn module_args_honor_ignore_ssrc_false() {
        // "Only one client": the receiver latches onto the first SSRC and drops
        // foreign senders. Requires a firmware with a stable (MAC-derived) SSRC.
        let args = rtp_source_module_args(46000, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_SOURCE_ADDR, false);
        assert!(args.contains("sess.ignore-ssrc = false"));
    }
}
