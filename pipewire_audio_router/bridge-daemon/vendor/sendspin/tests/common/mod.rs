// ABOUTME: Shared fixtures for the server-role integration tests: bare peers,
// ABOUTME: stream configs, frame draining, and retry wrappers for the tests that
// ABOUTME: genuinely depend on real mDNS multicast timing.
//
// Each test binary compiles its own copy of this module and none uses all of it,
// so the whole module is allowed to have unused items rather than annotating each.
#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use sendspin::protocol::client::AudioChunk;
use sendspin::protocol::messages::{
    ClientHello, Message, PlayerCommand, PlayerCommandType, StreamPlayerConfig,
};
use sendspin::server::{ServerConnection, ServerListener, ServerRole};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

/// The read half of a bare test peer.
pub type PeerRead = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

/// A `client/hello` declaring player@v1 support and nothing else.
pub fn test_hello(client_id: &str) -> ClientHello {
    ClientHello {
        client_id: client_id.to_string(),
        name: "Test Player".to_string(),
        version: 1,
        supported_roles: vec!["player@v1".to_string()],
        device_info: None,
        player_v1_support: None,
        visualizer_v1_support: None,
        artwork_v1_support: None,
    }
}

/// 48 kHz stereo S16 PCM.
pub fn pcm_config() -> StreamPlayerConfig {
    pcm_config_at(48_000)
}

/// Stereo S16 PCM at `sample_rate` — for asserting a format change is observed.
pub fn pcm_config_at(sample_rate: u32) -> StreamPlayerConfig {
    StreamPlayerConfig {
        codec: "pcm".to_string(),
        sample_rate,
        channels: 2,
        bit_depth: 16,
        codec_header: None,
    }
}

/// A `Volume` player command.
pub fn volume(v: u8) -> PlayerCommand {
    PlayerCommand {
        command: PlayerCommandType::Volume,
        volume: Some(v),
        mute: None,
        static_delay_ms: None,
    }
}

/// Bind a listener on an ephemeral port, returning it and its `ws://` URL.
pub async fn bind_test_listener() -> (ServerListener, String) {
    bind_test_listener_with(ServerRole::new("test-server", "Test Server")).await
}

/// [`bind_test_listener`] with connection settings of the test's choosing — a short
/// write or handshake deadline, say, so a test does not have to wait out the
/// production defaults.
pub async fn bind_test_listener_with(role: ServerRole) -> (ServerListener, String) {
    let listener = role.bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("local_addr"));
    (listener, url)
}

/// Connect a bare peer that plays the client role manually: sends `client/hello`,
/// discards `server/hello`, then hands back its read half and reads nothing
/// further until the test asks. That last part is what lets a test control exactly
/// how much the server's socket can drain.
pub async fn connect_peer(url: &str, client_id: &str) -> PeerRead {
    let (ws, _) = connect_async(url.to_string()).await.expect("ws connect");
    let (mut write, mut read) = ws.split();
    let hello = serde_json::to_string(&Message::ClientHello(test_hello(client_id))).unwrap();
    write.send(WsMessage::Text(hello.into())).await.unwrap();
    read.next().await.expect("no server/hello").unwrap();
    read
}

/// Connect a peer and accept it, returning both ends.
pub async fn accept_peer(
    listener: &ServerListener,
    url: &str,
    client_id: &str,
) -> (ServerConnection, PeerRead) {
    let peer = tokio::spawn({
        let url = url.to_string();
        let id = client_id.to_string();
        async move { connect_peer(&url, &id).await }
    });
    let (conn, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept timed out")
        .expect("accept failed");
    (conn, peer.await.expect("peer task"))
}

/// What a client actually observed, reduced to the part the ordering tests care
/// about: the order of lifecycle frames and audio payloads.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// `stream/start`, carrying the sample rate so a format change is visible.
    Start(u32),
    End,
    Clear,
    /// One audio frame, identified by its (uniform) first payload byte.
    Audio(u8),
}

/// Drain everything a peer has been sent, until the close frame.
pub async fn drain(read: &mut PeerRead) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(msg))) = timeout(Duration::from_secs(5), read.next()).await {
        match msg {
            WsMessage::Text(text) => match serde_json::from_str::<Message>(&text).unwrap() {
                Message::StreamStart(s) => {
                    out.push(Frame::Start(s.player.expect("player config").sample_rate))
                }
                Message::StreamEnd(_) => out.push(Frame::End),
                Message::StreamClear(_) => out.push(Frame::Clear),
                _ => {}
            },
            WsMessage::Binary(bytes) => {
                let chunk = AudioChunk::from_bytes(&bytes).unwrap();
                out.push(Frame::Audio(chunk.data[0]));
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    out
}

/// Whether real-network tests (which use live mDNS multicast) are enabled, via the
/// `SENDSPIN_NET_TESTS` environment variable. Off by default so an ordinary
/// `cargo test` run doesn't depend on multicast being available.
pub fn net_tests_enabled() -> bool {
    std::env::var_os("SENDSPIN_NET_TESTS").is_some()
}

/// Re-run an async test body up to `attempts` times, succeeding as soon as one
/// attempt doesn't panic and only failing (re-raising the last attempt's panic) if
/// every attempt does.
///
/// Intended for tests that depend on real mDNS multicast timing, where occasional
/// packet loss or scheduling delay on a shared LAN is environmental noise rather
/// than a real failure. A test that fails the same way every time still fails after
/// `attempts` tries.
///
/// `test_fn` must be a zero-argument async function so each retry gets a fresh
/// attempt; `tokio::spawn` isolates a panicking attempt and yields a `JoinError`
/// to detect it.
pub async fn retry_flaky<F, Fut>(attempts: u32, test_fn: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    for attempt in 1..=attempts {
        match tokio::spawn(test_fn()).await {
            Ok(()) => return,
            Err(join_err) if attempt < attempts => {
                eprintln!(
                    "flaky test attempt {attempt}/{attempts} failed ({join_err}), retrying..."
                );
            }
            Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
        }
    }
}

/// Synchronous counterpart to [`retry_flaky`], for plain `#[test]` functions
/// that don't need a tokio runtime (e.g. tests using `mdns_sd`'s blocking
/// `recv_timeout` directly rather than `ClientBrowser`'s async API).
pub fn retry_flaky_sync<F>(attempts: u32, test_fn: F)
where
    F: Fn(),
{
    for attempt in 1..=attempts {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&test_fn)) {
            Ok(()) => return,
            Err(payload) if attempt < attempts => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                eprintln!("flaky test attempt {attempt}/{attempts} failed ({msg}), retrying...");
            }
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}
