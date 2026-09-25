//! Per-phase wall clock for one harness run.
//!
//! NativeChat paints every `TEXT_MESSAGE` as chat, so a four-round BIR read looks like minutes
//! of chatter with no clock. Grok Bot hid the same hops behind status chrome. The CUSTOM
//! `run-timing` frame is the number NativeChat can put in a debug drawer; the log line is for
//! operators grepping a replica. Compact on every run — one frame, one line — so it does not
//! need a flag to exist. `OG_TURN_TIMING=1` only adds the JSON body to the log.

use std::time::Instant;

use opengrok_wire::agui::{Event, EventType};
use serde_json::{Value, json};

use crate::projection::Projection;

/// Wire `name` NativeChat (and logs) key off. Same channel as `run-stopped` /
/// `run-awaiting-approval`.
pub const RUN_TIMING_NAME: &str = "run-timing";

/// Verbose log of the JSON body. Compact CUSTOM is always emitted.
pub fn verbose_from_env() -> bool {
    matches!(
        std::env::var("OG_TURN_TIMING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

pub fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Clone)]
pub struct ToolPhase {
    pub name: String,
    pub ms: u64,
}

#[derive(Debug)]
pub struct TurnTiming {
    started: Instant,
    model_ms: Vec<u64>,
    tools: Vec<ToolPhase>,
    tool_wait_ms: u64,
    auto_review_ms: u64,
    tool_rounds: u32,
    /// The limits this run was held to, so a replay can say what it was allowed.
    budget: Option<Value>,
    /// Why the run ended on its wrap-up call, when it did: which limit it reached.
    wrapped_up: Option<String>,
}

impl TurnTiming {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            model_ms: Vec::new(),
            tools: Vec::new(),
            tool_wait_ms: 0,
            auto_review_ms: 0,
            tool_rounds: 0,
            budget: None,
            wrapped_up: None,
        }
    }

    pub fn budget(&mut self, budget: &crate::RunBudget) {
        self.budget = serde_json::to_value(budget).ok();
    }

    pub fn wrapped_up(&mut self, why: &str) {
        self.wrapped_up = Some(why.to_string());
    }

    pub fn record_model(&mut self, ms: u64) {
        self.model_ms.push(ms);
    }

    pub fn record_tools(&mut self, phases: Vec<ToolPhase>, wait_ms: u64, auto_review_ms: u64) {
        self.tool_wait_ms = self.tool_wait_ms.saturating_add(wait_ms);
        self.auto_review_ms = self.auto_review_ms.saturating_add(auto_review_ms);
        self.tool_rounds = self.tool_rounds.saturating_add(1);
        self.tools.extend(phases);
    }

    pub fn total_ms(&self) -> u64 {
        elapsed_ms(self.started)
    }

    /// The run's own record of what it spent. Every frame from the loop now carries `budget`;
    /// `wrapped_up` only when the run ended on its wrap-up call. Both are keys a reader that
    /// does not know them skips, and a frame that set neither (a resume stopped before its
    /// approved call) reads as it always did.
    pub fn value(&self) -> Value {
        let mut value = json!({
            "model_ms": self.model_ms,
            "tools": self.tools.iter().map(|tool| {
                json!({ "name": tool.name, "ms": tool.ms })
            }).collect::<Vec<_>>(),
            "tool_wait_ms": self.tool_wait_ms,
            "auto_review_ms": self.auto_review_ms,
            "total_ms": self.total_ms(),
            "tool_rounds": self.tool_rounds,
        });
        if let Some(object) = value.as_object_mut() {
            if let Some(budget) = &self.budget {
                object.insert("budget".to_string(), budget.clone());
            }
            if let Some(why) = &self.wrapped_up {
                object.insert("wrapped_up".to_string(), json!(why));
            }
        }
        value
    }

    /// Compact CUSTOM. Flattened `total_ms` / `tool_rounds` so a drawer that only reads
    /// top-level extra still has a clock; the rest lives in `value`.
    pub fn event(&self, projection: &Projection) -> Event {
        let value = self.value();
        projection
            .custom(RUN_TIMING_NAME, value.clone())
            .with("total_ms", value["total_ms"].clone())
            .with("tool_rounds", value["tool_rounds"].clone())
    }

    pub fn log(&self, run_id: &str, verbose: bool) {
        let total_ms = self.total_ms();
        tracing::info!(
            run_id,
            total_ms,
            tool_rounds = self.tool_rounds,
            model_calls = self.model_ms.len(),
            tool_wait_ms = self.tool_wait_ms,
            auto_review_ms = self.auto_review_ms,
            "run-timing"
        );
        if verbose {
            tracing::info!(run_id, value = %self.value(), "run-timing detail");
        }
    }
}

/// CUSTOM sits with the closer, never after it: a consumer holds its spinner on
/// `RUN_FINISHED` / `RUN_ERROR`.
pub fn splice_before_run_end(events: &mut Vec<Event>, extra: Event) {
    let at = events
        .iter()
        .rposition(|event| {
            matches!(
                event.event_type,
                EventType::RunFinished | EventType::RunError
            )
        })
        .unwrap_or(events.len());
    events.insert(at, extra);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn timing_event_is_custom_run_timing_with_compact_value() {
        let mut projection = Projection::new("t1", "r1", 7);
        let _ = projection.start();
        let mut timing = TurnTiming::new();
        timing.record_model(12);
        timing.record_tools(
            vec![ToolPhase {
                name: "shell".into(),
                ms: 3,
            }],
            3,
            0,
        );
        let event = timing.event(&projection);
        assert_eq!(event.event_type, EventType::Custom);
        assert_eq!(
            event.extra.get("name").and_then(Value::as_str),
            Some(RUN_TIMING_NAME)
        );
        assert_eq!(
            event.extra.get("threadId").and_then(Value::as_str),
            Some("t1")
        );
        assert_eq!(event.extra.get("runId").and_then(Value::as_str), Some("r1"));
        let value = event.extra.get("value").cloned().unwrap();
        assert_eq!(value["model_ms"], json!([12]));
        assert_eq!(value["tools"][0]["name"], "shell");
        assert_eq!(value["tools"][0]["ms"], 3);
        assert_eq!(value["tool_wait_ms"], 3);
        assert_eq!(value["auto_review_ms"], 0);
        assert_eq!(value["tool_rounds"], 1);
        assert!(value["total_ms"].as_u64().is_some());
        assert_eq!(
            event.extra.get("tool_rounds").and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn splice_puts_timing_before_run_finished() {
        let mut events = vec![
            Event::new(EventType::TextMessageEnd, 1).with("messageId", "m1"),
            Event::new(EventType::RunFinished, 1).with("runId", "r1"),
        ];
        splice_before_run_end(
            &mut events,
            Event::new(EventType::Custom, 1).with("name", RUN_TIMING_NAME),
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>(),
            vec![
                EventType::TextMessageEnd,
                EventType::Custom,
                EventType::RunFinished
            ]
        );
    }

    #[test]
    fn splice_puts_timing_before_run_error() {
        let mut events = vec![Event::new(EventType::RunError, 1).with("message", "nope")];
        splice_before_run_end(
            &mut events,
            Event::new(EventType::Custom, 1).with("name", RUN_TIMING_NAME),
        );
        assert_eq!(events[0].event_type, EventType::Custom);
        assert_eq!(events[1].event_type, EventType::RunError);
    }
}
