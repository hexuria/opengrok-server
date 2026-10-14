//! The bounds: turn caps, stalled streams, wall-clock wrap-ups, repeat refusals.

use super::*;

/// A model that never stops asking would otherwise run until the money ran out. At the cap the
/// run used to end in RUN_ERROR with the person told nothing about the work. Now it makes one
/// last call with no tools, asks for a summary, and finishes with it (#93).
#[tokio::test]
async fn a_model_that_never_stops_is_bounded_and_told_why() {
    #[derive(Default)]
    struct AlwaysToolDoor {
        calls: Mutex<Vec<ModelRequest>>,
    }
    #[async_trait::async_trait]
    impl ModelDoor for AlwaysToolDoor {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let script = if request.tools.is_empty() {
                vec![ModelDelta::Text(
                    "I ran `again` eight times and it never settled.".to_string(),
                )]
            } else {
                vec![
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"again"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ]
            };
            self.calls.lock().unwrap().push(request);
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let door = AlwaysToolDoor::default();
    let journal = MemoryJournal::new();
    let runner = tool_runner();
    let events =
        run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunFinished, "{last:?}");
    // The run finished, but at its limit, and the frame says which (#244).
    assert_eq!(last.extra.get("reason").unwrap(), "budget", "{last:?}");
    assert!(
        assistant_text(&events).contains("eight times"),
        "the wrap-up is the answer: {events:?}"
    );
    let calls = door.calls.lock().unwrap();
    assert_eq!(calls.len(), MAX_ROUNDS + 1, "the cap, then one wrap-up");
    let wrap_ups: Vec<_> = calls.iter().filter(|call| call.tools.is_empty()).collect();
    assert_eq!(wrap_ups.len(), 1, "exactly one call without tools");
    let nudge = &wrap_ups[0].messages.last().unwrap().content;
    assert!(
        nudge.starts_with("[harness]") && nudge.contains("limit"),
        "{nudge}"
    );
    let timing = run_timing_value(&events).expect("run-timing");
    assert!(
        timing["wrapped_up"]
            .as_str()
            .is_some_and(|why| why.contains("limit")),
        "the reason is on the run's own record: {timing}"
    );
    assert_eq!(timing["budget"]["max_rounds"], MAX_ROUNDS);
}

/// A wrap-up that cannot be had still ends the run, with the cap's own reason.
#[tokio::test]
async fn a_wrap_up_that_fails_still_ends_with_the_cap() {
    struct Door;
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            if request.tools.is_empty() {
                return Err(ModelError::Stream("the gateway went away".to_string()));
            }
            let script = vec![
                ModelDelta::ToolCallStart {
                    id: "c1".to_string(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: "c1".to_string(),
                    delta: r#"{"command":"again"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "c1".to_string(),
                },
            ];
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let events = run_conversation(
        &Door,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    let message = last.extra["message"].as_str().unwrap_or_default();
    assert!(message.contains("limit of 8 model calls"), "{message}");
}

/// A provider that sends a word and then goes quiet used to hold the run open for as long as
/// the process lived, renewing its lease the whole time.
#[tokio::test]
async fn a_stalled_model_stream_ends_with_a_reason() {
    struct Stalls;
    #[async_trait::async_trait]
    impl ModelDoor for Stalls {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            Ok(Box::pin(
                futures::stream::iter([Ok(ModelDelta::Text("hi".to_string()))])
                    .chain(futures::stream::pending()),
            ))
        }
    }
    let budget = RunBudget {
        idle_ms: 100,
        ..RunBudget::default()
    };
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_conversation_within(
            &Stalls,
            None,
            &MemoryJournal::new(),
            request("hello"),
            RunContext::new("t1", "r1", 1),
            budget,
            None,
        ),
    )
    .await
    .expect("the run ends on its own");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    let message = last.extra["message"].as_str().unwrap_or_default();
    assert!(message.contains("stopped answering"), "{message}");
}

/// A door that never opens is bounded too.
#[tokio::test]
async fn a_model_call_that_never_starts_ends_with_a_reason() {
    struct NeverOpens;
    #[async_trait::async_trait]
    impl ModelDoor for NeverOpens {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            futures::future::pending().await
        }
    }
    let budget = RunBudget {
        call_timeout_ms: 100,
        ..RunBudget::default()
    };
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_conversation_within(
            &NeverOpens,
            None,
            &MemoryJournal::new(),
            request("hello"),
            RunContext::new("t1", "r1", 1),
            budget,
            None,
        ),
    )
    .await
    .expect("the run ends on its own");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    let message = last.extra["message"].as_str().unwrap_or_default();
    assert!(message.contains("did not start answering"), "{message}");
}

