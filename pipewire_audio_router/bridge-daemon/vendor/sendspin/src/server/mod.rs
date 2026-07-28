// ABOUTME: Server-role implementation of the Sendspin protocol
// ABOUTME: Accepts or dials player clients, syncs clocks, streams audio to synchronized multi-client groups

// Handles both connection directions: clients that dial in
// (`ServerListener::accept`) and clients that only run their own embedded
// server and must be discovered over mDNS and dialed (`ClientBrowser` +
// `dial_client`, or the supervised `ClientManager`).
//
// Each connection has one writer task fed by two queues: a control lane (stream
// lifecycle, player commands, close) and a data lane (audio). Control is
// dequeued ahead of audio, so a member with a backlog of audio — or a socket
// that has stopped draining entirely — can't delay a volume change or its own
// disconnect. Every write is bounded by a timeout (`DEFAULT_WRITE_TIMEOUT`), as is
// the handshake that precedes the writer (`DEFAULT_HANDSHAKE_TIMEOUT`), so a
// stalled socket becomes a dead connection rather than a stuck task.
//
// `server/time` echoes travel in a third, single-slot lane between the two: a
// reply is derived only from the newest request, so a peer that floods
// `client/time` can never have more than one outstanding and cannot turn its own
// send rate into server memory or starve the audio lane.
//
// Because control overtakes audio, each control frame declares how it relates to
// the audio it just overtook: a player command ignores it, `stream/start` and
// `stream/clear` discard it (it belongs to a stream they supersede), and
// `stream/end` writes it out first (it means "after everything I sent", so
// overtaking would truncate the tail). See `connection.rs` for the mechanism and
// `Group` for how the two lanes are kept in a single, valid order.
//
// Not yet supported: per-client codec transcoding (one PCM format per group),
// the non-player roles (color, visualizer, artwork, controller, metadata),
// external player registration, and late-join history replay — a client that
// joins mid-stream receives the current stream and all subsequent audio,
// synchronized with existing members, but nothing buffered from before it
// joined.

mod binary;
mod connection;
mod dial;
mod discovery;
mod group;
mod listener;
mod manager;
mod role;
mod timeline;
mod writer;

pub use binary::{encode_audio_frame, AudioFrame};
pub use connection::{
    AudioEnqueue, QueuedControl, ServerConnection, ServerConnectionGuard, ServerConnectionParts,
    ServerSender, DEFAULT_HANDSHAKE_TIMEOUT,
};
pub use discovery::{Advertisement, ClientBrowser, Discovered};
pub use group::{Group, OwnsTimeline, SharesTimeline};
pub use listener::ServerListener;
pub use manager::{ClientEvent, ClientManager};
/// Re-export of the mDNS crate this one is built on, so callers can construct and
/// configure a `ServiceDaemon` — e.g. restrict it to a single interface with
/// `IfKind` — and share it across [`Advertisement`]/[`ClientBrowser`] through their
/// `with_daemon` constructors instead of each spawning its own daemon thread.
///
/// Re-exported whole rather than as a few cherry-picked types on purpose: those
/// constructors only type-check if the caller's `mdns_sd` is the same version as
/// this crate's, and going through `sendspin::server::mdns_sd` makes that true by
/// construction. It also means `mdns_sd` is part of this crate's public API — a
/// major or minor bump of it is a breaking change here and will be released as one.
pub use mdns_sd;
pub use role::ServerRole;
pub use timeline::{SharedTimeline, DEFAULT_SEND_AHEAD_US};
pub use writer::DEFAULT_WRITE_TIMEOUT;
