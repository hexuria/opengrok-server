//! The person's side of a conversation, kept on the run, and the history a turn is asked with.
//!
//! NOTHING THAT MATTERS LIVES IN A CLIENT (CLAUDE.md #5). A run used to journal only what the
//! coworker emitted, so a second device rebuilt a thread as answers with no questions, a resumed
//! run forgot what it was asked, and the model's history was whatever the client sent — including
//! assistant turns the coworker never said (#6). Each turn now journals the messages that were
//! new to it on `RunEvent::Started::prompt`, and a signed-in turn's history is composed from the
//! thread's own log.
//!
//! THE CLIENT'S COPY STILL WINS ON A THREAD THE LOG CANNOT TELL WHOLE. A run written before
//! prompts were journaled has only the coworker's half, and composing from it would hand the
//! model answers with no questions, which is worse than what NativeChat's whole-bubble send
//! already gives it. So one such run in the window sends the turn back to the client's messages,
//! exactly as before; the window slides past it within `HISTORY_RUNS` turns.

use std::collections::HashSet;

use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::Run;
use opengrok_harness::ChatMessage;
use opengrok_wire::agui::{Message, RunAgentInput};
use serde_json::{Value, json};

use super::routes::{
    AgUiState, STEER_CONTINUATION, STEER_TOOL_CAP, STEER_TOOL_CHARS, chat_message, clip_chars,
    prior_turn_can_continue, to_chat_messages,
};

/// How many of a thread's runs a turn's history reaches back over — the same twenty a thread's
/// replay answers with, so the model is never asked about turns the person cannot scroll to.
pub(crate) const HISTORY_RUNS: i64 = 20;

/// Arguments of an earlier turn's call, bounded like the steer splice bounds them.
const EARLIER_ARGS_CHARS: usize = 400;

/// What a turn is asked with, and what it journals about the person.
pub(crate) struct Asked {
    pub messages: Vec<ChatMessage>,
    /// The messages new to this turn, verbatim, for `RunEvent::Started::prompt`.
    pub prompt: Vec<Value>,
    /// The history is the client's copy (an anonymous turn, a thread with a run from before
    /// prompts were journaled, or a log that could not be read). The stopped-turn splice only
    /// applies here: a composed history already carries the stopped turn's tools.
    pub from_client: bool,
}

