// ABOUTME: Server-initiated connections to Sendspin clients that only run
// ABOUTME: their own embedded WebSocket server (never dialing out
// ABOUTME: themselves) — discovered via discovery::ClientBrowser, dialed here.

use crate::error::Error;
use crate::server::connection::ServerConnection;
use crate::server::listener::transport_config;
use crate::server::role::ServerRole;
use tokio_tungstenite::connect_async_with_config;

/// Dial `url` and drive the server-role handshake over the connection.
///
/// Reached through [`ServerRole::dial`], which is where the identity, the
/// deadlines and the announced [`crate::protocol::messages::ConnectionReason`]
/// come from.
pub(crate) async fn dial(role: &ServerRole, url: &str) -> Result<ServerConnection, Error> {
    let (ws, _response) = connect_async_with_config(url, Some(transport_config()), false)
        .await
        .map_err(|e| Error::Connection(format!("dial to {url} failed: {e}")))?;
    ServerConnection::drive(
        ws,
        &role.server_id,
        &role.server_name,
        role.connection_reason.clone(),
        std::sync::Arc::clone(&role.clock),
        role.write_timeout,
        role.handshake_timeout,
    )
    .await
}
