//! `GET /api/events` — **the** push socket. One connection, many topics.
//!
//! ## Why there is exactly one
//!
//! There used to be four status sockets — `/api/routing/ws`, `/api/align/ws`,
//! `/api/align/measure/ws`, `/api/align/equivalence/ws` — and a browser gives a page
//! **six** connections per host over HTTP/1.1. The alignment wizard alone opened three
//! of them while the routing graph held a fourth, which left no headroom for the REST
//! calls those very pages make: a poll can then sit queued behind an idle socket that
//! will not close until the user navigates away. That is the whole reason for this
//! module, and it is why the topics are *subscribed*, not passed in the URL — a page
//! that leaves unsubscribes, and the daemon stops doing that work, without dropping the
//! connection the next page will need.
//!
//! ## The protocol
//!
//! Client → server, JSON text frames:
//!
//! ```json
//! { "op": "subscribe",   "topics": ["matrix", "meters"] }
//! { "op": "unsubscribe", "topics": ["meters"] }
//! ```
//!
//! Server → client, JSON text frames, internally tagged by `type`:
//!
//! * `subscribed` — the acknowledgement, listing what is now on and what was not
//!   understood. A client that names a topic this daemon does not have learns so
//!   immediately instead of waiting forever for a frame;
//! * one frame per topic, named by the topic ([`Frame`]).
//!
//! Two rules that consumers depend on:
//!
//! 1. **Subscribing sends that topic's current state at once** — every one of the four
//!    old sockets pushed a full snapshot on connect, and a subscription is the successor
//!    of a connect. Except [`Topic::Meters`], which has nothing to say until its next
//!    tick;
//! 2. **frames are deduplicated per topic**: a change that leaves a topic's payload
//!    byte-identical sends nothing. The daemon's change notifier fires for *any* change,
//!    so without this every topic would wake for every one.
//!
//! ## What subscribing costs, and why that is the point
//!
//! [`Topic::Meters`] arms per-source peak metering and the PipeWire profiler while at
//! least one client wants it, and disarms them when the last one goes away — the
//! accounting that used to be tied to the socket's lifetime is now tied to the
//! *subscription*, which is strictly better: an alignment wizard no longer pays for a
//! meter tick it never renders, and closing a page stops the work even if the socket
//! stays open for the next one.
//!
//! ## What is deliberately *not* here
//!
//! `GET /api/align/mic/ws` (binary microphone ingest, client → server, one socket at a
//! time, its own hello/ready handshake) and `GET /api/agent/ws` (the receiver-agent
//! protocol, authenticated with that host's bearer token, not a UI feed). Neither is a
//! status feed; folding either into this would mean two protocols on one socket.

use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// How often the [`Topic::Meters`] fast lane goes out. Peaks and xrun counts move
/// without any graph change, so they cannot ride the change notifier; everything else
/// on this socket does.
const METER_TICK: Duration = Duration::from_millis(250);

/// One subscribable stream of frames.
///
/// The names are the wire names, and they are the same strings the old sockets used as
/// their frame `type`, so a consumer's frame handling did not have to change when the
/// sockets merged — only its plumbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// The routing matrix: sources, outputs and the links between them.
    Matrix,
    /// `GET /api/outputs` — the adopted devices.
    Outputs,
    /// `GET /api/outputs/discovered` — offered but not added.
    Discovered,
    /// `GET /api/agents` — paired receiver hosts and pending pair requests.
    Agents,
    /// Per-source now-playing metadata.
    NowPlaying,
    /// The fast lane: peak levels and xrun counts, on a 250 ms tick. Subscribing to
    /// this is what arms metering and the profiler.
    Meters,
    /// The alignment **session** — the speakers held, who is audible, and the frame
    /// that says the session is gone.
    Align,
    /// The alignment **measurement run**.
    Measure,
    /// The relay-vs-device delay experiment.
    Equivalence,
}

