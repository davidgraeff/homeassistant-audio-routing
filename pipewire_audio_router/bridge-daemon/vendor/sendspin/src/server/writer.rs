// ABOUTME: The per-connection writer task: two lanes plus a coalesced time slot,
// ABOUTME: a deadline on every write, and the policy deciding what a control frame
// ABOUTME: does about the audio it overtakes.

use crate::error::Error;
use crate::protocol::messages::{Message, ServerTime};
use crate::sync::raw_clock::Clock;
use futures_util::{stream::SplitSink, SinkExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use tokio_tungstenite::{
    tungstenite::{Bytes, Message as WsMessage},
    WebSocketStream,
};

/// Maximum audio frames a single connection may have queued but not yet
/// written before [`ServerSender::enqueue_audio`] starts dropping frames. This
/// bounds memory for a slow or stalled member so it can't back up the whole
/// process — its own audio suffers, nobody else's does.
pub(super) const MAX_QUEUED_AUDIO_FRAMES: usize = 32;

/// Minimum spacing between `server/time` replies. The spec's cadence is about
/// one `client/time` per second; a peer that asks far faster gains nothing (each
/// reply supersedes the last) and would otherwise convert its own send rate into
/// server work. Requests arriving inside this window are answered by the reply
/// already pending rather than queueing another.
pub(super) const MIN_TIME_REPLY_INTERVAL_US: i64 = 50_000;

/// Default deadline for a single WebSocket write before the connection is
/// declared dead (override with [`crate::server::ServerRole::write_timeout`]).
///
/// A member whose socket stops draining — a client that dropped off the WiFi
/// while holding the TCP connection open, so the kernel send buffer fills and
/// never empties — must not park the writer task forever. The writer is the
/// only thing that touches the socket, so a write that never completes stalls
/// *everything* behind it: `stream/end`, a volume command, the close handshake.
/// Every caller awaiting one of those would wait with it. Bounding each write
/// converts that indefinite hang into a dead connection: the writer exits,
/// every [`crate::server::ServerSender`] method starts returning `Err`, and
/// [`crate::server::Group`] prunes the member. A real client re-dials.
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a control frame does about the audio already queued ahead of it.
///
/// Control frames are dequeued ahead of audio (that's the whole point of the
/// separate lane), so each one has to say how it relates to the audio it just
/// overtook. Getting this wrong is how a client ends up seeing `stream/end`
/// followed by audio — or, in the other direction, how the tail of a stream gets
/// truncated.
///
/// The `u64` in the ordered variants is the audio sequence number this frame was
/// queued at: audio below it was pushed earlier, audio at or above it was pushed
/// later and is always left alone.
#[derive(Debug, Clone, Copy)]
pub(super) enum AudioOrdering {
    /// Unrelated to the audio stream — write it now and leave the queue alone.
    /// Player commands (volume, mute, static delay) take effect on arrival, so
    /// overtaking audio is exactly what's wanted.
    Independent,
    /// Write it now and *discard* the audio queued before it. That audio belongs
    /// to a stream this frame supersedes (`stream/start` after a format change)
    /// or explicitly invalidates (`stream/clear`), so delivering it afterwards
    /// would be wrong, not merely late.
    Supersede(u64),
    /// Write the audio queued before it *first*, then the frame itself.
    /// `stream/end` logically follows everything already pushed, so overtaking
    /// it would truncate the tail of the stream. The flush shares a single
    /// write-timeout budget with the frame it precedes — per-frame deadlines
    /// would let a merely-slow member hold `stream/end` for backlog × timeout —
    /// so once that budget is spent the remaining tail is dropped.
    Flush(u64),
}

/// A control-plane command for the writer task.
///
/// Control travels on its own channel and is dequeued ahead of queued audio (see
/// [`writer_task`]), so a backlog of audio for a slow member can't delay a
/// player command or the close handshake — and, together with [`AudioOrdering`],
/// can't reorder a stream lifecycle transition against the audio around it
/// either.
pub(super) enum ControlCommand {
    Send {
        msg: WsMessage,
        ordering: AudioOrdering,
        ack: tokio::sync::oneshot::Sender<Result<(), Error>>,
    },
    Close {
        ack: tokio::sync::oneshot::Sender<Result<(), Error>>,
    },
}

/// A pending `server/time` echo.
///
/// This travels in a single-slot [`watch`] channel rather than a queue, and that
/// is a correctness property, not an optimisation: the reply is derived purely
/// from the *latest* request, so a peer that floods `client/time` can only ever
/// have one outstanding. Queueing one per request instead lets a peer's send rate
/// dictate the server's memory use and, because control frames are written ahead
/// of audio, starve the audio lane to a standstill.
///
/// `server_transmitted` is stamped by the writer immediately before the frame
/// reaches the wire, not here — waiting time would otherwise leak into the
/// client's clock filter as measurement error.
#[derive(Debug, Clone, Copy)]
pub(super) struct TimeRequest {
    pub(super) client_transmitted: i64,
    pub(super) server_received: i64,
}

/// One data-plane (audio) frame for the writer task.
pub(super) struct AudioCommand {
    /// Enqueue order within this connection, compared against a control frame's
    /// `purge_audio_before`.
    pub(super) seq: u64,
    pub(super) frame: Bytes,
    /// `Some` only for [`ServerSender::send_audio_chunk`], which awaits its own
    /// frame. The group broadcast path ([`ServerSender::enqueue_audio`]) is
    /// fire-and-forget, so broadcasting never blocks on any member's socket.
    pub(super) ack: Option<tokio::sync::oneshot::Sender<Result<(), Error>>>,
}

/// Write one frame, bounded by `write_timeout`.
///
/// `SinkExt::send` is not cancel-safe, so timing out can leave the sink
/// mid-frame — that's acceptable only because the caller treats a timeout as
/// fatal: the writer loop exits and drops the sink, and nothing else ever
/// touches it.
pub(super) async fn write_frame<S>(
    sink: &mut SplitSink<WebSocketStream<S>, WsMessage>,
    msg: WsMessage,
    write_timeout: Duration,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match tokio::time::timeout(write_timeout, sink.send(msg)).await {
        Ok(result) => result.map_err(|e| Error::WebSocket(e.to_string())),
        Err(_) => Err(Error::WebSocket(format!(
            "write stalled for {write_timeout:?}"
        ))),
    }
}

/// Write out every already-queued audio frame with `seq < before`, so a control
/// frame that logically *follows* that audio ([`AudioOrdering::Flush`]) doesn't
/// overtake it.
///
/// Only frames already sitting in `audio_rx` are flushed, which is exactly the
/// right set: the caller queued this control frame after those audio frames, and
/// an unbounded channel's `send` completes immediately, so anything below `before`
/// is already here. (A caller enqueueing audio concurrently from another task
/// without serializing against the control frame gets best-effort ordering — see
/// [`QueuedControl`] for how [`crate::server::Group`] serializes the two.)
///
/// Bounded by `deadline` **overall**, not per frame. Per-frame deadlines would
/// make the flush cost backlog × write_timeout — a member slow enough to keep
/// succeeding could hold `stream/end` (and the close behind it) for minutes. Past
/// the deadline the remaining tail is dropped, which is the right trade: the tail
/// of an ending stream is worth less than the connection.
///
/// Returns the first write error, leaving the rest un-flushed: the connection is
/// finished at that point, so there's nothing to salvage.
async fn flush_audio_before<S>(
    sink: &mut SplitSink<WebSocketStream<S>, WsMessage>,
    audio_rx: &mut UnboundedReceiver<AudioCommand>,
    audio_queued: &AtomicUsize,
    before: u64,
    drop_audio_before: u64,
    deadline: Instant,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    while let Ok(audio) = audio_rx.try_recv() {
        // A frame at or above the marker was pushed after this control frame was
        // queued, so it isn't ours to flush — but it's already out of the channel
        // and re-queueing it would put it behind whatever arrived since, so write
        // it and stop. That also bounds the flush against a task that keeps
        // pushing audio concurrently.
        let reached_marker = audio.seq >= before;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = if audio.seq < drop_audio_before || remaining.is_zero() {
            // Superseded by an earlier transition that already went out, or the
            // flush budget is spent and the tail is being dropped.
            Ok(())
        } else {
            write_frame(sink, WsMessage::Binary(audio.frame), remaining).await
        };
        audio_queued.fetch_sub(1, Ordering::Relaxed);
        match result {
            Ok(()) => {
                if let Some(ack) = audio.ack {
                    let _ = ack.send(Ok(()));
                }
            }
            Err(e) => {
                // Report the flush failure as its own thing: the caller is awaiting
                // a lifecycle frame and would otherwise be told its own write
                // stalled, with the error text wrapped twice.
                let propagated = Error::WebSocket(format!(
                    "audio flush before a stream transition failed: {e}"
                ));
                if let Some(ack) = audio.ack {
                    let _ = ack.send(Err(e));
                }
                return Err(propagated);
            }
        }
        if reached_marker {
            break;
        }
    }
    Ok(())
}

pub(super) async fn writer_task<S>(
    mut sink: SplitSink<WebSocketStream<S>, WsMessage>,
    mut ctrl_rx: UnboundedReceiver<ControlCommand>,
    mut time_rx: watch::Receiver<Option<TimeRequest>>,
    mut audio_rx: UnboundedReceiver<AudioCommand>,
    clock: Arc<dyn Clock>,
    audio_queued: Arc<AtomicUsize>,
    write_timeout: Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Audio whose sequence number is below this was enqueued before a lifecycle
    // transition that has already been written, so writing it now would put it
    // on the wrong side of that transition. See `purge_audio_before`.
    let mut drop_audio_before: u64 = 0;

    loop {
        // `biased` makes this a strict priority rather than a random choice:
        // whenever a control frame is queued it is taken first, so queued audio
        // can never delay one. The time lane sits between the two: it is
        // single-slot, so it can hold at most one frame's worth of priority over
        // audio no matter how fast a peer asks.
        tokio::select! {
            biased;
            Some(cmd) = ctrl_rx.recv() => {
                match cmd {
                    ControlCommand::Send { msg, ordering, ack } => {
                        let result = match ordering {
                            AudioOrdering::Independent => {
                                write_frame(&mut sink, msg, write_timeout).await
                            }
                            AudioOrdering::Supersede(seq) => {
                                drop_audio_before = drop_audio_before.max(seq);
                                write_frame(&mut sink, msg, write_timeout).await
                            }
                            AudioOrdering::Flush(seq) => {
                                // One budget covers the flushed tail *and* the frame
                                // itself, so the whole operation stays inside two
                                // write timeouts however deep the backlog is.
                                let deadline = Instant::now() + write_timeout;
                                match flush_audio_before(
                                    &mut sink,
                                    &mut audio_rx,
                                    &audio_queued,
                                    seq,
                                    drop_audio_before,
                                    deadline,
                                )
                                .await
                                {
                                    Ok(()) => write_frame(&mut sink, msg, write_timeout).await,
                                    Err(e) => Err(e),
                                }
                            }
                        };
                        let failed = result.is_err();
                        log::debug!("Wrote control frame: ok={}", !failed);
                        // Ignore SendError: the caller may have dropped its receiver.
                        let _ = ack.send(result);
                        if failed {
                            break;
                        }
                    }
                    ControlCommand::Close { ack } => {
                        // No purge watermark needed: the loop exits below, so
                        // nothing queued behind a close is ever written.
                        let result = match tokio::time::timeout(write_timeout, sink.close()).await {
                            Ok(result) => result.map_err(|e| Error::WebSocket(e.to_string())),
                            Err(_) => Err(Error::WebSocket(format!(
                                "close stalled for {write_timeout:?}"
                            ))),
                        };
                        let _ = ack.send(result);
                        break;
                    }
                }
            }
            Ok(()) = time_rx.changed() => {
                // Stamp `server_transmitted` here, immediately before the write, so
                // however long this reply waited behind other frames does not leak
                // into the client's clock filter as measurement error.
                let Some(req) = *time_rx.borrow_and_update() else {
                    continue;
                };
                let reply = Message::ServerTime(ServerTime {
                    client_transmitted: req.client_transmitted,
                    server_received: req.server_received,
                    server_transmitted: clock.now_micros(),
                });
                let result = match serde_json::to_string(&reply) {
                    Ok(json) => {
                        write_frame(&mut sink, WsMessage::Text(json.into()), write_timeout).await
                    }
                    Err(e) => Err(Error::Protocol(e.to_string())),
                };
                if result.is_err() {
                    break;
                }
            }
            Some(audio) = audio_rx.recv() => {
                let result = if audio.seq < drop_audio_before {
                    log::trace!("dropping audio frame superseded by a stream lifecycle transition");
                    Ok(())
                } else {
                    write_frame(&mut sink, WsMessage::Binary(audio.frame), write_timeout).await
                };
                audio_queued.fetch_sub(1, Ordering::Relaxed);
                let failed = result.is_err();
                if let Some(ack) = audio.ack {
                    let _ = ack.send(result);
                }
                if failed {
                    break;
                }
            }
            else => break,
        }
    }
    log::debug!("Server connection writer task exiting");
}
