//! Running what the model asked for, on the coworker's own computer.
//!
//! The harness never decides *where* a tool runs — `opengrok-tools::Executor` does, from the
//! coworker's row. This module only reassembles the fragments a stream delivers into whole calls
//! and hands them over.
//!
//! REASSEMBLY IS THE JOB. A provider sends a tool call as a name, then arguments in pieces, then a
//! close. Acting on a fragment would mean running a command whose arguments are half-written, so
//! nothing runs until the closing fragment arrives.

use opengrok_tools::{Executor, ToolCall, ToolContext, ToolResult};
use opengrok_wire::agui::{Event, EventType};

/// A tool the SERVER answers in-process rather than the coworker's computer — a group's
/// `SendMessage`, which posts to the room. Synchronous on purpose: it records, it does not
/// reach out.
pub type LocalTool = std::sync::Arc<dyn Fn(&ToolCall) -> ToolResult + Send + Sync>;

/// The executor plus the identity to run as. Assembled by the server from the session.
pub struct ToolRunner {
    /// `None` for a coworker with no computer that still has local tools (a group member
    /// speaking to the room): every other call is refused in words, never run elsewhere.
    executor: Option<(Executor, ToolContext)>,
    local: Vec<(serde_json::Value, LocalTool)>,
}

impl ToolRunner {
    pub fn new(executor: Executor, context: ToolContext) -> Self {
        Self {
            executor: Some((executor, context)),
            local: Vec::new(),
        }
    }

    /// A runner with no computer behind it: only the local tools added to it can run.
    pub fn local_only() -> Self {
        Self {
            executor: None,
            local: Vec::new(),
        }
    }

    /// Offer one more tool, answered in-process. `schema` is the OpenAI function definition
    /// (`{type, function: {name, …}}`) the model is shown; the handler runs when it is called.
    #[must_use]
    pub fn with_local(mut self, schema: serde_json::Value, handler: LocalTool) -> Self {
        self.local.push((schema, handler));
        self
    }

