//! pw-sink output backend — another PipeWire Linux host as a routable output.
//!
//! The cheap backend, because the receiver speaks our transport natively: no codec, no
//! Rust hot path of its own. [`applemidi`] is the AppleMIDI/RTP sender that carries the
//! PCM and the clock.
//!
//! It was not always that sender. The first version loaded two native PipeWire modules —
//! `libpipewire-module-rtp-sink` per target plus one `rtp-sap` announcer — and only ever
//! ran that way under a spike; the shipped path replaced it because a module gives no
//! per-target clock control and no way to see a stalled sender. The reasoning and the
//! measurements are in `docs/pipewire-sink-output.md` and
//! `docs/old/pipewire-sink-spike-results.md`; the module-args builder went with the spike.
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
pub(crate) mod sender_liveness;
pub(crate) mod server;
pub(crate) mod target_liveness;
