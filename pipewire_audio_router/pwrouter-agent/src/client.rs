//! The control plane: discover the daemon, pair once, then hold one outbound
//! WebSocket and translate its command enum into `pw_thread::Cmd`s.
//!
//! Everything here is ordinary async code; nothing touches libpipewire (that is
//! `pw_thread`'s exclusive job). The safety rails from plan §9 live here:
//!
//! * **keepalive deadline** — if no `Ping` arrives within twice the daemon's
//!   advertised interval, the agent assumes the other end is gone, restores any
//!   duck *itself*, and reconnects. A daemon-side timer cannot cover this: the
//!   daemon is exactly the thing that may have died.
//! * **restore on every disconnect**, not just on exit, so a dropped socket can
//!   never leave the user's music attenuated.
//!
//! The receive-side module is deliberately *not* torn down on disconnect: audio
//! should keep playing while control is briefly unavailable. It is reloaded on the
//! next `Welcome`, which doubles as the sleep/resume remedy (plan §13.4).
//!
//! Every state change logged here is also pushed to [`Desktop`], which mirrors it
//! into a tray icon and (for pairing) a notification on sessions that have them.
//! That is presentation only: the log lines remain the record, and a host without a
//! desktop behaves exactly as before.

use crate::autostart;
use crate::config::{self, Config};
use crate::desktop::{Desktop, Request};
use crate::proto::{AgentMsg, DaemonMsg, HostState, PROTOCOL_VERSION};
use crate::pw_thread::{ramp_schedule, Cmd, Event, Handle, MasterState};
use anyhow::{anyhow, Context as _};
use futures_util::{SinkExt as _, StreamExt as _};
use pipewire as pw;
use std::time::Duration;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::Message;

/// mDNS service the daemon advertises for its control endpoint (plan §8).
const CONTROL_SERVICE_TYPE: &str = "_pwrouter-ctl._tcp.local.";

/// Depth of the tray → client request queue. Bounded on the same principle as
/// every other queue here; one click produces one request, so this only has to
/// outlast a momentary stall in the client loop.
pub const REQUEST_DEPTH: usize = 16;

/// How long to wait for mDNS to turn up a daemon before giving up on a try.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect backoff bounds. Short at first (a daemon restart should be picked up
/// promptly), capped so a long outage doesn't hammer the network.
const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Fallback when the daemon's `Welcome` omits an interval.
const DEFAULT_KEEPALIVE_SECS: u64 = 15;

/// Ramp used when the agent restores on its own (deadline, disconnect): quick but
/// not a step, since the user may be listening to the very stream coming back.
const RESTORE_RAMP_MS: u64 = 150;

/// Finds the daemon over mDNS. Blocking browse on a worker thread — `mdns-sd`'s
/// receiver is synchronous, and this runs at most once per connection attempt.
async fn discover_daemon() -> anyhow::Result<String> {
    tokio::task::spawn_blocking(|| {
        let daemon = mdns_sd::ServiceDaemon::new().context("starting mDNS browser")?;
        let receiver = daemon
            .browse(CONTROL_SERVICE_TYPE)
            .context("browsing for the daemon")?;
        let deadline = std::time::Instant::now() + DISCOVERY_TIMEOUT;
        let found = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break None;
            }
            match receiver.recv_timeout(remaining) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    if let Some(addr) = info.get_addresses_v4().into_iter().next() {
                        break Some(format!("{addr}:{}", info.get_port()));
                    }
                }
                Ok(_) => continue,
                Err(_) => break None,
            }
        };
        let _ = daemon.shutdown();
        found.ok_or_else(|| anyhow!("no {CONTROL_SERVICE_TYPE} daemon found on the LAN"))
    })
    .await
    .context("mDNS browse task panicked")?
}

/// Resolves the daemon address: explicit override, remembered address, discovery.
async fn daemon_address(
    config: &mut Config,
    override_addr: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(addr) = override_addr {
        return Ok(addr.to_string());
    }
    if let Some(addr) = config.daemon.clone() {
        return Ok(addr);
    }
    let addr = discover_daemon().await?;
    tracing::info!("discovered daemon at {addr}");
    config.daemon = Some(addr.clone());
    let _ = config::save(config);
    Ok(addr)
}

