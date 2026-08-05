//! Per-user agent config: where the daemon is, and the pairing token.
//!
//! Lives in the user's own `~/.config/pwrouter-agent/config.json`, mode `0600`.
//! That location *is* the multi-user decision from plan §13.2: one agent per
//! logged-in session, each with its own token, so two users on one host pair as
//! two independent targets and neither can drive the other's audio.

use anyhow::{anyhow, Context as _};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// `host:port` of the add-on's daemon. Filled by discovery on first pairing,
    /// overridable with `--daemon` for routed/non-mDNS setups.
    pub daemon: Option<String>,
    /// Bearer token minted when the pairing was approved.
    pub token: Option<String>,
    /// The sink the received audio must play into, by PipeWire `node.name`
    /// (`alsa_output.pci-0000_00_1f.3.analog-stereo`, …). Chosen from the tray menu.
    ///
    /// `None` — the default — means "follow the system default sink", which is what
    /// the drop-in this agent replaced always did. A name here is a **pin**: it is
    /// never silently changed, and while that sink is absent nothing is played. Its
    /// whole point is that the machine in the workshop keeps sending to the workshop
    /// speakers instead of following whatever the desktop's default became.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_sink: Option<String>,
}

/// `~/.config`, honouring `XDG_CONFIG_HOME`. Also where the systemd user unit goes
/// (`autostart::unit_path`), so the two cannot disagree about where "the user's
/// config" is.
pub fn config_home() -> anyhow::Result<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
            Ok(PathBuf::from(home).join(".config"))
        }
    }
}

/// `~/.config/pwrouter-agent/config.json`, honouring `XDG_CONFIG_HOME`.
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_home()?.join("pwrouter-agent").join("config.json"))
}

pub fn load() -> anyhow::Result<Config> {
    let path = config_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes the config `0600`. The token is a credential, so the mode is set
/// *before* the bytes land — a `write_all` followed by `set_permissions` would
/// leave a readable window.
pub fn save(config: &Config) -> anyhow::Result<()> {
    let path = config_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let json = serde_json::to_vec_pretty(config)?;

    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("installing {}", path.display()))?;
    Ok(())
}

/// `/etc/machine-id` — the stable half of the pairing identity. Preferred over the
/// hostname because hostnames change on a whim (and mDNS renames them on
/// collision, which is how this host became `david-local-2`), while the machine id
/// survives renames and re-pairings.
pub fn machine_id() -> String {
    std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-machine".to_string())
}

/// Identity as the daemon derives it: machine id plus user (plan §13.2). Kept here
/// for logging, so the operator can match a log line to an `/api/agents` row.
pub fn identity() -> String {
    format!("{}:{}", machine_id(), user())
}

/// Human label for the pairing UI.
pub fn label() -> String {
    format!("{} ({})", hostname(), user())
}

pub fn user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_follows_xdg_then_home() {
        // Serialised via one process-wide env mutation per assertion; these two
        // vars are only read here.
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg");
        assert_eq!(
            config_path().unwrap(),
            PathBuf::from("/tmp/xdg/pwrouter-agent/config.json")
        );
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", "/home/someone");
        assert_eq!(
            config_path().unwrap(),
            PathBuf::from("/home/someone/.config/pwrouter-agent/config.json")
        );
    }

    #[test]
    fn identity_is_machine_plus_user() {
        let id = identity();
        assert!(
            id.contains(':'),
            "identity should be machine:user, got {id}"
        );
        assert!(id.ends_with(&user()));
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfg = Config {
            daemon: Some("192.168.178.22:8099".into()),
            token: Some("t0ken".into()),
            target_sink: Some("alsa_output.usb-Focusrite".into()),
        };
        let json = serde_json::to_vec(&cfg).unwrap();
        assert_eq!(serde_json::from_slice::<Config>(&json).unwrap(), cfg);
    }

    #[test]
    fn a_config_written_before_the_output_picker_still_loads() {
        // `target_sink` is `#[serde(default)]`, so an agent that is upgraded keeps its
        // pairing and simply follows the default sink as it did before.
        let cfg: Config =
            serde_json::from_str(r#"{"daemon":"host:8099","token":"t0ken"}"#).unwrap();
        assert_eq!(cfg.target_sink, None);
        assert_eq!(cfg.token.as_deref(), Some("t0ken"));
        // And it is left out of the file entirely while unset, rather than written
        // as an explicit null that reads like a decision.
        assert!(!serde_json::to_string(&cfg).unwrap().contains("target_sink"));
    }

    #[test]
    fn missing_config_is_not_an_error() {
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/pwrouter-agent-does-not-exist");
        assert_eq!(load().unwrap(), Config::default());
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}
