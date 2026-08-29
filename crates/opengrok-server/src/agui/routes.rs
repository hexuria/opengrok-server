//! `POST /ag-ui` — the endpoint openbot adds as a Bot.
//!
//! One request opens one SSE stream carrying the events of a single run. openbot supplies the
//! thread and run ids (`RunAgentInput`), so we do not mint them: the client correlates its own UI
//! against those values, and inventing our own would orphan the reply.
//!
//! THE ENVELOPE IS THE CONTRACT, EVEN WHEN THE MIDDLE IS A STUB. A run that starts must finish or
//! error — `RUN_STARTED` … `RUN_FINISHED` — because a consumer holds its spinner open on the
//! promise of that closing event. This slice streams a real, correctly-shaped conversation with a
//! placeholder body; slice 3 replaces the middle with the harness, and the framing does not change.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::stream::{self, Stream};
use opengrok_wire::agui::{Event, EventType, RunAgentInput};

use crate::auth::AuthState;

pub fn router(state: AuthState) -> Router {
    Router::new().route("/ag-ui", post(run)).with_state(state)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Start a run and stream its events.
pub async fn run(State(_state): State<AuthState>, Json(input): Json<RunAgentInput>) -> Response {
    let events = plan_run(&input);
    sse(stream::iter(
        events.into_iter().map(Ok::<_, std::io::Error>),
    ))
}

/// Build the event sequence for one run.
///
/// Split out from the handler so the *sequence* is testable without a socket — the ordering rules
/// (started first, finished last, message start/content/end nested inside) are the part a consumer
/// actually depends on.
pub fn plan_run(input: &RunAgentInput) -> Vec<Event> {
    let at = now_ms();
    let message_id = format!("msg_{}", input.run_id);

    // What the person last said. The harness will consume this properly in slice 3; echoing it
    // here proves the request body arrived intact rather than that a constant can be returned.
    let last_user_text = input
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .and_then(|message| message.content.clone())
        .unwrap_or_else(|| "(no user message)".to_string());

    vec![
        Event::new(EventType::RunStarted, at)
            .with("threadId", input.thread_id.clone())
            .with("runId", input.run_id.clone()),
        Event::new(EventType::TextMessageStart, at)
            .with("messageId", message_id.clone())
            .with("role", "assistant"),
        Event::new(EventType::TextMessageContent, at)
            .with("messageId", message_id.clone())
            .with(
                "delta",
                format!("OpenGrok received: {last_user_text}. The harness lands in slice 3."),
            ),
        Event::new(EventType::TextMessageEnd, at).with("messageId", message_id),
        Event::new(EventType::RunFinished, at)
            .with("threadId", input.thread_id.clone())
            .with("runId", input.run_id.clone()),
    ]
}

/// Wrap an event stream in the SSE response openbot expects.
fn sse<S, E>(events: S) -> Response
where
    S: Stream<Item = Result<Event, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    use futures::StreamExt;

    let body = events.map(|event| {
        event.map(|event| {
            // An event that will not serialise is dropped rather than allowed to panic mid-run;
            // `to_sse_frame` returns None and the stream continues.
            event.to_sse_frame().unwrap_or_default()
        })
    });

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            // Without this a proxy may buffer the whole run and deliver it at the end, which looks
            // exactly like a server that never streamed.
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use opengrok_wire::agui::Message;
    use serde_json::json;

    fn input(messages: Vec<Message>) -> RunAgentInput {
        RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            parent_run_id: None,
            state: json!(null),
            messages,
            tools: json!(null),
            context: json!(null),
            forwarded_props: json!(null),
            extra: Default::default(),
        }
    }

    fn user(text: &str) -> Message {
        Message {
            id: "m1".to_string(),
            role: "user".to_string(),
            content: Some(text.to_string()),
            name: None,
            extra: Default::default(),
        }
    }

    /// A consumer holds its spinner open until the closing event. Started first, finished last.
    #[test]
    fn a_run_opens_and_closes() {
        let events = plan_run(&input(vec![user("hello")]));
        assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    }

    /// A message must be started before it is filled and ended after — out of order, a consumer
    /// renders content into a message it has not created.
    #[test]
    fn the_message_events_are_properly_nested() {
        let events = plan_run(&input(vec![user("hello")]));
        let types: Vec<_> = events.iter().map(|event| event.event_type).collect();
        assert_eq!(
            types,
            vec![
                EventType::RunStarted,
                EventType::TextMessageStart,
                EventType::TextMessageContent,
                EventType::TextMessageEnd,
                EventType::RunFinished,
            ]
        );
    }

    /// Every event of one message must carry the same id, or the pieces land in different bubbles.
    #[test]
    fn the_message_id_is_stable_across_its_events() {
        let events = plan_run(&input(vec![user("hello")]));
        let ids: Vec<_> = events
            .iter()
            .filter_map(|event| event.extra.get("messageId"))
            .collect();
        assert_eq!(ids.len(), 3, "start, content and end each carry the id");
        assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    }

    /// The client's own ids come back untouched — it correlates its UI against them.
    #[test]
    fn the_clients_thread_and_run_ids_are_echoed_not_minted() {
        let events = plan_run(&input(vec![user("hello")]));
        let started = &events[0];
        assert_eq!(started.extra.get("threadId").unwrap(), "t1");
        assert_eq!(started.extra.get("runId").unwrap(), "r1");
    }

    #[test]
    fn the_last_user_message_reaches_the_run() {
        let events = plan_run(&input(vec![user("first"), user("second")]));
        let delta = events[2].extra.get("delta").unwrap().as_str().unwrap();
        assert!(delta.contains("second"), "{delta}");
    }

    /// A run with no user message must still be a well-formed run, not a panic or an empty stream.
    #[test]
    fn a_run_with_no_messages_still_opens_and_closes() {
        let events = plan_run(&input(vec![]));
        assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    }

    #[test]
    fn every_event_renders_to_a_single_sse_frame() {
        for event in plan_run(&input(vec![user("hello")])) {
            let frame = event.to_sse_frame().unwrap();
            assert!(frame.starts_with("data: "));
            assert!(frame.ends_with("\n\n"));
            assert_eq!(frame.matches("\n\n").count(), 1, "{frame:?}");
        }
    }
}
