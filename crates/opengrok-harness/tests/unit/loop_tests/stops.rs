//! Stop and card paths: a stopped run asks the model nothing further; a refused card never runs.

use super::*;

/// A STOP IS THE ONLY WAY OUT OF A BOT IN A LOOP, so the first thing it has to buy is that the
/// model is not asked again. The run ends as a stop — `run-stopped` and then `RUN_FINISHED`,
/// never `RUN_ERROR` — because a person changing their mind is not a coworker failing.
#[tokio::test]
async fn a_stopped_run_asks_the_model_nothing_further_and_ends_as_a_stop() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    let journal = StoppingJournal::saying_stop_after(0);
    let runner = tool_runner();

    let events =
        run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "a run stopped before its first round must not spend a model call"
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|name| name.as_str()) == Some("run-stopped")),
        "the reason travels as its own frame, or a client cannot tell a stop from a finish: \
         {events:?}"
    );
    assert_eq!(
        events.last().unwrap().event_type,
        EventType::RunFinished,
        "the stream still closes, or the client holds its spinner open forever"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::RunError),
        "a stop is not a failure: {events:?}"
    );
}

/// THE PLACE THAT ACTUALLY STOPS THE YOUTUBE SEARCH HAPPENING ONE MORE TIME. The model has
/// answered and asked to play the recipe again; the check sits between that ask and the doing,
/// so the tool never runs. And the frames of the round it was in the middle of are journaled
/// WITH the ending, so the transcript shows what the coworker was about to do rather than
/// ending a step short of it.
#[tokio::test]
async fn a_stop_lands_between_the_model_asking_for_a_tool_and_the_tool_running() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    // Not stopped when the round opens; stopped by the time the tool is about to run.
    let journal = StoppingJournal::saying_stop_after(1);
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);

    let events =
        run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "the round that was already open still gets its answer; nothing after it is asked"
    );
    assert_eq!(
        computer.last_box(),
        None,
        "the tool the model asked for must not run: that is the repetition the person pressed \
         stop to end"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult),
        "and no result is invented for a call that never happened: {events:?}"
    );

    // WHAT WAS SPENT IS ACCOUNTED FOR, AND THIS IS WHERE THAT IS VISIBLE. The model call that
    // was already open is read to its last delta rather than dropped — `TOOL_CALL_END` is the
    // final thing the door yields, so seeing it means the stream was drained. The gateway
    // records a call's usage when the call completes; abandoning a half-read stream would
    // leave tokens spent at the provider and missing from the meter, which is exactly the
    // spend landing on the floor.
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallEnd),
        "the model call already in flight is drained, not abandoned: {events:?}"
    );

    // The round's own frames and the ending in one write: the transcript keeps the
    // call it asked for, then the stop. Intent preamble ("playing it again") is
    // withheld because a work tool followed — NativeChat would paint it as chat.
    let last = journal.batches().pop().expect("a final journal batch");
    assert!(
        last.iter()
            .any(|event| event.event_type == EventType::ToolCallEnd),
        "the call of the round in progress goes down with the stop: {last:?}"
    );
    assert!(
        !last
            .iter()
            .any(|event| event.event_type == EventType::TextMessageContent),
        "intent preamble is withheld, not journaled as chat: {last:?}"
    );
    assert!(
        last.iter()
            .any(|event| event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|name| name.as_str()) == Some("run-stopped")),
        "{last:?}"
    );
}

/// THE CLOSE IS A STEP BOUNDARY TOO (`formal/tla/HarnessLoop.tla` StopIsHonoured). TLC's
/// trace: the round opens unstopped, the person presses Stop while the model is answering in
/// words, the answer ends the run. It used to end as RUN_FINISHED — the person was told the
/// coworker finished when they had stopped it.
#[tokio::test]
async fn a_stop_pressed_during_the_final_answer_ends_the_run_as_a_stop() {
    // Not stopped when the round opens; stopped by the time the answer closes it.
    let journal = StoppingJournal::saying_stop_after(1);

    let events = run_conversation(
        &MockDoor::echoing(),
        None,
        &journal,
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert!(
        events.iter().any(is_run_stopped),
        "a stop recorded before the run ended is how it ends: {events:?}"
    );
    assert!(
        assistant_text(&events).contains("hello"),
        "what the model already said is kept, not withdrawn: {events:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let endings = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                EventType::RunFinished | EventType::RunError
            )
        })
        .count();
    assert_eq!(endings, 1, "still exactly one ending: {events:?}");
}

/// A STOP PRESSED WHILE THE CARD WAS UP WINS OVER THE ANSWER (`formal/tla/RunLifecycle.tla`
/// NoApprovedAfterStop). The answer is appended, then the Stop, then the continuation starts:
/// the approved call must not run, and the model must not be asked anything.
#[tokio::test]
async fn a_run_stopped_after_its_card_was_answered_does_not_run_the_approved_call() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    let journal = StoppingJournal::saying_stop_after(0);
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "play the recipe"}),
    };

    let events = resume_conversation(
        &door,
        &runner,
        &journal,
        request("go"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1),
    )
    .await;

    assert_eq!(computer.last_box(), None, "the approved call did not run");
    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "and the model was not asked again"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult),
        "no result is invented for a call that never ran: {events:?}"
    );
    assert!(events.iter().any(is_run_stopped), "{events:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(
        journal.batches().concat().iter().any(is_run_stopped),
        "the stop is journaled, not only shown"
    );
}

/// A REFUSAL NEVER REACHES THE EXECUTOR: the
/// person said no, so the model reads the refusal and the box is not touched.
#[tokio::test]
async fn a_refused_card_is_read_by_the_model_and_never_runs() {
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "rm -rf build"}),
    };

    let events = resume_conversation(
        &MockDoor::echoing(),
        &runner,
        &MemoryJournal::new(),
        request("clean up"),
        RunContext::new("t1", "r1", 1),
        Resumption::refused(call, 1, "the person said no"),
    )
    .await;

    assert_eq!(computer.last_box(), None, "a refused call does not run");
    let result = events
        .iter()
        .find(|event| event.event_type == EventType::ToolCallResult)
        .expect("the refusal is written where the result would have gone");
    assert_eq!(result.extra.get("ok"), Some(&serde_json::json!(false)));
    // The person decided, and the wait for them is not the call's (#305).
    assert_eq!(duration_of(result), Some(0), "{result:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}
