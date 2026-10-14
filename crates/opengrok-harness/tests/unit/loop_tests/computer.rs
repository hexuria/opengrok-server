//! The computer tools: screenshots, repeated screens, denied machines, user forms.

use super::*;

fn shot(call_id: &str) -> opengrok_tools::ToolResult {
    opengrok_tools::ToolResult::ok(call_id, "screenshot of the 1280x800 screen attached")
        .with_image(opengrok_tools::ToolImage {
            mime: "image/png".into(),
            base64: "iVBORw0KGgo=".into(),
            width: 1280,
            height: 800,
            visibility: opengrok_tools::ImageVisibility::Agent,
        })
}

#[test]
fn a_tool_result_with_a_picture_becomes_a_message_with_an_image() {
    let message = tool_result_message(&shot("c1"));
    assert_eq!(message.role, "tool");
    assert_eq!(message.tool_call_id.as_deref(), Some("c1"));
    assert!(message.content.starts_with("screenshot"));
    assert_eq!(message.images.len(), 1);
    assert_eq!(message.images[0].mime, "image/png");

    let plain = tool_result_message(&opengrok_tools::ToolResult::ok("c2", "done"));
    assert!(plain.images.is_empty());
}

#[test]
fn an_empty_result_array_carries_the_dead_end_sentence() {
    let empty = tool_result_message(&opengrok_tools::ToolResult::ok(
        "c1",
        r#"{"ok":true,"result":[]}"#,
    ));
    assert!(
        empty.content.contains(intent::EMPTY_RESULT_NUDGE),
        "{:?}",
        empty.content
    );
    let full = tool_result_message(&opengrok_tools::ToolResult::ok(
        "c1",
        r#"{"ok":true,"result":[{"name":"Juan"}]}"#,
    ));
    assert!(
        !full.content.contains(intent::EMPTY_RESULT_NUDGE),
        "{:?}",
        full.content
    );
    let garbage = tool_result_message(&opengrok_tools::ToolResult::ok("c1", "not-json"));
    assert_eq!(garbage.content, "not-json");
}

/// Screenshots are the widest thing in a request; only the last two say where the screen is.
#[test]
fn only_the_two_most_recent_screenshots_travel() {
    let mut messages: Vec<ChatMessage> = (1..=4)
        .map(|n| tool_result_message(&shot(&format!("c{n}"))))
        .collect();
    messages.insert(2, ChatMessage::text("assistant", "clicking"));

    keep_recent_images(&mut messages, RECENT_IMAGES);

    let carried: Vec<bool> = messages.iter().map(|m| !m.images.is_empty()).collect();
    assert_eq!(carried, vec![false, false, false, true, true]);
    // The words stay even where the picture went.
    assert!(messages[0].content.starts_with("screenshot"));
    assert_eq!(messages[0].tool_call_id.as_deref(), Some("c1"));
}

