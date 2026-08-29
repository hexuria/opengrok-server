//! The contract the desktop client already speaks.
//!
//! THIS CRATE IS A TRANSCRIPTION, NOT A DESIGN. Every shape here exists because the Grok Bot
//! host sends or expects it; the field names are its field names. Changing one to something
//! tidier breaks a client we do not compile. Where the client is loose (`kind` unions carrying
//! extra keys), we keep the loose shape and preserve unknown fields rather than narrowing them,
//! so a newer client than the one we tested against degrades to "rendered as-is" instead of
//! "rejected".
//!
//! Four wires, kept apart on purpose:
//!   1. `command` — the host's JSON request/response calls (listAgents, sendPrompt, …).
//!   2. `transcript` — the durable entries a conversation is made of.
//!   3. `activity` — the live stream while a coworker is working.
//!   4. `agui` — the AG-UI protocol openbot speaks. A published spec, not a reconstruction; see
//!      that module's note on why its rules differ from the three above.
//!
//! Provenance: shapes derived from the client's own recovered surface in
//! `opengrok/source/host/gateway-protocol.ts`, `source/shared/transcript.ts` and
//! `docs/grok-0.27-disparity-proto.md`. Interop only: none of Cursor's server code is used.

pub mod activity;
pub mod agui;
pub mod command;
pub mod transcript;

pub use activity::{ActivityTransition, ActivityUpdate, AgentActivity, StreamFrame};
pub use agui::{
    Event as AgUiEvent, EventType as AgUiEventType, Message as AgUiMessage, RunAgentInput,
};
pub use command::{Command, CommandError, CommandResponse, P1_COMMANDS, P2_COMMANDS};
pub use transcript::{Entry, EntryKind, Extra, KnownKind, Role};
