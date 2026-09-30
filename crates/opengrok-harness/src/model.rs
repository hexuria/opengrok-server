//! The model door, and what comes back through it.
//!
//! EVERY MODEL CALL EXITS THROUGH open-ai-gateway (CLAUDE.md #4), except a person's own-subscription
//! turn, which goes to their proxy on this server's loopback, or is relayed through their own Mac,
//! when they choose it (`ModelEndpoint`).
//! A coworker's pin (`xai/grok-4.6@sub`) is a route, not a key: the gateway holds the provider
//! credentials and we hold an `oag_live_` key that says who is asking; the proxy holds the person's
//! provider sign-in itself. Nothing in this crate ever sees a provider secret, and `ModelRequest`
//! deliberately has nowhere to put one.
//!
//! `ModelDelta` is provider-neutral on purpose. It is the vocabulary the projection consumes, so a
//! second door — a recorded fixture in a test, or a provider the gateway does not route — plugs in
//! without the AG-UI projection knowing anything changed.
//!
//! There was such a second door, over `rig-core`, and it was retired on 17 Sep 2026: rig did not
//! put the model on the wire, so the gateway saw a modelless request and answered on whatever rung
//! its classifier picked. The trait earned its keep anyway — removing that door touched this file
//! not at all, which is the property it exists to provide.

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

/// The credential ONE request goes out with when it is not the deployment's: a coworker's own
/// gateway key, so its spend lands on its own cap. `Unavailable` is the fail-closed half — the
/// coworker HAS a key of its own but it could not be produced (the vault, the row), and running
/// the turn on the deployment's key would step around the cap; the door refuses with the
/// reason instead. Redacted `Debug`, no `Serialize`: a request is journaled by its messages,
/// never by what opened the door.
///
/// `key_id` is the gateway's id for the key that was SENT, when the caller read one. A refusal is
/// booked against that id, not against whatever row is live by the time the refusal arrives: a
/// turn that set out on a key since re-minted must not flag the new one (#226).
#[derive(Clone, PartialEq, Eq)]
pub enum GatewayKey {
    Own { key: String, key_id: Option<String> },
    Unavailable(String),
}

impl GatewayKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self::Own {
            key: key.into(),
            key_id: None,
        }
    }

    pub fn with_id(key: impl Into<String>, key_id: impl Into<String>) -> Self {
        Self::Own {
            key: key.into(),
            key_id: Some(key_id.into()),
        }
    }

    /// The gateway's id for the key this request carries, when it is known.
    pub fn key_id(&self) -> Option<&str> {
        match self {
            Self::Own { key_id, .. } => key_id.as_deref(),
            Self::Unavailable(_) => None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable(reason.into())
    }
}

impl std::fmt::Debug for GatewayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Own { .. } => f.write_str("GatewayKey::Own(<redacted>)"),
            Self::Unavailable(reason) => write!(f, "GatewayKey::Unavailable({reason:?})"),
        }
    }
}

/// Where ONE request goes when it is not the gateway: a person's own subscription, through the
/// OpenAI-compatible proxy on this server's loopback (`local_proxy`), or through their own Mac
/// (`relay`). `Unavailable` is the fail-closed half, as it is for `GatewayKey`: the person chose
/// their own subscription and it cannot be used as set, so the door refuses with this sentence
/// rather than send the turn anywhere else — never quietly to the gateway, which would bill a key
/// they chose not to use, nor the other way to their subscription. `via` is the way it would have
/// gone, when that is known, for the turn's frame to say.
///
/// `auth` is the header the proxy reads its key from, and the key: the person's own, sealed in the
/// vault, never the gateway's. Redacted `Debug`, no `Serialize`, for the reason `GatewayKey` has.
#[derive(Clone, PartialEq, Eq)]
pub enum ModelEndpoint {
    Proxy {
        base_url: String,
        auth: Option<(String, String)>,
    },
    Relay(crate::relay::RelayTo),
    Unavailable {
        why: String,
        via: Option<opengrok_core::inference::Via>,
    },
}

impl ModelEndpoint {
    /// The way a turn on this endpoint goes, for its `opengrok.inferenceSource` frame.
    pub fn via(&self) -> Option<opengrok_core::inference::Via> {
        use opengrok_core::inference::Via;
        match self {
            Self::Proxy { .. } => Some(Via::Loopback),
            Self::Relay(_) => Some(Via::Mac),
            Self::Unavailable { via, .. } => *via,
        }
    }
}

