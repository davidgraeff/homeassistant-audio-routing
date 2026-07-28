// ABOUTME: Per-client connection actor for the server role: drives the
// ABOUTME: server-side handshake, time-sync echo, and message dispatch.

use crate::error::Error;
use crate::protocol::messages::{
    ClientHello, ConnectionReason, Message, PlayerCommand, ServerCommand, ServerHello, StreamClear,
    StreamEnd, StreamPlayerConfig, StreamStart,
};
use crate::server::binary::{encode_audio_frame, AudioFrame};
use crate::server::writer::{
    write_frame, writer_task, AudioCommand, AudioOrdering, ControlCommand, TimeRequest,
    MAX_QUEUED_AUDIO_FRAMES, MIN_TIME_REPLY_INTERVAL_US,
};
use crate::sync::raw_clock::Clock;
use futures_util::{
    stream::{SplitSink, SplitStream},
    StreamExt,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio_tungstenite::{tungstenite::Message as WsMessage, WebSocketStream};

/// The only role this server negotiates in v1. See the crate-level server
/// docs for the list of roles deferred for a later contribution
/// (color/visualizer/artwork/controller/metadata).
const PLAYER_ROLE: &str = "player@v1";

/// Outcome of a non-blocking [`ServerSender::queue_audio`].
///
/// Three states rather than `Result<_, Error>` because all three are ordinary
/// outcomes the caller must distinguish, and none is *its* failure: a dead member
/// is not the pusher's error to propagate, and the error it would carry has one
/// producer and no detail.
#[must_use = "the caller must handle a dropped frame and a dead connection"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEnqueue {
    /// Queued for the writer task. Not yet on the wire — see
    /// [`ServerSender::send_audio_chunk`] if you need to know that.
    Queued,
    /// The connection's audio backlog was at capacity, so this frame was dropped.
    /// The newest frame is the one discarded; the connection is healthy.
    Dropped,
    /// The writer task is gone. Stop pushing to this member and prune it.
    Disconnected,
}

/// The error every `ServerSender` path reports once its writer task is gone. It
/// carries no detail because there is none to carry: the connection is over, and
/// the caller's only useful response is to stop using it.
pub(super) fn connection_closed() -> Error {
    Error::WebSocket("connection closed".to_string())
}

/// Default deadline for the inbound handshake — `client/hello` must arrive, and
/// `server/hello` must be written, within this.
///
/// A peer that completes the WebSocket handshake and then goes silent (or stops
/// reading) would otherwise park the task driving it forever. That matters more
/// than it sounds: [`crate::server::ServerListener::accept`] drives the handshake
/// inline, so one such peer blocks every subsequent inbound connection, and on the
/// dial side it parks a [`crate::server::ClientManager`] supervisor with no
/// backoff progression and no way to redirect it.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A control frame that has been *placed in* a connection's write queue, but
/// not yet written.
///
/// Queueing is synchronous. That's the point: a caller holding a lock can fix
/// the order in which the client will observe frames — `stream/start`, audio,
/// `stream/end` — and only then release the lock and await the writes. It's
/// what lets [`crate::server::Group`] serialize lifecycle transitions against
/// concurrent audio pushes without ever awaiting while holding its member lock.
#[must_use = "a queued control frame should be awaited (via `written`) so write failures are noticed"]
pub struct QueuedControl {
    /// `Err` when the frame could not even be queued (serialization failed, or
    /// the writer task is gone), so `written()` can report it uniformly.
    result: Result<tokio::sync::oneshot::Receiver<Result<(), Error>>, Error>,
}

impl std::future::IntoFuture for QueuedControl {
    type Output = Result<(), Error>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Self::Output> + Send>>;

    /// So a caller that just wants the frame written can `.await` the queue call
    /// directly — `sender.queue_player_command(cmd).await?` — while one that needs
    /// to fix frame order under a lock still queues first and awaits later.
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.written())
    }
}

impl QueuedControl {
    /// Wait for this frame to reach the socket.
    ///
    /// Bounded by the connection's write timeout (see [`crate::server::DEFAULT_WRITE_TIMEOUT`])
    /// plus whatever control frames were already queued ahead of it. A
    /// `stream/end` additionally waits for the audio pushed before it, but that
    /// flush shares one write-timeout budget with the frame itself, so the total
    /// stays within two write timeouts regardless of backlog depth.
    pub async fn written(self) -> Result<(), Error> {
        match self.result {
            Ok(ack) => ack.await.map_err(|_| connection_closed())?,
            Err(e) => Err(e),
        }
    }
}

