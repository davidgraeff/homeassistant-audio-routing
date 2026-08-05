//! Desktop integration: a status-tray icon that shows the pairing code, and a
//! desktop notification that points the user at it.
//!
//! The agent is a background service with no window, so until now the only way to
//! read the pairing code was `journalctl --user -u pwrouter-agent`. That is fine
//! for a server and poor for a desktop, where the person who just installed the
//! helper is standing in front of a session that can perfectly well show them a
//! six-character code.
//!
//! Two deliberate limits:
//!
//! * **Both are optional, always.** A host with no session bus (a headless
//!   server), or a desktop with no tray implementation, gets exactly what it got
//!   before: the log line. Nothing here can fail the agent, and nothing here is
//!   the *only* way to reach any information — the journal keeps printing
//!   everything, and the add-on UI shows the same code on the host's card.
//! * **The menu shows state; it decides only what belongs to this machine.** The
//!   add-on drives volume and what is routed here — a tray that could unpair or quit
//!   would be a second, divergent way to manage a systemd unit that `Restart=always`
//!   would undo anyway, so the status rows stay disabled. What *is* local to this
//!   machine is which of its own outputs the audio should come out of, and whether
//!   the session starts the agent by itself; those two are settings, and they live
//!   here rather than in the add-on because the person at this keyboard is the one
//!   who knows which speakers they mean.
//!
//! Tray support on Linux is [StatusNotifierItem] over D-Bus: KDE, Xfce, Cinnamon,
//! MATE and most WM bars implement it natively; GNOME needs the AppIndicator
//! extension. `ksni` handles a watcher that appears *after* us (a user unit can
//! easily start before the shell does), which is why `assume_sni_available` is set
//! rather than treating "no watcher yet" as failure.
//!
//! [StatusNotifierItem]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/

use crate::autostart::{self, State as Autostart};
use crate::proto::HostState;
use crate::pw_thread::SinkInfo;
use ksni::{Handle, TrayMethods as _};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use zbus::zvariant::Value;

/// A tray's own handle, filled in the moment `spawn` returns it.
///
/// Menu callbacks get `&mut AgentTray` and must not block — anything that talks to
/// D-Bus (which the autostart switch does) has to run in a task, and that task then
/// needs a way back in to publish the *real* outcome. `ksni` has no
/// `Tray::handle()`, so the tray carries the slot its own handle lands in.
type TrayHandle = Arc<OnceLock<Handle<AgentTray>>>;

/// Icon shown in the tray, and the one on the notification. Deliberately the
/// *legacy* (non-`-symbolic`) names: those exist in Breeze and in Adwaita — via
/// its `Inherits=AdwaitaLegacy` — whereas the symbolic spellings of the two icons
/// wanted here are not both present in both themes.
const ICON: &str = "audio-speakers";
/// Shown instead of [`ICON`] while the item is in `NeedsAttention`.
const ATTENTION_ICON: &str = "dialog-password";

/// What a menu row asks the agent to do.
///
/// The tray applies nothing itself: it reports the choice and `client::run` — which
/// owns the config file and the PipeWire thread — persists it and acts on it, then
/// publishes the result back. One writer for the config, and the menu can never end
/// up showing a setting that was never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Pin playback to this sink (`node.name`), or `None` to follow the default.
    SetTarget(Option<String>),
}

/// Where the pairing stands, as far as this process knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pairing {
    /// No token, and the daemon has not answered our hello yet.
    Unpaired,
    /// The daemon is holding a pending request. `code` is what its UI shows, which
    /// is the code we offered unless the daemon had to mint one itself.
    Pending { code: String },
    /// A token is held and the daemon accepted it.
    Paired,
    /// The daemon refused us. Not fatal (see `client::run`), but a human usually
    /// has to do something about it.
    Refused { reason: String },
}

