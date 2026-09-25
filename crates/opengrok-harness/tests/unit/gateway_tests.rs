use super::*;

/// A client that could not be built answers every call as an unreachable gateway. The old
/// fallback, `reqwest::Client::new()`, panics on the failure it was there to survive.
#[tokio::test]
async fn a_door_whose_client_did_not_build_is_unreachable_not_a_panic() {
    let door = GatewayDoor {
        base_url: "http://gateway.invalid".to_string(),
        key: "k".to_string(),
        http: Err("no TLS backend".to_string()),
        ready_seen: Mutex::new(None),
    };
    let error = door
        .stream(ModelRequest::default())
        .await
        .err()
        .expect("no client, no stream");
    assert!(matches!(error, ModelError::Unreachable(_)), "{error:?}");
    assert!(error.to_string().contains("no TLS backend"), "{error}");
    assert!(matches!(
        door.probe().await,
        Err(ModelError::Unreachable(_))
    ));
}

/// A gateway that answers every request with `status_line`, counting the requests.
async fn a_gateway_answering(status_line: &'static str) -> (String, Arc<Mutex<usize>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(Mutex::new(0usize));
    let counted = asked.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            *counted.lock().unwrap() += 1;
            let reply = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                 content-length: 11\r\nconnection: close\r\n\r\n{{\"data\":[]}}"
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (url, asked)
}

/// `/ready` asks through `ready`, and a tight prober must not become the gateway's traffic:
/// a second ask inside `READY_FOR` is answered from the first, refusal included. Boot's
/// `probe` always asks.
#[tokio::test]
async fn readiness_is_asked_of_the_gateway_once_per_window() {
    let (url, asked) = a_gateway_answering("200 OK").await;
    let door = GatewayDoor::new(url, "k");
    assert!(matches!(door.ready().await, Some(Ok(()))));
    assert!(matches!(door.ready().await, Some(Ok(()))));
    assert_eq!(*asked.lock().unwrap(), 1);
    assert!(door.probe().await.is_ok());
    assert_eq!(*asked.lock().unwrap(), 2);

    let (url, asked) = a_gateway_answering("401 Unauthorized").await;
    let door = GatewayDoor::new(url, "k");
    for _ in 0..3 {
        assert!(matches!(
            door.ready().await,
            Some(Err(ModelError::Refused { status: 401, .. }))
        ));
    }
    assert_eq!(*asked.lock().unwrap(), 1);
}

/// The internal gateway address is not the person's business. An unreachable gateway used
/// to print the reqwest error whole, URL and path included, into the chat.
#[tokio::test]
async fn an_unreachable_gateway_does_not_leak_its_url() {
    let door = GatewayDoor::new("http://127.0.0.1:1", "k");
    let error = door
        .stream(ModelRequest::default())
        .await
        .err()
        .expect("nothing listens on port 1");
    for text in [error.to_string(), error.sentence()] {
        assert!(!text.contains("127.0.0.1:1"), "{text}");
        assert!(!text.contains("/v1/chat/completions"), "{text}");
    }
    assert!(
        matches!(error, ModelError::Unreachable(_)),
        "a refused connection sent nothing, which is what makes it safe to retry: {error:?}"
    );
}

/// Answers every connection with one fixed HTTP response.
async fn answering(response: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 8192];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{address}"), task)
}

/// A 429 carries how long to wait; the door keeps it so the loop can honour it.
#[tokio::test]
async fn a_rate_limit_keeps_its_retry_after() {
    let (url, task) = answering(
        "HTTP/1.1 429 Too Many Requests\r\nretry-after: 3\r\ncontent-type: application/json\r\ncontent-length: 74\r\nconnection: close\r\n\r\n{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}",
    )
    .await;
    let error = GatewayDoor::new(url, "k")
        .stream(ModelRequest::default())
        .await
        .err()
        .expect("a 429 is not a stream");
    assert!(
        matches!(
            error,
            ModelError::Refused {
                status: 429,
                retry_after_s: Some(3),
                ..
            }
        ),
        "{error:?}"
    );
    task.abort();
}

/// A wrong OG_GATEWAY_TOKEN is found at boot, not on the first turn.
#[tokio::test]
async fn the_boot_probe_names_a_refused_key() {
    let (url, task) =
        answering("HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
    let probed = GatewayDoor::new(url, "oag_live_wrong").probe().await;
    assert!(
        matches!(probed, Err(ModelError::Refused { status: 401, .. })),
        "{probed:?}"
    );
    task.abort();
}

/// A probe of a gateway that accepts and never answers ends on its own clock, not on the
/// door's 200 s read timeout.
#[tokio::test]
async fn a_probe_of_a_hung_gateway_ends_on_its_own_clock() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            open.push(socket);
        }
    });
    let door = GatewayDoor::new(format!("http://{address}"), "k");
    let probed = tokio::time::timeout(PROBE_TIMEOUT * 2, door.ready())
        .await
        .expect("the probe has its own clock");
    assert!(
        matches!(probed, Some(Err(ModelError::TimedOut(_)))),
        "{probed:?}"
    );
    held.abort();
}

