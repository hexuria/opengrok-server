//! Shell paths: read-only fast paths, retries on missing tools, repeat refusals, edits between tests.

use super::*;

/// Live NativeChat: wrong port, then missing profiles, with a diary of "isn't answering"
/// between them. One silent retry, then one short failure fact, no third hop.
#[tokio::test]
async fn two_failed_tools_emit_one_short_fact_not_a_retry_diary() {
    struct DiaryDoor(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for DiaryDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 2,
                "a third hop is the diary we are killing: round {round}"
            );
            let (text, id, args) = if round == 1 {
                (
                    "I'll pull the BIR host",
                    "c1",
                    r#"{"command":"gpui-agent hello"}"#,
                )
            } else {
                (
                    "The BIR agent isn't answering. I'll try another port.",
                    "c2",
                    r#"{"command":"gpui-agent invoke profile.list"}"#,
                )
            };
            let script = vec![
                ModelDelta::Text(text.to_string()),
                ModelDelta::ToolCallStart {
                    id: id.to_string(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.to_string(),
                    delta: args.to_string(),
                },
                ModelDelta::ToolCallEnd { id: id.to_string() },
            ];
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let n = Arc::new(Mutex::new(0usize));
    let n_run = n.clone();
    let fail: LocalTool = Arc::new(move |call| {
        let i = {
            let mut count = n_run.lock().unwrap();
            let i = *count;
            *count += 1;
            i
        };
        let why = if i == 0 {
            "connection refused on 17421"
        } else {
            "no profiles in the database"
        };
        opengrok_tools::ToolResult::refused(&call.id, why)
    });
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        fail,
    );

    let events = run_conversation(
        &DiaryDoor(Mutex::new(0)),
        Some(&runner),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll pull"), "{text:?}");
    assert!(!lower.contains("isn't answering"), "{text:?}");
    assert!(!lower.contains("i'll try"), "{text:?}");
    assert!(
        text.contains("no profiles") || text.contains("connection refused"),
        "one short failure fact from the tool: {text:?}"
    );
    assert!(
        text.chars().count() < 240,
        "must not be a multi-paragraph diary: {text:?}"
    );
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["tool_rounds"], 2);
}

/// Live NativeChat Hog Rider: `user_machine_shell` returns ok=true with
/// ExecOutcome::render `exit 127`. One model round, one short fact, no 8-call burn.
#[tokio::test]
async fn missing_binary_on_user_machine_ends_in_one_round() {
    struct Door(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for Door {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            assert!(
                round <= 1,
                "a missing binary must not spend a silent retry: round {round}"
            );
            Ok(Box::pin(futures::stream::iter(
                ums_deltas("c1", "gpui-agent hello", "I'll pull the BIR host")
                    .into_iter()
                    .map(Ok),
            )))
        }
    }

    let miss: LocalTool = Arc::new(|call| {
        opengrok_tools::ToolResult::ok(
            &call.id,
            "exit 127\n--- stderr ---\nzsh: command not found: gpui-agent",
        )
    });

    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(miss)),
        &MemoryJournal::new(),
        request("list profiles"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("i'll pull"), "{text:?}");
    assert!(
        text.contains("command not found"),
        "one short failure fact from the tool: {text:?}"
    );
    assert!(
        text.chars().count() < 240,
        "must not be a multi-paragraph diary: {text:?}"
    );
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["tool_rounds"], 1);
    assert_eq!(timing["model_ms"].as_array().map(Vec::len), Some(1));
}

/// The 4m 17s Hog Rider turn ran `find ~` and `find /Users/uriah`. Those
/// commands must not reach the Mac. The first refusal is for the model, so
/// the next round can call the catalog instead of ending on the refusal.
#[tokio::test]
async fn a_home_directory_find_is_refused_before_it_runs() {
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
                1 => ums_deltas("c1", "find ~ -name AGENT.md", "I'll look up AGENT.md"),
                2 => ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.search --q juan",
                    "I'll search",
                ),
                _ => vec![ModelDelta::Text(
                    "Juan Dela Cruz, TIN 00000000000000.".to_string(),
                )],
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let ran = Arc::new(Mutex::new(Vec::<String>::new()));
    let ran_tool = ran.clone();
    let tool: LocalTool = Arc::new(move |call| {
        let command = call.arguments["command"].as_str().unwrap_or("").to_string();
        ran_tool.lock().unwrap().push(command);
        opengrok_tools::ToolResult::ok(&call.id, "exit 0\n--- stdout ---\nJuan Dela Cruz")
    });

    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(tool)),
        &MemoryJournal::new(),
        request("open the profile"),
        "t1",
        "r1",
        1,
    )
    .await;

    let ran = ran.lock().unwrap();
    assert!(
        ran.iter().all(|command| !command.starts_with("find ")),
        "find must not be dispatched: {ran:?}"
    );
    assert!(
        ran.iter().any(|command| command.contains("profile.search")),
        "the catalog invoke runs after the refusal: {ran:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    assert!(
        text.contains("Juan Dela Cruz"),
        "the catalog answer is the chat, not the refusal: {text:?}"
    );
    assert!(!text.contains("AGENT.md"), "{text:?}");
    assert!(!text.to_ascii_lowercase().contains("i'll look"), "{text:?}");
    // Refused by the loop before anything ran, in no time to speak of (#305); the catalog call
    // after it ran, and says its own.
    let refused = results_for(&events, "c1");
    assert_eq!(
        refused.iter().map(|e| duration_of(e)).collect::<Vec<_>>(),
        [Some(0)]
    );
    assert!(
        duration_of(results_for(&events, "c2")[0]).is_some(),
        "{events:?}"
    );
}

