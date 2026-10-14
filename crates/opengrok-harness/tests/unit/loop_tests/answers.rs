//! What the person sees: plan-only text, withheld preambles, intent openings, streamed answers.

use super::*;

/// Seen live (NativeChat Shot A): tools were offered, the model wrote a plan of the work
/// as text, and never started a call. Crossing the character bound ends the run with a
/// reason a person can read, instead of streaming the rest of the flood — including a tool
/// call that arrives only after it. What the model wrote up to the bound is shown as written
/// first: every exit flushes the withheld text (#178).
#[tokio::test]
async fn plan_only_text_with_tools_offered_and_no_call_ends_the_run() {
    struct PlanDoor;
    #[async_trait::async_trait]
    impl ModelDoor for PlanDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let mut script: Vec<ModelDelta> = "I'll check the host and then list the profiles. "
                .repeat(PLAN_ONLY_TEXT_LIMIT / 40)
                .split_inclusive(' ')
                .map(|word| ModelDelta::Text(word.to_string()))
                .collect();
            script.extend([
                ModelDelta::ToolCallStart {
                    id: "late".to_string(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "late".to_string(),
                },
            ]);
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let journal = MemoryJournal::new();
    let runner = tool_runner();
    assert!(
        !runner.tool_schemas().is_empty(),
        "this test is the tools-offered case"
    );
    let events = run_conversation(
        &PlanDoor,
        Some(&runner),
        &journal,
        request("list the forms"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallStart),
        "must stop before a tool call that arrives only after the flood: {events:?}"
    );
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    let message = last
        .extra
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("without starting any of it"),
        "the reason is a sentence, not engine-speak: {message}"
    );
    assert!(
        assistant_text(&events).starts_with("I'll check the host and then list the profiles."),
        "the words are flushed before the run closes: {events:?}"
    );
}

/// A stream that breaks after words that opened with intent shows those words before the
/// RUN_ERROR (#178, "any remaining exit flushes withheld text"). Filtered for intent they came
/// to nothing, and the person saw the error alone.
#[tokio::test]
async fn a_broken_stream_shows_the_words_it_withheld() {
    struct BreaksAfterWords;
    #[async_trait::async_trait]
    impl ModelDoor for BreaksAfterWords {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(ModelDelta::Text(
                    "I'll keep the old config and only change the port.".to_string(),
                )),
                Err(ModelError::Stream("upstream hung up".to_string())),
            ])))
        }
    }
    let events = run_conversation(
        &BreaksAfterWords,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("what would you change?"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunError);
    assert_eq!(
        assistant_text(&events),
        "I'll keep the old config and only change the port."
    );
}

/// The verifier's probe for #178: asked "what would you do? just tell me", a coworker with a
/// shell answers in sentences that each open with intent. Filtered for intent, that answer came
/// to nothing and the person saw only the RUN_ERROR.
#[tokio::test]
async fn an_answer_that_is_all_intent_is_shown_before_the_plan_bound_stops_it() {
    let answer = [
        "I'll install nginx from the distribution's packages. ",
        "I'll then request a certificate with certbot for your domain. ",
        "I'll point the server block at the app on port 8080. ",
        "I'll finish by reloading nginx and checking the site answers over https. ",
    ]
    .concat()
    .repeat(PLAN_ONLY_TEXT_LIMIT / 200);
    let door = MockDoor::with_script(
        answer
            .split_inclusive(' ')
            .map(|word| ModelDelta::Text(word.to_string()))
            .collect(),
    );
    let events = run_conversation(
        &door,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("what would you do to put my app online? just tell me"),
        "t1",
        "r1",
        1,
    )
    .await;
    let text = assistant_text(&events);
    assert!(text.starts_with("I'll install nginx"), "{events:?}");
    assert!(text.contains("request a certificate"), "{text:?}");
    assert!(text.chars().count() > PLAN_ONLY_TEXT_LIMIT, "{text:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunError);
}

/// A LONG ANSWER IS NOT A STALL. Any coworker with a computer answering "explain X" or a
/// routine's daily briefing past ~250 words used to end in RUN_ERROR "plan-only text", and
/// the withheld answer was never shown. Five thousand characters, #178's size.
#[tokio::test]
async fn a_long_answer_with_tools_offered_is_delivered_not_failed() {
    let answer = format!("{0} {0} {0}", long_answer());
    assert!(answer.chars().count() >= 5_000, "{}", answer.len());
    let events = run_conversation(
        &MockDoor::with_script(words(&answer)),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("explain the borrow checker"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), answer);
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::RunError),
        "{events:?}"
    );
}

