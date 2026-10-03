//! `events.rs`: the notes as NativeChat reads them, field by field.

use super::*;
use serde_json::{Value, json};

fn data(note: &Note) -> Value {
    serde_json::from_str(&note.data()).unwrap()
}

/// The names and fields are the contract's (nativechat#171); a tidier spelling is a client that
/// reads nothing. Every value is an id or one of the history's words.
#[test]
fn every_note_says_what_changed_in_ids_and_the_contracts_names() {
    let thread = Note::ThreadChanged {
        thread_id: "sched_1",
        coworker_id: "cw_1",
    };
    assert_eq!(thread.event(), "thread.changed");
    assert_eq!(
        data(&thread),
        json!({ "threadId": "sched_1", "coworkerId": "cw_1" })
    );

    let started = Note::RunStarted {
        run_id: "run_1",
        thread_id: "sched_1",
        coworker_id: "cw_1",
        routine_id: Some("sched_1"),
        cause: "bot",
    };
    assert_eq!(started.event(), "run.started");
    assert_eq!(
        data(&started),
        json!({ "runId": "run_1", "threadId": "sched_1", "coworkerId": "cw_1",
                "routineId": "sched_1", "cause": "bot" })
    );

    let finished = Note::RunFinished {
        run_id: "run_1",
        thread_id: "sched_1",
        coworker_id: "cw_1",
        routine_id: Some("sched_1"),
        state: "ok",
    };
    assert_eq!(finished.event(), "run.finished");
    assert_eq!(
        data(&finished),
        json!({ "runId": "run_1", "threadId": "sched_1", "coworkerId": "cw_1",
                "routineId": "sched_1", "state": "ok" })
    );

    let routine = Note::RoutineChanged {
        routine_id: "sched_1",
        coworker_id: "cw_1",
        change: Change::Paused,
    };
    assert_eq!(routine.event(), "routine.changed");
    assert_eq!(
        data(&routine),
        json!({ "routineId": "sched_1", "coworkerId": "cw_1", "change": "paused" })
    );
}

/// `routineId?` is left out, not null, for a run no routine fired: an app that reads a present
/// key as a routine would otherwise look one up named `null`.
#[test]
fn a_run_no_routine_fired_has_no_routine_id_at_all() {
    let started = Note::RunStarted {
        run_id: "run_1",
        thread_id: "t",
        coworker_id: "cw_1",
        routine_id: None,
        cause: "chat",
    };
    assert!(data(&started).get("routineId").is_none());
    let finished = Note::RunFinished {
        run_id: "run_1",
        thread_id: "t",
        coworker_id: "cw_1",
        routine_id: None,
        state: "error",
    };
    assert!(data(&finished).get("routineId").is_none());
}

/// Five changes, in the contract's words.
#[test]
fn a_routine_changes_in_five_words() {
    let said: Vec<Value> = [
        Change::Created,
        Change::Updated,
        Change::Deleted,
        Change::Paused,
        Change::Resumed,
    ]
    .iter()
    .map(|change| serde_json::to_value(change).unwrap())
    .collect();
    assert_eq!(
        said,
        ["created", "updated", "deleted", "paused", "resumed"].map(Value::from)
    );
}

/// A block is three fields and a blank line; a ping is a comment. `data` is one line even when an
/// id the client chose has a newline in it: JSON escapes it.
#[test]
fn a_block_is_id_event_and_one_line_of_data() {
    assert_eq!(block(7, RESET, "{}"), "id: 7\nevent: reset\ndata: {}\n\n");
    assert_eq!(PING, ": ping\n\n");
    let hostile = Note::ThreadChanged {
        thread_id: "a\nb\r\nid: 99",
        coworker_id: "cw_1",
    };
    let sent = block(8, hostile.event(), &hostile.data());
    assert_eq!(sent.matches('\n').count(), 4, "{sent:?}");
    assert!(!sent.contains('\r'), "{sent:?}");
}

/// `EVENTS` is what the corpus lists as sent: one name per kind of block, and the reset.
#[test]
fn the_list_of_names_is_every_kind_and_the_reset() {
    assert_eq!(
        EVENTS,
        [
            "thread.changed",
            "run.started",
            "run.finished",
            "routine.changed",
            "reset"
        ]
    );
}
