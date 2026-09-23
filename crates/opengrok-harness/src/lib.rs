//! The agent loop: a run, from a client's request to the events it sees.
//!
//! Three pieces, kept apart because each fails differently:
//!   - `model`      — the door to a model, and the provider-neutral deltas that come back.
//!   - `projection` — deltas to a well-formed AG-UI run. Pure, and where the bracketing rules live.
//!   - `gateway` / `mock` — the two doors: open-ai-gateway, and a scripted one that spends nothing.
//!
//! The loop is deliberately thin. Everything hard is either in the projection (pure, tested
//! exhaustively) or in a door (isolated, swappable), which is what makes a run reproducible
//! without a provider.

pub mod cloaked_door;
pub mod gateway;
mod intent;
pub mod journal;
pub mod mock;
pub mod model;
pub mod projection;
pub mod review;
mod timing;
pub mod tools;

pub use gateway::GatewayDoor;
pub use journal::{JournalError, MemoryJournal, RunJournal};
pub use mock::MockDoor;
pub use model::{
    ChatMessage, DeltaStream, GatewayKey, ImagePart, ModelDelta, ModelDoor, ModelError,
    ModelRequest,
};
pub use projection::Projection;
pub use review::{JUDGE_MARKER, JUDGE_SYSTEM, ModelJudge, parse_verdict};
pub use timing::RUN_TIMING_NAME;
pub use tools::{LocalTool, ToolRunner, collect_tool_calls};

use futures::StreamExt;
use opengrok_wire::agui::Event;

/// Run one turn and collect every event a client should see.
///
/// Collecting rather than streaming, for now: a run's events are small, and having the whole
/// sequence in hand is what lets the caller write it to the event log before the client sees it —
/// which is how a run survives the client disconnecting. Streaming straight through would be
/// faster to the first token and is the obvious next step; it is not what makes a run durable.
pub async fn run_turn(
    door: &dyn ModelDoor,
    request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
) -> Vec<Event> {
    run_turn_with_tools(door, None, request, thread_id, run_id, at_ms).await
}

/// How many model calls one conversation may make.
///
/// A model that answers every tool result with another tool call would otherwise run until it ran
/// out of money. The bound is generous enough for real work and finite, and hitting it ends the
/// run as a *result* the client can see rather than a silent stop.
pub const MAX_ROUNDS: usize = 8;

/// How many rounds may be spent looking at and acting on the box's screen. A look is a model
/// call too, but a cheap and expected one: a task on a desktop is a dozen screenshots and clicks
/// before a sentence, so those rounds are counted apart from the words.
pub const MAX_COMPUTER_ROUNDS: usize = 24;

/// Identical screenshots in a row before the run stops: the model is waiting for something
/// that is not happening, and the honest thing is to say so rather than to keep looking.
pub const SAME_SCREEN_LIMIT: usize = 4;

/// Assistant text a tool-capable round may emit before a `ToolCallStart`.
///
/// Seen live (NativeChat Shot A): a model offered tools wrote a plan of the work as
/// `TEXT_MESSAGE` and never started a call. Words without a call are a reply; a flood of
/// them is the model stalling. The bound is a couple of short paragraphs — enough for
/// "I'll look that up" plus a real answer, not enough for a repeated plan of unused tools.
/// The same lesson is a sentence in `computer_system_prompt`; this is the stop that
/// prompt text alone did not provide.
///
/// Short intent *before* a tool (`I'll probe…`) is a different bug: NativeChat paints
/// every `TEXT_MESSAGE` as chat. Work-tool rounds drop that prose; a text-only round
/// still drops it when it is intent/status/diary, and only flushes leftover facts.
/// This bound still fires on the withheld bytes — a flood with no call is still a flood.
pub const PLAN_ONLY_TEXT_LIMIT: usize = 1500;

/// Tools NativeChat paints itself. Offered to the model; TOOL_CALL frames are
/// streamed; after one chart/form this HTTP run ends so the model cannot call
/// bar_chart again in the same request.
///
/// THESE ARE THE `chat_ui::attach` LOCALS (`bar_chart`, `form`, and the name
/// spellings a model actually emits). They are paint widgets, not work tools.
pub fn is_client_render_tool(name: &str) -> bool {
    matches!(
        name.trim()
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
            .as_str(),
        "bar-chart"
            | "barchart"
            | "show-bar-chart"
            | "render-bar-chart"
            | "form"
            | "show-form"
            | "render-form"
    )
}

/// Shot A is unused *work* tools (shell, `user_machine_shell`, computer). A
/// schema list that is only `bar_chart` / `form` is still chat: production
/// AG-UI always runs `chat_ui::attach`, which adds those two even when there
/// is no computer.
fn work_tools_offered(schemas: &[serde_json::Value]) -> bool {
    schemas.iter().any(|schema| {
        schema["function"]["name"]
            .as_str()
            .is_some_and(|name| !is_client_render_tool(name))
    })
}

