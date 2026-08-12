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
//! [`capture`] is backend-agnostic, and every consumer shares the one
//! implementation: the sendspin, AP2 and pw-sink senders, the overlay mixer and
//! the alignment measurement all pull their PCM through it. There is no
//! per-backend capture path, and a new backend should not add one.

pub(crate) mod capture;
pub(crate) mod metering;
pub(crate) mod player;
pub(crate) mod profiler;
pub(crate) mod thread;
