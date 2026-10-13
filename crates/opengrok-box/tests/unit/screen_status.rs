use super::*;

fn facts(state: &str) -> Facts<'_> {
    Facts {
        name: "Ada",
        busy: None,
        update: None,
        state,
        screen: ScreenProgress::Given,
        has_page: true,
        error: None,
    }
}

/// Each thing that blanks a screen has its own word and a sentence naming the Bot, and the work in
/// progress wins over what the computer's state would say (a reset passes through `absent`).
#[test]
fn each_reason_for_a_blank_screen_says_itself() {
    let said = |f: Facts| status(&f);
    assert_eq!(said(facts("running")), ("ready", String::new()));
    let resetting = Facts {
        busy: Some(Busy::Resetting),
        ..facts("absent")
    };
    assert_eq!(
        said(resetting),
        ("resetting", "Resetting Ada's computer…".into())
    );
    let restarting = Facts {
        busy: Some(Busy::Restarting),
        ..facts("stopped")
    };
    assert_eq!(said(restarting).0, "restarting");
    let opening = Facts {
        busy: Some(Busy::Starting),
        ..facts("absent")
    };
    assert_eq!(
        said(opening),
        ("starting-computer", "Opening Ada's computer…".into())
    );
    assert_eq!(said(facts("starting")).0, "starting-computer");
    let pulling = Facts {
        update: Some(("pulling", None)),
        ..facts("running")
    };
    assert_eq!(
        said(pulling),
        (
            "updating",
            "Updating Ada's computer: getting the newest image…".into()
        )
    );
    let moving = Facts {
        update: Some(("transferring", None)),
        ..facts("stopped")
    };
    assert!(said(moving).1.contains("moving its files"));
    let failed = Facts {
        update: Some(("failed", Some("no disk left"))),
        ..facts("stopped")
    };
    assert_eq!(
        said(failed),
        (
            "failed",
            "Ada's computer could not be updated: no disk left".into()
        )
    );
    let failed_but_running = Facts {
        update: Some(("failed", Some("x"))),
        ..facts("running")
    };
    assert_eq!(
        said(failed_but_running).0,
        "ready",
        "the old computer still shows its screen"
    );
    assert_eq!(said(facts("stopped")).0, "stopped");
    assert_eq!(said(facts("absent")).0, "absent");
    let refused = Facts {
        error: Some("quota reached"),
        ..facts("absent")
    };
    assert_eq!(
        said(refused),
        ("failed", "Ada has no computer: quota reached".into())
    );
    assert_eq!(said(facts("exited")).0, "failed");
}

/// A running computer is not ready until the Bot's own screen is up and there is a page to load.
#[test]
fn a_running_computer_waits_for_its_screen() {
    for screen in [ScreenProgress::NotAsked, ScreenProgress::Starting] {
        let f = Facts {
            screen,
            ..facts("running")
        };
        assert_eq!(
            status(&f),
            ("starting-screen", "Starting Ada's screen…".into())
        );
    }
    let no_page = Facts {
        has_page: false,
        ..facts("running")
    };
    assert_eq!(status(&no_page).0, "starting-screen");
    for screen in [ScreenProgress::OldImage, ScreenProgress::Full] {
        assert_eq!(
            status(&Facts {
                screen,
                ..facts("running")
            })
            .0,
            "ready"
        );
    }
}

/// The pane says which screen the Bot is really on: a computer too old for own screens is
/// `own-pending` with a sentence saying an update fixes it, never `own` (the label that lied).
#[test]
fn the_screen_word_tells_the_truth_on_an_old_or_full_computer() {
    assert_eq!(screen_word(true, ScreenProgress::Given), "own");
    assert_eq!(screen_word(true, ScreenProgress::Starting), "own");
    assert_eq!(screen_word(true, ScreenProgress::OldImage), "own-pending");
    assert_eq!(screen_word(true, ScreenProgress::Full), "shared");
    assert_eq!(screen_word(false, ScreenProgress::OldImage), "shared");
    let old = screen_note("Ada", ScreenProgress::OldImage).unwrap();
    assert!(old.contains("after a computer update"), "{old}");
    assert!(
        screen_note("Ada", ScreenProgress::Full)
            .unwrap()
            .contains("room for")
    );
    assert_eq!(screen_note("Ada", ScreenProgress::Given), None);
}

/// A mark lasts as long as the work holding it, keyed by scope so a reset's new box keeps it.
#[test]
fn a_busy_mark_lasts_as_long_as_its_work() {
    let key = scope_key("account", "acc_busy_test");
    assert_eq!(busy(&key), None);
    {
        let _mark = mark_busy(key.clone(), Busy::Resetting);
        assert_eq!(busy(&key), Some(Busy::Resetting));
        assert_eq!(busy(&scope_key("account", "acc_other")), None);
    }
    assert_eq!(busy(&key), None, "dropping the mark clears it");
}

/// An update record covers the screen while it moves, and a failed one always; one that stopped
/// moving 15 minutes ago is a crashed update and says nothing.
#[test]
fn only_a_live_or_failed_update_record_is_read() {
    let now = 100 * 60 * 1000;
    let record = |phase: &str, at: i64| serde_json::json!({ "phase": phase, "updatedAtMs": at, "error": null });
    assert_eq!(
        update_of(&record("pulling", now - 1000), now),
        Some(("pulling", None))
    );
    assert_eq!(
        update_of(&record("pulling", now - 16 * 60 * 1000), now),
        None
    );
    assert_eq!(update_of(&record("failed", 0), now), Some(("failed", None)));
    assert_eq!(update_of(&serde_json::Value::Null, now), None);
}

/// The body the pane reads: the status pair always, `screenNote` only when the Bot is not on the
/// screen it would have.
#[test]
fn the_body_carries_the_status_and_says_why_a_bot_is_on_the_shared_screen() {
    let mut body = serde_json::json!({ "state": "running" });
    stamp_status(&mut body, &facts("starting"));
    assert_eq!(body["screenStatus"], "starting-computer");
    assert_eq!(body["screenStatusText"], "Opening Ada's computer…");
    stamp_screen(&mut body, "Ada", true, ScreenProgress::OldImage);
    assert_eq!(body["screen"], "own-pending");
    assert!(
        body["screenNote"]
            .as_str()
            .unwrap()
            .contains("computer update")
    );
    let mut shared = serde_json::json!({});
    stamp_screen(&mut shared, "Ada", false, ScreenProgress::Given);
    assert_eq!(shared["screen"], "shared");
    assert!(shared.get("screenNote").is_none());
}
