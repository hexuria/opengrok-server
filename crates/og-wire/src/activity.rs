//! The live stream: what a coworker is doing right now.
//!
//! Provenance: `grok-bot/source/host/sand-activity.ts:3,6,7,8` and
//! `docs/research/client-grok-bot.md` §4.1.
//!
//! NOT THE TRANSCRIPT. Nothing here is durable, and a client that missed an update must be able to
//! catch up from the transcript alone. Two distinct types live here and conflating them was a bug
//! this file already had once:
//!   - `ActivityUpdate` is the PRODUCER event, tagged on `type`;
//!   - `AgentActivity` is the REDUCED state a roster row shows, tagged on `kind`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What the producer emits while a turn runs.
///
/// An OPEN UNION on the client (`sand-activity.ts:8` ends with `{ type: string; [k]: unknown }`),
/// so `Other` is not defensive padding — it is the declared shape, and dropping it would make us
/// reject events a newer client considers ordinary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ActivityUpdate {
    ThinkingDelta,
    TextDelta {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    SendMessage,
    TurnEnded,
    /// Anything else the producer sends. Carried whole.
    #[serde(untagged)]
    Other(Value),
}

/// The reduced state a roster row draws: idle (absent), thinking, or in a named tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AgentActivity {
    Thinking,
    Tool {
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(rename = "callId")]
        call_id: String,
    },
}

/// What a reduction step says to do with the row's current activity
/// (`sand-activity.ts:7` — `ActivityTransition`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ActivityTransition {
    Keep,
    Clear,
    Set { activity: AgentActivity },
}

/// One frame of the SSE stream, kept loose.
///
/// The client's event stream carries eighteen channels; naming them all here before a slice needs
/// them would be inventing a contract rather than transcribing one. The channel and its payload
/// are carried; §5 of the reference has the inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamFrame {
    pub channel: String,
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_update_is_tagged_on_type_not_kind() {
        let raw = r#"{"type":"tool-call","id":"c1","name":"shell","status":"running"}"#;
        let update: ActivityUpdate = serde_json::from_str(raw).unwrap();
        assert!(matches!(update, ActivityUpdate::ToolCall { .. }));
        let back = serde_json::to_string(&update).unwrap();
        assert!(back.contains("\"type\":\"tool-call\""), "{back}");
    }

    #[test]
    fn an_unknown_update_type_is_carried_not_rejected() {
        let raw = r#"{"type":"something-new","detail":{"a":1}}"#;
        let update: ActivityUpdate = serde_json::from_str(raw).unwrap();
        assert!(matches!(update, ActivityUpdate::Other(_)));
    }

    #[test]
    fn a_reduced_activity_is_tagged_on_kind() {
        let raw = r#"{"kind":"tool","tool":"shell","callId":"c1"}"#;
        let activity: AgentActivity = serde_json::from_str(raw).unwrap();
        let back = serde_json::to_string(&activity).unwrap();
        assert!(back.contains("\"callId\":\"c1\""), "{back}");
    }
}
