//! Unit tests for `agui::history`, kept out of the crate's source count.

use super::*;
use opengrok_core::run::{RunCommand, RunEvent};
use serde_json::json;

fn input(messages: Value) -> RunAgentInput {
    serde_json::from_value(json!({"threadId": "t1", "runId": "r1", "messages": messages})).unwrap()
}

fn ids(messages: &[&Message]) -> Vec<String> {
    messages.iter().map(|message| message.id.clone()).collect()
}

fn run_with(prompt: Option<Vec<Value>>, emitted: Vec<Value>) -> Run {
    let mut run = Run::default();
    let started = run
        .decide(RunCommand::Start {
            thread_id: "t1".to_string(),
            coworker_id: None,
            model: None,
            system: None,
            skill_id: None,
            prompt,
            at_ms: 1,
        })
        .unwrap();
    for event in &started {
        run.apply(event);
    }
    for (seq, payload) in emitted.into_iter().enumerate() {
        run.apply(&RunEvent::Emitted {
            seq: seq as i64,
            payload,
            at_ms: 2,
        });
    }
    run
}

/// A whole-bubble send and a newest-only send end in the same tail.
#[test]
fn a_turn_brings_what_follows_the_last_answer() {
    let whole = input(json!([
        {"id": "m1", "role": "user", "content": "my name is Juana"},
        {"id": "a1", "role": "assistant", "content": "Hello Juana"},
        {"id": "m2", "role": "user", "content": "what is my name?"},
    ]));
    let newest = input(json!([{"id": "m2", "role": "user", "content": "what is my name?"}]));
    let none = HashSet::new();
    assert_eq!(ids(&new_turn_messages(&whole, &none, None)), ["m2"]);
    assert_eq!(ids(&new_turn_messages(&newest, &none, None)), ["m2"]);
}

/// A question an earlier turn already journaled is not journaled again, and a frontend tool's
/// result — the person's answer to a form — is kept beside the words.
#[test]
fn a_question_already_kept_is_not_kept_twice() {
    let resent = input(json!([
        {"id": "m1", "role": "user", "content": "my name is Juana"},
        {"id": "m2", "role": "user", "content": "hello?"},
        {"id": "t1", "role": "tool", "toolCallId": "c9", "content": "{\"pick\":\"b\"}"},
        {"id": "s1", "role": "system", "content": "SPEAK FRENCH"},
    ]));
    let journaled: HashSet<String> = ["m1".to_string()].into();
    let made: HashSet<String> = ["c9".to_string()].into();
    assert_eq!(
        ids(&new_turn_messages(&resent, &journaled, Some(&made))),
        ["m2", "t1"]
    );
    // A result for a call the log never shows is the client writing the coworker's past.
    assert_eq!(
        ids(&new_turn_messages(
            &resent,
            &journaled,
            Some(&HashSet::new())
        )),
        ["m2"]
    );
}

/// A call that parked and was then answered has two results; only the last one is its result.
#[test]
fn a_calls_last_result_is_its_result() {
    let said = said_in(&[
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "shell"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{\"command\":"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "\"ls\"}"}),
        json!({"type": "TOOL_CALL_RESULT", "toolCallId": "c1", "content": "waiting for approval: x"}),
        json!({"type": "CUSTOM", "name": "run-awaiting-approval"}),
        json!({"type": "TOOL_CALL_RESULT", "toolCallId": "c1", "content": "a b c"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "x"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "x", "delta": "three files"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "x"}),
    ]);
    assert_eq!(
        said,
        vec![
            Said::Call {
                id: "c1".into(),
                name: "shell".into(),
                arguments: "{\"command\":\"ls\"}".into()
            },
            Said::Result {
                id: "c1".into(),
                content: "a b c".into(),
                image: None,
            },
            Said::Text("three files".into()),
        ]
    );
}

