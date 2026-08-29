//! The agent loop: a run, from a client's request to the events it sees.
//!
//! Three pieces, kept apart because each fails differently:
//!   - `model`      — the door to a model, and the provider-neutral deltas that come back.
//!   - `projection` — deltas to a well-formed AG-UI run. Pure, and where the bracketing rules live.
//!   - `gateway` / `mock` — the two doors: open-ai-gateway, and a scripted one that spends nothing.
//!
//! The loop is deliberately thin. Everything hard is either in the projection (pure, tested
//! exhaustively) or in a door (isolated, swappable), which is what makes a run reproducible
//! without a provider.

pub mod gateway;
pub mod journal;
pub mod mock;
pub mod model;
pub mod projection;
pub mod rig_door;
pub mod tools;

pub use gateway::GatewayDoor;
pub use journal::{JournalError, MemoryJournal, RunJournal};
pub use mock::MockDoor;
pub use model::{ChatMessage, DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
pub use projection::Projection;
pub use rig_door::RigDoor;
pub use tools::{ToolRunner, collect_tool_calls};

use futures::StreamExt;
use opengrok_wire::agui::Event;

/// Run one turn and collect every event a client should see.
///
/// Collecting rather than streaming, for now: a run's events are small, and having the whole
/// sequence in hand is what lets the caller write it to the event log before the client sees it —
/// which is how a run survives the client disconnecting. Streaming straight through would be
/// faster to the first token and is the obvious next step; it is not what makes a run durable.
pub async fn run_turn(
    door: &dyn ModelDoor,
    request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
) -> Vec<Event> {
    run_turn_with_tools(door, None, request, thread_id, run_id, at_ms).await
}

/// How many model calls one conversation may make.
///
/// A model that answers every tool result with another tool call would otherwise run until it ran
/// out of money. The bound is generous enough for real work and finite, and hitting it ends the
/// run as a *result* the client can see rather than a silent stop.
pub const MAX_ROUNDS: usize = 8;

/// Run a turn, and run any tools the model asked for. One round; see `run_conversation` for the
/// durable multi-round loop.
pub async fn run_turn_with_tools(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
) -> Vec<Event> {
    let mut projection = Projection::new(thread_id, run_id, at_ms);
    let mut events = projection.start();

    let mut stream = match door.stream(request).await {
        Ok(stream) => stream,
        // A door that will not open is a failed run, not a crash: the client gets an ending it can
        // render and reason about (CLAUDE.md #8, fail closed and say why).
        Err(error) => {
            events.extend(projection.fail(error.to_string()));
            return events;
        }
    };

    while let Some(delta) = stream.next().await {
        match delta {
            Ok(delta) => events.extend(projection.push(delta)),
            Err(error) => {
                events.extend(projection.fail(error.to_string()));
                return events;
            }
        }
    }

    // Anything the model asked for, run on the coworker's own computer. The results are emitted as
    // AG-UI tool-result events so a person watching sees what happened, and so the log holds it.
    if let Some(runner) = tools {
        for result in runner.run_all(&collect_tool_calls(&events)).await {
            events.extend(projection.push_tool_result(&result));
        }
    }

    events.extend(projection.finish());
    events
}

/// The durable loop: model, tools, model again, until the model stops asking.
///
/// THE ORDERING IS THE POINT. Each round's events reach the journal *before* the next model call
/// is made, so a crash between rounds leaves a log that says exactly how far the run got. Reversing
/// those two lines would still pass every test about what a client sees and would quietly destroy
/// the property the whole project is built on.
///
/// The conversation grows as it goes: the model's own reply and the tool results are appended to
/// the messages, so the next call sees what happened rather than being asked the same question
/// again.
pub async fn run_conversation(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    journal: &dyn RunJournal,
    mut request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
) -> Vec<Event> {
    let mut projection = Projection::new(thread_id, run_id, at_ms);
    let mut all = Vec::new();

    let mut opening = projection.start();
    if let Err(error) = journal.record(run_id, &opening).await {
        // A run we cannot record must not proceed: it would produce work that a reconnect can
        // never reproduce, which is the failure this design exists to prevent.
        let mut failed = projection.fail(format!("the run could not be recorded: {error}"));
        all.append(&mut opening);
        all.append(&mut failed);
        return all;
    }
    all.append(&mut opening);

    for round in 0..MAX_ROUNDS {
        let mut round_events = Vec::new();

        let stream = match door.stream(request.clone()).await {
            Ok(stream) => Some(stream),
            Err(error) => {
                round_events.extend(projection.fail(error.to_string()));
                None
            }
        };

        let mut said = String::new();
        if let Some(mut stream) = stream {
            let mut broke = false;
            while let Some(delta) = stream.next().await {
                match delta {
                    Ok(delta) => {
                        if let ModelDelta::Text(text) = &delta {
                            said.push_str(text);
                        }
                        round_events.extend(projection.push(delta));
                    }
                    Err(error) => {
                        round_events.extend(projection.fail(error.to_string()));
                        broke = true;
                        break;
                    }
                }
            }
            if !broke {
                // Tools for this round, run on the coworker's own computer.
                let calls = collect_tool_calls(&round_events);
                if let (Some(runner), false) = (tools, calls.is_empty()) {
                    let results = runner.run_all(&calls).await;
                    // A pending approval SUSPENDS the run rather than failing it: the person can
                    // still say yes tomorrow, and the log already holds everything up to here.
                    let suspended = results.iter().any(|result| result.awaiting_approval);

                    for result in &results {
                        round_events.extend(projection.push_tool_result(result));
                        // The model needs to see what its tool said, in its own transcript.
                        request.messages.push(ChatMessage {
                            role: "user".to_string(),
                            content: format!("[tool {} result] {}", result.call_id, result.content),
                        });
                    }

                    if suspended {
                        // Ended as a readable state, not a silent stop and not a failure. The run
                        // stays `running` in the log, which is exactly what `interrupted_runs`
                        // looks for — resumption and approval share the same machinery.
                        let mut waiting = projection.awaiting_approval();
                        let _ = journal.record(run_id, &round_events).await;
                        let _ = journal.record(run_id, &waiting).await;
                        all.append(&mut round_events);
                        all.append(&mut waiting);
                        return all;
                    }
                    if !said.is_empty() {
                        request.messages.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: said,
                        });
                    }

                    // DURABLE BEFORE THE NEXT CALL. Recorded here, at the top of the next round's
                    // dependency chain, so a crash after this point can be picked up.
                    if let Err(error) = journal.record(run_id, &round_events).await {
                        round_events.extend(
                            projection.fail(format!("the run could not be recorded: {error}")),
                        );
                        all.append(&mut round_events);
                        return all;
                    }
                    all.append(&mut round_events);

                    if round + 1 == MAX_ROUNDS {
                        // Ending as a result, not a silent stop: the client is told why.
                        let mut ending = projection.fail(format!(
                            "this run reached its limit of {MAX_ROUNDS} model calls"
                        ));
                        let _ = journal.record(run_id, &ending).await;
                        all.append(&mut ending);
                        return all;
                    }
                    continue;
                }
            }
        }

        // No tools were asked for, or the run failed: this is the last round either way.
        let mut ending = projection.finish();
        round_events.append(&mut ending);
        let _ = journal.record(run_id, &round_events).await;
        all.append(&mut round_events);
        return all;
    }

    all
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use opengrok_wire::agui::EventType;

    fn tool_runner() -> ToolRunner {
        use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand};
        use opengrok_core::id::{BoxId, CoworkerId};
        use opengrok_tools::{Executor, ToolContext};
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
            Executor::with_policy(
                Arc::new(crate::tools::tests_support::RecordingComputer::default()),
                policy,
            ),
            ToolContext::from_coworker(account, CoworkerId::from_stored("cw_ada"), &coworker),
        )
    }

    fn request(text: &str) -> ModelRequest {
        ModelRequest {
            model: "mock".to_string(),
            system: None,
            messages: vec![ChatMessage {
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
    }

    /// Without tools wired in, a tool call is still well-formed — it simply produces no result.
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
                    let mut calls = self.1.lock().map_err(|_| {
                        ModelError::Stream("the door's lock was poisoned".to_string())
                    })?;
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
                .any(|event| event.event_type
                    == opengrok_wire::agui::EventType::TextMessageContent)
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
}