/// A person's no is about their machine, not about one spelling of the command. Seen live:
/// the model reworded the command eight times after a deny and the round cap was what
/// stopped it.
#[tokio::test]
async fn a_denied_machine_ends_the_run_even_when_the_command_is_reworded() {
    struct RewordingDoor(Mutex<usize>);
    #[async_trait::async_trait]
    impl ModelDoor for RewordingDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let n = {
                let mut count = self.0.lock().unwrap();
                *count += 1;
                *count
            };
            let script = vec![
                ModelDelta::ToolCallStart {
                    id: format!("c{n}"),
                    name: "user_machine_shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: format!("c{n}"),
                    delta: format!(r#"{{"command":"open -a Safari https://facebook.com/{n}"}}"#),
                },
                ModelDelta::ToolCallEnd {
                    id: format!("c{n}"),
                },
            ];
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    struct DenyingSink;
    #[async_trait::async_trait]
    impl opengrok_tools::UserMachineSink for DenyingSink {
        async fn decide(
            &self,
            _account_id: &opengrok_core::id::AccountId,
            _command: &str,
        ) -> opengrok_tools::UserMachineVerdict {
            opengrok_tools::UserMachineVerdict::Deny("the machine's owner said no".into())
        }
        async fn run(
            &self,
            _account_id: &opengrok_core::id::AccountId,
            _command: &str,
            _call_id: &str,
            _approved: bool,
        ) -> opengrok_tools::UserMachineReply {
            opengrok_tools::UserMachineReply::Refused("the machine's owner said no".into())
        }
    }

    let journal = MemoryJournal::new();
    let runner = tool_runner_with(|executor| executor.with_user_machine(Arc::new(DenyingSink)));
    let events = run_conversation(
        &RewordingDoor(Mutex::new(0)),
        Some(&runner),
        &journal,
        request("visit facebook.com"),
        "t1",
        "r1",
        1,
    )
    .await;

    let starts = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .count();
    assert_eq!(starts, 2, "a no, one more ask, then stop: {events:?}");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    assert!(
        last.extra
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .contains("twice"),
        "{last:?}"
    );
}

/// A computer whose screen changes on every look (or not), for the screen budgets.
struct ScreenComputer {
    changing: bool,
    looks: Mutex<usize>,
}
#[async_trait::async_trait]
impl opengrok_box::Computer for ScreenComputer {
    async fn create(&self, _ttl: Option<u64>) -> opengrok_box::BoxResult<String> {
        Ok("box_screen".into())
    }
    async fn run(
        &self,
        _b: &str,
        _c: &str,
        _t: u32,
    ) -> opengrok_box::BoxResult<opengrok_box::CommandOutput> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn start(
        &self,
        _b: &str,
        _c: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(
        &self,
        _b: &str,
        _p: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn read_file(&self, _b: &str, _p: &str) -> opengrok_box::BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> opengrok_box::BoxResult<String> {
        Ok(String::new())
    }
    async fn stop(&self, _b: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> opengrok_box::BoxResult<String> {
        Ok("running".into())
    }
    async fn offers_a_screen(&self, _box_id: &str) -> bool {
        true
    }
    async fn screen_url(
        &self,
        _b: &str,
        _screen: &opengrok_box::Screen,
    ) -> opengrok_box::BoxResult<Option<String>> {
        Ok(Some("http://127.0.0.1:1/vnc.html".into()))
    }
    async fn screenshot(
        &self,
        _b: &str,
        _screen: &opengrok_box::Screen,
    ) -> opengrok_box::BoxResult<opengrok_box::Screenshot> {
        let n = {
            let mut looks = self.looks.lock().unwrap();
            *looks += 1;
            *looks
        };
        Ok(opengrok_box::Screenshot {
            mime: "image/png".into(),
            png_base64: if self.changing {
                format!("frame-{n}")
            } else {
                "frame-same".into()
            },
            width: 1280,
            height: 800,
        })
    }
    async fn act(
        &self,
        _b: &str,
        _screen: &opengrok_box::Screen,
        _a: &opengrok_box::CuaAction,
    ) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
}

/// A model that only ever takes screenshots.
struct LookingDoor(Mutex<usize>, usize);
#[async_trait::async_trait]
impl ModelDoor for LookingDoor {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let n = {
            let mut count = self.0.lock().unwrap();
            *count += 1;
            *count
        };
        let script = if n > self.1 {
            vec![ModelDelta::Text("done looking".to_string())]
        } else {
            vec![
                ModelDelta::ToolCallStart {
                    id: format!("c{n}"),
                    name: "computer".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: format!("c{n}"),
                    delta: r#"{"action":"screenshot"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: format!("c{n}"),
                },
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn screen_runner(changing: bool) -> ToolRunner {
    tool_runner_on(
        Arc::new(ScreenComputer {
            changing,
            looks: Mutex::new(0),
        }),
        |executor| executor.with_screen(true),
    )
}

/// Looking is the work on a desktop: twelve screenshots must not trip the eight-call cap
/// meant for chatter.
#[tokio::test]
async fn looking_at_a_changing_screen_is_not_chatter() {
    let journal = MemoryJournal::new();
    let runner = screen_runner(true);
    let events = run_conversation(
        &LookingDoor(Mutex::new(0), 12),
        Some(&runner),
        &journal,
        request("find the terminal"),
        "t1",
        "r1",
        1,
    )
    .await;
    let looks = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .count();
    assert_eq!(looks, 12, "{events:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Step PNGs are `agent` and the journal drops their bytes. The run-end pin keeps one PNG.
#[tokio::test]
async fn computer_step_shots_are_agent_and_the_journal_keeps_the_end_pin() {
    let journal = MemoryJournal::new();
    let runner = screen_runner(true);
    let events = run_conversation(
        &LookingDoor(Mutex::new(0), 3),
        Some(&runner),
        &journal,
        request("find the terminal"),
        "t1",
        "r1",
        1,
    )
    .await;
    let live_shots: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .filter_map(|event| event.extra.get("image"))
        .collect();
    assert_eq!(
        live_shots.len(),
        4,
        "three steps plus the end pin: {live_shots:?}"
    );
    assert!(
        live_shots.iter().take(3).all(|image| {
            image["visibility"] == "agent" && image.get("base64").and_then(|v| v.as_str()).is_some()
        }),
        "live SSE still carries step PNGs for the Computer pane: {live_shots:?}"
    );
    assert_eq!(live_shots.last().unwrap()["visibility"], "end");
    assert!(
        live_shots
            .last()
            .unwrap()
            .get("base64")
            .and_then(|v| v.as_str())
            .is_some_and(|b| !b.is_empty()),
        "the end pin keeps the PNG: {live_shots:?}"
    );

    let journaled: Vec<_> = journal
        .batches()
        .into_iter()
        .flatten()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .filter_map(|event| event.extra.get("image").cloned())
        .collect();
    let agent_without_bytes = journaled
        .iter()
        .filter(|image| image["visibility"] == "agent" && image.get("base64").is_none())
        .count();
    let end_with_bytes = journaled
        .iter()
        .filter(|image| {
            image["visibility"] == "end"
                && image
                    .get("base64")
                    .and_then(|v| v.as_str())
                    .is_some_and(|b| !b.is_empty())
        })
        .count();
    assert_eq!(
        agent_without_bytes, 3,
        "journal must drop step PNG bytes: {journaled:?}"
    );
    assert_eq!(
        end_with_bytes, 1,
        "journal keeps the end pin PNG: {journaled:?}"
    );
}

/// The case the earlier test could not fail: a real streaming model sends the
/// arguments in pieces, and a per-fragment scrub sees no JSON to scrub.
#[test]
fn streamed_fragments_of_a_smuggled_password_are_assembled_and_scrubbed() {
    use opengrok_wire::agui::EventType;
    let ev = |kind: EventType| Event::new(kind, 0);
    let events = vec![
        ev(EventType::ToolCallStart)
            .with("toolCallId", "call-1")
            .with("toolCallName", opengrok_tools::REQUEST_USER_FORM),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-1")
            .with(
                "delta",
                "{\"title\":\"Log in\",\"values\":{\"password\":\"s3",
            ),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-1")
            .with("delta", "cret\"}}"),
        ev(EventType::ToolCallEnd).with("toolCallId", "call-1"),
        // A tool that carries no secrets keeps its fragments exactly as they were.
        ev(EventType::ToolCallStart)
            .with("toolCallId", "call-2")
            .with("toolCallName", "shell"),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-2")
            .with("delta", "{\"cmd\":\"ls"),
        ev(EventType::ToolCallArgs)
            .with("toolCallId", "call-2")
            .with("delta", " -la\"}"),
    ];
    let out = scrub_streamed_tool_args(events);
    let text = serde_json::to_string(&out).expect("serialise");
    assert!(
        !text.contains("s3cret"),
        "the smuggled password must not survive: {text}"
    );
    assert!(!text.contains("s3"), "not even a fragment of it: {text}");
    let form_args: Vec<&Event> = out
        .iter()
        .filter(|e| {
            e.event_type == opengrok_wire::agui::EventType::ToolCallArgs
                && e.extra
                    .get("toolCallId")
                    .and_then(serde_json::Value::as_str)
                    == Some("call-1")
        })
        .collect();
    assert_eq!(
        form_args.len(),
        1,
        "one assembled fragment stands in for the pieces"
    );
    let delta = form_args[0]
        .extra
        .get("delta")
        .and_then(serde_json::Value::as_str)
        .unwrap();
    assert!(
        delta.contains("Log in"),
        "non-secret fields are kept: {delta}"
    );
    let shell_args: Vec<&Event> = out
        .iter()
        .filter(|e| {
            e.extra
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                == Some("call-2")
        })
        .collect();
    assert_eq!(shell_args.len(), 3, "an ordinary tool is untouched");
    assert!(text.contains("ls"), "{text}");
}

#[tokio::test]
async fn each_user_form_in_one_completion_gets_an_awaiting_custom_and_the_stream_closes() {
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &MockDoor::asking_for_stacked_user_forms(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
    )
    .await;
    let forms: Vec<_> = events
        .iter()
        .filter(|event| {
            event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|v| v.as_str()) == Some("run-awaiting-approval")
                && event.extra.get("reason").and_then(|v| v.as_str()) == Some("user-form")
        })
        .collect();
    assert_eq!(
        forms.len(),
        3,
        "one CUSTOM per stacked request_user_form: {events:?}"
    );
    let call_ids: Vec<_> = forms
        .iter()
        .filter_map(|event| event.extra.get("callId").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        call_ids,
        vec!["mock-form-1", "mock-form-2", "mock-form-3"],
        "{call_ids:?}"
    );
    assert_eq!(
        events.last().map(|event| event.event_type),
        Some(EventType::RunFinished),
        "HITL park must close the SSE or Waiting spins forever: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::RunStarted),
        "{events:?}"
    );
}

#[tokio::test]
async fn two_website_logins_keep_provider_call_ids_on_each_awaiting_custom() {
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &MockDoor::asking_for_two_website_logins(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
    )
    .await;
    let call_ids: Vec<_> = events
        .iter()
        .filter(|event| {
            event.event_type == EventType::Custom
                && event.extra.get("name").and_then(|v| v.as_str()) == Some("run-awaiting-approval")
                && event.extra.get("reason").and_then(|v| v.as_str()) == Some("user-form")
        })
        .filter_map(|event| event.extra.get("callId").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        call_ids,
        vec!["call-42628be6", "call-42628be6-1"],
        "provider-style parallel ids must each get a CUSTOM: {events:?}"
    );
}

/// The same picture four times running is waiting, not working; the run says so.
#[tokio::test]
async fn the_same_screen_four_times_ends_the_run() {
    let journal = MemoryJournal::new();
    let runner = screen_runner(false);
    let events = run_conversation(
        &LookingDoor(Mutex::new(0), 40),
        Some(&runner),
        &journal,
        request("wait for the page"),
        "t1",
        "r1",
        1,
    )
    .await;
    let looks = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .count();
    assert_eq!(looks, SAME_SCREEN_LIMIT, "{events:?}");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    assert!(
        last.extra
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .contains("has not changed"),
        "{last:?}"
    );
}

/// A second home-directory find in the same turn stops. The person sees the
/// refusal, not another 90s walk.
#[tokio::test]
async fn a_second_home_directory_find_ends_the_turn() {
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
                round <= 2,
                "a second home-directory find ends the turn: round {round}"
            );
            Ok(Box::pin(futures::stream::iter(
                ums_deltas("c1", "find ~ -name AGENT.md", "I'll look again")
                    .into_iter()
                    .map(Ok),
            )))
        }
    }

    let ran = Arc::new(Mutex::new(0usize));
    let ran_tool = ran.clone();
    let tool: LocalTool = Arc::new(move |call| {
        *ran_tool.lock().unwrap() += 1;
        opengrok_tools::ToolResult::ok(&call.id, "should not run")
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

    assert_eq!(*ran.lock().unwrap(), 0, "neither find is dispatched");
    let text = assistant_text(&events);
    assert!(
        text.contains("home directory"),
        "the second refusal is the sentence: {text:?}"
    );
    assert!(!text.contains("AGENT.md"), "{text:?}");
    let timing = run_timing_value(&events).expect("run-timing");
    assert_eq!(timing["model_ms"].as_array().map(Vec::len), Some(2));
}

/// The Hog Rider turn walked `find ~`, then `find /Users/uriah`: two spellings of one walk. A
/// refusal on an ordinary tool now counts only when it repeats the last failure (#183), and a
/// home-directory walk is kept out of that rule, or a respelled second walk was one more round.
#[tokio::test]
async fn a_second_home_directory_find_spelled_differently_ends_the_turn() {
    let door = Rounds::new(
        vec![
            ums_deltas("c1", "find ~ -name AGENT.md", "I'll look up AGENT.md"),
            ums_deltas("c2", "find /Users/uriah -name AGENT.md", "I'll look again"),
            ums_deltas("c3", "gpui-agent invoke profile.list", "I'll list them"),
        ],
        "unreachable",
    );
    let ran = Arc::new(Mutex::new(0usize));
    let ran_tool = ran.clone();
    let tool: LocalTool = Arc::new(move |call| {
        *ran_tool.lock().unwrap() += 1;
        opengrok_tools::ToolResult::ok(&call.id, "should not run")
    });
    let events = run_conversation(
        &door,
        Some(&ums_runner(tool)),
        &MemoryJournal::new(),
        request("open the profile"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*ran.lock().unwrap(), 0, "neither find is dispatched");
    assert_eq!(door.calls(), 2, "the second walk ends the turn");
    assert!(
        assistant_text(&events).contains("home directory"),
        "{events:?}"
    );
}

/// Grey run 01a0c9ed: hello exited 0, then profile.search was skipped as a
/// second listing and the chat stayed empty. Hello is a probe. The search runs.
#[tokio::test]
async fn hello_does_not_consume_the_listing_so_profile_search_runs() {
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
                1 => ums_deltas("c1", "gpui-agent hello", "I'll probe the host"),
                2 => ums_deltas(
                    "c2",
                    "GPUI_AGENT_ADDR=127.0.0.1:17421 gpui-agent invoke profile.search --q juan",
                    "I'll search",
                ),
                _ => vec![ModelDelta::Text(
                    "Juan Dela Cruz, TIN 00000000000000.".to_string(),
                )],
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let commands = Arc::new(Mutex::new(Vec::<String>::new()));
    let commands_run = commands.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let command = call.arguments["command"].as_str().unwrap_or("").to_string();
            commands_run.lock().unwrap().push(command.clone());
            let body = if command.contains("profile.search") {
                "exit 0\n--- stdout ---\nJuan Dela Cruz TIN 00000000000000"
            } else {
                "exit 0\n--- stdout ---\n{\"hello\":{\"app\":\"bir-desktop\",\"ready\":true}}"
            };
            opengrok_tools::ToolResult::ok(&call.id, body)
        }))),
        &MemoryJournal::new(),
        request("get juan dela cruz tax profile"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = commands.lock().unwrap();
    assert_eq!(ran.len(), 2, "hello and the search both run: {ran:?}");
    assert!(
        ran.iter().any(|command| command.contains("profile.search")),
        "profile.search must be dispatched: {ran:?}"
    );
    let text = assistant_text(&events);
    assert!(
        text.contains("Juan Dela Cruz"),
        "the search answer is the chat: {text:?}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("i'll probe"),
        "{text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// A third catalog read used to finish before any TEXT_MESSAGE. The person
/// still gets the sentence from the read that actually ran.
#[tokio::test]
async fn a_repeated_listing_closes_with_the_catalog_sentence() {
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
                round <= 3,
                "the third listing closes the turn: round {round}"
            );
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.list", "I'll list"),
                2 => ums_deltas("c2", "gpui-agent invoke profile.search --q juan", "again"),
                3 => ums_deltas("c3", "gpui-agent invoke profile.list", "once more"),
                _ => Vec::new(),
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
            opengrok_tools::ToolResult::ok(
                &call.id,
                "exit 0\n--- stdout ---\nJuan Dela Cruz TIN 00000000000000",
            )
        }))),
        &MemoryJournal::new(),
        request("list tax profiles"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*n.lock().unwrap(), 1, "only the first catalog read runs");
    let text = assistant_text(&events);
    assert!(
        text.contains("Juan Dela Cruz"),
        "a repeated listing still leaves a sentence: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Green run 01a0ca61: `profile.create` ignores name and tin, returns only
/// `{view: profile-manager}`, and quote variants of that invoke burned all
/// 8 model calls. The second create is the same action. It does not run,
/// and the chat is the editor sentence.
#[tokio::test]
async fn a_second_profile_create_closes_with_the_editor_sentence() {
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
                round <= 4,
                "a repeated profile.create closes the turn: round {round}"
            );
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.list", "I'll list"),
                2 => ums_deltas(
                    "c2",
                    "gpui-agent invoke profile.create --arg name=Juana Jane --arg tin=00000000000001",
                    "",
                ),
                3 => ums_deltas(
                    "c3",
                    "gpui-agent invoke profile.create --arg name='Juana Jane' --arg tin=00000000000001",
                    "",
                ),
                4 => ums_deltas(
                    "c4",
                    "gpui-agent invoke profile.create --arg 'name=Juana Jane' --arg tin=00000000000001",
                    "",
                ),
                _ => Vec::new(),
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let commands = Arc::new(Mutex::new(Vec::<String>::new()));
    let commands_run = commands.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let command = call.arguments["command"].as_str().unwrap_or("").to_string();
            commands_run.lock().unwrap().push(command.clone());
            let body = if command.contains("profile.list") {
                "exit 0\n--- stdout ---\n{\"result\":[{\"name\":\"Juan Dela Cruz\"}]}\n"
            } else if command.contains("name=Juana Jane") && !command.contains('\'') {
                "exit 2\n--- stderr ---\nerror: unexpected argument 'Jane' found\n"
            } else {
                "exit 0\n--- stdout ---\n{\"v\":2,\"ok\":true,\"result\":{\"view\":\"profile-manager\"}}\n"
            };
            opengrok_tools::ToolResult::ok(&call.id, body)
        }))),
        &MemoryJournal::new(),
        request("create Juana Jane"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = commands.lock().unwrap();
    assert_eq!(
        ran.iter()
            .filter(|command| command.contains("profile.create"))
            .count(),
        2,
        "the failed quote and the one open run; the repeat does not: {ran:?}"
    );
    let text = assistant_text(&events);
    assert!(
        text.contains("Opened profile-manager. Nothing was saved."),
        "the editor sentence is the chat, not the round cap: {text:?}"
    );
    assert!(
        !text.contains("limit of 8 model calls"),
        "the cap is not the answer: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// After the editor opens, a different command still runs. Create is not a
/// wall in front of set-value.
#[tokio::test]
async fn set_value_after_profile_create_still_runs() {
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
                round <= 3,
                "set-value is one more command, then the answer: round {round}"
            );
            let script = match round {
                1 => ums_deltas("c1", "gpui-agent invoke profile.create", ""),
                2 => ums_deltas("c2", "gpui-agent set-value profile-name 'Juana Jane'", ""),
                3 => vec![ModelDelta::Text(
                    "The editor is open for Juana Jane. Nothing is saved yet.".to_string(),
                )],
                _ => Vec::new(),
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }
    let commands = Arc::new(Mutex::new(Vec::<String>::new()));
    let commands_run = commands.clone();
    let events = run_conversation(
        &Door(Mutex::new(0)),
        Some(&ums_runner(Arc::new(move |call| {
            let command = call.arguments["command"].as_str().unwrap_or("").to_string();
            commands_run.lock().unwrap().push(command.clone());
            let body = if command.contains("set-value") {
                "exit 0\n--- stdout ---\n{\"ok\":true,\"result\":{\"id\":\"profile-name\"}}\n"
            } else {
                "exit 0\n--- stdout ---\n{\"v\":2,\"ok\":true,\"result\":{\"view\":\"profile-manager\"}}\n"
            };
            opengrok_tools::ToolResult::ok(&call.id, body)
        }))),
        &MemoryJournal::new(),
        request("create Juana Jane"),
        "t1",
        "r1",
        1,
    )
    .await;
    let ran = commands.lock().unwrap();
    assert!(
        ran.iter().any(|command| command.contains("set-value")),
        "set-value still runs after create: {ran:?}"
    );
    let text = assistant_text(&events);
    assert!(
        text.contains("Juana Jane"),
        "the model's answer is the chat: {text:?}"
    );
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}