/// A gateway that accepts the connection and never answers — a hung process, a proxy with
/// nothing behind it — used to hold the call, and the run, for as long as the process lived.
#[tokio::test]
async fn a_gateway_that_accepts_and_never_answers_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            open.push(socket);
        }
    });
    let door = GatewayDoor::with_timeouts(
        format!("http://{address}"),
        "k",
        std::time::Duration::from_secs(1),
        std::time::Duration::from_millis(200),
    );
    let answered = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        door.stream(ModelRequest::default()),
    )
    .await
    .expect("the door gives up on its own");
    assert!(
        answered.is_err(),
        "a silent gateway is an error, not a stream"
    );
    held.abort();
}

/// WHAT MAY BE WRITTEN DOWN ABOUT A REFUSED KEY. The gateway records nothing at all for a
/// key it rejects, so our log is the only place that can ever name the credential a 401 was
/// about — but a log that carried the whole key would trade one problem for a worse one.
#[test]
fn a_logged_key_names_its_row_and_nothing_else() {
    // A real key: `oag_live_` plus seven characters is exactly `api_key.key_prefix` on the
    // gateway, which is the whole question a 401 is asking.
    let key = "oag_live_f69df82cafe1234567890abcdef";
    assert_eq!(logged_prefix(key), "oag_live_f69df82");

    // THE HALF THAT MATTERS: the secret does not travel. Asserted as "the tail is absent"
    // rather than "the head is right", because a prefix that silently grew to swallow the
    // whole key would still satisfy the equality above if that were the only check.
    assert!(
        !logged_prefix(key).contains("cafe1234567890abcdef"),
        "the secret tail reached the log: {}",
        logged_prefix(key)
    );
}

/// A key is opaque to us: we neither mint it nor validate its shape. A byte-index slice would
/// panic on a multi-byte character, and losing a turn to a logging call is an absurd way to
/// fail — so the rule is defined on characters and every odd input has to survive it.
#[test]
fn logging_a_strange_key_cannot_panic() {
    for odd in [
        "",
        "short",
        "oag_live_",
        "ключ-которого-не-бывает",
        "🔑🔑🔑",
    ] {
        let logged = logged_prefix(odd);
        assert!(
            logged.chars().count() <= KEY_PREFIX_LEN,
            "{odd:?} logged {} characters",
            logged.chars().count()
        );
        assert!(
            odd.starts_with(&logged),
            "{odd:?} -> {logged:?} is not a prefix"
        );
    }
}

#[test]
fn a_tool_call_frame_becomes_start_args_end() {
    let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#;
    assert_eq!(
        parse_sse_line(line).expect("a tool call frame is not an error"),
        vec![
            ModelDelta::ToolCallStart {
                id: "call_1".to_string(),
                name: "shell".to_string()
            },
            ModelDelta::ToolCallArgs {
                id: "call_1".to_string(),
                delta: "{\"command\":\"ls\"}".to_string()
            },
            ModelDelta::ToolCallEnd {
                id: "call_1".to_string()
            },
        ]
    );
}

