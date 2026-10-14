//! Run lifecycle: what a well-formed run looks like, its opening, and how it ends.

use super::*;

#[tokio::test]
async fn a_mock_run_is_a_well_formed_agui_run() {
    let events = run_turn(&MockDoor::echoing(), request("hello"), "t1", "r1", 1).await;
    assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text: String = events
        .iter()
        .filter(|event| event.event_type == EventType::TextMessageContent)
        .filter_map(|event| event.extra.get("delta").and_then(|d| d.as_str()))
        .collect();
    assert!(text.contains("hello"), "{text}");
}

/// Where a turn asks is said right after `RUN_STARTED`, in the opening's own write, so a replay
/// shows which source answered each turn of a thread. A resumed segment opens nothing, and says
/// nothing again: it asks where its start asked.
#[tokio::test]
async fn the_opening_says_where_the_turn_asks_in_the_same_write() {
    let proxy = ModelRequest {
        model: "gpt-5.5".to_string(),
        endpoint: Some(ModelEndpoint::Proxy {
            base_url: "http://127.0.0.1:8080".to_string(),
            auth: Some(("X-OpenCodex-API-Key".to_string(), "k".to_string())),
        }),
        ..request("hello")
    };
    let mac = ModelRequest {
        endpoint: Some(ModelEndpoint::Relay(crate::relay::RelayTo {
            broker: std::sync::Arc::default(),
            account: "acct-1".to_string(),
            run_id: "r1".to_string(),
            machines: vec!["mac-1".to_string()],
        })),
        ..proxy.clone()
    };
    for (asked, said) in [
        (
            request("hello"),
            serde_json::json!({"kind": "gateway", "model": "mock"}),
        ),
        (
            proxy,
            serde_json::json!({"kind": "local_proxy", "via": "loopback", "model": "gpt-5.5"}),
        ),
        (
            mac,
            serde_json::json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"}),
        ),
    ] {
        let journal = MemoryJournal::new();
        let events =
            run_conversation(&MockDoor::echoing(), None, &journal, asked, "t1", "r1", 1).await;
        assert_eq!(events[0].event_type, EventType::RunStarted);
        assert_eq!(events[1].event_type, EventType::Custom);
        assert_eq!(events[1].extra["name"], INFERENCE_SOURCE_NAME);
        assert_eq!(events[1].extra["value"], said);
        let opening = journal.batches().remove(0);
        assert_eq!(
            opening.iter().map(|e| e.event_type).collect::<Vec<_>>(),
            [EventType::RunStarted, EventType::Custom],
            "one write, the opening and where it asks"
        );
        assert!(
            !format!("{events:?}").contains("\"k\""),
            "the key is never a frame"
        );
    }

    let events = continue_interrupted(
        &MockDoor::echoing(),
        None,
        &MemoryJournal::new(),
        request("hello"),
        RunContext::new("t1", "r1", 1),
        3,
        Spent::default(),
        RunBudget::default(),
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::RunStarted
                || event.extra.get("name") == Some(&serde_json::json!(INFERENCE_SOURCE_NAME))),
        "a carry-on neither opens nor says it again: {events:?}"
    );
}

/// What a turn staged before the model was asked rides the request into the same opening write
/// as the inference source — so a replay paints it too, in first-seen order, before the first
/// tool call. A carry-on segment opens nothing, so it announces nothing either.
#[tokio::test]
async fn a_request_carrying_customs_opens_with_them() {
    let journal = MemoryJournal::new();
    let asked = ModelRequest {
        customs: vec![
            (
                "opengrok.officeDoc".to_string(),
                serde_json::json!({"docId": "ds_1", "path": "/home/bot/office/report.docx"}),
            ),
            ("anything.else".to_string(), serde_json::json!({"a": 1})),
        ],
        ..request("hello")
    };
    let events = run_conversation(&MockDoor::echoing(), None, &journal, asked, "t1", "r1", 1).await;
    let names: Vec<&str> = events
        .iter()
        .filter(|event| event.event_type == EventType::Custom)
        .filter_map(|event| event.extra["name"].as_str())
        .collect();
    assert_eq!(
        names[..3],
        [INFERENCE_SOURCE_NAME, "opengrok.officeDoc", "anything.else"],
        "the source first, then the staged frames in order: {names:?}"
    );
    let opening = journal.batches().remove(0);
    let journaled: Vec<&str> = opening
        .iter()
        .filter_map(|event| event.extra.get("name").and_then(|name| name.as_str()))
        .collect();
    assert!(
        journaled.contains(&"opengrok.officeDoc"),
        "journaled with the opening, so a replay repaints it: {journaled:?}"
    );
}

/// A reply of nothing but whitespace is not a reply. With or without a computer, it used to end
/// RUN_FINISHED with an empty bubble: the empty success (CLAUDE.md, three facts №3).
#[tokio::test]
async fn a_reply_of_only_whitespace_ends_as_an_error_that_says_so() {
    for tools in [None, Some(tool_runner())] {
        let door = MockDoor::with_script(vec![
            ModelDelta::Text("\n\n".to_string()),
            ModelDelta::Text("  ".to_string()),
        ]);
        let events = run_conversation(
            &door,
            tools.as_ref(),
            &MemoryJournal::new(),
            request("hello"),
            "t1",
            "r1",
            1,
        )
        .await;
        let ending = events.last().unwrap();
        assert_eq!(ending.event_type, EventType::RunError, "{events:?}");
        assert_eq!(
            ending.extra.get("message").unwrap(),
            "the model returned no text"
        );
    }
}