impl std::fmt::Debug for ModelEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proxy { base_url, auth } => write!(
                f,
                "ModelEndpoint::Proxy({base_url}, key: {})",
                if auth.is_some() { "<redacted>" } else { "none" }
            ),
            Self::Relay(to) => write!(f, "ModelEndpoint::Relay({to:?})"),
            Self::Unavailable { why, .. } => write!(f, "ModelEndpoint::Unavailable({why:?})"),
        }
    }
}

/// What we ask the door for.
#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    /// A catalogue id the gateway understands (`xai/grok-4.6`, `oag/cheap`). A *route*, not a key.
    pub model: String,
    /// How hard the model thinks: the coworker's, captured when its run started. `Inherit` sends
    /// no `reasoning_effort`, so the route's own default applies.
    pub effort: opengrok_core::coworker::Effort,
    pub messages: Vec<ChatMessage>,
    pub system: Option<String>,
    /// The tools the model is OFFERED this turn, as OpenAI function-calling defs (`{type, function}`).
    /// Filled by the harness from the run's `ToolRunner` before each door call — the model cannot ask
    /// for a tool it was never told about, so an empty list here is why a bot says "I can't run
    /// commands" even with a computer attached. Empty when the run has no tools.
    pub tools: Vec<serde_json::Value>,
    /// The coworker's own gateway credential, when it has one (`spend caps`). `None` ⇒ the
    /// deployment's key, which is every request before caps and every request for a coworker
    /// that was never given a key.
    pub gateway_key: Option<GatewayKey>,
    /// Whose spend this request is, for a guard around the door that evaluates spend limits
    /// before each call: the coworker's id. The key alone does not say whose it is, and the
    /// harness does not know what a coworker is — it carries the scope, the server reads it.
    pub spend_scope: Option<String>,
    /// Whose spend it is: the account whose pool this turn draws on. A SECOND field rather than
    /// a pair packed into `spend_scope`, because the two genuinely differ — the scope is the
    /// coworker whose cap and key are counted, the actor is the person being billed — and a
    /// composite string would have to be split apart again by everything that reads either.
    /// On a coworker only its owner can reach they name the same person; on a shared one they
    /// do not, and the difference is the whole point of the field.
    pub spend_actor: Option<String>,
    /// How many tokens the model can read, prompt and answer together, as the server learned it
    /// from the gateway's catalogue or `OG_CONTEXT_TOKENS`. `None` ⇒ unknown, and the harness
    /// only counts. Never sent: the door builds its body field by field (#90).
    pub context_tokens: Option<u64>,
    /// Not the gateway: a person's own proxy, when their turn chose it. `None` is the gateway,
    /// which is every request but those. Filled where `gateway_key` is, and on a proxy turn
    /// `gateway_key` is `None` and `model` is the proxy's own id, never the coworker's pin.
    pub endpoint: Option<ModelEndpoint>,
}

/// One message of a conversation, in the OpenAI chat dialect the gateway speaks.
///
/// A TOOL ROUND IS TWO KINDS OF MESSAGE, NOT ONE. The assistant's own message names the calls it
/// made (`tool_calls`), and each result answers one of them by id (`role: "tool"`,
/// `tool_call_id`). Results used to come back as `user` lines keyed by ids the model had never
/// been shown, so it could not tell two parallel results apart or see that it had already run a
/// command (#189).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Pictures that go with the words: the screen after a `computer` action, which the model
    /// must see to act on it. Sent to the door as image parts; empty for a message of words only.
    /// On a `tool` message the gateway door moves them to a user message after the round, since
    /// the dialect carries no image in a tool result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePart>,
    /// The calls an assistant message made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRef>,
    /// The call a `tool` message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// A call as the assistant message that made it carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRef {
    pub id: String,
    pub name: String,
    /// The arguments as JSON text: the dialect carries them as a string, not an object.
    pub arguments: String,
}

impl ChatMessage {
    /// Words from `role`, and nothing else.
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            ..Self::default()
        }
    }

    /// The assistant's message for a round of calls, with what it said alongside them.
    pub fn calls(content: impl Into<String>, calls: Vec<ToolCallRef>) -> Self {
        Self {
            tool_calls: calls,
            ..Self::text("assistant", content)
        }
    }

    /// What a call returned.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            ..Self::text("tool", content)
        }
    }

    /// The message in words alone, for a reader that speaks no tool dialect: the mock door, and a
    /// provider that would refuse a result whose call it cannot see. A call reads as the line it
    /// made, a result as the line the loop has always written.
    pub fn as_text(&self) -> String {
        let mut text = self.content.clone();
        for call in &self.tool_calls {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "[called {} {} as {}]",
                call.name, call.arguments, call.id
            ));
        }
        match &self.tool_call_id {
            Some(id) => format!("[tool {id} result] {text}"),
            None => text,
        }
    }
}

