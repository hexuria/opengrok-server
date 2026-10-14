//! Limits and timing: history trimming, per-call estimates, tool-start journaling.

use super::*;

/// Answers every call with a word and keeps what it was asked.
#[derive(Default)]
struct RecordingDoor(Mutex<Vec<ModelRequest>>);

#[async_trait::async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.0.lock().unwrap().push(request);
        let script = vec![ModelDelta::Text("Done.".to_string())];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// A shell whose every answer is `bytes` long: a log dump, a large file read.
fn loud_shell(bytes: usize, pause_ms: u64) -> ToolRunner {
    ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        Arc::new(move |call| {
            std::thread::sleep(std::time::Duration::from_millis(pause_ms));
            opengrok_tools::ToolResult::ok(&call.id, "o".repeat(bytes))
        }),
    )
}

fn run_error(events: &[Event]) -> String {
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError, "{events:?}");
    last.extra["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// A LONG THREAD USED TO REACH THE PROVIDER WHOLE (#90), and came back a 400 after a round trip.
/// Now the oldest turns are left out before the call, this turn is kept, and the model is told.
#[tokio::test]
async fn an_over_limit_history_is_trimmed_before_the_call_and_keeps_this_turn() {
    let mut asked = request("now");
    let mut messages = Vec::new();
    for turn in 0..10 {
        messages.push(ChatMessage::text(
            "user",
            format!("question {turn} {}", "q".repeat(3_000)),
        ));
        messages.push(ChatMessage::text(
            "assistant",
            format!("answer {turn} {}", "a".repeat(3_000)),
        ));
    }
    messages.append(&mut asked.messages);
    asked.messages = messages;
    asked.context_tokens = Some(20_000);
    let door = RecordingDoor::default();

    let events = run_conversation(&door, None, &MemoryJournal::new(), asked, "t1", "r1", 1).await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let calls = door.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let sent = &calls[0];
    assert!(sent.messages.len() < 21, "{}", sent.messages.len());
    assert_eq!(sent.messages.last().unwrap().content, "now");
    assert!(sent.messages[0].content.starts_with("question "));
    assert!(sent.system.as_deref().unwrap().contains("oldest messages"));
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["context"]["limit"], 20_000, "{timing}");
    assert_eq!(timing["context"]["estimated"].as_array().unwrap().len(), 1);
    assert!(
        timing["context"]["left_out"].as_u64().unwrap() > 0,
        "{timing}"
    );
}

/// Every call's estimate is on the run's record, known limit or not.
#[tokio::test]
async fn run_timing_carries_each_calls_estimate() {
    let door = RecordingDoor::default();
    let events = run_conversation(
        &door,
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    let timing = run_timing_value(&events).expect("run-timing");
    assert!(timing["context"]["limit"].is_null(), "{timing}");
    assert!(
        timing["context"]["estimated"][0].as_u64().unwrap() > 0,
        "{timing}"
    );
}

/// This turn alone does not fit: nothing is sent, and the person is told why in a sentence
/// instead of reading the provider's 400.
#[tokio::test]
async fn a_conversation_that_cannot_fit_ends_with_a_sentence_and_asks_no_model() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    let mut asked = request(&"x".repeat(60_000));
    asked.context_tokens = Some(8_000);
    let events = run_conversation(&door, None, &MemoryJournal::new(), asked, "t1", "r1", 1).await;
    assert_eq!(*calls.lock().unwrap(), 0);
    let message = run_error(&events);
    assert!(message.contains("too long for mock"), "{message}");
}

/// A run's own tool results are never left out, so a run whose results outgrow the model ends
/// before the call that would have been refused — not after it.
#[tokio::test]
async fn a_round_that_outgrows_the_limit_ends_before_its_call() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    let runner = loud_shell(60_000, 0);
    let mut asked = request("dump the log");
    asked.context_tokens = Some(16_000);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        asked,
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "the call that would not fit is never made"
    );
    assert!(run_error(&events).contains("too long"));
}

