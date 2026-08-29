//! Durable transcript entries, in the client's own vocabulary.
//!
//! Provenance: `opengrok/source/shared/transcript.ts:21-22` (the threadable kinds),
//! `source/host/extensions/transcript/send-message-shaping.ts:77-160` (the constructors), and
//! `docs/research/client-grok-bot.md` §3.1 (the field tables). Field names are the client's own —
//! see the note on casing below, which is why this file looks inconsistent.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Fields we did not name, kept so they survive a round trip.
///
/// A shape transcribed today must not silently amputate a field a newer client adds tomorrow.
/// Every variant carries one of these.
pub type Extra = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// THE CLIENT'S CASING IS NOT CONSISTENT, AND NEITHER IS THIS.
///
/// `kind: "message"` carries `clientNonce`, `replyTo`, `timestampMs` — camelCase. But
/// `kind: "user-attachment"` carries `file_path` and `file_name` — snake_case, on the same wire, in
/// the same transcript. That is what `send-message-shaping.ts` writes, so that is what we read and
/// emit. Normalising either one to match the other is exactly the tidying that breaks a client we
/// do not compile, and `clientNonce` is the field the renderer matches to settle an optimistic
/// bubble: get it wrong and the person's own message hangs pending forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum KnownKind {
    /// What a person said, or a coworker's plain utterance.
    Message {
        role: Role,
        content: String,
        #[serde(default, rename = "richText", skip_serializing_if = "Option::is_none")]
        rich_text: Option<Value>,
        #[serde(
            default,
            rename = "isStreaming",
            skip_serializing_if = "Option::is_none"
        )]
        is_streaming: Option<bool>,
        #[serde(
            default,
            rename = "clientNonce",
            skip_serializing_if = "Option::is_none"
        )]
        client_nonce: Option<String>,
        #[serde(default, rename = "replyTo", skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(default, rename = "batchId", skip_serializing_if = "Option::is_none")]
        batch_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branched: Option<bool>,
        /// An agent-to-agent hop. An outbound hop whose `toAgent.kind != "agent"` is HIDDEN by the
        /// client, and peer messages must not raise the unread pip (`transcript.ts:38-41`).
        #[serde(default, rename = "fromAgent", skip_serializing_if = "Option::is_none")]
        from_agent: Option<Value>,
        #[serde(default, rename = "toAgent", skip_serializing_if = "Option::is_none")]
        to_agent: Option<Value>,
        #[serde(flatten)]
        extra: Extra,
    },

    /// Every assistant utterance and every card. The card union lives inside `message.type` and is
    /// deliberately a `Value` here: twelve card types with their own validation rules belong to the
    /// handler that renders or produces them, not to the envelope. Reference §3.2.
    SendMessage {
        message: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming: Option<bool>,
        #[serde(default, rename = "replyTo", skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(
            default,
            rename = "respondedValue",
            skip_serializing_if = "Option::is_none"
        )]
        responded_value: Option<Value>,
        #[serde(
            default,
            rename = "widgetDismissed",
            skip_serializing_if = "Option::is_none"
        )]
        widget_dismissed: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },

    /// A file on a person's row. `file_path`/`file_name` really are snake_case — see above.
    UserAttachment {
        file_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<i64>,
        #[serde(default, rename = "byteSize", skip_serializing_if = "Option::is_none")]
        byte_size: Option<i64>,
        #[serde(
            default,
            rename = "clientNonce",
            skip_serializing_if = "Option::is_none"
        )]
        client_nonce: Option<String>,
        #[serde(default, rename = "replyTo", skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },

    /// A muted system line in the timeline.
    Notice {
        text: String,
        #[serde(flatten)]
        extra: Extra,
    },

    /// Renames, channel connections, automation changes
    /// (`source/shared/sand-timeline-events.ts:9-18`).
    Event {
        event: Value,
        #[serde(flatten)]
        extra: Extra,
    },

    /// The tool line. Also seen spelled `toolCall` and `tool`; those arrive as `Unknown` and
    /// round-trip, which is correct until a slice actually needs to read them.
    ToolCall {
        name: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
        #[serde(flatten)]
        extra: Extra,
    },
}

/// A kind we transcribed, or one we have not — kept whole either way.
///
/// `#[serde(other)]` was the obvious spelling and it is a trap: it fits only a UNIT variant, so it
/// matches an unrecognised `kind` and then throws the entire payload away. An untagged fallback to
/// a raw `Value` is what actually honours "an entry kind we do not recognise is preserved and
/// re-emitted" — the invariant exists because dropping an entry deletes somebody's message from
/// their own history, and a client newer than this file must never be able to cause that.
// clippy asks us to box `Known` because it dwarfs `Unknown`. Not taken: `Known` is the
// overwhelmingly common variant — nearly every entry in a transcript is one — so boxing would add
// an allocation to the hot path to save bytes on the rare one, and every match site would grow a
// deref for it.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EntryKind {
    Known(KnownKind),
    Unknown(Value),
}

/// One entry as the client stores it.
///
/// `id` and `timestampMs` sit on every kind, so they live here and the kind flattens over them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(rename = "timestampMs")]
    pub timestamp_ms: i64,
    #[serde(flatten)]
    pub kind: EntryKind,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The casing contract, asserted rather than hoped for.
    #[test]
    fn a_user_message_keeps_the_clients_field_names() {
        let raw = r#"{"id":"e1","timestampMs":1,"kind":"message","role":"user",
            "content":"hi","clientNonce":"n1","replyTo":"e0","isStreaming":false}"#;
        let entry: Entry = serde_json::from_str(raw).unwrap();
        let back = serde_json::to_string(&entry).unwrap();
        assert!(back.contains("\"clientNonce\":\"n1\""), "{back}");
        assert!(back.contains("\"replyTo\":\"e0\""), "{back}");
        assert!(back.contains("\"timestampMs\":1"), "{back}");
    }

    /// An attachment's snake_case is the client's, and stays.
    #[test]
    fn an_attachment_keeps_snake_case_where_the_client_uses_it() {
        let raw = r#"{"id":"e2","timestampMs":2,"kind":"user-attachment",
            "file_path":"/tmp/a.png","file_name":"a.png","byteSize":9}"#;
        let entry: Entry = serde_json::from_str(raw).unwrap();
        let back = serde_json::to_string(&entry).unwrap();
        assert!(back.contains("\"file_path\""), "{back}");
        assert!(back.contains("\"byteSize\":9"), "{back}");
    }

    /// The invariant that stops us deleting somebody's history.
    #[test]
    fn an_unknown_kind_survives_whole() {
        let raw = r#"{"id":"e3","timestampMs":3,"kind":"some-future-kind",
            "payload":{"deep":[1,2,3]},"note":"keep me"}"#;
        let entry: Entry = serde_json::from_str(raw).unwrap();
        assert!(matches!(entry.kind, EntryKind::Unknown(_)));
        let back: Value = serde_json::to_value(&entry).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(back, original, "an unknown entry must round-trip unchanged");
    }

    /// An unknown FIELD on a known kind survives too — same reason, one level down.
    #[test]
    fn an_unknown_field_on_a_known_kind_survives() {
        let raw = r#"{"id":"e4","timestampMs":4,"kind":"notice","text":"hi",
            "aFieldFromNextYear":true}"#;
        let entry: Entry = serde_json::from_str(raw).unwrap();
        let back = serde_json::to_string(&entry).unwrap();
        assert!(back.contains("aFieldFromNextYear"), "{back}");
    }
}
