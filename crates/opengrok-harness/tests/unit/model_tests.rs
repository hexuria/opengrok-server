use super::*;

fn refused(status: u16, kind: &str, message: &str, retry_after_s: Option<u64>) -> ModelError {
    ModelError::Refused {
        status,
        body: serde_json::json!({"type": "error", "error": {"type": kind, "message": message}})
            .to_string(),
        retry_after_s,
    }
}

/// #185: every one of these reached the chat as `the model gateway refused: <status> {…}`.
#[test]
fn a_refusal_reads_as_a_sentence_not_json() {
    let cases = [
        (
            refused(429, "rate_limit_error", "too many requests", Some(7)),
            "7 seconds",
        ),
        (refused(503, "at_capacity", "all seats busy", None), "busy"),
        (
            refused(503, "no_credential", "no credential for openai", None),
            "no provider credential",
        ),
        (
            refused(504, "stream_idle", "idle for 180s", None),
            "stopped answering",
        ),
        (
            refused(
                400,
                "upstream_error",
                "This model's maximum context length is 128000 tokens.",
                None,
            ),
            "maximum context length is 128000 tokens",
        ),
        (
            refused(401, "authentication_error", "invalid key", None),
            "key",
        ),
        (
            refused(500, "internal_error", "boom", None),
            "failed on its side",
        ),
    ];
    for (error, expected) in cases {
        let sentence = error.sentence();
        assert!(!sentence.contains('{'), "{sentence}");
        assert!(
            !sentence.starts_with("the model gateway refused:"),
            "{sentence}"
        );
        assert!(
            sentence.contains(expected),
            "{sentence} should say {expected}"
        );
    }
}

#[test]
fn an_html_error_page_is_not_repeated() {
    let error = ModelError::Refused {
        status: 404,
        body: "<html><body>nginx</body></html>".to_string(),
        retry_after_s: None,
    };
    assert_eq!(
        error.sentence(),
        "The model gateway could not take this request (404)."
    );
}

/// A refused connection and a short Retry-After are asked again; a request the gateway
/// could not take, a long wait and a key it refused are not.
#[test]
fn only_what_cannot_bill_twice_is_retried() {
    let unreachable = ModelError::Unreachable("connection refused".to_string());
    assert!(unreachable.retry_wait(0).is_some());
    assert!(unreachable.retry_wait(1).is_none());
    assert_eq!(
        refused(429, "rate_limit_error", "slow down", Some(2)).retry_wait(0),
        Some(std::time::Duration::from_secs(2))
    );
    assert!(
        refused(429, "rate_limit_error", "slow down", Some(2))
            .retry_wait(1)
            .is_none()
    );
    assert!(
        refused(429, "rate_limit_error", "slow down", Some(60))
            .retry_wait(0)
            .is_none()
    );
    assert!(
        refused(503, "at_capacity", "busy", Some(1))
            .retry_wait(0)
            .is_some()
    );
    assert!(
        refused(503, "no_credential", "none", Some(1))
            .retry_wait(0)
            .is_none()
    );
    assert!(
        refused(400, "invalid_request", "bad", Some(1))
            .retry_wait(0)
            .is_none()
    );
    assert!(
        ModelError::Stream("cut".to_string())
            .retry_wait(0)
            .is_none()
    );
}