/// The failure that matters: the client still gets an ending, so its spinner stops.
#[tokio::test]
async fn a_broken_stream_still_ends_the_run() {
    let door = MockDoor::failing_with("upstream hung up");
    let events = run_turn(&door, request("hello"), "t1", "r1", 1).await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunError);
    assert!(
        events
            .last()
            .unwrap()
            .extra
            .get("message")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("upstream hung up")
    );
}

/// AN EMPTY SUCCESS IS THE DANGEROUS REPLY (CLAUDE.md, three facts №3). A round that produced
/// nothing — no words, no tool calls — used to end the run with `RUN_FINISHED` and an empty
/// transcript, which every client had to invent a reason for. It says the reason itself now.
#[tokio::test]
async fn a_run_that_produced_nothing_ends_as_an_error_that_says_so() {
    let events = run_conversation(
        &MockDoor::silent(),
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
    let ending = events.last().unwrap();
    assert_eq!(ending.event_type, EventType::RunError);
    assert_eq!(
        ending.extra.get("message").unwrap(),
        "the model returned no text"
    );
}

/// And a run that did produce something still ends cleanly: the new ending must not turn a
/// working turn into a failure.
#[tokio::test]
async fn a_run_that_said_something_still_finishes() {
    let events = run_conversation(
        &MockDoor::echoing(),
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// THE WHOLE CHAIN, JOINED. A model asks for a tool, the tool runs on the coworker's own
/// computer, and the result comes back as an event the client can render — all in one turn.
#[tokio::test]
async fn a_models_tool_call_runs_on_the_coworkers_computer() {
    use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand};
    use opengrok_core::id::{BoxId, CoworkerId};
    use opengrok_tools::{Executor, ToolContext};
    use opengrok_wire::agui::EventType;
    use std::sync::Arc;

    // A coworker with a computer of its own.
    let mut coworker = Coworker::default();
    for command in [
        CoworkerCommand::Hire {
            name: "Ada".to_string(),
            model: "m".to_string(),
            at_ms: 1,
        },
        CoworkerCommand::AssignComputer {
            box_id: BoxId::from_stored("box_ada"),
            mode: BoxMode::Dedicated,
            at_ms: 2,
        },
    ] {
        for event in coworker.decide(command).unwrap() {
            coworker.apply(&event);
        }
    }

    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let account = opengrok_core::id::AccountId::from_stored("acct_ada");
    let policy = opengrok_policy::Context {
        grant: Some(opengrok_policy::Grant {
            principal: account.clone(),
            coworker: CoworkerId::from_stored("cw_ada"),
            profile: opengrok_policy::ToolSet::All,
            needs_approval: opengrok_policy::ToolSet::None,
            revoked: false,
        }),
        ceiling: Some(opengrok_policy::Ceiling {
            coworker: CoworkerId::from_stored("cw_ada"),
            tools: opengrok_policy::ToolSet::All,
        }),
    };
    let runner = ToolRunner::new(
        Executor::with_policy(computer.clone(), policy),
        ToolContext::from_coworker(account, CoworkerId::from_stored("cw_ada"), &coworker),
    );

    // A model that asks to run a command — on somebody else's box, for good measure.
    let door = MockDoor::with_script(vec![
        ModelDelta::Text("let me check".to_string()),
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: r#"{"command":"whoami","box_id":"box_of_someone_else"}"#.to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);

    let events = run_turn_with_tools(&door, Some(&runner), request("go"), "t1", "r1", 1).await;

    let result = events
        .iter()
        .find(|event| event.event_type == EventType::ToolCallResult)
        .expect("the tool result should reach the client");
    assert_eq!(result.extra.get("toolCallId").unwrap(), "c1");
    assert_eq!(result.extra.get("ok").unwrap(), true);

    // The identity rule, end to end: the model named another box and got its own.
    assert_eq!(computer.last_box().as_deref(), Some("box_ada"));
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|v| v.as_str()) == Some("box-waking")),
        "a running box is not announced as waking"
    );
}

/// Without tools wired in, a tool call is still well-formed — it simply produces no result.
/// A sleeping box is woken by the first tool of the turn that needs it, and the stream says
/// so with one `box-waking` frame before that tool's result — once, even when the round has
/// two box-bound calls. A box already running gets no frame (the test above).
#[tokio::test]
async fn a_turn_says_it_is_waking_the_box_once_before_the_first_tool_that_needs_it() {
    use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand};
    use opengrok_core::id::{BoxId, CoworkerId};
    use opengrok_tools::{Executor, ToolContext};
    use opengrok_wire::agui::EventType;
    use std::sync::Arc;

    let mut coworker = Coworker::default();
    for command in [
        CoworkerCommand::Hire {
            name: "Ada".to_string(),
            model: "m".to_string(),
            at_ms: 1,
        },
        CoworkerCommand::AssignComputer {
            box_id: BoxId::from_stored("box_ada"),
            mode: BoxMode::Dedicated,
            at_ms: 2,
        },
    ] {
        for event in coworker.decide(command).unwrap() {
            coworker.apply(&event);
        }
    }

    // The probe, the executor and the wake each read the state once before the start lands.
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::sleeping(&[
        "exited", "exited", "exited", "running",
    ]));
    let account = opengrok_core::id::AccountId::from_stored("acct_ada");
    let policy = opengrok_policy::Context {
        grant: Some(opengrok_policy::Grant {
            principal: account.clone(),
            coworker: CoworkerId::from_stored("cw_ada"),
            profile: opengrok_policy::ToolSet::All,
            needs_approval: opengrok_policy::ToolSet::None,
            revoked: false,
        }),
        ceiling: Some(opengrok_policy::Ceiling {
            coworker: CoworkerId::from_stored("cw_ada"),
            tools: opengrok_policy::ToolSet::All,
        }),
    };
    let runner = ToolRunner::new(
        Executor::with_policy(computer.clone(), policy),
        ToolContext::from_coworker(account, CoworkerId::from_stored("cw_ada"), &coworker),
    );

    let door = MockDoor::with_script(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: r#"{"command":"whoami"}"#.to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
        ModelDelta::ToolCallStart {
            id: "c2".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c2".to_string(),
            delta: r#"{"command":"uptime"}"#.to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c2".to_string(),
        },
    ]);

    let events = run_turn_with_tools(&door, Some(&runner), request("go"), "t1", "r1", 1).await;

    let waking: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|v| v.as_str()) == Some("box-waking")
        })
        .map(|(index, _)| index)
        .collect();
    let first_result = events
        .iter()
        .position(|event| event.event_type == EventType::ToolCallResult)
        .expect("a tool result");
    assert_eq!(waking.len(), 1, "one waking frame per turn: {events:?}");
    assert!(
        waking[0] < first_result,
        "the frame comes before the first tool result"
    );
    assert_eq!(
        events[waking[0]]
            .extra
            .get("coworkerId")
            .and_then(|v| v.as_str()),
        Some("cw_ada")
    );
    assert_eq!(computer.resumes(), 1, "the box was started once");
    let results = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .count();
    assert_eq!(results, 2, "both commands ran after the one wake");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

