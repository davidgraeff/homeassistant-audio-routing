//! The convention itself: a status that agrees with the body, and a `kind` a consumer can
//! branch on without reading prose.

use super::*;
use axum::body::to_bytes;

async fn body_of(r: Response) -> (StatusCode, serde_json::Value) {
    let status = r.status();
    let bytes = to_bytes(r.into_body(), 64 * 1024).await.expect("a body");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}

#[tokio::test]
async fn every_kind_maps_to_its_status_and_says_which_it_was() {
    for (kind, expected) in [
        (ErrorKind::NotFound, StatusCode::NOT_FOUND),
        (ErrorKind::BadRequest, StatusCode::BAD_REQUEST),
        (ErrorKind::Conflict, StatusCode::CONFLICT),
        (ErrorKind::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
        (ErrorKind::Internal, StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let (status, body) = body_of(ApiError::new(kind, "because").into_response()).await;
        assert_eq!(status, expected, "{kind:?}");
        assert_eq!(body["message"], "because");
        // The wire name is the contract with two consumers, so it is asserted as a string
        // rather than round-tripped through the enum.
        assert!(body["kind"].is_string(), "{body}");
        // And no `ok`: the status carries that, and two carriers for one fact is how they
        // come to disagree.
        assert!(body.get("ok").is_none(), "{body}");
    }
    let (_, body) = body_of(ApiError::conflict("nothing to clear").into_response()).await;
    assert_eq!(body["kind"], "conflict");
}

/// The case the whole convention is about: "there was nothing to act on" used to be
/// `200 {ok:false}` — a success a consumer had to read a flag to disbelieve.
#[tokio::test]
async fn nothing_to_act_on_is_a_conflict_not_a_200() {
    let happened = ok_if(true, "cleared 'Kitchen'").expect("a success");
    assert_eq!(happened.0.message, "cleared 'Kitchen'");

    let did_not = ok_if(false, "'Kitchen' has no live connection").expect_err("a refusal");
    assert_eq!(did_not.kind, ErrorKind::Conflict);
    let (status, body) = body_of(did_not.into_response()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["message"], "'Kitchen' has no live connection");
}

#[tokio::test]
async fn a_success_body_is_just_the_message() {
    let (status, body) = body_of(ok("set 'Kitchen' to 42%").expect("ok").into_response()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "message": "set 'Kitchen' to 42%" }));
}
