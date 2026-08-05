//! Start at login, or not: install/remove the systemd **user** unit from inside
//! the binary itself.
//!
//! The unit file is [`include_str!`]-ed, so a machine needs nothing but the binary
//! to become a permanent receiver — no `curl` of a raw.githubusercontent URL, no
//! copy-pasted heredoc, and no way for the file on disk to be a different vintage
//! than the program it starts.
//!
//! Two things are deliberate:
//!
//! * **`ExecStart` points at the binary that did the installing** (`current_exe`,
//!   written back as `%h/…` when it lives under the user's home so the unit
//!   survives a moved home directory). Anything else installs a unit that starts a
//!   *different* copy than the one the user just ran, which is how you end up
//!   debugging a version you are not looking at.
//! * **Enabling does not start, disabling does not stop.** The agent doing this is
//!   already running — starting the unit would give the user two agents fighting
//!   over one session's volume. So the switch is about the *next* login, and says
//!   so.
//!
//! systemd is driven over D-Bus rather than by shelling out to `systemctl`: the
//! session bus connection is already there for the tray, and this keeps the agent's
//! "it never runs commands" property intact.

use anyhow::{anyhow, Context as _};
use std::path::{Path, PathBuf};

/// The unit's name, as systemd knows it.
pub const UNIT_NAME: &str = "pwrouter-agent.service";

/// The shipped unit, compiled in. Also still a real file in the crate, because
/// `install -Dm644` in the from-source instructions is a fine way to install it and
/// because the comments in it are the documentation for its hardening.
const UNIT_TEMPLATE: &str = include_str!("../pwrouter-agent.service");

const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";

/// Whether this session starts the agent by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The unit is installed and enabled.
    Enabled,
    /// Not enabled — usually because the unit is not installed at all.
    Disabled,
    /// No systemd user manager to ask (a container, a non-systemd distro). The
    /// switch is meaningless here and callers should not offer it.
    Unsupported,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Enabled => "starts at login",
            State::Disabled => "does not start at login",
            State::Unsupported => "no systemd user session",
        }
    }
}

/// `$XDG_CONFIG_HOME/systemd/user/pwrouter-agent.service`.
///
/// The manager's own idea of that directory is what counts, so an agent running
/// with an overridden `XDG_CONFIG_HOME` writes somewhere systemd will not look —
/// which is exactly what you want for a test, and never the case for a real user
/// session.
pub fn unit_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::config_home()?
        .join("systemd")
        .join("user")
        .join(UNIT_NAME))
}

/// The unit text as it should land on disk: the shipped template with `ExecStart`
/// pointed at the running binary.
pub fn unit_text() -> String {
    match std::env::current_exe() {
        Ok(exe) => render_unit(
            UNIT_TEMPLATE,
            &exe,
            std::env::var_os("HOME").map(PathBuf::from),
        ),
        Err(e) => {
            // Keep the template's `%h/.local/bin` default: it is the path the
            // install instructions use, so it is the best guess available.
            tracing::warn!(
                "cannot determine this binary's path ({e}); leaving ExecStart as shipped"
            );
            UNIT_TEMPLATE.to_string()
        }
    }
}