fn shell_command(arguments: &serde_json::Value) -> &str {
    arguments
        .get("command")
        .or_else(|| arguments.get("cmd"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn looks_like_write(command: &str) -> bool {
    const MARKERS: &[&str] = &[
        "rm ",
        "rm\t",
        "mkdir",
        " mv ",
        "\tmv ",
        " cp ",
        "\tcp ",
        ">",
        "tee ",
        "chmod",
        "chown",
        "shutdown",
        "kill ",
        "profile.save",
        "profile.ensure",
        "profile.set",
        "nav.go",
        " unlink",
        "truncate",
        "bir-headless serve",
    ];
    MARKERS.iter().any(|marker| command.contains(marker))
}

fn looks_like_listing_or_show(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    // `gpui-agent hello` and `gpui-agent invoke --help` are probes. Counting either
    // as the one listing made the following `profile.search` a synthetic skip, and
    // the turn closed with no sentence (run 01a0c9ed).
    matches!(
        first,
        "ls" | "cat"
            | "head"
            | "tail"
            | "pwd"
            | "whoami"
            | "date"
            | "file"
            | "stat"
            | "echo"
            | "printf"
            | "type"
    ) || command.contains("profile.list")
        || command.contains("profile.search")
        || command.contains("dues.list")
        || command.contains("forms_set.get")
        || command.contains(".list")
}

/// `find ~` and `find /Users/<name>` exited 0 after about 90s on the demo machine.
/// That success cleared the work-fail streak and the turn kept going. A deeper
/// path is one directory and still runs.
fn is_broad_filesystem_walk(command: &str) -> bool {
    command
        .replace("&&", ";")
        .replace("||", ";")
        .split(['\n', ';', '|'])
        .any(segment_is_broad_find)
}

fn segment_is_broad_find(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len()
        && (tokens[index].contains('=')
            || tokens[index] == "export"
            || tokens[index] == "command"
            || tokens[index] == "sudo")
    {
        index += 1;
    }
    if tokens.get(index) != Some(&"find") {
        return false;
    }
    index += 1;
    while index < tokens.len() {
        let token = tokens[index].trim_matches(|ch| ch == '"' || ch == '\'');
        if token.starts_with('-') || token == "(" || token == "!" {
            break;
        }
        if is_broad_find_root(token) {
            return true;
        }
        index += 1;
    }
    false
}

fn is_broad_find_root(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    // Home itself, or one component under it (`$HOME/.config`). A deeper path
    // (`$HOME/Library/Application Support`) is a directory and still runs.
    // `starts_with("$HOME/")` used to refuse every one of those.
    if path == "~" || path == "$HOME" || path == "${HOME}" {
        return true;
    }
    if let Some(rest) = home_child(path) {
        return !rest.is_empty() && !rest.contains('/');
    }
    if path == "/Users" || path == "/Volumes" {
        return true;
    }
    if let Some(rest) = path.strip_prefix("/Users/") {
        return !rest.is_empty() && !rest.contains('/');
    }
    if let Some(rest) = path.strip_prefix("/Volumes/") {
        return !rest.is_empty() && !rest.contains('/');
    }
    false
}

fn home_child(path: &str) -> Option<&str> {
    path.strip_prefix("~/")
        .or_else(|| path.strip_prefix("$HOME/"))
        .or_else(|| path.strip_prefix("${HOME}/"))
}

fn is_broad_walk_call(call: &opengrok_tools::ToolCall) -> bool {
    matches!(
        call.name.as_str(),
        "shell" | opengrok_tools::USER_MACHINE_SHELL
    ) && is_broad_filesystem_walk(shell_command(&call.arguments))
}

fn refused_broad_walk(call: &opengrok_tools::ToolCall) -> opengrok_tools::ToolResult {
    opengrok_tools::ToolResult::refused(
        &call.id,
        "that search walks the whole home directory. The invoke names already in the skill are the catalog.",
    )
}

/// A read-only catalog read (`profile.list`, `profile.search`, `dues.list`).
/// A host probe such as `gpui-agent hello` is not one: it must not consume the
/// single listing this turn is allowed to run.
fn is_readonly_listing_shell(call: &opengrok_tools::ToolCall) -> bool {
    matches!(
        call.name.as_str(),
        "shell" | opengrok_tools::USER_MACHINE_SHELL
    ) && {
        let command = shell_command(&call.arguments).to_ascii_lowercase();
        let trimmed = command.trim();
        !trimmed.is_empty() && !looks_like_write(trimmed) && looks_like_listing_or_show(trimmed)
    }
}

/// Run a turn, and run any tools the model asked for. One round; see `run_conversation` for the
/// durable multi-round loop.
pub async fn run_turn_with_tools(
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

    let mut stream = match door.stream(request).await {
        Ok(stream) => stream,
        // A door that will not open is a failed run, not a crash: the client gets an ending it can
        // render and reason about (CLAUDE.md #8, fail closed and say why).
        Err(error) => {
            events.extend(projection.fail(error.to_string()));
            return events;
        }
    };

    while let Some(delta) = stream.next().await {
        match delta {
            Ok(delta) => events.extend(projection.push(delta)),
            Err(error) => {
                events.extend(projection.fail(error.to_string()));
                return events;
            }
        }
    }

    // Anything the model asked for, run on the coworker's own computer. The results are emitted as
    // AG-UI tool-result events so a person watching sees what happened, and so the log holds it.
    if let Some(runner) = tools {
        let calls = collect_tool_calls(&events);
        events.extend(box_wake_frame(runner, &mut projection, &calls).await);
        for result in runner.run_all(&calls).await {
            events.extend(projection.push_tool_result(&result));
        }
    }

    events.extend(projection.finish());
    events
}

/// The durable loop: model, tools, model again, until the model stops asking.
///
/// THE ORDERING IS THE POINT. Each round's events reach the journal *before* the next model call
/// is made, so a crash between rounds leaves a log that says exactly how far the run got. Reversing
/// those two lines would still pass every test about what a client sees and would quietly destroy
/// the property the whole project is built on.
///
/// The conversation grows as it goes: the model's own reply and the tool results are appended to
/// the messages, so the next call sees what happened rather than being asked the same question
/// again.
/// Somewhere to send a run's events AS THEY ARE PRODUCED, in addition to the `Vec` the run
/// returns at the end.
///
/// WHY A SINK AND NOT A `Stream`. Turning `converse` inside out into a stream is the tidier shape
/// and a far larger change: five callers take the `Vec`, and the durability rules read off the
/// completed round. A sink is additive — the `Vec` is unchanged, so a caller that does not pass
/// one cannot behave differently.
///
/// THE JOURNAL GUARANTEE IS UNAFFECTED, and this was the question that decided the shape. Events
/// are journaled once per ROUND, with the whole round, before the next model call
/// (`journal.record(run_id, &round_events)`); there is no per-event journaling anywhere. Handing
/// events to a sink earlier changes when bytes leave, not when the journal is written or when the
/// next call happens — so "journaled before the next model call" still holds.
///
/// A SINK MUST BE CHEAP AND MUST NOT FAIL THE RUN. It is awaited inside the delta loop, so slow
/// work here stalls reading the model's stream; throttle inside the implementation. It returns
/// nothing: a sink that cannot deliver has not made the run wrong, and the `Vec` still arrives.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    /// Events just produced, in order. Called many times per round, with only what is new.
    async fn emit(&self, events: &[Event]);
}

pub async fn run_conversation(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    journal: &dyn RunJournal,
    request: ModelRequest,
    thread_id: &str,
    run_id: &str,
    at_ms: i64,
) -> Vec<Event> {
    let projection = Projection::new(thread_id, run_id, at_ms);
    converse(
        door, tools, journal, request, projection, run_id, None, false,
    )
    .await
}

/// `run_conversation`, with a sink that sees each event as it is produced.
///
/// For the surfaces where a person is watching a bubble fill: seam A's `sendPrompt`, and
/// NativeChat's `POST /ag-ui`. Everything else keeps `run_conversation`.
#[allow(clippy::too_many_arguments)]
pub async fn run_conversation_streaming(
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
    converse(
        door,
        tools,
        journal,
        request,
        projection,
        run_id,
        Some(sink),
        false,
    )
    .await
}

/// Which run this is, and when. Three values that always travel together, so they travel as one.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub thread_id: String,
    pub run_id: String,
    pub at_ms: i64,
}

impl RunContext {
    pub fn new(thread_id: impl Into<String>, run_id: impl Into<String>, at_ms: i64) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            at_ms,
        }
    }
}

/// How the person answered. A refusal is ALSO an answer: the run carries on with a refusal
/// result the model can reason about (CLAUDE.md #8), rather than dying on the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    Approved,
    /// Refused, with the text the model reads — the rule that stopped it.
    Refused(String),
    /// The card was answered by something other than yes/no of a tool that should then run.
    /// The call is NOT re-executed; this text is the tool result. User-form submit fills
    /// outside `computer_use` and must not re-raise `request_user_form`.
    Settled(String),
}

/// What a resumed run already knows: the call that was answered, how, and where the first half
/// left off.
#[derive(Debug, Clone)]
pub struct Resumption {
    /// The call the person answered. On approval it is run directly, not re-proposed.
    pub approved: opengrok_tools::ToolCall,
    /// So the second half cannot collide with the first on a message id.
    pub message_seq: u32,
    pub outcome: ResumeOutcome,
}

impl Resumption {
    pub fn approved(call: opengrok_tools::ToolCall, message_seq: u32) -> Self {
        Self {
            approved: call,
            message_seq,
            outcome: ResumeOutcome::Approved,
        }
    }

    pub fn refused(
        call: opengrok_tools::ToolCall,
        message_seq: u32,
        why: impl Into<String>,
    ) -> Self {
        Self {
            approved: call,
            message_seq,
            outcome: ResumeOutcome::Refused(why.into()),
        }
    }

    pub fn settled(
        call: opengrok_tools::ToolCall,
        message_seq: u32,
        content: impl Into<String>,
    ) -> Self {
        Self {
            approved: call,
            message_seq,
            outcome: ResumeOutcome::Settled(content.into()),
        }
    }
}