/// The wall clock's wrap-up is measured too: it carries the round the fit has not seen yet.
/// One that cannot fit is not sent, and the run ends on the limit it reached.
#[tokio::test]
async fn a_wrap_up_that_cannot_fit_ends_on_its_limit_uncalled() {
    let calls = Arc::new(Mutex::new(0usize));
    let door = CountingToolDoor(calls.clone());
    let runner = loud_shell(60_000, 30);
    let mut asked = request("dump the log");
    asked.context_tokens = Some(16_000);
    let events = run_conversation_within(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        asked,
        RunContext::new("t1", "r1", 1),
        RunBudget {
            max_wall_ms: 10,
            ..RunBudget::default()
        },
        None,
    )
    .await;
    assert_eq!(*calls.lock().unwrap(), 1, "no wrap-up call was sent");
    assert!(run_error(&events).contains("time limit"));
}

/// A journal that notes each tool start and whether the computer had already been touched when
/// it was asked, and can be told to refuse the write.
struct StartWatchingJournal {
    computer: Arc<crate::tools::tests_support::RecordingComputer>,
    refuse: bool,
    starts: Mutex<Vec<(Vec<String>, bool)>>,
}

impl StartWatchingJournal {
    fn new(computer: Arc<crate::tools::tests_support::RecordingComputer>, refuse: bool) -> Self {
        Self {
            computer,
            refuse,
            starts: Mutex::new(Vec::new()),
        }
    }

    fn starts(&self) -> Vec<(Vec<String>, bool)> {
        self.starts.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl RunJournal for StartWatchingJournal {
    async fn record(&self, _run_id: &str, _events: &[Event]) -> Result<(), JournalError> {
        Ok(())
    }

    async fn tools_starting(
        &self,
        _run_id: &str,
        tools: &[opengrok_core::run::StartedTool],
    ) -> Result<(), JournalError> {
        let already_ran = self.computer.last_box().is_some();
        if let Ok(mut starts) = self.starts.lock() {
            starts.push((
                tools
                    .iter()
                    .map(|started| format!("{}:{}", started.call_id, started.tool))
                    .collect(),
                already_ran,
            ));
        }
        if self.refuse {
            return Err(JournalError::Unwritable("the database is down".to_string()));
        }
        Ok(())
    }
}

/// #91: A TOOL'S START IS ON RECORD BEFORE IT RUNS. Without it a log interrupted mid-tool is
/// indistinguishable from one interrupted between rounds, and only the second is safe to resume.
#[tokio::test]
async fn a_tools_start_is_journaled_before_it_runs() {
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let journal = StartWatchingJournal::new(computer.clone(), false);
    let door = CountingToolDoor(Arc::new(Mutex::new(0)));

    run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    let starts = journal.starts();
    assert!(!starts.is_empty(), "the start was journaled");
    assert_eq!(
        starts[0].0,
        vec!["c1:shell".to_string()],
        "naming the call and its tool"
    );
    assert!(!starts[0].1, "and before the computer was touched");
    assert!(computer.last_box().is_some(), "the tool then ran");
}

/// A start that cannot be written is a tool that does not run. Running it anyway would put an
/// action in the world the log cannot account for — the one case a resume must never meet blind.
#[tokio::test]
async fn a_tool_whose_start_cannot_be_journaled_never_runs() {
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let journal = StartWatchingJournal::new(computer.clone(), true);
    let door = CountingToolDoor(Arc::new(Mutex::new(0)));

    let events =
        run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

    assert_eq!(computer.last_box(), None, "the tool did not run");
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult),
        "no result is invented for a call that never ran: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::RunError),
        "the run ends as an error that says why: {events:?}"
    );
}

