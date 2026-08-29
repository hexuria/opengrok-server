//! The live stream: what a coworker is doing right now.
//!
//! The client draws eyes, a thinking row and tool lines from this. It is NOT the transcript —
//! nothing here is durable, and a client that missed an update must be able to catch up from the
//! transcript alone.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ActivityUpdate {
    ThinkingDelta { text: String },
    TextDelta { text: String },
    ToolCall {
        id: String,
        name: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

/// What the roster shows beside a coworker: idle, or the thing it is in the middle of.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    #[serde(rename = "isRunning")]
    pub is_running: bool,
    #[serde(rename = "currentActivity", skip_serializing_if = "Option::is_none")]
    pub current: Option<ActivityUpdate>,
}
