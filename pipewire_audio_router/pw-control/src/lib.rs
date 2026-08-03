//! Shared PipeWire control primitives for the add-on's daemon and the
//! receiver-side agent (docs/receiver-agent-plan.md §12).
//!
//! Both sides do the same two low-level things — encode/decode volume pods, and
//! load a module into their *own* `pw_context` — and both had a marked copy of
//! the code with a "mirror changes" comment. This crate is that shared copy.
//!
//! ## What belongs here
//!
//! Only mechanism that is identical on both sides and has no policy in it:
//!
//! * [`pods`] — the `Props`/`Route` pods for volume and mute, plus the cubic↔linear
//!   scale (`channelVolumes = V³`) that `wpctl` and Home Assistant's
//!   `volume_level` both use;
//! * [`module`] — `pw_context_load_module`, which `pipewire-rs` does not wrap.
//!
//! What deliberately stays out: *which* node or device to write (the daemon drives
//! its own graph by node id, the agent walks from its receive stream to whichever
//! sink is playing it), connection/mainloop management, and anything that knows
//! about routing, groups or announcements.

pub mod module;
pub mod pods;
