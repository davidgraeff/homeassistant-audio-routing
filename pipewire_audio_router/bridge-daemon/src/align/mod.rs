//! Latency alignment: making several outputs that share one source arrive at the
//! listener at the same time (docs/mic-alignment-plan.md).
//!
//! The boundary is *measuring and deciding delays*, never carrying audio. A
//! session ([`calibrate`]) plays the test signal off one clock with a chosen
//! subset audible; the by-ear path stops there and the microphone path continues
//! through ingest ([`mic`]), arrival estimation ([`estimator`]), playback-level
//! learning ([`levels`]) and orchestration ([`measure`]), with [`group`] forming
//! the temporary exclusive speaker group a measurement runs against.
//!
//! [`relay_delay`] lives here rather than under `outputs/` for the same reason:
//! its provisional delay line and calibration mute exist only for alignment, even
//! though the hook that applies them sits in the three output relays.

pub(crate) mod calibrate;
pub(crate) mod estimator;
pub(crate) mod group;
pub(crate) mod levels;
pub(crate) mod measure;
pub(crate) mod mic;
pub(crate) mod relay_delay;
