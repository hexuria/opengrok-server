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
    }

    impl RecordingComputer {
        pub fn last_box(&self) -> Option<String> {
            self.boxes
                .lock()
                .ok()
                .and_then(|calls| calls.last().cloned())
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
            Ok(())
        }
        async fn state(&self, _b: &str) -> BoxResult<String> {
            Ok("running".to_string())
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
}
