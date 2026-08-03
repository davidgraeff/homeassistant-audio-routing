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

use crate::config::{self, Config};
use crate::proto::{AgentMsg, DaemonMsg, HostState, PROTOCOL_VERSION};
use crate::pw_thread::{ramp_schedule, Cmd, Event, Handle, MasterState};
use anyhow::{anyhow, Context as _};
use pipewire as pw;
use futures_util::{SinkExt as _, StreamExt as _};
use std::time::Duration;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::Message;

/// mDNS service the daemon advertises for its control endpoint (plan §8).
const CONTROL_SERVICE_TYPE: &str = "_pwrouter-ctl._tcp.local.";

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
        let receiver = daemon.browse(CONTROL_SERVICE_TYPE).context("browsing for the daemon")?;
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
async fn daemon_address(config: &mut Config, override_addr: Option<&str>) -> anyhow::Result<String> {
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

/// Runs until the process is asked to stop. Reconnects on its own; only a
/// hard-denied pairing or a protocol mismatch is fatal.
pub async fn run(handle: Handle, mut events: tokio::sync::mpsc::UnboundedReceiver<Event>, override_addr: Option<String>) -> anyhow::Result<()> {
    let mut config = config::load()?;
    let mut backoff = BACKOFF_START;

    loop {
        let addr = match daemon_address(&mut config, override_addr.as_deref()).await {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!("cannot locate the daemon: {e}; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        match session(&addr, &mut config, &handle, &mut events).await {
            Ok(Outcome::Reconnect) => {
                backoff = BACKOFF_START;
            }
            Ok(Outcome::Paired) => {
                tracing::info!("pairing approved; reconnecting with the token");
                backoff = BACKOFF_START;
                continue;
            }
            Ok(Outcome::Denied(reason)) => {
                // A denial is a decision, not a transport failure: stop bothering
                // the daemon and let the user re-pair deliberately.
                return Err(anyhow!("daemon refused this agent: {reason}"));
            }
            Err(e) => {
                tracing::warn!("connection to {addr} ended: {e}");
                // The remembered address may be stale (daemon moved); fall back to
                // discovery on the next attempt unless it was given explicitly.
                if override_addr.is_none() {
                    config.daemon = None;
                }
            }
        }

        // Whatever ended the session, the host must not stay ducked.
        handle.send(Cmd::Unduck { ramp_ms: RESTORE_RAMP_MS });
        drive_ramp(handle.sender(), RESTORE_RAMP_MS);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

enum Outcome {
    /// Socket closed for a transient reason; try again.
    Reconnect,
    /// A token was just minted and persisted; reconnect immediately with it.
    Paired,
    /// The daemon refused us.
    Denied(String),
}

async fn session(
    addr: &str,
    config: &mut Config,
    handle: &Handle,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) -> anyhow::Result<Outcome> {
    let url = format!("ws://{addr}/api/agent/ws");
    tracing::info!("connecting to {url}");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.with_context(|| format!("connecting to {url}"))?;

    let hello = AgentMsg::Hello {
        protocol: PROTOCOL_VERSION,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        identity: config::identity(),
        label: config::label(),
        token: config.token.clone(),
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;

    let mut keepalive = Duration::from_secs(DEFAULT_KEEPALIVE_SECS) * 2;
    let mut last_seen = Instant::now();

    loop {
        // The deadline is checked by bounding the wait, so a silent socket (no
        // FIN, no RST — a rebooted daemon or a dead network) still trips it.
        let remaining = keepalive.saturating_sub(last_seen.elapsed());
        if remaining.is_zero() {
            tracing::warn!("no ping within {keepalive:?}; restoring and reconnecting");
            handle.send(Cmd::Unduck { ramp_ms: RESTORE_RAMP_MS });
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
                        match handle_daemon_msg(msg, config, handle, &mut keepalive, &mut ws).await? {
                            Some(outcome) => return Ok(outcome),
                            None => {}
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
                    Event::Master(master) => AgentMsg::State(host_state(&master)),
                    Event::ForeignSession(session) => AgentMsg::ForeignSession { session },
                };
                ws.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
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
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> anyhow::Result<Option<Outcome>> {
    match msg {
        DaemonMsg::PairPending { code } => {
            // Logged so the person clicking "Approve" can confirm the code matches
            // this host — an approval prompt alone can't tell two requests apart.
            tracing::warn!("waiting for approval in the add-on UI — pairing code: {code}");
            Ok(None)
        }
        DaemonMsg::Paired { token } => {
            config.token = Some(token);
            config::save(config).context("saving the pairing token")?;
            Ok(Some(Outcome::Paired))
        }
        DaemonMsg::Denied { reason } => Ok(Some(Outcome::Denied(reason))),
        DaemonMsg::Welcome { session_name, ifname, jitter_ms, keepalive_secs } => {
            *keepalive = Duration::from_secs(keepalive_secs.max(1)) * 2;
            // Reload even if the session is unchanged: a reconnect is exactly when
            // a resumed-from-suspend receiver needs rebuilding (plan §13.4).
            match handle.load_receiver(&session_name, ifname, jitter_ms) {
                Ok(()) => tracing::info!("receiving session '{session_name}'"),
                Err(e) => tracing::error!("could not become the receiver for '{session_name}': {e}"),
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
            ws.send(Message::Text(serde_json::to_string(&AgentMsg::Pong)?.into())).await?;
            Ok(None)
        }
    }
}
