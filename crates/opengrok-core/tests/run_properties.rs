//! The run aggregate against arbitrary command sequences: the invariants formal/tla and
//! formal/lean prove for the model, checked on the code the model stands for.
//!
//! `decide` then `apply`, as the store does, for every command that is accepted; a refused one
//! changes nothing. After every step:
//! - at most one ending (Finished, Failed, Stopped) was ever accepted — TLA `ExactlyOneEnding`,
//!   Lean `Ending.at_most_one_terminal`;
//! - an ended run stays ended, with the same status — Lean `ended_is_stable`;
//! - no call is answered twice — TLA `ApprovedAtMostOnce`, Lean `Answer.at_most_one_commit`;
//! - frames are numbered 0, 1, 2 … and none follows a Finished or a Failed;
//! - replaying the accepted events from nothing gives the same state (the log is the truth).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_core::run::{Run, RunCommand, RunEvent, RunStatus, SuspendReason};
use proptest::prelude::*;
use serde_json::json;

fn command() -> impl Strategy<Value = RunCommand> {
    // Few call ids, so answers collide with suspensions and with each other.
    let call = prop_oneof![Just("c1"), Just("c2"), Just("c3")].prop_map(str::to_string);
    prop_oneof![
        Just(RunCommand::Start {
            thread_id: "t".into(),
            coworker_id: None,
            model: None,
            system: None,
            skill_id: None,
            prompt: None,
            at_ms: 0,
        }),
        Just(RunCommand::Emit {
            payload: json!({"type": "TEXT"}),
            at_ms: 0,
        }),
        Just(RunCommand::Finish { at_ms: 0 }),
        Just(RunCommand::Fail {
            reason: "r".into(),
            at_ms: 0,
        }),
        Just(RunCommand::Stop {
            by: "person".into(),
            at_ms: 0,
        }),
        call.clone().prop_map(|call_id| RunCommand::Suspend {
            call_id,
            tool: "computer".into(),
            arguments: json!({}),
            reason: SuspendReason::PolicyApproval,
            at_ms: 0,
        }),
        (call, any::<bool>()).prop_map(|(call_id, approved)| RunCommand::Answer {
            call_id,
            approved,
            by: "person".into(),
            at_ms: 0,
        }),
    ]
}

fn is_ending(event: &RunEvent) -> bool {
    matches!(
        event,
        RunEvent::Finished { .. } | RunEvent::Failed { .. } | RunEvent::Stopped { .. }
    )
}

proptest! {
    // 2,000 cases in the gate. PROPTEST_CASES overrides it (nightly.yml runs 100,000 natively
    // and 16 under Miri): `with_cases` alone would ignore the variable.
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|n| n.parse().ok()).unwrap_or(2000)
    ))]

    #[test]
    fn the_run_aggregate_keeps_the_models_invariants(commands in prop::collection::vec(command(), 0..40)) {
        let mut run = Run::default();
        let mut log: Vec<RunEvent> = Vec::new();
        let mut ended: Option<RunStatus> = None;
        let mut self_ended = false;
        let mut answered: Vec<String> = Vec::new();

        for command in commands {
            let Ok(events) = run.decide(command) else { continue };
            for event in &events {
                if is_ending(event) {
                    prop_assert!(ended.is_none(), "a second ending {event:?} after {ended:?}");
                }
                if let RunEvent::Answered { call_id, .. } = event {
                    prop_assert!(!answered.contains(call_id), "{call_id} answered twice");
                    answered.push(call_id.clone());
                }
                if let RunEvent::Emitted { seq, .. } = event {
                    prop_assert!(!self_ended, "a frame after the run finished or failed");
                    prop_assert_eq!(*seq, run.next_seq());
                }
                run.apply(event);
                log.push(event.clone());
                if is_ending(event) {
                    ended = Some(run.status);
                    self_ended = matches!(event, RunEvent::Finished { .. } | RunEvent::Failed { .. });
                }
            }
            if let Some(status) = ended {
                prop_assert_eq!(run.status, status, "an ended run changed its status");
            }
            prop_assert_eq!(&Run::replay(&log), &run, "replaying the log disagrees");
        }
    }
}
