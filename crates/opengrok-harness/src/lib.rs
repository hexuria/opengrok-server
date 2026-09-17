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

pub mod gateway;
pub mod journal;
pub mod mock;
pub mod model;
pub mod projection;
pub mod review;
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

/// Tools NativeChat paints itself. Offered to the model; TOOL_CALL frames are
/// streamed; after one chart/form this HTTP run ends so the model cannot call
/// bar_chart again in the same request.
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
        for result in runner.run_all(&collect_tool_calls(&events)).await {
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
    converse(door, tools, journal, request, projection, run_id, None).await
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

    // A refusal never reaches the executor: the result is synthesised here and pushed exactly
    // like a real one, so the model learns which rule stopped it and carries on.
    let results = match outcome {
        ResumeOutcome::Approved => tools.run_all(std::slice::from_ref(&approved)).await,
        ResumeOutcome::Refused(why) => vec![opengrok_tools::ToolResult::refused(&approved.id, why)],
    };
    for result in &results {
        all.extend(projection.push_tool_result(result));
        request.messages.push(tool_result_message(result));
    }
    let _ = journal.record(&run_id, &all).await;

    // If the approved call itself is still waiting, something is wrong with the approval rather
    // than with the run; stop rather than loop.
    if let Some(still_waiting) = results.iter().find(|result| result.awaiting_approval) {
        let reason = still_waiting
            .awaiting_reason
            .unwrap_or(opengrok_tools::AwaitingReason::ExecConsent);
        let mut waiting =
            projection.awaiting_approval(&approved, reason, awaiting_why(&still_waiting.content));
        let _ = journal.record(&run_id, &waiting).await;
        all.append(&mut waiting);
        return all;
    }

    let mut rest = converse(
        door,
        Some(tools),
        journal,
        request,
        projection,
        &run_id,
        None,
    )
    .await;
    all.append(&mut rest);
    all
}

/// Forward a batch to a live watcher. Empty batches are skipped so a no-op `finish` after
/// `fail` does not wake the sink.
async fn emit_live(sink: Option<&dyn EventSink>, events: &[Event]) {
    if let Some(sink) = sink
        && !events.is_empty()
    {
        sink.emit(events).await;
    }
}

