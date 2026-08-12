//! Development-only experiments, reachable over `/api/spike/*`.
//!
//! Each of these answers one question about a backend with the real hardware in
//! the loop, without a routed source: [`per_device`] drives the sendspin
//! per-device sender path, [`ap2`] plays a tone through the AP2 sender and its PTP
//! grandmaster, [`pwsink`] loads the native modules against a remote PipeWire
//! host.
//!
//! They are not part of any audio path the UI can reach, and grouping them makes
//! that visible — and makes `#[cfg(feature = "spike")]` a one-line change if the
//! shipped binary should stop carrying them.

pub(crate) mod ap2;
pub(crate) mod per_device;
pub(crate) mod pwsink;
