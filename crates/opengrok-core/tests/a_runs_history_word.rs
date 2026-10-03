//! The word a routine's run history gives a run's status (`RunStatus::history_word`), which the
//! `run.finished` note says too: one vocabulary for the app, not the wire's `as_str`.

use opengrok_core::run::RunStatus;

#[test]
fn the_history_says_ok_and_error_where_the_wire_says_finished_failed_and_stopped() {
    let words: Vec<(&str, &str)> = [
        RunStatus::Running,
        RunStatus::AwaitingApproval,
        RunStatus::Finished,
        RunStatus::Failed,
        RunStatus::Stopped,
    ]
    .iter()
    .map(|status| (status.as_str(), status.history_word()))
    .collect();
    assert_eq!(
        words,
        [
            ("running", "running"),
            ("awaiting-approval", "waiting"),
            ("finished", "ok"),
            ("failed", "error"),
            ("stopped", "error"),
        ]
    );
}

/// A run that has ended says `ok` or `error` and one that has not never does: a note that says
/// a run finished with `waiting` or `running` would be a run the app thinks is over.
#[test]
fn only_a_run_that_has_ended_is_ok_or_error() {
    for status in [
        RunStatus::Running,
        RunStatus::AwaitingApproval,
        RunStatus::Finished,
        RunStatus::Failed,
        RunStatus::Stopped,
    ] {
        let ended = matches!(status.history_word(), "ok" | "error");
        assert_eq!(ended, status.is_terminal(), "{status:?}");
    }
}