impl Topic {
    /// Every topic, for the "subscribe to everything" case a diagnostic client wants.
    const ALL: [Topic; 9] = [
        Topic::Matrix,
        Topic::Outputs,
        Topic::Discovered,
        Topic::Agents,
        Topic::NowPlaying,
        Topic::Meters,
        Topic::Align,
        Topic::Measure,
        Topic::Equivalence,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "matrix" => Some(Self::Matrix),
            "outputs" => Some(Self::Outputs),
            "discovered" => Some(Self::Discovered),
            "agents" => Some(Self::Agents),
            "now_playing" => Some(Self::NowPlaying),
            "meters" => Some(Self::Meters),
            "align" => Some(Self::Align),
            "measure" => Some(Self::Measure),
            "equivalence" => Some(Self::Equivalence),
            "all" | "*" => None,
            _ => None,
        }
    }

    /// Is this one of the listings that ride the change notifier and are rebuilt
    /// together? They come from one pass over the stores, so one subscriber to any of
    /// them costs the same as four.
    fn is_listing(self) -> bool {
        matches!(self, Self::Outputs | Self::Discovered | Self::Agents | Self::NowPlaying)
    }
}

/// A client control message.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Control {
    Subscribe { topics: Vec<String> },
    Unsubscribe { topics: Vec<String> },
}

/// One frame on the socket.
///
/// **Internally tagged, and the matrix stays flat.** The matrix frame historically *was*
/// the whole frame — a bare `{sources, outputs, links}` — so `type` is added beside
/// those fields rather than nesting them.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame<'a> {
    /// The acknowledgement of a `subscribe`, so an unknown topic name is a visible
    /// mistake rather than silence.
    Subscribed {
        topics: Vec<Topic>,
        unknown: Vec<&'a str>,
    },
    Matrix(&'a crate::routing::RoutingMatrix),
    Outputs {
        outputs: &'a [crate::outputs::listing::OutputInfo],
    },
    Discovered {
        outputs: &'a [crate::outputs::listing::OutputInfo],
    },
    Agents {
        agents: &'a [crate::outputs::pwsink::agent::AgentInfo],
    },
    NowPlaying {
        sources: &'a BTreeMap<String, crate::sources::now_playing::NowPlaying>,
    },
    /// Keyed by node name, so the client merges it onto the matrix it already has.
    /// Absent field means zero — that is how a level decaying to silence and an xrun
    /// counter at rest are expressed without sending anything.
    Meters {
        nodes: &'a BTreeMap<String, crate::routing::MeterSample>,
    },
    Align {
        state: &'a crate::align::calibrate::AlignState,
    },
    Measure {
        status: &'a crate::align::measure::MeasureStatus,
    },
    Equivalence {
        status: &'a crate::align::measure::EquivalenceStatus,
    },
}

