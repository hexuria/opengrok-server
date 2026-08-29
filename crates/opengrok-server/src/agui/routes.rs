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
use opengrok_harness::{ChatMessage, ModelDoor, ModelRequest, ToolRunner, run_conversation};
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
    /// The tools this server offers, already bound to whose computer they run on. `None` until a
    /// coworker with a box is resolved from the session — see `docs/GOAL.md`.
    pub tools: Option<Arc<ToolRunner>>,
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

    // The journal writes each round to Postgres before the next model call. A run that cannot be
    // recorded fails inside the loop rather than being streamed — better a client told the run
    // failed than one shown work that will not be there when it looks again (CLAUDE.md #5).
    let journal = StoreJournal {
        state: state.clone(),
        thread_id: input.thread_id.clone(),
    };

    let events = run_conversation(
        state.door.as_ref(),
        state.tools.as_deref(),
        &journal,
        request,
        &input.thread_id,
        &input.run_id,
        now_ms(),
    )
    .await;

    sse(stream::iter(
        events.into_iter().map(Ok::<_, std::io::Error>),
    ))
}

/// The event store, as the harness's journal.
///
/// The harness owns *when* to write (before the next model call); this owns *where*. Keeping them
/// apart is what lets the ordering rule be tested without a database and still be enforced against
/// one.
pub struct StoreJournal {
    pub state: AgUiState,
    pub thread_id: String,
}

#[async_trait::async_trait]
impl opengrok_harness::RunJournal for StoreJournal {
    async fn record(
        &self,
        run_id: &str,
        events: &[Event],
    ) -> Result<(), opengrok_harness::JournalError> {
        append_events(&self.state, run_id, &self.thread_id, events)
            .await
            .map_err(|error| opengrok_harness::JournalError::Unwritable(error.to_string()))
    }
}

/// Append a batch of a run's events to the log, starting the run if this is its first batch.
async fn append_events(
    state: &AgUiState,
    run_id: &str,
    thread_id: &str,
    events: &[Event],
) -> Result<(), opengrok_store::StoreError> {
    if events.is_empty() {
        return Ok(());
    }
    let run_id = RunId::from_stored(run_id.to_string());
    let at_ms = now_ms();

    let (mut run, seq) = state.auth.store.load_run(&run_id).await?;
    let mut to_append = Vec::new();

    if !run.started {
        let started = run
            .decide(RunCommand::Start {
                thread_id: thread_id.to_string(),
                coworker_id: None,
                at_ms,
            })
            .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
        for event in &started {
            run.apply(event);
        }
        to_append.extend(started);
    }

    for event in events {
        let payload = serde_json::to_value(event)
            .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
        // The aggregate refuses a frame after an ending; that is a rule, not a hiccup.
        let Ok(decided) = run.decide(RunCommand::Emit { payload, at_ms }) else {
            break;
        };
        for decided_event in &decided {
            run.apply(decided_event);
        }
        to_append.extend(decided);

        // The run's own ending, recorded once, from the event that carries it.
        let closing = match event.event_type {
            opengrok_wire::agui::EventType::RunFinished => {
                Some(run.decide(RunCommand::Finish { at_ms }))
            }
            opengrok_wire::agui::EventType::RunError => Some(
                run.decide(RunCommand::Fail {
                    reason: event
                        .extra
                        .get("message")
                        .and_then(|message| message.as_str())
                        .unwrap_or("the run failed")
                        .to_string(),
                    at_ms,
                }),
            ),
            _ => None,
        };
        if let Some(Ok(closing)) = closing {
            for closing_event in &closing {
                run.apply(closing_event);
            }
            to_append.extend(closing);
        }
    }

    let view = RunView {
        id: run_id.clone(),
        thread_id: thread_id.to_string(),
        status: run.status,
        event_count: run.emitted.len() as i64,
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
        let events = opengrok_harness::run_conversation(
            &door,
            None,
            &opengrok_harness::MemoryJournal::new(),
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
