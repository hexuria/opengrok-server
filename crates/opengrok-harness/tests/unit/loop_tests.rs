use super::*;
use opengrok_tools::Executor;
use opengrok_wire::agui::EventType;
use std::sync::{Arc, Mutex};

fn tool_runner() -> ToolRunner {
    tool_runner_on(
        Arc::new(crate::tools::tests_support::RecordingComputer::default()),
        |executor| executor,
    )
}

fn tool_runner_with(shape: impl FnOnce(Executor) -> Executor) -> ToolRunner {
    tool_runner_on(
        Arc::new(crate::tools::tests_support::RecordingComputer::default()),
        shape,
    )
}

/// Ada's runner on any computer, with the executor shaped by the caller (a screen, a sink).
fn tool_runner_on(
    computer: Arc<dyn opengrok_box::Computer>,
    shape: impl FnOnce(Executor) -> Executor,
) -> ToolRunner {
    use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand};
    use opengrok_core::id::{BoxId, CoworkerId};
    use opengrok_tools::{Executor, ToolContext};

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
    // A permissive policy: these tests are about the loop, not about policy, and an executor
    // built without one now refuses everything by design.
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
    ToolRunner::new(
        shape(Executor::with_policy(computer, policy)),
        ToolContext::from_coworker(account, CoworkerId::from_stored("cw_ada"), &coworker),
    )
}

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        gateway_key: None,
        spend_scope: None,
        spend_actor: None,
        model: "mock".to_string(),
        system: None,
        tools: Vec::new(),
        messages: vec![ChatMessage {
            images: Vec::new(),
            role: "user".to_string(),
            content: text.to_string(),
        }],
    }
}

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

/// A journal that reports the run stopped from the `stop_after`-th question onwards, and keeps
/// what it was asked to record so the ordering can be asserted.
struct StoppingJournal {
    stop_after: usize,
    asked: Mutex<usize>,
    batches: Mutex<Vec<Vec<Event>>>,
}

impl StoppingJournal {
    fn saying_stop_after(questions: usize) -> Self {
        Self {
            stop_after: questions,
            asked: Mutex::new(0),
            batches: Mutex::new(Vec::new()),
        }
    }

    fn batches(&self) -> Vec<Vec<Event>> {
        self.batches.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl RunJournal for StoppingJournal {
    async fn record(&self, _run_id: &str, events: &[Event]) -> Result<(), JournalError> {
        if let Ok(mut batches) = self.batches.lock() {
            batches.push(events.to_vec());
        }
        Ok(())
    }

    async fn stopped(&self, _run_id: &str) -> bool {
        let Ok(mut asked) = self.asked.lock() else {
            return false;
        };
        *asked += 1;
        *asked > self.stop_after
    }
}

/// Asks for a tool on every call, and counts how many times it was called.
struct CountingToolDoor(Arc<Mutex<usize>>);

#[async_trait::async_trait]
impl ModelDoor for CountingToolDoor {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        if let Ok(mut calls) = self.0.lock() {
            *calls += 1;
        }
        let script = vec![
            ModelDelta::Text("playing it again".to_string()),
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: r#"{"command":"play the recipe"}"#.to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// A STOP IS THE ONLY WAY OUT OF A BOT IN A LOOP, so the first thing it has to buy is that the
/// model is not asked again. The run ends as a stop — `run-stopped` and then `RUN_FINISHED`,
/// never `RUN_ERROR` — because a person changing their mind is not a coworker failing.
#[tokio::test]
async fn a_stopped_run_asks_the_model_nothing_further_and_ends_as_a_stop() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    let journal = StoppingJournal::saying_stop_after(0);
    let runner = tool_runner();

    let events =
        run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "a run stopped before its first round must not spend a model call"
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|name| name.as_str()) == Some("run-stopped")),
        "the reason travels as its own frame, or a client cannot tell a stop from a finish: \
         {events:?}"
    );
    assert_eq!(
        events.last().unwrap().event_type,
        EventType::RunFinished,
        "the stream still closes, or the client holds its spinner open forever"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::RunError),
        "a stop is not a failure: {events:?}"
    );
}

/// THE PLACE THAT ACTUALLY STOPS THE YOUTUBE SEARCH HAPPENING ONE MORE TIME. The model has
/// answered and asked to play the recipe again; the check sits between that ask and the doing,
/// so the tool never runs. And the frames of the round it was in the middle of are journaled
/// WITH the ending, so the transcript shows what the coworker was about to do rather than
/// ending a step short of it.
#[tokio::test]
async fn a_stop_lands_between_the_model_asking_for_a_tool_and_the_tool_running() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    // Not stopped when the round opens; stopped by the time the tool is about to run.
    let journal = StoppingJournal::saying_stop_after(1);
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);

    let events =
        run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "the round that was already open still gets its answer; nothing after it is asked"
    );
    assert_eq!(
        computer.last_box(),
        None,
        "the tool the model asked for must not run: that is the repetition the person pressed \
         stop to end"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult),
        "and no result is invented for a call that never happened: {events:?}"
    );

    // WHAT WAS SPENT IS ACCOUNTED FOR, AND THIS IS WHERE THAT IS VISIBLE. The model call that
    // was already open is read to its last delta rather than dropped — `TOOL_CALL_END` is the
    // final thing the door yields, so seeing it means the stream was drained. The gateway
    // records a call's usage when the call completes; abandoning a half-read stream would
    // leave tokens spent at the provider and missing from the meter, which is exactly the
    // spend landing on the floor.
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallEnd),
        "the model call already in flight is drained, not abandoned: {events:?}"
    );

    // The round's own frames and the ending in one write: the transcript keeps the
    // call it asked for, then the stop. Intent preamble ("playing it again") is
    // withheld because a work tool followed — NativeChat would paint it as chat.
    let last = journal.batches().pop().expect("a final journal batch");
    assert!(
        last.iter()
            .any(|event| event.event_type == EventType::ToolCallEnd),
        "the call of the round in progress goes down with the stop: {last:?}"
    );
    assert!(
        !last
            .iter()
            .any(|event| event.event_type == EventType::TextMessageContent),
        "intent preamble is withheld, not journaled as chat: {last:?}"
    );
    assert!(
        last.iter()
            .any(|event| event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|name| name.as_str()) == Some("run-stopped")),
        "{last:?}"
    );
}

