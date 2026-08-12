//! Audio format conversion, with no PipeWire and no I/O in it.
//!
//! The boundary is *bytes in, bytes out*: [`decode`] turns a fetched announce clip
//! (mp3/aac/flac/…) into PCM via symphonia, [`resample`] normalizes any PCM to the
//! one format the overlay path mixes in (48 kHz, stereo, S16LE), and [`wav`] wraps
//! PCM in a container — or unwraps one — for the players and the test signals.
//!
//! Every module here is a pure function library with no crate dependencies, which
//! is why this directory can be read on its own and why its tests need no fixtures.

pub(crate) mod decode;
pub(crate) mod resample;
pub(crate) mod wav;