/// A run that reached its cap with a target already open ends on the opened sentence, not a
/// wrap-up call, and it still says it stopped at its limit (#244, review of #265).
#[tokio::test]
async fn a_cap_reached_with_a_target_open_still_says_budget() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = shell_deltas(&format!("c{round}"), &format!("step {round}"));
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        Arc::new(|call| {
            let body = if call.arguments["command"] == "step 1" {
                "exit 0\n--- stdout ---\n{\"ok\":true,\"result\":{\"name\":\"Inbox\",\"view\":\"mail\"}}\n"
            } else {
                "exit 0\n--- stdout ---\ndone\n"
            };
            opengrok_tools::ToolResult::ok(&call.id, body)
        }),
    );
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&runner),
        &MemoryJournal::new(),
        request("keep going"),
        "t1",
        "r1",
        1,
    )
    .await;
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunFinished, "{last:?}");
    assert!(
        assistant_text(&events).contains("Inbox is selected on mail."),
        "{:?}",
        assistant_text(&events)
    );
    assert_eq!(last.extra.get("reason").unwrap(), "budget", "{last:?}");
}

/// Past its wall clock, a run stops starting work and wraps up.
#[tokio::test]
async fn a_run_past_its_wall_clock_wraps_up() {
    /// Works for as long as it is offered tools, and answers in words when it is not.
    struct Tireless;
    #[async_trait::async_trait]
    impl ModelDoor for Tireless {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let script = if request.tools.is_empty() {
                vec![ModelDelta::Text(
                    "Out of time after one command.".to_string(),
                )]
            } else {
                shell_deltas("c1", "sleep 1")
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let door = Tireless;
    let (_, ran) = shell_runner(&[]);
    let slow_ran = ran.clone();
    let slow = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        Arc::new(move |call| {
            slow_ran.lock().unwrap().push("sleep 1".to_string());
            std::thread::sleep(std::time::Duration::from_millis(30));
            opengrok_tools::ToolResult::ok(&call.id, "[exit code 0]")
        }),
    );
    let budget = RunBudget {
        max_wall_ms: 10,
        ..RunBudget::default()
    };
    let events = run_conversation_within(
        &door,
        Some(&slow),
        &MemoryJournal::new(),
        request("sleep twice"),
        RunContext::new("t1", "r1", 1),
        budget,
        None,
    )
    .await;
    assert_eq!(
        ran.lock().unwrap().len(),
        1,
        "the second command never starts"
    );
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunFinished);
    assert_eq!(last.extra.get("reason").unwrap(), "budget", "{last:?}");
    assert!(assistant_text(&events).contains("Out of time"));
    let timing = run_timing_value(&events).expect("run-timing");
    assert!(
        timing["wrapped_up"]
            .as_str()
            .is_some_and(|why| why.contains("time limit")),
        "{timing}"
    );
}

/// A Stop that lands at the cap wins over the wrap-up: the person asked for nothing more.
#[tokio::test]
async fn a_stop_at_the_cap_is_not_wrapped_up() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            *self.0.lock().unwrap() += 1;
            assert!(!request.tools.is_empty(), "no wrap-up after a stop");
            let script = vec![
                ModelDelta::ToolCallStart {
                    id: "c1".to_string(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: "c1".to_string(),
                    delta: r#"{"command":"again"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "c1".to_string(),
                },
            ];
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    // Two questions per round (the top of the round, before the tools), for MAX_ROUNDS rounds.
    let journal = StoppingJournal::saying_stop_after(2 * MAX_ROUNDS);
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&tool_runner()),
        &journal,
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert!(events.iter().any(is_run_stopped), "{events:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Seen live: a cheap model asked for `user_machine_shell` with no arguments, was refused,
/// and asked again identically until the round cap. The second identical refusal ends the
/// run with a reason, instead of six more model calls that change nothing.
#[tokio::test]
async fn a_call_refused_the_same_way_twice_ends_the_run() {
    struct ArgumentLessDoor;
    #[async_trait::async_trait]
    impl ModelDoor for ArgumentLessDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let script = vec![
                ModelDelta::ToolCallStart {
                    id: "c1".to_string(),
                    name: "user_machine_shell".to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "c1".to_string(),
                },
            ];
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let journal = MemoryJournal::new();
    let runner = tool_runner();
    let events = run_conversation(
        &ArgumentLessDoor,
        Some(&runner),
        &journal,
        request("run date on my computer"),
        "t1",
        "r1",
        1,
    )
    .await;

    let starts = events
        .iter()
        .filter(|event| event.event_type == opengrok_wire::agui::EventType::ToolCallStart)
        .count();
    assert_eq!(starts, 2, "two identical refusals, then stop: {events:?}");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, opengrok_wire::agui::EventType::RunError);
    let message = last
        .extra
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("user_machine_shell") && message.contains("twice"),
        "{message}"
    );
}