fn is_run_stopped(event: &Event) -> bool {
    event.event_type == EventType::Custom
        && event.extra.get("name").and_then(|name| name.as_str()) == Some("run-stopped")
}

/// THE CLOSE IS A STEP BOUNDARY TOO (`formal/tla/HarnessLoop.tla` StopIsHonoured). TLC's
/// trace: the round opens unstopped, the person presses Stop while the model is answering in
/// words, the answer ends the run. It used to end as RUN_FINISHED — the person was told the
/// coworker finished when they had stopped it.
#[tokio::test]
async fn a_stop_pressed_during_the_final_answer_ends_the_run_as_a_stop() {
    // Not stopped when the round opens; stopped by the time the answer closes it.
    let journal = StoppingJournal::saying_stop_after(1);

    let events = run_conversation(
        &MockDoor::echoing(),
        None,
        &journal,
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert!(
        events.iter().any(is_run_stopped),
        "a stop recorded before the run ended is how it ends: {events:?}"
    );
    assert!(
        assistant_text(&events).contains("hello"),
        "what the model already said is kept, not withdrawn: {events:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let endings = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                EventType::RunFinished | EventType::RunError
            )
        })
        .count();
    assert_eq!(endings, 1, "still exactly one ending: {events:?}");
}

/// A STOP PRESSED WHILE THE CARD WAS UP WINS OVER THE ANSWER (`formal/tla/RunLifecycle.tla`
/// NoApprovedAfterStop). The answer is appended, then the Stop, then the continuation starts:
/// the approved call must not run, and the model must not be asked anything.
#[tokio::test]
async fn a_run_stopped_after_its_card_was_answered_does_not_run_the_approved_call() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    let journal = StoppingJournal::saying_stop_after(0);
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "play the recipe"}),
    };

    let events = resume_conversation(
        &door,
        &runner,
        &journal,
        request("go"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1),
    )
    .await;

    assert_eq!(computer.last_box(), None, "the approved call did not run");
    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "and the model was not asked again"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult),
        "no result is invented for a call that never ran: {events:?}"
    );
    assert!(events.iter().any(is_run_stopped), "{events:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(
        journal.batches().concat().iter().any(is_run_stopped),
        "the stop is journaled, not only shown"
    );
}

