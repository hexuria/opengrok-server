//! Recipe replay: each plays at most once per request, journal-kept across restarts and budgets.

use super::*;

/// `ls` then `cat README.md` on the box. Both used to be "the one listing": the `cat` got a
/// synthetic "A listing already succeeded" and the file was never read (#183).
#[tokio::test]
async fn ls_then_cat_reads_the_file() {
    let door = Rounds::new(
        vec![
            shell_deltas("c1", "ls"),
            shell_deltas("c2", "cat README.md"),
        ],
        "The README says hello.",
    );
    let (runner, ran) = shell_runner(&[
        ("ls", "README.md\n[exit code 0]"),
        ("cat README.md", "# hello\n[exit code 0]"),
    ]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("what does my README say?"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(ran.lock().unwrap().as_slice(), ["ls", "cat README.md"]);
    assert!(
        !events.iter().any(|event| {
            event.event_type == EventType::ToolCallResult
                && event
                    .extra
                    .get("content")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains("A listing already succeeded"))
        }),
        "{events:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(assistant_text(&events).contains("README says hello"));
}

/// Another program's `invoke x.list` is not the BIR catalog: its second read runs. The
/// one-listing rule matched any `invoke <name>.list` on the box shell before it was scoped to
/// the binary it was measured on.
#[tokio::test]
async fn another_programs_invoke_list_is_not_the_one_catalog_listing() {
    let door = Rounds::new(
        vec![
            shell_deltas("c1", "todo invoke items.list"),
            shell_deltas("c2", "todo invoke items.get --id 3"),
        ],
        "Item 3 is due Friday.",
    );
    let (runner, ran) = shell_runner(&[
        ("todo invoke items.list", "1 2 3\n[exit code 0]"),
        ("todo invoke items.get --id 3", "due friday\n[exit code 0]"),
    ]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("when is item 3 due?"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(
        ran.lock().unwrap().as_slice(),
        ["todo invoke items.list", "todo invoke items.get --id 3"]
    );
    assert!(assistant_text(&events).contains("due Friday"), "{events:?}");
}

/// A grep with no match (exit 1) and a failing test run (exit 101) are two different
/// outcomes of ordinary work, not two failed retries. The turn used to end on the second
/// with the first line of the test output as the answer.
#[tokio::test]
async fn non_zero_exits_from_normal_commands_do_not_end_the_turn() {
    let door = Rounds::new(
        vec![
            shell_deltas("c1", "grep -rn TODO src"),
            shell_deltas("c2", "cargo test"),
            shell_deltas("c3", "cat src/lib.rs"),
        ],
        "One test fails: parse rejects an empty line.",
    );
    let (runner, ran) = shell_runner(&[
        ("grep -rn TODO src", "[exit code 1]"),
        (
            "cargo test",
            "test result: FAILED. 1 failed\n[exit code 101]",
        ),
        ("cat src/lib.rs", "pub fn parse() {}\n[exit code 0]"),
    ]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("why do my tests fail?"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(door.calls(), 4);
    assert_eq!(ran.lock().unwrap().len(), 3);
    let text = assistant_text(&events);
    assert!(text.contains("parse rejects an empty line"), "{text:?}");
    assert!(!text.contains("test result: FAILED"), "{text:?}");
}

/// #183's own case: a grep with no match, then the same search case-insensitive, is a new
/// command and not the retry diary. The second grep runs, and so does the read after it.
#[tokio::test]
async fn a_grep_with_no_match_then_grep_i_does_not_end_the_turn() {
    let door = Rounds::new(
        vec![
            shell_deltas("c1", "grep -rn todo src"),
            shell_deltas("c2", "grep -rni todo src"),
            shell_deltas("c3", "cat src/lib.rs"),
        ],
        "src/lib.rs:3 has the TODO.",
    );
    let (runner, ran) = shell_runner(&[
        ("grep -rn todo src", "[exit code 1]"),
        ("grep -rni todo src", "[exit code 1]"),
        ("cat src/lib.rs", "// TODO: parse\n[exit code 0]"),
    ]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("where is the todo?"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = ran.lock().unwrap().clone();
    assert_eq!(ran.len(), 3, "{ran:?}");
    assert_eq!(assistant_text(&events), "src/lib.rs:3 has the TODO.");
}

/// `python` missing on the box is fixed by `python3`. Exit 127 used to set the failure streak
/// straight to its ceiling, so the retry was never asked for.
#[tokio::test]
async fn a_missing_python_gets_a_python3_retry() {
    let door = Rounds::new(
        vec![
            shell_deltas("c1", "python x.py"),
            shell_deltas("c2", "python3 x.py"),
        ],
        "Done.",
    );
    let (runner, ran) = shell_runner(&[
        (
            "python x.py",
            "bash: python: command not found\n[exit code 127]",
        ),
        ("python3 x.py", "ok\n[exit code 0]"),
    ]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("run x.py"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(
        ran.lock().unwrap().as_slice(),
        ["python x.py", "python3 x.py"]
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(assistant_text(&events).contains("Done."));
}

/// Two different files that are not there are two outcomes, not a retry of one: a tool with no
/// `command` argument is told apart by its arguments. The fake answers a missing file the way
/// the executor does: `cat` fails in the box, the box says `Refused`, and `read_file` returns a
/// refusal. An earlier fake returned ok with "[exit code 1]", which the executor never produces,
/// and so hid that every refusal still counted (#183, verifier's probe).
#[tokio::test]
async fn two_different_missing_files_do_not_end_the_turn() {
    fn read(id: &str, path: &str) -> Vec<ModelDelta> {
        vec![
            ModelDelta::ToolCallStart {
                id: id.to_string(),
                name: "read_file".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: id.to_string(),
                delta: serde_json::json!({ "path": path }).to_string(),
            },
            ModelDelta::ToolCallEnd { id: id.to_string() },
        ]
    }
    let door = Rounds::new(
        vec![
            read("c1", "notes.md"),
            read("c2", "NOTES.md"),
            read("c3", "docs/notes.md"),
        ],
        "Found it in docs/notes.md.",
    );
    let reads = Arc::new(Mutex::new(0usize));
    let counted = reads.clone();
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "read_file" } }),
        Arc::new(move |call| {
            let n = {
                let mut reads = counted.lock().unwrap();
                *reads += 1;
                *reads
            };
            if n < 3 {
                let path = call.arguments["path"].as_str().unwrap_or_default();
                opengrok_tools::ToolResult::refused(
                    &call.id,
                    format!(
                        "the computer refused the request (1): cat: {path}: No such file or directory"
                    ),
                )
            } else {
                opengrok_tools::ToolResult::ok(&call.id, "the notes")
            }
        }),
    );
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("read my notes"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*reads.lock().unwrap(), 3);
    assert!(
        assistant_text(&events).contains("docs/notes.md"),
        "{events:?}"
    );
}

/// The streak still does its job for the same command failing the same way: the second
/// identical failure ends the turn with one short fact, not a diary of retries.
#[tokio::test]
async fn the_same_command_failing_twice_still_ends_the_turn() {
    let door = Rounds::new(
        vec![
            shell_deltas("c1", "make build"),
            shell_deltas("c2", "make build"),
        ],
        "unreachable",
    );
    let (runner, _) = shell_runner(&[(
        "make build",
        "make: *** No rule to make target 'build'.  Stop.\n[exit code 2]",
    )]);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("build it"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(door.calls(), 2);
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(
        assistant_text(&events).contains("No rule to make target"),
        "{events:?}"
    );
}

fn recipe_deltas(id: &str, recipe: &str) -> Vec<ModelDelta> {
    vec![
        ModelDelta::ToolCallStart {
            id: id.to_string(),
            name: opengrok_tools::RUN_RECIPE.to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: id.to_string(),
            delta: serde_json::json!({ "recipe": recipe }).to_string(),
        },
        ModelDelta::ToolCallEnd { id: id.to_string() },
    ]
}

/// A `run_recipe` that counts its plays.
fn recipe_runner() -> (ToolRunner, Arc<Mutex<usize>>) {
    let plays = Arc::new(Mutex::new(0usize));
    let counted = plays.clone();
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": opengrok_tools::RUN_RECIPE } }),
        Arc::new(move |call| {
            *counted.lock().unwrap() += 1;
            opengrok_tools::ToolResult::ok(&call.id, "played 4 steps; the results page is open")
        }),
    );
    (runner, plays)
}

/// #120, the kabisado run: asked to search YouTube, the coworker played its recipe again and
/// again — typing the same words into a field that already held them. "Run a recipe at most
/// once per request" was a line in the prompt; the loop now keeps it. A replay is answered
/// without touching the box, and asking a second time ends the turn.
#[tokio::test]
async fn a_recipe_is_played_at_most_once_per_request() {
    let door = AlwaysRecipe(Mutex::new(0));
    let (runner, plays) = recipe_runner();
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("search youtube for kabisado"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 1, "the box played it once");
    assert!(*door.0.lock().unwrap() <= 3, "a replay ends the turn");
    let second = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .nth(1)
        .and_then(|event| event.extra.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(second.starts_with("Not played again"), "{second:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(
        assistant_text(&events).contains("search-youtube"),
        "the turn says which recipe it did not replay: {events:?}"
    );
}

/// A door that asks for the same recipe every round (#120, #256).
struct AlwaysRecipe(Mutex<usize>);
#[async_trait::async_trait]
impl ModelDoor for AlwaysRecipe {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let round = {
            let mut count = self.0.lock().unwrap();
            *count += 1;
            *count
        };
        let script = recipe_deltas(&format!("c{round}"), "search-youtube");
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn resumed_context() -> RunContext {
    RunContext::new("t1", "r1", 1)
}

/// #256. A recipe that played before a restart is still played in this request: the sweep's
/// resume starts a new segment, and a segment that forgot it played the recipe again, typing
/// or posting a second time.
#[tokio::test]
async fn a_recipe_played_before_a_restart_is_not_played_again_after_it() {
    let door = AlwaysRecipe(Mutex::new(0));
    let (runner, plays) = recipe_runner();
    let spent = Spent {
        recipes: ["search-youtube".to_string()].into_iter().collect(),
        started_a_tool: true,
        ..Spent::default()
    };
    let events = continue_interrupted(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("search youtube for kabisado"),
        resumed_context(),
        5,
        spent,
        RunBudget::default(),
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 0, "the box did not play it again");
    let first = events
        .iter()
        .find(|event| event.event_type == EventType::ToolCallResult)
        .and_then(|event| event.extra.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(first.starts_with("Not played again"), "{first:?}");
}

/// #256. What a round played is written to the journal as soon as the round learns it, so a
/// resume can read it back.
#[tokio::test]
async fn a_recipe_the_loop_plays_is_written_to_the_journal() {
    let door = AlwaysRecipe(Mutex::new(0));
    let (runner, _) = recipe_runner();
    let journal = MemoryJournal::new();
    run_conversation(
        &door,
        Some(&runner),
        &journal,
        request("search youtube for kabisado"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(recipes_played(&journal), vec!["search-youtube".to_string()]);
    let (frames, spent) = journal.spent_with_frames().remove(0);
    assert_eq!(spent.round, Some(opengrok_core::run::RoundKind::Spoken));
    assert!(
        frames
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult),
        "the play is recorded in the write that closes its call: {frames:?}"
    );
}

/// #256. A resumed segment spends what is left of the run's rounds, not a fresh budget: each
/// resume used to hand the run another `MAX_ROUNDS`.
#[tokio::test]
async fn a_resumed_segment_spends_what_is_left_of_the_run_s_rounds() {
    let door = Rounds::new(
        (0..MAX_ROUNDS + 2)
            .map(|n| shell_deltas(&format!("s{n}"), &format!("echo {n}")))
            .collect(),
        "done",
    );
    let (runner, _) = fix_then_test_runner(usize::MAX);
    let spent = Spent {
        spoken_rounds: MAX_ROUNDS - 1,
        started_a_tool: true,
        ..Spent::default()
    };
    continue_interrupted(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("keep going"),
        resumed_context(),
        5,
        spent,
        RunBudget::default(),
    )
    .await;
    assert!(
        door.calls() <= 2,
        "one round left, then the wrap-up: {} calls",
        door.calls()
    );
}

/// A shell call every round, each a different command, for as long as the door is asked.
fn endless_shell() -> Rounds {
    Rounds::new(
        (0..MAX_ROUNDS + 2)
            .map(|n| shell_deltas(&format!("s{n}"), &format!("echo {n}")))
            .collect(),
        "done",
    )
}

fn two_rounds() -> RunBudget {
    RunBudget::held_to(&opengrok_core::limits::RunLimits {
        max_rounds: std::num::NonZeroU32::new(2),
        ..Default::default()
    })
}

/// A segment the sweep resumed is held to the budget it is handed — what its run captured under
/// its org's ceiling — and not to the server's: a restart must not buy a run what the ceiling
/// took away.
#[tokio::test]
async fn a_resumed_segment_is_held_to_the_budget_it_is_handed() {
    let door = endless_shell();
    let (runner, _) = fix_then_test_runner(usize::MAX);
    let spent = Spent {
        started_a_tool: true,
        ..Spent::default()
    };
    let events = continue_interrupted(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("keep going"),
        resumed_context(),
        5,
        spent,
        two_rounds(),
    )
    .await;
    assert_eq!(door.calls(), 3, "two rounds, then the wrap-up");
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["budget"]["max_rounds"], 2, "{timing}");
}

/// A card's continuation keeps its fresh rounds per answer, of the run's own size: the budget it
/// is handed, never the server's.
#[tokio::test]
async fn a_card_continuation_is_held_to_the_budget_it_is_handed() {
    let door = endless_shell();
    let (runner, ran) = fix_then_test_runner(usize::MAX);
    let call = opengrok_tools::ToolCall {
        id: "approved".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "echo approved"}),
    };
    let resumption = Resumption {
        budget: two_rounds(),
        ..Resumption::approved(call, 1)
    };
    resume_conversation(
        &door,
        &runner,
        &MemoryJournal::new(),
        request("go"),
        resumed_context(),
        resumption,
    )
    .await;
    assert_eq!(
        ran.lock().unwrap()[0],
        "echo approved",
        "the approved call ran"
    );
    assert_eq!(door.calls(), 3, "two rounds, then the wrap-up");
}

/// Two plays of one recipe in ONE completion both reached `run_all`: `played` learns a round's
/// recipes only after the round ran, so neither looked like a replay and the box typed the
/// search twice, the kabisado failure inside a single reply (#120, verifier's probe).
#[tokio::test]
async fn a_recipe_asked_twice_in_one_completion_plays_once() {
    let door = Rounds::new(
        vec![
            [
                recipe_deltas("c1", "search-youtube"),
                recipe_deltas("c2", "search-youtube"),
            ]
            .concat(),
        ],
        "The results page is open.",
    );
    let (runner, plays) = recipe_runner();
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("search youtube for kabisado"),
        "t1",
        "r1",
        1,
    )
    .await;
    let results = tool_results(&events);
    assert_eq!(*plays.lock().unwrap(), 1, "{results:?}");
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results[0].starts_with("played"), "{results:?}");
    assert!(results[1].starts_with("Not played twice"), "{results:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), "The results page is open.");
}

/// A different recipe is a different task and still runs.
#[tokio::test]
async fn a_second_recipe_in_the_same_request_still_plays() {
    let door = Rounds::new(
        vec![
            recipe_deltas("c1", "search-youtube"),
            recipe_deltas("c2", "open-inbox"),
        ],
        "Searched, then opened the inbox.",
    );
    let (runner, plays) = recipe_runner();
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("search youtube, then open my inbox"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 2);
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// The approved recipe ran in the resumed half's first step; the model asking for it again
/// in that same request is a replay too, although it is a new `converse_raw`.
#[tokio::test]
async fn a_resumed_run_does_not_replay_the_recipe_it_was_approved_for() {
    let door = Rounds::new(
        vec![recipe_deltas("c2", "search-youtube")],
        "The results page is open.",
    );
    let (runner, plays) = recipe_runner();
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: opengrok_tools::RUN_RECIPE.to_string(),
        arguments: serde_json::json!({ "recipe": "search-youtube" }),
    };
    let events = resume_conversation(
        &door,
        &runner,
        &MemoryJournal::new(),
        request("search youtube for kabisado"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1),
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 1, "{events:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// #256. Each round is counted once on the log, however it ends: the round a budget ran out on
/// is written before the wrap-up call, and the wrap-up's own write must not count it again.
#[tokio::test]
async fn every_round_is_counted_once_on_the_log() {
    let door = Rounds::new(
        (0..MAX_ROUNDS + 2)
            .map(|n| shell_deltas(&format!("s{n}"), &format!("echo {n}")))
            .collect(),
        "done",
    );
    let (runner, _) = fix_then_test_runner(usize::MAX);
    let journal = MemoryJournal::new();
    run_conversation(
        &door,
        Some(&runner),
        &journal,
        request("keep going"),
        "t1",
        "r1",
        1,
    )
    .await;
    let rounds = journal
        .spent()
        .iter()
        .filter(|spent| spent.round.is_some())
        .count();
    assert_eq!(rounds, MAX_ROUNDS, "{:?}", journal.spent());
}

/// #256, the card path. A recipe played before the card is not played again after it, and the
/// recipe the approved call plays goes on the log like any round's.
#[tokio::test]
async fn a_card_answered_later_in_a_request_does_not_replay_an_earlier_recipe() {
    let door = Rounds::new(
        vec![recipe_deltas("c2", "search-youtube")],
        "Posted, and the search already ran.",
    );
    let (runner, plays) = recipe_runner();
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: opengrok_tools::RUN_RECIPE.to_string(),
        arguments: serde_json::json!({ "recipe": "post-reply" }),
    };
    let journal = MemoryJournal::new();
    let spent = Spent {
        recipes: ["search-youtube".to_string()].into_iter().collect(),
        ..Spent::default()
    };
    resume_conversation(
        &door,
        &runner,
        &journal,
        request("post the reply"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1).having_spent(spent),
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 1, "only the approved recipe played");
    assert_eq!(recipes_played(&journal), vec!["post-reply".to_string()]);
    let (frames, spent) = journal.spent_with_frames().remove(0);
    assert_eq!(spent.round, None, "an approved call is not a round");
    assert!(
        frames
            .iter()
            .any(|event| event.event_type == EventType::ToolCallResult
                && event.extra.get("toolCallId").and_then(|id| id.as_str()) == Some("c1")),
        "the play is recorded in the write that closes its call: {frames:?}"
    );
}

/// A `run_recipe` that refuses a call with no `values` before anything plays, the way the
/// executor refuses a missing parameter, and plays (and counts) one that has them.
fn binding_recipe_runner() -> (ToolRunner, Arc<Mutex<usize>>) {
    let plays = Arc::new(Mutex::new(0usize));
    let counted = plays.clone();
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": opengrok_tools::RUN_RECIPE } }),
        Arc::new(move |call| {
            if call.arguments.get("values").is_none() {
                return opengrok_tools::ToolResult::refused(&call.id, "missing value for `query`");
            }
            *counted.lock().unwrap() += 1;
            opengrok_tools::ToolResult::ok(&call.id, "played 4 steps; the results page is open")
        }),
    );
    (runner, plays)
}

fn recipe_with_values(id: &str, recipe: &str) -> Vec<ModelDelta> {
    vec![
        ModelDelta::ToolCallStart {
            id: id.to_string(),
            name: opengrok_tools::RUN_RECIPE.to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: id.to_string(),
            delta: serde_json::json!({ "recipe": recipe, "values": { "query": "kabisado" } })
                .to_string(),
        },
        ModelDelta::ToolCallEnd { id: id.to_string() },
    ]
}

/// A recipe refused before the box (here a missing parameter, worded so the model can fix it)
/// played nothing. It was counted as played, so the corrected call was answered "Not played
/// again" and the search the person asked for never ran (#120, verifier's probe).
#[tokio::test]
async fn a_recipe_refused_before_the_box_still_plays_when_corrected() {
    let door = Rounds::new(
        vec![
            recipe_deltas("c1", "search-youtube"),
            recipe_with_values("c2", "search-youtube"),
        ],
        "The results page is open.",
    );
    let (runner, plays) = binding_recipe_runner();
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("search youtube for kabisado"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 1, "{:?}", tool_results(&events));
    assert!(
        tool_results(&events)
            .iter()
            .all(|content| !content.starts_with("Not played again")),
        "{:?}",
        tool_results(&events)
    );
    assert_eq!(assistant_text(&events), "The results page is open.");
}

/// One completion names a recipe twice and the first call is refused before the box. The second
/// was answered by the loop with an `ok` result, and `played` took it, so the recipe counted as
/// played although nothing ran: next round's corrected call was a "replay", the turn ended
/// saying the recipe "already ran", and the search never happened (#120 again, verifier's
/// probes A and B on 9e4b6d1).
#[tokio::test]
async fn a_refused_recipe_named_twice_in_one_completion_still_plays_when_corrected() {
    for (probe, first_round) in [
        (
            "both calls lack values",
            [
                recipe_deltas("c1", "search-youtube"),
                recipe_deltas("c2", "search-youtube"),
            ]
            .concat(),
        ),
        (
            "only the first lacks values",
            [
                recipe_deltas("c1", "search-youtube"),
                recipe_with_values("c2", "search-youtube"),
            ]
            .concat(),
        ),
    ] {
        let door = Rounds::new(
            vec![first_round, recipe_with_values("c3", "search-youtube")],
            "The results page is open.",
        );
        let (runner, plays) = binding_recipe_runner();
        let events = run_conversation(
            &door,
            Some(&runner),
            &MemoryJournal::new(),
            request("search youtube for kabisado"),
            "t1",
            "r1",
            1,
        )
        .await;
        let results = tool_results(&events);
        assert_eq!(*plays.lock().unwrap(), 1, "{probe}: {results:?}");
        assert_eq!(results.len(), 3, "{probe}: {results:?}");
        assert!(
            results[1].starts_with("Not played twice"),
            "{probe}: the answer must not claim a refused call ran: {results:?}"
        );
        assert!(results[2].starts_with("played"), "{probe}: {results:?}");
        assert_eq!(
            assistant_text(&events),
            "The results page is open.",
            "{probe}"
        );
    }
}

/// A recipe the box played until a step failed did play: asking for it again is a replay.
#[tokio::test]
async fn a_recipe_that_stopped_part_way_is_not_played_again() {
    let plays = Arc::new(Mutex::new(0usize));
    let counted = plays.clone();
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": opengrok_tools::RUN_RECIPE } }),
        Arc::new(move |call| {
            *counted.lock().unwrap() += 1;
            opengrok_tools::ToolResult::refused(&call.id, "recipe stopped at step 2").part_way()
        }),
    );
    let door = Rounds::new(
        vec![
            recipe_deltas("c1", "search-youtube"),
            recipe_deltas("c2", "search-youtube"),
        ],
        "It stopped at step 2.",
    );
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("search youtube for kabisado"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 1, "{:?}", tool_results(&events));
    assert!(tool_results(&events)[1].starts_with("Not played again"));
}

/// The approved call of a resume that was refused before the box is not carried as played.
#[tokio::test]
async fn a_resumed_recipe_refused_before_the_box_may_be_asked_again() {
    let door = Rounds::new(
        vec![recipe_with_values("c2", "search-youtube")],
        "The results page is open.",
    );
    let (runner, plays) = binding_recipe_runner();
    let call = opengrok_tools::ToolCall {
        id: "c1".to_string(),
        name: opengrok_tools::RUN_RECIPE.to_string(),
        arguments: serde_json::json!({ "recipe": "search-youtube" }),
    };
    let events = resume_conversation(
        &door,
        &runner,
        &MemoryJournal::new(),
        request("search youtube for kabisado"),
        RunContext::new("t1", "r1", 1),
        Resumption::approved(call, 1),
    )
    .await;
    assert_eq!(*plays.lock().unwrap(), 1, "{:?}", tool_results(&events));
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// profile.list then profile.search: one real read, then a facts hop — not a second invoke.
#[tokio::test]
async fn a_second_profile_search_after_list_is_not_executed() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.list", "I'll pull profiles"),
                2 => ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.search --q buwiz",
                    "I'll look up Buwiz",
                ),
                _ => vec![ModelDelta::Text(
                    "TIN 123-456-789. Forms: 1701.".to_string(),
                )],
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let n = Arc::new(Mutex::new(0usize));
    let n_run = n.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            *n_run.lock().unwrap() += 1;
            opengrok_tools::ToolResult::ok(&call.id, "profiles: Buwiz")
        }))),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*n.lock().unwrap(), 1, "second listing must be skipped");
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll pull"), "{text:?}");
    assert!(!lower.contains("i'll look"), "{text:?}");
    assert!(text.contains("TIN 123-456-789"), "{text:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Wrong port then a listing that works: the old error must not become the answer.
#[tokio::test]
async fn a_successful_listing_does_not_paint_a_prior_failure() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = if round == 1 {
                ums_deltas("c1", "gpui-agent hello", "I'll pull the BIR host")
            } else if round == 2 {
                ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.list",
                    "The BIR agent isn't answering",
                )
            } else {
                vec![ModelDelta::Text("I'll look up the profiles.".to_string())]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let n = Arc::new(Mutex::new(0usize));
    let n_run = n.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let i = {
                let mut count = n_run.lock().unwrap();
                let i = *count;
                *count += 1;
                i
            };
            if i == 0 {
                opengrok_tools::ToolResult::refused(&call.id, "connection refused on 17421")
            } else {
                opengrok_tools::ToolResult::ok(&call.id, "profiles: Buwiz")
            }
        }))),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll"), "{text:?}");
    assert!(!lower.contains("isn't answering"), "{text:?}");
    assert!(
        !lower.contains("connection refused"),
        "stale fail must not become chat after a listing: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}