#[tokio::test]
async fn a_run_without_a_tool_runner_still_ends_cleanly() {
    use opengrok_wire::agui::EventType;
    let door = MockDoor::with_script(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);
    let events = run_turn(&door, request("go"), "t1", "r1", 1).await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult)
    );
}

/// THE ORDERING RULE, ASSERTED. A journal that records when the model was called proves the
/// tool results were durable BEFORE the next call — the property a crash between rounds
/// depends on, and one that no test about client-visible events would ever notice breaking.
#[tokio::test]
async fn each_rounds_results_are_recorded_before_the_next_model_call() {
    use std::sync::{Arc, Mutex};

    /// Records journal writes and model calls on one timeline.
    #[derive(Default)]
    struct Timeline {
        entries: Mutex<Vec<String>>,
    }
    impl Timeline {
        fn note(&self, what: &str) {
            if let Ok(mut entries) = self.entries.lock() {
                entries.push(what.to_string());
            }
        }
        fn entries(&self) -> Vec<String> {
            self.entries.lock().map(|e| e.clone()).unwrap_or_default()
        }
    }

    struct WatchingJournal(Arc<Timeline>);
    #[async_trait::async_trait]
    impl RunJournal for WatchingJournal {
        async fn record(&self, _run_id: &str, events: &[Event]) -> Result<(), JournalError> {
            self.0.note(&format!("journal({})", events.len()));
            Ok(())
        }
    }

    /// Asks for a tool on the first call and simply answers on the second.
    struct TwoRoundDoor(Arc<Timeline>, Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for TwoRoundDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut calls = self
                    .1
                    .lock()
                    .map_err(|_| ModelError::Stream("the door's lock was poisoned".to_string()))?;
                *calls += 1;
                *calls
            };
            self.0.note(&format!("model call {round}"));
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
                vec![ModelDelta::Text("all done".to_string())]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let timeline = Arc::new(Timeline::default());
    let door = TwoRoundDoor(timeline.clone(), Mutex::new(0));
    let journal = WatchingJournal(timeline.clone());
    let runner = tool_runner();

    let events =
        run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    let entries = timeline.entries();
    let second_call = entries
        .iter()
        .position(|entry| entry == "model call 2")
        .expect("the model should have been called a second time");
    // At least one journal write must sit between the two calls: that is the tool results
    // reaching durable storage before the call that depends on them.
    let journals_before_second = entries[..second_call]
        .iter()
        .filter(|entry| entry.starts_with("journal("))
        .count();
    assert!(
        journals_before_second >= 2,
        "results must be durable before the next call; timeline was {entries:?}"
    );

    assert_eq!(
        events.last().unwrap().event_type,
        opengrok_wire::agui::EventType::RunFinished
    );
}
