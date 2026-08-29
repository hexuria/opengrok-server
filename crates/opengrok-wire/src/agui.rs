//! AG-UI — the protocol openbot speaks to us.
//!
//! Provenance: `@ag-ui/core` **0.0.57**, the version pinned in
//! `hexuria/openbot/app/package.json`. Read from the published package's `dist/index.d.ts`
//! (`EventType` at :4142, `BaseEventSchema` at :4192, `RunAgentInputSchema` at :2305).
//!
//! UNLIKE `transcript` AND `command`, THIS IS A SPECIFICATION, NOT A RECONSTRUCTION. AG-UI is
//! published, versioned and MIT-licensed, so implementing it is ordinary interop work with none of
//! `LEGAL.md`'s constraints. The discipline is the same anyway — the names here are the wire's
//! names, in SCREAMING_SNAKE_CASE because that is what the enum on the wire uses.
//!
//! THE SPEC ITSELF SAYS "PASSTHROUGH". `BaseEventSchema` is declared passthrough in zod, meaning a
//! conforming producer may add fields and a conforming consumer must not choke on them. So the
//! round-trip invariant (CLAUDE.md #2) is not us being careful here — it is the protocol's own
//! rule, and `extra` on every event is how we keep it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Unknown-but-preserved fields, exactly as in `transcript`.
pub type Extra = Map<String, Value>;

/// Every event type in 0.0.57, in declaration order.
///
/// The five `THINKING_*` variants are deprecated in favour of `REASONING_*` and slated for removal
/// in 1.0.0. They are carried because a client pinned to 0.0.57 may still emit them, and dropping
/// one would be dropping somebody's reasoning trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    TextMessageStart,
    TextMessageContent,
    TextMessageEnd,
    TextMessageChunk,
    ToolCallStart,
    ToolCallArgs,
    ToolCallEnd,
    ToolCallChunk,
    ToolCallResult,
    /// Deprecated in 0.0.57; use `ReasoningStart`.
    ThinkingStart,
    /// Deprecated in 0.0.57; use `ReasoningEnd`.
    ThinkingEnd,
    /// Deprecated in 0.0.57; use `ReasoningMessageStart`.
    ThinkingTextMessageStart,
    /// Deprecated in 0.0.57; use `ReasoningMessageContent`.
    ThinkingTextMessageContent,
    /// Deprecated in 0.0.57; use `ReasoningMessageEnd`.
    ThinkingTextMessageEnd,
    StateSnapshot,
    StateDelta,
    MessagesSnapshot,
    ActivitySnapshot,
    ActivityDelta,
    Raw,
    Custom,
    RunStarted,
    RunFinished,
    RunError,
    StepStarted,
    StepFinished,
    ReasoningStart,
    ReasoningMessageStart,
    ReasoningMessageContent,
    ReasoningMessageEnd,
    ReasoningMessageChunk,
    ReasoningEnd,
    ReasoningEncryptedValue,
}

/// One event on the wire.
///
/// Deliberately NOT a variant-per-event enum. The payload differs per type but the envelope does
/// not, and the spec's passthrough rule means the useful shape is "the three known fields plus
/// whatever else came". A 32-variant enum would have to be edited for every spec release; this
/// survives one, which is the same bet `StreamFrame` makes for the desktop client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// Milliseconds. Optional in the schema, so absent rather than zero when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// The provider event this was derived from, when a producer chooses to carry it.
    #[serde(default, rename = "rawEvent", skip_serializing_if = "Option::is_none")]
    pub raw_event: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Event {
    /// A bare event of `event_type`, stamped now.
    pub fn new(event_type: EventType, timestamp_ms: i64) -> Self {
        Self {
            event_type,
            timestamp: Some(timestamp_ms),
            raw_event: None,
            extra: Map::new(),
        }
    }

    /// Add a field to the envelope. Chainable, because most events are two or three fields over the
    /// base and a struct per event would be 32 structs.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_string(), value.into());
        self
    }

    /// Render as one SSE `data:` frame, terminated.
    ///
    /// Serialisation cannot fail for a `Map<String, Value>` envelope, but `unwrap` is denied
    /// workspace-wide and a panic mid-stream would take down a run: an unserialisable event
    /// degrades to `None` and the caller decides.
    pub fn to_sse_frame(&self) -> Option<String> {
        serde_json::to_string(self)
            .ok()
            .map(|json| format!("data: {json}\n\n"))
    }
}

