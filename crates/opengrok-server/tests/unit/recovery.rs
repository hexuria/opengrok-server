#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use opengrok_core::run::{Run, RunEvent};
use serde_json::json;

fn run_with(events: Vec<serde_json::Value>) -> Run {
    let mut log = vec![RunEvent::Started {
        thread_id: "t1".to_string(),
        coworker_id: None,
        model: None,
        system: None,
        skill_id: None,
        prompt: None,
        at_ms: 1,
    }];
    for (index, payload) in events.into_iter().enumerate() {
        log.push(RunEvent::Emitted {
            seq: index as i64,
            payload,
            at_ms: 2,
        });
    }
    Run::replay(&log)
}

/// The ambiguous case: a call went out and no result came back.
#[test]
fn a_tool_call_without_a_result_is_the_unresolved_one() {
    let run = run_with(vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "shell"}),
    ]);
    assert_eq!(unresolved_tool_call(&run).as_deref(), Some("shell"));
}

/// A completed call is settled and must not be reported as in flight — that would tell a
/// person their command might have run when the log says it did.
#[test]
fn a_tool_call_with_a_result_is_settled() {
    let run = run_with(vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "shell"}),
        json!({"type": "TOOL_CALL_RESULT", "toolCallId": "c1", "content": "done"}),
    ]);
    assert_eq!(unresolved_tool_call(&run), None);
}

/// With several calls, the one still open is the one that matters.
#[test]
fn the_open_call_is_found_among_settled_ones() {
    let run = run_with(vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "read_file"}),
        json!({"type": "TOOL_CALL_RESULT", "toolCallId": "c1", "content": "ok"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "shell"}),
    ]);
    assert_eq!(unresolved_tool_call(&run).as_deref(), Some("shell"));
}

/// A run that only talked has nothing in flight.
#[test]
fn a_run_with_no_tools_has_nothing_unresolved() {
    let run = run_with(vec![
        json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "hello"}),
    ]);
    assert_eq!(unresolved_tool_call(&run), None);
}
