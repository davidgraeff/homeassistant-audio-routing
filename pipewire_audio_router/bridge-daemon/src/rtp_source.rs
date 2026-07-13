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

/// The SPA-JSON `args` object for the rtp-source module, ready to pass as the
/// `args` string to `pw_context_load_module` (the braces-wrapped form the
/// module's own `pw_properties_new_string(args)` parses). `stream.props` is a
/// nested object so the received audio lands on a node with the right
/// `media.class`/`node.name`. Only the listen port varies; see the module docs
/// for why the format/rate/channels are fixed.
pub fn rtp_source_module_args(port: u16) -> String {
    let mut a = String::new();
    a.push_str("{ ");
    a.push_str("source.ip = \"0.0.0.0\" ");
    write!(a, "source.port = {port} ").unwrap();
    a.push_str("sess.media = \"audio\" ");
    a.push_str("sess.ignore-ssrc = true ");
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
        let args = rtp_source_module_args(46000);
        // Braces so the module's pw_properties_new_string parses it as an object.
        assert!(args.starts_with("{ ") && args.ends_with('}'));
        assert!(args.contains("source.ip = \"0.0.0.0\""));
        assert!(args.contains("source.port = 46000"));
        // The three settings that must match the firmware exactly.
        assert!(args.contains("sess.ignore-ssrc = true"));
        assert!(args.contains("audio.format = \"S16LE\""));
        assert!(args.contains("audio.rate = 44100"));
        assert!(args.contains("audio.channels = 2"));
        // Nested stream.props with the routable source node.
        assert!(args.contains("stream.props = { media.class = \"Audio/Source\""));
        assert!(args.contains("node.name = \"bt-bridge-rtp\""));
    }

    #[test]
    fn module_args_honor_a_custom_port() {
        let args = rtp_source_module_args(47100);
        assert!(args.contains("source.port = 47100"));
    }
}
