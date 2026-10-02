//! Running what the model asked for, on the coworker's own computer.
//!
//! The harness never decides *where* a tool runs — `opengrok-tools::Executor` does, from the
//! coworker's row. This module only reassembles the fragments a stream delivers into whole calls
//! and hands them over.
//!
//! REASSEMBLY IS THE JOB. A provider sends a tool call as a name, then arguments in pieces, then a
//! close. Acting on a fragment would mean running a command whose arguments are half-written, so
//! nothing runs until the closing fragment arrives.

use opengrok_tools::message_bot::{BotMail, BotOffer, MESSAGE_BOT};
use opengrok_tools::skill::{SkillOffer, SkillSource, USE_SKILL};
use opengrok_tools::{Executor, ToolCall, ToolContext, ToolResult};
use opengrok_wire::agui::{Event, EventType};

/// A tool the SERVER answers in-process rather than the coworker's computer — a group's
/// `SendMessage`, which posts to the room. Synchronous on purpose: it records, it does not
/// reach out.
pub type LocalTool = std::sync::Arc<dyn Fn(&ToolCall) -> ToolResult + Send + Sync>;

/// `message_bot`'s offers, its sender and whether it is advertised, and its mail (#314).
type Bots = (Vec<BotOffer>, (String, bool), std::sync::Arc<dyn BotMail>);

/// The executor plus the identity to run as. Assembled by the server from the session.
pub struct ToolRunner {
    /// `None` for a coworker with no computer that still has local tools (a group member
    /// speaking to the room): every other call is refused in words, never run elsewhere.
    executor: Option<(Executor, ToolContext)>,
    local: Vec<(serde_json::Value, LocalTool)>,
    /// The skills `use_skill` reads this turn, and where from (#270). `None` offers no tool.
    skills: Option<(Vec<SkillOffer>, std::sync::Arc<dyn SkillSource>)>,
    /// The person's Bots `message_bot` may name, the sender's id, whether the tool is advertised,
    /// and where a call's messages are written (#314). Not advertised at its chain's last hop, but
    /// a call still reaches `mail`, which refuses it in the contract's words. `None`: no tool.
    bots: Option<Bots>,
}

impl ToolRunner {
    pub fn new(executor: Executor, context: ToolContext) -> Self {
        Self {
            executor: Some((executor, context)),
            local: Vec::new(),
            skills: None,
            bots: None,
        }
    }

    /// A runner with no computer behind it: only the local tools added to it can run.
    pub fn local_only() -> Self {
        Self {
            executor: None,
            local: Vec::new(),
            skills: None,
            bots: None,
        }
    }

    /// Offer one more tool, answered in-process. `schema` is the OpenAI function definition
    /// (`{type, function: {name, …}}`) the model is shown; the handler runs when it is called.
    #[must_use]
    pub fn with_local(mut self, schema: serde_json::Value, handler: LocalTool) -> Self {
        self.local.push((schema, handler));
        self
    }

    /// Offer `use_skill` for `offers`, read through `source` — and nothing for none, so the tool
    /// is on offer exactly when `skills_line` lists a skill (`opengrok_tools::skill`).
    #[must_use]
    pub fn with_skills(
        mut self,
        offers: Vec<SkillOffer>,
        source: std::sync::Arc<dyn SkillSource>,
    ) -> Self {
        self.skills = (!offers.is_empty()).then_some((offers, source));
        self
    }

    /// Offer `message_bot` naming `offers`, sent as `sender` through `mail` (#314).
    #[must_use]
    pub fn with_bots(
        mut self,
        offers: Vec<BotOffer>,
        sender: (String, bool),
        mail: std::sync::Arc<dyn BotMail>,
    ) -> Self {
        self.bots = Some((offers, sender, mail));
        self
    }

    /// The skills `use_skill` reads this turn, as a run captures them: its resumes offer these.
    pub fn offered_skills(&self) -> Vec<opengrok_core::run::OfferedSkill> {
        let offers = self.skills.iter().flat_map(|(offers, _)| offers);
        offers.map(Into::into).collect()
    }

    /// The system message's list of the skills this runner offers, or nothing.
    pub fn skills_line(&self) -> String {
        let offers = self.skills.as_ref().map(|(offers, _)| offers.as_slice());
        opengrok_tools::skill::offered_line(offers.unwrap_or_default())
    }

    /// Whether running `call` would first wake the coworker's box — asked before a round so the
    /// stream can say so (`box-waking`) instead of going quiet for the wait.
    pub async fn box_needs_wake(&self, call: &ToolCall) -> bool {
        match self.executor.as_ref() {
            Some((executor, context)) => executor.box_needs_wake(context, call).await,
            None => false,
        }
    }

    /// The coworker whose tools these are, for the frames that name one.
    pub fn coworker_id(&self) -> Option<String> {
        self.executor
            .as_ref()
            .map(|(_, context)| context.coworker_id.to_string())
    }