pub async fn events_ws(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Per-socket state: what it wants, and what it last sent for each topic.
#[derive(Default)]
struct Session {
    on: HashSet<Topic>,
    /// Last payload sent per topic, for the dedupe. Serialised JSON, compared as such —
    /// which is what keeps a client that asked for everything quiet when nothing moved.
    sent: HashMap<Topic, String>,
    /// Whether this socket currently holds the meter/profiler watch, so the arm and the
    /// disarm cannot get out of step (subscribing twice must not count twice).
    metering: bool,
}

impl Session {
    fn wants(&self, t: Topic) -> bool {
        self.on.contains(&t)
    }

    /// Remember `json` as this topic's payload, answering whether it is new — i.e.
    /// whether it is worth a frame. The pure half of [`push`], because "a change that
    /// leaves a topic identical sends nothing" is a rule worth a test of its own.
    fn record(&mut self, topic: Topic, json: &str) -> bool {
        if self.sent.get(&topic).is_some_and(|last| last == json) {
            return false;
        }
        self.sent.insert(topic, json.to_string());
        true
    }

    fn wants_any_listing(&self) -> bool {
        self.on.iter().any(|t| t.is_listing())
    }
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut changes = state.changes.subscribe();
    let mut align_changes = state.align.subscribe();
    let mut measure_changes = crate::align::measure::shared().subscribe();
    let mut equivalence_changes = crate::align::measure::equivalence().subscribe();

    let mut tick = tokio::time::interval(METER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut s = Session::default();
    // The node names a meters frame may mention: whatever the last matrix showed. Kept
    // even when `matrix` is not subscribed, because `meters` alone is a legitimate
    // subscription (the Outputs page's level bars) and the fast lane must still know
    // which nodes exist.
    let mut matrix_nodes: Vec<String> = Vec::new();
    // Set by the change arm, flushed by the tick arm: a burst of changes should cost one
    // rebuild of the listings, not one each.
    let mut listings_dirty = false;

    // There is deliberately **no periodic matrix re-check**. The matrix is pushed when,
    // and only when, something notifies `changes` — so a mutation path that forgets to
    // notify leaves a visibly stale graph instead of self-healing a fraction of a second
    // later. That is the point: the old unconditional 250 ms push had been hiding
    // exactly such a bug.
    loop {
        tokio::select! {
            changed = changes.recv() => {
                match changed {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if s.wants(Topic::Matrix) || s.wants(Topic::Meters) {
                            let matrix = crate::routing::build_snapshot(&state).await;
                            if s.metering {
                                state.meters.reconcile_sources(&crate::routing::present_source_meters(&matrix));
                            }
                            matrix_nodes = crate::routing::matrix_node_names(&matrix);
                            if s.wants(Topic::Matrix) && push(&mut socket, &mut s, Topic::Matrix, Frame::Matrix(&matrix)).await.is_err() {
                                break;
                            }
                        }
                        listings_dirty = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = tick.tick() => {
                if s.wants(Topic::Meters) {
                    let samples = crate::routing::meter_samples(&state, &matrix_nodes);
                    if push(&mut socket, &mut s, Topic::Meters, Frame::Meters { nodes: &samples }).await.is_err() {
                        break;
                    }
                }
                if listings_dirty && s.wants_any_listing() {
                    listings_dirty = false;
                    if push_listings(&mut socket, &state, &mut s).await.is_err() {
                        break;
                    }
                }
            }
            changed = align_changes.changed() => {
                if changed.is_err() {
                    break; // the notifier is gone: the process is shutting down
                }
                if s.wants(Topic::Align) && push_align(&mut socket, &state, &mut s).await.is_err() {
                    break;
                }
            }
            changed = measure_changes.changed() => {
                if changed.is_err() {
                    break;
                }
                if s.wants(Topic::Measure) && push_measure(&mut socket, &mut s).await.is_err() {
                    break;
                }
            }
            changed = equivalence_changes.changed() => {
                if changed.is_err() {
                    break;
                }
                if s.wants(Topic::Equivalence) && push_equivalence(&mut socket, &mut s).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    // A control frame. Anything unparseable is ignored rather than
                    // fatal: killing the socket over a typo would take every other
                    // topic down with it.
                    Some(Ok(Message::Text(text))) => {
                        if apply_control(&mut socket, &state, &mut s, &text, &mut matrix_nodes).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
                    // None or an error: the client is gone. This arm is why a closed tab
                    // is noticed promptly — without reading the socket, a dead client
                    // would hold its subscriptions until the next failed send.
                    _ => break,
                }
            }
        }
    }
    // One teardown for every exit path, and the only place the meter watch is dropped.
    stop_metering(&state, &mut s);
}

/// Apply one control message, pushing the current state of everything it turned on.
async fn apply_control(
    socket: &mut WebSocket,
    state: &AppState,
    s: &mut Session,
    text: &str,
    matrix_nodes: &mut Vec<String>,
) -> Result<(), axum::Error> {
    let Ok(control) = serde_json::from_str::<Control>(text) else {
        tracing::debug!("events socket: ignoring an unparseable control frame ({} bytes)", text.len());
        return Ok(());
    };
    match control {
        Control::Subscribe { topics } => {
            let (mut wanted, mut unknown) = (Vec::new(), Vec::new());
            for name in &topics {
                match Topic::parse(name) {
                    Some(t) => wanted.push(t),
                    // "all" is not a topic but it is the obvious thing a diagnostic
                    // client types, so it means every topic rather than a mistake.
                    None if name == "all" || name == "*" => wanted.extend(Topic::ALL),
                    None => unknown.push(name.as_str()),
                }
            }
            wanted.sort();
            wanted.dedup();
            let fresh: Vec<Topic> = wanted.iter().copied().filter(|t| s.on.insert(*t)).collect();
            let ack = Frame::Subscribed { topics: wanted.clone(), unknown };
            send(socket, &ack).await?;
            if fresh.contains(&Topic::Meters) {
                start_metering(state, s);
            }
            // Rule 1: a subscription is the successor of a connect, so it answers with
            // the current state — otherwise a page that subscribes to `align` while
            // nothing is changing would sit blank until the next change.
            push_fresh(socket, state, s, &fresh, matrix_nodes).await
        }
        Control::Unsubscribe { topics } => {
            for name in &topics {
                let dropped: Vec<Topic> = match Topic::parse(name) {
                    Some(t) => vec![t],
                    None if name == "all" || name == "*" => Topic::ALL.to_vec(),
                    None => Vec::new(),
                };
                for t in dropped {
                    s.on.remove(&t);
                    // The dedupe memory goes with the subscription: re-subscribing must
                    // resend the current state even if it has not changed since.
                    s.sent.remove(&t);
                    if t == Topic::Meters {
                        stop_metering(state, s);
                    }
                }
            }
            Ok(())
        }
    }
}

/// Push the current state of each newly-subscribed topic.
async fn push_fresh(
    socket: &mut WebSocket,
    state: &AppState,
    s: &mut Session,
    fresh: &[Topic],
    matrix_nodes: &mut Vec<String>,
) -> Result<(), axum::Error> {
    if fresh.contains(&Topic::Matrix) || (fresh.contains(&Topic::Meters) && matrix_nodes.is_empty()) {
        let matrix = crate::routing::build_snapshot(state).await;
        if s.metering {
            state.meters.reconcile_sources(&crate::routing::present_source_meters(&matrix));
        }
        *matrix_nodes = crate::routing::matrix_node_names(&matrix);
        if fresh.contains(&Topic::Matrix) {
            push(socket, s, Topic::Matrix, Frame::Matrix(&matrix)).await?;
        }
    }
    if fresh.iter().any(|t| t.is_listing()) {
        push_listings(socket, state, s).await?;
    }
    if fresh.contains(&Topic::Align) {
        push_align(socket, state, s).await?;
    }
    if fresh.contains(&Topic::Measure) {
        push_measure(socket, s).await?;
    }
    if fresh.contains(&Topic::Equivalence) {
        push_equivalence(socket, s).await?;
    }
    // `Meters` has nothing to send until its next tick, which is 250 ms away at most.
    Ok(())
}

/// The four listings come from one pass over the stores, so they are built together and
/// each is then sent only if the subscriber wants it *and* it changed.
async fn push_listings(socket: &mut WebSocket, state: &AppState, s: &mut Session) -> Result<(), axum::Error> {
    let (adopted, offered) = crate::outputs::listing::outputs_listings(state).await;
    let agents = state.agents.lock().await.snapshot();
    let now_playing = state.now_playing.snapshot();
    if s.wants(Topic::Outputs) {
        push(socket, s, Topic::Outputs, Frame::Outputs { outputs: &adopted }).await?;
    }
    if s.wants(Topic::Discovered) {
        push(socket, s, Topic::Discovered, Frame::Discovered { outputs: &offered }).await?;
    }
    if s.wants(Topic::Agents) {
        push(socket, s, Topic::Agents, Frame::Agents { agents: &agents }).await?;
    }
    if s.wants(Topic::NowPlaying) {
        push(socket, s, Topic::NowPlaying, Frame::NowPlaying { sources: &now_playing }).await?;
    }
    Ok(())
}

async fn push_align(socket: &mut WebSocket, state: &AppState, s: &mut Session) -> Result<(), axum::Error> {
    let st = state.align.status().await;
    push(socket, s, Topic::Align, Frame::Align { state: &st }).await
}

async fn push_measure(socket: &mut WebSocket, s: &mut Session) -> Result<(), axum::Error> {
    let st = crate::align::measure::shared().status();
    push(socket, s, Topic::Measure, Frame::Measure { status: &st }).await
}

async fn push_equivalence(socket: &mut WebSocket, s: &mut Session) -> Result<(), axum::Error> {
    let st = crate::align::measure::equivalence().status();
    push(socket, s, Topic::Equivalence, Frame::Equivalence { status: &st }).await
}

/// Send a frame only if this socket's last payload for that topic differed. Every
/// topic frame goes out through here; there is no unconditional send left.
async fn push(socket: &mut WebSocket, s: &mut Session, topic: Topic, frame: Frame<'_>) -> Result<(), axum::Error> {
    let json = match serde_json::to_string(&frame) {
        Ok(json) => json,
        // Unreachable in practice; dropping one frame beats killing the socket.
        Err(e) => {
            tracing::warn!("could not serialise a {topic:?} frame: {e}");
            return Ok(());
        }
    };
    if !s.record(topic, &json) {
        return Ok(());
    }
    socket.send(Message::Text(json.into())).await
}

/// A frame with no dedupe (the subscribe acknowledgement, which is a reply rather than
/// a state).
async fn send(socket: &mut WebSocket, frame: &Frame<'_>) -> Result<(), axum::Error> {
    match serde_json::to_string(frame) {
        Ok(json) => socket.send(Message::Text(json.into())).await,
        Err(e) => {
            tracing::warn!("could not serialise a control reply: {e}");
            Ok(())
        }
    }
}

/// Arm per-source metering and the PipeWire profiler for this socket, once.
///
/// Takes the three handles it touches rather than the whole [`AppState`], so the
/// arm/disarm pairing — the part that leaks a metering tap or a permanently-armed
/// profiler when it goes wrong — is testable without a daemon.
fn start_metering(state: &AppState, s: &mut Session) {
    if std::mem::replace(&mut s.metering, true) {
        return;
    }
    arm(&state.meters, &state.profiler_watchers, |on| profiling(state, on), true);
}

/// Drop them again — on unsubscribe and on every exit path. Idempotent, which is what
/// makes it safe to call from both.
fn stop_metering(state: &AppState, s: &mut Session) {
    if !std::mem::replace(&mut s.metering, false) {
        return;
    }
    arm(&state.meters, &state.profiler_watchers, |on| profiling(state, on), false);
}

/// The refcounted half: the meter hub counts watchers itself, and the profiler is armed
/// by the first subscriber and disarmed by the last.
fn arm(meters: &crate::pw::metering::SharedMeters, watchers: &std::sync::atomic::AtomicUsize, set_profiling: impl Fn(bool), on: bool) {
    if on {
        meters.watch();
        // `fetch_add` returns the previous count, so `== 0` means we are the first.
        if watchers.fetch_add(1, Ordering::SeqCst) == 0 {
            set_profiling(true);
        }
    } else {
        meters.unwatch();
        if watchers.fetch_sub(1, Ordering::SeqCst) == 1 {
            set_profiling(false);
        }
    }
}

/// Tell the PipeWire thread to arm or disarm per-node xrun profiling. A closure at the
/// call site above rather than a sender passed down, so [`arm`]'s refcounting is
/// testable without a PipeWire channel.
fn profiling(state: &AppState, on: bool) {
    let _ = state.pw_cmd.send(crate::pw::thread::PwCommand::SetProfiling(on));
}

#[cfg(test)]
mod tests;