/// Sender half of a server-role connection. Cheap to clone; all clones share
/// the same underlying connection, audio backlog counter, and frame ordering.
#[derive(Debug, Clone)]
pub struct ServerSender {
    ctrl_tx: UnboundedSender<ControlCommand>,
    audio_tx: UnboundedSender<AudioCommand>,
    audio_queued: Arc<AtomicUsize>,
    audio_seq: Arc<AtomicU64>,
}

impl ServerSender {
    /// Queue one framed audio chunk without waiting for it to reach the wire — a
    /// group broadcast calls this on every member, so it must never block on any
    /// one member's socket. Cloning an [`AudioFrame`] is a refcount bump, so fanning
    /// one chunk out to N members is N cheap clones, not N copies.
    ///
    /// See [`AudioEnqueue`] for the three outcomes.
    pub fn queue_audio(&self, frame: AudioFrame) -> AudioEnqueue {
        // Liveness is checked before the backlog, and must stay that way. The
        // counter is only decremented by the writer, so frames still queued when
        // the writer exits are never accounted for — leaving the counter at or
        // above the cap on a connection that is already dead. Checking the backlog
        // first would then report `Evicted` forever and the caller would never
        // learn to prune the member.
        if self.audio_tx.is_closed() {
            return AudioEnqueue::Disconnected;
        }
        if self.audio_queued.load(Ordering::Relaxed) >= MAX_QUEUED_AUDIO_FRAMES {
            return AudioEnqueue::Dropped;
        }
        self.audio_queued.fetch_add(1, Ordering::Relaxed);
        let cmd = AudioCommand {
            seq: self.next_audio_seq(),
            frame: frame.0,
            ack: None,
        };
        match self.audio_tx.send(cmd) {
            Ok(()) => AudioEnqueue::Queued,
            Err(_) => {
                self.audio_queued.fetch_sub(1, Ordering::Relaxed);
                AudioEnqueue::Disconnected
            }
        }
    }

    /// Claim the next audio sequence number.
    ///
    /// Note what this does *not* buy: no memory ordering on this counter can order
    /// the claim against the separate `audio_tx.send` that follows it, so a frame
    /// can be numbered before a control frame computes its marker and still reach
    /// the channel after. What makes the marker exact is the caller holding one
    /// lock across claim *and* send — which is precisely what
    /// [`crate::server::Group`] does. Concurrent pushers on one `ServerSender`
    /// without that lock get best-effort ordering.
    fn next_audio_seq(&self) -> u64 {
        self.audio_seq.fetch_add(1, Ordering::AcqRel)
    }

    /// Queue one control frame with the given relationship to the audio queued
    /// ahead of it (see [`AudioOrdering`]). `mark` builds that relationship from
    /// the audio sequence number this frame is queued at.
    fn queue_control(
        &self,
        msg: Message,
        mark: impl FnOnce(u64) -> AudioOrdering,
    ) -> QueuedControl {
        let json = match serde_json::to_string(&msg) {
            Ok(json) => json,
            Err(e) => {
                return QueuedControl {
                    result: Err(Error::Protocol(e.to_string())),
                }
            }
        };
        // Deliberately no logging here: `Group` calls this while holding the lock
        // that orders frames, and formatting a log record — let alone a subscriber
        // blocking on a full pipe — would extend that critical section without
        // bound. The writer logs what actually went out instead.
        let ordering = mark(self.audio_seq.load(Ordering::Acquire));
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let cmd = ControlCommand::Send {
            msg: WsMessage::Text(json.into()),
            ordering,
            ack: ack_tx,
        };
        QueuedControl {
            result: match self.ctrl_tx.send(cmd) {
                Ok(()) => Ok(ack_rx),
                Err(_) => Err(connection_closed()),
            },
        }
    }

    /// Queue the start of a player audio stream, fixing its position in this
    /// connection's frame order without awaiting the write. Audio queued before
    /// it belongs to the previous stream and is discarded; audio pushed after it
    /// is unaffected.
    pub fn queue_stream_start(&self, player: StreamPlayerConfig) -> QueuedControl {
        self.queue_control(
            Message::StreamStart(StreamStart {
                player: Some(player),
                artwork: None,
                visualizer: None,
            }),
            AudioOrdering::Supersede,
        )
    }

    /// Queue the end of the player audio stream. Audio pushed before it is
    /// written *first* — `stream/end` means "after everything I sent", so
    /// overtaking it would cut off the tail of the stream.
    pub fn queue_stream_end(&self) -> QueuedControl {
        self.queue_control(
            Message::StreamEnd(StreamEnd {
                roles: Some(vec!["player".to_string()]),
            }),
            AudioOrdering::Flush,
        )
    }