/// What openbot POSTs to start a run (`RunAgentInputSchema`, `:2305`).
///
/// `state`, `tools`, `context` and `forwardedProps` are `Value` rather than modelled types: they
/// are caller-defined by the spec, and inventing Rust shapes for them would be inventing a contract
/// rather than transcribing one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAgentInput {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(
        default,
        rename = "parentRunId",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_run_id: Option<String>,
    #[serde(default)]
    pub state: Value,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Value,
    #[serde(default)]
    pub context: Value,
    #[serde(default, rename = "forwardedProps")]
    pub forwarded_props: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One message in the conversation the client sends us.
///
/// The spec discriminates on `role` across developer/system/assistant/user/tool. We keep the
/// envelope and carry the rest: the harness needs role and content, and narrowing the union here
/// would reject a role a later client considers ordinary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The wire spells these in SCREAMING_SNAKE_CASE. Getting this wrong means openbot silently
    /// ignores every event we send.
    #[test]
    fn event_types_serialise_as_the_wire_spells_them() {
        let json = serde_json::to_string(&EventType::TextMessageContent).unwrap();
        assert_eq!(json, "\"TEXT_MESSAGE_CONTENT\"");
        let json = serde_json::to_string(&EventType::ReasoningEncryptedValue).unwrap();
        assert_eq!(json, "\"REASONING_ENCRYPTED_VALUE\"");
        let parsed: EventType = serde_json::from_str("\"TOOL_CALL_RESULT\"").unwrap();
        assert_eq!(parsed, EventType::ToolCallResult);
    }

    /// The deprecated names are still on the wire in 0.0.57 and must round-trip.
    #[test]
    fn the_deprecated_thinking_events_still_parse() {
        let parsed: EventType = serde_json::from_str("\"THINKING_TEXT_MESSAGE_START\"").unwrap();
        assert_eq!(parsed, EventType::ThinkingTextMessageStart);
    }

    #[test]
    fn an_event_carries_its_own_fields() {
        let event = Event::new(EventType::TextMessageContent, 42)
            .with("messageId", "m1")
            .with("delta", "hello");
        let json: Value = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "TEXT_MESSAGE_CONTENT");
        assert_eq!(json["timestamp"], 42);
        assert_eq!(json["messageId"], "m1");
        assert_eq!(json["delta"], "hello");
    }

    /// The spec declares the base event schema `passthrough`, so an unknown field is conforming
    /// input, not an error — and it must survive us.
    #[test]
    fn an_unknown_field_on_an_event_round_trips() {
        let raw = r#"{"type":"CUSTOM","timestamp":7,"name":"x","value":{"deep":[1,2]},
            "aFieldFromNextRelease":true}"#;
        let event: Event = serde_json::from_str(raw).unwrap();
        let back: Value = serde_json::to_value(&event).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            back, original,
            "a passthrough event must round-trip unchanged"
        );
    }

    #[test]
    fn an_sse_frame_is_one_data_line_and_a_blank_line() {
        let frame = Event::new(EventType::RunStarted, 1)
            .with("threadId", "t1")
            .with("runId", "r1")
            .to_sse_frame()
            .unwrap();
        assert!(frame.starts_with("data: {"), "{frame}");
        assert!(frame.ends_with("\n\n"), "{frame:?}");
        // Exactly one frame: a newline inside the JSON would split it into two events.
        assert_eq!(frame.matches("data: ").count(), 1);
        assert!(!frame.trim_end().contains('\n'), "{frame:?}");
    }

    #[test]
    fn a_run_input_parses_with_only_its_required_fields() {
        let raw = r#"{"threadId":"t1","runId":"r1","messages":[]}"#;
        let input: RunAgentInput = serde_json::from_str(raw).unwrap();
        assert_eq!(input.thread_id, "t1");
        assert_eq!(input.run_id, "r1");
        assert!(input.messages.is_empty());
    }

    #[test]
    fn a_run_input_keeps_messages_and_unknown_roles() {
        let raw = r#"{"threadId":"t1","runId":"r1","messages":[
            {"id":"m1","role":"user","content":"hi"},
            {"id":"m2","role":"a-role-from-next-year","content":"?","extraField":1}]}"#;
        let input: RunAgentInput = serde_json::from_str(raw).unwrap();
        assert_eq!(input.messages.len(), 2);
        assert_eq!(input.messages[1].role, "a-role-from-next-year");
        let back = serde_json::to_string(&input.messages[1]).unwrap();
        assert!(back.contains("extraField"), "{back}");
    }
}
