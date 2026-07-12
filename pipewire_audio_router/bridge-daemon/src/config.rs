//! Add-on configuration, matching the `options`/`schema` shape declared in
//! `config.yaml`. Home Assistant's supervisor writes the user's configured
//! values to `/data/options.json` in this exact shape at container start.

use serde::Deserialize;
use std::path::Path;

/// RAOP encryption type. Defaults to `AuthSetup` — confirmed in spike 2
/// (spikes/02-raop-static-sink.md) as the only mode that actually works
/// against real hardware (Pioneer VSX-934, Dusche); `None`/`Rsa` both get
/// `403 Forbidden` on ANNOUNCE against those receivers.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
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
    // NOT a safe universal default — see spikes/02-raop-static-sink.md:
    // real devices on this network use 7000, not the "traditional" 5000.
    // Kept only as a schema fallback; real configs should set this explicitly
    // per device (discovered via mDNS/avahi-browse).
    7000
}

#[derive(Debug, Clone, Deserialize)]
pub struct RaopOutputConfig {
    pub name: String,
    pub ip: String,
    #[serde(default = "default_raop_port")]
    pub port: u16,
    #[serde(default)]
    pub encryption: RaopEncryption,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendspinOutputConfig {
    pub name: String,
}

fn default_sendspin_base_port() -> u16 {
    // Matches MA's own default sendspin server port (8927) — see
    // spikes/03-sendspin-pushstream.md.
    8927
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AddonOptions {
    #[serde(default)]
    pub outputs: Vec<RaopOutputConfig>,
    #[serde(default)]
    pub sendspin_outputs: Vec<SendspinOutputConfig>,
    /// Display/service name for the single AirPlay-receive source
    /// (shairport-sync — spikes/shairport-sync-source.md). Absent/empty
    /// means no AirPlay-receive source is started. Phase 2 scope is a
    /// single source; multiple simultaneous AirPlay-receive sources are a
    /// later-phase extension, not modeled here yet.
    #[serde(default)]
    pub airplay_source_name: Option<String>,
    #[serde(default = "default_sendspin_base_port")]
    pub sendspin_base_port: u16,
}

/// One planned runtime process derived from the add-on's options — what
/// `run.sh` actually needs to start, in a form it doesn't have to
/// re-derive config-parsing logic to get right itself.
#[derive(Debug, Clone)]
pub enum RuntimeComponent {
    AirplaySource {
        display_name: String,
    },
    SendspinAdapter {
        node_name: String,
        display_name: String,
        port: u16,
    },
}

pub const SENDSPIN_NODE_PREFIX: &str = "sendspin-out-";

impl AddonOptions {
    /// Every process `run.sh` needs to spawn beyond PipeWire/WirePlumber/
    /// the bridge daemon itself, in a stable order (airplay source first,
    /// then one entry per configured sendspin output).
    pub fn runtime_components(&self) -> Vec<RuntimeComponent> {
        let mut components = Vec::new();
        if let Some(name) = &self.airplay_source_name {
            if !name.trim().is_empty() {
                components.push(RuntimeComponent::AirplaySource {
                    display_name: name.clone(),
                });
            }
        }
        for (i, output) in self.sendspin_outputs.iter().enumerate() {
            components.push(RuntimeComponent::SendspinAdapter {
                node_name: format!("{SENDSPIN_NODE_PREFIX}{}", slugify(&output.name)),
                display_name: output.name.clone(),
                port: self.sendspin_base_port + i as u16,
            });
        }
        components
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading options file {}: {e}", path.display()))?;
        let options: AddonOptions = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing options file {}: {e}", path.display()))?;
        Ok(options)
    }
}

/// Turns an output's PipeWire node name into something safe to use as a
/// PipeWire object name and (later) an HA entity-id fragment: lowercase,
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
    fn parses_full_options_document() {
        let json = r#"{
            "outputs": [
                { "name": "Pioneer VSX-934", "ip": "192.168.178.35", "port": 7000, "encryption": "auth_setup" },
                { "name": "Dusche", "ip": "192.168.178.165" }
            ]
        }"#;
        let opts: AddonOptions = serde_json::from_str(json).unwrap();
        assert_eq!(opts.outputs.len(), 2);
        assert_eq!(opts.outputs[0].port, 7000);
        assert_eq!(opts.outputs[0].encryption, RaopEncryption::AuthSetup);
        // second output omits port/encryption - defaults must apply
        assert_eq!(opts.outputs[1].port, 7000);
        assert_eq!(opts.outputs[1].encryption, RaopEncryption::AuthSetup);
    }

    #[test]
    fn slugify_collapses_punctuation_and_spaces() {
        assert_eq!(slugify("Pioneer VSX-934"), "pioneer_vsx_934");
        assert_eq!(slugify("Dusche"), "dusche");
    }

    #[test]
    fn runtime_components_orders_airplay_source_first_then_sendspin_outputs() {
        let json = r#"{
            "airplay_source_name": "PipeWire Router",
            "sendspin_outputs": [ { "name": "Kitchen" }, { "name": "Bedroom" } ]
        }"#;
        let opts: AddonOptions = serde_json::from_str(json).unwrap();
        let components = opts.runtime_components();
        assert_eq!(components.len(), 3);
        match &components[0] {
            RuntimeComponent::AirplaySource { display_name } => assert_eq!(display_name, "PipeWire Router"),
            _ => panic!("expected AirplaySource first"),
        }
        match &components[1] {
            RuntimeComponent::SendspinAdapter { node_name, port, .. } => {
                assert_eq!(node_name, "sendspin-out-kitchen");
                assert_eq!(*port, 8927);
            }
            _ => panic!("expected SendspinAdapter"),
        }
        match &components[2] {
            RuntimeComponent::SendspinAdapter { node_name, port, .. } => {
                assert_eq!(node_name, "sendspin-out-bedroom");
                assert_eq!(*port, 8928);
            }
            _ => panic!("expected SendspinAdapter"),
        }
    }

    #[test]
    fn empty_airplay_source_name_means_disabled() {
        let json = r#"{ "airplay_source_name": "" }"#;
        let opts: AddonOptions = serde_json::from_str(json).unwrap();
        assert!(opts.runtime_components().is_empty());
    }
}