/// Carry on a run that a person has just answered.
///
/// RUNS THE APPROVED CALL, RATHER THAN ASKING THE MODEL AGAIN. The person approved *that command*;
/// re-prompting could produce a different one, and running something nobody approved is the exact
/// failure the approval was for. So the call is executed directly, its result is appended, and only
/// then does the model get to carry on.
pub async fn resume_conversation(
    door: &dyn ModelDoor,
    tools: &ToolRunner,
    journal: &dyn RunJournal,
    mut request: ModelRequest,
    context: RunContext,
    resumption: Resumption,
) -> Vec<Event> {
    let Resumption {
        approved,
        message_seq,
        outcome,
    } = resumption;
    let run_id = context.run_id.clone();
    // Already started: a resumed run must not draw itself twice.
    let mut projection = Projection::resumed(
        &context.thread_id,
        &context.run_id,
        context.at_ms,
        message_seq,
    );
    let mut all = Vec::new();

    // A STOP PRESSED WHILE THE CARD WAS UP WINS OVER THE ANSWER. The answer and the Stop are two
    // appends to the same log, and the continuation is spawned after the answer lands — so a
    // Stop can arrive between the two, and without this check the approved call ran anyway on
    // a run the person had already stopped: the first `stopped` question in `converse_raw`
    // comes after it. Asked before anything touches the world, including for a refusal, so a
    // stopped run is not handed back to the model either (`formal/tla/RunLifecycle.tla`
    // NoApprovedAfterStop). It narrows the window, it does not close it: a Stop landing
    // after this question and before `run_all` still lets the call through.
    if journal.stopped(&run_id).await {
        return stop_here(
            journal,
            &mut projection,
            None,
            &run_id,
            Vec::new(),
            &mut None,
            &timing::TurnTiming::new(),
            timing::verbose_from_env(),
        )
        .await;
    }

    // A refusal never reaches the executor: the result is synthesised here and pushed exactly
    // like a real one, so the model learns which rule stopped it and carries on.
    let results = match outcome {
        ResumeOutcome::Approved => {
            // The person may have answered the card long after the box went to sleep.
            all.extend(
                box_wake_frame(tools, &mut projection, std::slice::from_ref(&approved)).await,
            );
            tools.run_all(std::slice::from_ref(&approved)).await
        }
        ResumeOutcome::Refused(why) => vec![opengrok_tools::ToolResult::refused(&approved.id, why)],
        ResumeOutcome::Settled(content) => {
            vec![opengrok_tools::ToolResult::ok(&approved.id, content)]
        }
    };
    for result in &results {
        all.extend(projection.push_tool_result(result));
        request.messages.push(tool_result_message(result));
    }
    let _ = record_round(journal, &run_id, &all).await;

    // If the approved call itself is still waiting, something is wrong with the approval rather
    // than with the run; stop rather than loop.
    if let Some(still_waiting) = results.iter().find(|result| result.awaiting_approval) {
        let reason = still_waiting
            .awaiting_reason
            .unwrap_or(opengrok_tools::AwaitingReason::ExecConsent);
        let parked = (&approved, reason, still_waiting.content.as_str());
        let mut waiting = park_awaiting(&mut projection, std::slice::from_ref(&parked));
        let _ = record_round(journal, &run_id, &waiting).await;
        all.append(&mut waiting);
        return all;
    }

    // The first half already recorded ToolCallStart (that is why this run is
    // resuming). converse_raw would otherwise start with started_a_tool = false
    // and treat a long post-HITL summary as a plan-only flood.
    let mut rest = converse(
        door,
        Some(tools),
        journal,
        request,
        projection,
        &run_id,
        None,
        true,
    )
    .await;
    all.append(&mut rest);
    all
}

/// End a run because a person stopped it, from wherever in the loop noticed.
///
/// ONE JOURNAL WRITE, CARRYING BOTH. `round_events` is whatever the turn had produced in the round
/// it was in the middle of — the model's words, the tool call it was about to make — and it has not
/// been recorded yet, because the loop records a whole round at a time. Writing it together with
/// the ending is what makes the transcript end at the moment the button was pressed instead of one
/// step before it.
#[allow(clippy::too_many_arguments)]
async fn stop_here(
    journal: &dyn RunJournal,
    projection: &mut Projection,
    sink: Option<&dyn EventSink>,
    run_id: &str,
    mut round_events: Vec<Event>,
    last_agent_shot: &mut Option<Event>,
    timing: &timing::TurnTiming,
    verbose_timing: bool,
) -> Vec<Event> {
    pin_last_agent_shot(
        sink,
        last_agent_shot,
        opengrok_tools::ImageVisibility::End,
        &mut round_events,
    )
    .await;
    let ending = projection.stopped();
    finish_round(
        journal,
        sink,
        run_id,
        projection,
        timing,
        verbose_timing,
        round_events,
        ending,
    )
    .await
}

async fn flush_withheld_text(
    projection: &mut Projection,
    sink: Option<&dyn EventSink>,
    withheld: &mut String,
    round_events: &mut Vec<Event>,
    last_failure: Option<&str>,
) {
    let Some(visible) = intent::visible_chat(&std::mem::take(withheld), last_failure) else {
        return;
    };
    let produced = projection.push(ModelDelta::Text(visible));
    emit_live(sink, &produced).await;
    round_events.extend(produced);
}

async fn emit_visible_text(
    projection: &mut Projection,
    sink: Option<&dyn EventSink>,
    round_events: &mut Vec<Event>,
    text: String,
) {
    if text.is_empty() {
        return;
    }
    let produced = projection.push(ModelDelta::Text(text));
    emit_live(sink, &produced).await;
    round_events.extend(produced);
}

fn round_has_assistant_text(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| event.event_type == opengrok_wire::agui::EventType::TextMessageContent)
}

/// Attach `run-timing`, show it live, journal the round+ending as one or two writes
/// depending on what the caller already recorded. Always one CUSTOM then the closer.
#[allow(clippy::too_many_arguments)]
async fn finish_round(
    journal: &dyn RunJournal,
    sink: Option<&dyn EventSink>,
    run_id: &str,
    projection: &Projection,
    timing: &timing::TurnTiming,
    verbose_timing: bool,
    mut round_events: Vec<Event>,
    mut ending: Vec<Event>,
) -> Vec<Event> {
    if !ending.is_empty() {
        timing::splice_before_run_end(&mut ending, timing.event(projection));
    }
    emit_live(sink, &ending).await;
    round_events.extend(ending);
    let _ = record_round(journal, run_id, &round_events).await;
    timing.log(run_id, verbose_timing);
    round_events
}

/// A clean finish — unless a Stop was recorded while the model was talking or a tool was running.
///
/// THE STOP CHECK POINTS ARE STEP BOUNDARIES, AND THE CLOSE IS ONE. The loop asks `stopped` at
/// the top of a round and before tools; a person who pressed Stop during the final answer, or
/// while the last tool ran, was otherwise told the run "finished". Every clean ending comes
/// through here so that rule lives in one place (`formal/tla/HarnessLoop.tla` StopIsHonoured
/// is the trace this closes). A failure keeps its own sentence and a
/// park keeps its card: only `finish` yields.
async fn finish_or_stop(
    journal: &dyn RunJournal,
    run_id: &str,
    projection: &mut Projection,
) -> Vec<Event> {
    if journal.stopped(run_id).await {
        projection.stopped()
    } else {
        projection.finish()
    }
}

/// When the round is already journaled, only the ending (with timing) is a new write.
async fn finish_ending(
    journal: &dyn RunJournal,
    sink: Option<&dyn EventSink>,
    run_id: &str,
    projection: &Projection,
    timing: &timing::TurnTiming,
    verbose_timing: bool,
    mut ending: Vec<Event>,
) -> Vec<Event> {
    if !ending.is_empty() {
        timing::splice_before_run_end(&mut ending, timing.event(projection));
    }
    emit_live(sink, &ending).await;
    let _ = record_round(journal, run_id, &ending).await;
    timing.log(run_id, verbose_timing);
    ending
}

