use super::*;

fn types(events: &[Event]) -> Vec<EventType> {
    events.iter().map(|event| event.event_type).collect()
}

fn run(deltas: Vec<ModelDelta>) -> Vec<Event> {
    let mut projection = Projection::new("t1", "r1", 100);
    let mut events = Vec::new();
    for delta in deltas {
        events.extend(projection.push(delta));
    }
    events.extend(projection.finish());
    events
}

#[test]
fn a_single_text_fragment_becomes_a_whole_message() {
    let events = run(vec![ModelDelta::Text("hello".to_string())]);
    assert_eq!(
        types(&events),
        vec![
            EventType::RunStarted,
            EventType::TextMessageStart,
            EventType::TextMessageContent,
            EventType::TextMessageEnd,
            EventType::RunFinished,
        ]
    );
}

/// The point of streaming: many fragments, one message — not one message per fragment.
#[test]
fn consecutive_text_fragments_share_one_message() {
    let events = run(vec![
        ModelDelta::Text("one ".to_string()),
        ModelDelta::Text("two ".to_string()),
        ModelDelta::Text("three".to_string()),
    ]);
    assert_eq!(
        types(&events),
        vec![
            EventType::RunStarted,
            EventType::TextMessageStart,
            EventType::TextMessageContent,
            EventType::TextMessageContent,
            EventType::TextMessageContent,
            EventType::TextMessageEnd,
            EventType::RunFinished,
        ]
    );
    let ids: Vec<_> = events
        .iter()
        .filter_map(|event| event.extra.get("messageId"))
        .collect();
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
}

/// A tool line inside an unterminated bubble is a rendering bug in every consumer.
#[test]
fn a_tool_call_closes_the_open_text_message_first() {
    let events = run(vec![
        ModelDelta::Text("thinking about it".to_string()),
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: "{\"cmd\":".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: "\"ls\"}".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);
    assert_eq!(
        types(&events),
        vec![
            EventType::RunStarted,
            EventType::TextMessageStart,
            EventType::TextMessageContent,
            EventType::TextMessageEnd,
            EventType::ToolCallStart,
            EventType::ToolCallArgs,
            EventType::ToolCallArgs,
            EventType::ToolCallEnd,
            EventType::RunFinished,
        ]
    );
}

/// Text after a tool call is a NEW message, not a resumption of the closed one.
#[test]
fn text_after_a_tool_call_opens_a_second_message() {
    let events = run(vec![
        ModelDelta::Text("before".to_string()),
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
        ModelDelta::Text("after".to_string()),
    ]);
    let ids: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == EventType::TextMessageStart)
        .filter_map(|event| event.extra.get("messageId").and_then(|id| id.as_str()))
        .collect();
    assert_eq!(ids.len(), 2, "two messages");
    assert_ne!(ids[0], ids[1], "and they must be told apart");
}

#[test]
fn reasoning_and_text_do_not_share_a_message() {
    let events = run(vec![
        ModelDelta::Reasoning("hmm".to_string()),
        ModelDelta::Text("answer".to_string()),
    ]);
    assert_eq!(
        types(&events),
        vec![
            EventType::RunStarted,
            EventType::ReasoningMessageStart,
            EventType::ReasoningMessageContent,
            EventType::ReasoningMessageEnd,
            EventType::TextMessageStart,
            EventType::TextMessageContent,
            EventType::TextMessageEnd,
            EventType::RunFinished,
        ]
    );
}

/// A stream that dies mid-sentence must still close its message and end its run, or the
/// consumer streams a bubble forever.
#[test]
fn a_failure_closes_what_is_open_and_still_ends_the_run() {
    let mut projection = Projection::new("t1", "r1", 100);
    let mut events = projection.push(ModelDelta::Text("half a sen".to_string()));
    events.extend(projection.fail("upstream hung up"));
    assert_eq!(
        types(&events),
        vec![
            EventType::RunStarted,
            EventType::TextMessageStart,
            EventType::TextMessageContent,
            EventType::TextMessageEnd,
            EventType::RunError,
        ]
    );
    let last = events.last().unwrap();
    assert_eq!(last.extra.get("message").unwrap(), "upstream hung up");
}

/// A run with nothing in it is still a run. An empty stream must not hang a client.
#[test]
fn an_empty_run_still_opens_and_closes() {
    let events = run(vec![]);
    assert_eq!(
        types(&events),
        vec![EventType::RunStarted, EventType::RunFinished]
    );
}

#[test]
fn finishing_twice_does_not_end_the_run_twice() {
    let mut projection = Projection::new("t1", "r1", 100);
    let first = projection.finish();
    let second = projection.finish();
    assert_eq!(first.len(), 2, "started + finished");
    assert!(second.is_empty(), "{second:?}");
}

/// Once a run has ended, a late error must not append a second ending.
#[test]
fn a_failure_after_finishing_is_ignored() {
    let mut projection = Projection::new("t1", "r1", 100);
    projection.finish();
    assert!(projection.fail("too late").is_empty());
}

/// An ending the log refused becomes exactly one `RUN_ERROR`: the card and the finish it
/// carried are gone, the message it had open is still closed.
#[test]
fn an_unrecorded_ending_is_one_run_error_that_keeps_its_brackets() {
    let mut projection = Projection::new("t1", "r1", 100);
    projection.push(ModelDelta::Text("waiting on you".to_string()));
    let mut refused = projection.awaiting_approval(
        &opengrok_tools::ToolCall {
            id: "c1".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::Value::Null,
        },
        opengrok_tools::AwaitingReason::ExecConsent,
        None,
    );
    refused.extend(projection.finish());
    let told = projection.unrecorded(refused, "the run could not be recorded: down");
    assert_eq!(
        types(&told),
        vec![EventType::TextMessageEnd, EventType::RunError],
        "{told:?}"
    );
}