/// #61, back for every coworker with a computer since 83f09fe: with a work tool offered every
/// text delta was withheld for the whole round, so the answer arrived as one burst.
#[tokio::test]
async fn a_coworker_with_a_computer_streams_its_answer() {
    let answer = long_answer();
    let events = run_conversation(
        &MockDoor::with_script(words(&answer)),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("explain the borrow checker"),
        "t1",
        "r1",
        1,
    )
    .await;

    let pieces = events
        .iter()
        .filter(|event| event.event_type == EventType::TextMessageContent)
        .count();
    assert!(pieces > 1, "one burst is not streaming: {pieces} piece(s)");
    assert_eq!(assistant_text(&events), answer);
}

/// A streamed answer after a failed tool is the answer. The failure fact is for a round that
/// said nothing of its own; painted after a real answer it reads as the conclusion.
#[tokio::test]
async fn a_streamed_answer_after_a_failed_tool_is_not_followed_by_the_failure() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = if round == 1 {
                vec![
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"cat notes.txt"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ]
            } else {
                words(&long_answer())
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        Arc::new(|call| opengrok_tools::ToolResult::refused(&call.id, "no such file: notes.txt")),
    );
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&runner),
        &MemoryJournal::new(),
        request("summarise my notes"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), long_answer());
}

/// And the pieces reach a live watcher while the model is still talking.
#[tokio::test]
async fn a_paced_answer_with_a_computer_reaches_the_sink_before_the_run_ends() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    struct FirstText(std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    #[async_trait::async_trait]
    impl EventSink for FirstText {
        async fn emit(&self, events: &[Event]) {
            if events
                .iter()
                .any(|event| event.event_type == EventType::TextMessageContent)
                && let Some(tx) = self.0.lock().ok().and_then(|mut slot| slot.take())
            {
                let _ = tx.send(());
            }
        }
    }

    let sink = FirstText(std::sync::Mutex::new(Some(tx)));
    let handle = tokio::spawn(async move {
        run_conversation_streaming(
            &MockDoor::with_script(words(&long_answer())).paced_by_ms(10),
            Some(&tool_runner()),
            &MemoryJournal::new(),
            request("explain the borrow checker"),
            "t1",
            "r1",
            1,
            &sink,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("first text should arrive before the run ends")
        .unwrap();
    assert!(
        !handle.is_finished(),
        "text arrived only after the turn finished — the answer came as one burst"
    );
    let events = handle.await.unwrap();
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// A file list with a closing offer, answered by a coworker with a computer, arrives as the
/// model wrote it — not as `README. md` on one flattened line.
#[tokio::test]
async fn a_markdown_answer_with_a_computer_arrives_as_written() {
    let reply = "Here are the files in your project:\n\n- README.md\n- src/main.rs\n\nLet me know if you want more.";
    let events = run_conversation(
        &MockDoor::with_script(words(reply)),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("list my files"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), reply);
}

/// Words the person saw before a tool ran are part of the conversation: the next call must
/// not be asked as if they were never said.
#[tokio::test]
async fn text_the_person_saw_reaches_the_next_request() {
    struct SpyDoor {
        round: Mutex<usize>,
        assistant: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ModelDoor for SpyDoor {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.round.lock().unwrap();
                *count += 1;
                *count
            };
            let script = if round == 1 {
                let mut script = words(&long_answer());
                script.extend([
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"cargo check"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ]);
                script
            } else {
                self.assistant.lock().unwrap().extend(
                    request
                        .messages
                        .iter()
                        .filter(|message| message.role == "assistant")
                        .map(|message| message.content.clone()),
                );
                vec![ModelDelta::Text("It builds.".to_string())]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let door = SpyDoor {
        round: Mutex::new(0),
        assistant: Mutex::new(Vec::new()),
    };
    let events = run_conversation(
        &door,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("explain, then check it builds"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let assistant = door.assistant.lock().unwrap();
    assert_eq!(assistant.as_slice(), [long_answer()], "{assistant:?}");
}

/// The bound is "tools offered and unused", not "the model wrote a lot". A coworker
/// with an empty toolbox may still answer at length.
#[tokio::test]
async fn plan_only_text_does_not_stop_when_no_tools_are_offered() {
    struct PlanDoor;
    #[async_trait::async_trait]
    impl ModelDoor for PlanDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let flood = "x".repeat(PLAN_ONLY_TEXT_LIMIT + 1);
            Ok(Box::pin(futures::stream::iter(
                vec![Ok(ModelDelta::Text(flood))].into_iter(),
            )))
        }
    }

    let events = run_conversation(
        &PlanDoor,
        None,
        &MemoryJournal::new(),
        request("explain at length"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Production AG-UI always attaches `bar_chart` and `form` (`chat_ui::attach`),
/// even when there is no computer. Those paint widgets must not trip the
/// plan-only stop: a long chat answer is still a chat answer.
#[tokio::test]
async fn plan_only_text_does_not_stop_when_only_paint_tools_are_offered() {
    struct PlanDoor;
    #[async_trait::async_trait]
    impl ModelDoor for PlanDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let flood = "x".repeat(PLAN_ONLY_TEXT_LIMIT + 1);
            Ok(Box::pin(futures::stream::iter(
                vec![Ok(ModelDelta::Text(flood))].into_iter(),
            )))
        }
    }

    let painted: LocalTool = Arc::new(|call| opengrok_tools::ToolResult::ok(&call.id, "painted"));
    let runner = ToolRunner::local_only()
        .with_local(
            serde_json::json!({
                "type": "function",
                "function": { "name": "bar_chart" }
            }),
            painted.clone(),
        )
        .with_local(
            serde_json::json!({
                "type": "function",
                "function": { "name": "form" }
            }),
            painted,
        );
    assert!(
        !work_tools_offered(&runner.tool_schemas()),
        "bar_chart/form are paint widgets, not work tools: {:?}",
        runner.tool_schemas()
    );

    let events = run_conversation(
        &PlanDoor,
        Some(&runner),
        &MemoryJournal::new(),
        request("explain at length"),
        "t1",
        "r1",
        1,
    )
    .await;

    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunFinished, "{last:?}");
}

/// A tool call, then a long answer, is work. The character bound must not fire on
/// the summary just because this round had no second ToolCallStart.
#[tokio::test]
async fn a_tool_call_then_text_is_not_plan_only() {
    struct ToolThenTextDoor(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for ToolThenTextDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = if round == 1 {
                vec![
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"ls"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ]
            } else {
                vec![ModelDelta::Text("x".repeat(PLAN_ONLY_TEXT_LIMIT + 1))]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let events = run_conversation(
        &ToolThenTextDoor(Mutex::new(0)),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("list files"),
        "t1",
        "r1",
        1,
    )
    .await;

    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunFinished, "{last:?}");
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallStart),
        "{events:?}"
    );
}

/// Resume starts a fresh converse_raw. The first half already ran a tool;
/// a long summary after HITL is work, not a plan-only flood.
#[tokio::test]
async fn a_resumed_run_does_not_treat_a_long_summary_as_plan_only() {
    struct SummaryDoor;
    #[async_trait::async_trait]
    impl ModelDoor for SummaryDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let flood = "x".repeat(PLAN_ONLY_TEXT_LIMIT + 1);
            Ok(Box::pin(futures::stream::iter(
                vec![Ok(ModelDelta::Text(flood))].into_iter(),
            )))
        }
    }

    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "ls"}),
    };
    let events = resume_conversation(
        &SummaryDoor,
        &tool_runner(),
        &MemoryJournal::new(),
        request("list files"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1),
    )
    .await;

    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunFinished, "{last:?}");
    assert!(
        !events.iter().any(|event| {
            event.event_type == EventType::RunError
                && event
                    .extra
                    .get("message")
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.contains("plan-only text"))
        }),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_short_reply_with_tools_offered_still_finishes() {
    let events = run_conversation(
        &MockDoor::echoing(),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    assert!(text.contains("hello"), "real answers still flush: {text:?}");
}

/// NativeChat Shot: "I'll probe…" then a work tool must not become chat. The
/// facts-first answer after the tool is still a TEXT_MESSAGE.
#[tokio::test]
async fn preamble_before_a_work_tool_is_not_chat_and_the_answer_still_is() {
    struct ProbeThenAnswer(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for ProbeThenAnswer {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = if round == 1 {
                vec![
                    ModelDelta::Text("I'll probe the BIR host".to_string()),
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"gpui-agent hello"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ]
            } else {
                vec![ModelDelta::Text(
                    "TIN 123-456-789. Forms: 1701, 2550M.".to_string(),
                )]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let events = run_conversation(
        &ProbeThenAnswer(Mutex::new(0)),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("list my BIR profile"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    assert!(
        !text.contains("I'll probe"),
        "intent preamble must not be user-visible chat: {text:?}"
    );
    assert!(
        text.contains("TIN 123-456-789"),
        "the facts-first answer after the tool still streams: {text:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallStart),
        "{events:?}"
    );
    let timing = run_timing_value(&events).expect("run-timing CUSTOM on a tool turn");
    assert_eq!(timing["tool_rounds"], 1);
    assert!(timing["tools"][0]["name"] == "shell");
    assert!(timing["model_ms"].as_array().map(|m| m.len()).unwrap_or(0) >= 2);
    assert!(timing["total_ms"].as_u64().is_some());
    assert!(timing["tool_wait_ms"].as_u64().is_some());
    assert_eq!(timing["auto_review_ms"], 0);
}

/// F8: withheld preamble must not grow the next hop as an assistant message.
#[tokio::test]
async fn withheld_preamble_is_not_appended_to_the_next_request() {
    struct SpyDoor {
        round: Mutex<usize>,
        assistant: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ModelDoor for SpyDoor {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.round.lock().unwrap();
                *count += 1;
                *count
            };
            if round == 2 {
                self.assistant.lock().unwrap().extend(
                    request
                        .messages
                        .iter()
                        .filter(|message| message.role == "assistant")
                        .map(|message| message.content.clone()),
                );
            }
            let script = if round == 1 {
                vec![
                    ModelDelta::Text("I'll probe the BIR host".to_string()),
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"gpui-agent hello"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ]
            } else {
                vec![ModelDelta::Text(
                    "TIN 123-456-789. Forms: 1701, 2550M.".to_string(),
                )]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let door = SpyDoor {
        round: Mutex::new(0),
        assistant: Mutex::new(Vec::new()),
    };
    let events = run_conversation(
        &door,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("list my BIR profile"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let assistant = door.assistant.lock().unwrap();
    assert!(
        assistant
            .iter()
            .all(|message| !message.contains("I'll probe")),
        "withheld preamble must not be replayed: {assistant:?}"
    );
}

/// Between-tool narration is the same bug one hop later.
#[tokio::test]
async fn between_tool_intent_text_is_not_chat() {
    struct TwoProbes(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for TwoProbes {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = match round {
                1 => vec![
                    ModelDelta::Text("I'll probe…".to_string()),
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"echo hello"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ],
                2 => vec![
                    ModelDelta::Text("I'll list the forms".to_string()),
                    ModelDelta::ToolCallStart {
                        id: "c2".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c2".to_string(),
                        delta: r#"{"command":"echo profiles"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c2".to_string(),
                    },
                ],
                _ => vec![ModelDelta::Text("deadline 15 April".to_string())],
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let events = run_conversation(
        &TwoProbes(Mutex::new(0)),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("forms and dues"),
        "t1",
        "r1",
        1,
    )
    .await;

    let text = assistant_text(&events);
    assert!(!text.contains("I'll probe"), "{text:?}");
    assert!(!text.contains("I'll list"), "{text:?}");
    assert!(text.contains("deadline 15 April"), "{text:?}");
    assert_eq!(run_timing_value(&events).expect("timing")["tool_rounds"], 2);
}

/// After a successful catalog listing, the next model request carries the harness
/// nudge so the hop answers instead of announcing another probe.
#[tokio::test]
async fn a_successful_listing_shell_nudges_the_next_hop_to_answer() {
    struct SpyDoor {
        round: Mutex<usize>,
        seen: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ModelDoor for SpyDoor {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.round.lock().unwrap();
                *count += 1;
                *count
            };
            if round == 2 {
                let blob = request
                    .messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.seen.lock().unwrap().push(blob);
            }
            let script = if round == 1 {
                vec![
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"gpui-agent invoke profile.list"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ]
            } else {
                vec![ModelDelta::Text("here are the files".to_string())]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let door = SpyDoor {
        round: Mutex::new(0),
        seen: Mutex::new(Vec::new()),
    };
    let (runner, _) = shell_runner(&[(
        "gpui-agent invoke profile.list",
        "Juan Dela Cruz\n[exit code 0]",
    )]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("list files"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let seen = door.seen.lock().unwrap().join("\n");
    assert!(
        seen.contains(intent::READONLY_SHELL_NUDGE),
        "second hop must see the listing nudge: {seen:?}"
    );
    assert!(assistant_text(&events).contains("here are the files"));
}

/// A turn whose whole reply is intent — it said what it would do and called nothing — shows
/// that reply. Dropping it finished the run with no text at all: the empty success (CLAUDE.md,
/// three facts №3), which the person blames on the app.
#[tokio::test]
async fn an_intent_only_reply_is_shown_rather_than_an_empty_success() {
    struct IntentDoor;
    #[async_trait::async_trait]
    impl ModelDoor for IntentDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            Ok(Box::pin(futures::stream::iter(
                vec![Ok(ModelDelta::Text(
                    "I'll pull the BIR profile and then look up dues.".to_string(),
                ))]
                .into_iter(),
            )))
        }
    }
    let events = run_conversation(
        &IntentDoor,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("list my profile"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(
        assistant_text(&events),
        "I'll pull the BIR profile and then look up dues."
    );
}

/// A round that calls no tool is the answer, and the answer is shown as the model wrote it
/// (#180). The intent filter is for words before a tool call; on a final answer it cut
/// "Let me explain." off an explanation and "I'll look up the TIN." off a BIR answer.
#[tokio::test]
async fn a_final_answer_that_opens_with_intent_is_shown_as_written() {
    for reply in [
        "Let me explain. Ownership moves a value; borrowing lends it.",
        "I'll look up the TIN.\n\nTIN 123-456-789. Forms: 1701.",
        "    let x = 1; // an indented code line keeps its indent",
    ] {
        let events = run_conversation(
            &MockDoor::with_script(words(reply)),
            Some(&tool_runner()),
            &MemoryJournal::new(),
            request("explain"),
            "t1",
            "r1",
            1,
        )
        .await;
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
        assert_eq!(assistant_text(&events), reply);
    }
}

/// A long answer goes live part way through; its opening sentence is part of it (#178).
#[tokio::test]
async fn a_long_answer_that_opens_with_intent_keeps_its_opening() {
    let answer = format!("Let me explain. {}", long_answer());
    let events = run_conversation(
        &MockDoor::with_script(words(&answer)),
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("explain the borrow checker"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), answer);
}

/// A non-empty answer is never swapped for the last failure fact. After a grep with no match,
/// "I'll need a different pattern: nothing in src mentions foo." reached the person as
/// "grep: no match" (verifier's probe).
#[tokio::test]
async fn a_final_answer_after_a_failed_command_is_not_swapped_for_the_failure() {
    let reply = "I'll need a different pattern: nothing in src mentions foo.";
    let door = Rounds::new(vec![shell_deltas("c1", "grep -rn foo src")], reply);
    let (runner, _) = shell_runner(&[("grep -rn foo src", "grep: no match\n[exit code 1]")]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("where is foo used"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), reply);
}
