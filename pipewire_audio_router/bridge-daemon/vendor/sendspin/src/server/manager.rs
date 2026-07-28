// ABOUTME: Continuous discovery + reconnect-with-backoff supervision for clients
// ABOUTME: that only run their own embedded server (the supervised form of dial_client)

use crate::protocol::messages::{ClientHello, Message};
use crate::server::connection::{ServerConnection, ServerSender};
use crate::server::discovery::{ClientBrowser, Discovered};
use crate::server::role::ServerRole;
use mdns_sd::ServiceDaemon;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Initial reconnect backoff, and the value backoff resets to after a stable
/// session or an address change.
const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// Reconnect backoff ceiling — matches aiosendspin's `MAX_RECONNECT_BACKOFF_S`.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// A connection must last at least this long before backoff resets to the
/// minimum (matches aiosendspin's `STABLE_SERVER_INITIATED_SESSION_S`), so a
/// crash-looping device isn't hammered at 1-second intervals.
const STABLE_SESSION: Duration = Duration::from_secs(10);

/// Control signal sent to a client supervisor task via a watch channel.
#[derive(Clone, Debug)]
enum Directive {
    /// (Re)dial the client at this URL. A new URL for an already-connected
    /// client makes the supervisor drop the current connection and redial.
    Dial(String),
    /// Stop supervising this client and end its task. Sent only by
    /// [`ClientManager::stop_client`] — an mDNS removal is a possible power-save
    /// lapse rather than proof of departure, so it does not imply this.
    Stop,
}

/// Events for a client discovered and managed by [`ClientManager`]. The
/// manager owns each connection's message loop internally so it can detect
/// disconnection and drive reconnects — callers get a [`ServerSender`] for
/// control instead of the raw [`ServerConnection`].
#[derive(Debug)]
pub enum ClientEvent {
    /// A client connected — the first time, or after a reconnect. `client_id`
    /// is stable across reconnects (it comes from the client's own
    /// `client/hello`), so callers can use it as the group-membership key.
    Connected {
        /// The connected client's identifier, from its `client/hello`.
        client_id: String,
        /// The mDNS instance fullname this connection was dialed from — stable
        /// discovery identity, lets callers map a connection back to the
        /// discovered service (the `client_id` may be an opaque MAC that does
        /// not match the advertised name).
        fullname: String,
        /// Roles this server granted the client.
        active_roles: Vec<String>,
        /// The client's own `client/hello` — its advertised capabilities
        /// (`player@v1_support.supported_formats`, buffer capacity, supported
        /// commands) and `device_info`. Forwarded because the manager owns the
        /// connection internally, so a caller that never sees the
        /// [`crate::server::ServerConnection`] would otherwise have no way to read
        /// them — and without them a server cannot negotiate a codec/rate the
        /// device actually supports (it can only guess). Boxed to keep the event
        /// enum small.
        hello: Box<ClientHello>,
        /// Sender for pushing stream/audio/command messages to this client.
        sender: ServerSender,
    },
    /// A `client/state`, `client/command`, or `client/goodbye` message from
    /// a connected client (`client/time` is consumed internally and never
    /// forwarded, same convention as [`ServerConnection::recv_message`]).
    Message {
        /// Which client sent this message.
        client_id: String,
        /// The message itself.
        message: Box<Message>,
    },
    /// The client disconnected. A reconnect attempt is already running in the
    /// background — unconditionally, since supervision outlives an mDNS lapse — so
    /// this just tells the caller to stop treating `client_id` as a live group
    /// member for now.
    Disconnected {
        /// The client that disconnected.
        client_id: String,
    },
}

struct ManagedClient {
    handle: JoinHandle<()>,
    directive_tx: watch::Sender<Directive>,
    url: String,
}