/// The tray item. Cheap to clone-free-update: `ksni` hands us `&mut Self` and
/// re-reads the menu, so state changes are plain field assignments.
struct AgentTray {
    /// `hostname (user)` — the same label the add-on's pairing card shows.
    label: String,
    pairing: Pairing,
    /// The code this process offered in `Hello`, kept so the menu can show one
    /// before the daemon has answered (or if it never does).
    offered_code: Option<String>,
    daemon: Option<String>,
    host: HostState,
    /// This host's playback sinks, as its PipeWire graph currently has them.
    sinks: Vec<SinkInfo>,
    /// The pinned sink (`node.name`), or `None` while following the default.
    target: Option<String>,
    /// Whether this session starts the agent by itself, as systemd reports it.
    autostart: Autostart,
    notifier: Option<Notifier>,
    handle: TrayHandle,
    /// Where a menu choice goes. `None` in tests and in the spike.
    requests: Option<tokio::sync::mpsc::UnboundedSender<Request>>,
}

impl AgentTray {
    /// The code to display: whatever the daemon is actually showing, if it told
    /// us, else the one we offered.
    fn code(&self) -> Option<&str> {
        match &self.pairing {
            Pairing::Pending { code } => Some(code.as_str()),
            _ => self.offered_code.as_deref(),
        }
    }

    fn needs_attention(&self) -> bool {
        !matches!(self.pairing, Pairing::Paired)
    }

    /// The pinned sink, if the graph currently has it.
    fn pinned_sink(&self) -> Option<&SinkInfo> {
        let target = self.target.as_deref()?;
        self.sinks.iter().find(|s| s.node_name == target)
    }

    /// How to name the chosen output in a sentence: its description while it is
    /// here, else the bare node name — which is all we know about a device that is
    /// unplugged, and still enough to recognise it.
    fn target_label(&self) -> Option<String> {
        let target = self.target.as_deref()?;
        Some(match self.pinned_sink() {
            Some(sink) => sink.description.clone(),
            None => target.to_string(),
        })
    }

    /// The picker's rows: `(what it selects, what it says)`, first one first.
    ///
    /// Split out and tested because two of its cases are easy to get wrong. A pin
    /// whose device is currently absent still gets a row — otherwise the radio would
    /// have nothing to sit on and the menu would show the user following the default
    /// when they are not. And "follow the default" stays on offer, because it is the
    /// behaviour every host had before there was a picker, and the right one for a
    /// laptop whose speakers change with its dock.
    fn target_choices(&self) -> Vec<(Option<String>, String)> {
        let mut choices: Vec<(Option<String>, String)> =
            vec![(None, "Follow the system default output".to_string())];
        choices.extend(
            self.sinks
                .iter()
                .map(|sink| (Some(sink.node_name.clone()), sink.description.clone())),
        );
        if let Some(target) = self.target.as_deref() {
            if self.pinned_sink().is_none() {
                choices.push((
                    Some(target.to_string()),
                    format!("{target} (not available)"),
                ));
            }
        }
        choices
    }

    /// The menu's text, in order. Split out from [`Self::menu`] because this is
    /// the part worth testing: no D-Bus, no callbacks, just state → strings.
    fn status_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(match &self.pairing {
            Pairing::Unpaired => "Not paired yet".to_string(),
            Pairing::Pending { .. } => "Waiting for approval in Home Assistant".to_string(),
            Pairing::Paired => "Paired".to_string(),
            Pairing::Refused { reason } => format!("Refused by the add-on: {reason}"),
        });
        if let Some(code) = self.code() {
            lines.push(format!("Pairing code: {code}"));
        }
        lines.push(match &self.daemon {
            Some(addr) => format!("Add-on: {addr}"),
            None => "Add-on: looking for it on the network".to_string(),
        });
        if self.pairing == Pairing::Paired {
            // What is actually happening, which is not always what was chosen — and
            // when the two differ, why. A pin whose device is unplugged is silent on
            // purpose (no fallback), and that is the one state a user would otherwise
            // read as a bug.
            lines.push(match (self.host.receiving, &self.host.sink_name) {
                (true, Some(sink)) => {
                    let shown = self
                        .sinks
                        .iter()
                        .find(|s| &s.node_name == sink)
                        .map(|s| s.description.clone())
                        .unwrap_or_else(|| sink.clone());
                    format!("Playing to: {shown}")
                }
                (true, None) => "Playing".to_string(),
                (false, _) => match (self.target_label(), self.pinned_sink().is_some()) {
                    (Some(label), false) => {
                        format!("Chosen output '{label}' is not available — nothing is played")
                    }
                    _ => "Idle — nothing routed here".to_string(),
                },
            });
            if let Some(volume) = self.host.volume {
                let muted = if self.host.muted == Some(true) {
                    "  (muted)"
                } else {
                    ""
                };
                lines.push(format!(
                    "Volume: {}%{muted}",
                    (volume * 100.0).round() as i32
                ));
            }
            if self.host.ducked {
                lines.push("Other audio is turned down for an announcement".to_string());
            }
        }
        lines
    }
}