/// The loop both entry points share.
#[allow(clippy::too_many_arguments)]
async fn converse(
    door: &dyn ModelDoor,
    tools: Option<&ToolRunner>,
    journal: &dyn RunJournal,
    mut request: ModelRequest,
    mut projection: Projection,
    run_id: &str,
    sink: Option<&dyn EventSink>,
) -> Vec<Event> {
    let mut all = Vec::new();

    // Advertise the run's tools to the model — the offering half of tool use. Set once; every round
    // clones this request, so a bot that CAN run a shell command is now told it can, instead of
    // answering "I can't run commands" with a computer sitting right there.
    if let Some(runner) = tools {
        request.tools = runner.tool_schemas();
    }

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
    // Rounds that ended in words or box tools, and rounds spent on the screen: two budgets.
    let mut spoken_rounds = 0usize;
    let mut computer_rounds = 0usize;
    // The last screenshot the model was shown, and how many times in a row it was the same.
    let mut last_screen: Option<u64> = None;
    let mut same_screen = 0usize;
    // Whether the model has produced anything at all this run — a word, a tool call, a thought.
    // A run that ends having produced nothing is a failure with a sentence, not a silent finish.
    let mut any_delta = false;

    for _round in 0..(MAX_ROUNDS + MAX_COMPUTER_ROUNDS) {
        let mut round_events = Vec::new();

        keep_recent_images(&mut request.messages, RECENT_IMAGES);
        let stream = match door.stream(request.clone()).await {
            Ok(stream) => Some(stream),
            Err(error) => {
                let failed = projection.fail(error.to_string());
                emit_live(sink, &failed).await;
                round_events.extend(failed);
                None
            }
        };

        let mut said = String::new();
        if let Some(mut stream) = stream {
            let mut broke = false;
            while let Some(delta) = stream.next().await {
                match delta {
                    Ok(delta) => {
                        any_delta = true;
                        if let ModelDelta::Text(text) = &delta {
                            said.push_str(text);
                        }
                        let produced = projection.push(delta);
                        // AS THEY ARE PRODUCED. The `Vec` still collects everything; this only
                        // adds a second reader that does not have to wait for the run to end.
                        if let Some(sink) = sink
                            && !produced.is_empty()
                        {
                            sink.emit(&produced).await;
                        }
                        round_events.extend(produced);
                    }
                    Err(error) => {
                        let failed = projection.fail(error.to_string());
                        emit_live(sink, &failed).await;
                        round_events.extend(failed);
                        broke = true;
                        break;
                    }
                }
            }
            if !broke {
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
                    let results = runner.run_all(&calls).await;

                    for result in &results {
                        let produced = projection.push_tool_result(result);
                        emit_live(sink, &produced).await;
                        round_events.extend(produced);
                        // The model needs to see what its tool said, in its own transcript.
                        request.messages.push(tool_result_message(result));
                    }

                    if let Some((waiting, reason, why)) = results
                        .iter()
                        .position(|result| result.awaiting_approval)
                        .and_then(|index| {
                            let reason = results[index]
                                .awaiting_reason
                                .unwrap_or(opengrok_tools::AwaitingReason::ExecConsent);
                            calls
                                .get(index)
                                .map(|call| (call, reason, results[index].content.clone()))
                        })
                    {
                        // Ended as a readable state, not a silent stop and not a failure. The run
                        // stays `running` in the log, which is exactly what `interrupted_runs`
                        // looks for — resumption and approval share the same machinery.
                        let mut waiting_events =
                            projection.awaiting_approval(waiting, reason, awaiting_why(&why));
                        let _ = journal.record(run_id, &round_events).await;
                        let _ = journal.record(run_id, &waiting_events).await;
                        emit_live(sink, &waiting_events).await;
                        all.append(&mut round_events);
                        all.append(&mut waiting_events);
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
                    if every_call_refused && last_refused.as_ref() == Some(&refused) {
                        let names: Vec<&str> =
                            refused.iter().map(|(name, _)| name.as_str()).collect();
                        let why = results
                            .first()
                            .map(|result| result.content.as_str())
                            .unwrap_or("refused");
                        let mut ending = projection.fail(format!(
                            "`{}` was refused the same way twice ({why}); stopping instead of retrying",
                            names.join("`, `")
                        ));
                        let _ = journal.record(run_id, &round_events).await;
                        let _ = journal.record(run_id, &ending).await;
                        emit_live(sink, &ending).await;
                        all.append(&mut round_events);
                        all.append(&mut ending);
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
                        let mut ending = projection.fail(format!(
                            "the screen has not changed after {SAME_SCREEN_LIMIT} looks; stopping instead of waiting"
                        ));
                        let _ = journal.record(run_id, &round_events).await;
                        let _ = journal.record(run_id, &ending).await;
                        emit_live(sink, &ending).await;
                        all.append(&mut round_events);
                        all.append(&mut ending);
                        return all;
                    }

                    if !said.is_empty() {
                        request.messages.push(ChatMessage {
                            images: Vec::new(),
                            role: "assistant".to_string(),
                            content: said,
                        });
                    }

                    // DURABLE BEFORE THE NEXT CALL. Recorded here, at the top of the next round's
                    // dependency chain, so a crash after this point can be picked up.
                    if let Err(error) = journal.record(run_id, &round_events).await {
                        let failed =
                            projection.fail(format!("the run could not be recorded: {error}"));
                        emit_live(sink, &failed).await;
                        round_events.extend(failed);
                        all.append(&mut round_events);
                        return all;
                    }
                    all.append(&mut round_events);

                    // bar_chart/form already painted from TOOL_CALL frames. Another model
                    // round in this HTTP request is what doubled charts on "generate another".
                    if calls.iter().any(|call| is_client_render_tool(&call.name)) {
                        let mut ending = projection.finish();
                        emit_live(sink, &ending).await;
                        let _ = journal.record(run_id, &ending).await;
                        all.append(&mut ending);
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
                        let mut ending = projection.fail(why);
                        let _ = journal.record(run_id, &ending).await;
                        emit_live(sink, &ending).await;
                        all.append(&mut ending);
                        return all;
                    }
                    continue;
                }
            }
        }

        // No tools were asked for, or the run failed: this is the last round either way.
        //
        // A run that produced nothing at all ends as a failure that says so. Finishing cleanly
        // with an empty transcript is the dangerous empty success (CLAUDE.md, three facts №3):
        // the client cannot tell it from a coworker with nothing to say, and every one of them
        // invented its own placeholder. A run that already failed keeps its own message — `fail`
        // and `finish` are both no-ops once the run has ended.
        let mut ending = if any_delta {
            projection.finish()
        } else {
            projection.fail("the model returned no text")
        };
        emit_live(sink, &ending).await;
        round_events.append(&mut ending);
        let _ = journal.record(run_id, &round_events).await;
        all.append(&mut round_events);
        return all;
    }

    all
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
        content: format!("[tool {} result] {}", result.call_id, result.content),
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
mod tests {
    use super::*;
    use opengrok_tools::Executor;
    use opengrok_wire::agui::EventType;
    use std::sync::{Arc, Mutex};

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
        // A permissive policy: these tests are about the loop, not about policy, and an executor
        // built without one now refuses everything by design.
        let account = opengrok_core::id::AccountId::from_stored("acct_ada");
        let policy = opengrok_policy::Context {
            grant: Some(opengrok_policy::Grant {
                principal: account.clone(),
                coworker: CoworkerId::from_stored("cw_ada"),
                profile: opengrok_policy::ToolSet::All,
                needs_approval: opengrok_policy::ToolSet::None,
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
            gateway_key: None,
            spend_scope: None,
            spend_actor: None,
            model: "mock".to_string(),
            system: None,
            tools: Vec::new(),
            messages: vec![ChatMessage {
                images: Vec::new(),
                role: "user".to_string(),
                content: text.to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn a_mock_run_is_a_well_formed_agui_run() {
        let events = run_turn(&MockDoor::echoing(), request("hello"), "t1", "r1", 1).await;
        assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
        let text: String = events
            .iter()
            .filter(|event| event.event_type == EventType::TextMessageContent)
            .filter_map(|event| event.extra.get("delta").and_then(|d| d.as_str()))
            .collect();
        assert!(text.contains("hello"), "{text}");
    }

    /// The failure that matters: the client still gets an ending, so its spinner stops.
    #[tokio::test]
    async fn a_broken_stream_still_ends_the_run() {
        let door = MockDoor::failing_with("upstream hung up");
        let events = run_turn(&door, request("hello"), "t1", "r1", 1).await;
        assert_eq!(events.last().unwrap().event_type, EventType::RunError);
        assert!(
            events
                .last()
                .unwrap()
                .extra
                .get("message")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("upstream hung up")
        );
    }

    /// AN EMPTY SUCCESS IS THE DANGEROUS REPLY (CLAUDE.md, three facts №3). A round that produced
    /// nothing — no words, no tool calls — used to end the run with `RUN_FINISHED` and an empty
    /// transcript, which every client had to invent a reason for. It says the reason itself now.
    #[tokio::test]
    async fn a_run_that_produced_nothing_ends_as_an_error_that_says_so() {
        let events = run_conversation(
            &MockDoor::silent(),
            None,
            &MemoryJournal::new(),
            request("hello"),
            "t1",
            "r1",
            1,
        )
        .await;
        assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
        let ending = events.last().unwrap();
        assert_eq!(ending.event_type, EventType::RunError);
        assert_eq!(
            ending.extra.get("message").unwrap(),
            "the model returned no text"
        );
    }

    /// And a run that did produce something still ends cleanly: the new ending must not turn a
    /// working turn into a failure.
    #[tokio::test]
    async fn a_run_that_said_something_still_finishes() {
        let events = run_conversation(
            &MockDoor::echoing(),
            None,
            &MemoryJournal::new(),
            request("hello"),
            "t1",
            "r1",
            1,
        )
        .await;
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    }

    /// THE WHOLE CHAIN, JOINED. A model asks for a tool, the tool runs on the coworker's own
    /// computer, and the result comes back as an event the client can render — all in one turn.
    #[tokio::test]
    async fn a_models_tool_call_runs_on_the_coworkers_computer() {
        use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand};
        use opengrok_core::id::{BoxId, CoworkerId};
        use opengrok_tools::{Executor, ToolContext};
        use opengrok_wire::agui::EventType;
        use std::sync::Arc;

        // A coworker with a computer of its own.
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

        let computer = Arc::new(crate::tools::tests_support::RecordingComputer::default());
        let account = opengrok_core::id::AccountId::from_stored("acct_ada");
        let policy = opengrok_policy::Context {
            grant: Some(opengrok_policy::Grant {
                principal: account.clone(),
                coworker: CoworkerId::from_stored("cw_ada"),
                profile: opengrok_policy::ToolSet::All,
                needs_approval: opengrok_policy::ToolSet::None,
                revoked: false,
            }),
            ceiling: Some(opengrok_policy::Ceiling {
                coworker: CoworkerId::from_stored("cw_ada"),
                tools: opengrok_policy::ToolSet::All,
            }),
        };
        let runner = ToolRunner::new(
            Executor::with_policy(computer.clone(), policy),
            ToolContext::from_coworker(account, CoworkerId::from_stored("cw_ada"), &coworker),
        );

        // A model that asks to run a command — on somebody else's box, for good measure.
        let door = MockDoor::with_script(vec![
            ModelDelta::Text("let me check".to_string()),
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: r#"{"command":"whoami","box_id":"box_of_someone_else"}"#.to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ]);

        let events = run_turn_with_tools(&door, Some(&runner), request("go"), "t1", "r1", 1).await;

        let result = events
            .iter()
            .find(|event| event.event_type == EventType::ToolCallResult)
            .expect("the tool result should reach the client");
        assert_eq!(result.extra.get("toolCallId").unwrap(), "c1");
        assert_eq!(result.extra.get("ok").unwrap(), true);

        // The identity rule, end to end: the model named another box and got its own.
        assert_eq!(computer.last_box().as_deref(), Some("box_ada"));
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    }

    /// Without tools wired in, a tool call is still well-formed — it simply produces no result.
    #[tokio::test]
    async fn a_run_without_a_tool_runner_still_ends_cleanly() {
        use opengrok_wire::agui::EventType;
        let door = MockDoor::with_script(vec![
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ]);
        let events = run_turn(&door, request("go"), "t1", "r1", 1).await;
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
        assert!(
            !events
                .iter()
                .any(|event| event.event_type == EventType::ToolCallResult)
        );
    }

    /// THE ORDERING RULE, ASSERTED. A journal that records when the model was called proves the
    /// tool results were durable BEFORE the next call — the property a crash between rounds
    /// depends on, and one that no test about client-visible events would ever notice breaking.
    #[tokio::test]
    async fn each_rounds_results_are_recorded_before_the_next_model_call() {
        use std::sync::{Arc, Mutex};

        /// Records journal writes and model calls on one timeline.
        #[derive(Default)]
        struct Timeline {
            entries: Mutex<Vec<String>>,
        }
        impl Timeline {
            fn note(&self, what: &str) {
                if let Ok(mut entries) = self.entries.lock() {
                    entries.push(what.to_string());
                }
            }
            fn entries(&self) -> Vec<String> {
                self.entries.lock().map(|e| e.clone()).unwrap_or_default()
            }
        }

        struct WatchingJournal(Arc<Timeline>);
        #[async_trait::async_trait]
        impl RunJournal for WatchingJournal {
            async fn record(&self, _run_id: &str, events: &[Event]) -> Result<(), JournalError> {
                self.0.note(&format!("journal({})", events.len()));
                Ok(())
            }
        }

        /// Asks for a tool on the first call and simply answers on the second.
        struct TwoRoundDoor(Arc<Timeline>, Mutex<usize>);
        #[async_trait::async_trait]
        impl ModelDoor for TwoRoundDoor {
            async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
                let round = {
                    let mut calls = self.1.lock().map_err(|_| {
                        ModelError::Stream("the door's lock was poisoned".to_string())
                    })?;
                    *calls += 1;
                    *calls
                };
                self.0.note(&format!("model call {round}"));
                let script = if round == 1 {
                    vec![
                        ModelDelta::ToolCallStart {
                            id: "c1".to_string(),
                            name: "shell".to_string(),
                        },
                        ModelDelta::ToolCallArgs {
                            id: "c1".to_string(),
                            delta: r#"{"command":"ls"}"#.to_string(),
                        },
                        ModelDelta::ToolCallEnd {
                            id: "c1".to_string(),
                        },
                    ]
                } else {
                    vec![ModelDelta::Text("all done".to_string())]
                };
                Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
            }
        }

        let timeline = Arc::new(Timeline::default());
        let door = TwoRoundDoor(timeline.clone(), Mutex::new(0));
        let journal = WatchingJournal(timeline.clone());
        let runner = tool_runner();

        let events =
            run_conversation(&door, Some(&runner), &journal, request("go"), "t1", "r1", 1).await;

        let entries = timeline.entries();
        let second_call = entries
            .iter()
            .position(|entry| entry == "model call 2")
            .expect("the model should have been called a second time");
        // At least one journal write must sit between the two calls: that is the tool results
        // reaching durable storage before the call that depends on them.
        let journals_before_second = entries[..second_call]
            .iter()
            .filter(|entry| entry.starts_with("journal("))
            .count();
        assert!(
            journals_before_second >= 2,
            "results must be durable before the next call; timeline was {entries:?}"
        );

        assert_eq!(
            events.last().unwrap().event_type,
            opengrok_wire::agui::EventType::RunFinished
        );
    }

    /// A model that never stops asking would otherwise run until the money ran out. The bound ends
    /// the run as a result the client can see, not a silent stop.
    #[tokio::test]
    async fn a_model_that_never_stops_is_bounded_and_told_why() {
        struct AlwaysToolDoor;
        #[async_trait::async_trait]
        impl ModelDoor for AlwaysToolDoor {
            async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
                let script = vec![
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: "c1".to_string(),
                        delta: r#"{"command":"again"}"#.to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ];
                Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
            }
        }

        let journal = MemoryJournal::new();
        let runner = tool_runner();
        let events = run_conversation(
            &AlwaysToolDoor,
            Some(&runner),
            &journal,
            request("go"),
            "t1",
            "r1",
            1,
        )
        .await;

        let last = events.last().unwrap();
        assert_eq!(last.event_type, opengrok_wire::agui::EventType::RunError);
        assert!(
            last.extra
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .contains("limit"),
            "{last:?}"
        );
    }

    /// Seen live: a cheap model asked for `user_machine_shell` with no arguments, was refused,
    /// and asked again identically until the round cap. The second identical refusal ends the
    /// run with a reason, instead of six more model calls that change nothing.
    #[tokio::test]
    async fn a_call_refused_the_same_way_twice_ends_the_run() {
        struct ArgumentLessDoor;
        #[async_trait::async_trait]
        impl ModelDoor for ArgumentLessDoor {
            async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
                let script = vec![
                    ModelDelta::ToolCallStart {
                        id: "c1".to_string(),
                        name: "user_machine_shell".to_string(),
                    },
                    ModelDelta::ToolCallEnd {
                        id: "c1".to_string(),
                    },
                ];
                Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
            }
        }

        let journal = MemoryJournal::new();
        let runner = tool_runner();
        let events = run_conversation(
            &ArgumentLessDoor,
            Some(&runner),
            &journal,
            request("run date on my computer"),
            "t1",
            "r1",
            1,
        )
        .await;

        let starts = events
            .iter()
            .filter(|event| event.event_type == opengrok_wire::agui::EventType::ToolCallStart)
            .count();
        assert_eq!(starts, 2, "two identical refusals, then stop: {events:?}");
        let last = events.last().unwrap();
        assert_eq!(last.event_type, opengrok_wire::agui::EventType::RunError);
        let message = last
            .extra
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        assert!(
            message.contains("user_machine_shell") && message.contains("twice"),
            "{message}"
        );
    }

    /// A run that cannot be recorded must not proceed: it would produce work a reconnect can never
    /// reproduce, which is the failure this design exists to prevent.
    #[tokio::test]
    async fn a_run_that_cannot_be_recorded_does_not_run() {
        struct BrokenJournal;
        #[async_trait::async_trait]
        impl RunJournal for BrokenJournal {
            async fn record(&self, _run_id: &str, _events: &[Event]) -> Result<(), JournalError> {
                Err(JournalError::Unwritable("the disk is gone".to_string()))
            }
        }

        let events = run_conversation(
            &MockDoor::echoing(),
            None,
            &BrokenJournal,
            request("go"),
            "t1",
            "r1",
            1,
        )
        .await;

        assert_eq!(
            events.last().unwrap().event_type,
            opengrok_wire::agui::EventType::RunError
        );
        // Nothing was said: the model was never called.
        assert!(
            !events
                .iter()
                .any(|event| event.event_type
                    == opengrok_wire::agui::EventType::TextMessageContent)
        );
    }

    /// Everything a client saw is in the journal — that is what makes a replay complete.
    #[tokio::test]
    async fn every_event_a_client_saw_reached_the_journal() {
        let journal = MemoryJournal::new();
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
        assert_eq!(journal.event_count(), events.len());
    }

    /// Exactly one ending, however the run went — two would double-render in a consumer.
    #[tokio::test]
    async fn a_run_has_exactly_one_ending() {
        for door in [MockDoor::echoing(), MockDoor::failing_with("nope")] {
            let events = run_turn(&door, request("hello"), "t1", "r1", 1).await;
            let endings = events
                .iter()
                .filter(|event| {
                    matches!(
                        event.event_type,
                        EventType::RunFinished | EventType::RunError
                    )
                })
                .count();
            assert_eq!(endings, 1, "{:?}", events.last());
        }
    }

    /// NativeChat paints POST /ag-ui from this sink. If the sink is a subset of the Vec, the
    /// HTTP body is missing the opening, the close, or both — which looks like a client that
    /// never streamed.
    #[tokio::test]
    async fn a_streaming_sink_sees_every_event_the_run_produced() {
        struct Collect(std::sync::Mutex<Vec<Event>>);
        #[async_trait::async_trait]
        impl EventSink for Collect {
            async fn emit(&self, events: &[Event]) {
                if let Ok(mut seen) = self.0.lock() {
                    seen.extend(events.iter().cloned());
                }
            }
        }

        let sink = Collect(std::sync::Mutex::new(Vec::new()));
        let events = run_conversation_streaming(
            &MockDoor::echoing(),
            None,
            &MemoryJournal::new(),
            request("hello"),
            "t1",
            "r1",
            1,
            &sink,
        )
        .await;
        let sunk = sink.0.lock().unwrap().clone();
        let produced: Vec<_> = events.iter().map(|event| event.event_type).collect();
        let live: Vec<_> = sunk.iter().map(|event| event.event_type).collect();
        assert_eq!(live, produced, "sink={live:?} vec={produced:?}");
    }

    /// A paced door must deliver the first word while the run is still in flight. If this
    /// fires only after `run_conversation_streaming` joins, POST /ag-ui would still look
    /// like a buffered JSON response.
    #[tokio::test]
    async fn a_paced_sink_receives_text_while_the_model_is_still_talking() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        struct FirstText(std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
        #[async_trait::async_trait]
        impl EventSink for FirstText {
            async fn emit(&self, events: &[Event]) {
                if events
                    .iter()
                    .any(|event| event.event_type == EventType::TextMessageContent)
                    && let Some(tx) = self.0.lock().ok().and_then(|mut slot| slot.take())
                {
                    let _ = tx.send(());
                }
            }
        }

        let sink = FirstText(std::sync::Mutex::new(Some(tx)));
        let handle = tokio::spawn(async move {
            run_conversation_streaming(
                &MockDoor::echoing().paced_by_ms(40),
                None,
                &MemoryJournal::new(),
                request("hello from a paced mock door"),
                "t1",
                "r1",
                1,
                &sink,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("first text should arrive before the run ends")
            .unwrap();
        assert!(
            !handle.is_finished(),
            "text arrived only after the turn finished — the HTTP body would dump at the end"
        );
        let events = handle.await.unwrap();
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    }

    fn shot(call_id: &str) -> opengrok_tools::ToolResult {
        opengrok_tools::ToolResult::ok(call_id, "screenshot of the 1280x800 screen attached")
            .with_image(opengrok_tools::ToolImage {
                mime: "image/png".into(),
                base64: "iVBORw0KGgo=".into(),
                width: 1280,
                height: 800,
            })
    }

    #[test]
    fn a_tool_result_with_a_picture_becomes_a_message_with_an_image() {
        let message = tool_result_message(&shot("c1"));
        assert_eq!(message.role, "user");
        assert!(message.content.starts_with("[tool c1 result] screenshot"));
        assert_eq!(message.images.len(), 1);
        assert_eq!(message.images[0].mime, "image/png");

        let plain = tool_result_message(&opengrok_tools::ToolResult::ok("c2", "done"));
        assert!(plain.images.is_empty());
    }

    /// Screenshots are the widest thing in a request; only the last two say where the screen is.
    #[test]
    fn only_the_two_most_recent_screenshots_travel() {
        let mut messages: Vec<ChatMessage> = (1..=4)
            .map(|n| tool_result_message(&shot(&format!("c{n}"))))
            .collect();
        messages.insert(
            2,
            ChatMessage {
                role: "assistant".into(),
                content: "clicking".into(),
                images: Vec::new(),
            },
        );

        keep_recent_images(&mut messages, RECENT_IMAGES);

        let carried: Vec<bool> = messages.iter().map(|m| !m.images.is_empty()).collect();
        assert_eq!(carried, vec![false, false, false, true, true]);
        // The words stay even where the picture went.
        assert!(messages[0].content.contains("[tool c1 result]"));
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
                        delta: format!(
                            r#"{{"command":"open -a Safari https://facebook.com/{n}"}}"#
                        ),
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
        async fn expose_port(
            &self,
            _b: &str,
            _p: u16,
            _t: &str,
        ) -> opengrok_box::BoxResult<String> {
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
        async fn screen_url(&self, _b: &str) -> opengrok_box::BoxResult<Option<String>> {
            Ok(Some("http://127.0.0.1:1/vnc.html".into()))
        }
        async fn screenshot(&self, _b: &str) -> opengrok_box::BoxResult<opengrok_box::Screenshot> {
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
        async fn act(&self, _b: &str, _a: &opengrok_box::CuaAction) -> opengrok_box::BoxResult<()> {
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
}