/// The approved call takes the same road: its start is journaled first, and a start that
/// cannot be written keeps it from running.
#[tokio::test]
async fn an_approved_call_whose_start_cannot_be_journaled_never_runs() {
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let journal = StartWatchingJournal::new(computer.clone(), true);
    let door = CountingToolDoor(Arc::new(Mutex::new(0)));
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "play the recipe"}),
    };

    resume_conversation(
        &door,
        &runner,
        &journal,
        request("go"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1),
    )
    .await;

    assert_eq!(
        journal.starts().first().map(|s| s.0.clone()),
        Some(vec!["c1:shell".to_string()])
    );
    assert_eq!(computer.last_box(), None, "the approved call did not run");
}

/// A shell answered in-process: `sleep <ms>` takes that long, and anything else answers at once.
fn paced_shell() -> ToolRunner {
    ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        Arc::new(|call| {
            let command = call.arguments["command"].as_str().unwrap_or_default();
            if let Some(ms) = command
                .strip_prefix("sleep ")
                .and_then(|ms| ms.parse().ok())
            {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            opengrok_tools::ToolResult::ok(&call.id, "[exit code 0]")
        }),
    )
}

/// EACH CALL'S OWN TIME, NOT ITS ROUND'S (#305). A round's calls run one after another and their
/// results go out together, so the quick call's result is sent after the slow call's wait: the
/// time on it must still be its own. The journal keeps the very number the stream said, which is
/// what a replay reads back, and `run-timing` counts the same numbers.
#[tokio::test]
async fn each_call_in_a_round_says_its_own_time_and_the_journal_keeps_it() {
    const SLOW_MS: u64 = 150;
    let door = Rounds::new(
        vec![
            [
                shell_deltas("c1", &format!("sleep {SLOW_MS}")),
                shell_deltas("c2", "true"),
            ]
            .concat(),
        ],
        "Both ran.",
    );
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &door,
        Some(&paced_shell()),
        &journal,
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let slow = duration_of(results_for(&events, "c1")[0]).expect("the slow call's time");
    let quick = duration_of(results_for(&events, "c2")[0]).expect("the quick call's time");
    assert!(slow >= SLOW_MS, "{slow}");
    assert!(quick < SLOW_MS, "the quick call's time is its own: {quick}");

    let journaled = journal.batches().concat();
    for (id, said) in [("c1", slow), ("c2", quick)] {
        let kept: Vec<Option<u64>> = results_for(&journaled, id)
            .into_iter()
            .map(duration_of)
            .collect();
        assert_eq!(kept, [Some(said)], "the journal keeps what the stream said");
    }
    let timing = run_timing_value(&events).expect("run-timing");
    let counted: Vec<u64> = timing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["ms"].as_u64().unwrap())
        .collect();
    assert_eq!(counted, [slow, quick], "{timing}");
}