/// Forward a batch to a live watcher. Empty batches are skipped so a no-op `finish` after
/// `fail` does not wake the sink.
/// The `box-waking` frame, when a call in this round is about to wake the coworker's box; empty
/// otherwise. The executor remembers a box it has seen running, so the frame comes once per turn.
async fn box_wake_frame(
    runner: &ToolRunner,
    projection: &mut Projection,
    calls: &[opengrok_tools::ToolCall],
) -> Vec<Event> {
    for call in calls {
        if runner.box_needs_wake(call).await {
            let coworker = runner.coworker_id().unwrap_or_default();
            return projection.box_waking(&coworker);
        }
    }
    Vec::new()
}

async fn emit_live(sink: Option<&dyn EventSink>, events: &[Event]) {
    if let Some(sink) = sink
        && !events.is_empty()
    {
        let clean: Vec<Event> = events.iter().cloned().map(scrub_event_secrets).collect();
        sink.emit(&clean).await;
    }
}

/// The loop both entry points share.
#[allow(clippy::too_many_arguments)]
/// What a run emitted, with streamed secret-bearing tool arguments assembled and scrubbed.
/// The raw loop is `converse_raw`; every exit of it goes through this one gate, so
/// `run.emitted` — which API clients read back — cannot carry a fragment the journal would
/// have refused.
async fn converse(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    journal: &dyn RunJournal,
    request: ModelRequest,
    projection: Projection,
    run_id: &str,
    sink: Option<&dyn EventSink>,
    already_started_a_tool: bool,
) -> Vec<Event> {
    scrub_streamed_tool_args(
        converse_raw(
            door,
            tools,
            journal,
            request,
            projection,
            run_id,
            sink,
            already_started_a_tool,
        )
        .await,
    )
}

