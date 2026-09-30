//! How hard a coworker thinks (#271), on its aggregate and on the run that captures it.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerError, Effort};
use opengrok_core::run::{Run, RunCommand, RunEvent};

fn hired() -> Coworker {
    Coworker::replay(
        &Coworker::default()
            .decide(CoworkerCommand::Hire {
                name: "Ada".to_string(),
                model: "xai/grok-4.6".to_string(),
                at_ms: 1,
            })
            .unwrap(),
    )
}

fn decided(coworker: &mut Coworker, command: CoworkerCommand) -> Result<(), CoworkerError> {
    for event in coworker.decide(command)? {
        coworker.apply(&event);
    }
    Ok(())
}

/// How hard a coworker thinks is its own decision, and going back to inherit is one too.
#[test]
fn setting_the_effort_changes_how_hard_it_thinks_and_nothing_else() {
    let mut coworker = hired();
    assert_eq!(coworker.effort, Effort::Inherit, "inherit until it is set");
    for effort in [Effort::High, Effort::Inherit, Effort::Off] {
        decided(
            &mut coworker,
            CoworkerCommand::SetEffort { effort, at_ms: 2 },
        )
        .unwrap();
        assert_eq!(coworker.effort, effort);
    }
    assert_eq!(coworker.model, "xai/grok-4.6", "not a repin");
    assert_eq!(coworker.name, "Ada", "not a rename");

    decided(&mut coworker, CoworkerCommand::Retire { at_ms: 3 }).unwrap();
    assert_eq!(
        decided(
            &mut coworker,
            CoworkerCommand::SetEffort {
                effort: Effort::Max,
                at_ms: 4,
            }
        ),
        Err(CoworkerError::Retired),
        "a retired coworker takes no more decisions"
    );
}

/// The words are the gateway's `reasoning_effort` plus inherit, spelled the same by `as_str`,
/// `parse` and the log; anything else is refused rather than stored as a guess.
#[test]
fn the_effort_words_are_the_gateways_plus_inherit() {
    let words: Vec<&str> = Effort::ALL.iter().map(|effort| effort.as_str()).collect();
    assert_eq!(
        words,
        ["inherit", "none", "low", "medium", "high", "xhigh", "max"]
    );
    for effort in Effort::ALL {
        assert_eq!(Effort::parse(effort.as_str()), Some(effort));
        assert_eq!(
            serde_json::to_value(effort).unwrap(),
            serde_json::json!(effort.as_str()),
            "the log spells it as the wire does"
        );
    }
    for word in ["loud", "minimal", "High", " high", ""] {
        assert_eq!(Effort::parse(word), None, "{word:?}");
    }
    assert_eq!(
        Effort::Inherit.reasoning_effort(),
        None,
        "inherit sends none"
    );
    assert_eq!(
        Effort::Off.reasoning_effort(),
        Some("none"),
        "off is a word"
    );
    assert_eq!(Effort::XHigh.reasoning_effort(), Some("xhigh"));
}

/// How hard a turn thinks goes into the log with its start and comes back out of it, so a
/// resume thinks as hard as the turn it continues, whatever the coworker says by then.
#[test]
fn a_started_run_keeps_the_effort_it_started_with() {
    let started = Run::default()
        .decide(RunCommand::Start {
            thread_id: "t1".to_string(),
            coworker_id: None,
            model: Some("openai/gpt-5.5".to_string()),
            effort: Effort::High,
            inference_source: Default::default(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms: 1,
        })
        .unwrap();
    let stored: Vec<RunEvent> = started
        .iter()
        .map(|event| serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap())
        .collect();
    assert_eq!(Run::replay(&stored).effort, Effort::High);
}

/// A start written before effort existed reads as inherit, which is what that turn sent: no
/// `reasoning_effort` at all. A resume of it goes on sending none.
#[test]
fn a_start_written_before_effort_reads_as_inherit() {
    let event: RunEvent = serde_json::from_str(
        r#"{"type":"started","thread_id":"t1","coworker_id":null,"model":"xai/grok-4.6","at_ms":1}"#,
    )
    .unwrap();
    let run = Run::replay([&event]);
    assert!(run.started);
    assert_eq!(run.effort, Effort::Inherit);
    assert_eq!(run.effort.reasoning_effort(), None);
}
