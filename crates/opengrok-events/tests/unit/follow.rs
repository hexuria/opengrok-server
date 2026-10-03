//! `follow.rs`: what a live read merges, and what it never does.

use serde_json::{Value, json};

use super::*;

fn thread(id: i64, name: &str, bot: &str, run: Value) -> Stored {
    Stored {
        id,
        kind: THREAD_CHANGED.to_string(),
        payload: json!({ "threadId": name, "coworkerId": bot, "runId": run }),
    }
}

fn run_started(id: i64, run: &str) -> Stored {
    Stored {
        id,
        kind: "run.started".to_string(),
        payload: json!({ "runId": run, "threadId": "t", "coworkerId": "cw_1", "cause": "chat" }),
    }
}

/// What is left, as `(id, runId)` of each `thread.changed` and the id alone of anything else.
fn left(notes: Vec<Stored>) -> Vec<(i64, Option<Value>)> {
    notes
        .into_iter()
        .map(|note| {
            let run = (note.kind == THREAD_CHANGED).then(|| note.payload["runId"].clone());
            (note.id, run)
        })
        .collect()
}

/// A run's own rounds merge into the last, which names the run: the app that is streaming that run
/// reads nothing, as it was asked to.
#[test]
fn one_runs_rounds_are_told_once_and_name_the_run() {
    let notes = vec![
        thread(1, "t", "cw_1", json!("run_a")),
        thread(2, "t", "cw_1", json!("run_a")),
        thread(3, "t", "cw_1", json!("run_a")),
    ];
    assert_eq!(left(coalesced(notes)), [(3, Some(json!("run_a")))]);
}

/// TWO RUNS' ROUNDS NEVER MERGE INTO ONE THAT NAMES EITHER. The last says `run_b`, and an app
/// streaming `run_b` would read nothing of what `run_a` did on the thread: `null` makes it read.
#[test]
fn two_runs_rounds_merge_into_a_note_that_names_nobody() {
    let notes = vec![
        thread(1, "t", "cw_1", json!("run_a")),
        thread(2, "t", "cw_1", json!("run_b")),
        thread(3, "t", "cw_1", json!("run_b")),
    ];
    assert_eq!(left(coalesced(notes)), [(3, Some(Value::Null))]);
}

/// A person's change (`null`) among a run's rounds, first, between or last: it is never lost to the
/// run, which would have the app that streams the run skip reading what the person did.
#[test]
fn a_persons_change_among_a_runs_rounds_is_never_lost_to_the_run() {
    for order in [[0, 1, 1], [1, 0, 1], [1, 1, 0]] {
        let notes = order
            .iter()
            .enumerate()
            .map(|(at, by_person)| {
                let run = if *by_person == 0 {
                    Value::Null
                } else {
                    json!("run_a")
                };
                thread(at as i64 + 1, "t", "cw_1", run)
            })
            .collect();
        assert_eq!(
            left(coalesced(notes)),
            [(3, Some(Value::Null))],
            "{order:?}"
        );
    }
}

/// Only a thread's own notes merge, and a Bot's: the same thread id under two Bots is two.
#[test]
fn threads_and_bots_are_kept_apart_each_with_its_own_run() {
    let notes = vec![
        thread(1, "t", "cw_1", json!("run_a")),
        thread(2, "u", "cw_1", json!("run_b")),
        thread(3, "t", "cw_2", json!("run_c")),
        thread(4, "t", "cw_1", json!("run_a")),
        thread(5, "u", "cw_1", Value::Null),
    ];
    assert_eq!(
        left(coalesced(notes)),
        [
            (3, Some(json!("run_c"))),
            (4, Some(json!("run_a"))),
            (5, Some(Value::Null))
        ]
    );
}

/// Nothing but `thread.changed` is merged, and what is between keeps its place.
#[test]
fn nothing_else_is_merged_and_order_is_kept() {
    let notes = vec![
        thread(1, "t", "cw_1", json!("run_a")),
        run_started(2, "run_a"),
        thread(3, "t", "cw_1", json!("run_a")),
        run_started(4, "run_b"),
    ];
    assert_eq!(
        left(coalesced(notes)),
        [(2, None), (3, Some(json!("run_a"))), (4, None)]
    );
}