/// One image in a message, base64 with its media type — what an `image_url` data URL needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
    pub mime: String,
    pub base64: String,
}

/// What went wrong at the door. `Display` is the detail, for the log; what a person is shown is
/// `sentence`.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The gateway could not be connected to, so nothing was sent and nothing was billed.
    #[error("the model gateway is unreachable: {0}")]
    Unreachable(String),
    #[error("the model gateway refused: {status} {body}")]
    Refused {
        status: u16,
        body: String,
        /// The gateway's `Retry-After`, in seconds, when it sent one (a 429 always does).
        retry_after_s: Option<u64>,
    },
    #[error("the stream broke: {0}")]
    Stream(String),
    /// A model call ran past the run's clock (`RunBudget`): it never started answering, or it
    /// went quiet. Already a sentence; nothing was billed for the silence.
    #[error("{0}")]
    TimedOut(String),
    /// A LIMIT SOMEBODY SET was reached: the gateway answered 402, or the points guard counted
    /// this coworker over its cap. Already a sentence a person can act on — it is what the
    /// transcript shows, and what `skills::from_tape` answers 402 with.
    #[error("{0}")]
    SpendCap(String),
    /// The guard could not COUNT this call, so it held it rather than let it through unmetered:
    /// a meter that would not answer, a key that could not be read, limits that could not be
    /// loaded, a deployment with no admin connection, a request our own code built without a
    /// payer.
    ///
    /// SEPARATE FROM `SpendCap`, AND THE DIFFERENCE IS NOT COSMETIC. Nobody has spent anything,
    /// the condition is usually transient, and it is frequently OUR fault rather than the
    /// person's — so a route turning one into an HTTP status must not tell somebody to check
    /// their billing because Postgres blinked for two seconds. These sentences also carry store
    /// errors, which belong in a log rather than in a reply. Both print the same way, so a
    /// transcript reads exactly as it did when there was one variant.
    #[error("{0}")]
    Held(String),
    /// A person's own proxy refused the call or could not be reached, or their source cannot be
    /// used as set. Already a sentence, and one that names the proxy: the gateway's sentences
    /// would send somebody whose proxy is not running off to check a gateway key.
    #[error("{0}")]
    Proxy(String),
    /// The person's own Mac could not carry the call (`relay`): none is connected, it went quiet,
    /// or its plan refused. Already a sentence; `code` rides the run's `RUN_ERROR` beside it
    /// (`relay_offline`, `relay_timeout`, `relay_failed`), so a client can say which.
    #[error("{sentence}")]
    Relay {
        code: &'static str,
        sentence: String,
    },
}

impl ModelError {
    /// The code a `RUN_ERROR` carries beside the sentence, when the failure has one.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Relay { code, .. } => Some(code),
            _ => None,
        }
    }

    /// The sentence a person is shown when a turn ends on this error.
    ///
    /// NEVER THE GATEWAY'S BODY. A 429, a 503 or a context overflow reached the chat as
    /// `the model gateway refused: 429 {"type":"error",…}` with up to 500 characters of JSON, and
    /// an unreachable gateway printed the reqwest error with the internal gateway URL in it
    /// (#185). The envelope's `error.type` picks the sentence
    /// (`gateway-open-ai-gateway.md` §5); only a request the gateway could not take keeps the
    /// gateway's own `error.message`, bounded, because that message is the fix (a context window,
    /// a model id). `Display` keeps the detail for the log.
    pub fn sentence(&self) -> String {
        match self {
            Self::Unreachable(_) => {
                "The model gateway could not be reached, so no model was asked. \
                 Try again in a moment."
                    .to_string()
            }
            Self::Refused {
                status,
                body,
                retry_after_s,
            } => refused_sentence(*status, body, *retry_after_s),
            Self::Stream(detail) => format!("The model's answer broke off: {}", bounded(detail)),
            Self::TimedOut(sentence)
            | Self::SpendCap(sentence)
            | Self::Held(sentence)
            | Self::Proxy(sentence)
            | Self::Relay { sentence, .. } => sentence.clone(),
        }
    }

    /// How long to wait before asking the door again, when asking again cannot bill twice and
    /// may well work: a connection that was refused (a gateway restarting), or a 429 / busy 503
    /// whose `Retry-After` is short. `attempt` counts the retries already made.
    ///
    /// NOTHING AFTER A REPLY STARTED. This is asked only of a door that did not open, so no
    /// model has answered and nothing has been shown. A refusal that is about the request (400)
    /// or the key (401) is not retried: it would get the same answer. A key the coworker owns
    /// is retried once on the deployment's key by the spend guard, which is a different question.
    pub fn retry_wait(&self, attempt: u32) -> Option<std::time::Duration> {
        use std::time::Duration;
        match self {
            // Once, as #185 asks: each try against a black-holed gateway costs the connect
            // timeout, and two retries made a dead gateway take half a minute to say so.
            Self::Unreachable(_) if attempt == 0 => Some(Duration::from_secs(1)),
            Self::Refused {
                status,
                body,
                retry_after_s: Some(seconds),
            } if attempt == 0
                && *seconds <= MAX_RETRY_AFTER_S
                && (*status == 429 || (*status == 503 && error_kind(body) == "at_capacity")) =>
            {
                Some(Duration::from_secs(*seconds))
            }
            _ => None,
        }
    }
}

