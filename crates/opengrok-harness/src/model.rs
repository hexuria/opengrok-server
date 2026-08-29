//! The model door, and what comes back through it.
//!
//! EVERY MODEL CALL EXITS THROUGH open-ai-gateway (CLAUDE.md #4). A coworker's pin
//! (`xai/grok-4.6@sub`) is a route, not a key: the gateway holds the provider credentials and we
//! hold an `oag_live_` key that says who is asking. Nothing in this crate ever sees a provider
//! secret, and `ModelRequest` deliberately has nowhere to put one.
//!
//! `ModelDelta` is provider-neutral on purpose. It is the vocabulary the projection consumes, so a
//! second door — Rig's abstraction over many providers, a recorded fixture in a test — plugs in
//! without the AG-UI projection knowing anything changed.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};

/// One thing a model said, in the smallest useful piece.
///
/// Tool calls arrive in three parts because that is how every streaming provider sends them: a
/// name up front, arguments in fragments, and a close. Collapsing them into one "tool call" event
/// would mean buffering the whole call before showing anything, which is the latency the streaming
/// was for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelDelta {
    /// A fragment of the assistant's reply.
    Text(String),
    /// A fragment of the model's reasoning, where a provider exposes it.
    Reasoning(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    /// A fragment of the JSON arguments for `id`.
    ToolCallArgs {
        id: String,
        delta: String,
    },
    ToolCallEnd {
        id: String,
    },
}

/// What we ask the door for.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    /// A catalogue id the gateway understands (`xai/grok-4.6`, `oag/cheap`). A *route*, not a key.
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub system: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("the model gateway is unreachable: {0}")]
    Unreachable(String),
    #[error("the model gateway refused: {status} {body}")]
    Refused { status: u16, body: String },
    #[error("the stream broke: {0}")]
    Stream(String),
}

pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send>>;

/// A way to reach a model. One implementation today; the seam exists so a test can hand the
/// harness a scripted stream and assert on what the client would have seen.
#[async_trait::async_trait]
pub trait ModelDoor: Send + Sync {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError>;
}
