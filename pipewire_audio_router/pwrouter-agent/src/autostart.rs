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

/// Where the agent lives once it is installed: `~/.local/bin/pwrouter-agent`, the path
/// the shipped unit and the in-app instructions both name.
///
/// A *stable* path is the point. `ExecStart` used to be rewritten to wherever the
/// binary that ran `autostart enable` happened to sit — usually `~/Downloads/
/// pwrouter-agent-x86_64` — which is right exactly once: replace that binary with a
/// newer download and the unit still starts whatever is left at the old path, or
/// nothing at all. With one canonical location, updating is a copy over this file and
/// `systemctl --user restart`, and nothing has to be rewritten.
pub fn install_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("no HOME to install into"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("bin")
        .join("pwrouter-agent"))
}

/// What [`enable`] did, so the CLI can print it and the tray can say so.
#[derive(Debug, Clone)]
pub struct Installed {
    /// The unit file that was written.
    pub unit: PathBuf,
    /// What its `ExecStart` points at.
    pub exec: PathBuf,
    /// Whether this binary was copied to [`install_path`] as part of enabling.
    pub copied: bool,
    /// Why `exec` is *not* the canonical path, when it isn't — a read-only home, a
    /// sandboxed agent. Worth showing: it is the difference between "updating means
    /// copying one file" and "updating means running enable again".
    pub note: Option<String>,
}

/// Copies this binary to [`install_path`] unless it is already running from there.
///
/// Temp file plus rename, like the unit and the config: a rename replaces the
/// *directory entry*, so it works even when the destination is the currently running
/// agent (writing to it in place would fail with `ETXTBSY`) and never leaves a half-
/// copied executable behind.
fn install_self() -> anyhow::Result<(PathBuf, bool)> {
    let target = install_path()?;
    let exe = std::env::current_exe().context("locating this binary")?;
    // `canonicalize` so a symlink or a `./pwrouter-agent` invocation is compared by
    // what it actually is; a failure here just means "not the same file".
    let same = match (exe.canonicalize(), target.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => exe == target,
    };
    if same {
        return Ok((target, false));
    }
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("install path has no parent"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(".pwrouter-agent.new");
    std::fs::copy(&exe, &tmp)
        .with_context(|| format!("copying {} to {}", exe.display(), tmp.display()))?;
    // 0755 explicitly: `copy` carries the source's mode, and a binary fetched by a
    // browser can arrive without the execute bit.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", tmp.display()))?;
    }
    std::fs::rename(&tmp, &target).with_context(|| format!("installing {}", target.display()))?;
    Ok((target, true))
}