/// The conversation a live turn is asked with.
pub(crate) async fn for_turn(
    state: &AgUiState,
    account: Option<&AccountId>,
    input: &RunAgentInput,
) -> Asked {
    let prior = match account {
        Some(account) => thread_runs(state, account, &input.thread_id, &input.run_id).await,
        None => None,
    };
    let runs = prior.as_deref().unwrap_or_default();
    let seen: HashSet<String> = runs
        .iter()
        .flat_map(|run| run.prompt.iter().flatten())
        .filter_map(|message| message.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    let calls: Option<HashSet<String>> = prior.as_ref().map(|runs| {
        runs.iter()
            .flat_map(|run| said_in(&run.emitted))
            .filter_map(|said| match said {
                Said::Call { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    });
    let new = new_turn_messages(input, &seen, calls.as_ref());
    let prompt = new
        .iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .collect();
    let whole = prior
        .as_ref()
        .is_some_and(|runs| runs.iter().all(|run| run.prompt.is_some()));
    if !whole {
        return Asked {
            messages: to_chat_messages(input),
            prompt,
            from_client: true,
        };
    }
    let mut messages = thread_messages(runs);
    messages.extend(
        new.iter()
            .filter_map(|message| chat_message(message, &input.messages)),
    );
    Asked {
        messages,
        prompt,
        from_client: false,
    }
}

/// The thread's earlier started runs, oldest first, or `None` when they could not be read.
///
/// A READ THAT FAILS FALLS BACK RATHER THAN FAILING THE TURN. The client's copy is what every
/// turn was asked with before this, so a database blink costs the turn its server-kept history,
/// not the turn itself.
async fn thread_runs(
    state: &AgUiState,
    account: &AccountId,
    thread_id: &str,
    this_run: &str,
) -> Option<Vec<Run>> {
    let newest_first = match state
        .auth
        .store
        .runs_for_thread_owned_by(thread_id, account, HISTORY_RUNS)
        .await
    {
        Ok(runs) => runs,
        Err(error) => {
            tracing::warn!(%error, thread = thread_id, "could not read a thread's history");
            return None;
        }
    };
    let mut runs = Vec::with_capacity(newest_first.len());
    for summary in newest_first.into_iter().rev() {
        if summary.id.as_str() == this_run {
            continue;
        }
        let run = match state.auth.store.load_run(&summary.id).await {
            Ok((run, _)) => run,
            Err(error) => {
                tracing::warn!(%error, run = %summary.id, "could not read a run of a thread's history");
                return None;
            }
        };
        if run.started {
            runs.push(run);
        }
    }
    Some(runs)
}

/// The messages this turn brings: those after the client's last assistant message, less any an
/// earlier run on the thread already journaled under the same client id.
///
/// AFTER THE LAST ASSISTANT MESSAGE, because a client that sends its whole bubble list re-sends
/// every question already answered, and one that sends only the newest message sends no answer
/// at all — both end in the same tail. The dedupe covers a turn that never answered: its question
/// is re-sent beside the next one, and was already journaled once.
///
/// `tool` IS KEPT BESIDE `user`: NativeChat continues a frontend tool (a `form` the person filled)
/// by sending its result as a tool message, and that result is the person's answer. But only for
/// a call the thread's log shows the coworker making (`calls`, when the log could be read): a
/// result for a call it never made is the client writing the coworker's past for it (CLAUDE.md
/// #6).
pub(crate) fn new_turn_messages<'a>(
    input: &'a RunAgentInput,
    journaled: &HashSet<String>,
    calls: Option<&HashSet<String>>,
) -> Vec<&'a Message> {
    let after = input
        .messages
        .iter()
        .rposition(|message| message.role == "assistant")
        .map_or(0, |at| at + 1);
    input.messages[after..]
        .iter()
        .filter(|message| match message.role.as_str() {
            "user" => true,
            "tool" => calls.is_none_or(|calls| {
                message
                    .extra
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| calls.contains(id))
            }),
            _ => false,
        })
        .filter(|message| message.id.is_empty() || !journaled.contains(&message.id))
        .collect()
}

/// A run's journaled prompt, read back as the messages the client sent.
fn prompt_of(run: &Run) -> Vec<Message> {
    run.prompt
        .iter()
        .flatten()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect()
}

/// What a run's emitted frames say, in order: the coworker's words, its calls, their results.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Said {
    Text(String),
    Call {
        id: String,
        name: String,
        arguments: String,
    },
    Result {
        id: String,
        content: String,
    },
}

/// Read a run's frames back into what was said.
///
/// A CALL'S LAST RESULT IS ITS RESULT. A call that parked on a card has a "waiting for approval"
/// result, and a second one once somebody answered; showing both would tell the model the call
/// both waited and ran.
pub(crate) fn said_in(emitted: &[Value]) -> Vec<Said> {
    let mut said = Vec::new();
    let mut text = String::new();
    for payload in emitted {
        let field = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or("");
        match field("type") {
            "TEXT_MESSAGE_CONTENT" => text.push_str(field("delta")),
            // An empty message is skipped rather than kept: a provider that rejects empty content
            // would fail the whole turn over nothing.
            "TEXT_MESSAGE_END" if !text.is_empty() => {
                said.push(Said::Text(std::mem::take(&mut text)))
            }
            "TOOL_CALL_START" => said.push(Said::Call {
                id: field("toolCallId").to_string(),
                name: field("toolCallName").to_string(),
                arguments: String::new(),
            }),
            "TOOL_CALL_ARGS" => {
                let id = field("toolCallId");
                if let Some(Said::Call { arguments, .. }) = said
                    .iter_mut()
                    .rev()
                    .find(|said| matches!(said, Said::Call { id: call, .. } if call == id))
                {
                    arguments.push_str(field("delta"));
                }
            }
            "TOOL_CALL_RESULT" => said.push(Said::Result {
                id: field("toolCallId").to_string(),
                content: field("content").to_string(),
            }),
            _ => {}
        }
    }
    let mut kept = HashSet::new();
    let mut last_first: Vec<Said> = said
        .into_iter()
        .rev()
        .filter(|said| match said {
            Said::Result { id, .. } => kept.insert(id.clone()),
            _ => true,
        })
        .collect();
    last_first.reverse();
    last_first
}

