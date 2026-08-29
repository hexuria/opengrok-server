//! The host's JSON commands.
//!
//! The client sends a command name and a JSON body; every response is JSON. We transcribe the
//! names it actually calls and keep the bodies as `Value` at this layer — the shape of
//! `sendPrompt`'s body belongs to the handler that serves it, not to the envelope.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One call from the client. `name` is the command; `body` is its parsed JSON arguments.
#[derive(Debug, Clone, Deserialize)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub body: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CommandResponse {
    Ok(Value),
    Err(CommandError),
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandError {
    pub error: String,
    /// Said in the client's own words where possible: it renders this to a person.
    pub message: String,
}

/// The commands the first slice answers. The client calls roughly seventy; a command we have not
/// implemented must fail loudly with `unimplemented` rather than silently returning an empty
/// success, which the client renders as "you have no coworkers".
pub const IMPLEMENTED: &[&str] = &[
    "listAgents",
    "createAgent",
    "getAgentTranscriptTail",
    "openAgentTail",
    "sendPrompt",
];
