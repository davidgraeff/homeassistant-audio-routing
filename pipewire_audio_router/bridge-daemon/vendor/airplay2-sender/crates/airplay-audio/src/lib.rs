//! # airplay-audio
//!
//! Audio encoding and RTP streaming for AirPlay 2.
//!
//! This crate provides:
//! - Audio decoding from various formats (via symphonia)
//! - Live audio streaming from external sources (e.g., Bluetooth)
//! - ALAC encoding for realtime streaming
//! - AAC encoding for buffered streaming
//! - RTP packet formatting and transmission
//! - Audio buffer management
//! - Retransmission handling

// LOCAL: lint noise from upstream silenced here so it can't bury a real
// warning from the daemon (upstream API surface the daemon doesn't call; upstream leftovers). Fixing the ~50 sites
// across this tree would make it undiffable against lmcgartland/airplay2-rs.
#![allow(dead_code)]
#![allow(unused_imports)]

pub mod cipher;
mod decoder;
mod encoder;
pub mod eq;
mod live_decoder;
mod rtp;
mod buffer;
mod streamer;
mod traits;

pub use decoder::{AudioDecoder, DecodedFrame};
pub use encoder::{AlacEncoder, AacEncoder, AudioEncoder, EncodedPacket, create_encoder};
pub use eq::{EqConfig, EqParams, Equalizer};
pub use live_decoder::{LiveAudioDecoder, LiveFrameSender, LivePcmFrame};
pub use rtp::{RtpPacket, RtpSender, RtpReceiver, RtpHeader, RetransmitRequest, build_retransmit_response};
pub use buffer::{AudioBuffer, AudioFrame};
pub use streamer::AudioStreamer;
pub use traits::{AudioSource, EncoderTrait};