/// OpenAI-shaped streams name the call on the first chunk (often with empty
/// `arguments`) and send the JSON on later chunks that have `index` but no `id`.
/// Closing the call on that first chunk is how Hexuria Ask cards showed no command.
#[test]
fn streamed_tool_call_arguments_are_assembled_across_chunks() {
    let mut parser = SseParser::default();
    assert_eq!(
        parser
            .push_line(
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"user_machine_shell","arguments":""}}]}}]}"#
            )
            .expect("a tool call frame is not an error"),
        vec![ModelDelta::ToolCallStart {
            id: "call_1".to_string(),
            name: "user_machine_shell".to_string()
        }]
    );
    assert_eq!(
        parser
            .push_line(
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"ls /Volumes/goldcoders\"}"}}]}}]}"#
            )
            .expect("an arguments frame is not an error"),
        vec![ModelDelta::ToolCallArgs {
            id: "call_1".to_string(),
            delta: "{\"command\":\"ls /Volumes/goldcoders\"}".to_string()
        }]
    );
    assert_eq!(
        parser
            .push_line(r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
            .expect("a finish frame is not an error"),
        vec![ModelDelta::ToolCallEnd {
            id: "call_1".to_string()
        }]
    );
}

#[test]
fn a_content_frame_becomes_a_text_delta() {
    let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
    assert_eq!(
        parse_sse_line(line).expect("a content frame is not an error"),
        vec![ModelDelta::Text("hello".to_string())]
    );
}

/// The sentinel is not JSON. Parsing it as JSON is the classic way to end a working stream
/// with a spurious error.
#[test]
fn the_done_sentinel_is_not_an_error() {
    assert!(
        parse_sse_line("data: [DONE]")
            .expect("a sentinel")
            .is_empty()
    );
}

#[test]
fn comments_and_blank_lines_are_ignored() {
    assert!(parse_sse_line(": ping").expect("a comment").is_empty());
    assert!(parse_sse_line("").expect("a blank line").is_empty());
    assert!(parse_sse_line("data: ").expect("an empty frame").is_empty());
}

/// AN EMPTY SUCCESS IS THE DANGEROUS REPLY (CLAUDE.md). A gateway that answers 200 and puts
/// the error in the body left the run with no deltas at all, which reached the person as the
/// coworker having nothing to say.
#[test]
fn an_error_frame_breaks_the_stream_instead_of_being_skipped() {
    let line = r#"data: {"error":{"message":"upstream provider is out of credit","type":"insufficient_quota"}}"#;
    let error = parse_sse_line(line).expect_err("an error frame is not deltas");
    assert!(
        matches!(&error, ModelError::Stream(message) if message == "upstream provider is out of credit"),
        "{error:?}"
    );
}