impl ksni::Tray for AgentTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        format!("PipeWire audio router — {}", self.label)
    }

    fn category(&self) -> ksni::Category {
        // Not `ApplicationStatus`: this is the state of an audio output, which is
        // what `Hardware` is for, and what tray hosts group next to volume.
        ksni::Category::Hardware
    }

    fn status(&self) -> ksni::Status {
        if self.needs_attention() {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_name(&self) -> String {
        ICON.into()
    }

    fn attention_icon_name(&self) -> String {
        ATTENTION_ICON.into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let lines = self.status_lines();
        ksni::ToolTip {
            icon_name: if self.needs_attention() {
                ATTENTION_ICON.into()
            } else {
                ICON.into()
            },
            title: self.title(),
            description: lines.join("\n"),
            ..Default::default()
        }
    }

    // Left click opens the menu instead of doing nothing: the menu *is* the whole
    // interface, and an item that ignores a click reads as broken.
    const MENU_ON_ACTIVATE: bool = true;

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

        let mut items: Vec<MenuItem<Self>> = self
            .status_lines()
            .into_iter()
            .map(|label| {
                StandardItem {
                    label,
                    // The point of this menu: readable, not actionable.
                    enabled: false,
                    ..Default::default()
                }
                .into()
            })
            .collect();

        // Everything below the separator: the only rows that do anything.
        let mut actions: Vec<MenuItem<Self>> = Vec::new();

        // Which of this machine's outputs the audio comes out of. Hidden only when
        // the graph has told us nothing yet — an empty picker would read as "this
        // host has no speakers", which is never what an empty list means here.
        if !self.sinks.is_empty() {
            let choices = self.target_choices();
            let selected = choices
                .iter()
                .position(|(target, _)| target == &self.target)
                .unwrap_or(0);
            let requests = self.requests.clone();
            let options: Vec<RadioItem> = choices
                .iter()
                .map(|(_, label)| RadioItem {
                    label: label.clone(),
                    ..Default::default()
                })
                .collect();
            actions.push(
                SubMenu {
                    label: "Play to".into(),
                    submenu: vec![RadioGroup {
                        selected,
                        select: Box::new(move |this: &mut Self, index| {
                            let Some((target, _)) = this.target_choices().get(index).cloned()
                            else {
                                return;
                            };
                            // Shown at once so the radio doesn't sit on the old choice
                            // while the config is written; `set_target` publishes what
                            // was actually stored right after.
                            this.target = target.clone();
                            if let Some(requests) = &requests {
                                let _ = requests.send(Request::SetTarget(target));
                            }
                        }),
                        options,
                    }
                    .into()],
                    ..Default::default()
                }
                .into(),
            );
        }

        // The one setting, and the only row that changes anything about this
        // machine: whether the session starts the agent by itself. Hidden entirely
        // where there is no systemd user manager to ask.
        if self.autostart != Autostart::Unsupported {
            let slot = self.handle.clone();
            let notifier = self.notifier.clone();
            actions.push(
                SubMenu {
                    label: "Autostart".into(),
                    submenu: vec![RadioGroup {
                        selected: usize::from(self.autostart != Autostart::Enabled),
                        select: Box::new(move |this: &mut Self, selected| {
                            let want = selected == 0;
                            // Shown at once, so the radio does not sit on the old
                            // choice while D-Bus works; the task below replaces it
                            // with what systemd actually ends up reporting.
                            this.autostart = if want {
                                Autostart::Enabled
                            } else {
                                Autostart::Disabled
                            };
                            let (slot, notifier) = (slot.clone(), notifier.clone());
                            tokio::spawn(async move {
                                let outcome = if want {
                                    autostart::enable()
                                        .await
                                        .map(|path| tracing::info!("installed {}", path.display()))
                                } else {
                                    autostart::disable().await.inspect(|()| {
                                        tracing::info!("removed the systemd user unit")
                                    })
                                };
                                if let Err(e) = outcome {
                                    tracing::warn!("autostart change failed: {e:#}");
                                    if let Some(notifier) = &notifier {
                                        notifier
                                            .failed("Could not change autostart", &format!("{e:#}"))
                                            .await;
                                    }
                                }
                                // Publish the truth either way: a change that failed
                                // must not leave the menu claiming it worked.
                                let state = autostart::state().await;
                                if let Some(handle) = slot.get() {
                                    handle
                                        .update(|tray: &mut Self| tray.autostart = state)
                                        .await;
                                }
                            });
                        }),
                        options: vec![
                            RadioItem {
                                label: "Start at login".into(),
                                ..Default::default()
                            },
                            RadioItem {
                                label: "Don't start at login".into(),
                                ..Default::default()
                            },
                        ],
                    }
                    .into()],
                    ..Default::default()
                }
                .into(),
            );
        }

        // Only while there is something to approve — once paired, re-raising a
        // pairing notification would be noise.
        if let (Some(code), Some(notifier)) = (self.code(), self.notifier.clone()) {
            if self.needs_attention() {
                let (code, label) = (code.to_string(), self.label.clone());
                actions.push(
                    StandardItem {
                        label: "Show the pairing notification again".into(),
                        icon_name: ATTENTION_ICON.into(),
                        activate: Box::new(move |_this: &mut Self| {
                            // Off the callback immediately: it must not block the
                            // menu, and the D-Bus round trip is not instant.
                            let (notifier, code, label) =
                                (notifier.clone(), code.clone(), label.clone());
                            tokio::spawn(async move {
                                notifier.pairing(&code, &label, true).await;
                            });
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        if !actions.is_empty() {
            items.push(MenuItem::Separator);
            items.extend(actions);
        }
        items
    }

    fn watcher_online(&self) {
        tracing::debug!("tray host is back; the status icon is showing again");
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        // `true`: keep the service alive and wait. A GNOME shell restart, or a
        // desktop that has not finished starting, both land here and both come
        // back — and if none ever does, an idle D-Bus object costs nothing.
        tracing::debug!("no tray host ({reason:?}); the status icon is not showing");
        true
    }
}

/// Sends `org.freedesktop.Notifications` toasts. Its own session connection —
/// `ksni` keeps its own and does not lend it out; one extra socket is cheaper than
/// the coupling.
#[derive(Clone)]
pub struct Notifier {
    conn: zbus::Connection,
    /// Server-assigned id of the pairing toast, so a re-send *replaces* the
    /// bubble instead of stacking a new one.
    pairing_id: Arc<AtomicU32>,
    /// The code the pairing toast last showed. An unpaired agent reconnects on a
    /// backoff and gets `PairPending` every time; without this the user's screen
    /// would flash the same banner every few seconds.
    notified: Arc<Mutex<Option<String>>>,
}

impl Notifier {
    async fn connect() -> Option<Self> {
        match zbus::Connection::session().await {
            Ok(conn) => Some(Self {
                conn,
                pairing_id: Arc::new(AtomicU32::new(0)),
                notified: Arc::new(Mutex::new(None)),
            }),
            Err(e) => {
                tracing::debug!("no session bus ({e}); desktop notifications are off");
                None
            }
        }
    }

    /// One `Notify` call. Returns the id the server assigned, if it answered.
    async fn notify(
        &self,
        replaces: u32,
        icon: &str,
        summary: &str,
        body: &str,
        expire_ms: i32,
    ) -> Option<u32> {
        // Normal urgency, spelled out: `critical` is the one urgency servers keep
        // on screen until dismissed, and an optional pairing prompt has no claim to
        // that. The tray icon is what keeps the code readable afterwards.
        let hints: HashMap<&str, Value> = HashMap::from([("urgency", Value::U8(1))]);
        let reply = self
            .conn
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    "PipeWire audio router",
                    replaces,
                    icon,
                    summary,
                    body,
                    &[] as &[&str],
                    hints,
                    expire_ms,
                ),
            )
            .await;
        match reply {
            // A desktop with no notification daemon answers `ServiceUnknown`, and
            // that is a normal configuration, not a problem to report.
            Err(e) => {
                tracing::debug!("could not post a notification ({e})");
                None
            }
            Ok(reply) => {
                let id = reply.body().deserialize::<u32>().ok();
                tracing::debug!("posted notification {id:?}: {summary}");
                id
            }
        }
    }

    /// "Approve this machine, here is its code." `force` re-shows it even if this
    /// code was already announced (the menu item).
    pub async fn pairing(&self, code: &str, label: &str, force: bool) {
        {
            let mut notified = self.notified.lock().expect("notified mutex");
            if !force && notified.as_deref() == Some(code) {
                return;
            }
            *notified = Some(code.to_string());
        }
        let id = self
            .notify(
                self.pairing_id.load(Ordering::Relaxed),
                ATTENTION_ICON,
                "Pair this computer with Home Assistant",
                &format!(
                    "Open the audio router add-on, find \"{label}\" under discovered \
                     devices and press Pair.\nPairing code: {code}"
                ),
                // Never expire: the code has to stay readable while the user walks
                // to another device to approve it.
                0,
            )
            .await;
        if let Some(id) = id {
            self.pairing_id.store(id, Ordering::Relaxed);
        }
    }

    /// Something the user asked for from the menu did not work. The menu has no way
    /// to answer back, so this is where the reason goes — besides the log.
    pub async fn failed(&self, summary: &str, detail: &str) {
        self.notify(0, "dialog-error", summary, detail, 10_000)
            .await;
    }

    /// Replaces the pairing toast with a short confirmation, so a bubble asking for
    /// something already done cannot linger.
    pub async fn paired(&self) {
        self.notified.lock().expect("notified mutex").take();
        let replaces = self.pairing_id.swap(0, Ordering::Relaxed);
        self.notify(
            replaces,
            ICON,
            "Paired with Home Assistant",
            "This computer can now be used as an audio output.",
            5_000,
        )
        .await;
    }
}

/// What the rest of the agent talks to. Every method is a no-op when the desktop
/// side could not be set up, so callers never branch on it.
pub struct Desktop {
    tray: Option<Handle<AgentTray>>,
    notifier: Option<Notifier>,
}

impl Desktop {
    /// Never fails. `offered_code` is the code this process minted, `paired`
    /// whether a token was already on disk when we started, `target` the pinned sink
    /// from the config, and `requests` where menu choices are sent (`None` disables
    /// the rows that would need it).
    pub async fn start(
        label: String,
        offered_code: Option<String>,
        paired: bool,
        target: Option<String>,
        requests: Option<tokio::sync::mpsc::UnboundedSender<Request>>,
    ) -> Self {
        let notifier = Notifier::connect().await;
        let slot: TrayHandle = Arc::new(OnceLock::new());
        let tray = AgentTray {
            label,
            pairing: if paired {
                Pairing::Paired
            } else {
                Pairing::Unpaired
            },
            offered_code,
            daemon: None,
            host: HostState::default(),
            sinks: Vec::new(),
            target,
            autostart: autostart::state().await,
            notifier: notifier.clone(),
            handle: slot.clone(),
            requests,
        };
        let tray = match tray
            // A user unit can start before the desktop shell: treat "no watcher"
            // as "not yet" rather than as failure.
            .assume_sni_available(true)
            .spawn()
            .await
        {
            Ok(handle) => {
                // Before any menu can be opened, so a callback's task always finds it.
                let _ = slot.set(handle.clone());
                Some(handle)
            }
            Err(e) => {
                tracing::debug!("no status tray ({e}); the journal remains the way to read state");
                None
            }
        };
        Self { tray, notifier }
    }

    async fn update(&self, f: impl FnOnce(&mut AgentTray)) {
        if let Some(tray) = &self.tray {
            tray.update(f).await;
        }
    }

    /// The daemon is holding a pending request: show the code everywhere.
    pub async fn pair_pending(&self, code: &str) {
        self.update(|tray| {
            tray.pairing = Pairing::Pending {
                code: code.to_string(),
            }
        })
        .await;
        if let Some(notifier) = &self.notifier {
            let label = crate::config::label();
            notifier.pairing(code, &label, false).await;
        }
    }

    pub async fn paired(&self) {
        self.update(|tray| tray.pairing = Pairing::Paired).await;
        if let Some(notifier) = &self.notifier {
            notifier.paired().await;
        }
    }

    pub async fn refused(&self, reason: &str) {
        let reason = reason.to_string();
        self.update(|tray| tray.pairing = Pairing::Refused { reason })
            .await;
    }

    /// Forgotten token: back to square one, so the code becomes interesting again.
    pub async fn unpaired(&self) {
        self.update(|tray| tray.pairing = Pairing::Unpaired).await;
    }

    pub async fn set_daemon(&self, addr: Option<String>) {
        self.update(|tray| tray.daemon = addr).await;
    }

    pub async fn set_host(&self, host: HostState) {
        self.update(|tray| tray.host = host).await;
    }

    /// The host's sinks changed: re-offer them.
    pub async fn set_sinks(&self, sinks: Vec<SinkInfo>) {
        self.update(|tray| tray.sinks = sinks).await;
    }

    /// The pin as it now *is* — called after the choice has been stored, so the menu
    /// never claims a setting that wasn't written.
    pub async fn set_target(&self, target: Option<String>) {
        self.update(|tray| tray.target = target).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tray(pairing: Pairing) -> AgentTray {
        AgentTray {
            label: "david-local (david)".into(),
            pairing,
            offered_code: Some("4F2A9C".into()),
            daemon: Some("192.168.1.20:8099".into()),
            host: HostState::default(),
            sinks: Vec::new(),
            target: None,
            autostart: Autostart::Unsupported,
            notifier: None,
            handle: Arc::new(OnceLock::new()),
            requests: None,
        }
    }

    /// Two sinks, as a host with built-in audio and a headset would report them.
    fn two_sinks() -> Vec<SinkInfo> {
        vec![
            SinkInfo {
                node_name: "alsa_output.pci-0000_00_1f.3.analog-stereo".into(),
                description: "Built-in Audio Analogue Stereo".into(),
            },
            SinkInfo {
                node_name: "alsa_output.usb-Focusrite".into(),
                description: "Scarlett Solo".into(),
            },
        ]
    }

    #[test]
    fn an_unpaired_host_shows_the_code_it_offered() {
        let lines = tray(Pairing::Unpaired).status_lines();
        assert_eq!(lines[0], "Not paired yet");
        assert_eq!(lines[1], "Pairing code: 4F2A9C");
    }

    #[test]
    fn a_pending_request_shows_the_daemons_code_not_ours() {
        // The daemon mints its own when the one we offered was missing or
        // malformed; the tray must then show what the add-on's card shows.
        let lines = tray(Pairing::Pending {
            code: "AB12CD".into(),
        })
        .status_lines();
        assert!(lines[0].contains("Waiting for approval"));
        assert_eq!(lines[1], "Pairing code: AB12CD");
    }

    #[test]
    fn a_paired_idle_host_reports_being_idle_and_hides_volume() {
        let lines = tray(Pairing::Paired).status_lines();
        assert_eq!(lines[0], "Paired");
        assert!(lines.iter().any(|l| l.contains("Idle")));
        assert!(!lines.iter().any(|l| l.starts_with("Volume")));
    }

    #[test]
    fn a_receiving_host_reports_sink_volume_and_duck() {
        let mut t = tray(Pairing::Paired);
        t.host = HostState {
            volume: Some(0.42),
            muted: Some(true),
            sink_name: Some("alsa_output.pci-0000_00_1f.3".into()),
            receiving: true,
            ducked: true,
        };
        let lines = t.status_lines();
        assert!(lines.contains(&"Playing to: alsa_output.pci-0000_00_1f.3".to_string()));
        assert!(lines.contains(&"Volume: 42%  (muted)".to_string()));
        assert!(lines.iter().any(|l| l.contains("turned down")));
    }

    #[test]
    fn host_state_is_not_shown_before_pairing() {
        // Volume and sink come from our own PipeWire thread and are known even
        // while unpaired — but showing them next to "not paired yet" would imply
        // the add-on is already driving this machine.
        let mut t = tray(Pairing::Unpaired);
        t.host = HostState {
            volume: Some(0.5),
            receiving: true,
            ..Default::default()
        };
        assert!(!t.status_lines().iter().any(|l| l.starts_with("Volume")));
    }

    #[test]
    fn only_an_unsettled_pairing_asks_for_attention() {
        assert!(tray(Pairing::Unpaired).needs_attention());
        assert!(tray(Pairing::Pending { code: "A".into() }).needs_attention());
        assert!(tray(Pairing::Refused { reason: "x".into() }).needs_attention());
        assert!(!tray(Pairing::Paired).needs_attention());
    }

    #[test]
    fn the_picker_offers_the_default_first_then_every_sink() {
        let mut t = tray(Pairing::Paired);
        t.sinks = two_sinks();
        let choices = t.target_choices();
        assert_eq!(choices[0].0, None, "following the default stays on offer");
        assert!(choices[0].1.contains("default"));
        assert_eq!(
            choices
                .iter()
                .skip(1)
                .map(|(target, _)| target.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("alsa_output.pci-0000_00_1f.3.analog-stereo".to_string()),
                Some("alsa_output.usb-Focusrite".to_string()),
            ]
        );
        // Descriptions, not node names: the node name is the key, not the label.
        assert_eq!(choices[2].1, "Scarlett Solo");
    }

    #[test]
    fn a_chosen_sink_that_is_unplugged_keeps_its_row_and_the_selection() {
        // Otherwise the radio would fall back to the first row and the menu would
        // claim the user is following the default — the one thing a pin must never
        // appear to do.
        let mut t = tray(Pairing::Paired);
        t.sinks = two_sinks();
        t.target = Some("alsa_output.usb-Focusrite".into());
        let selected = |t: &AgentTray| {
            t.target_choices()
                .iter()
                .position(|(target, _)| target == &t.target)
                .unwrap()
        };
        assert_eq!(selected(&t), 2);

        t.sinks.remove(1); // unplugged
        let choices = t.target_choices();
        assert_eq!(selected(&t), choices.len() - 1);
        assert!(choices.last().unwrap().1.contains("not available"));
    }

    #[test]
    fn an_unavailable_pin_is_reported_as_silence_not_as_idle() {
        // "Idle — nothing routed here" would send the user looking in the add-on for
        // a problem that is on this machine.
        let mut t = tray(Pairing::Paired);
        t.sinks = two_sinks();
        t.target = Some("alsa_output.usb-Focusrite".into());
        t.sinks.remove(1);
        let lines = t.status_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("not available") && l.contains("nothing is played")),
            "{lines:?}"
        );

        // Chosen, present, but the add-on is sending nothing here: that *is* idle.
        t.sinks = two_sinks();
        assert!(t.status_lines().iter().any(|l| l.contains("Idle")));
    }

    #[test]
    fn what_is_playing_is_named_the_way_the_picker_names_it() {
        let mut t = tray(Pairing::Paired);
        t.sinks = two_sinks();
        t.target = Some("alsa_output.usb-Focusrite".into());
        t.host = HostState {
            sink_name: Some("alsa_output.usb-Focusrite".into()),
            receiving: true,
            ..Default::default()
        };
        assert!(t
            .status_lines()
            .contains(&"Playing to: Scarlett Solo".to_string()));
    }

    #[test]
    fn a_host_that_cannot_find_the_addon_says_so() {
        let mut t = tray(Pairing::Unpaired);
        t.daemon = None;
        assert!(t
            .status_lines()
            .iter()
            .any(|l| l.contains("looking for it on the network")));
    }
}