/// A RESUMED APPROVED CALL IS TIMED TOO, FROM WHEN IT RUNS (#305); it used to run untimed. The
/// person answers the card later than the call takes to run, and the time on its result is the
/// call's own, the wait on the person left out. The card's own result says how long the gate
/// took to decide to ask, before anything ran.
#[tokio::test]
async fn an_approved_call_says_its_own_time_not_the_wait_on_its_card() {
    use opengrok_policy::ToolSet;
    const RUNS_MS: u64 = 120;
    const ANSWERED_AFTER_MS: u64 = 900;
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::taking(
        RUNS_MS,
    ));
    let door = Rounds::new(vec![shell_deltas("c1", "make release")], "Released.");
    let journal = MemoryJournal::new();
    let asking = runner_granted(computer.clone(), ToolSet::All, ToolSet::All, |e| e);

    let parked = run_conversation(
        &door,
        Some(&asking),
        &journal,
        request("ship it"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert!(parked.iter().any(is_awaiting_card), "{parked:?}");
    assert_eq!(computer.last_box(), None, "nothing ran before the yes");
    let asked = duration_of(results_for(&parked, "c1")[0]).expect("the card's result");
    assert!(asked < RUNS_MS, "{asked}");

    tokio::time::sleep(std::time::Duration::from_millis(ANSWERED_AFTER_MS)).await;
    let yes = |executor: Executor| executor.with_approved(["c1".to_string()]);
    let approved = runner_granted(computer.clone(), ToolSet::All, ToolSet::All, yes);
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "make release"}),
    };
    let resumed = resume_conversation(
        &door,
        &approved,
        &journal,
        request("ship it"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1),
    )
    .await;

    assert_eq!(computer.last_box().as_deref(), Some("box_ada"), "it ran");
    let ran = results_for(&resumed, "c1");
    assert_eq!(ran.len(), 1, "{resumed:?}");
    assert_eq!(ran[0].extra.get("ok"), Some(&serde_json::json!(true)));
    let took = duration_of(ran[0]).expect("the approved call's time");
    assert!(took >= RUNS_MS, "{took}");
    assert!(
        took < ANSWERED_AFTER_MS,
        "the wait on the card is not the call's: {took}"
    );
    let kept: Vec<Option<u64>> = results_for(&journal.batches().concat(), "c1")
        .into_iter()
        .map(duration_of)
        .collect();
    assert_eq!(kept, [Some(asked), Some(took)], "both, as they were said");
}

/// A CALL ITS POLICY REFUSES SAYS HOW LONG THE REFUSAL TOOK (#305): nothing ran, and the time is
/// the deciding, however short.
#[tokio::test]
async fn a_call_its_policy_refuses_says_how_long_the_refusal_took() {
    use opengrok_policy::ToolSet;
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let reading = ToolSet::Only(["read_file".to_string()].into_iter().collect());
    let runner = runner_granted(computer.clone(), reading, ToolSet::None, |e| e);
    let door = Rounds::new(vec![shell_deltas("c1", "rm -rf build")], "I may not.");
    let journal = MemoryJournal::new();

    let events = run_conversation(
        &door,
        Some(&runner),
        &journal,
        request("clean up"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(computer.last_box(), None, "a refused call does not run");
    let refused = results_for(&events, "c1");
    assert_eq!(refused.len(), 1, "{events:?}");
    assert_eq!(refused[0].extra.get("ok"), Some(&serde_json::json!(false)));
    let said = refused[0].extra["content"].as_str().unwrap_or_default();
    assert!(
        said.contains("may not make coworker"),
        "the grant's no: {said}"
    );
    let decided = duration_of(refused[0]).expect("a refusal says its time to decide");
    let kept: Vec<Option<u64>> = results_for(&journal.batches().concat(), "c1")
        .into_iter()
        .map(duration_of)
        .collect();
    assert_eq!(kept, [Some(decided)]);
}

/// A RESULT WRITTEN FROM A CARD'S ANSWER SAYS NO TIME (#305). A form submitted or a box handed
/// back is settled outside the loop, which runs nothing for it: a number there would be made up,
/// so the key is left out, as on every frame journaled before the field existed.
#[tokio::test]
async fn a_result_settled_from_a_cards_answer_says_no_time() {
    let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
    let runner = tool_runner_on(computer.clone(), |executor| executor);
    let call = opengrok_tools::ToolCall {
        id: "f1".to_string(),
        name: opengrok_tools::REQUEST_USER_FORM.to_string(),
        arguments: serde_json::json!({ "title": "Sign in" }),
    };

    let events = resume_conversation(
        &MockDoor::echoing(),
        &runner,
        &MemoryJournal::new(),
        request("sign in"),
        RunContext::new("t1", "r1", 1),
        Resumption::settled(call, 1, "The person filled the form in."),
    )
    .await;

    assert_eq!(computer.last_box(), None, "nothing ran");
    let settled = results_for(&events, "f1");
    assert_eq!(settled.len(), 1, "{events:?}");
    assert!(settled[0].extra.get("durationMs").is_none(), "{settled:?}");
}