/// The other shape, and the same reasoning: a bare string under `error`.
#[test]
fn an_error_frame_with_a_bare_string_still_breaks_the_stream() {
    let error = parse_sse_line(r#"data: {"error":"model not found"}"#)
        .expect_err("an error frame is not deltas");
    assert!(
        matches!(&error, ModelError::Stream(message) if message == "model not found"),
        "{error:?}"
    );
}

/// A 200 whose body is not SSE at all reaches the parser as ordinary lines.
#[test]
fn an_error_body_that_is_not_sse_breaks_the_stream() {
    let error = parse_sse_line(r#"{"error":{"message":"bad request"}}"#)
        .expect_err("an error body is not deltas");
    assert!(
        matches!(&error, ModelError::Stream(message) if message == "bad request"),
        "{error:?}"
    );
}

/// One bad frame must not discard the reply that came before it.
#[test]
fn a_malformed_frame_is_skipped_rather_than_fatal() {
    assert!(
        parse_sse_line("data: {not json")
            .expect("a malformed frame")
            .is_empty()
    );
}

/// An empty content string is a keepalive, not a word — emitting it would open a message for
/// nothing.
#[test]
fn an_empty_content_delta_produces_nothing() {
    let line = r#"data: {"choices":[{"delta":{"content":""}}]}"#;
    assert!(parse_sse_line(line).expect("a keepalive").is_empty());
}

#[test]
fn a_frame_with_no_choices_produces_nothing() {
    assert!(
        parse_sse_line(r#"data: {"choices":[]}"#)
            .expect("a frame")
            .is_empty()
    );
    assert!(
        parse_sse_line(r#"data: {"id":"x","object":"chunk"}"#)
            .expect("a frame")
            .is_empty()
    );
}

#[test]
fn reasoning_arrives_before_the_content_of_the_same_frame() {
    let line = r#"data: {"choices":[{"delta":{"reasoning_content":"hmm","content":"answer"}}]}"#;
    assert_eq!(
        parse_sse_line(line).expect("a reasoning frame is not an error"),
        vec![
            ModelDelta::Reasoning("hmm".to_string()),
            ModelDelta::Text("answer".to_string()),
        ]
    );
}

/// A field a provider adds tomorrow must not break a run today.
#[test]
fn unknown_fields_do_not_break_a_frame() {
    let line = r#"data: {"choices":[{"delta":{"content":"hi","somethingNew":42}}],"extra":1}"#;
    assert_eq!(
        parse_sse_line(line).expect("an unknown field is not an error"),
        vec![ModelDelta::Text("hi".to_string())]
    );
}

fn pinned(scope: Option<&str>, actor: Option<&str>) -> Option<String> {
    conversation_pin(&ModelRequest {
        model: "m".to_string(),
        messages: Vec::new(),
        system: None,
        tools: Vec::new(),
        gateway_key: None,
        spend_scope: scope.map(str::to_string),
        spend_actor: actor.map(str::to_string),
    })
}

/// The three properties the gateway's tier-1 affinity actually depends on.
///
/// Stable, or it pins nothing and every turn picks a fresh credential. Distinct per
/// conversation, or two conversations share a credential and evict each other's prompt cache.
/// Absent when there is no pair, because falling back to the gateway's coarser per-caller tier
/// is the behaviour every request had before this existed, and is not a failure.
#[test]
fn the_conversation_pin_is_stable_distinct_and_optional() {
    let a = pinned(Some("cw-1"), Some("acct-1")).expect("a pair pins");
    assert_eq!(a, pinned(Some("cw-1"), Some("acct-1")).expect("stable"));

    // A shared coworker holds one transcript PER PERSON, so those are two conversations with
    // two prefixes; one pin between them would have each evicting the other's cache.
    assert_ne!(
        a,
        pinned(Some("cw-1"), Some("acct-2")).expect("other person")
    );
    assert_ne!(
        a,
        pinned(Some("cw-2"), Some("acct-1")).expect("other coworker")
    );

    // The separator earns its place: without it these two would hash identically.
    assert_ne!(pinned(Some("ab"), Some("c")), pinned(Some("a"), Some("bc")));

    assert_eq!(pinned(None, Some("acct-1")), None);
    assert_eq!(pinned(Some("cw-1"), None), None);

    // It leaves our boundary, so it must not carry the ids themselves.
    assert!(!a.contains("cw-1") && !a.contains("acct-1"), "{a}");
}

/// The key must not be printable, however it is logged.
#[test]
fn the_door_does_not_print_its_key() {
    let door = GatewayDoor::new("http://localhost:29080", "oag_live_secret");
    let printed = format!("{door:?}");
    assert!(!printed.contains("oag_live_secret"), "{printed}");
    assert!(printed.contains("<redacted>"), "{printed}");
}

#[test]
fn a_message_without_images_is_a_bare_string_on_the_wire() {
    let message = ChatMessage {
        role: "user".into(),
        content: "hello".into(),
        images: Vec::new(),
    };
    assert_eq!(message_content(&message), serde_json::json!("hello"));
}

#[test]
fn a_message_with_a_screenshot_is_text_then_image_url_parts() {
    let message = ChatMessage {
        role: "user".into(),
        content: "[tool c1 result] screenshot attached".into(),
        images: vec![crate::ImagePart {
            mime: "image/png".into(),
            base64: "AAAA".into(),
        }],
    };
    let parts = message_content(&message);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "[tool c1 result] screenshot attached");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
}
