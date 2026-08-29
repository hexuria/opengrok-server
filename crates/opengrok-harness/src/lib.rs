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

pub use gateway::GatewayDoor;
pub use mock::MockDoor;
pub use model::{ChatMessage, DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
pub use projection::Projection;

use futures::StreamExt;
use opengrok_wire::agui::Event;

/// Run one turn and collect every event a client should see.
///
/// Collecting rather than streaming, for now: a run's events are small, and having the whole
/// sequence in hand is what lets slice 4 write it to the event log before the client sees it —
/// which is how a run survives the client disconnecting. Streaming straight through would be
/// faster to the first token and is the obvious next step; it is not what makes a run durable.
pub async fn run_turn(
    door: &dyn ModelDoor,
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

    events.extend(projection.finish());
    events
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