/// A suspended run is neither finished nor failed, and must still be addable to.
#[test]
fn awaiting_approval_leaves_the_run_open() {
    let mut projection = Projection::new("t1", "r1", 100);
    projection.push(ModelDelta::Text("about to run something".to_string()));
    let waiting = projection.awaiting_approval(
        &opengrok_tools::ToolCall {
            id: "c1".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::Value::Null,
        },
        opengrok_tools::AwaitingReason::ExecConsent,
        None,
    );

    // The open message is closed, so nothing streams forever.
    assert!(types(&waiting).contains(&EventType::TextMessageEnd));
    assert_eq!(waiting.last().unwrap().event_type, EventType::Custom);
    assert_eq!(
        waiting.last().unwrap().extra.get("name").unwrap(),
        "run-awaiting-approval"
    );

    // And the run can still be finished later, when the answer arrives.
    let finished = projection.finish();
    assert_eq!(
        finished.last().unwrap().event_type,
        EventType::RunFinished,
        "a suspended run must still be finishable"
    );
}

#[test]
fn awaiting_a_user_form_drops_smuggled_values_from_the_run_log() {
    let mut projection = Projection::new("t1", "r1", 100);
    let waiting = projection.awaiting_approval(
        &opengrok_tools::ToolCall {
            id: "c1".to_string(),
            name: opengrok_tools::REQUEST_USER_FORM.to_string(),
            arguments: serde_json::json!({
                "title": "Sign in",
                "fields": [{ "id": "password", "label": "Password", "type": "password" }],
                "values": { "password": "s3cret-should-never-land" }
            }),
        },
        opengrok_tools::AwaitingReason::UserForm,
        Some("Waiting for you"),
    );
    let last = waiting.last().unwrap();
    let dumped = format!("{:?}", last.extra);
    assert!(!dumped.contains("s3cret-should-never-land"), "{dumped}");
    assert_eq!(
        last.extra.get("reason").and_then(|v| v.as_str()),
        Some("user-form")
    );
    assert!(
        last.extra
            .get("arguments")
            .and_then(|v| v.get("values"))
            .is_none(),
        "{dumped}"
    );
}

/// A stray end for a call that is not open must not close whatever is.
#[test]
fn an_unmatched_tool_call_end_does_not_close_an_open_message() {
    let mut projection = Projection::new("t1", "r1", 100);
    projection.push(ModelDelta::Text("open".to_string()));
    projection.push(ModelDelta::ToolCallEnd {
        id: "not-open".to_string(),
    });
    // The text message is still open, so finishing must close it.
    let tail = projection.finish();
    assert_eq!(
        types(&tail),
        vec![EventType::TextMessageEnd, EventType::RunFinished]
    );
}

/// Every run carries the client's ids on both ends, so a consumer can correlate them.
#[test]
fn the_run_is_bracketed_by_the_clients_ids() {
    let events = run(vec![ModelDelta::Text("x".to_string())]);
    for event in [events.first().unwrap(), events.last().unwrap()] {
        assert_eq!(event.extra.get("threadId").unwrap(), "t1");
        assert_eq!(event.extra.get("runId").unwrap(), "r1");
    }
}

#[test]
fn a_tool_result_with_a_picture_puts_it_on_the_frame() {
    let mut projection = Projection::new("t1", "r1", 100);
    let result = opengrok_tools::ToolResult::ok("c1", "screenshot attached").with_image(
        opengrok_tools::ToolImage {
            mime: "image/png".into(),
            base64: "AAAA".into(),
            width: 1280,
            height: 800,
            visibility: opengrok_tools::ImageVisibility::Agent,
        },
    );
    let events = projection.push_tool_result(&result, None);
    let frame = events
        .iter()
        .find(|event| event.event_type == EventType::ToolCallResult)
        .unwrap();
    let image = frame.extra.get("image").unwrap();
    assert_eq!(image["mime"], "image/png");
    assert_eq!(image["base64"], "AAAA");
    assert_eq!(
        (image["width"].as_u64(), image["height"].as_u64()),
        (Some(1280), Some(800))
    );
    assert_eq!(image["visibility"], "agent");

    let plain = projection.push_tool_result(&opengrok_tools::ToolResult::ok("c2", "done"), None);
    let frame = plain
        .iter()
        .find(|event| event.event_type == EventType::ToolCallResult)
        .unwrap();
    assert!(frame.extra.get("image").is_none());
}

/// A call's own time rides its result as an integer `durationMs` (#305), and a result nothing
/// timed carries no key at all: not 0, not `null`, which a client would paint as a time.
#[test]
fn a_tool_result_says_how_long_its_call_took_and_an_untimed_one_says_nothing() {
    let mut projection = Projection::new("t1", "r1", 100);
    let timed =
        projection.push_tool_result(&opengrok_tools::ToolResult::ok("c1", "done"), Some(1234));
    let frame = timed
        .iter()
        .find(|event| event.event_type == EventType::ToolCallResult)
        .unwrap();
    let wire = serde_json::to_value(frame).unwrap();
    assert_eq!(wire["durationMs"], serde_json::json!(1234), "{wire}");
    assert!(
        wire["durationMs"].is_u64(),
        "an integer count of milliseconds: {wire}"
    );
    assert_eq!(opengrok_wire::agui::DURATION_MS, "durationMs");

    let untimed = projection.push_tool_result(&opengrok_tools::ToolResult::ok("c2", "done"), None);
    let wire = serde_json::to_value(untimed.last().unwrap()).unwrap();
    assert_eq!(wire["type"], "TOOL_CALL_RESULT");
    assert!(wire.get("durationMs").is_none(), "{wire}");
}