/// An earlier turn reads back as the question, what was run, and the answer, in that order.
#[test]
fn an_earlier_turn_is_its_question_its_calls_and_its_answer() {
    let run = run_with(
        Some(vec![
            json!({"id": "m1", "role": "user", "content": "count the files"}),
        ]),
        vec![
            json!({"type": "RUN_STARTED"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "shell"}),
            json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{\"command\":\"ls\"}"}),
            json!({"type": "TOOL_CALL_RESULT", "toolCallId": "c1", "content": "a b c"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "three"}),
            json!({"type": "TEXT_MESSAGE_END"}),
            json!({"type": "RUN_FINISHED"}),
        ],
    );
    let mut run = run;
    run.apply(&RunEvent::Finished { at_ms: 3 });
    let messages = thread_messages(std::slice::from_ref(&run));
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message.role.as_str())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "tool", "assistant"],
        "{messages:?}"
    );
    assert_eq!(messages[0].content, "count the files");
    assert_eq!(
        messages[1].tool_calls,
        vec![ToolCallRef {
            id: "c1".into(),
            name: "shell".into(),
            arguments: "{\"command\":\"ls\"}".into()
        }],
        "the model sees the call it made, not a line about it (#189)"
    );
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(messages[2].content, "a b c");
    assert_eq!(messages[3].content, "three");
}

/// A long earlier session keeps its last calls, each still answered, and its arguments still JSON.
#[test]
fn an_earlier_turn_keeps_its_last_calls_whole_and_bounded() {
    let mut said = Vec::new();
    for n in 0..(STEER_TOOL_CAP + 3) {
        said.push(Said::Call {
            id: format!("c{n}"),
            name: "shell".into(),
            arguments: json!({"command": "x".repeat(EARLIER_ARGS_CHARS)}).to_string(),
        });
        said.push(Said::Result {
            id: format!("c{n}"),
            content: "y".repeat(STEER_TOOL_CHARS * 2),
            image: Some(ImagePart {
                mime: "image/png".into(),
                base64: "AAAA".into(),
            }),
        });
    }
    let messages = conversation_of(&said, true);
    assert_eq!(messages.len(), STEER_TOOL_CAP * 2);
    assert_eq!(messages[0].tool_calls[0].id, "c3");
    for message in &messages {
        assert!(
            message.images.is_empty(),
            "an earlier screen is left behind"
        );
        for call in &message.tool_calls {
            assert!(
                serde_json::from_str::<Value>(&call.arguments).is_ok(),
                "{call:?}"
            );
        }
        assert!(message.content.chars().count() <= STEER_TOOL_CHARS + 1);
    }
}

/// AG-UI's own shapes for a call and its result round-trip into tool calls, not words: a
/// call-only assistant message used to be dropped for having no content (#189).
#[test]
fn a_tool_call_only_assistant_message_round_trips() {
    let messages = to_chat_messages(&input(json!([
        {"id": "a1", "role": "assistant", "toolCalls": [
            {"id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{}"}}
        ]},
        {"id": "t1", "role": "tool", "toolCallId": "c1", "content": "ok"},
    ])));
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0].role, "assistant");
    assert_eq!(messages[0].tool_calls[0].id, "c1");
    assert_eq!(messages[0].tool_calls[0].name, "shell");
    assert_eq!(messages[1].role, "tool");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("c1"));
}

/// A replayed run opens with `RUN_STARTED`, then the person's words under their client's id,
/// then everything the coworker said. A tool result the client sent is kept in the log and not
/// drawn as a bubble.
#[test]
fn a_replayed_run_draws_the_question_after_it_opens() {
    let run = run_with(
        Some(vec![
            json!({"id": "m1", "role": "user", "content": "hi", "replyTo": "a0"}),
            json!({"id": "t1", "role": "tool", "toolCallId": "c9", "content": "{}"}),
        ]),
        Vec::new(),
    );
    let events = with_prompt_frames(
        &run,
        vec![
            json!({"type": "RUN_STARTED", "timestamp": 7}),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "x", "role": "assistant"}),
        ],
    );
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "TEXT_MESSAGE_START"
        ]
    );
    assert_eq!(events[1]["role"], "user");
    assert_eq!(events[1]["messageId"], "m1");
    assert_eq!(events[1]["timestamp"], 7);
    assert_eq!(events[2]["delta"], "hi");
}

/// A run from before prompts were journaled replays exactly as it always did.
#[test]
fn a_run_with_no_journaled_prompt_replays_unchanged() {
    let run = run_with(None, Vec::new());
    let events = vec![
        json!({"type": "RUN_STARTED"}),
        json!({"type": "RUN_FINISHED"}),
    ];
    assert_eq!(with_prompt_frames(&run, events.clone()), events);
}