/// Split out from [`unit_text`] to be testable without touching the process's own
/// environment.
fn render_unit(template: &str, exe: &Path, home: Option<PathBuf>) -> String {
    // `%h/…` for a binary under the user's home: the unit then keeps working if the
    // home directory itself moves, and an install into the documented
    // `~/.local/bin` reproduces the shipped line byte for byte.
    let exec = match home.as_deref().and_then(|home| exe.strip_prefix(home).ok()) {
        Some(rest) => format!("%h/{}", rest.display()),
        None => exe.display().to_string(),
    };
    let mut out = String::with_capacity(template.len() + 64);
    for line in template.lines() {
        if line.starts_with("ExecStart=") {
            out.push_str(&format!("ExecStart={exec} run\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

async fn manager_call<B>(method: &str, body: &B) -> zbus::Result<zbus::Message>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    let conn = zbus::Connection::session().await?;
    conn.call_method(Some(SYSTEMD), SYSTEMD_PATH, Some(MANAGER), method, body)
        .await
}

/// True for the answer systemd gives about a unit file that is not installed —
/// a normal state of the world, not a broken manager.
fn is_missing_unit(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.FileNotFound" | "org.freedesktop.systemd1.NoSuchUnit"
        ),
        _ => false,
    }
}

pub async fn state() -> State {
    match manager_call("GetUnitFileState", &(UNIT_NAME,)).await {
        Ok(reply) => match reply.body().deserialize::<String>() {
            // `enabled-runtime` too, hence the prefix.
            Ok(s) if s.starts_with("enabled") => State::Enabled,
            Ok(_) => State::Disabled,
            Err(_) => State::Unsupported,
        },
        Err(e) if is_missing_unit(&e) => State::Disabled,
        Err(e) => {
            tracing::debug!("no systemd user manager to ask about {UNIT_NAME} ({e})");
            State::Unsupported
        }
    }
}

/// Writes the unit and enables it. Returns where it landed.
///
/// Idempotent: re-running it also *refreshes* an older unit file, which is how a
/// host picks up hardening changes after an upgrade.
pub async fn enable() -> anyhow::Result<PathBuf> {
    let path = unit_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("unit path has no parent"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    // Temp file plus rename, so a half-written unit is never something systemd can
    // load. Same reason as the config file, different directory.
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, unit_text()).with_context(|| {
        format!(
            "writing {} (a sandboxed agent needs the unit's \
             ReadWritePaths=-%h/.config/systemd/user; from a terminal it always works)",
            tmp.display()
        )
    })?;
    std::fs::rename(&tmp, &path).with_context(|| format!("installing {}", path.display()))?;

    // Reload first: `EnableUnitFiles` reads the file itself, but the manager must
    // know about it before the symlink means anything.
    manager_call("Reload", &())
        .await
        .context("asking systemd to reload its unit files")?;
    manager_call("EnableUnitFiles", &(vec![UNIT_NAME], false, true))
        .await
        .with_context(|| format!("enabling {UNIT_NAME}"))?;
    Ok(path)
}

/// Disables the unit and removes the file. Leaves a running agent running.
pub async fn disable() -> anyhow::Result<()> {
    // Disable before deleting: with the file gone, systemd has nothing to resolve
    // the install symlinks from and would leave them dangling.
    match manager_call("DisableUnitFiles", &(vec![UNIT_NAME], false)).await {
        Ok(_) => {}
        Err(e) if is_missing_unit(&e) => {}
        Err(e) => return Err(anyhow!("disabling {UNIT_NAME}: {e}")),
    }
    let path = unit_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "removing {} (a sandboxed agent needs the unit's \
                     ReadWritePaths=-%h/.config/systemd/user; from a terminal it always works)",
                    path.display()
                )
            })
        }
    }
    manager_call("Reload", &())
        .await
        .context("asking systemd to reload its unit files")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_unit_is_the_one_we_install() {
        // The point of embedding it: no second copy to drift. If the file loses its
        // `ExecStart` or its `[Install]` section, installing it would produce
        // something systemd cannot enable.
        assert!(UNIT_TEMPLATE.contains("\nExecStart="));
        assert!(UNIT_TEMPLATE.contains("[Install]"));
        assert!(UNIT_TEMPLATE.contains("WantedBy=default.target"));
        // The autostart switch writes this very file from inside the sandbox.
        assert!(UNIT_TEMPLATE.contains("ReadWritePaths=-%h/.config/systemd/user"));
    }

    #[test]
    fn a_binary_under_home_is_written_back_as_percent_h() {
        let unit = render_unit(
            UNIT_TEMPLATE,
            Path::new("/home/someone/.local/bin/pwrouter-agent"),
            Some(PathBuf::from("/home/someone")),
        );
        assert!(unit.contains("ExecStart=%h/.local/bin/pwrouter-agent run\n"));
        // ... and that is exactly what the shipped file already said, so installing
        // from the documented location changes nothing.
        assert_eq!(unit.trim_end(), UNIT_TEMPLATE.trim_end());
    }

    #[test]
    fn a_binary_outside_home_gets_an_absolute_exec_start() {
        let unit = render_unit(
            UNIT_TEMPLATE,
            Path::new("/opt/pwrouter/pwrouter-agent"),
            Some(PathBuf::from("/home/someone")),
        );
        assert!(unit.contains("ExecStart=/opt/pwrouter/pwrouter-agent run\n"));
        assert!(!unit.contains("%h/.local/bin"));
    }

    #[test]
    fn rendering_replaces_the_exec_start_line_and_nothing_else() {
        let unit = render_unit(
            UNIT_TEMPLATE,
            Path::new("/opt/pwrouter/pwrouter-agent"),
            None,
        );
        assert_eq!(unit.lines().count(), UNIT_TEMPLATE.lines().count());
        assert_eq!(
            unit.lines().filter(|l| l.starts_with("ExecStart=")).count(),
            1
        );
        for keep in ["ProtectSystem=strict", "KillSignal=SIGTERM"] {
            assert!(unit.contains(keep), "lost {keep}");
        }
    }

    #[test]
    fn unit_path_lives_under_the_users_config_home() {
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-autostart");
        assert_eq!(
            unit_path().unwrap(),
            PathBuf::from("/tmp/xdg-autostart/systemd/user/pwrouter-agent.service")
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}
