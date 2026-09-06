//! The fixture catalogue, ABSENT — what `gateway::mock_fixtures` is when the `mock-fixtures`
//! feature is off, which is every default and every release build.
//!
//! WHY A STUB AND NOT A `#[cfg]` AT EVERY CALL SITE. The catalogue is reached from five places
//! across three files (the coworker's turn, a room member's turn, and the two attachment read
//! verbs). Gating each one would put five conditionals into code whose subject is not mocking,
//! and the first person to add a sixth call site would not know to add a sixth `#[cfg]` — it
//! would compile fine with the feature on and break the release build. One switch in `mod.rs`
//! and a stub that answers honestly cannot drift that way: if it compiles with the feature, it
//! compiles without it.
//!
//! WHAT IS ACTUALLY GONE. The real module carries ~65 KB of `include_bytes!` fixtures (a video,
//! an mp3, a pptx, a pdf…), the entry builders, and `read_fixture`, which serves bytes off the
//! filesystem. None of it is compiled here, so a production binary cannot be talked into serving
//! any of it by an environment variable — which was the whole point: the runtime door check in
//! `enabled()` fails closed, but it is one `OG_MODEL_DOOR=mock-cards` deep, and a capability that
//! is merely switched off is not the same as one that is not present.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Never. The callers all guard on this, so with the feature off the fixture tool is never
/// offered, never drained, and `entries_for` does not exist to be called.
pub fn enabled() -> bool {
    false
}

/// Unreachable in practice — `enabled()` is false, and every caller checks it first — but it has
/// to typecheck. A no-op tool that refuses is the honest stand-in for one that is not here.
pub fn tool() -> (opengrok_harness::LocalTool, Arc<Mutex<Vec<Value>>>) {
    let sink: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let handler: opengrok_harness::LocalTool =
        Arc::new(
            move |call: &opengrok_tools::ToolCall| opengrok_tools::ToolResult {
                call_id: call.id.clone(),
                ok: false,
                content: "this server was built without the mock fixture catalogue".to_string(),
                awaiting_approval: false,
                awaiting_reason: None,
            },
        );
    (handler, sink)
}

/// The same shape the real schema has, so a caller that offers it is still well-formed. It is
/// never offered: `enabled()` gates every call.
pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "mock_fixture",
            "description": "Not built into this server.",
            "parameters": { "type": "object", "properties": {} }
        }
    })
}

/// Nothing was recorded, so there is nothing to append.
pub async fn drain_into(
    _state: &super::GatewayState,
    _coworker: &opengrok_core::id::CoworkerId,
    _account: &opengrok_core::id::AccountId,
    _sink: &Arc<Mutex<Vec<Value>>>,
    _at_ms: i64,
) {
}

/// THE REFUSAL THE ATTACHMENT VERBS FALL BACK TO, word for word what they answered before the
/// catalogue existed — `uploadAttachment` and `readAttachmentImage` still answer it in every
/// build, so a server built without the feature gives one consistent answer across all four
/// rather than two different stories about the same missing slice.
pub fn read_fixture(_path: &str) -> Result<Vec<u8>, String> {
    Err("attachments are not stored by this server yet (artifacts is a planned slice)".to_string())
}
