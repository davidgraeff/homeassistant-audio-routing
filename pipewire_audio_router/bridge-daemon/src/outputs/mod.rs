//! Audio **out of** the graph: one directory per output backend, plus the piece
//! they all share.
//!
//! A backend exists because its receiver cannot take a PipeWire stream: it needs
//! a capture off the group's anchor monitor, an encoder, and per-device writers.
//! [`sendspin`] does that for ESPHome speakers and [`ap2`] for AirPlay-2
//! receivers, which also owns the host-global PTP grandmaster its timing needs.
//! `pwsink` is the exception that proves the rule — a remote PipeWire host speaks
//! our transport natively: [`pwsink`] needs no codec and no hot path of its own.
//!
//! [`overlay_mixer`] is what every backend shares: while an announcement is
//! active on one output, that device's frame becomes duck(music)+overlay while its
//! groupmates keep plain music. It is per-*output*, which is what makes
//! announcing to one speaker of a group possible at all.
//!
//! Not here on purpose: `align/relay_delay.rs`, even though the three output
//! relays call it every block. It exists only for alignment's provisional delays
//! and calibration mute, so it lives with the code that gives it meaning — the
//! hook is in the relays, the reason is not (see `align/mod.rs`).

pub(crate) mod ap2;
pub(crate) mod listing;
pub(crate) mod overlay_mixer;
pub(crate) mod pwsink;
pub(crate) mod sendspin;
