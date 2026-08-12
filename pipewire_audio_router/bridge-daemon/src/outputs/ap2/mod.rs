//! AirPlay-2 output backend — AV receivers (Yamaha WX-021, Pioneer VSX-934, …).
//!
//! [`server`] is the in-process AP2 *sender*: it captures the group's anchor
//! monitor and streams timed RTP to each adopted receiver, replacing the RAOP
//! output path that was removed. [`ptp`] is the daemon's single host-global gPTP
//! grandmaster, shared by every AP2 stream and injected into the vendored sender —
//! there is exactly one per host, which is why it is a module and not a per-device
//! thing. [`discovery`] browses `_airplay._tcp` and registers each receiver as a
//! PTP peer, [`probe`] asks whether the AirPlay *service* answers rather than just
//! its TCP port, [`liveness`] turns those signals into an online/offline verdict,
//! [`health`] is what the UI shows when a receiver is reachable but silent, and
//! [`volume`] the per-device volume/mute channel.
//!
//! `server` ↔ `health` ↔ `liveness` is a genuine cycle. It is contained by this
//! directory, which is the point: untangling it is behaviour work, not layout
//! work, and the tree no longer hides that the three are one subsystem.

pub(crate) mod discovery;
pub(crate) mod health;
pub(crate) mod liveness;
pub(crate) mod probe;
pub(crate) mod ptp;
pub(crate) mod server;
pub(crate) mod volume;