    /// Whether the box behind this runner has a screen, i.e. `open_url` and `computer` are on
    /// offer. The prompt must say the same thing the offering does.
    pub fn has_screen(&self) -> bool {
        self.executor
            .as_ref()
            .is_some_and(|(executor, _)| executor.has_screen())
    }

    /// The computer may not use the person's network while the tunnel is on: the browser tools
    /// are withheld and the prompt says why. See `Executor::network_off`.
    pub fn network_off(&self) -> bool {
        self.executor
            .as_ref()
            .is_some_and(|(executor, _)| executor.network_off())
    }

    /// The withholding is a fail-closed stand-in, not the person's choice. See
    /// `Executor::network_unconfirmed`.
    pub fn network_unconfirmed(&self) -> bool {
        self.executor
            .as_ref()
            .is_some_and(|(executor, _)| executor.network_unconfirmed())
    }

    /// The same, asked once the box is awake, for a path that wakes the box itself (the
    /// user-form fill). See `Executor::network_off_now`.
    pub async fn network_off_now(&self) -> bool {
        match self.executor.as_ref() {
            Some((executor, context)) => match context.box_id.as_ref() {
                Some(box_id) => executor.network_off_now(box_id.as_str()).await,
                None => executor.network_off(),
            },
            None => false,
        }
    }

    /// A run a routine started: of the routine tools, only the listing (#316). See
    /// `Executor::with_routines_listing_only`.
    #[must_use]
    pub fn with_routines_listing_only(mut self) -> Self {
        if let Some((executor, context)) = self.executor.take() {
            self.executor = Some((executor.with_routines_listing_only(), context));
        }
        self
    }

    /// The person said yes, in this run, to a leave-box action: the tunnel is not asked about
    /// again. See `Executor::with_egress_consented`.
    #[must_use]
    pub fn with_egress_consented(mut self, consented: bool) -> Self {
        if let Some((executor, context)) = self.executor.take() {
            self.executor = Some((executor.with_egress_consented(consented), context));
        }
        self
    }

    /// The judge's failures in a row so far in this run. See `judge_failure_streak`.
    #[must_use]
    pub fn with_judge_failures(mut self, failures: u32) -> Self {
        if let Some((executor, context)) = self.executor.take() {
            self.executor = Some((executor.with_judge_failures(failures), context));
        }
        self
    }

    /// Bring the coworker's own box up before typing into it outside `computer_use`, through the
    /// executor's memo and in-use stamp.
    pub async fn wake_fill_target(&self) -> Result<(), String> {
        match self.executor.as_ref() {
            Some((executor, context)) => executor.wake_own_box(context).await,
            None => Err("this coworker has no computer".to_string()),
        }
    }

    /// The live box to type into outside `computer_use`. `None` when this runner has no computer.
    #[must_use]
    pub fn fill_target(&self) -> Option<(std::sync::Arc<dyn opengrok_box::Computer>, String)> {
        let (executor, context) = self.executor.as_ref()?;
        Some((
            executor.computer(),
            context.box_id.as_ref()?.as_str().to_string(),
        ))
    }

    /// Carry through what the person chose in the composer: a recipe, and the values they typed.
    ///
    /// Applied after the runner is built rather than threaded through its constructor, because
    /// the constructor is shared with the scheduler and the MCP door, and neither of those has a
    /// composer or a person in front of it.
    pub fn with_chosen_recipe(
        mut self,
        recipe_id: impl Into<String>,
        // The map itself rather than the recipes crate's alias for it: the harness has no other
        // reason to depend on that crate, and a dependency edge for a type alias is not one.
        values: std::collections::BTreeMap<String, String>,
    ) -> Self {
        if let Some((executor, context)) = self.executor.take() {
            self.executor = Some((executor.with_chosen_recipe(recipe_id, values), context));
        }
        self
    }

    /// The system-message sentence naming plugin servers that could not be reached this turn, or
    /// nothing. See `Executor::unavailable_plugins_line`.
    pub fn unavailable_plugins_line(&self) -> String {
        self.executor
            .as_ref()
            .map(|(executor, _)| executor.unavailable_plugins_line())
            .unwrap_or_default()
    }

    /// Whether this runner offers `run_recipe`: a screen plus at least one granted recipe.
    pub fn has_recipes(&self) -> bool {
        self.executor
            .as_ref()
            .is_some_and(|(executor, _)| executor.has_recipes())
    }

    fn local_for(&self, name: &str) -> Option<&LocalTool> {
        self.local
            .iter()
            .find(|(schema, _)| schema["function"]["name"] == name)
            .map(|(_, handler)| handler)
    }

