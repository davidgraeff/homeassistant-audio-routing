// ABOUTME: ServerRole — this server's identity and per-connection settings, shared
// ABOUTME: by every way of obtaining a connection: dial out, bind and accept, or
// ABOUTME: supervise discovered clients.

use crate::error::Error;
use crate::protocol::messages::ConnectionReason;
use crate::server::connection::{ServerConnection, DEFAULT_HANDSHAKE_TIMEOUT};
use crate::server::listener::ServerListener;
use crate::server::writer::DEFAULT_WRITE_TIMEOUT;
use crate::sync::raw_clock::{Clock, DefaultClock};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::ToSocketAddrs;

/// Who this server says it is, and how its connections behave.
///
/// The three ways to obtain a connection — [`Self::dial`] out to a client that runs
/// its own server, [`Self::bind`] and accept inbound ones, or [`Self::manage`]
/// discovered clients — all need the same identity and the same per-connection
/// settings, so they take them from one value rather than each growing its own
/// parameter list. That is not only tidier: separate entry points with one
/// parameter each cannot express combinations, and this crate previously had three
/// dial functions covering three of four combinations, leaving no way to give a
/// *supervised* connection a non-default write timeout.
///
/// It also removes a hazard of its own making: `server_id` and `server_name` are
/// adjacent strings whose transposition is silent and, because `server_id` is the
/// identity a client persists to recognise this server across reconnects, lasting.
/// Naming them once, at construction, is one place to get it right.
///
/// ```no_run
/// # use sendspin::server::ServerRole;
/// # use std::time::Duration;
/// # async fn f() -> Result<(), sendspin::error::Error> {
/// let role = ServerRole::new("my-server", "My Server").write_timeout(Duration::from_secs(2));
/// let listener = role.bind(("0.0.0.0", 8927)).await?.path("/sendspin");
/// let dialed = role.dial("ws://192.168.1.42:8928/sendspin").await?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct ServerRole {
    pub(crate) server_id: String,
    pub(crate) server_name: String,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) write_timeout: Duration,
    pub(crate) handshake_timeout: Duration,
    pub(crate) connection_reason: ConnectionReason,
}

impl std::fmt::Debug for ServerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerRole")
            .field("server_id", &self.server_id)
            .field("server_name", &self.server_name)
            .field("write_timeout", &self.write_timeout)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("connection_reason", &self.connection_reason)
            .finish_non_exhaustive()
    }
}

impl ServerRole {
    /// `server_id` should be stable across restarts — it is how a client
    /// recognises "the same server" across reconnects. `server_name` is
    /// human-readable and shown to users.
    pub fn new(server_id: impl Into<String>, server_name: impl Into<String>) -> Self {
        Self {
            server_id: server_id.into(),
            server_name: server_name.into(),
            clock: Arc::new(DefaultClock::default()),
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            connection_reason: ConnectionReason::Playback,
        }
    }

    /// Use a custom clock instead of [`DefaultClock`] — mainly for tests that need
    /// deterministic or synchronized-with-a-peer timestamps. Every connection made
    /// through this role shares it, which is what puts their timestamps in one
    /// domain.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Deadline for a single WebSocket write before a connection is declared dead.
    /// Defaults to [`crate::server::DEFAULT_WRITE_TIMEOUT`].
    pub fn write_timeout(mut self, write_timeout: Duration) -> Self {
        self.write_timeout = write_timeout;
        self
    }

    /// Deadline for a peer to complete the handshake. Defaults to
    /// [`crate::server::DEFAULT_HANDSHAKE_TIMEOUT`].
    pub fn handshake_timeout(mut self, handshake_timeout: Duration) -> Self {
        self.handshake_timeout = handshake_timeout;
        self
    }

    /// What this server tells clients it connected *for*.
    ///
    /// The spec lets several servers connect to one client and leaves the
    /// keep-or-switch decision to the client, which weighs each server's reason.
    /// [`ConnectionReason::Playback`] (the default) is right for a connection made
    /// in order to stream. Pass [`ConnectionReason::Discovery`] for one held open
    /// merely to *be ready* — so an announcement or a control command can reach an
    /// otherwise-idle device without paying a cold connect — because a server that
    /// claims Playback while streaming nothing looks like the active one and can
    /// stop the device switching to the server the user actually asked to play.
    ///
    /// Only meaningful for [`Self::dial`] and [`Self::manage`]: an inbound
    /// connection was the client's idea, so accepting one always announces
    /// Discovery.
    pub fn connection_reason(mut self, connection_reason: ConnectionReason) -> Self {
        self.connection_reason = connection_reason;
        self
    }

    /// Dial a client's own WebSocket server (e.g. a URL discovered via
    /// [`crate::server::ClientBrowser`]) and drive the server-role handshake over
    /// the resulting connection.
    ///
    /// The protocol-level roles are identical regardless of which side opened the
    /// TCP connection — the client still sends `client/hello` first, this still
    /// replies `server/hello` — so this is [`crate::server::ServerListener::accept`]'s
    /// handshake, dialed rather than accepted.
    pub async fn dial(&self, url: &str) -> Result<ServerConnection, Error> {
        crate::server::dial::dial(self, url).await
    }

    /// Bind a listener for inbound connections. Chain
    /// [`ServerListener::path`] to restrict the accepted HTTP path.
    pub async fn bind(&self, addr: impl ToSocketAddrs) -> Result<ServerListener, Error> {
        ServerListener::bind_with_role(self.clone(), addr).await
    }

    /// Start a [`crate::server::ClientManager`]: discover clients that only run
    /// their own embedded server, dial them, and keep them connected. See
    /// [`crate::server::ClientManager::start`] for the parameters.
    pub fn manage(
        &self,
        allow: impl Fn(&str) -> bool + Send + 'static,
        daemon: Option<crate::server::mdns_sd::ServiceDaemon>,
    ) -> Result<
        (
            crate::server::ClientManager,
            tokio::sync::mpsc::UnboundedReceiver<crate::server::ClientEvent>,
        ),
        Error,
    > {
        crate::server::ClientManager::start(self, allow, daemon)
    }
}
