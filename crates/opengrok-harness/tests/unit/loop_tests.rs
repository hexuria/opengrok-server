use super::*;
use futures::StreamExt;
use opengrok_tools::Executor;
use opengrok_wire::agui::EventType;
use std::sync::{Arc, Mutex};

/// `run_conversation`, with a sink that sees each event as it is produced, on the default budget.
/// Only these tests ask for it: `POST /ag-ui` streams through `run_conversation_within`, held to its
/// org's ceiling.
#[allow(clippy::too_many_arguments)]
async fn run_conversation_streaming(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    journal: &dyn RunJournal,
    request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
    sink: &dyn EventSink,
) -> Vec<Event> {
    let projection = Projection::new(thread_id, run_id, at_ms);
    let carried = Carried::default();
    let sink = Some(sink);
    converse(
        door, tools, journal, request, projection, run_id, sink, carried,
    )
    .await
}

/// One turn with no tools, every event a client should see collected. Only these tests ask for
/// it: every route runs a turn through `run_conversation_within`.
async fn run_turn(
    door: &dyn ModelDoor,
    request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
) -> Vec<Event> {
    run_turn_with_tools(door, None, request, thread_id, run_id, at_ms).await
}

/// Run a turn, and run any tools the model asked for: one round, no journal. Only these tests
/// ask for it, which is why it lives here; `run_conversation` is the durable multi-round loop.
async fn run_turn_with_tools(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    mut request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
) -> Vec<Event> {
    let mut projection = Projection::new(thread_id, run_id, at_ms);
    let mut events = projection.start();

    // Offer the tools to the model — see the note in `converse`.
    if let Some(runner) = tools {
        request.tools = runner.tool_schemas();
    }

    let budget = RunBudget::default();
    let mut stream = match budget.open(door, request).await {
        Ok(stream) => stream,
        // A door that will not open is a failed run, not a crash: the client gets an ending it can
        // render and reason about (CLAUDE.md #8, fail closed and say why).
        Err(error) => {
            tracing::warn!(%error, "the model door did not open");
            events.extend(projection.fail(error.sentence()));
            return events;
        }
    };

    while let Some(delta) = budget.next(&mut stream).await {
        match delta {
            Ok(delta) => events.extend(projection.push(delta)),
            Err(error) => {
                tracing::warn!(%error, "the model stream broke");
                events.extend(projection.fail(error.sentence()));
                return events;
            }
        }
    }

    // Anything the model asked for, run on the coworker's own computer. The results are emitted as
    // AG-UI tool-result events so a person watching sees what happened, and so the log holds it.
    if let Some(runner) = tools {
        let calls = collect_tool_calls(&events);
        events.extend(box_wake_frame(runner, &mut projection, &calls).await);
        for (result, ms) in runner.run_all(&calls).await {
            events.extend(projection.push_tool_result(&result, Some(ms)));
        }
    }

    events.extend(projection.finish());
    events
}

fn tool_runner() -> ToolRunner {
    tool_runner_on(
        Arc::new(crate::tools::tests_support::RecordingComputer::default()),
        |executor| executor,
    )
}

fn tool_runner_with(shape: impl FnOnce(Executor) -> Executor) -> ToolRunner {
    tool_runner_on(
        Arc::new(crate::tools::tests_support::RecordingComputer::default()),
        shape,
    )
}

/// Ada's runner on any computer, with the executor shaped by the caller (a screen, a sink).
fn tool_runner_on(
    computer: Arc<dyn opengrok_box::Computer>,
    shape: impl FnOnce(Executor) -> Executor,
) -> ToolRunner {
    use opengrok_policy::ToolSet;
    runner_granted(computer, ToolSet::All, ToolSet::None, shape)
}

/// Ada's runner under a grant of `profile`, with `needs_approval` behind a person's yes.
fn runner_granted(
    computer: Arc<dyn opengrok_box::Computer>,
    profile: opengrok_policy::ToolSet,
    needs_approval: opengrok_policy::ToolSet,
    shape: impl FnOnce(Executor) -> Executor,
) -> ToolRunner {
    use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand};
    use opengrok_core::id::{BoxId, CoworkerId};
    use opengrok_tools::{Executor, ToolContext};

    let mut coworker = Coworker::default();
    for command in [
        CoworkerCommand::Hire {
            name: "Ada".to_string(),
            model: "m".to_string(),
            at_ms: 1,
        },
        CoworkerCommand::AssignComputer {
            box_id: BoxId::from_stored("box_ada"),
            mode: BoxMode::Dedicated,
            at_ms: 2,
        },
    ] {
        for event in coworker.decide(command).unwrap() {
            coworker.apply(&event);
        }
    }
    // Permissive unless the caller narrows the grant: these tests are about the loop, not about
    // policy, and an executor built without one now refuses everything by design.
    let account = opengrok_core::id::AccountId::from_stored("acct_ada");
    let policy = opengrok_policy::Context {
        grant: Some(opengrok_policy::Grant {
            principal: account.clone(),
            coworker: CoworkerId::from_stored("cw_ada"),
            profile,
            needs_approval,
            revoked: false,
        }),
        ceiling: Some(opengrok_policy::Ceiling {
            coworker: CoworkerId::from_stored("cw_ada"),
            tools: opengrok_policy::ToolSet::All,
        }),
    };
    ToolRunner::new(
        shape(Executor::with_policy(computer, policy)),
        ToolContext::from_coworker(account, CoworkerId::from_stored("cw_ada"), &coworker),
    )
}

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        model: "mock".to_string(),
        messages: vec![ChatMessage::text("user", text.to_string())],
        ..ModelRequest::default()
    }
}