#[allow(clippy::too_many_arguments)]
async fn converse_raw(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    journal: &dyn RunJournal,
    mut request: ModelRequest,
    mut projection: Projection,
    run_id: &str,
    sink: Option<&dyn EventSink>,
    already_started_a_tool: bool,
) -> Vec<Event> {
    let mut all = Vec::new();

    // Advertise the run's tools to the model — the offering half of tool use. Set once; every round
    // clones this request, so a bot that CAN run a shell command is now told it can, instead of
    // answering "I can't run commands" with a computer sitting right there.
    if let Some(runner) = tools {
        request.tools = runner.tool_schemas();
    }
    // Not `!request.tools.is_empty()`. Production AG-UI always runs
    // `chat_ui::attach` (opengrok-server `agui/chat_ui.rs`, offered from
    // `agui/routes.rs` on every turn, including coworkers with no computer),
    // which adds `bar_chart` and `form`. Those are paint widgets NativeChat
    // draws from TOOL_CALL frames. A long prose answer that never calls them
    // is normal chat, not Shot A — Shot A is unused shell / user_machine_shell
    // / computer. Count the plan-only flood only when a non-paint tool is on
    // the schema list.
    let tools_offered = work_tools_offered(&request.tools);

    let mut opening = projection.start();
    if let Err(error) = journal.record(run_id, &opening).await {
        // A run we cannot record must not proceed: it would produce work that a reconnect can
        // never reproduce, which is the failure this design exists to prevent.
        let mut failed = projection.fail(format!("the run could not be recorded: {error}"));
        emit_live(sink, &opening).await;
        emit_live(sink, &failed).await;
        all.append(&mut opening);
        all.append(&mut failed);
        return all;
    }
    emit_live(sink, &opening).await;
    all.append(&mut opening);

    // The calls the tools refused last round, by name and arguments. A model that asks
    // for exactly the same thing again is not going to get a different answer, and
    // burning the remaining rounds on it only delays telling the person.
    let mut last_refused: Option<Vec<(String, serde_json::Value)>> = None;
    // Across the whole run, not the round: a tool call then a long summary is work,
    // not a plan. Resetting this each round — or on resume, which is a new
    // converse_raw — would fail that summary as plan-only text.
    let mut started_a_tool = already_started_a_tool;
    let mut plan_only_chars = 0usize;
    // Rounds that ended in words or box tools, and rounds spent on the screen: two budgets.
    let mut spoken_rounds = 0usize;
    let mut computer_rounds = 0usize;
    // The last screenshot the model was shown, and how many times in a row it was the same.
    let mut last_screen: Option<u64> = None;
    let mut same_screen = 0usize;
    // Last computer-step TOOL_CALL_RESULT whose image is still `agent`. Promoted to `end`
    // or `failure` when the run closes so replay keeps one PNG, not every step.
    let mut last_agent_shot: Option<Event> = None;
    // Whether the model has produced anything at all this run — a word, a tool call, a thought.
    // A run that ends having produced nothing is a failure with a sentence, not a silent finish.
    let mut any_delta = false;
    let mut last_failure: Option<String> = None;
    let mut work_fail_streak: u32 = 0;
    let mut had_successful_listing = false;
    let mut skipped_redundant_listing = false;
    // The catalog sentence to show if a later round only repeats the listing.
    // Without it the early finish below closes the run with no TEXT_MESSAGE.
    let mut last_listing: Option<String> = None;
    // A shell that already selected a named target or opened a view-only editor,
    // keyed by invoke name so quote variants are one action. The sentence is
    // what the chat shows if that action is asked again or spends the last call.
    let mut opened: Option<(String, String)> = None;
    let mut timing = timing::TurnTiming::new();
    let verbose_timing = timing::verbose_from_env();

    for _round in 0..(MAX_ROUNDS + MAX_COMPUTER_ROUNDS) {
        let mut round_events = Vec::new();

        // WHERE A STOP LANDS, THE FIRST OF TWO PLACES. No further model call: whatever the loop was
        // going to ask next is not asked, and nothing more is spent on it.
        if journal.stopped(run_id).await {
            all.extend(
                stop_here(
                    journal,
                    &mut projection,
                    sink,
                    run_id,
                    round_events,
                    &mut last_agent_shot,
                    &timing,
                    verbose_timing,
                )
                .await,
            );
            return all;
        }

        keep_recent_images(&mut request.messages, RECENT_IMAGES);
        let model_started = std::time::Instant::now();
        let stream = match door.stream(request.clone()).await {
            Ok(stream) => Some(stream),
            Err(error) => {
                timing.record_model(timing::elapsed_ms(model_started));
                pin_last_agent_shot(
                    sink,
                    &mut last_agent_shot,
                    opengrok_tools::ImageVisibility::Failure,
                    &mut round_events,
                )
                .await;
                let failed = projection.fail(error.to_string());
                all.extend(
                    finish_round(
                        journal,
                        sink,
                        run_id,
                        &projection,
                        &timing,
                        verbose_timing,
                        round_events,
                        failed,
                    )
                    .await,
                );
                return all;
            }
        };

        let mut said = String::new();
        let mut withheld = String::new();
        let mut round_work_tool = false;
        if let Some(mut stream) = stream {
            while let Some(delta) = stream.next().await {
                match delta {
                    Ok(delta) => {
                        any_delta = true;
                        if let ModelDelta::Text(text) = &delta {
                            // F8: discarded intent must not land on `said`. The next
                            // hop would append it as an assistant message and re-bill
                            // the diary. Count plan-only from the bytes; keep them off
                            // the next request.
                            if !tools_offered {
                                said.push_str(text);
                            }
                            if tools_offered && !started_a_tool {
                                plan_only_chars =
                                    plan_only_chars.saturating_add(text.chars().count());
                            }
                            if tools_offered {
                                withheld.push_str(text);
                            }
                        }
                        if let ModelDelta::ToolCallStart { name, .. } = &delta {
                            started_a_tool = true;
                            if !is_client_render_tool(name) {
                                round_work_tool = true;
                            }
                        }
                        // Work-tool rounds withhold TEXT until ToolCallStart / finalization
                        // so NativeChat does not paint "I'll probe…" as chat. Reasoning and
                        // tool frames still stream. A text-only round flushes below.
                        let withhold_text = tools_offered && matches!(delta, ModelDelta::Text(_));
                        if !withhold_text {
                            let produced = projection.push(delta);
                            // AS THEY ARE PRODUCED. The `Vec` still collects everything; this only
                            // adds a second reader that does not have to wait for the run to end.
                            // Through `emit_live`, never `sink.emit` directly, so the live
                            // delta path meets `scrub_event_secrets` like every other path.
                            // Without it a model that smuggled a `values.password` into its
                            // own tool args reached NativeChat verbatim.
                            emit_live(sink, &produced).await;
                            round_events.extend(produced);
                        }
                        if tools_offered
                            && !started_a_tool
                            && plan_only_chars > PLAN_ONLY_TEXT_LIMIT
                        {
                            timing.record_model(timing::elapsed_ms(model_started));
                            let ending = projection.fail(format!(
                                "plan-only text: {plan_only_chars} characters with tools offered and no tool call started; stopping instead of waiting"
                            ));
                            pin_last_agent_shot(
                                sink,
                                &mut last_agent_shot,
                                opengrok_tools::ImageVisibility::Failure,
                                &mut round_events,
                            )
                            .await;
                            let _ = record_round(journal, run_id, &round_events).await;
                            all.append(&mut round_events);
                            all.extend(
                                finish_ending(
                                    journal,
                                    sink,
                                    run_id,
                                    &projection,
                                    &timing,
                                    verbose_timing,
                                    ending,
                                )
                                .await,
                            );
                            return all;
                        }
                    }
                    Err(error) => {
                        timing.record_model(timing::elapsed_ms(model_started));
                        if !round_work_tool {
                            flush_withheld_text(
                                &mut projection,
                                sink,
                                &mut withheld,
                                &mut round_events,
                                last_failure.as_deref(),
                            )
                            .await;
                        }
                        pin_last_agent_shot(
                            sink,
                            &mut last_agent_shot,
                            opengrok_tools::ImageVisibility::Failure,
                            &mut round_events,
                        )
                        .await;
                        let failed = projection.fail(error.to_string());
                        all.extend(
                            finish_round(
                                journal,
                                sink,
                                run_id,
                                &projection,
                                &timing,
                                verbose_timing,
                                round_events,
                                failed,
                            )
                            .await,
                        );
                        return all;
                    }
                }
            }
            timing.record_model(timing::elapsed_ms(model_started));
            if round_work_tool {
                withheld.clear();
            } else {
                flush_withheld_text(
                    &mut projection,
                    sink,
                    &mut withheld,
                    &mut round_events,
                    last_failure.as_deref(),
                )
                .await;
            }
            // Tools for this round, run on the coworker's own computer.
            let mut calls = collect_tool_calls(&round_events);
            // One chart/form per round. Extra bar_chart calls in the same
            // completion are what turned "generate another" into 2, then 4.
            if let Some(ui) = calls
                .iter()
                .rev()
                .find(|call| is_client_render_tool(&call.name))
                .cloned()
            {
                calls.retain(|call| !is_client_render_tool(&call.name));
                calls.push(ui);
            }
            if let (Some(runner), false) = (tools, calls.is_empty()) {
                // WHERE A STOP LANDS, THE SECOND AND MORE USEFUL PLACE. The model has just
                // asked to do something to the world — play the recipe again, type into the
                // search field again — and this is the last moment before it happens. Checking
                // only at the top of the round would let one more of them through, which is one
                // more than the person who pressed the button asked for.
                //
                // WHAT THIS DOES NOT DO, SAID PLAINLY: a call already in flight is not reached
                // from here. `run_all` is one await, a recipe playing on the box is a single
                // request inside it, and neither this loop nor the box's API on the pinned
                // revision can take it back. So a recipe that has started finishes, and the
                // stop takes hold before the next one.
                if journal.stopped(run_id).await {
                    all.extend(
                        stop_here(
                            journal,
                            &mut projection,
                            sink,
                            run_id,
                            round_events,
                            &mut last_agent_shot,
                            &timing,
                            verbose_timing,
                        )
                        .await,
                    );
                    return all;
                }
                let listing_only = calls.iter().any(is_readonly_listing_shell)
                    && calls.iter().all(|call| {
                        is_client_render_tool(&call.name) || is_readonly_listing_shell(call)
                    });
                if had_successful_listing && listing_only && skipped_redundant_listing {
                    if !round_has_assistant_text(&round_events)
                        && let Some(fact) = last_listing.clone()
                    {
                        emit_visible_text(&mut projection, sink, &mut round_events, fact).await;
                    }
                    pin_last_agent_shot(
                        sink,
                        &mut last_agent_shot,
                        opengrok_tools::ImageVisibility::End,
                        &mut round_events,
                    )
                    .await;
                    let ending = finish_or_stop(journal, run_id, &mut projection).await;
                    all.extend(
                        finish_round(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            round_events,
                            ending,
                        )
                        .await,
                    );
                    return all;
                }
                let same_open = opened.as_ref().is_some_and(|(previous, _)| {
                    !calls.is_empty()
                        && calls.iter().all(|call| {
                            matches!(
                                call.name.as_str(),
                                "shell" | opengrok_tools::USER_MACHINE_SHELL
                            ) && intent::shell_action_key(shell_command(&call.arguments))
                                == *previous
                        })
                });
                if same_open && let Some((_, sentence)) = opened.clone() {
                    if !round_has_assistant_text(&round_events) {
                        emit_visible_text(&mut projection, sink, &mut round_events, sentence).await;
                    }
                    pin_last_agent_shot(
                        sink,
                        &mut last_agent_shot,
                        opengrok_tools::ImageVisibility::End,
                        &mut round_events,
                    )
                    .await;
                    let ending = finish_or_stop(journal, run_id, &mut projection).await;
                    all.extend(
                        finish_round(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            round_events,
                            ending,
                        )
                        .await,
                    );
                    return all;
                }
                let waking = box_wake_frame(runner, &mut projection, &calls).await;
                emit_live(sink, &waking).await;
                round_events.extend(waking);
                let skip_listing = had_successful_listing && listing_only;
                let tool_started = std::time::Instant::now();
                let ((results, per_tool), auto_review_ms) = if skip_listing {
                    skipped_redundant_listing = true;
                    let results: Vec<_> = calls
                        .iter()
                        .map(|call| {
                            opengrok_tools::ToolResult::ok(
                                &call.id,
                                "A listing already succeeded this turn. Answer from that result; do not list again.",
                            )
                        })
                        .collect();
                    let times: Vec<_> = calls.iter().map(|call| (call.name.clone(), 0)).collect();
                    ((results, times), 0)
                } else if calls.iter().any(is_broad_walk_call) {
                    let runnable: Vec<_> = calls
                        .iter()
                        .filter(|call| !is_broad_walk_call(call))
                        .cloned()
                        .collect();
                    let (ran, ran_times, review_ms) = if runnable.is_empty() {
                        (Vec::new(), Vec::new(), 0)
                    } else {
                        let ((ran, times), ms) =
                            review::time_auto_review(runner.run_all_timed(&runnable)).await;
                        (ran, times, ms)
                    };
                    let mut ran = ran.into_iter();
                    let mut ran_times = ran_times.into_iter();
                    let results = calls
                        .iter()
                        .map(|call| {
                            if is_broad_walk_call(call) {
                                refused_broad_walk(call)
                            } else {
                                ran.next().unwrap_or_else(|| {
                                    opengrok_tools::ToolResult::refused(
                                        &call.id,
                                        "the tool did not run",
                                    )
                                })
                            }
                        })
                        .collect();
                    let times = calls
                        .iter()
                        .map(|call| {
                            if is_broad_walk_call(call) {
                                (call.name.clone(), 0)
                            } else {
                                ran_times.next().unwrap_or_else(|| (call.name.clone(), 0))
                            }
                        })
                        .collect();
                    ((results, times), review_ms)
                } else {
                    review::time_auto_review(runner.run_all_timed(&calls)).await
                };
                timing.record_tools(
                    per_tool
                        .into_iter()
                        .map(|(name, ms)| timing::ToolPhase { name, ms })
                        .collect(),
                    timing::elapsed_ms(tool_started),
                    auto_review_ms,
                );

                for (result, call) in results.iter().zip(calls.iter()) {
                    let produced = projection.push_tool_result(result);
                    emit_live(sink, &produced).await;
                    remember_agent_shot(&produced, &mut last_agent_shot);
                    round_events.extend(produced);
                    let mut message = tool_result_message(result);
                    let shell_failed = intent::counts_as_work_failure(result.ok, &result.content);
                    if result.ok
                        && !shell_failed
                        && is_readonly_listing_shell(call)
                        && !skip_listing
                    {
                        had_successful_listing = true;
                        last_listing = Some(intent::short_failure_fact(&result.content));
                        message.content.push_str("\n\n");
                        message.content.push_str(intent::READONLY_SHELL_NUDGE);
                    } else if shell_failed && !result.awaiting_approval {
                        last_failure = Some(intent::short_failure_fact(&result.content));
                        if work_fail_streak == 0
                            && !intent::is_unrecoverable_command_miss(&result.content)
                            && !is_broad_walk_call(call)
                        {
                            message.content.push_str("\n\n");
                            message.content.push_str(intent::FAILED_TOOL_NUDGE);
                        }
                    }
                    if matches!(
                        call.name.as_str(),
                        "shell" | opengrok_tools::USER_MACHINE_SHELL
                    ) {
                        let opened_now = intent::opened_target_sentence(&result.content)
                            .map(|sentence| (sentence, intent::OPENED_TARGET_NUDGE))
                            .or_else(|| {
                                intent::opened_editor_sentence(&result.content)
                                    .map(|sentence| (sentence, intent::OPENED_EDITOR_NUDGE))
                            });
                        if let Some((sentence, nudge)) = opened_now {
                            let key = intent::shell_action_key(shell_command(&call.arguments));
                            if !key.is_empty() {
                                opened = Some((key, sentence));
                                message.content.push_str("\n\n");
                                message.content.push_str(nudge);
                            }
                        }
                    }
                    request.messages.push(message);
                }
                let work_failed = results.iter().zip(calls.iter()).any(|(result, call)| {
                    !is_client_render_tool(&call.name)
                        && !result.awaiting_approval
                        && intent::counts_as_work_failure(result.ok, &result.content)
                });
                let work_ok = results.iter().zip(calls.iter()).any(|(result, call)| {
                    !is_client_render_tool(&call.name)
                        && result.ok
                        && !intent::counts_as_work_failure(true, &result.content)
                });
                if work_failed {
                    // A missing binary is not fixed by rewording the same command. Stop
                    // this round. A home-directory find is refused once so the model can
                    // call the catalog; a second find reaches the streak ceiling below.
                    // A rejected `--arg` (missing year, a JSON blob as a positional) is
                    // the host's sentence for the model to correct. It must not spend
                    // the one retry, or the second mistake becomes the chat bubble.
                    let argv_mistake = results.iter().all(|result| {
                        !intent::counts_as_work_failure(result.ok, &result.content)
                            || intent::is_invoke_argv_mistake(&result.content)
                    });
                    if results
                        .iter()
                        .any(|result| intent::is_unrecoverable_command_miss(&result.content))
                    {
                        work_fail_streak = intent::MAX_FAILED_WORK_ROUNDS;
                    } else if !argv_mistake {
                        work_fail_streak = work_fail_streak.saturating_add(1);
                    }
                } else if work_ok && !skip_listing {
                    // A synthetic "already listed" ok is not a catalog read. Clearing
                    // last_failure here is how a later skip finished with a blank chat.
                    work_fail_streak = 0;
                    last_failure = None;
                }

                let waiting: Vec<(
                    &opengrok_tools::ToolCall,
                    opengrok_tools::AwaitingReason,
                    &str,
                )> = results
                    .iter()
                    .zip(calls.iter())
                    .filter(|(result, _)| result.awaiting_approval)
                    .map(|(result, call)| {
                        (
                            call,
                            result
                                .awaiting_reason
                                .unwrap_or(opengrok_tools::AwaitingReason::ExecConsent),
                            result.content.as_str(),
                        )
                    })
                    .collect();
                if !waiting.is_empty() {
                    // One CUSTOM per awaiting call in this completion — NativeChat paints a
                    // Website login card per TOOL_CALL. Live SSE must not forward those
                    // TOOL_CALLs until the matching CUSTOM has a gateway `e_*` (AgUiSink
                    // holds them). Parking on the first leftover left stacked cards with
                    // only a raw `call-*` id.
                    //
                    // Then `RUN_FINISHED`: AG-UI/NativeChat hold Waiting on that closer.
                    // The HTTP stream used to drop after CUSTOM with no ending, which is a
                    // forever spinner. The aggregate stays `awaiting-approval` (the journal
                    // does not Finish a suspended run) so Continue can still resume. A new
                    // user message interrupts instead of leaving a zombie parked run.
                    let waiting_events = park_awaiting(&mut projection, &waiting);
                    let _ = record_round(journal, run_id, &round_events).await;
                    all.append(&mut round_events);
                    all.extend(
                        finish_ending(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            waiting_events,
                        )
                        .await,
                    );
                    return all;
                }

                let refused: Vec<(String, serde_json::Value)> = calls
                    .iter()
                    .zip(&results)
                    .filter(|(_, result)| !result.ok)
                    // A person's no on their own machine was about the machine, not one
                    // spelling of the command: a reworded retry is the same ask.
                    .map(|(call, _)| {
                        let arguments = if call.name == opengrok_tools::USER_MACHINE_SHELL {
                            serde_json::Value::Null
                        } else {
                            call.arguments.clone()
                        };
                        (call.name.clone(), arguments)
                    })
                    .collect();
                let every_call_refused = !results.is_empty() && refused.len() == results.len();
                // A home-directory find is refused before it runs. The second one
                // stops through the work-fail streak, which emits one sentence.
                // The identical-refusal closer would fail the run with no chat line.
                let broad_walk = calls.iter().any(is_broad_walk_call);
                if every_call_refused && !broad_walk && last_refused.as_ref() == Some(&refused) {
                    let names: Vec<&str> = refused.iter().map(|(name, _)| name.as_str()).collect();
                    let why = results
                        .first()
                        .map(|result| result.content.as_str())
                        .unwrap_or("refused");
                    let ending = projection.fail(format!(
                        "`{}` was refused the same way twice ({why}); stopping instead of retrying",
                        names.join("`, `")
                    ));
                    pin_last_agent_shot(
                        sink,
                        &mut last_agent_shot,
                        opengrok_tools::ImageVisibility::Failure,
                        &mut round_events,
                    )
                    .await;
                    let _ = record_round(journal, run_id, &round_events).await;
                    all.append(&mut round_events);
                    all.extend(
                        finish_ending(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            ending,
                        )
                        .await,
                    );
                    return all;
                }
                last_refused = every_call_refused.then_some(refused);

                // Screens: the same picture four times running means waiting, not working.
                for result in results.iter().filter(|result| result.ok) {
                    if let Some(image) = &result.image {
                        let hash = screen_hash(&image.base64);
                        if last_screen == Some(hash) {
                            same_screen += 1;
                        } else {
                            last_screen = Some(hash);
                            same_screen = 0;
                        }
                    }
                }
                if same_screen + 1 >= SAME_SCREEN_LIMIT {
                    let ending = projection.fail(format!(
                            "the screen has not changed after {SAME_SCREEN_LIMIT} looks; stopping instead of waiting"
                        ));
                    pin_last_agent_shot(
                        sink,
                        &mut last_agent_shot,
                        opengrok_tools::ImageVisibility::Failure,
                        &mut round_events,
                    )
                    .await;
                    let _ = record_round(journal, run_id, &round_events).await;
                    all.append(&mut round_events);
                    all.extend(
                        finish_ending(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            ending,
                        )
                        .await,
                    );
                    return all;
                }

                // Work-tool preamble was withheld from chat AND from `said` (F8).
                // Replaying it as an assistant message is how rounds got slower.
                if !said.is_empty() && !round_work_tool {
                    request.messages.push(ChatMessage {
                        images: Vec::new(),
                        role: "assistant".to_string(),
                        content: said,
                    });
                }

                if work_fail_streak >= intent::MAX_FAILED_WORK_ROUNDS {
                    if !round_has_assistant_text(&round_events)
                        && let Some(fact) = last_failure.clone()
                    {
                        emit_visible_text(&mut projection, sink, &mut round_events, fact).await;
                    }
                    pin_last_agent_shot(
                        sink,
                        &mut last_agent_shot,
                        opengrok_tools::ImageVisibility::End,
                        &mut round_events,
                    )
                    .await;
                    let ending = finish_or_stop(journal, run_id, &mut projection).await;
                    all.extend(
                        finish_round(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            round_events,
                            ending,
                        )
                        .await,
                    );
                    return all;
                }

                // DURABLE BEFORE THE NEXT CALL. Recorded here, at the top of the next round's
                // dependency chain, so a crash after this point can be picked up.
                if let Err(error) = record_round(journal, run_id, &round_events).await {
                    let failed = projection.fail(format!("the run could not be recorded: {error}"));
                    all.extend(
                        finish_round(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            round_events,
                            failed,
                        )
                        .await,
                    );
                    return all;
                }
                all.append(&mut round_events);

                // bar_chart/form already painted from TOOL_CALL frames. Another model
                // round in this HTTP request is what doubled charts on "generate another".
                if calls.iter().any(|call| is_client_render_tool(&call.name)) {
                    let mut pin_events = Vec::new();
                    pin_last_agent_shot(
                        sink,
                        &mut last_agent_shot,
                        opengrok_tools::ImageVisibility::End,
                        &mut pin_events,
                    )
                    .await;
                    let _ = record_round(journal, run_id, &pin_events).await;
                    all.append(&mut pin_events);
                    let ending = finish_or_stop(journal, run_id, &mut projection).await;
                    all.extend(
                        finish_ending(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            ending,
                        )
                        .await,
                    );
                    return all;
                }

                // Which budget this round drew on: every call a successful screen action, or anything
                // else. The screen budget is wider because looking is the work there.
                let on_screen = calls
                    .iter()
                    .zip(&results)
                    .all(|(call, result)| call.name == "computer" && result.ok);
                if on_screen {
                    computer_rounds += 1;
                } else {
                    spoken_rounds += 1;
                }
                let over = if spoken_rounds >= MAX_ROUNDS {
                    Some(format!(
                        "this run reached its limit of {MAX_ROUNDS} model calls"
                    ))
                } else if computer_rounds >= MAX_COMPUTER_ROUNDS {
                    Some(format!(
                        "this run reached its limit of {MAX_COMPUTER_ROUNDS} looks and actions on its computer"
                    ))
                } else {
                    None
                };
                if let Some(why) = over {
                    if let Some((_, sentence)) = opened.clone() {
                        let mut pin_events = Vec::new();
                        if !round_has_assistant_text(&round_events) {
                            emit_visible_text(&mut projection, sink, &mut pin_events, sentence)
                                .await;
                        }
                        pin_last_agent_shot(
                            sink,
                            &mut last_agent_shot,
                            opengrok_tools::ImageVisibility::End,
                            &mut pin_events,
                        )
                        .await;
                        let _ = record_round(journal, run_id, &pin_events).await;
                        all.append(&mut pin_events);
                        let ending = finish_or_stop(journal, run_id, &mut projection).await;
                        all.extend(
                            finish_ending(
                                journal,
                                sink,
                                run_id,
                                &projection,
                                &timing,
                                verbose_timing,
                                ending,
                            )
                            .await,
                        );
                        return all;
                    }
                    let mut pin_events = Vec::new();
                    pin_last_agent_shot(
                        sink,
                        &mut last_agent_shot,
                        opengrok_tools::ImageVisibility::Failure,
                        &mut pin_events,
                    )
                    .await;
                    let _ = record_round(journal, run_id, &pin_events).await;
                    all.append(&mut pin_events);
                    let ending = projection.fail(why);
                    all.extend(
                        finish_ending(
                            journal,
                            sink,
                            run_id,
                            &projection,
                            &timing,
                            verbose_timing,
                            ending,
                        )
                        .await,
                    );
                    return all;
                }
                continue;
            }
        }

        // No tools were asked for, or the run failed: this is the last round either way.
        //
        // A run that produced nothing at all ends as a failure that says so. Finishing cleanly
        // with an empty transcript is the dangerous empty success (CLAUDE.md, three facts №3):
        // the client cannot tell it from a coworker with nothing to say, and every one of them
        // invented its own placeholder. A run that already failed keeps its own message — `fail`
        // and `finish` are both no-ops once the run has ended.
        let pin = if any_delta {
            opengrok_tools::ImageVisibility::End
        } else {
            opengrok_tools::ImageVisibility::Failure
        };
        pin_last_agent_shot(sink, &mut last_agent_shot, pin, &mut round_events).await;
        let ending = if any_delta {
            finish_or_stop(journal, run_id, &mut projection).await
        } else {
            projection.fail("the model returned no text")
        };
        all.extend(
            finish_round(
                journal,
                sink,
                run_id,
                &projection,
                &timing,
                verbose_timing,
                round_events,
                ending,
            )
            .await,
        );
        return all;
    }

    // UNREACHABLE TODAY, AND KEPT AN ENDING ANYWAY. Every `continue` spends one unit of one of
    // the two budgets and the run ends when either is spent, so at most
    // MAX_ROUNDS + MAX_COMPUTER_ROUNDS - 2 rounds continue — proved for every budget in
    // `formal/lean/Harness.lean` (Budget.never_falls_out). This line used to return `all`
    // with no terminal event: a `continue` added later without spending a budget would have
    // left the client's spinner up forever. The `for` stays as the spend backstop; leaving it
    // is a failure that says so.
    let ending = projection.fail(format!(
        "this run reached its loop bound of {} rounds",
        MAX_ROUNDS + MAX_COMPUTER_ROUNDS
    ));
    all.extend(
        finish_round(
            journal,
            sink,
            run_id,
            &projection,
            &timing,
            verbose_timing,
            Vec::new(),
            ending,
        )
        .await,
    );
    all
}

