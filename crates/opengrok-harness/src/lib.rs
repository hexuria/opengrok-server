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
pub mod mock;
pub mod model;
pub mod projection;
pub mod tools;

pub use gateway::GatewayDoor;
pub use mock::MockDoor;
pub use model::{ChatMessage, DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
pub use projection::Projection;
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

/// Run a turn, and run any tools the model asked for.
///
/// ONE ROUND OF TOOLS, NOT A LOOP, AND THE LIMIT IS DELIBERATE. Feeding results back for another
/// model call is the obvious next step and it is where the durability question gets hard: a
/// multi-round loop must be resumable *between* rounds, which means each round's results have to
/// reach the log before the next call is made. Doing that properly is the next slice; pretending
/// to do it with an in-memory `while` would build exactly the thing this project exists to avoid.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use opengrok_wire::agui::EventType;

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
        let runner = ToolRunner::new(
            Executor::new(computer.clone()),
            ToolContext::from_coworker(CoworkerId::from_stored("cw_ada"), &coworker),
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
