//! # airplay-rtsp
//!
//! RTSP protocol implementation for AirPlay 2.
//!
//! This crate provides:
//! - RTSP client connection management
//! - Request/response formatting
//! - Binary plist payload handling
//! - Session management
//! - Two-phase SETUP handling

// LOCAL: lint noise from upstream silenced here so it can't bury a real
// warning from the daemon (upstream API surface the daemon doesn't call). Fixing the ~50 sites
// across this tree would make it undiffable against lmcgartland/airplay2-rs.
#![allow(dead_code)]

mod connection;
mod request;
mod response;
mod session;
mod plist_codec;
pub mod raop_session;
pub mod sdp;
mod traits;

pub use connection::RtspConnection;
pub use raop_session::RaopSession;
pub use request::{RtspRequest, RtspMethod};
pub use response::RtspResponse;
pub use sdp::SdpBuilder;
pub use session::{RtspSession, SessionState};
pub use traits::RtspTransport;