/// The unit text as it should land on disk: the shipped template with `ExecStart`
/// pointed at `exec`.
pub fn unit_text_for(exec: &Path) -> String {
    render_unit(
        UNIT_TEMPLATE,
        exec,
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Split out from [`unit_text_for`] to be testable without touching the process's own
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

/// Installs this binary to [`install_path`], writes the unit pointed at it, and enables
/// it. Returns what it did ([`Installed`]).
///
/// Idempotent: re-running it also *refreshes* an older unit file, which is how a
/// host picks up hardening changes after an upgrade — and re-installs the binary, which
/// is how "I downloaded a newer one and ran enable" does the obvious thing.
///
/// The install is best-effort: a read-only home (a sandboxed agent whose unit predates
/// the `ReadWritePaths` relaxation) still gets a working unit, pointed at wherever this
/// binary already is, plus a note saying so. A unit that starts *something* beats
/// refusing to enable.
pub async fn enable() -> anyhow::Result<Installed> {
    let (exec, copied, note) = match install_self() {
        Ok((path, copied)) => (path, copied, None),
        Err(e) => {
            let exe = std::env::current_exe().context("locating this binary")?;
            tracing::warn!(
                "could not install into ~/.local/bin ({e:#}); pointing the unit at {}",
                exe.display()
            );
            let note = format!(
                "could not copy this binary to {} ({e:#}), so the unit starts it where it is — \
                 replacing it later means running `autostart enable` again",
                install_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "~/.local/bin/pwrouter-agent".into())
            );
            (exe, false, Some(note))
        }
    };
    let path = unit_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("unit path has no parent"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    // Temp file plus rename, so a half-written unit is never something systemd can
    // load. Same reason as the config file, different directory.
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, unit_text_for(&exec)).with_context(|| {
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
    Ok(Installed {
        unit: path,
        exec,
        copied,
        note,
    })
}

/// Is *this* process the unit systemd is running — the same PID it calls the
/// service's `MainPID`?
///
/// The question the tray's Quit has to answer, and it cannot be answered by
/// `INVOCATION_ID`: a graphical terminal is itself a unit on modern systemd, so a
/// hand-launched agent inherits an `INVOCATION_ID` of its own and would think it is the
/// service. Comparing PIDs is exact, and gets the two cases that matter right — a
/// manually started agent must not stop *another* instance's unit, and the real service
/// must not simply exit into a `Restart=always` respawn.
///
/// `None` whenever the answer is "not us": no session bus, no systemd, the unit not
/// loaded, or a different PID.
pub async fn is_the_running_service() -> bool {
    let Ok(reply) = manager_call("GetUnit", &(UNIT_NAME,)).await else {
        return false;
    };
    let Ok(path) = reply
        .body()
        .deserialize::<zbus::zvariant::OwnedObjectPath>()
    else {
        return false;
    };
    let Ok(conn) = zbus::Connection::session().await else {
        return false;
    };
    let reply = conn
        .call_method(
            Some(SYSTEMD),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.systemd1.Service", "MainPID"),
        )
        .await;
    let Ok(reply) = reply else { return false };
    match reply.body().deserialize::<zbus::zvariant::Value>() {
        Ok(value) => u32::try_from(&value)
            .map(|pid| pid == std::process::id())
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Asks systemd to stop the unit, and lets it do the stopping.
///
/// This is what "quit" has to mean for the service instance: the unit is
/// `Restart=always`, so exiting on our own would have the manager start us again
/// within seconds — which reads as a menu item that does not work. systemd answers a
/// stop by sending SIGTERM, so the ordinary shutdown path (restore the host's volume,
/// unload the receiver) is the one that runs, exactly as it does for `systemctl stop`.
/// Autostart is untouched, so a login still brings the agent back.
pub async fn stop() -> anyhow::Result<()> {
    manager_call("StopUnit", &(UNIT_NAME, "replace"))
        .await
        .with_context(|| format!("stopping {UNIT_NAME}"))?;
    Ok(())
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
    fn the_install_path_is_the_one_the_shipped_unit_names() {
        // The invariant that keeps `enable` honest: it copies the binary to
        // `install_path()` and points the unit there, so the rendered `ExecStart` has to
        // be the line the template already carries — otherwise "replace the file at that
        // path and restart" would be advice about the wrong path.
        std::env::set_var("HOME", "/home/someone");
        let installed = install_path().unwrap();
        assert_eq!(
            installed,
            PathBuf::from("/home/someone/.local/bin/pwrouter-agent")
        );
        let unit = render_unit(
            UNIT_TEMPLATE,
            &installed,
            Some(PathBuf::from("/home/someone")),
        );
        assert_eq!(unit.trim_end(), UNIT_TEMPLATE.trim_end());
        std::env::remove_var("HOME");
    }

    #[test]
    fn the_build_id_is_always_baked_in() {
        // build.rs guarantees it, and every surface (log line, `version`, the tray, the
        // `agent_version` in `hello`) reads the same string — an empty one would make
        // "which build is this?" unanswerable everywhere at once.
        assert!(!env!("PWROUTER_BUILD").is_empty());
        assert!(crate::version().contains(env!("CARGO_PKG_VERSION")));
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
