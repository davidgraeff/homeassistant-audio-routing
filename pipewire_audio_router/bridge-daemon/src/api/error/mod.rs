//! One way to fail, and one way to say a write worked.
//!
//! ## The rule
//!
//! **The HTTP status carries success. A failure body says why, typed.** A 2xx means it
//! happened; anything else carries `{kind, message}` where `kind` is machine-readable and
//! `message` is the sentence a user reads.
//!
//! ## What it replaces, and why that mattered
//!
//! Most of this API used to answer `{ok, message}` with the status *and* the flag both
//! claiming to carry success — and they disagreed. `POST /api/sendspin/clear` on a
//! disconnected device answered `200 {ok:false}`; `PUT /api/pwsink/volume` with no agent
//! answered `503 {ok:false}`; a bad output name answered `400 {ok:false}`. So every
//! consumer had to check both, and both did: the web UI's `run()` inspected `ok` *after*
//! `fetch` resolved, and the HA integration raised on either. The rule was unwritten and
//! inconsistently applied, which is the worst of the three possible states.
//!
//! The second half was worse for anything that wanted to *react*: `message` was the only
//! carrier of why, so telling "no agent connected" from "unknown output" meant matching
//! prose. `kind` exists for that.
//!
//! ## The vocabulary
//!
//! Deliberately small, and about the *caller's* situation rather than the daemon's
//! internals — five kinds cover every write in this API. The alignment subsystem keeps its
//! own richer vocabulary in [`crate::align::measure::Refusal`] (which serialises to the
//! same `{kind, message, …}` envelope, so a consumer has one shape): its kinds name states
//! a user can act on, like a lost microphone or an estimator that refused.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Why a write did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKind {
    /// No such thing (an unknown node name, source id, hold, group).
    NotFound,
    /// The request itself is wrong: a value out of range, a kind that has no such knob,
    /// a body that does not make sense together.
    BadRequest,
    /// The request is fine but the target is not in a state where it applies — a device
    /// with no live connection to clear, a receiver with no session to rebuild, a session
    /// somebody else owns. **This is the one that used to be `200 {ok:false}`.**
    Conflict,
    /// A far end this daemon depends on is not answering: a PipeWire host with no agent,
    /// the PipeWire thread gone. Retryable in principle, and not the caller's fault.
    Unavailable,
    /// This daemon broke: a store that could not be persisted, a lock that is poisoned.
    Internal,
}

impl ErrorKind {
    fn status(self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Conflict => StatusCode::CONFLICT,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A refused write: the status comes from [`ErrorKind`], the body is `{kind, message}`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApiError {
    pub(crate) kind: ErrorKind,
    pub(crate) message: String,
}

impl ApiError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, message)
    }

    /// For "there was nothing to act on": no live connection, no session, no hold. The
    /// old shape reported these as `200 {ok:false}`, which is a success a consumer had to
    /// read a flag to disbelieve.
    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.kind.status(), Json(self)).into_response()
    }
}

/// A write that happened, with the sentence the UI shows.
///
/// `message` and nothing else: there is no `ok` field, because the status already said so
/// and two carriers for one fact is how they come to disagree.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OpOk {
    pub(crate) message: String,
}

/// Every write in this API answers this: `Ok` → `200 {message}`, `Err` → the refusal's
/// own status with `{kind, message}`.
pub(crate) type OpResult = Result<Json<OpOk>, ApiError>;

/// `Ok` with a message — the common tail of a handler.
pub(crate) fn ok(message: impl Into<String>) -> OpResult {
    Ok(Json(OpOk { message: message.into() }))
}

/// `Ok` when it happened, [`ErrorKind::Conflict`] when there was nothing to act on, with
/// the same sentence either way.
///
/// The shape of every "tell this device something" handler: a device that is not connected
/// is not an error in the caller's request, but it is not a success either — and it used
/// to be reported as one.
pub(crate) fn ok_if(happened: bool, message: impl Into<String>) -> OpResult {
    let message = message.into();
    if happened {
        ok(message)
    } else {
        Err(ApiError::conflict(message))
    }
}

#[cfg(test)]
mod tests;