    /// Queue a `stream/clear`. Dropping the queued-but-unwritten audio is
    /// exactly this message's own semantics, applied one hop earlier — there's
    /// no point writing audio the client is being told to discard.
    pub fn queue_stream_clear(&self) -> QueuedControl {
        self.queue_control(
            Message::StreamClear(StreamClear {
                roles: Some(vec!["player".to_string()]),
            }),
            AudioOrdering::Supersede,
        )
    }

    /// Queue a player command (volume, mute, static delay). These are
    /// independent of the audio stream — they take effect as soon as they
    /// arrive — so they overtake queued audio without disturbing any of it.
    pub fn queue_player_command(&self, command: PlayerCommand) -> QueuedControl {
        self.queue_control(
            Message::ServerCommand(ServerCommand {
                player: Some(command),
            }),
            |_| AudioOrdering::Independent,
        )
    }

    /// Push one player audio chunk. `timestamp_us` is the intended playback
    /// time in this server's clock domain (see [`crate::sync::raw_clock::Clock`]);
    /// the client converts it to its own domain using the offset/drift it
    /// tracks from `server/time` replies.
    ///
    /// Travels the same data-plane queue as [`Self::queue_audio`], so mixing
    /// the two keeps audio in push order; awaiting the write is the only
    /// difference.
    pub async fn send_audio_chunk(&self, timestamp_us: i64, payload: &[u8]) -> Result<(), Error> {
        let frame = encode_audio_frame(timestamp_us, payload);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.audio_queued.fetch_add(1, Ordering::Relaxed);
        let cmd = AudioCommand {
            seq: self.next_audio_seq(),
            frame: frame.0,
            ack: Some(ack_tx),
        };
        if self.audio_tx.send(cmd).is_err() {
            self.audio_queued.fetch_sub(1, Ordering::Relaxed);
            return Err(connection_closed());
        }
        ack_rx.await.map_err(|_| connection_closed())?
    }
}

/// Aborts background tasks on drop. Hold this alive for the lifetime of the
/// connection — mirrors [`crate::protocol::client::ConnectionGuard`].
#[derive(Debug)]
pub struct ServerConnectionGuard {
    sender: ServerSender,
    router_handle: Option<tokio::task::JoinHandle<()>>,
    writer_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ServerConnectionGuard {
    /// Close the connection. Unlike the client role, the server has no
    /// `goodbye` message of its own to send — it just closes the socket
    /// (optionally after the caller has already sent `stream/end`).
    ///
    /// The close command travels the control lane, so it is never queued behind
    /// this connection's pending audio. It can still wait on one in-flight write
    /// plus its own — and, if a `stream/end` is queued ahead of it, on that
    /// frame's flush budget — each bounded by the connection's write timeout, so
    /// this returns even against a socket that has stopped draining entirely.
    ///
    /// Audio still queued when the close is processed is **discarded**: nothing
    /// behind a close is written. Call [`ServerSender::queue_stream_end`] first if
    /// the tail of the stream matters.
    pub async fn disconnect(mut self) -> Result<(), Error> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let close_result = self
            .sender
            .ctrl_tx
            .send(ControlCommand::Close { ack: ack_tx })
            .map_err(|_| connection_closed());
        let result = match close_result {
            Ok(()) => ack_rx.await.map_err(|_| connection_closed())?,
            Err(e) => Err(e),
        };
        if let Some(h) = self.writer_handle.take() {
            let _ = h.await;
        }
        if let Some(h) = self.router_handle.take() {
            h.abort();
        }
        result
    }
}

impl Drop for ServerConnectionGuard {
    fn drop(&mut self) {
        if let Some(h) = self.router_handle.take() {
            h.abort();
        }
        if let Some(h) = self.writer_handle.take() {
            h.abort();
        }
    }
}

/// The parts of a [`ServerConnection`], from [`ServerConnection::split`].
#[derive(Debug)]
pub struct ServerConnectionParts {
    /// The client's `client/hello` payload.
    pub hello: ClientHello,
    /// Roles this server granted this client.
    pub active_roles: Vec<String>,
    /// `client/state`, `client/command` and `client/goodbye`, as received.
    pub messages: UnboundedReceiver<Message>,
    /// Handle for pushing stream control and audio to this client.
    pub sender: ServerSender,
    /// Keeps the connection alive; dropping it tears the connection down.
    pub guard: ServerConnectionGuard,
}

