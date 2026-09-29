#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `gateway_errors_as_json` on the bodies a router cannot see coming: every plain-text 502, 503
//! and 504 leaves as a sentence, even one too long to read (review of #262).

use super::{GATEWAY_ERROR_MAX, gateway_errors_as_json};
use axum::http::{StatusCode, header};
use tower::ServiceExt;

async fn through_the_layer(body: String) -> (StatusCode, Option<String>, serde_json::Value) {
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(move || {
                let body = body.clone();
                async move { (StatusCode::SERVICE_UNAVAILABLE, body) }
            }),
        )
        .layer(axum::middleware::from_fn(gateway_errors_as_json));
    let response = app
        .oneshot(
            axum::http::Request::get("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .map(|value| value.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, length, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn a_sentence_is_wrapped_as_the_error() {
    let (status, _, body) = through_the_layer("the store did not answer".to_string()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        serde_json::json!({ "error": "the store did not answer" })
    );
}

#[tokio::test]
async fn a_body_past_the_cap_is_still_a_sentence_and_not_an_empty_503() {
    let (status, length, body) = through_the_layer("x".repeat(GATEWAY_ERROR_MAX + 1)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let sentence = body["error"].as_str().expect("an error sentence");
    assert!(!sentence.is_empty() && sentence.len() < 200, "{sentence}");
    // The handler's length was for the body this layer threw away; any length left must be
    // this body's.
    if let Some(length) = length {
        assert_eq!(length.parse::<usize>().unwrap(), body.to_string().len());
    }
}

/// The longest sentence the server writes today: a from-tape refusal at the 64 KiB it reads,
/// plus the promise after it.
#[tokio::test]
async fn a_long_from_tape_refusal_keeps_its_words() {
    let long = format!("{} and the tape is kept", "y".repeat(64 * 1024));
    let (_, _, body) = through_the_layer(long.clone()).await;
    assert_eq!(body["error"], long);
}
