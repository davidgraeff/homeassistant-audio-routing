//! The push loop every alignment status socket uses: **one full status on connect,
//! then one per change** (plan §11 — "progress should be pushed, not polled").
//!
//! Three sockets share it, and the sharing is the point rather than a tidiness
//! preference:
//!
//! - `GET /api/align/measure/ws` — the measurement run ([`crate::align::measure`]);
//! - `GET /api/align/equivalence/ws` — the relay-vs-device experiment;
//! - `GET /api/align/ws` — the **session** itself ([`crate::align::calibrate`]), whose
//!   most important frame is the one that says the session is *gone*.
//!
//! What must not be copied a fourth time is the closed-tab handling below. A socket
//! that never reads is a socket that keeps its `watch` subscription until the next
//! state change happens to fail to send, which for the session socket could be an hour
//! — and the session socket exists precisely so that a *disappearance* is noticed
//! promptly.
//!
//! The snapshot is a **future** rather than a value: the run's state sits behind a
//! `std::sync::Mutex` and answers synchronously, but the session's sits behind a
//! `tokio::sync::Mutex` (its handlers await speakers), so serialising it has to be
//! awaited. Taking a future covers both instead of forcing the session to keep a
//! second, sync copy of its own state.

use axum::extract::ws::{Message, WebSocket};

/// One serialised status frame, or `None` when the state cannot be serialised at all —
/// which closes the socket rather than sending something a client would adopt.
///
/// Boxed and `'static` (rather than borrowing the closure) so a snapshot may own a
/// clone of whatever handle it reads: the session manager is `Clone`, and cloning it
/// into the future is what keeps the closure a plain `Fn` with no lifetime to name.
pub(crate) type StatusFrame = std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>;

/// Push `snapshot()` once on connect, then once per `changes` bump, until the client
/// goes away or the notifier is dropped.
pub(crate) async fn status_socket<F>(mut socket: WebSocket, mut changes: tokio::sync::watch::Receiver<u64>, snapshot: F)
where
    F: Fn() -> StatusFrame,
{
    let mut push = true;
    loop {
        if push {
            let Some(json) = snapshot().await else {
                return;
            };
            if socket.send(Message::Text(json.into())).await.is_err() {
                return;
            }
        }
        // Either the state moved, or the client said something. Reading the socket
        // matters even though these endpoints take no commands: it is how a closed tab
        // is noticed, and without it a dead socket would sit here holding a
        // subscription until the next state change happened to fail to send.
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return; // the notifier is gone: the process is shutting down
                }
                push = true;
            }
            msg = socket.recv() => match msg {
                // Text/binary is reserved for future control messages; ignoring it
                // keeps an older daemon usable with a newer client. A client frame is
                // not a state change, so it must not trigger a push of its own — and
                // for the session socket it must not count as *activity* either: the
                // idle timeout exists so that a forgotten tab cannot leave a room
                // muted, and a tab that chats on a socket is exactly a forgotten tab
                // (plan §1.2). Saying "I am still here" is a deliberate
                // `POST /api/align/still-here`, never a frame on this socket.
                Some(Ok(Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => push = false,
                _ => return,
            },
        }
    }
}
