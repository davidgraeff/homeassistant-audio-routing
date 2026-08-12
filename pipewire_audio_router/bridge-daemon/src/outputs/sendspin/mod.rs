//! Sendspin output backend — ESPHome speakers (HA Voice PE and friends).
//!
//! [`server`] is the embedded Sendspin *server* role: it dials each adopted
//! device, hands it a codec-tagged PCM stream off the group's anchor monitor, and
//! owns the realtime relay thread that must never be starved. [`codec`] decides
//! and encodes the wire format, [`discovery`] browses `_sendspin._tcp`,
//! [`liveness`] owns the online/offline verdict (mDNS only ever *adds*, so a
//! flapping record must not tear down a live group), and [`volume`] is the
//! per-device volume/mute control channel.
//!
//! The expensive fact about this backend, worth knowing before touching any of
//! it: a reconnecting device takes tens of seconds to render audio again,
//! whatever the daemon does. Session churn is not cheap here.

pub(crate) mod codec;
pub(crate) mod discovery;
pub(crate) mod liveness;
pub(crate) mod server;
pub(crate) mod volume;
