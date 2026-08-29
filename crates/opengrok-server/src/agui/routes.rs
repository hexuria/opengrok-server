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

use axum::extract::Path;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream};
use opengrok_wire::agui::{Event, RunAgentInput};

use crate::auth::AuthState;
use opengrok_core::id::RunId;
use opengrok_core::run::{RunCommand, RunStatus, RunView};
use opengrok_harness::{ChatMessage, ModelDoor, ModelRequest, run_turn};
use std::sync::Arc;

/// What the endpoint needs: a way to reach a model, and which route to ask for.
///
/// The door is a trait object so `OG_MODEL_DOOR=mock` swaps the whole model layer without the
/// endpoint, the harness or the projection knowing — which is what lets CI exercise this path.
#[derive(Clone)]
pub struct AgUiState {
    pub auth: AuthState,
    pub door: Arc<dyn ModelDoor>,
    pub model: String,
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/ag-ui", post(run))
        .route("/ag-ui/runs/{run_id}", get(replay_run))
        .with_state(state)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Start a run and stream its events.
pub async fn run(State(state): State<AgUiState>, Json(input): Json<RunAgentInput>) -> Response {
    let request = ModelRequest {
        model: state.model.clone(),
        system: None,
        messages: to_chat_messages(&input),
    };

    let events = run_turn(
        state.door.as_ref(),
        request,
        &input.thread_id,
        &input.run_id,
        now_ms(),
    )
    .await;

    // STORE BEFORE SENDING. A frame the client received but we never wrote is work a reconnect
    // cannot reproduce — which is the failure this whole project exists to prevent (CLAUDE.md #5).
    // A run that cannot be recorded is therefore refused rather than streamed: better a client that
    // is told the run failed than one shown work that will not be there when it looks again.
    if let Err(error) = persist_run(&state, &input, &events).await {
        tracing::error!(run_id = %input.run_id, %error, "refusing to stream a run we cannot record");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the run could not be recorded, so it was not started",
        )
            .into_response();
    }

    sse(stream::iter(
        events.into_iter().map(Ok::<_, std::io::Error>),
    ))
}

/// Append a whole run to the log, in the order the client will see it.
async fn persist_run(
    state: &AgUiState,
    input: &RunAgentInput,
    events: &[Event],
) -> Result<(), opengrok_store::StoreError> {
    let run_id = RunId::from_stored(input.run_id.clone());
    let at_ms = now_ms();

    let (run, seq) = state.auth.store.load_run(&run_id).await?;
    let mut state_machine = run;
    let mut to_append = Vec::new();

    if !state_machine.started {
        let started = state_machine
            .decide(RunCommand::Start {
                thread_id: input.thread_id.clone(),
                coworker_id: None,
                at_ms,
            })
            .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
        for event in &started {
            state_machine.apply(event);
        }
        to_append.extend(started);
    }

    let mut ended = false;
    for event in events {
        let payload = serde_json::to_value(event)
            .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
        // The aggregate refuses a frame after an ending; that is a real rule, not a hiccup, so the
        // remaining frames are dropped rather than forced in.
        let Ok(decided) = state_machine.decide(RunCommand::Emit { payload, at_ms }) else {
            break;
        };
        for decided_event in &decided {
            state_machine.apply(decided_event);
        }
        to_append.extend(decided);

        if matches!(
            event.event_type,
            opengrok_wire::agui::EventType::RunFinished | opengrok_wire::agui::EventType::RunError
        ) {
            ended = true;
        }
    }

    if ended {
        let closing = match events.last().map(|event| event.event_type) {
            Some(opengrok_wire::agui::EventType::RunError) => {
                state_machine.decide(RunCommand::Fail {
                    reason: events
                        .last()
                        .and_then(|event| event.extra.get("message"))
                        .and_then(|message| message.as_str())
                        .unwrap_or("the run failed")
                        .to_string(),
                    at_ms,
                })
            }
            _ => state_machine.decide(RunCommand::Finish { at_ms }),
        };
        if let Ok(closing) = closing {
            for event in &closing {
                state_machine.apply(event);
            }
            to_append.extend(closing);
        }
    }

    let view = RunView {
        id: run_id.clone(),
        thread_id: input.thread_id.clone(),
        status: state_machine.status,
        event_count: state_machine.emitted.len() as i64,
        updated_at_ms: at_ms,
    };

    state
        .auth
        .store
        .append_run(&run_id, seq, &to_append, &view)
        .await?;
    Ok(())
}