/// A REFUSAL NEVER REACHES THE EXECUTOR (`formal/lean/Harness.lean` Close.runsApproved): the
/// person said no, so the model reads the refusal and the box is not touched.
#[tokio::test]
async fn a_refused_card_is_read_by_the_model_and_never_runs() {
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "rm -rf build"}),
    };

    let events = resume_conversation(
        &MockDoor::echoing(),
        &runner,
        &MemoryJournal::new(),
        request("clean up"),
        RunContext::new("t1", "r1", 1),
        Resumption::refused(call, 1, "the person said no"),
    )
    .await;

    assert_eq!(computer.last_box(), None, "a refused call does not run");
    let result = events
        .iter()
        .find(|event| event.event_type == EventType::ToolCallResult)
        .expect("the refusal is written where the result would have gone");
    assert_eq!(result.extra.get("ok"), Some(&serde_json::json!(false)));
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// A model that never stops asking would otherwise run until the money ran out. The bound ends
/// the run as a result the client can see, not a silent stop.
#[tokio::test]
async fn a_model_that_never_stops_is_bounded_and_told_why() {
    struct AlwaysToolDoor;
    #[async_trait::async_trait]
    impl ModelDoor for AlwaysToolDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
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

    let journal = MemoryJournal::new();
    let runner = tool_runner();
    let events = run_conversation(
        &AlwaysToolDoor,
        Some(&runner),
        &journal,
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;

    let last = events.last().unwrap();
    assert_eq!(last.event_type, opengrok_wire::agui::EventType::RunError);
    assert!(
        last.extra
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .contains("limit"),
        "{last:?}"
    );
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

/// Seen live (NativeChat Shot A): tools were offered, the model wrote a plan of the work
/// as text, and never started a call. Crossing the character bound ends the run with a
/// reason, instead of streaming the rest of the flood — including a tool call that
/// arrives only after it.
#[tokio::test]
async fn plan_only_text_with_tools_offered_and_no_call_ends_the_run() {
    struct PlanDoor;
    #[async_trait::async_trait]
    impl ModelDoor for PlanDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let flood = "x".repeat(PLAN_ONLY_TEXT_LIMIT + 1);
            let script = vec![
                ModelDelta::Text(flood),
                ModelDelta::Text(" and then I will call the tool.".to_string()),
                ModelDelta::ToolCallStart {
                    id: "late".to_string(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "late".to_string(),
                },
            ];
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
    assert!(message.contains("plan-only text"), "{message}");
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

fn assistant_text(events: &[Event]) -> String {
    events
        .iter()
        .filter(|event| event.event_type == EventType::TextMessageContent)
        .filter_map(|event| event.extra.get("delta").and_then(|d| d.as_str()))
        .collect()
}

fn run_timing_value(events: &[Event]) -> Option<&serde_json::Value> {
    events.iter().rev().find_map(|event| {
        (event.event_type == EventType::Custom
            && event.extra.get("name").and_then(|n| n.as_str()) == Some(RUN_TIMING_NAME))
        .then(|| event.extra.get("value"))
        .flatten()
    })
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

/// After a successful listing shell, the next model request carries the harness
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
                        delta: r#"{"command":"ls"}"#.to_string(),
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
    let events = run_conversation(
        &door,
        Some(&tool_runner()),
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

#[tokio::test]
async fn intent_only_text_with_tools_offered_is_not_chat() {
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
    let text = assistant_text(&events);
    assert!(
        !text.to_ascii_lowercase().contains("i'll pull"),
        "intent-only rounds must not become bubbles: {text:?}"
    );
    assert!(!text.contains("look up"), "{text:?}");
}

#[tokio::test]
async fn mixed_intent_then_facts_flushes_only_the_facts() {
    struct MixedDoor;
    #[async_trait::async_trait]
    impl ModelDoor for MixedDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            Ok(Box::pin(futures::stream::iter(
                vec![Ok(ModelDelta::Text(
                    "I'll look up the TIN.\n\nTIN 123-456-789. Forms: 1701.".to_string(),
                ))]
                .into_iter(),
            )))
        }
    }
    let events = run_conversation(
        &MixedDoor,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("tin"),
        "t1",
        "r1",
        1,
    )
    .await;
    let text = assistant_text(&events);
    assert!(!text.to_ascii_lowercase().contains("i'll look"), "{text:?}");
    assert!(text.contains("TIN 123-456-789"), "{text:?}");
}

/// Live NativeChat: wrong port, then missing profiles, with a diary of "isn't answering"
/// between them. One silent retry, then one short failure fact, no third hop.
#[tokio::test]
async fn two_failed_tools_emit_one_short_fact_not_a_retry_diary() {
    struct DiaryDoor(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for DiaryDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 2,
                "a third hop is the diary we are killing: round {round}"
            );
            let (text, id, args) = if round == 1 {
                (
                    "I'll pull the BIR host",
                    "c1",
                    r#"{"command":"gpui-agent hello"}"#,
                )
            } else {
                (
                    "The BIR agent isn't answering. I'll try another port.",
                    "c2",
                    r#"{"command":"gpui-agent invoke profile.list"}"#,
                )
            };
            let script = vec![
                ModelDelta::Text(text.to_string()),
                ModelDelta::ToolCallStart {
                    id: id.to_string(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.to_string(),
                    delta: args.to_string(),
                },
                ModelDelta::ToolCallEnd { id: id.to_string() },
            ];
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let n = Arc::new(Mutex::new(0usize));
    let n_run = n.clone();
    let fail: LocalTool = Arc::new(move |call| {
        let i = {
            let mut count = n_run.lock().unwrap();
            let i = *count;
            *count += 1;
            i
        };
        let why = if i == 0 {
            "connection refused on 17421"
        } else {
            "no profiles in the database"
        };
        opengrok_tools::ToolResult::refused(&call.id, why)
    });
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        fail,
    );

    let events = run_conversation(
        &DiaryDoor(Mutex::new(0)),
        Some(&runner),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll pull"), "{text:?}");
    assert!(!lower.contains("isn't answering"), "{text:?}");
    assert!(!lower.contains("i'll try"), "{text:?}");
    assert!(
        text.contains("no profiles") || text.contains("connection refused"),
        "one short failure fact from the tool: {text:?}"
    );
    assert!(
        text.chars().count() < 240,
        "must not be a multi-paragraph diary: {text:?}"
    );
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["tool_rounds"], 2);
}

/// Live NativeChat Hog Rider: `user_machine_shell` returns ok=true with
/// ExecOutcome::render `exit 127`. One model round, one short fact, no 8-call burn.
#[tokio::test]
async fn missing_binary_on_user_machine_ends_in_one_round() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 1,
                "a missing binary must not spend a silent retry: round {round}"
            );
            Ok(Box::pin(futures::stream::iter(
                ums_deltas("c1", "gpui-agent hello", "I'll pull the BIR host")
                    .into_iter()
                    .map(Ok),
            )))
        }
    }

    let miss: LocalTool = Arc::new(|call| {
        opengrok_tools::ToolResult::ok(
            &call.id,
            "exit 127\n--- stderr ---\nzsh: command not found: gpui-agent",
        )
    });

    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(miss)),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll pull"), "{text:?}");
    assert!(
        text.contains("command not found"),
        "one short failure fact from the tool: {text:?}"
    );
    assert!(
        text.chars().count() < 240,
        "must not be a multi-paragraph diary: {text:?}"
    );
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["tool_rounds"], 1);
    assert_eq!(timing["model_ms"].as_array().map(Vec::len), Some(1));
}

/// The 4m 17s Hog Rider turn ran `find ~` and `find /Users/uriah`. Those
/// commands must not reach the Mac. The first refusal is for the model, so
/// the next round can call the catalog instead of ending on the refusal.
#[tokio::test]
async fn a_home_directory_find_is_refused_before_it_runs() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = match round {
                1 => ums_deltas("c1", "find ~ -name AGENT.md", "I'll look up AGENT.md"),
                2 => ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.search --q juan",
                    "I'll search",
                ),
                _ => vec![ModelDelta::Text(
                    "Juan Dela Cruz, TIN 00000000000000.".to_string(),
                )],
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let ran = Arc::new(Mutex::new(Vec::<String>::new()));
    let ran_tool = ran.clone();
    let tool: LocalTool = Arc::new(move |call| {
        let command = call.arguments["command"].as_str().unwrap_or("").to_string();
        ran_tool.lock().unwrap().push(command);
        opengrok_tools::ToolResult::ok(&call.id, "exit 0\n--- stdout ---\nJuan Dela Cruz")
    });

    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(tool)),
        &MemoryJournal::new(),
        request("open the profile"),
        "t1",
        "r1",
        1,
    )
    .await;

    let ran = ran.lock().unwrap();
    assert!(
        ran.iter().all(|command| !command.starts_with("find ")),
        "find must not be dispatched: {ran:?}"
    );
    assert!(
        ran.iter().any(|command| command.contains("profile.search")),
        "the catalog invoke runs after the refusal: {ran:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    assert!(
        text.contains("Juan Dela Cruz"),
        "the catalog answer is the chat, not the refusal: {text:?}"
    );
    assert!(!text.contains("AGENT.md"), "{text:?}");
    assert!(!text.to_ascii_lowercase().contains("i'll look"), "{text:?}");
}