/// The longest `Retry-After` worth waiting for inside a turn. Longer, and the person is better
/// told now than kept watching a spinner.
const MAX_RETRY_AFTER_S: u64 = 5;

/// `error.type` from the gateway's envelope, or `""`.
fn error_kind(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"]["type"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The gateway's own words for why, when they are words: `error.message` from the envelope, or
/// a short plain-text body. Never JSON and never a page of HTML. `detail` too, which is how the
/// ChatGPT backend behind a person's proxy says it ("…not supported when using Codex with a
/// ChatGPT account").
pub(crate) fn gateway_message(body: &str) -> Option<String> {
    let trimmed = body.trim();
    let message = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value) => value["error"]["message"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .or_else(|| value["message"].as_str())
            .or_else(|| value["detail"].as_str())
            .map(str::to_string)?,
        Err(_) if trimmed.starts_with('<') || trimmed.starts_with('{') => return None,
        Err(_) => trimmed.to_string(),
    };
    Some(bounded(&message)).filter(|message| !message.is_empty())
}

fn refused_sentence(status: u16, body: &str, retry_after_s: Option<u64>) -> String {
    let again = match retry_after_s {
        Some(seconds) => format!("Try again in {seconds} seconds."),
        None => "Try again in a moment.".to_string(),
    };
    match (status, error_kind(body).as_str()) {
        (401 | 403, _) => {
            "The model gateway did not accept the key this turn was sent with, so no \
             model was asked. Whoever runs this server needs to check its gateway key."
                .to_string()
        }
        (429, _) => format!("The model's provider is limiting how often it can be asked. {again}"),
        (503, "at_capacity") => format!("Every credential for this model is busy. {again}"),
        (503, "no_credential" | "no_credential_of_kind") => "There is no provider credential for \
             this model's route, so no model was asked. Pin the coworker to another model, or add \
             a credential for this one."
            .to_string(),
        (503, "quota_reserve_held") => "This model's remaining quota is held back for other work. \
             Try again later, or pin the coworker to another model."
            .to_string(),
        (504, _) => "The model stopped answering before it finished.".to_string(),
        (400..=499, _) => match gateway_message(body) {
            Some(message) => format!("The model gateway could not take this request: {message}"),
            None => format!("The model gateway could not take this request ({status})."),
        },
        (500..=599, _) => format!("The model gateway failed on its side ({status}). {again}"),
        _ => format!("The model gateway refused this turn ({status})."),
    }
}

/// One upstream sentence, on one line, short enough to read.
pub(crate) fn bounded(message: &str) -> String {
    let flat = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 200 {
        let kept: String = flat.chars().take(200).collect();
        format!("{kept}…")
    } else {
        flat
    }
}

pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send>>;

/// A way to reach a model. One implementation today; the seam exists so a test can hand the
/// harness a scripted stream and assert on what the client would have seen.
#[async_trait::async_trait]
pub trait ModelDoor: Send + Sync {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError>;

    /// Whether what is behind this door would take a request, asked without billing one. `None`
    /// for a door with nothing behind it to ask (a mock). A door that wraps another forwards it.
    async fn ready(&self) -> Option<Result<(), ModelError>> {
        None
    }
}

#[cfg(test)]
#[path = "../tests/unit/model_tests.rs"]
mod tests;