/// Replay a run from the log.
///
/// THIS IS THE PROMISE, MADE CHECKABLE. Close the tab mid-run, come back, ask here: every event
/// the run produced is returned, in order, without asking a model anything a second time.
pub async fn replay_run(State(state): State<AgUiState>, Path(run_id): Path<String>) -> Response {
    let run_id = RunId::from_stored(run_id);
    let (run, _) = match state.auth.store.load_run(&run_id).await {
        Ok(loaded) => loaded,
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    };

    if !run.started {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }

    Json(serde_json::json!({
        "runId": run_id.as_str(),
        "threadId": run.thread_id,
        "status": match run.status {
            RunStatus::Running => "running",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
        },
        "failure": run.failure,
        "events": run.emitted,
    }))
    .into_response()
}

/// AG-UI messages to the model's vocabulary.
///
/// Roles the model door does not understand are dropped rather than passed through: a provider
/// that rejects an unknown role fails the whole turn, and AG-UI carries roles (`developer`) that
/// have no place in a chat completion.
pub fn to_chat_messages(input: &RunAgentInput) -> Vec<ChatMessage> {
    input
        .messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "user" | "assistant" | "system"))
        .filter_map(|message| {
            message.content.as_ref().map(|content| ChatMessage {
                role: message.role.clone(),
                content: content.clone(),
            })
        })
        .collect()
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
    use opengrok_harness::MockDoor;
    use opengrok_wire::agui::{EventType, Message};
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

    fn message(role: &str, content: Option<&str>) -> Message {
        Message {
            id: "m1".to_string(),
            role: role.to_string(),
            content: content.map(str::to_string),
            name: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn chat_roles_the_model_understands_are_kept() {
        let messages = to_chat_messages(&input(vec![
            message("system", Some("be brief")),
            message("user", Some("hello")),
            message("assistant", Some("hi")),
        ]));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].content, "hello");
    }

    /// AG-UI carries roles a chat completion has no place for. Passing one through fails the whole
    /// turn on providers that reject unknown roles.
    #[test]
    fn roles_the_model_does_not_understand_are_dropped() {
        let messages = to_chat_messages(&input(vec![
            message("developer", Some("internal")),
            message("tool", Some("result")),
            message("user", Some("hello")),
        ]));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    /// A message with no content is a placeholder the client is still filling in.
    #[test]
    fn a_message_without_content_is_skipped() {
        let messages = to_chat_messages(&input(vec![message("user", None)]));
        assert!(messages.is_empty());
    }

    /// End to end through the mock door: no provider, no key, and still a complete run.
    #[tokio::test]
    async fn a_run_through_the_mock_door_is_well_formed() {
        let door = MockDoor::echoing();
        let events = run_turn(
            &door,
            ModelRequest {
                model: "mock".to_string(),
                system: None,
                messages: to_chat_messages(&input(vec![message("user", Some("ping"))])),
            },
            "t1",
            "r1",
            1,
        )
        .await;

        assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
        assert_eq!(events.first().unwrap().extra.get("threadId").unwrap(), "t1");

        for event in &events {
            let frame = event.to_sse_frame().unwrap();
            assert!(frame.starts_with("data: "));
            assert_eq!(frame.matches("\n\n").count(), 1, "{frame:?}");
        }
    }
}