#[test]
fn listing_and_show_commands_are_readonly_shell_fast_path() {
    let call = |name: &str, command: &str| opengrok_tools::ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: serde_json::json!({ "command": command }),
    };
    assert!(
        !is_readonly_listing_shell(&call("shell", "gpui-agent hello")),
        "a host probe must not consume the one listing"
    );
    assert!(
        !is_readonly_listing_shell(&call(
            opengrok_tools::USER_MACHINE_SHELL,
            "gpui-agent invoke --help"
        )),
        "help is not a catalog read"
    );
    assert!(is_readonly_listing_shell(&call(
        opengrok_tools::USER_MACHINE_SHELL,
        "gpui-agent invoke profile.list"
    )));
    assert!(is_readonly_listing_shell(&call(
        opengrok_tools::USER_MACHINE_SHELL,
        "gpui-agent invoke profile.search --q buwiz"
    )));
    assert!(is_readonly_listing_shell(&call("shell", "ls -la")));
    assert!(!is_readonly_listing_shell(&call(
        "shell",
        "echo opengrok-tool-ran > /tmp/opengrok-tool-ran"
    )));
    assert!(!is_readonly_listing_shell(&call(
        "shell",
        "gpui-agent invoke profile.save"
    )));
    assert!(!is_readonly_listing_shell(&call("computer", "ls")));
}

#[test]
fn a_find_of_the_home_directory_is_a_broad_walk_and_a_deeper_path_is_not() {
    assert!(is_broad_filesystem_walk("find ~ -name AGENT.md"));
    assert!(is_broad_filesystem_walk(
        "find /Users/uriah -name 'AGENT.md'"
    ));
    assert!(is_broad_filesystem_walk(
        "find /Volumes/goldcoders -name AGENT.md"
    ));
    assert!(is_broad_filesystem_walk(
        "export GPUI_AGENT_ADDR=127.0.0.1:17423\nfind $HOME -name AGENT.md"
    ));
    assert!(!is_broad_filesystem_walk(
        "GPUI_AGENT_ADDR=127.0.0.1:17423 gpui-agent invoke profile.list"
    ));
    assert!(!is_broad_filesystem_walk(
        "find /Volumes/goldcoders/reverse-engineer-ebir-forms/bir/crates/bir-desktop/docs -name AGENT.md"
    ));
    assert!(!is_broad_filesystem_walk(
        "find /Users/uriah/code/bir -name AGENT.md"
    ));
    // Run 01a0c9f5. `$HOME/.config` is one component and stays broad.
    // `$HOME/Library/...` is a directory and is not.
    assert!(is_broad_filesystem_walk(
        r#"find "$HOME/.config" -iname '*gpui*'"#
    ));
    assert!(!is_broad_filesystem_walk(
        r#"find "$HOME/Library/Application Support" "$HOME/Library/Preferences" -iname '*gpui*'"#
    ));
    assert!(!is_broad_filesystem_walk(
        "find /Users/uriah/Library/Preferences -iname '*bir*'"
    ));
}

fn ums_deltas(id: &str, command: &str, text: &str) -> Vec<ModelDelta> {
    vec![
        ModelDelta::Text(text.to_string()),
        ModelDelta::ToolCallStart {
            id: id.to_string(),
            name: opengrok_tools::USER_MACHINE_SHELL.to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: id.to_string(),
            delta: format!(r#"{{"command":"{command}"}}"#),
        },
        ModelDelta::ToolCallEnd { id: id.to_string() },
    ]
}

fn ums_runner(tool: LocalTool) -> ToolRunner {
    ToolRunner::local_only().with_local(
        serde_json::json!({
            "type": "function",
            "function": { "name": opengrok_tools::USER_MACHINE_SHELL }
        }),
        tool,
    )
}

/// profile.list then profile.search: one real read, then a facts hop — not a second invoke.
#[tokio::test]
async fn a_second_profile_search_after_list_is_not_executed() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.list", "I'll pull profiles"),
                2 => ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.search --q buwiz",
                    "I'll look up Buwiz",
                ),
                _ => vec![ModelDelta::Text(
                    "TIN 123-456-789. Forms: 1701.".to_string(),
                )],
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let n = Arc::new(Mutex::new(0usize));
    let n_run = n.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            *n_run.lock().unwrap() += 1;
            opengrok_tools::ToolResult::ok(&call.id, "profiles: Buwiz")
        }))),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*n.lock().unwrap(), 1, "second listing must be skipped");
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll pull"), "{text:?}");
    assert!(!lower.contains("i'll look"), "{text:?}");
    assert!(text.contains("TIN 123-456-789"), "{text:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Wrong port then a listing that works: the old error must not become the answer.
#[tokio::test]
async fn a_successful_listing_does_not_paint_a_prior_failure() {
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
                ums_deltas("c1", "gpui-agent hello", "I'll pull the BIR host")
            } else if round == 2 {
                ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.list",
                    "The BIR agent isn't answering",
                )
            } else {
                vec![ModelDelta::Text("I'll look up the profiles.".to_string())]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let n = Arc::new(Mutex::new(0usize));
    let n_run = n.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let i = {
                let mut count = n_run.lock().unwrap();
                let i = *count;
                *count += 1;
                i
            };
            if i == 0 {
                opengrok_tools::ToolResult::refused(&call.id, "connection refused on 17421")
            } else {
                opengrok_tools::ToolResult::ok(&call.id, "profiles: Buwiz")
            }
        }))),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll"), "{text:?}");
    assert!(!lower.contains("isn't answering"), "{text:?}");
    assert!(
        !lower.contains("connection refused"),
        "stale fail must not become chat after a listing: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// A run that cannot be recorded must not proceed: it would produce work a reconnect can never
/// reproduce, which is the failure this design exists to prevent.
#[tokio::test]
async fn a_run_that_cannot_be_recorded_does_not_run() {
    struct BrokenJournal;
    #[async_trait::async_trait]
    impl RunJournal for BrokenJournal {
        async fn record(&self, _run_id: &str, _events: &[Event]) -> Result<(), JournalError> {
            Err(JournalError::Unwritable("the disk is gone".to_string()))
        }
    }

    let events = run_conversation(
        &MockDoor::echoing(),
        None,
        &BrokenJournal,
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(
        events.last().unwrap().event_type,
        opengrok_wire::agui::EventType::RunError
    );
    // Nothing was said: the model was never called.
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == opengrok_wire::agui::EventType::TextMessageContent)
    );
}

