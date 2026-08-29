//! Durable transcript entries, in the client's own vocabulary.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// The union the client stores and renders.
///
/// `#[serde(tag = "kind")]` matches how the client writes them. `Unknown` is deliberate: a newer
/// client emitting a kind we have not transcribed must survive a round trip, because dropping an
/// entry silently deletes somebody's message from their own history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EntryKind {
    /// What a person typed.
    Message {
        role: Role,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_nonce: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    /// What a coworker said: text, or one of the cards the client knows how to draw.
    SendMessage {
        message: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        responded_value: Option<Value>,
    },
    /// A file the person attached, which the client stores as its own entry kind.
    UserAttachment {
        file_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_name: Option<String>,
    },
    /// A system line in the timeline.
    Notice { text: String },
    /// Renames, connections, automation changes.
    Event { event: Value },
    /// Anything a newer client knows and we do not. Kept whole, re-emitted unchanged.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(rename = "timestampMs")]
    pub timestamp_ms: i64,
    #[serde(flatten)]
    pub kind: EntryKind,
}