/// Drives an in-flight ramp: the PipeWire thread holds the ramp state, this just
/// paces the steps. One extra tick is sent so a rounding-down never leaves a ramp
/// a step short of its target.
fn drive_ramp(sender: pw::channel::Sender<Cmd>, ramp_ms: u64) {
    let (steps, interval) = ramp_schedule(ramp_ms);
    tokio::spawn(async move {
        for _ in 0..=steps {
            tokio::time::sleep(interval).await;
            if sender.send(Cmd::RampTick).is_err() {
                return; // PipeWire thread gone; nothing left to ramp.
            }
        }
    });
}

fn host_state(master: &MasterState) -> HostState {
    HostState {
        volume: master.volume,
        muted: master.muted,
        sink_name: master.sink_name.clone(),
        receiving: master.receiving,
        ducked: master.ducked,
    }
}

/// Runs until the process is asked to stop. Reconnects on its own, always: no
/// answer from the daemon ends this loop, because the daemon is the end that may
/// have been reinstalled, restored from a snapshot, or had this host unpaired.
pub async fn run(
    handle: Handle,
    mut events: tokio::sync::mpsc::Receiver<Event>,
    override_addr: Option<String>,
) -> anyhow::Result<()> {
    let mut config = config::load()?;
    let mut backoff = BACKOFF_START;
    // Minted once per process, so the code the approver compares is the one this
    // host printed at startup and keeps printing across reconnects. Deliberately
    // not persisted: restarting the agent is then the way to ask for a fresh code.
    let pair_code = new_pair_code();
    if config.token.is_none() {
        tracing::warn!("not paired yet — pairing code for this host: {pair_code}");
    } else {
        tracing::info!("pairing code, should this host ever need to pair again: {pair_code}");
    }

    // Never fails: a host with no tray and no notification server gets a `Desktop`
    // whose every method is a no-op.
    let (req_tx, mut requests) = tokio::sync::mpsc::channel::<Request>(REQUEST_DEPTH);
    let desktop = Desktop::start(
        config::label(),
        (!pair_code.is_empty()).then(|| pair_code.clone()),
        config.token.is_some(),
        config.target_sink.clone(),
        Some(req_tx),
    )
    .await;
    // The stored pin has to reach the PipeWire thread before any `welcome` does,
    // or the first session would play out of the default sink.
    if let Some(sink) = &config.target_sink {
        tracing::info!("playing to '{sink}' (chosen on this machine; no automatic switching)");
        handle.send(Cmd::SetTargetSink(Some(sink.clone())));
    }

    loop {
        let addr = match daemon_address(&mut config, override_addr.as_deref()).await {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!("cannot locate the daemon: {e}; retrying in {backoff:?}");
                desktop.set_daemon(None).await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        desktop.set_daemon(Some(addr.clone())).await;

        // Did this attempt get a socket up at all? What follows treats "the daemon was
        // there and the connection later broke" differently from "the daemon is not
        // answering" — see the `Err` arm.
        let mut connected = false;
        match session(
            &addr,
            &mut config,
            &pair_code,
            &handle,
            &mut events,
            &desktop,
            &mut requests,
            &mut connected,
        )
        .await
        {
            Ok(Outcome::Reconnect) => {
                backoff = BACKOFF_START;
            }
            Ok(Outcome::Paired) => {
                tracing::info!("pairing approved; reconnecting with the token");
                backoff = BACKOFF_START;
                continue;
            }
            // The only way out of this loop other than a fatal error: returning lets
            // `main` drop the PipeWire handle, which restores the host's level and
            // unloads the receiver exactly as a signal would.
            Ok(Outcome::Quit) => return Ok(()),
            Ok(Outcome::Denied(reason)) => {
                // Never fatal. A token we hold that the daemon does not honour is
                // worthless — it was revoked by an unpair, or its store was lost —
                // so drop it and let the next hello be a pair request. That is what
                // makes this host show up as pairable again in the add-on, with no
                // login on this machine (plan §8).
                if config.token.take().is_some() {
                    let _ = config::save(&config);
                    tracing::warn!(
                        "the add-on no longer accepts this host's pairing ({reason}); \
                         asking to pair again with code {pair_code}"
                    );
                    // Back to offering a code, so the tray stops claiming this host
                    // is paired and starts showing what to approve.
                    desktop.unpaired().await;
                } else {
                    tracing::warn!(
                        "the add-on refused this host: {reason}; retrying in {backoff:?}"
                    );
                    desktop.refused(&reason).await;
                }
            }
            Err(e) => {
                tracing::warn!("connection to {addr} ended: {e}");
                // The remembered address may be stale (daemon moved); fall back to
                // discovery on the next attempt unless it was given explicitly.
                if override_addr.is_none() {
                    config.daemon = None;
                }
                // A socket that was up and then broke is not evidence that the daemon
                // is unreachable — it is what an add-on restart looks like from here
                // (the WS dies mid-frame: "Connection reset without closing
                // handshake"), and the daemon is usually back within seconds. Without
                // this reset the backoff only ever grew, so from the second restart
                // onwards every one cost a **full minute** of a host that was offline
                // in the add-on while the agent sat waiting — measured 2026-08-12,
                // where every reconnect in the journal was exactly 60 s late. Only a
                // genuinely unreachable daemon (connect refused, no mDNS) may back off.
                if connected {
                    backoff = BACKOFF_START;
                }
            }
        }

        // Whatever ended the session, the host must not stay ducked.
        handle.send(Cmd::Unduck {
            ramp_ms: RESTORE_RAMP_MS,
        });
        drive_ramp(handle.sender(), RESTORE_RAMP_MS);
        // Keep serving the tray while waiting to reconnect: picking an output is a
        // local setting and has nothing to do with the add-on being reachable, so it
        // must not be swallowed for up to a minute (or lost with the process).
        let until = tokio::time::Instant::now() + backoff;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(until) => break,
                request = requests.recv() => match request {
                    // Quitting has to work here too — a host that cannot reach the add-on
                    // is exactly when someone reaches for the tray.
                    Some(request) => match apply_request(request, &mut config, &handle, &desktop).await {
                        std::ops::ControlFlow::Continue(()) => {}
                        std::ops::ControlFlow::Break(()) => return Ok(()),
                    },
                    // The tray is gone (it never existed, or its task ended); just wait.
                    None => { tokio::time::sleep_until(until).await; break }
                },
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Applies one tray choice: store it, act on it, then publish what was stored.
///
/// The order matters. Persisting first means a choice survives even if the reload
/// below fails; publishing last means the menu shows the truth rather than the
/// optimistic value the callback already painted.
///
/// Volume and mute are the two that store *nothing*: the sink's level is the host's
/// own, kept by WirePlumber across reboots and moved by the user's volume keys, so this
/// end must never hold a second copy of it. They go straight to the PipeWire thread —
/// the same `Cmd`s the add-on's `SetVolume`/`SetMute` produce — and the resulting graph
/// change comes back as a `Master` event, which is what corrects the value the tray
/// painted optimistically and what tells the add-on (§9.4).
/// `Break` means the agent was asked to stop and this loop must return, so `main`'s
/// shutdown runs (dropping the PipeWire handle restores the host and unloads the
/// receiver — plan §9.1, the same path SIGTERM takes).
async fn apply_request(
    request: Request,
    config: &mut Config,
    handle: &Handle,
    desktop: &Desktop,
) -> std::ops::ControlFlow<()> {
    match request {
        Request::Quit => {
            // The service instance must not merely exit: its unit is `Restart=always`,
            // so systemd would bring it straight back and the menu item would look
            // broken. Ask systemd to stop the unit instead and keep running — the
            // SIGTERM it sends in reply is what ends us, through the ordinary path.
            if autostart::is_the_running_service().await {
                match autostart::stop().await {
                    Ok(()) => {
                        tracing::info!(
                            "quit from the tray: asked systemd to stop {}",
                            autostart::UNIT_NAME
                        );
                        return std::ops::ControlFlow::Continue(());
                    }
                    // Falling through to a plain exit is better than ignoring the click:
                    // a respawn is at least visible, whereas nothing happening is not.
                    Err(e) => {
                        tracing::warn!("could not stop the unit ({e:#}); exiting directly instead")
                    }
                }
            }
            tracing::info!("quit from the tray; restoring host state");
            return std::ops::ControlFlow::Break(());
        }
        Request::SetVolume(volume) => {
            tracing::debug!("volume set to {:.0}% on this machine", volume * 100.0);
            handle.send(Cmd::SetMasterVolume(volume));
        }
        Request::SetMute(muted) => {
            tracing::debug!(
                "{} on this machine",
                if muted { "muted" } else { "unmuted" }
            );
            handle.send(Cmd::SetMasterMute(muted));
        }
        Request::SetTarget(target) => {
            if config.target_sink == target {
                return std::ops::ControlFlow::Continue(());
            }
            config.target_sink = target.clone();
            if let Err(e) = config::save(config) {
                tracing::warn!("could not remember the chosen output: {e:#}");
            }
            match &target {
                Some(sink) => tracing::info!("chosen output is now '{sink}'"),
                None => tracing::info!("chosen output is now the system default"),
            }
            handle.send(Cmd::SetTargetSink(target.clone()));
            desktop.set_target(target).await;
        }
    }
    std::ops::ControlFlow::Continue(())
}

enum Outcome {
    /// Socket closed for a transient reason; try again.
    Reconnect,
    /// A token was just minted and persisted; reconnect immediately with it.
    Paired,
    /// The daemon refused us. Handled, not fatal — see `run`.
    Denied(String),
    /// The tray asked the agent to stop. The one outcome that ends `run`.
    Quit,
}

/// Six uppercase hex characters, the shape the daemon validates. Read straight
/// from `/dev/urandom` rather than pulling in an RNG crate for one string; an
/// unreadable `/dev/urandom` yields `None` so the daemon mints the code instead of
/// this offering something predictable.
fn new_pair_code() -> String {
    let mut buf = [0u8; 3];
    match std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
    {
        Ok(()) => buf.iter().map(|b| format!("{b:02X}")).collect(),
        Err(e) => {
            tracing::warn!(
                "cannot read /dev/urandom ({e}); letting the add-on pick the pairing code"
            );
            String::new()
        }
    }
}

/// `connected` is set the moment the socket is up, so the caller can tell a broken
/// connection (retry promptly) from an absent daemon (back off).
#[allow(clippy::too_many_arguments)]
async fn session(
    addr: &str,
    config: &mut Config,
    pair_code: &str,
    handle: &Handle,
    events: &mut tokio::sync::mpsc::Receiver<Event>,
    desktop: &Desktop,
    requests: &mut tokio::sync::mpsc::Receiver<Request>,
    connected: &mut bool,
) -> anyhow::Result<Outcome> {
    let url = format!("ws://{addr}/api/agent/ws");
    tracing::info!("connecting to {url}");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    *connected = true;

    let hello = AgentMsg::Hello {
        protocol: PROTOCOL_VERSION,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        machine_id: config::machine_id(),
        hostname: config::hostname(),
        user: config::user(),
        token: config.token.clone(),
        pair_code: (!pair_code.is_empty()).then(|| pair_code.to_string()),
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;

    let mut keepalive = Duration::from_secs(DEFAULT_KEEPALIVE_SECS) * 2;
    let mut last_seen = Instant::now();

    loop {
        // The deadline is checked by bounding the wait, so a silent socket (no
        // FIN, no RST — a rebooted daemon or a dead network) still trips it.
        let remaining = keepalive.saturating_sub(last_seen.elapsed());
        if remaining.is_zero() {
            tracing::warn!("no ping within {keepalive:?}; restoring and reconnecting");
            handle.send(Cmd::Unduck {
                ramp_ms: RESTORE_RAMP_MS,
            });
            drive_ramp(handle.sender(), RESTORE_RAMP_MS);
            return Ok(Outcome::Reconnect);
        }

        tokio::select! {
            biased;

            incoming = timeout(remaining, ws.next()) => {
                let Ok(incoming) = incoming else { continue }; // deadline handled above
                let Some(message) = incoming else { return Ok(Outcome::Reconnect) };
                let message = message?;
                last_seen = Instant::now();
                match message {
                    Message::Text(text) => {
                        let Ok(msg) = serde_json::from_str::<DaemonMsg>(&text) else {
                            // Unknown/newer message: ignore it rather than guess
                            // (proto.rs keeps this a closed enum on purpose).
                            tracing::debug!("ignoring unrecognised daemon message: {text}");
                            continue;
                        };
                        if let Some(outcome) = handle_daemon_msg(msg, config, handle, &mut keepalive, &mut ws, desktop).await? {
                            return Ok(outcome);
                        }
                    }
                    Message::Ping(payload) => ws.send(Message::Pong(payload)).await?,
                    Message::Close(_) => return Ok(Outcome::Reconnect),
                    _ => {}
                }
            }

            event = events.recv() => {
                let Some(event) = event else { return Err(anyhow!("PipeWire thread is gone")) };
                let msg = match event {
                    Event::Master(master) => {
                        let state = host_state(&master);
                        desktop.set_host(state.clone()).await;
                        AgentMsg::State(state)
                    }
                    // The host's own outputs are nobody's business but this machine's:
                    // the picker is local, so this goes to the tray and not the wire.
                    Event::Sinks(sinks) => {
                        desktop.set_sinks(sinks).await;
                        continue;
                    }
                    Event::ForeignSession(session) => AgentMsg::ForeignSession { session },
                };
                ws.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
            }

            request = requests.recv() => {
                match request {
                    Some(request) => match apply_request(request, config, handle, desktop).await {
                        std::ops::ControlFlow::Continue(()) => {}
                        std::ops::ControlFlow::Break(()) => return Ok(Outcome::Quit),
                    },
                    // Tray gone: stop selecting on a closed channel, which would
                    // otherwise return immediately forever and spin this loop.
                    None => std::future::pending::<()>().await,
                }
            }
        }
    }
}

/// Applies one daemon message. `Some(outcome)` ends the session.
async fn handle_daemon_msg(
    msg: DaemonMsg,
    config: &mut Config,
    handle: &Handle,
    keepalive: &mut Duration,
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    desktop: &Desktop,
) -> anyhow::Result<Option<Outcome>> {
    match msg {
        DaemonMsg::PairPending { code } => {
            // Logged so the person clicking "Approve" can confirm the code matches
            // this host — an approval prompt alone can't tell two requests apart.
            tracing::warn!("waiting for approval in the add-on UI — pairing code: {code}");
            // Notified for the same reason, on the desktops that can: the person who
            // just installed this is at *this* machine, not tailing its journal. Sent
            // with the daemon's code, which is ours unless it had to mint one. Repeat
            // pendings from reconnects do not re-raise the banner.
            desktop.pair_pending(&code).await;
            Ok(None)
        }
        DaemonMsg::Paired { token } => {
            config.token = Some(token);
            config::save(config).context("saving the pairing token")?;
            desktop.paired().await;
            Ok(Some(Outcome::Paired))
        }
        DaemonMsg::Denied { reason } => Ok(Some(Outcome::Denied(reason))),
        DaemonMsg::Welcome {
            session_name,
            ifname,
            jitter_ms,
            keepalive_secs,
        } => {
            *keepalive = Duration::from_secs(keepalive_secs.max(1)) * 2;
            // Reload even if the session is unchanged: a reconnect is exactly when
            // a resumed-from-suspend receiver needs rebuilding (plan §13.4).
            match handle.load_receiver(&session_name, ifname, jitter_ms) {
                Ok(()) => tracing::info!("receiving session '{session_name}'"),
                Err(e) => {
                    tracing::error!("could not become the receiver for '{session_name}': {e}")
                }
            }
            Ok(None)
        }
        DaemonMsg::Release => {
            tracing::info!("released by the daemon; unloading the receiver");
            handle.send(Cmd::UnloadReceiver);
            Ok(None)
        }
        DaemonMsg::SetVolume { volume } => {
            handle.send(Cmd::SetMasterVolume(volume));
            Ok(None)
        }
        DaemonMsg::SetMute { muted } => {
            handle.send(Cmd::SetMasterMute(muted));
            Ok(None)
        }
        DaemonMsg::Duck { depth, ramp_ms } => {
            handle.send(Cmd::DuckOthers { depth, ramp_ms });
            drive_ramp(handle.sender(), ramp_ms);
            Ok(None)
        }
        DaemonMsg::Unduck { ramp_ms } => {
            handle.send(Cmd::Unduck { ramp_ms });
            drive_ramp(handle.sender(), ramp_ms);
            Ok(None)
        }
        DaemonMsg::Ping => {
            ws.send(Message::Text(
                serde_json::to_string(&AgentMsg::Pong)?.into(),
            ))
            .await?;
            Ok(None)
        }
    }
}
