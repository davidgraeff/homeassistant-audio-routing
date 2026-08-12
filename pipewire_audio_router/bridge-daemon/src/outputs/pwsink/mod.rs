//! pw-sink output backend — another PipeWire Linux host as a routable output.
//!
//! The cheap backend, because the receiver speaks our transport natively: no
//! codec, no Rust hot path of its own. [`module_args`] is literally the SPA-JSON
//! `args` for the two native PipeWire modules that do the streaming, and
//! [`applemidi`] the AppleMIDI/RTP sender that carries the PCM and the clock.
//!
//! [`agent`] is the source of truth for which pw-sink outputs exist: a paired
//! receiver agent connects *to us* over a WebSocket, so unlike sendspin and ap2
//! there is no mDNS registry behind these devices. [`discovery`] browses for
//! remote targets as a *diagnostic* only — nothing in the routing path reads it —
//! which is exactly the kind of fact a flat file list cannot tell you.
//! [`server`] drives one group's targets off the anchor monitor.
//!
//! Two liveness modules, because there are two independent things to be wrong:
//! [`sender_liveness`] watches our own AppleMIDI sender (is it draining?) and
//! feeds each target's status into the UI, [`target_liveness`] watches the remote
//! host (is it still reachable?). A target can be perfectly reachable while our
//! sender is stalled, and vice versa — check the one that matches the symptom.

pub(crate) mod agent;
pub(crate) mod applemidi;
pub(crate) mod discovery;
pub(crate) mod module_args;
pub(crate) mod sender_liveness;
pub(crate) mod server;
pub(crate) mod target_liveness;
