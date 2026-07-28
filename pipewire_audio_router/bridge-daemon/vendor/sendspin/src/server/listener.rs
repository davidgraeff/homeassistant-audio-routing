// ABOUTME: Inbound WebSocket acceptor that drives the Sendspin protocol-server
// ABOUTME: state machine (client/hello -> server/hello) on every peer that connects.

use crate::error::Error;
use crate::protocol::messages::ConnectionReason;
use crate::server::connection::ServerConnection;
use crate::server::role::ServerRole;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{lookup_host, TcpListener, TcpSocket, TcpStream, ToSocketAddrs};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{accept_async_with_config, accept_hdr_async_with_config, WebSocketStream};

/// WebSocket transport limits for a server-role connection.
///
/// `tungstenite`'s defaults are sized for a general-purpose WebSocket endpoint: a
/// 128 KiB read buffer allocated up front and a 64 MiB maximum message size. A
/// Sendspin peer only ever sends small JSON control frames — the protocol has no
/// client-to-server binary frames at all, and this crate discards any it receives
/// — so those defaults cost ~128 KiB of idle memory per connection and let a peer
/// make the server accumulate up to 64 MiB of fragments for a message that is then
/// thrown away.
pub(crate) fn transport_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(8 * 1024)
        .max_message_size(Some(64 * 1024))
        .max_frame_size(Some(64 * 1024))
}

/// Accept inbound WebSocket peers and drive each one through the
/// protocol-**server** state machine: read `client/hello`, reply
/// `server/hello`, then hand back a [`ServerConnection`] for pushing
/// stream/audio messages and receiving state/command/goodbye.
///
/// This is the counterpart to [`crate::protocol::listener::ProtocolListener`],
/// which accepts inbound connections but drives the protocol-**client** role
/// on them (used when a server dials out to a client that runs its own tiny
/// WS listener — a reversed-topology case). `ServerListener` is what a
/// Sendspin server itself binds to accept the usual inbound player
/// connections.
///
/// [`Self::accept`] drives the full handshake before returning, so it serves
/// one inbound connection at a time — a slow handshake blocks the next
/// `accept()`. `tokio::spawn` a task per accepted connection if you need to
/// keep accepting while driving existing ones.
pub struct ServerListener {
    tcp: TcpListener,
    role: ServerRole,
    path: Option<String>,
}

impl std::fmt::Debug for ServerListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerListener")
            .field("local_addr", &self.tcp.local_addr().ok())
            .field("role", &self.role)
            .field("path", &self.path)
            .finish()
    }
}

impl ServerListener {
    /// Bind a listener for `role`. Reached through [`ServerRole::bind`].
    pub(crate) async fn bind_with_role(
        role: ServerRole,
        addr: impl ToSocketAddrs,
    ) -> Result<Self, Error> {
        // Bind with SO_REUSEADDR so a port freed by a just-torn-down server can
        // be reused immediately — otherwise recreating a group on the same port
        // can race the old socket's close and fail with EADDRINUSE.
        let sockaddr: SocketAddr = lookup_host(addr)
            .await
            .map_err(|e| Error::Connection(format!("resolve failed: {e}")))?
            .next()
            .ok_or_else(|| Error::Connection("no address to bind".to_string()))?;
        let socket = if sockaddr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .map_err(|e| Error::Connection(format!("socket create failed: {e}")))?;
        socket
            .set_reuseaddr(true)
            .map_err(|e| Error::Connection(format!("set_reuseaddr failed: {e}")))?;
        socket
            .bind(sockaddr)
            .map_err(|e| Error::Connection(format!("bind failed: {e}")))?;
        let tcp = socket
            .listen(128)
            .map_err(|e| Error::Connection(format!("listen failed: {e}")))?;
        Ok(Self {
            tcp,
            role,
            path: None,
        })
    }

    /// The identity and connection settings every peer accepted here is driven
    /// with.
    pub fn role(&self) -> &ServerRole {
        &self.role
    }

    /// Restrict accepted connections to a specific HTTP path (the Sendspin
    /// spec fixes this to `/sendspin` for real deployments). Mismatches are
    /// rejected with HTTP 404 during the WebSocket handshake; the listener
    /// stays bound. Defaults to accepting any path.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.path = Some(if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        });
        self
    }

    /// Accept the next inbound connection, returning the driven
    /// [`ServerConnection`] and the peer's address.
    ///
    /// Per-peer failures surface as [`Error`] without affecting the
    /// listener; callers typically call `accept()` in a loop.
    ///
    /// Not cancel-safe: dropping the returned future mid-handshake tears down
    /// that connection. A peer that connects and then stalls the handshake fails
    /// after [`ServerRole::handshake_timeout`] rather than blocking this future — which
    /// matters because the handshake is driven inline, so an unbounded one would
    /// hold up every subsequent inbound connection.
    pub async fn accept(&self) -> Result<(ServerConnection, SocketAddr), Error> {
        let (tcp_stream, peer_addr) = self
            .tcp
            .accept()
            .await
            .map_err(|e| Error::Connection(format!("TCP accept failed: {e}")))?;
        log::debug!("Accepted TCP connection from {}", peer_addr);

        match self.handshake_and_drive(tcp_stream).await {
            Ok(conn) => Ok((conn, peer_addr)),
            Err(e) => {
                log::warn!("Inbound handshake from {peer_addr} failed: {e}");
                Err(e)
            }
        }
    }

    async fn handshake_and_drive(&self, tcp_stream: TcpStream) -> Result<ServerConnection, Error> {
        let ws = self.handshake_ws(tcp_stream).await?;
        // Inbound: the client initiated the connection, so the server is
        // simply present/available — announce Discovery rather than Playback.
        ServerConnection::drive(
            ws,
            &self.role.server_id,
            &self.role.server_name,
            ConnectionReason::Discovery,
            Arc::clone(&self.role.clock),
            self.role.write_timeout,
            self.role.handshake_timeout,
        )
        .await
    }

    // `ErrorResponse` is large by Clippy's standard but mandated by
    // tungstenite's `Callback` trait — same tradeoff ProtocolListener makes.
    #[allow(clippy::result_large_err)]
    async fn handshake_ws<S>(&self, stream: S) -> Result<WebSocketStream<S>, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        match &self.path {
            Some(expected_path) => {
                let expected = expected_path.clone();
                let callback = move |request: &Request, response: Response| {
                    if request.uri().path() == expected {
                        Ok(response)
                    } else {
                        log::debug!(
                            "Rejecting inbound connection: path {:?} != expected {:?}",
                            request.uri().path(),
                            expected
                        );
                        Err(http::Response::builder()
                            .status(http::StatusCode::NOT_FOUND)
                            .body(None)
                            .expect("static 404 response is well-formed"))
                            as Result<Response, ErrorResponse>
                    }
                };
                accept_hdr_async_with_config(stream, callback, Some(transport_config()))
                    .await
                    .map_err(|e| Error::WebSocket(format!("WebSocket handshake failed: {e}")))
            }
            None => accept_async_with_config(stream, Some(transport_config()))
                .await
                .map_err(|e| Error::WebSocket(format!("WebSocket handshake failed: {e}"))),
        }
    }

    /// Local bound address.
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        self.tcp
            .local_addr()
            .map_err(|e| Error::Connection(format!("local_addr failed: {e}")))
    }
}
