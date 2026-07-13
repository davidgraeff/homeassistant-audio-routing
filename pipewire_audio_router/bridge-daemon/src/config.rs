//! Shared config *types* for RAOP outputs and the node-name conventions used
//! across the daemon.
//!
//! There is no add-on `options.json` seeding anymore — every output and source
//! is created at runtime via the REST API and persisted in the daemon's own
//! stores (outputs_store.rs, sources_store.rs). These types are just the
//! shapes those stores and the API handlers share.

use serde::{Deserialize, Serialize};

/// RAOP encryption type. Defaults to `AuthSetup` — confirmed in spike 2
/// (spikes/02-raop-static-sink.md) as the only mode that actually works
/// against real hardware (Pioneer VSX-934, Dusche); `None`/`Rsa` both get
/// `403 Forbidden` on ANNOUNCE against those receivers. Auto-discovery derives
/// it from the mDNS `et` field instead of defaulting (discovery.rs).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RaopEncryption {
    None,
    #[serde(rename = "RSA")]
    Rsa,
    AuthSetup,
}

impl Default for RaopEncryption {
    fn default() -> Self {
        RaopEncryption::AuthSetup
    }
}

impl RaopEncryption {
    /// The literal string PipeWire's `raop.encryption.type` module arg expects.
    pub fn as_pipewire_arg(&self) -> &'static str {
        match self {
            RaopEncryption::None => "none",
            RaopEncryption::Rsa => "RSA",
            RaopEncryption::AuthSetup => "auth_setup",
        }
    }
}

fn default_raop_port() -> u16 {
    // Default for an API-added output that omits `port` — NOT a safe universal
    // value (see spikes/02-raop-static-sink.md: real devices on this network
    // use 7000, not the "traditional" 5000). Callers should set it per device;
    // auto-discovery reads the real port from mDNS.
    7000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RaopOutputConfig {
    pub name: String,
    pub ip: String,
    #[serde(default = "default_raop_port")]
    pub port: u16,
    #[serde(default)]
    pub encryption: RaopEncryption,
}

/// Node-name prefix for sendspin output sink nodes (shared with
/// sources_store.rs and the output classification in api.rs/routing.rs).
pub const SENDSPIN_NODE_PREFIX: &str = "sendspin-out-";

/// Node-name prefix for a discovered sendspin *device* — a virtual routing
/// output (no PipeWire node of its own; audio reaches it via whatever group
/// sink it's dialed into). This is what appears as a matrix column and on the
/// Outputs tab, mirroring how RAOP receivers show up. See sendspin_discovery.rs.
pub const SENDSPIN_DEV_PREFIX: &str = "sendspin-dev-";

/// Turns an output's display name into something safe to use as a PipeWire
/// object name and (later) an HA entity-id fragment: lowercase,
/// spaces/punctuation collapsed to underscores.
pub fn slugify(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raop_output_applies_port_and_encryption_defaults() {
        let output: RaopOutputConfig = serde_json::from_str(r#"{ "name": "Dusche", "ip": "192.168.178.165" }"#).unwrap();
        assert_eq!(output.port, 7000);
        assert_eq!(output.encryption, RaopEncryption::AuthSetup);
    }

    #[test]
    fn slugify_collapses_punctuation_and_spaces() {
        assert_eq!(slugify("Pioneer VSX-934"), "pioneer_vsx_934");
        assert_eq!(slugify("Dusche"), "dusche");
    }
}