/// One earlier run's part of a conversation: the person's messages, then what the coworker said
/// and a bounded line per call it made — the last `STEER_TOOL_CAP` of them, so a long session on
/// the screen does not become the next prompt.
fn earlier_run(run: &Run) -> Vec<ChatMessage> {
    let sent = prompt_of(run);
    let mut messages: Vec<ChatMessage> = sent
        .iter()
        .filter_map(|message| chat_message(message, &sent))
        .collect();
    let said = said_in(&run.emitted);
    let results = said
        .iter()
        .filter(|said| matches!(said, Said::Result { .. }))
        .count();
    let mut skip = results.saturating_sub(STEER_TOOL_CAP);
    for entry in &said {
        match entry {
            Said::Text(text) => messages.push(ChatMessage::text("assistant", text.clone())),
            Said::Result { id, content } if !content.is_empty() => {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let (name, arguments) = said
                    .iter()
                    .find_map(|said| match said {
                        Said::Call {
                            id: call,
                            name,
                            arguments,
                        } if call == id => Some((name.as_str(), arguments.as_str())),
                        _ => None,
                    })
                    .unwrap_or(("tool", ""));
                messages.push(ChatMessage::text(
                    "user",
                    format!(
                        "[earlier {name} {}] {}",
                        clip_chars(arguments, EARLIER_ARGS_CHARS),
                        clip_chars(content, STEER_TOOL_CHARS)
                    ),
                ));
            }
            _ => {}
        }
    }
    messages
}

/// Every earlier run in order, and — when the newest one stopped or failed partway through its
/// tools — the sentence that tells the model to continue from them rather than start again.
fn thread_messages(runs: &[Run]) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = runs.iter().flat_map(earlier_run).collect();
    if let Some(newest) = runs.last()
        && prior_turn_can_continue(newest.status, &newest.emitted)
        && said_in(&newest.emitted)
            .iter()
            .any(|said| matches!(said, Said::Result { .. }))
    {
        messages.push(ChatMessage::text("user", STEER_CONTINUATION.to_string()));
    }
    messages
}

/// A run's frames as a client is handed them: the person's messages as `TEXT_MESSAGE_*` with
/// `role: "user"`, under the id their client gave them, right after the run's `RUN_STARTED`.
///
/// AFTER `RUN_STARTED`, NOT BEFORE IT. AG-UI opens a run with that frame and a verifying client
/// refuses anything ahead of it; the person's words still come before everything the coworker
/// said. `role: "user"` on `TEXT_MESSAGE_START` is the spec's own (`@ag-ui/core` 0.0.57
/// `TextMessageRoleSchema`, dist/index.js:349), and only a `user` message with words is drawn:
/// a tool result the person's client sent stays in the log and out of the bubbles.
pub(crate) fn with_prompt_frames(run: &Run, mut events: Vec<Value>) -> Vec<Value> {
    let at = usize::from(
        events
            .first()
            .is_some_and(|event| event.get("type").and_then(Value::as_str) == Some("RUN_STARTED")),
    );
    // The run's own opening time: `timestamp` is optional in the spec but a number when present,
    // so a run with none gives its prompt frames none rather than a `null`.
    let timestamp = events
        .first()
        .and_then(|event| event.get("timestamp"))
        .filter(|at| at.is_number())
        .cloned();
    let frame = |mut frame: Value| {
        if let (Some(at), Some(object)) = (&timestamp, frame.as_object_mut()) {
            object.insert("timestamp".to_string(), at.clone());
        }
        frame
    };
    let frames: Vec<Value> = prompt_of(run)
        .into_iter()
        .filter(|message| message.role == "user")
        .filter_map(|message| {
            let words = message.content.filter(|words| !words.is_empty())?;
            let id = message.id;
            Some([
                frame(json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "user"})),
                frame(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": id, "delta": words})),
                frame(json!({"type": "TEXT_MESSAGE_END", "messageId": id})),
            ])
        })
        .flatten()
        .collect();
    events.splice(at..at, frames);
    events
}

/// The id a routine's journaled instruction goes under: the run's own, so it is unique and says
/// where it came from.
pub(crate) fn routine_prompt(run_id: &RunId, instruction: &str) -> Vec<Value> {
    vec![
        json!({"id": format!("{}-prompt", run_id.as_str()), "role": "user", "content": instruction}),
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../../tests/unit/history.rs"]
mod tests;