/// Everything a client saw is in the journal — that is what makes a replay complete.
#[tokio::test]
async fn every_event_a_client_saw_reached_the_journal() {
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &MockDoor::echoing(),
        None,
        &journal,
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(journal.event_count(), events.len());
}

/// Exactly one ending, however the run went — two would double-render in a consumer.
#[tokio::test]
async fn a_run_has_exactly_one_ending() {
    for door in [MockDoor::echoing(), MockDoor::failing_with("nope")] {
        let events = run_turn(&door, request("hello"), "t1", "r1", 1).await;
        let endings = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    EventType::RunFinished | EventType::RunError
                )
            })
            .count();
        assert_eq!(endings, 1, "{:?}", events.last());
    }
}

/// NativeChat paints POST /ag-ui from this sink. If the sink is a subset of the Vec, the
/// HTTP body is missing the opening, the close, or both — which looks like a client that
/// never streamed.
#[tokio::test]
async fn a_streaming_sink_sees_every_event_the_run_produced() {
    struct Collect(std::sync::Mutex<Vec<Event>>);
    #[async_trait::async_trait]
    impl EventSink for Collect {
        async fn emit(&self, events: &[Event]) {
            if let Ok(mut seen) = self.0.lock() {
                seen.extend(events.iter().cloned());
            }
        }
    }

    let sink = Collect(std::sync::Mutex::new(Vec::new()));
    let events = run_conversation_streaming(
        &MockDoor::echoing(),
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
        &sink,
    )
    .await;
    let sunk = sink.0.lock().unwrap().clone();
    let produced: Vec<_> = events.iter().map(|event| event.event_type).collect();
    let live: Vec<_> = sunk.iter().map(|event| event.event_type).collect();
    assert_eq!(live, produced, "sink={live:?} vec={produced:?}");
}

