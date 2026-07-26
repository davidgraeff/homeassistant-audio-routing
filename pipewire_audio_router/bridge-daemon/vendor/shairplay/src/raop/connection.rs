//! Per-connection state and RTSP request handling.

use super::MAX_NONCE_LEN;
use super::handlers_ap1 as handlers;
use super::rtsp;
use super::types::*;
use crate::crypto::fairplay::FairPlay;
use crate::crypto::pairing::Pairing;
use crate::crypto::rsa::RsaKey;
use crate::net::server::{ConnectionHandler, HttpdCallbacks};
use crate::proto::digest;
use crate::proto::http::{HttpRequest, HttpResponse};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;

/// Shared state passed to each connection.
pub(crate) struct RaopShared {
    pub(crate) rsakey: Arc<RsaKey>,
    pub(crate) pairing: Arc<Pairing>,
    pub(crate) hwaddr: Vec<u8>,
    pub(crate) password: String,
    pub(crate) handler: Arc<dyn AudioHandler>,
    /// The client that currently holds the audio session, if any — passed to
    /// `AudioHandler::authorize_session` as the incumbent so an embedder can
    /// enforce single-session / anti-takeover / priority policy. Claimed at RTSP
    /// SETUP, released when the owning connection drops.
    pub(crate) session_owner: std::sync::Mutex<Option<SessionInfo>>,
    /// Live connections keyed by peer IP, each with a one-shot "please close"
    /// signal. Lets `disconnect_client` (embedder-driven force-disconnect, and
    /// takeover) tear down a specific session; the connection loop selects on it.
    pub(crate) connections: std::sync::Mutex<HashMap<String, watch::Sender<bool>>>,
    #[cfg(feature = "ap2")]
    pub(crate) pairing_store: Arc<dyn PairingStore>,
    /// Accessory's long-term Ed25519 identity seed (random, persisted via the store).
    #[cfg(feature = "ap2")]
    pub(crate) identity_seed: [u8; 32],
    pub(crate) output_sample_rate: Option<u32>,
    /// Only consulted by the AP2 mixdown path; dead in AP1-only builds.
    #[cfg_attr(not(feature = "ap2"), allow(dead_code))]
    pub(crate) output_max_channels: Option<u8>,
    #[cfg(feature = "ap2")]
    pub(crate) pin: Option<String>,
    #[cfg(feature = "video")]
    pub(crate) video_handler: Option<Arc<dyn crate::raop::video::VideoHandler>>,
    /// Shared video encryption keys — set by audio SETUP, read by video SETUP.
    #[cfg(feature = "video")]
    pub(crate) video_ekey: Arc<std::sync::RwLock<Option<[u8; 16]>>>,
    #[cfg(feature = "video")]
    pub(crate) video_eiv: Arc<std::sync::RwLock<Option<[u8; 16]>>>,
    #[cfg(feature = "ap2")]
    pub(crate) pairing_id: String,
    /// Accessory device id (`hwaddr_airplay(hwaddr)`), computed once at build.
    #[cfg(feature = "ap2")]
    pub(crate) device_id: String,
    #[cfg(feature = "ap2")]
    pub(crate) airplay_name: String,
    /// Stop-handle for the currently-active audio session. iOS opens parallel
    /// connections (Happy Eyeballs) and switches between them; registering each
    /// new session here — and stopping the previous — keeps only the newest
    /// playout feeding the output (avoids overlapping / post-disconnect audio).
    #[cfg(feature = "ap2")]
    pub(crate) active_audio: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(feature = "hls")]
    pub(crate) hls_handler: Option<Arc<dyn crate::raop::hls::HlsHandler>>,
}

#[cfg(feature = "ap2")]
impl RaopShared {
    /// Register a newly-started audio session, stopping the previous one so only
    /// the latest connection's playout feeds the audio output.
    pub(crate) fn set_active_audio(&self, stop: Box<dyn FnOnce() + Send>) {
        let prev = self.active_audio.lock().ok().and_then(|mut g| g.replace(stop));
        if let Some(prev) = prev {
            prev();
        }
    }
}