    /// Give this turn the room's shared computer as well: `machine: "group"` on the box tools.
    #[must_use]
    pub fn with_group_box(mut self, box_id: opengrok_core::id::BoxId, name: &str) -> Self {
        if let Some((executor, context)) = self.executor.as_mut() {
            executor.set_group_box_name(name);
            context.group_box = Some(opengrok_tools::GroupBox {
                box_id,
                name: name.to_string(),
            });
        }
        self
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

    /// The person said yes, in this run, to a leave-box action: the tunnel is not asked about
    /// again. See `Executor::with_egress_consented`.
    #[must_use]
    pub fn with_egress_consented(mut self, consented: bool) -> Self {
        if let Some((executor, context)) = self.executor.take() {
            self.executor = Some((executor.with_egress_consented(consented), context));
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
        schemas
    }

    /// Run a single call — the MCP door's shape, where each request is one call with no turn
    /// around it. Same executor, same identity: the door gets no path around the gates. A local
    /// tool answers here; a computer tool with no computer is refused, never run elsewhere.
    pub async fn run_one(&self, call: &ToolCall) -> ToolResult {
        if let Some(handler) = self.local_for(&call.name) {
            return handler(call);
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
            },
        }
    }

    pub async fn run_all(&self, calls: &[ToolCall]) -> Vec<ToolResult> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            // Sequentially: a model's calls in one turn frequently depend on each other (write a
            // file, then run it), and running them concurrently would race on the same filesystem.
            results.push(self.run_one(call).await);
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
                    } else if name == opengrok_tools::REQUEST_CREDENTIAL
                        || name
                            == opengrok_tools::openai_safe_tool_name(
                                opengrok_tools::REQUEST_CREDENTIAL,
                            )
                    {
                        opengrok_tools::credential::sanitize_request(&arguments)
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
pub mod tests_support {
    use async_trait::async_trait;
    use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct RecordingComputer {
        boxes: Mutex<Vec<String>>,
        /// Scripted states, the last repeating; empty means "running" from the start.
        states: Mutex<std::collections::VecDeque<&'static str>>,
        resumes: std::sync::atomic::AtomicUsize,
    }

    impl RecordingComputer {
        /// A box that reports these states in order (the last one repeats).
        pub fn sleeping(states: &[&'static str]) -> Self {
            Self {
                states: Mutex::new(states.iter().copied().collect()),
                ..Self::default()
            }
        }

        pub fn last_box(&self) -> Option<String> {
            self.boxes
                .lock()
                .ok()
                .and_then(|calls| calls.last().cloned())
        }

        pub fn resumes(&self) -> usize {
            self.resumes.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Computer for RecordingComputer {
        async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
            Ok("box_new".to_string())
        }
        async fn run(&self, box_id: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
            if let Ok(mut boxes) = self.boxes.lock() {
                boxes.push(box_id.to_string());
            }
            Ok(CommandOutput {
                exit_code: 0,
                stdout: format!("ran `{command}`"),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                timed_out: false,
            })
        }
        async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
            Err(opengrok_box::BoxError::NoSuchBox)
        }
        async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
            Err(opengrok_box::BoxError::NoSuchBox)
        }
        async fn read_file(&self, box_id: &str, _p: &str) -> BoxResult<String> {
            if let Ok(mut boxes) = self.boxes.lock() {
                boxes.push(box_id.to_string());
            }
            Ok(String::new())
        }
        async fn write_file(&self, box_id: &str, _p: &str, _c: &str) -> BoxResult<()> {
            if let Ok(mut boxes) = self.boxes.lock() {
                boxes.push(box_id.to_string());
            }
            Ok(())
        }
        async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
            Ok(String::new())
        }
        async fn stop(&self, _b: &str) -> BoxResult<()> {
            Ok(())
        }
        async fn resume(&self, _b: &str) -> BoxResult<()> {
            self.resumes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn state(&self, _b: &str) -> BoxResult<String> {
            let mut states = self
                .states
                .lock()
                .map_err(|_| opengrok_box::BoxError::NoSuchBox)?;
            let next = if states.len() > 1 {
                states.pop_front().unwrap_or("running")
            } else {
                states.front().copied().unwrap_or("running")
            };
            Ok(next.to_string())
        }
        async fn destroy(&self, _b: &str) -> BoxResult<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::ModelDelta;
    use crate::projection::Projection;

    fn events_for(deltas: Vec<ModelDelta>) -> Vec<Event> {
        let mut projection = Projection::new("t1", "r1", 1);
        let mut events = Vec::new();
        for delta in deltas {
            events.extend(projection.push(delta));
        }
        events
    }

    #[test]
    fn fragments_are_reassembled_into_one_call() {
        let events = events_for(vec![
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: "{\"command\":".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: "\"ls -la\"}".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ]);
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    /// A truncated stream leaves partial JSON. Running it would be acting on half a sentence.
    #[test]
    fn an_unterminated_call_is_not_run() {
        let events = events_for(vec![
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: "{\"command\": \"rm -r".to_string(),
            },
        ]);
        assert!(collect_tool_calls(&events).is_empty());
    }

    #[test]
    fn several_calls_are_kept_apart_and_in_order() {
        let events = events_for(vec![
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: "{\"command\":\"one\"}".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
            ModelDelta::ToolCallStart {
                id: "c2".to_string(),
                name: "read_file".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c2".to_string(),
                delta: "{\"path\":\"/tmp/a\"}".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c2".to_string(),
            },
        ]);
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["command"], "one");
        assert_eq!(calls[1].name, "read_file");
    }

    /// Unparseable arguments must still produce a call, so the executor can refuse it with a
    /// reason. Dropping it would leave the model waiting for a result that never comes.
    #[test]
    fn unparseable_arguments_still_produce_a_call_to_refuse() {
        let events = events_for(vec![
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: "not json at all".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ]);
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].arguments.is_null());
    }

    #[test]
    fn a_run_with_no_tool_calls_yields_none() {
        let events = events_for(vec![ModelDelta::Text("just talking".to_string())]);
        assert!(collect_tool_calls(&events).is_empty());
    }

    #[test]
    fn request_user_form_arguments_drop_smuggled_values() {
        let events = events_for(vec![
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: opengrok_tools::REQUEST_USER_FORM.to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: serde_json::json!({
                    "title": "Sign in",
                    "fields": [{ "id": "password", "label": "Password", "type": "password" }],
                    "values": { "password": "s3cret-should-never-land" }
                })
                .to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ]);
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 1);
        let dumped = calls[0].arguments.to_string();
        assert!(!dumped.contains("s3cret-should-never-land"), "{dumped}");
        assert!(calls[0].arguments.get("values").is_none(), "{dumped}");
        assert_eq!(calls[0].arguments["title"], "Sign in");
    }

    #[test]
    fn credential_request_arguments_drop_a_smuggled_password() {
        for tool_name in [opengrok_tools::REQUEST_CREDENTIAL, "credential_request"] {
            let events = events_for(vec![
                ModelDelta::ToolCallStart {
                    id: "c1".to_string(),
                    name: tool_name.to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: "c1".to_string(),
                    delta: serde_json::json!({
                        "origin": "accounts.google.com",
                        "password": "s3cret-should-never-land"
                    })
                    .to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "c1".to_string(),
                },
            ]);
            let calls = collect_tool_calls(&events);
            assert_eq!(calls.len(), 1, "{tool_name}");
            let dumped = calls[0].arguments.to_string();
            assert!(
                !dumped.contains("s3cret-should-never-land"),
                "{tool_name}: {dumped}"
            );
            assert_eq!(calls[0].arguments["origin"], "accounts.google.com");
            assert!(
                calls[0].arguments.get("password").is_none(),
                "{tool_name}: {dumped}"
            );
        }
    }
}