/// A paced door must deliver the first word while the run is still in flight. If this
/// fires only after `run_conversation_streaming` joins, POST /ag-ui would still look
/// like a buffered JSON response.
#[tokio::test]
async fn a_paced_sink_receives_text_while_the_model_is_still_talking() {
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
            &MockDoor::echoing().paced_by_ms(40),
            None,
            &MemoryJournal::new(),
            request("hello from a paced mock door"),
            "t1",
            "r1",
            1,
            &sink,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("first text should arrive before the run ends")
        .unwrap();
    assert!(
        !handle.is_finished(),
        "text arrived only after the turn finished — the HTTP body would dump at the end"
    );
    let events = handle.await.unwrap();
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

fn shot(call_id: &str) -> opengrok_tools::ToolResult {
    opengrok_tools::ToolResult::ok(call_id, "screenshot of the 1280x800 screen attached")
        .with_image(opengrok_tools::ToolImage {
            mime: "image/png".into(),
            base64: "iVBORw0KGgo=".into(),
            width: 1280,
            height: 800,
            visibility: opengrok_tools::ImageVisibility::Agent,
        })
}

#[test]
fn a_tool_result_with_a_picture_becomes_a_message_with_an_image() {
    let message = tool_result_message(&shot("c1"));
    assert_eq!(message.role, "user");
    assert!(message.content.starts_with("[tool c1 result] screenshot"));
    assert_eq!(message.images.len(), 1);
    assert_eq!(message.images[0].mime, "image/png");

    let plain = tool_result_message(&opengrok_tools::ToolResult::ok("c2", "done"));
    assert!(plain.images.is_empty());
}

#[test]
fn an_empty_result_array_carries_the_dead_end_sentence() {
    let empty = tool_result_message(&opengrok_tools::ToolResult::ok(
        "c1",
        r#"{"ok":true,"result":[]}"#,
    ));
    assert!(
        empty.content.contains(intent::EMPTY_RESULT_NUDGE),
        "{:?}",
        empty.content
    );
    let full = tool_result_message(&opengrok_tools::ToolResult::ok(
        "c1",
        r#"{"ok":true,"result":[{"name":"Juan"}]}"#,
    ));
    assert!(
        !full.content.contains(intent::EMPTY_RESULT_NUDGE),
        "{:?}",
        full.content
    );
    let garbage = tool_result_message(&opengrok_tools::ToolResult::ok("c1", "not-json"));
    assert_eq!(garbage.content, "[tool c1 result] not-json");
}

/// Screenshots are the widest thing in a request; only the last two say where the screen is.
#[test]
fn only_the_two_most_recent_screenshots_travel() {
    let mut messages: Vec<ChatMessage> = (1..=4)
        .map(|n| tool_result_message(&shot(&format!("c{n}"))))
        .collect();
    messages.insert(
        2,
        ChatMessage {
            role: "assistant".into(),
            content: "clicking".into(),
            images: Vec::new(),
        },
    );

    keep_recent_images(&mut messages, RECENT_IMAGES);

    let carried: Vec<bool> = messages.iter().map(|m| !m.images.is_empty()).collect();
    assert_eq!(carried, vec![false, false, false, true, true]);
    // The words stay even where the picture went.
    assert!(messages[0].content.contains("[tool c1 result]"));
}

/// A person's no is about their machine, not about one spelling of the command. Seen live:
/// the model reworded the command eight times after a deny and the round cap was what
/// stopped it.
#[tokio::test]
async fn a_denied_machine_ends_the_run_even_when_the_command_is_reworded() {
    struct RewordingDoor(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for RewordingDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let n = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = vec![
                ModelDelta::ToolCallStart {
                    id: format!("c{n}"),
                    name: "user_machine_shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: format!("c{n}"),
                    delta: format!(r#"{{"command":"open -a Safari https://facebook.com/{n}"}}"#),
                },
                ModelDelta::ToolCallEnd {
                    id: format!("c{n}"),
                },
            ];
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    struct DenyingSink;
    #[async_trait::async_trait]
    impl opengrok_tools::UserMachineSink for DenyingSink {
        async fn decide(
            &self,
            _account_id: &opengrok_core::id::AccountId,
            _command: &str,
        ) -> opengrok_tools::UserMachineVerdict {
            opengrok_tools::UserMachineVerdict::Deny("the machine's owner said no".into())
        }
        async fn run(
            &self,
            _account_id: &opengrok_core::id::AccountId,
            _command: &str,
            _call_id: &str,
            _approved: bool,
        ) -> opengrok_tools::UserMachineReply {
            opengrok_tools::UserMachineReply::Refused("the machine's owner said no".into())
        }
    }

    let journal = MemoryJournal::new();
    let runner = tool_runner_with(|executor| executor.with_user_machine(Arc::new(DenyingSink)));
    let events = run_conversation(
        &RewordingDoor(Mutex::new(0)),
        Some(&runner),
        &journal,
        request("visit facebook.com"),
        "t1",
        "r1",
        1,
    )
    .await;

    let starts = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .count();
    assert_eq!(starts, 2, "a no, one more ask, then stop: {events:?}");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    assert!(
        last.extra
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .contains("twice"),
        "{last:?}"
    );
}

/// A computer whose screen changes on every look (or not), for the screen budgets.
struct ScreenComputer {
    changing: bool,
    looks: Mutex<usize>,
}
#[async_trait::async_trait]
impl opengrok_box::Computer for ScreenComputer {
    async fn create(&self, _ttl: Option<u64>) -> opengrok_box::BoxResult<String> {
        Ok("box_screen".into())
    }
    async fn run(
        &self,
        _b: &str,
        _c: &str,
        _t: u32,
    ) -> opengrok_box::BoxResult<opengrok_box::CommandOutput> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn start(
        &self,
        _b: &str,
        _c: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(
        &self,
        _b: &str,
        _p: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn read_file(&self, _b: &str, _p: &str) -> opengrok_box::BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> opengrok_box::BoxResult<String> {
        Ok(String::new())
    }
    async fn stop(&self, _b: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> opengrok_box::BoxResult<String> {
        Ok("running".into())
    }
    async fn offers_a_screen(&self, _box_id: &str) -> bool {
        true
    }
    async fn screen_url(&self, _b: &str) -> opengrok_box::BoxResult<Option<String>> {
        Ok(Some("http://127.0.0.1:1/vnc.html".into()))
    }
    async fn screenshot(&self, _b: &str) -> opengrok_box::BoxResult<opengrok_box::Screenshot> {
        let n = {
            let mut looks = self.looks.lock().unwrap();
            *looks += 1;
            *looks
        };
        Ok(opengrok_box::Screenshot {
            mime: "image/png".into(),
            png_base64: if self.changing {
                format!("frame-{n}")
            } else {
                "frame-same".into()
            },
            width: 1280,
            height: 800,
        })
    }
    async fn act(&self, _b: &str, _a: &opengrok_box::CuaAction) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
}

/// A model that only ever takes screenshots.
struct LookingDoor(Mutex<usize>, usize);
#[async_trait::async_trait]
impl ModelDoor for LookingDoor {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let n = {
            let mut count = self.0.lock().unwrap();
            *count += 1;
            *count
        };
        let script = if n > self.1 {
            vec![ModelDelta::Text("done looking".to_string())]
        } else {
            vec![
                ModelDelta::ToolCallStart {
                    id: format!("c{n}"),
                    name: "computer".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: format!("c{n}"),
                    delta: r#"{"action":"screenshot"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: format!("c{n}"),
                },
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn screen_runner(changing: bool) -> ToolRunner {
    tool_runner_on(
        Arc::new(ScreenComputer {
            changing,
            looks: Mutex::new(0),
        }),
        |executor| executor.with_screen(true),
    )
}

/// Looking is the work on a desktop: twelve screenshots must not trip the eight-call cap
/// meant for chatter.
#[tokio::test]
async fn looking_at_a_changing_screen_is_not_chatter() {
    let journal = MemoryJournal::new();
    let runner = screen_runner(true);
    let events = run_conversation(
        &LookingDoor(Mutex::new(0), 12),
        Some(&runner),
        &journal,
        request("find the terminal"),
        "t1",
        "r1",
        1,
    )
    .await;
    let looks = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .count();
    assert_eq!(looks, 12, "{events:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Step PNGs are `agent` and the journal drops their bytes. The run-end pin keeps one PNG.
#[tokio::test]
async fn computer_step_shots_are_agent_and_the_journal_keeps_the_end_pin() {
    let journal = MemoryJournal::new();
    let runner = screen_runner(true);
    let events = run_conversation(
        &LookingDoor(Mutex::new(0), 3),
        Some(&runner),
        &journal,
        request("find the terminal"),
        "t1",
        "r1",
        1,
    )
    .await;
    let live_shots: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .filter_map(|event| event.extra.get("image"))
        .collect();
    assert_eq!(
        live_shots.len(),
        4,
        "three steps plus the end pin: {live_shots:?}"
    );
    assert!(
        live_shots.iter().take(3).all(|image| {
            image["visibility"] == "agent" && image.get("base64").and_then(|v| v.as_str()).is_some()
        }),
        "live SSE still carries step PNGs for the Computer pane: {live_shots:?}"
    );
    assert_eq!(live_shots.last().unwrap()["visibility"], "end");
    assert!(
        live_shots
            .last()
            .unwrap()
            .get("base64")
            .and_then(|v| v.as_str())
            .is_some_and(|b| !b.is_empty()),
        "the end pin keeps the PNG: {live_shots:?}"
    );

    let journaled: Vec<_> = journal
        .batches()
        .into_iter()
        .flatten()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .filter_map(|event| event.extra.get("image").cloned())
        .collect();
    let agent_without_bytes = journaled
        .iter()
        .filter(|image| image["visibility"] == "agent" && image.get("base64").is_none())
        .count();
    let end_with_bytes = journaled
        .iter()
        .filter(|image| {
            image["visibility"] == "end"
                && image
                    .get("base64")
                    .and_then(|v| v.as_str())
                    .is_some_and(|b| !b.is_empty())
        })
        .count();
    assert_eq!(
        agent_without_bytes, 3,
        "journal must drop step PNG bytes: {journaled:?}"
    );
    assert_eq!(
        end_with_bytes, 1,
        "journal keeps the end pin PNG: {journaled:?}"
    );
}

/// The case the earlier test could not fail: a real streaming model sends the
/// arguments in pieces, and a per-fragment scrub sees no JSON to scrub.
#[test]
fn streamed_fragments_of_a_smuggled_password_are_assembled_and_scrubbed() {
    use opengrok_wire::agui::EventType;
    let ev = |kind: EventType| Event::new(kind, 0);
    let events = vec![
        ev(EventType::ToolCallStart)
            .with("toolCallId", "call-1")
            .with("toolCallName", opengrok_tools::REQUEST_USER_FORM),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-1")
            .with(
                "delta",
                "{\"title\":\"Log in\",\"values\":{\"password\":\"s3",
            ),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-1")
            .with("delta", "cret\"}}"),
        ev(EventType::ToolCallEnd).with("toolCallId", "call-1"),
        // A tool that carries no secrets keeps its fragments exactly as they were.
        ev(EventType::ToolCallStart)
            .with("toolCallId", "call-2")
            .with("toolCallName", "shell"),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-2")
            .with("delta", "{\"cmd\":\"ls"),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-2")
            .with("delta", " -la\"}"),
    ];
    let out = scrub_streamed_tool_args(events);
    let text = serde_json::to_string(&out).expect("serialise");
    assert!(
        !text.contains("s3cret"),
        "the smuggled password must not survive: {text}"
    );
    assert!(!text.contains("s3"), "not even a fragment of it: {text}");
    let form_args: Vec<&Event> = out
        .iter()
        .filter(|e| {
            e.event_type == opengrok_wire::agui::EventType::ToolCallArgs
                && e.extra
                    .get("toolCallId")
                    .and_then(serde_json::Value::as_str)
                    == Some("call-1")
        })
        .collect();
    assert_eq!(
        form_args.len(),
        1,
        "one assembled fragment stands in for the pieces"
    );
    let delta = form_args[0]
        .extra
        .get("delta")
        .and_then(serde_json::Value::as_str)
        .unwrap();
    assert!(
        delta.contains("Log in"),
        "non-secret fields are kept: {delta}"
    );
    let shell_args: Vec<&Event> = out
        .iter()
        .filter(|e| {
            e.extra
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                == Some("call-2")
        })
        .collect();
    assert_eq!(shell_args.len(), 3, "an ordinary tool is untouched");
    assert!(text.contains("ls"), "{text}");
}

#[tokio::test]
async fn each_user_form_in_one_completion_gets_an_awaiting_custom_and_the_stream_closes() {
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &MockDoor::asking_for_stacked_user_forms(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
    )
    .await;
    let forms: Vec<_> = events
        .iter()
        .filter(|event| {
            event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|v| v.as_str()) == Some("run-awaiting-approval")
                && event.extra.get("reason").and_then(|v| v.as_str()) == Some("user-form")
        })
        .collect();
    assert_eq!(
        forms.len(),
        3,
        "one CUSTOM per stacked request_user_form: {events:?}"
    );
    let call_ids: Vec<_> = forms
        .iter()
        .filter_map(|event| event.extra.get("callId").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        call_ids,
        vec!["mock-form-1", "mock-form-2", "mock-form-3"],
        "{call_ids:?}"
    );
    assert_eq!(
        events.last().map(|event| event.event_type),
        Some(EventType::RunFinished),
        "HITL park must close the SSE or Waiting spins forever: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::RunStarted),
        "{events:?}"
    );
}

#[tokio::test]
async fn two_website_logins_keep_provider_call_ids_on_each_awaiting_custom() {
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &MockDoor::asking_for_two_website_logins(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
    )
    .await;
    let call_ids: Vec<_> = events
        .iter()
        .filter(|event| {
            event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|v| v.as_str()) == Some("run-awaiting-approval")
                && event.extra.get("reason").and_then(|v| v.as_str()) == Some("user-form")
        })
        .filter_map(|event| event.extra.get("callId").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        call_ids,
        vec!["call-42628be6", "call-42628be6-1"],
        "provider-style parallel ids must each get a CUSTOM: {events:?}"
    );
}

/// The same picture four times running is waiting, not working; the run says so.
#[tokio::test]
async fn the_same_screen_four_times_ends_the_run() {
    let journal = MemoryJournal::new();
    let runner = screen_runner(false);
    let events = run_conversation(
        &LookingDoor(Mutex::new(0), 40),
        Some(&runner),
        &journal,
        request("wait for the page"),
        "t1",
        "r1",
        1,
    )
    .await;
    let looks = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .count();
    assert_eq!(looks, SAME_SCREEN_LIMIT, "{events:?}");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    assert!(
        last.extra
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .contains("has not changed"),
        "{last:?}"
    );
}

/// A second home-directory find in the same turn stops. The person sees the
/// refusal, not another 90s walk.
#[tokio::test]
async fn a_second_home_directory_find_ends_the_turn() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 2,
                "a second home-directory find ends the turn: round {round}"
            );
            Ok(Box::pin(futures::stream::iter(
                ums_deltas("c1", "find ~ -name AGENT.md", "I'll look again")
                    .into_iter()
                    .map(Ok),
            )))
        }
    }

    let ran = Arc::new(Mutex::new(0usize));
    let ran_tool = ran.clone();
    let tool: LocalTool = Arc::new(move |call| {
        *ran_tool.lock().unwrap() += 1;
        opengrok_tools::ToolResult::ok(&call.id, "should not run")
    });

    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(tool)),
        &MemoryJournal::new(),
        request("open the profile"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(*ran.lock().unwrap(), 0, "neither find is dispatched");
    let text = assistant_text(&events);
    assert!(
        text.contains("home directory"),
        "the second refusal is the sentence: {text:?}"
    );
    assert!(!text.contains("AGENT.md"), "{text:?}");
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["model_ms"].as_array().map(Vec::len), Some(2));
}

/// Grey run 01a0c9ed: hello exited 0, then profile.search was skipped as a
/// second listing and the chat stayed empty. Hello is a probe. The search runs.
#[tokio::test]
async fn hello_does_not_consume_the_listing_so_profile_search_runs() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent hello", "I'll probe the host"),
                2 => ums_deltas(
                    "c2",
                    "GPUI_AGENT_ADDR=127.0.0.1:17421 gpui-agent invoke profile.search --q juan",
                    "I'll search",
                ),
                _ => vec![ModelDelta::Text(
                    "Juan Dela Cruz, TIN 00000000000000.".to_string(),
                )],
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let commands = Arc::new(Mutex::new(Vec::<String>::new()));
    let commands_run = commands.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let command = call.arguments["command"].as_str().unwrap_or("").to_string();
            commands_run.lock().unwrap().push(command.clone());
            let body = if command.contains("profile.search") {
                "exit 0\n--- stdout ---\nJuan Dela Cruz TIN 00000000000000"
            } else {
                "exit 0\n--- stdout ---\n{\"hello\":{\"app\":\"bir-desktop\",\"ready\":true}}"
            };
            opengrok_tools::ToolResult::ok(&call.id, body)
        }))),
        &MemoryJournal::new(),
        request("get juan dela cruz tax profile"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = commands.lock().unwrap();
    assert_eq!(ran.len(), 2, "hello and the search both run: {ran:?}");
    assert!(
        ran.iter().any(|command| command.contains("profile.search")),
        "profile.search must be dispatched: {ran:?}"
    );
    let text = assistant_text(&events);
    assert!(
        text.contains("Juan Dela Cruz"),
        "the search answer is the chat: {text:?}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("i'll probe"),
        "{text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// A third catalog read used to finish before any TEXT_MESSAGE. The person
/// still gets the sentence from the read that actually ran.
#[tokio::test]
async fn a_repeated_listing_closes_with_the_catalog_sentence() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 3,
                "the third listing closes the turn: round {round}"
            );
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.list", "I'll list"),
                2 => ums_deltas("c2", "gpui-agent invoke profile.search --q juan", "again"),
                3 => ums_deltas("c3", "gpui-agent invoke profile.list", "once more"),
                _ => Vec::new(),
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let n = Arc::new(Mutex::new(0usize));
    let n_run = n.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            *n_run.lock().unwrap() += 1;
            opengrok_tools::ToolResult::ok(
                &call.id,
                "exit 0\n--- stdout ---\nJuan Dela Cruz TIN 00000000000000",
            )
        }))),
        &MemoryJournal::new(),
        request("list tax profiles"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*n.lock().unwrap(), 1, "only the first catalog read runs");
    let text = assistant_text(&events);
    assert!(
        text.contains("Juan Dela Cruz"),
        "a repeated listing still leaves a sentence: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Green run 01a0ca61: `profile.create` ignores name and tin, returns only
/// `{view: profile-manager}`, and quote variants of that invoke burned all
/// 8 model calls. The second create is the same action. It does not run,
/// and the chat is the editor sentence.
#[tokio::test]
async fn a_second_profile_create_closes_with_the_editor_sentence() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 4,
                "a repeated profile.create closes the turn: round {round}"
            );
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.list", "I'll list"),
                2 => ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.create --arg name=Juana Jane --arg tin=00000000000001",
                    "",
                ),
                3 => ums_deltas(
                    "c3",
                    "gpui-agent invoke profile.create --arg name='Juana Jane' --arg tin=00000000000001",
                    "",
                ),
                4 => ums_deltas(
                    "c4",
                    "gpui-agent invoke profile.create --arg 'name=Juana Jane' --arg tin=00000000000001",
                    "",
                ),
                _ => Vec::new(),
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let commands = Arc::new(Mutex::new(Vec::<String>::new()));
    let commands_run = commands.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let command = call.arguments["command"].as_str().unwrap_or("").to_string();
            commands_run.lock().unwrap().push(command.clone());
            let body = if command.contains("profile.list") {
                "exit 0\n--- stdout ---\n{\"result\":[{\"name\":\"Juan Dela Cruz\"}]}\n"
            } else if command.contains("name=Juana Jane") && !command.contains('\'') {
                "exit 2\n--- stderr ---\nerror: unexpected argument 'Jane' found\n"
            } else {
                "exit 0\n--- stdout ---\n{\"v\":2,\"ok\":true,\"result\":{\"view\":\"profile-manager\"}}\n"
            };
            opengrok_tools::ToolResult::ok(&call.id, body)
        }))),
        &MemoryJournal::new(),
        request("create Juana Jane"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = commands.lock().unwrap();
    assert_eq!(
        ran.iter()
            .filter(|command| command.contains("profile.create"))
            .count(),
        2,
        "the failed quote and the one open run; the repeat does not: {ran:?}"
    );
    let text = assistant_text(&events);
    assert!(
        text.contains("Opened profile-manager. Nothing was saved."),
        "the editor sentence is the chat, not the round cap: {text:?}"
    );
    assert!(
        !text.contains("limit of 8 model calls"),
        "the cap is not the answer: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// After the editor opens, a different command still runs. Create is not a
/// wall in front of set-value.
#[tokio::test]
async fn set_value_after_profile_create_still_runs() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 3,
                "set-value is one more command, then the answer: round {round}"
            );
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.create", ""),
                2 => ums_deltas("c2", "gpui-agent set-value profile-name 'Juana Jane'", ""),
                3 => vec![ModelDelta::Text(
                    "The editor is open for Juana Jane. Nothing is saved yet.".to_string(),
                )],
                _ => Vec::new(),
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let commands = Arc::new(Mutex::new(Vec::<String>::new()));
    let commands_run = commands.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let command = call.arguments["command"].as_str().unwrap_or("").to_string();
            commands_run.lock().unwrap().push(command.clone());
            let body = if command.contains("set-value") {
                "exit 0\n--- stdout ---\n{\"ok\":true,\"result\":{\"id\":\"profile-name\"}}\n"
            } else {
                "exit 0\n--- stdout ---\n{\"v\":2,\"ok\":true,\"result\":{\"view\":\"profile-manager\"}}\n"
            };
            opengrok_tools::ToolResult::ok(&call.id, body)
        }))),
        &MemoryJournal::new(),
        request("create Juana Jane"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = commands.lock().unwrap();
    assert!(
        ran.iter().any(|command| command.contains("set-value")),
        "set-value still runs after create: {ran:?}"
    );
    let text = assistant_text(&events);
    assert!(
        text.contains("Juana Jane"),
        "the model's answer is the chat: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}