impl RaopShared {
    /// The current session owner if it isn't `me` (an IP) — i.e. the incumbent a
    /// newly-arriving `me` would be competing with. `None` when the session is
    /// free or already held by `me`.
    pub(crate) fn current_session_excluding(&self, me: &str) -> Option<SessionInfo> {
        self.session_owner.lock().ok().and_then(|g| g.clone()).filter(|o| o.addr != me)
    }

    /// Record `me` as the current session owner, replacing any previous owner.
    /// Callers gate this on `authorize_session` first.
    pub(crate) fn claim_session(&self, addr: &str, name: Option<&str>) {
        if let Ok(mut g) = self.session_owner.lock() {
            *g = Some(SessionInfo { addr: addr.to_string(), name: name.map(str::to_string) });
        }
    }

    /// Release the session iff `me` currently owns it (so a later owner that
    /// took over isn't cleared by the earlier connection's teardown).
    pub(crate) fn release_session(&self, me: &str) {
        if let Ok(mut g) = self.session_owner.lock() {
            if g.as_ref().is_some_and(|o| o.addr == me) {
                *g = None;
            }
        }
    }

    /// Register a live connection (by peer IP) and return the receiver its loop
    /// should select on; a `true` value means "close now".
    pub(crate) fn register_connection(&self, addr: &str) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        if let Ok(mut m) = self.connections.lock() {
            m.insert(addr.to_string(), tx);
        }
        rx
    }

    /// Deregister a connection (on teardown).
    pub(crate) fn deregister_connection(&self, addr: &str) {
        if let Ok(mut m) = self.connections.lock() {
            m.remove(addr);
        }
    }

    /// Ask the connection from `addr` to close. Used for embedder-driven
    /// force-disconnect and for takeover. No-op if there's no such connection.
    pub(crate) fn disconnect_client(&self, addr: &str) {
        if let Ok(m) = self.connections.lock() {
            if let Some(tx) = m.get(addr) {
                let _ = tx.send(true);
            }
        }
    }

    /// Ask every live connection to close gracefully. Used on server shutdown
    /// so each sender gets a clean FIN on its RTSP control socket — the only
    /// "session is gone" signal a RAOP receiver can give (it can't originate an
    /// RTSP TEARDOWN). Without this the sockets close only implicitly at process
    /// exit, which races a fast restart that rebinds the port first and leaves
    /// the sender streaming RTP into a session the new process never set up.
    pub(crate) fn disconnect_all(&self) {
        if let Ok(m) = self.connections.lock() {
            for tx in m.values() {
                let _ = tx.send(true);
            }
        }
    }
}

impl HttpdCallbacks for RaopShared {
    fn conn_init(self: Arc<Self>, local: SocketAddr, remote: SocketAddr) -> Option<Box<dyn ConnectionHandler>> {
        let local_bytes = match local.ip() {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let remote_bytes = match remote.ip() {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        };

        let conn = handlers::RaopConnection {
            raop_rtp: None,
            fairplay: FairPlay::new(),
            pairing: self.pairing.create_session(),
            local_addr: local_bytes,
            remote_addr: remote_bytes,
            remote_socket: remote,
            nonce: digest::generate_nonce(MAX_NONCE_LEN),
            #[cfg(feature = "ap2")]
            srp_server: None,
            #[cfg(feature = "ap2")]
            pair_verify: None,
            #[cfg(feature = "ap2")]
            ap2_shared_secret: None,
            #[cfg(feature = "ap2")]
            pair_verify_secret: None,
            #[cfg(feature = "ap2")]
            is_ap2: false,
            #[cfg(feature = "ap2")]
            playout_cmd: None,
            #[cfg(feature = "ap2")]
            event_sender: None,
            #[cfg(feature = "video")]
            ekey: None,
            #[cfg(feature = "video")]
            eiv: None,
            #[cfg(feature = "hls")]
            hls_state: crate::raop::hls::HlsState::new(),
            shared: self.clone(),
        };
        let remote_str = remote.ip().to_string();
        let cancel_rx = self.register_connection(&remote_str);
        conn.shared.handler.on_client_connected(&remote_str);
        Some(Box::new(RaopConnectionHandler {
            conn,
            remote_addr: remote_str,
            cancel_rx: Some(cancel_rx),
            connected_at: std::time::Instant::now(),
            #[cfg(feature = "ap2")]
            cipher: None,
            #[cfg(feature = "ap2")]
            pending_secret: None,
        }))
    }
}

struct RaopConnectionHandler {
    conn: handlers::RaopConnection,
    remote_addr: String,
    /// Taken once by the connection loop (`ConnectionHandler::cancellation`); a
    /// `true` value means an external caller asked to close this connection.
    cancel_rx: Option<watch::Receiver<bool>>,
    /// Connection-start instant, used to log per-request elapsed time for
    /// connect-latency diagnostics (AP2 PTP-sync wait vs AP1 fast path).
    connected_at: std::time::Instant,
    #[cfg(feature = "ap2")]
    cipher: Option<crate::crypto::chacha_transport::EncryptedChannel>,
    #[cfg(feature = "ap2")]
    pending_secret: Option<Vec<u8>>,
}

impl Drop for RaopConnectionHandler {
    fn drop(&mut self) {
        // Free the session slot if we held it (so the next sender isn't seen as
        // arriving while "busy") and drop our entry from the connection table.
        self.conn.shared.release_session(&self.remote_addr);
        self.conn.shared.deregister_connection(&self.remote_addr);
        self.conn.shared.handler.on_client_disconnected(&self.remote_addr);
    }
}

impl ConnectionHandler for RaopConnectionHandler {
    fn cancellation(&mut self) -> Option<watch::Receiver<bool>> {
        self.cancel_rx.take()
    }