/// A single accepted client, past the handshake. Returned by
/// [`crate::server::ServerListener::accept`].
#[derive(Debug)]
pub struct ServerConnection {
    /// The client's `client/hello` payload — identity, declared capabilities,
    /// device info. Kept in full so callers can read `player@v1_support`
    /// (supported formats, buffer capacity) before starting a stream.
    hello: ClientHello,
    /// Roles this server granted this client (currently always `["player@v1"]`
    /// if the client declared support for it, else empty).
    active_roles: Vec<String>,
    /// `client/state`, `client/command`, and `client/goodbye` messages,
    /// forwarded as received. `client/time` is consumed internally (time-sync
    /// echo) and never forwarded here — same convention as
    /// [`crate::protocol::client::Connection::messages`].
    messages: UnboundedReceiver<Message>,
    sender: ServerSender,
    guard: ServerConnectionGuard,
}

impl ServerConnection {
    /// The client's `client/hello` payload.
    pub fn hello(&self) -> &ClientHello {
        &self.hello
    }

    /// Convenience accessor for `hello().client_id`.
    pub fn client_id(&self) -> &str {
        &self.hello.client_id
    }

    /// Roles granted to this client.
    pub fn active_roles(&self) -> &[String] {
        &self.active_roles
    }

    /// A cheap-to-clone sender for pushing stream control and audio messages
    /// to this client, usable independently of `&mut self`.
    pub fn sender(&self) -> ServerSender {
        self.sender.clone()
    }

    /// Receive the next `client/state`, `client/command`, or `client/goodbye`
    /// message. Returns `None` once the connection has closed.
    pub async fn recv_message(&mut self) -> Option<Message> {
        self.messages.recv().await
    }

    /// Close the connection.
    pub async fn disconnect(self) -> Result<(), Error> {
        self.guard.disconnect().await
    }

    /// Split into its parts, so the message loop can be driven by one task while
    /// another holds the sender and the guard.
    ///
    /// This is the counterpart to [`crate::protocol::client::ProtocolClient::split`]
    /// on the client role, and the only way to obtain a [`ServerConnectionGuard`]:
    /// hold it for as long as the connection should live, since dropping it tears
    /// the connection down.
    pub fn split(self) -> ServerConnectionParts {
        ServerConnectionParts {
            hello: self.hello,
            active_roles: self.active_roles,
            messages: self.messages,
            sender: self.sender,
            guard: self.guard,
        }
    }

