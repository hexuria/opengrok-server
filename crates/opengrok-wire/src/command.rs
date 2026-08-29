//! The host's JSON commands.
//!
//! Provenance: `opengrok/source/host/gateway-protocol.ts:5-127` declares **123** commands;
//! `source/shared/rpc/coordinator.ts:92-183` exposes **90** of them to the renderer, leaving 33
//! reachable only from the host. Full inventory and per-command shapes:
//! `docs/research/client-grok-bot.md` §2.
//!
//! The surface is wider than these commands: the client also needs `GET /health`,
//! `GET /events` (SSE), and `/avatars/<id>`. Those are routes, not commands, and live in og-server.

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

/// The commands P1 answers, in the order the client first calls them.
///
/// AN EMPTY SUCCESS IS THE DANGEROUS REPLY, NOT AN ERROR. `listAgents` returning `[]` is a *valid*
/// answer: the renderer sets `rosterLoadFailed = false` and paints an empty sidebar, so the person
/// sees a working app with no coworkers and blames us
/// (`ProductionRenderer.tsx:2185-2193`; reference Trap 2). During bring-up, seed one coworker and
/// return it. The shape of the reply matters just as much: `countAgents` must be a NUMBER or the
/// app shows onboarding instead of the roster, and `getTrays` must be an ARRAY or the renderer
/// throws outright.
pub const P1_COMMANDS: &[&str] = &[
    "listAgents",
    "countAgents",
    "getTrays",
    "isAgentNetworkEnabled",
    "isGlobalSearchEnabled",
    "getHostSettings",
    "setHostSettings",
    "getForeverBoxStatus",
    "getSharingState",
    "setWindowFocused",
    "openAgentTail",
];

/// P2 adds the ability to say something and be answered.
pub const P2_COMMANDS: &[&str] = &["sendPrompt", "createAgent", "getAgentTranscriptTail"];