#[test]
fn listing_and_show_commands_are_readonly_shell_fast_path() {
    let call = |name: &str, command: &str| opengrok_tools::ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: serde_json::json!({ "command": command }),
    };
    assert!(
        !is_readonly_listing_shell(&call("shell", "gpui-agent hello")),
        "a host probe must not consume the one listing"
    );
    assert!(
        !is_readonly_listing_shell(&call(
            opengrok_tools::USER_MACHINE_SHELL,
            "gpui-agent invoke --help"
        )),
        "help is not a catalog read"
    );
    assert!(is_readonly_listing_shell(&call(
        opengrok_tools::USER_MACHINE_SHELL,
        "gpui-agent invoke profile.list"
    )));
    assert!(is_readonly_listing_shell(&call(
        opengrok_tools::USER_MACHINE_SHELL,
        "gpui-agent invoke profile.search --q buwiz"
    )));
    assert!(is_readonly_listing_shell(&call(
        "shell",
        "gpui-agent invoke profile.forms_set.get --arg year=2025"
    )));
    // Ordinary reads on a computer are work, not the turn's one catalog listing (#183).
    assert!(!is_readonly_listing_shell(&call("shell", "ls -la")));
    assert!(!is_readonly_listing_shell(&call("shell", "cat README.md")));
    assert!(!is_readonly_listing_shell(&call(
        "shell",
        "grep deb /etc/apt/sources.list"
    )));
    assert!(!is_readonly_listing_shell(&call(
        "shell",
        "echo opengrok-tool-ran > /tmp/opengrok-tool-ran"
    )));
    assert!(!is_readonly_listing_shell(&call(
        "shell",
        "gpui-agent invoke profile.save"
    )));
    assert!(!is_readonly_listing_shell(&call("computer", "ls")));
}

#[test]
fn a_find_of_the_home_directory_is_a_broad_walk_and_a_deeper_path_is_not() {
    assert!(is_broad_filesystem_walk("find ~ -name AGENT.md"));
    assert!(is_broad_filesystem_walk(
        "find /Users/uriah -name 'AGENT.md'"
    ));
    assert!(is_broad_filesystem_walk(
        "find /Volumes/goldcoders -name AGENT.md"
    ));
    assert!(is_broad_filesystem_walk(
        "export GPUI_AGENT_ADDR=127.0.0.1:17423\nfind $HOME -name AGENT.md"
    ));
    assert!(!is_broad_filesystem_walk(
        "GPUI_AGENT_ADDR=127.0.0.1:17423 gpui-agent invoke profile.list"
    ));
    assert!(!is_broad_filesystem_walk(
        "find /Volumes/goldcoders/reverse-engineer-ebir-forms/bir/crates/bir-desktop/docs -name AGENT.md"
    ));
    assert!(!is_broad_filesystem_walk(
        "find /Users/uriah/code/bir -name AGENT.md"
    ));
    // Run 01a0c9f5. `$HOME/.config` is one component and stays broad.
    // `$HOME/Library/...` is a directory and is not.
    assert!(is_broad_filesystem_walk(
        r#"find "$HOME/.config" -iname '*gpui*'"#
    ));
    assert!(!is_broad_filesystem_walk(
        r#"find "$HOME/Library/Application Support" "$HOME/Library/Preferences" -iname '*gpui*'"#
    ));
    assert!(!is_broad_filesystem_walk(
        "find /Users/uriah/Library/Preferences -iname '*bir*'"
    ));
}

/// The verifier's probe for #183: each round edits the file and runs the test again. The same
/// failing `cargo test` after an edit is the fix-then-test loop, not a retry, so the turn
/// reaches the third run (the one that passes) and ends in the model's words.
#[tokio::test]
async fn an_edit_between_two_failing_test_runs_does_not_end_the_turn() {
    let door = Rounds::new(
        vec![
            [
                write_deltas("w1", "src/lib.rs", "fn parse() { todo!() }"),
                shell_deltas("c1", "cargo test"),
            ]
            .concat(),
            [
                write_deltas("w2", "src/lib.rs", "fn parse() { if line.is_empty() {} }"),
                shell_deltas("c2", "cargo test"),
            ]
            .concat(),
            shell_deltas("c3", "cargo test"),
        ],
        "Fixed: parse now accepts an empty line, and all three tests pass.",
    );
    let (runner, ran) = fix_then_test_runner(3);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("make the tests pass"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = ran.lock().unwrap().clone();
    assert_eq!(
        ran.iter()
            .filter(|command| *command == "cargo test")
            .count(),
        3,
        "{ran:?}"
    );
    assert_eq!(door.calls(), 4);
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let text = assistant_text(&events);
    assert!(text.contains("all three tests pass"), "{text:?}");
    assert!(!text.contains("test result: FAILED"), "{text:?}");
}

/// Progress is new work, not the same success again: reading the same file beside the same
/// failing test changes nothing the test sees, so the second identical round still ends the
/// turn on the failure.
#[tokio::test]
async fn the_same_read_beside_the_same_failing_test_still_ends_the_turn() {
    let round = |n: u8| {
        [
            shell_deltas(&format!("r{n}"), "cat src/lib.rs"),
            shell_deltas(&format!("c{n}"), "cargo test"),
        ]
        .concat()
    };
    let door = Rounds::new(vec![round(1), round(2), round(3)], "unreachable");
    let (runner, _) = fix_then_test_runner(usize::MAX);
    let events = run_conversation(
        &door,
        Some(&runner),
        &MemoryJournal::new(),
        request("make the tests pass"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(door.calls(), 2);
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), "test parse_empty ... FAILED");
}