    /// Drive the server-side handshake and message loop over an
    /// already-handshaked WebSocket stream.
    pub(crate) async fn drive<S>(
        ws_stream: WebSocketStream<S>,
        server_id: &str,
        server_name: &str,
        connection_reason: ConnectionReason,
        clock: Arc<dyn Clock>,
        write_timeout: Duration,
        handshake_timeout: Duration,
    ) -> Result<Self, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut write, mut read) = ws_stream.split();

        // Everything up to the writer task's existence is bounded here, because
        // until it exists there is nothing else to bound it: a peer that finishes
        // the WebSocket handshake and then stays silent, or stops reading, would
        // otherwise park this task forever — and with it the accept loop or the
        // dial supervisor that is driving it.
        let handshake = tokio::time::timeout(
            handshake_timeout,
            Self::handshake(
                &mut write,
                &mut read,
                server_id,
                server_name,
                connection_reason,
                write_timeout,
            ),
        );
        let (hello, active_roles) = match handshake.await {
            Ok(result) => result?,
            Err(_) => {
                return Err(Error::Connection(format!(
                    "handshake did not complete within {handshake_timeout:?}"
                )))
            }
        };

        let (ctrl_tx, ctrl_rx) = unbounded_channel::<ControlCommand>();
        let (audio_tx, audio_rx) = unbounded_channel::<AudioCommand>();
        let (time_tx, time_rx) = watch::channel::<Option<TimeRequest>>(None);
        let (message_tx, message_rx) = unbounded_channel();

        let audio_queued = Arc::new(AtomicUsize::new(0));
        let writer_handle = tokio::spawn(writer_task(
            write,
            ctrl_rx,
            time_rx,
            audio_rx,
            Arc::clone(&clock),
            Arc::clone(&audio_queued),
            write_timeout,
        ));

        let router_handle = tokio::spawn(async move {
            Self::message_router(read, message_tx, time_tx, clock).await;
        });

        let sender = ServerSender {
            ctrl_tx,
            audio_tx,
            audio_queued,
            audio_seq: Arc::new(AtomicU64::new(0)),
        };
        Ok(Self {
            hello,
            active_roles: active_roles.clone(),
            messages: message_rx,
            sender: sender.clone(),
            guard: ServerConnectionGuard {
                sender,
                router_handle: Some(router_handle),
                writer_handle: Some(writer_handle),
            },
        })
    }

    /// Read `client/hello`, negotiate roles, reply `server/hello`. Split out of
    /// [`Self::drive`] so the whole exchange can sit inside one deadline.
    async fn handshake<S>(
        write: &mut SplitSink<WebSocketStream<S>, WsMessage>,
        read: &mut SplitStream<WebSocketStream<S>>,
        server_id: &str,
        server_name: &str,
        connection_reason: ConnectionReason,
        write_timeout: Duration,
    ) -> Result<(ClientHello, Vec<String>), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        log::debug!("Waiting for client/hello...");
        let hello = loop {
            let Some(result) = read.next().await else {
                return Err(Error::Connection(
                    "connection closed before client/hello".to_string(),
                ));
            };
            match result {
                Ok(WsMessage::Text(text)) => {
                    let msg: Message = serde_json::from_str(&text).map_err(|e| {
                        log::warn!("Failed to parse client message: {} (payload: {})", e, text);
                        Error::Protocol(e.to_string())
                    })?;
                    match msg {
                        Message::ClientHello(hello) => {
                            if hello.version != 1 {
                                return Err(Error::Protocol(format!(
                                    "unsupported protocol version {} (only 1 is supported)",
                                    hello.version
                                )));
                            }
                            break hello;
                        }
                        other => {
                            return Err(Error::Protocol(format!(
                                "expected client/hello, got {:?}",
                                other
                            )))
                        }
                    }
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => continue,
                Ok(WsMessage::Close(_)) => {
                    return Err(Error::Connection("client closed connection".to_string()))
                }
                Ok(_) => continue,
                Err(e) => return Err(Error::WebSocket(e.to_string())),
            }
        };
        log::debug!("Received client/hello: {:?}", hello);

        let active_roles: Vec<String> = if hello.supported_roles.iter().any(|r| r == PLAYER_ROLE) {
            vec![PLAYER_ROLE.to_string()]
        } else {
            Vec::new()
        };

        let server_hello = Message::ServerHello(ServerHello {
            server_id: server_id.to_string(),
            name: server_name.to_string(),
            version: 1,
            active_roles: active_roles.clone(),
            connection_reason,
        });
        let json =
            serde_json::to_string(&server_hello).map_err(|e| Error::Protocol(e.to_string()))?;
        // Bounded like every other write: this one runs before the writer task
        // exists, so it needs its own deadline rather than inheriting one.
        write_frame(write, WsMessage::Text(json.into()), write_timeout).await?;

        Ok((hello, active_roles))
    }

    async fn message_router<S>(
        mut read: SplitStream<WebSocketStream<S>>,
        message_tx: UnboundedSender<Message>,
        time_tx: watch::Sender<Option<TimeRequest>>,
        clock: Arc<dyn Clock>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut message_closed = false;
        let mut last_time_reply_us: Option<i64> = None;

        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Text(text)) => {
                    // Capture receive time before deserialization so
                    // `server_received` is as close to true arrival as possible.
                    let server_received = clock.now_micros();
                    match serde_json::from_str::<Message>(&text) {
                        Ok(Message::ClientTime(t)) => {
                            // Rate-limit, then coalesce. A peer asking faster than
                            // the spec's ~1/s cadence gains nothing — each reply
                            // supersedes the last — but answering every request
                            // would let its send rate drive our work and memory.
                            let too_soon = last_time_reply_us.is_some_and(|last| {
                                server_received - last < MIN_TIME_REPLY_INTERVAL_US
                            });
                            if too_soon {
                                log::trace!("Ignoring client/time inside the reply interval");
                                continue;
                            }
                            last_time_reply_us = Some(server_received);
                            if time_tx
                                .send(Some(TimeRequest {
                                    client_transmitted: t.client_transmitted,
                                    server_received,
                                }))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(Message::ClientHello(_)) => {
                            log::warn!("Ignoring unexpected client/hello after handshake");
                        }
                        Ok(msg) => {
                            log::debug!("Received message: {:?}", msg);
                            if !message_closed && message_tx.send(msg).is_err() {
                                log::error!(
                                    "Message receiver dropped — messages will be discarded"
                                );
                                message_closed = true;
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to parse message: {} (payload: {})", e, text);
                        }
                    }
                }
                Ok(WsMessage::Binary(_)) => {
                    // A client never sends binary frames in the current
                    // protocol (audio/artwork/visualizer are server->client
                    // only); log and ignore rather than erroring the
                    // connection over a forward-compatible future frame.
                    log::warn!("Ignoring unexpected binary frame from client");
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {}
                Ok(WsMessage::Close(_)) => {
                    log::info!("Client closed connection");
                    break;
                }
                Err(e) => {
                    log::warn!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
        log::debug!("Message router: WebSocket stream ended");
    }
}
