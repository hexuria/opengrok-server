use super::*;

#[test]
fn timing_event_is_custom_run_timing_with_compact_value() {
    let mut projection = Projection::new("t1", "r1", 7);
    let _ = projection.start();
    let mut timing = TurnTiming::new();
    timing.record_model(12);
    timing.record_tools(
        vec![ToolPhase {
            name: "shell".into(),
            ms: 3,
        }],
        3,
        0,
    );
    let event = timing.event(&projection);
    assert_eq!(event.event_type, EventType::Custom);
    assert_eq!(
        event.extra.get("name").and_then(Value::as_str),
        Some(RUN_TIMING_NAME)
    );
    assert_eq!(
        event.extra.get("threadId").and_then(Value::as_str),
        Some("t1")
    );
    assert_eq!(event.extra.get("runId").and_then(Value::as_str), Some("r1"));
    let value = event.extra.get("value").cloned().unwrap();
    assert_eq!(value["model_ms"], json!([12]));
    assert_eq!(value["tools"][0]["name"], "shell");
    assert_eq!(value["tools"][0]["ms"], 3);
    assert_eq!(value["tool_wait_ms"], 3);
    assert_eq!(value["auto_review_ms"], 0);
    assert_eq!(value["tool_rounds"], 1);
    assert!(value["total_ms"].as_u64().is_some());
    assert_eq!(
        event.extra.get("tool_rounds").and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn splice_puts_timing_before_run_finished() {
    let mut events = vec![
        Event::new(EventType::TextMessageEnd, 1).with("messageId", "m1"),
        Event::new(EventType::RunFinished, 1).with("runId", "r1"),
    ];
    splice_before_run_end(
        &mut events,
        Event::new(EventType::Custom, 1).with("name", RUN_TIMING_NAME),
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            EventType::TextMessageEnd,
            EventType::Custom,
            EventType::RunFinished
        ]
    );
}

#[test]
fn splice_puts_timing_before_run_error() {
    let mut events = vec![Event::new(EventType::RunError, 1).with("message", "nope")];
    splice_before_run_end(
        &mut events,
        Event::new(EventType::Custom, 1).with("name", RUN_TIMING_NAME),
    );
    assert_eq!(events[0].event_type, EventType::Custom);
    assert_eq!(events[1].event_type, EventType::RunError);
}
