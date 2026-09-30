//! Why a finished run finished (#244), on the aggregate and in the log it is replayed from.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_core::run::{FinishReason, Run, RunCommand, RunEvent, RunStatus};
use serde_json::json;

fn started() -> Run {
    let mut run = Run::default();
    for event in run
        .decide(RunCommand::Start {
            thread_id: "t".into(),
            coworker_id: None,
            model: None,
            effort: Default::default(),
            inference_source: Default::default(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms: 0,
        })
        .unwrap()
    {
        run.apply(&event);
    }
    run
}

#[test]
fn a_run_finished_at_its_limit_keeps_the_reason() {
    let mut run = started();
    let events = run
        .decide(RunCommand::Finish {
            at_ms: 1,
            reason: Some(FinishReason::Budget),
        })
        .unwrap();
    for event in &events {
        run.apply(event);
    }
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.finish_reason, Some(FinishReason::Budget));
    assert_eq!(
        serde_json::to_value(&events[0]).unwrap(),
        json!({ "type": "finished", "at_ms": 1, "reason": "budget" })
    );
}

/// THE LOG IS APPEND-ONLY, so every `Finished` written before #244 must still read: without a
/// reason, and as a run that was simply done. And a finish without one writes none, so the log
/// of an ordinary run is byte-for-byte what it was.
#[test]
fn a_finish_from_before_reasons_reads_as_simply_done() {
    let old: RunEvent = serde_json::from_value(json!({ "type": "finished", "at_ms": 7 })).unwrap();
    assert_eq!(
        old,
        RunEvent::Finished {
            at_ms: 7,
            reason: None
        }
    );
    let mut run = started();
    run.apply(&old);
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.finish_reason, None);
    assert_eq!(
        serde_json::to_value(&old).unwrap(),
        json!({ "type": "finished", "at_ms": 7 })
    );
}

#[test]
fn only_a_word_this_build_knows_is_a_reason() {
    assert_eq!(FinishReason::parse("budget"), Some(FinishReason::Budget));
    assert_eq!(FinishReason::parse("Budget"), None);
    assert_eq!(FinishReason::parse("tokens"), None);
}