    /// The OpenAI tool definitions to advertise to the model this turn — the offering that pairs
    /// with `run_all`'s execution. The harness fills `ModelRequest.tools` from this before each door
    /// call, so the model actually knows the tools exist.
    pub fn tool_schemas(&self) -> Vec<serde_json::Value> {
        let mut schemas = self
            .executor
            .as_ref()
            .map(|(executor, context)| {
                executor.tool_schemas(&context.account_id, &context.coworker_id)
            })
            .unwrap_or_default();
        schemas.extend(self.local.iter().map(|(schema, _)| schema.clone()));
        schemas.extend(
            self.skills
                .as_ref()
                .map(|(offers, _)| opengrok_tools::skill::schema(offers)),
        );
        let bots = self
            .bots
            .as_ref()
            .filter(|(_, (_, advertised), _)| *advertised);
        schemas
            .extend(bots.map(|(offers, me, _)| opengrok_tools::message_bot::schema(offers, &me.0)));
        schemas
    }

    /// Run a single call — the MCP door's shape, where each request is one call with no turn
    /// around it. Same executor, same identity: the door gets no path around the gates. A local
    /// tool answers here; a computer tool with no computer is refused, never run elsewhere.
    pub async fn run_one(&self, call: &ToolCall) -> ToolResult {
        if let Some(handler) = self.local_for(&call.name) {
            return handler(call);
        }
        // Before the executor, so no ceiling, grant or approval gates it, ON PURPOSE: it only reads
        // instructions its owner attached, as the system message already lists them, and it never
        // raises a card — no approval list may name it (the server's `set_approvals`).
        if let Some((offers, source)) = self.skills.as_ref().filter(|_| call.name == USE_SKILL) {
            return opengrok_tools::skill::answer(call, offers, source.as_ref()).await;
        }
        // Before the executor too: its ceiling, ownership and caps were asked when it was offered,
        // and its `BotMail` asks them again (#314); it runs nothing on a computer.
        if let Some((offers, me, mail)) = self.bots.as_ref().filter(|_| call.name == MESSAGE_BOT) {
            return opengrok_tools::message_bot::answer(call, offers, &me.0, mail.as_ref()).await;
        }
        match self.executor.as_ref() {
            Some((executor, context)) => executor.execute(context, call).await,
            None => ToolResult {
                image: None,
                call_id: call.id.clone(),
                ok: false,
                content: "this coworker has no computer, so it has no tools to run".to_string(),
                awaiting_approval: false,
                awaiting_reason: None,
                stopped_part_way: false,
            },
        }
    }

    /// Each call's result with its own wall clock in milliseconds, from the moment it starts
    /// running to the moment its result exists: `TOOL_CALL_RESULT.durationMs` (#305) and
    /// `run-timing`'s tools. The one place a call is timed, the approved call of a resume too.
    pub async fn run_all(&self, calls: &[ToolCall]) -> Vec<(ToolResult, u64)> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            // Sequentially: a model's calls in one turn frequently depend on each other (write a
            // file, then run it), and running them concurrently would race on the same filesystem.
            // So a call's time is its own, never the round's.
            let started = std::time::Instant::now();
            let result = self.run_one(call).await;
            results.push((result, crate::timing::elapsed_ms(started)));
        }
        results
    }
}

/// Rebuild whole tool calls from the events a run produced.
///
/// Only calls that were CLOSED are returned. An unterminated call is a truncated stream, and its
/// arguments are partial JSON — running it would be acting on half a sentence.
pub fn collect_tool_calls(events: &[Event]) -> Vec<ToolCall> {
    let mut open: Vec<(String, String, String)> = Vec::new();
    let mut done = Vec::new();

    for event in events {
        let id = event
            .extra
            .get("toolCallId")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            continue;
        }

        match event.event_type {
            EventType::ToolCallStart => {
                let name = event
                    .extra
                    .get("toolCallName")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                open.push((id, name, String::new()));
            }
            EventType::ToolCallArgs => {
                if let Some(entry) = open.iter_mut().find(|(open_id, _, _)| *open_id == id)
                    && let Some(delta) = event.extra.get("delta").and_then(|value| value.as_str())
                {
                    entry.2.push_str(delta);
                }
            }
            EventType::ToolCallEnd => {
                if let Some(index) = open.iter().position(|(open_id, _, _)| *open_id == id) {
                    let (id, name, arguments) = open.remove(index);
                    // Arguments that will not parse are passed through as null; the executor
                    // refuses them with a reason, which the model can act on. Dropping the call
                    // silently would leave it waiting for a result that never comes.
                    let arguments =
                        serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null);
                    // A model that smuggled `values` onto `request_user_form` must not have
                    // those secrets reach execute, the pending row, or a later resume.
                    let arguments = if name == opengrok_tools::REQUEST_USER_FORM {
                        opengrok_tools::user_form::sanitize_arguments(&arguments)
                    } else {
                        arguments
                    };
                    done.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            _ => {}
        }
    }

    done
}

/// A computer that records where it was asked to act, shared by the tests in this crate.
#[cfg(test)]
#[path = "../tests/support/tests_support.rs"]
pub mod tests_support;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/tools_tests.rs"]
mod tests;
