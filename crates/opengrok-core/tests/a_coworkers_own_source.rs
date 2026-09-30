//! A coworker's own source: the door its turns go through when a turn names none, on its
//! aggregate, its log and its row.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_core::coworker::{
    Coworker, CoworkerCommand, CoworkerError, CoworkerEvent, CoworkerView,
};
use opengrok_core::id::CoworkerId;
use opengrok_core::inference::SourceKind;

fn hired() -> Coworker {
    Coworker::replay(
        &Coworker::default()
            .decide(CoworkerCommand::Hire {
                name: "Ada".to_string(),
                model: "gpt-6-luna".to_string(),
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

/// None until its owner sets one, and none again when they clear it: the driving person's own
/// setting. Its own decision, which neither renames nor repins it.
#[test]
fn a_coworkers_own_source_is_set_and_cleared_and_nothing_else_moves() {
    let mut coworker = hired();
    assert_eq!(
        coworker.source, None,
        "the person's own setting until it is set"
    );
    for source in [
        Some(SourceKind::LocalProxy),
        Some(SourceKind::Gateway),
        None,
    ] {
        decided(
            &mut coworker,
            CoworkerCommand::SetSource { source, at_ms: 2 },
        )
        .unwrap();
        assert_eq!(coworker.source, source);
    }
    assert_eq!(coworker.model, "gpt-6-luna", "not a repin");
    assert_eq!(coworker.name, "Ada", "not a rename");

    decided(&mut coworker, CoworkerCommand::Retire { at_ms: 3 }).unwrap();
    let source = Some(SourceKind::LocalProxy);
    assert_eq!(
        decided(
            &mut coworker,
            CoworkerCommand::SetSource { source, at_ms: 4 }
        ),
        Err(CoworkerError::Retired),
        "a retired coworker takes no more decisions"
    );
}

/// The log says it in the wire's words and replays it; a row from before it reads as none.
#[test]
fn a_coworkers_own_source_survives_its_log_and_its_row() {
    let event = CoworkerEvent::SourceSet {
        source: Some(SourceKind::LocalProxy),
        at_ms: 2,
    };
    assert_eq!(event.event_type(), "coworker-source-set");
    let logged = serde_json::to_value(&event).unwrap();
    assert_eq!(
        logged,
        serde_json::json!({"type": "source-set", "source": "local_proxy", "at_ms": 2})
    );
    let read: CoworkerEvent = serde_json::from_value(logged).unwrap();
    let mut coworker = hired();
    coworker.apply(&read);
    assert_eq!(coworker.source, Some(SourceKind::LocalProxy));
    let row = CoworkerView::of(CoworkerId::from_stored("cw_1"), &coworker, 2);
    assert_eq!(
        row.source,
        Some(SourceKind::LocalProxy),
        "the one mapping carries it"
    );

    let mut before = serde_json::to_value(&row).unwrap();
    before.as_object_mut().unwrap().remove("source");
    let before: CoworkerView = serde_json::from_value(before).unwrap();
    assert_eq!(before.source, None);
}