/// Computer-step shots are `agent`: live SSE may carry the PNG for the Computer pane, but the
/// journal drops the bytes so a reconnecting client is not flooded with every click. Missing
/// `visibility` is a legacy frame — those PNGs were already first-class events and stay.
/// Accidental `password` keys are dropped here too — site logins must not persist.
fn strip_agent_png(mut event: Event) -> Event {
    if event.event_type != opengrok_wire::agui::EventType::ToolCallResult {
        return event;
    }
    let Some(image) = event.extra.get_mut("image") else {
        return event;
    };
    let vis = image
        .get("visibility")
        .and_then(|value| value.as_str())
        .unwrap_or("transcript");
    if vis == "agent"
        && let Some(object) = image.as_object_mut()
    {
        object.remove("base64");
    }
    event
}

fn scrub_event_secrets(mut event: Event) -> Event {
    let extra = serde_json::Value::Object(event.extra.clone());
    if let serde_json::Value::Object(map) = opengrok_tools::credential::scrub_secret_keys(&extra) {
        event.extra = map;
    }
    event
}

/// Streamed tool-call arguments arrive as `TOOL_CALL_ARGS` fragments — `"password":"s3`
/// and then `cret"` — and `scrub_secret_keys` can only scrub a value it can parse, so a
/// real streaming model walked a smuggled secret straight past the per-event scrub. For
/// the tools whose arguments may carry one, gather each call's fragments, scrub the
/// whole, and put back ONE fragment holding the scrubbed JSON where the first one was.
/// Fragments that never assemble into JSON become `{}`: text that cannot be scrubbed
/// does not get to leave as it is.
///
/// The wire shape is kept — one `TOOL_CALL_ARGS` per call rather than none — because
/// NativeChat builds its live login card by concatenating these very fragments.
pub fn scrub_streamed_tool_args(events: Vec<Event>) -> Vec<Event> {
    use opengrok_wire::agui::EventType;
    use std::collections::{HashMap, HashSet};

    let sensitive: HashSet<String> = events
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .filter(|event| {
            matches!(
                event
                    .extra
                    .get("toolCallName")
                    .and_then(serde_json::Value::as_str),
                Some(opengrok_tools::REQUEST_USER_FORM)
            )
        })
        .filter_map(|event| {
            event
                .extra
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    if sensitive.is_empty() {
        return events;
    }
    let mut joined: HashMap<String, String> = HashMap::new();
    for event in &events {
        if event.event_type == EventType::ToolCallArgs
            && let Some(id) = event
                .extra
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
            && sensitive.contains(id)
            && let Some(delta) = event.extra.get("delta").and_then(serde_json::Value::as_str)
        {
            joined.entry(id.to_string()).or_default().push_str(delta);
        }
    }
    let scrubbed: HashMap<String, String> = joined
        .into_iter()
        .map(|(id, text)| {
            let clean = serde_json::from_str::<serde_json::Value>(text.trim())
                .ok()
                .map(|value| opengrok_tools::credential::scrub_secret_keys(&value))
                .and_then(|value| serde_json::to_string(&value).ok())
                .unwrap_or_else(|| "{}".to_string());
            (id, clean)
        })
        .collect();
    let mut placed: HashSet<String> = HashSet::new();
    events
        .into_iter()
        .filter_map(|mut event| {
            if event.event_type != EventType::ToolCallArgs {
                return Some(event);
            }
            let Some(id) = event
                .extra
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                return Some(event);
            };
            let Some(clean) = scrubbed.get(&id) else {
                return Some(event);
            };
            if !placed.insert(id) {
                return None;
            }
            event.extra.insert(
                "delta".to_string(),
                serde_json::Value::String(clean.clone()),
            );
            Some(event)
        })
        .collect()
}

fn for_journal(events: &[Event]) -> Vec<Event> {
    scrub_streamed_tool_args(events.to_vec())
        .into_iter()
        .map(strip_agent_png)
        .map(scrub_event_secrets)
        .collect()
}

async fn record_round(
    journal: &dyn RunJournal,
    run_id: &str,
    events: &[Event],
) -> Result<(), JournalError> {
    journal.record(run_id, &for_journal(events)).await
}

fn is_agent_shot(event: &Event) -> bool {
    event.event_type == opengrok_wire::agui::EventType::ToolCallResult
        && event.extra.get("image").is_some()
        && event
            .extra
            .get("image")
            .and_then(|image| image.get("visibility"))
            .and_then(|value| value.as_str())
            == Some("agent")
}

fn remember_agent_shot(produced: &[Event], slot: &mut Option<Event>) {
    if let Some(event) = produced.iter().rev().find(|event| is_agent_shot(event)) {
        *slot = Some(event.clone());
    }
}

fn set_image_visibility(event: &mut Event, visibility: opengrok_tools::ImageVisibility) {
    if let Some(image) = event.extra.get_mut("image")
        && let Some(object) = image.as_object_mut()
    {
        object.insert(
            "visibility".to_string(),
            serde_json::Value::String(visibility.as_str().to_string()),
        );
    }
}

fn promote_agent_in(events: &mut [Event], visibility: opengrok_tools::ImageVisibility) -> bool {
    for event in events.iter_mut().rev() {
        if is_agent_shot(event) {
            set_image_visibility(event, visibility);
            return true;
        }
    }
    false
}

/// Keep one PNG when the run closes. Prefer promoting a shot still in this unjournaled
/// batch (journal then stores the bytes). Otherwise append the remembered last step shot
/// with `end` / `failure` so replay is not a blank Computer pane.
async fn pin_last_agent_shot(
    sink: Option<&dyn EventSink>,
    last_agent_shot: &mut Option<Event>,
    visibility: opengrok_tools::ImageVisibility,
    into: &mut Vec<Event>,
) {
    if promote_agent_in(into, visibility) {
        last_agent_shot.take();
        return;
    }
    let Some(mut pin) = last_agent_shot.take() else {
        return;
    };
    set_image_visibility(&mut pin, visibility);
    emit_live(sink, std::slice::from_ref(&pin)).await;
    into.push(pin);
}

/// How many screenshots a request carries. They are the model's eyes and also by far the widest
/// thing in it; the latest one or two say where the screen is now, older ones only cost.
const RECENT_IMAGES: usize = 2;

/// A screenshot's identity for the same-screen guard: the encoded bytes, hashed.
fn screen_hash(base64: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    base64.hash(&mut hasher);
    hasher.finish()
}

/// What the model is told a tool said, in its own transcript — with the picture, when there is one.
fn tool_result_message(result: &opengrok_tools::ToolResult) -> ChatMessage {
    ChatMessage {
        role: "user".to_string(),
        content: format!(
            "[tool {} result] {}",
            result.call_id,
            intent::annotate_empty_result(&result.content)
        ),
        images: result
            .image
            .iter()
            .map(|image| ImagePart {
                mime: image.mime.clone(),
                base64: image.base64.clone(),
            })
            .collect(),
    }
}

/// Keep the last `keep` messages that carry images; older ones keep their words, lose their pictures.
fn keep_recent_images(messages: &mut [ChatMessage], keep: usize) {
    let mut seen = 0;
    for message in messages.iter_mut().rev() {
        if message.images.is_empty() {
            continue;
        }
        seen += 1;
        if seen > keep {
            message.images.clear();
        }
    }
}

/// CUSTOM per waiting call, then `RUN_FINISHED` so the HTTP/SSE turn can close.
///
/// NativeChat keys Waiting chrome off `RUN_FINISHED`. The run aggregate still
/// stays `awaiting-approval` because the journal does not Finish a suspended run.
fn park_awaiting(
    projection: &mut Projection,
    waiting: &[(
        &opengrok_tools::ToolCall,
        opengrok_tools::AwaitingReason,
        &str,
    )],
) -> Vec<Event> {
    let mut events = Vec::new();
    for (call, reason, why) in waiting {
        events.extend(projection.awaiting_approval(call, *reason, awaiting_why(why)));
    }
    events.extend(projection.finish());
    events
}

/// The gate's sentence out of an awaiting result. `ToolResult::awaiting` writes
/// "waiting for approval: <why>"; the card wants only the why.
fn awaiting_why(content: &str) -> Option<&str> {
    content
        .strip_prefix("waiting for approval: ")
        .map(str::trim)
        .filter(|why| !why.is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/loop_tests.rs"]
mod tests;
