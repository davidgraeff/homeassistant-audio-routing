//! RAOP output module concerns: the node-name convention and the SPA-JSON
//! `args` string for one `libpipewire-module-raop-sink` instance.
//!
//! These modules are loaded into the bridge daemon's *own* PipeWire context at
//! runtime (see pw_module.rs / pw_thread.rs), one per configured output — not
//! from a static `pipewire.conf.d` file written before PipeWire starts, which
//! is what this module used to generate. See docs/decisions.md "Loading
//! PipeWire modules at runtime" for why runtime loading is both possible and
//! preferable (hot-reloadable outputs, no restart).

use crate::config::{slugify, RaopOutputConfig};
use std::fmt::Write as _;

/// Node name prefix so RAOP sink nodes are easy to recognize/filter in
/// `pw-dump`/`pw-link` output, in this daemon's own registry queries, and in
/// the shared "what counts as an output" classification (api.rs, routing.rs).
pub const RAOP_NODE_PREFIX: &str = "raop-out-";

/// The PipeWire module that provides a RAOP (AirPlay) sink. Loaded once per
/// configured output.
pub const RAOP_MODULE_NAME: &str = "libpipewire-module-raop-sink";

pub fn raop_node_name(output_name: &str) -> String {
    format!("{RAOP_NODE_PREFIX}{}", slugify(output_name))
}

/// The SPA-JSON `args` object for one raop-sink module instance, ready to pass
/// as the `args` string to `pw_context_load_module`. These are the same keys
/// the old static `pipewire.conf.d` generator emitted; the braces-wrapped form
/// is exactly what the module's own `pw_properties_new_string(args)` call
/// parses. `nofail` (a config-level module flag) has no analogue here — a
/// failed load returns NULL and is reported by the caller, so one bad device
/// still can't take anything else down.
pub fn raop_module_args(output: &RaopOutputConfig) -> String {
    let node_name = raop_node_name(&output.name);
    let mut a = String::new();
    a.push_str("{ ");
    write!(a, "raop.ip = \"{}\" ", output.ip).unwrap();
    write!(a, "raop.port = {} ", output.port).unwrap();
    write!(a, "raop.name = \"{}\" ", output.name).unwrap();
    a.push_str("raop.transport = \"udp\" ");
    write!(a, "raop.encryption.type = \"{}\" ", output.encryption.as_pipewire_arg()).unwrap();
    a.push_str("audio.format = \"S16\" ");
    a.push_str("audio.rate = 44100 ");
    a.push_str("audio.channels = 2 ");
    write!(a, "node.name = \"{node_name}\" ").unwrap();
    a.push('}');
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RaopEncryption, RaopOutputConfig};

    #[test]
    fn node_name_uses_prefix_and_slug() {
        assert_eq!(raop_node_name("Pioneer VSX-934"), "raop-out-pioneer_vsx_934");
    }

    #[test]
    fn module_args_carry_every_field_the_module_needs() {
        let output = RaopOutputConfig {
            name: "Pioneer VSX-934".to_string(),
            ip: "192.168.178.35".to_string(),
            port: 7000,
            encryption: RaopEncryption::AuthSetup,
        };
        let args = raop_module_args(&output);
        // Braces so the module's pw_properties_new_string parses it as an object.
        assert!(args.starts_with("{ ") && args.ends_with('}'));
        assert!(args.contains("raop.ip = \"192.168.178.35\""));
        assert!(args.contains("raop.port = 7000"));
        assert!(args.contains("raop.name = \"Pioneer VSX-934\""));
        assert!(args.contains("raop.encryption.type = \"auth_setup\""));
        assert!(args.contains("audio.format = \"S16\""));
        assert!(args.contains("node.name = \"raop-out-pioneer_vsx_934\""));
    }

    #[test]
    fn module_args_honor_non_default_encryption() {
        let output = RaopOutputConfig {
            name: "Dusche".to_string(),
            ip: "192.168.178.165".to_string(),
            port: 5000,
            encryption: RaopEncryption::None,
        };
        let args = raop_module_args(&output);
        assert!(args.contains("raop.encryption.type = \"none\""));
        assert!(args.contains("raop.port = 5000"));
    }
}
