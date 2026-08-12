//! PipeWire plumbing: talking to the local graph, with no product concepts in it.
//!
//! [`thread`] owns the connection — one dedicated OS thread running a blocking
//! main loop, because `pipewire-rs`'s core types are `Rc`-based and cannot cross
//! threads — and publishes the registry snapshot every other module reads. The
//! rest are the four things the daemon does *through* a graph node: capture PCM
//! off one ([`capture`]), write a clip into one ([`player`]), watch how loudly one
//! is playing ([`metering`]) and watch whether one is dropping frames
//! ([`profiler`]).
//!
//! [`capture`] used to be `sendspin_capture.rs`. It never was sendspin-specific:
//! the AP2, pw-sink and sendspin senders, the overlay mixer and the alignment
//! measurement all pull their audio through it. That the name said otherwise is
//! the clearest example of what this refactor is for.

pub(crate) mod capture;
pub(crate) mod metering;
pub(crate) mod player;
pub(crate) mod profiler;
pub(crate) mod thread;