    fn conn_request(&mut self, request: &HttpRequest) -> HttpResponse {
        // Connect-latency timeline: one line per RTSP request, elapsed since the
        // connection opened. `/feedback` is a ~2s keep-alive heartbeat, so it drops
        // to `debug` to keep the connect sequence readable; everything else at info.
        let elapsed_ms = self.connected_at.elapsed().as_millis() as u64;
        let method = request.method().unwrap_or("");
        let url = request.url().unwrap_or("");
        if url == "/feedback" {
            tracing::debug!(elapsed_ms, method, url, "RTSP request");
        } else {
            tracing::info!(elapsed_ms, method, url, "RTSP request");
        }
        let resp = rtsp::dispatch(&mut self.conn, request);

        // Queue encryption activation for AFTER this response is sent
        #[cfg(feature = "ap2")]
        if self.cipher.is_none()
            && let Some(secret) = &self.conn.ap2_shared_secret
        {
            self.pending_secret = Some(secret.clone());
        }

        resp
    }

    fn is_encrypted(&self) -> bool {
        #[cfg(feature = "ap2")]
        {
            self.cipher.is_some()
        }
        #[cfg(not(feature = "ap2"))]
        {
            false
        }
    }

    fn after_response(&mut self) {
        #[cfg(feature = "ap2")]
        if self.cipher.is_none()
            && let Some(secret) = self.pending_secret.take()
        {
            tracing::debug!(secret_len = secret.len(), "Activating cipher from pending_secret");
            match crate::crypto::chacha_transport::EncryptedChannel::control(&secret) {
                Ok(ch) => {
                    tracing::info!("Encrypted RTSP transport activated");
                    self.cipher = Some(ch);
                }
                Err(e) => tracing::warn!("Failed to create cipher: {e}"),
            }
        }
    }

    fn decrypt_incoming(&mut self, data: &[u8]) -> Option<(Vec<u8>, usize)> {
        #[cfg(feature = "ap2")]
        if let Some(ch) = &mut self.cipher {
            return ch.decrypt_ctx.decrypt(data).ok();
        }
        Some((data.to_vec(), data.len()))
    }

    fn encrypt_outgoing(&mut self, data: &[u8]) -> Vec<u8> {
        #[cfg(feature = "ap2")]
        if let Some(ch) = &mut self.cipher {
            // Once the channel is encrypted the peer expects ciphertext; never
            // fall back to emitting plaintext (which would leak the response and
            // desync the stream). On the practically-impossible AEAD encrypt
            // failure, return no bytes so the connection tears down instead.
            return ch.encrypt_ctx.encrypt(data).unwrap_or_else(|e| {
                tracing::warn!("Outgoing encryption failed; dropping response: {e}");
                Vec::new()
            });
        }
        data.to_vec()
    }
}

// On drop, RTP session is cleaned up automatically (RaopRtp dropped → shutdown sent)