/// A journal that reports the run stopped from the `stop_after`-th question onwards, and keeps
/// what it was asked to record so the ordering can be asserted.
struct StoppingJournal {
    stop_after: usize,
    asked: Mutex<usize>,
    batches: Mutex<Vec<Vec<Event>>>,
}

impl StoppingJournal {
    fn saying_stop_after(questions: usize) -> Self {
        Self {
            stop_after: questions,
            asked: Mutex::new(0),
            batches: Mutex::new(Vec::new()),
        }
    }

    fn batches(&self) -> Vec<Vec<Event>> {
        self.batches.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl RunJournal for StoppingJournal {
    async fn record(&self, _run_id: &str, events: &[Event]) -> Result<(), JournalError> {
        if let Ok(mut batches) = self.batches.lock() {
            batches.push(events.to_vec());
        }
        Ok(())
    }

    async fn stopped(&self, _run_id: &str) -> bool {
        let Ok(mut asked) = self.asked.lock() else {
            return false;
        };
        *asked += 1;
        *asked > self.stop_after
    }
}

/// Asks for a tool on every call, and counts how many times it was called.
struct CountingToolDoor(Arc<Mutex<usize>>);

#[async_trait::async_trait]
impl ModelDoor for CountingToolDoor {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        if let Ok(mut calls) = self.0.lock() {
            *calls += 1;
        }
        let script = vec![
            ModelDelta::Text("playing it again".to_string()),
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: r#"{"command":"play the recipe"}"#.to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn is_run_stopped(event: &Event) -> bool {
    event.event_type == EventType::Custom
        && event.extra.get("name").and_then(|name| name.as_str()) == Some("run-stopped")
}

/// Prose, longer than the plan-only bound, with no intent opener anywhere.
fn long_answer() -> String {
    "The borrow checker tracks who owns each value and for how long. "
        .repeat(PLAN_ONLY_TEXT_LIMIT / 60 + 2)
        .trim_end()
        .to_string()
}

/// Word by word, the way a provider streams.
fn words(text: &str) -> Vec<ModelDelta> {
    text.split_inclusive(' ')
        .map(|word| ModelDelta::Text(word.to_string()))
        .collect()
}

fn assistant_text(events: &[Event]) -> String {
    events
        .iter()
        .filter(|event| event.event_type == EventType::TextMessageContent)
        .filter_map(|event| event.extra.get("delta").and_then(|d| d.as_str()))
        .collect()
}

fn run_timing_value(events: &[Event]) -> Option<&serde_json::Value> {
    events.iter().rev().find_map(|event| {
        (event.event_type == EventType::Custom
            && event.extra.get("name").and_then(|n| n.as_str()) == Some(RUN_TIMING_NAME))
        .then(|| event.extra.get("value"))
        .flatten()
    })
}

fn ums_deltas(id: &str, command: &str, text: &str) -> Vec<ModelDelta> {
    vec![
        ModelDelta::Text(text.to_string()),
        ModelDelta::ToolCallStart {
            id: id.to_string(),
            name: opengrok_tools::USER_MACHINE_SHELL.to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: id.to_string(),
            delta: format!(r#"{{"command":"{command}"}}"#),
        },
        ModelDelta::ToolCallEnd { id: id.to_string() },
    ]
}

fn ums_runner(tool: LocalTool) -> ToolRunner {
    ToolRunner::local_only().with_local(
        serde_json::json!({
            "type": "function",
            "function": { "name": opengrok_tools::USER_MACHINE_SHELL }
        }),
        tool,
    )
}

fn shell_deltas(id: &str, command: &str) -> Vec<ModelDelta> {
    vec![
        ModelDelta::ToolCallStart {
            id: id.to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: id.to_string(),
            delta: serde_json::json!({ "command": command }).to_string(),
        },
        ModelDelta::ToolCallEnd { id: id.to_string() },
    ]
}

/// A box `shell` whose answer to each command is looked up in `answers`, and which records
/// every command it was asked to run.
fn shell_runner(answers: &[(&str, &str)]) -> (ToolRunner, Arc<Mutex<Vec<String>>>) {
    let ran = Arc::new(Mutex::new(Vec::<String>::new()));
    let ran_tool = ran.clone();
    let answers: Vec<(String, String)> = answers
        .iter()
        .map(|(command, body)| (command.to_string(), body.to_string()))
        .collect();
    let tool: LocalTool = Arc::new(move |call| {
        let command = call.arguments["command"].as_str().unwrap_or("").to_string();
        ran_tool.lock().unwrap().push(command.clone());
        let body = answers
            .iter()
            .find(|(asked, _)| *asked == command)
            .map(|(_, body)| body.clone())
            .unwrap_or_else(|| "[exit code 0]".to_string());
        opengrok_tools::ToolResult::ok(&call.id, body)
    });
    let runner = ToolRunner::local_only().with_local(
        serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
        tool,
    );
    (runner, ran)
}

/// A door that plays one script per round, then answers `last` in words.
struct Rounds {
    scripts: Vec<Vec<ModelDelta>>,
    last: String,
    calls: Mutex<usize>,
}

impl Rounds {
    fn new(scripts: Vec<Vec<ModelDelta>>, last: &str) -> Self {
        Self {
            scripts,
            last: last.to_string(),
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl ModelDoor for Rounds {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let round = {
            let mut count = self.calls.lock().unwrap();
            *count += 1;
            *count
        };
        let script = self
            .scripts
            .get(round - 1)
            .cloned()
            .unwrap_or_else(|| vec![ModelDelta::Text(self.last.clone())]);
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn write_deltas(id: &str, path: &str, content: &str) -> Vec<ModelDelta> {
    vec![
        ModelDelta::ToolCallStart {
            id: id.to_string(),
            name: "write_file".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: id.to_string(),
            delta: serde_json::json!({ "path": path, "content": content }).to_string(),
        },
        ModelDelta::ToolCallEnd { id: id.to_string() },
    ]
}

/// A box with `write_file` and a `shell` whose `cargo test` fails until its `passes_on`-th run.
fn fix_then_test_runner(passes_on: usize) -> (ToolRunner, Arc<Mutex<Vec<String>>>) {
    let ran = Arc::new(Mutex::new(Vec::<String>::new()));
    let shell_ran = ran.clone();
    let write_ran = ran.clone();
    let runner = ToolRunner::local_only()
        .with_local(
            serde_json::json!({ "type": "function", "function": { "name": "shell" } }),
            Arc::new(move |call| {
                let command = call.arguments["command"].as_str().unwrap_or("").to_string();
                let runs = {
                    let mut ran = shell_ran.lock().unwrap();
                    ran.push(command.clone());
                    ran.iter().filter(|asked| **asked == command).count()
                };
                let body = match command.as_str() {
                    "cargo test" if runs < passes_on => {
                        "test parse_empty ... FAILED\ntest result: FAILED. 1 failed\n[exit code 101]"
                    }
                    "cargo test" => "test result: ok. 3 passed",
                    _ => "pub fn parse() {}",
                };
                opengrok_tools::ToolResult::ok(&call.id, body)
            }),
        )
        .with_local(
            serde_json::json!({ "type": "function", "function": { "name": "write_file" } }),
            Arc::new(move |call| {
                let path = call.arguments["path"].as_str().unwrap_or("").to_string();
                write_ran.lock().unwrap().push(format!("write {path}"));
                opengrok_tools::ToolResult::ok(&call.id, format!("wrote {path}"))
            }),
        );
    (runner, ran)
}

fn tool_results(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .filter_map(|event| event.extra.get("content").and_then(|c| c.as_str()))
        .map(str::to_string)
        .collect()
}

fn is_awaiting_card(event: &Event) -> bool {
    event.event_type == EventType::Custom
        && event.extra.get("name").and_then(|name| name.as_str()) == Some("run-awaiting-approval")
}

fn endings(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                EventType::RunFinished | EventType::RunError
            )
        })
        .collect()
}

/// Everything a live watcher was sent, in order.
struct Collect(Mutex<Vec<Event>>);
#[async_trait::async_trait]
impl EventSink for Collect {
    async fn emit(&self, events: &[Event]) {
        if let Ok(mut seen) = self.0.lock() {
            seen.extend(events.iter().cloned());
        }
    }
}

/// The `TOOL_CALL_RESULT` frames answering `id`, in order: a call that waited on a card has two.
fn results_for<'a>(events: &'a [Event], id: &str) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallResult)
        .filter(|event| event.extra.get("toolCallId").and_then(|v| v.as_str()) == Some(id))
        .collect()
}

/// `durationMs` as the wire carries it: an integer, or nothing.
fn duration_of(event: &Event) -> Option<u64> {
    event
        .extra
        .get(opengrok_wire::agui::DURATION_MS)
        .and_then(serde_json::Value::as_u64)
}

/// Every recipe `record_spent` recorded, in order. Only tests ask, so it lives beside them.
fn recipes_played(journal: &crate::journal::MemoryJournal) -> Vec<String> {
    journal
        .spent()
        .into_iter()
        .flat_map(|spent| spent.recipes)
        .collect()
}

#[path = "loop_tests/answers.rs"]
mod answers;
#[path = "loop_tests/bounds.rs"]
mod bounds;
#[path = "loop_tests/computer.rs"]
mod computer;
#[path = "loop_tests/journal.rs"]
mod journal;
#[path = "loop_tests/recipes.rs"]
mod recipes;
#[path = "loop_tests/runs.rs"]
mod runs;
#[path = "loop_tests/shell.rs"]
mod shell;
#[path = "loop_tests/stops.rs"]
mod stops;
#[path = "loop_tests/timing.rs"]
mod timing;
