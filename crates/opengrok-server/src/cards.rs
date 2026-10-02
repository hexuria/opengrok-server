//! Transcript cards the server emits when a run suspends. The builders are
//! `opengrok_tools::cards`, beside the tools whose calls they describe; what stays is the one
//! that edits an AG-UI frame, since the tools crate does not reach the wire crate.

use serde_json::Value;

pub use opengrok_tools::cards::*;

/// Put `summary` on a `run-awaiting-approval` frame that has none (#249). The harness builds the
/// frame and cannot reach this crate's `summary_for`, so the server adds it where frames leave:
/// the live stream (`sse`) and the journal (`append_events_once`), which replays read from.
pub fn stamp_summary(event: &mut opengrok_wire::agui::Event) {
    if event.event_type != opengrok_wire::agui::EventType::Custom
        || event.extra.get("name").and_then(Value::as_str) != Some("run-awaiting-approval")
        || event.extra.contains_key("summary")
    {
        return;
    }
    let tool = event
        .extra
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let reason = event
        .extra
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = event.extra.get("arguments").cloned().unwrap_or(Value::Null);
    if let Some(summary) = summary_for_ask(&tool, &arguments, &reason) {
        event
            .extra
            .insert("summary".to_string(), Value::String(summary));
    }
}
