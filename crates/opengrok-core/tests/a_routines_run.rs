//! A routine's run, as its log tells it from a turn: by the one question only a routine writes.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_core::id::RunId;
use opengrok_core::run::{Run, RunCommand, routine_prompt};
use serde_json::{Value, json};

fn started(prompt: Option<Vec<Value>>) -> Run {
    let mut run = Run::default();
    let start = RunCommand::Start {
        thread_id: "sched_weekly".to_string(),
        coworker_id: None,
        model: None,
        effort: Default::default(),
        inference_source: Default::default(),
        system: None,
        skill_id: None,
        offered_skills: Vec::new(),
        prompt,
        limits: Default::default(),
        at_ms: 1,
    };
    for event in run.decide(start).unwrap() {
        run.apply(&event);
    }
    run
}

/// A ROUTINE'S RUN IS TOLD BY ITS QUESTION, journaled under the run's own id, and never by its
/// thread, which a client may name as it likes: another run's routine question, a person's
/// message, or a log from before questions were stored is a turn (#304).
#[test]
fn a_run_is_a_routines_only_by_the_question_journaled_under_its_own_id() {
    let id = RunId::new();
    let fired = started(Some(routine_prompt(&id, "write the weekly report")));
    assert!(fired.fired_by_routine(&id));
    let elsewhere = started(Some(routine_prompt(
        &RunId::new(),
        "write the weekly report",
    )));
    assert!(!elsewhere.fired_by_routine(&id), "another run's question");
    let person = started(Some(vec![
        json!({"id": "m1", "role": "user", "content": "hi"}),
    ]));
    assert!(!person.fired_by_routine(&id), "a person's message");
    assert!(
        !started(None).fired_by_routine(&id),
        "a log with no question"
    );
}