/// Discovers Sendspin clients that only run their own embedded server and keeps
/// each one connected: dials on discovery, retries with capped exponential
/// backoff on failure or disconnect, and re-dials promptly if a device reappears
/// at a new address. A device whose mDNS advertisement lapses keeps its supervisor
/// — see `Discovered::Removed` in the browse loop for why — so supervision ends
/// only when the caller asks for it ([`ClientManager::stop_client`]) or the manager
/// is dropped.
pub struct ClientManager {
    tasks: Arc<Mutex<HashMap<String, ManagedClient>>>,
    /// `None` for a manager started with [`ClientManager::start_without_discovery`],
    /// which has no browser of its own.
    browse_handle: Option<JoinHandle<()>>,
    /// Kept so [`ClientManager::supervise`] can spawn supervisors with the same
    /// identity/timeouts/connection-reason the manager was started with.
    role: ServerRole,
    event_tx: UnboundedSender<ClientEvent>,
}

impl ClientManager {
    /// Start discovering and managing Sendspin clients using `role`'s identity and
    /// connection settings. Returns immediately; events arrive on the returned
    /// receiver as they happen. Drop the returned `ClientManager` to stop discovery
    /// and every reconnect loop it is running.
    ///
    /// `allow` scopes discovery by mDNS instance full name (e.g.
    /// `my-device._sendspin._tcp.local.`). Accepting everything is rarely what you
    /// want on a LAN where other servers already serve some of those clients — you
    /// will compete with them for the devices.
    ///
    /// `daemon` lets an embedder share one mDNS `ServiceDaemon` (see
    /// [`ClientBrowser::with_daemon`]) across all of its mDNS rather than adding a
    /// daemon thread — and, under host networking, its multicast amplification —
    /// per manager. `None` spawns a private one.
    pub fn start(
        role: &ServerRole,
        allow: impl Fn(&str) -> bool + Send + 'static,
        daemon: Option<ServiceDaemon>,
    ) -> Result<(Self, UnboundedReceiver<ClientEvent>), crate::error::Error> {
        // Two clones survive the browse task: the manager keeps them so
        // `supervise` can spawn supervisors with the same identity later.
        let role_for_manager = role.clone();
        let role = role.clone();
        let (event_tx, event_rx) = unbounded_channel();
        let event_tx_for_manager = event_tx.clone();
        let tasks: Arc<Mutex<HashMap<String, ManagedClient>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let browser = match daemon {
            Some(d) => ClientBrowser::with_daemon(d)?,
            None => ClientBrowser::new()?,
        };
        let tasks_for_browse = Arc::clone(&tasks);
        let browse_handle = tokio::spawn(async move {
            while let Some(event) = browser.next_event().await {
                match event {
                    Discovered::Found { fullname, url } => {
                        if !allow(&fullname) {
                            continue;
                        }
                        let mut tasks = tasks_for_browse.lock().unwrap();
                        match tasks.get_mut(&fullname) {
                            // Same device, same address — already supervised.
                            Some(existing) if existing.url == url => {}
                            // Same device at a new address: redirect the running
                            // supervisor (it closes the current connection, emits
                            // Disconnected, and redials). Respawn only if the
                            // supervisor has already exited.
                            Some(existing) => {
                                log::info!(
                                    "[{fullname}] address changed ({} -> {url}), reconnecting",
                                    existing.url
                                );
                                existing.url = url.clone();
                                if existing
                                    .directive_tx
                                    .send(Directive::Dial(url.clone()))
                                    .is_err()
                                {
                                    *existing = spawn_supervisor(
                                        fullname.clone(),
                                        url,
                                        role.clone(),
                                        event_tx.clone(),
                                    );
                                }
                            }
                            None => {
                                let managed = spawn_supervisor(
                                    fullname.clone(),
                                    url,
                                    role.clone(),
                                    event_tx.clone(),
                                );
                                tasks.insert(fullname, managed);
                            }
                        }
                    }
                    // The device's mDNS advertisement went away. This is NOT proof
                    // the device left: WiFi power-saving speakers (e.g. Home Assistant
                    // Voice PE) routinely let their record lapse (TTL expiry) while
                    // still online, then re-announce. Stopping supervision here would let
                    // a brief mDNS flap silence the device permanently — nothing would
                    // redial it. So KEEP the supervisor: its dial loop already retries
                    // with backoff (1s→5min), so it reconnects the moment the device is
                    // reachable again, no dependency on a fresh mDNS announcement. A
                    // genuinely-gone device is removed by the host's own liveness layer
                    // (which restarts this manager without it) — not by an mDNS flap.
                    Discovered::Removed { fullname } => {
                        if tasks_for_browse.lock().unwrap().contains_key(&fullname) {
                            log::info!("[{fullname}] mDNS service removed — keeping supervision (dial loop retries; likely a power-save/TTL flap)");
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                tasks,
                browse_handle: Some(browse_handle),
                role: role_for_manager,
                event_tx: event_tx_for_manager,
            },
            event_rx,
        ))
    }

    /// Start a manager that does **no discovery of its own**: the caller supplies
    /// which clients to keep connected, with [`Self::supervise`].
    ///
    /// Use this when the embedder already browses `_sendspin._tcp` itself, and
    /// especially when it runs **several** managers over one shared
    /// [`ClientBrowser::with_daemon`] daemon — because mdns-sd keeps exactly one
    /// listener per service type ("If there is already a `listener`, it will be
    /// updated, i.e. overwritten"), so every manager that browses steals the
    /// subscription from the one before it. All but the newest then go deaf: they
    /// never see their devices and never dial, silently. One browse in the embedder,
    /// feeding N caller-driven managers, has no such failure mode — and costs one
    /// less multicast querier per manager.
    ///
    /// Everything after the dial is unchanged: the same supervisor loop, the same
    /// `Connected`/`Message`/`Disconnected` events, the same capped-backoff retry.
    pub fn start_without_discovery(role: &ServerRole) -> (Self, UnboundedReceiver<ClientEvent>) {
        let (event_tx, event_rx) = unbounded_channel();
        (
            Self {
                tasks: Arc::new(Mutex::new(HashMap::new())),
                browse_handle: None,
                role: role.clone(),
                event_tx,
            },
            event_rx,
        )
    }

    /// Keep the client at `url` connected, dialing it now and retrying with capped
    /// backoff for as long as this manager lives (or until
    /// [`Self::stop_client`]).
    ///
    /// Idempotent per `fullname`, which is what makes it safe to call on every pass
    /// of an embedder's own reconcile loop: an unchanged URL is a no-op, a changed
    /// one redirects the running supervisor (it closes the current connection, emits
    /// [`ClientEvent::Disconnected`], and redials the new address) exactly as a
    /// re-resolve through the browser would.
    pub fn supervise(&self, fullname: &str, url: &str) {
        let mut tasks = self.tasks();
        match tasks.get_mut(fullname) {
            Some(existing) if existing.url == url => {}
            Some(existing) => {
                log::info!("[{fullname}] address changed ({} -> {url}), reconnecting", existing.url);
                existing.url = url.to_string();
                if existing.directive_tx.send(Directive::Dial(url.to_string())).is_err() {
                    *existing = spawn_supervisor(
                        fullname.to_string(),
                        url.to_string(),
                        self.role.clone(),
                        self.event_tx.clone(),
                    );
                }
            }
            None => {
                let managed = spawn_supervisor(
                    fullname.to_string(),
                    url.to_string(),
                    self.role.clone(),
                    self.event_tx.clone(),
                );
                tasks.insert(fullname.to_string(), managed);
            }
        }
    }

    /// Stop supervising one client and end its reconnect loop, gracefully — a live
    /// connection emits [`ClientEvent::Disconnected`] as it goes.
    ///
    /// This is the counterpart to the manager's deliberate refusal to give up on a
    /// device whose mDNS record merely lapsed: because a missed announcement is not
    /// evidence a device left, deciding it *has* left is the caller's call, and this
    /// is how they say so. Without it an embedder must drop and rebuild the whole
    /// manager — every other device's connection included — to stop supervising one.
    ///
    /// Returns whether that client was being supervised.
    pub fn stop_client(&self, fullname: &str) -> bool {
        match self.tasks().remove(fullname) {
            Some(managed) => {
                log::info!("[{fullname}] supervision stopped by the caller");
                let _ = managed.directive_tx.send(Directive::Stop);
                true
            }
            None => false,
        }
    }

    /// mDNS instance full names of every client currently supervised.
    pub fn supervised(&self) -> Vec<String> {
        self.tasks().keys().cloned().collect()
    }

    fn tasks(&self) -> std::sync::MutexGuard<'_, HashMap<String, ManagedClient>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for ClientManager {
    fn drop(&mut self) {
        if let Some(h) = &self.browse_handle {
            h.abort();
        }
        for (_, managed) in self.tasks.lock().unwrap().drain() {
            managed.handle.abort();
        }
    }
}

fn spawn_supervisor(
    fullname: String,
    url: String,
    role: ServerRole,
    event_tx: UnboundedSender<ClientEvent>,
) -> ManagedClient {
    let (directive_tx, directive_rx) = watch::channel(Directive::Dial(url.clone()));
    let handle = tokio::spawn(supervise(fullname, directive_rx, role, event_tx));
    ManagedClient {
        handle,
        directive_tx,
        url,
    }
}

/// Keep one discovered client connected until told to stop. Dials the current
/// directive URL, reports Connected/Message/Disconnected, and reconnects with
/// capped backoff. A `Directive::Dial` with a new URL closes the current
/// connection and redials it immediately; `Directive::Stop` ends the task.
async fn supervise(
    fullname: String,
    mut directive_rx: watch::Receiver<Directive>,
    role: ServerRole,
    event_tx: UnboundedSender<ClientEvent>,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let url = match directive_rx.borrow_and_update().clone() {
            Directive::Dial(url) => url,
            Directive::Stop => return,
        };

        // Dialing is not selected against `directive_rx` because it is bounded:
        // the handshake has its own deadline, so a silent peer can delay this loop
        // by at most that, not indefinitely.
        match role.dial(&url).await {
            Ok(conn) => {
                // A Stop that arrived during the dial: don't announce a
                // connection we're about to tear down.
                if matches!(*directive_rx.borrow(), Directive::Stop) {
                    return;
                }
                let started = Instant::now();
                let client_id = conn.client_id().to_string();
                log::info!("[{fullname}] connected as {client_id} ({url})");
                let _ = event_tx.send(ClientEvent::Connected {
                    client_id: client_id.clone(),
                    fullname: fullname.clone(),
                    active_roles: conn.active_roles().to_vec(),
                    hello: Box::new(conn.hello().clone()),
                    sender: conn.sender(),
                });

                // Drain until the client disconnects, or a directive redirects
                // us (dropping `conn` here closes that connection).
                let redirected = tokio::select! {
                    _ = drain_until_disconnected(conn, &client_id, &event_tx) => false,
                    _ = directive_rx.changed() => true,
                };

                let _ = event_tx.send(ClientEvent::Disconnected {
                    client_id: client_id.clone(),
                });

                if matches!(*directive_rx.borrow(), Directive::Stop) {
                    return;
                }
                if redirected {
                    log::info!("[{fullname}] address changed, reconnecting immediately");
                    backoff = MIN_BACKOFF;
                    continue;
                }
                log::info!("[{fullname}] disconnected, will retry");
                if started.elapsed() >= STABLE_SESSION {
                    backoff = MIN_BACKOFF;
                }
            }
            Err(e) => {
                log::warn!("[{fullname}] dial to {url} failed: {e}");
            }
        }

        // Wait out the backoff, but wake early if a directive arrives.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = directive_rx.changed() => {}
        }
        if matches!(*directive_rx.borrow(), Directive::Stop) {
            return;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn drain_until_disconnected(
    mut conn: ServerConnection,
    client_id: &str,
    event_tx: &UnboundedSender<ClientEvent>,
) {
    while let Some(message) = conn.recv_message().await {
        if event_tx
            .send(ClientEvent::Message {
                client_id: client_id.to_string(),
                message: Box::new(message),
            })
            .is_err()
        {
            return; // receiver dropped; nothing left to report to
        }
    }
}
