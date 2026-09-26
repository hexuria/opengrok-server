//! The real door: open-ai-gateway's OpenAI-compatible streaming endpoint.
//!
//! `POST /v1/chat/completions` with `stream: true`, authenticated with an `oag_live_` key.
//! Bearer wins if several key headers are sent (`gateway-open-ai-gateway.md` §:167), so Bearer is
//! what we send and the only thing we send.
//!
//! THE KEY IS OURS, NOT A PROVIDER'S. It says who is asking; the gateway holds the provider
//! credentials and picks the cheapest live one for the route. A provider secret must never appear
//! in this crate, in a coworker's row, in a client payload, or in a log (CLAUDE.md #4) — which is
//! also why `Debug` here is hand-written.
//!
//! ON PARSING SSE BY HAND: the wire format is `data: {json}\n\n` with a literal `data: [DONE]`
//! sentinel, and the fragments that matter are three fields deep. A streaming JSON framework would
//! be more machinery than the twenty lines below, and the shape is fixed by the OpenAI dialect the
//! gateway already speaks.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures::{StreamExt, stream};
use serde::Deserialize;

use crate::model::{ChatMessage, DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};

/// How much of a refused key reaches the log: `oag_live_` plus seven characters, which is exactly
/// what the gateway stores as `api_key.key_prefix` and therefore exactly what identifies the row.
/// Shorter would not name a key; longer would start handing out the secret for no extra answer.
const KEY_PREFIX_LEN: usize = 16;

/// The part of a credential that may be written down. Pure, so the rule is tested without a
/// socket — the same reason `parse_line` below is pure.
///
/// `chars()` rather than a byte slice: a key is opaque to us, and a byte index that lands inside a
/// multi-byte character panics. Losing a turn to a logging call would be an unusually poor trade.
fn logged_prefix(key: &str) -> String {
    key.chars().take(KEY_PREFIX_LEN).collect()
}

pub struct GatewayDoor {
    base_url: String,
    key: String,
    /// `Err` holds why the client could not be built. There is no clockless fallback:
    /// `reqwest::Client::new()` panics on the same TLS or resolver failure the builder reports,
    /// so the door says the gateway cannot be reached, and says it on every call.
    http: Result<reqwest::Client, String>,
    /// The last readiness answer and when it was had. See `READY_FOR`.
    ready_seen: Mutex<Option<(std::time::Instant, Probed)>>,
}

/// How long one readiness answer about the gateway stands.
///
/// `/ready` is unauthenticated, and every call to it was a `GET /v1/models` on the gateway: a
/// prober asking in a tight loop drove the gateway's catalogue at the prober's rate. Five
/// seconds is shorter than any supervisor's interval, so a revoked token still reads false on
/// the next probe that matters.
const READY_FOR: std::time::Duration = std::time::Duration::from_secs(5);

/// A probe's outcome, kept so it can be answered again. `ModelError` is not `Clone`.
enum Probed {
    Ok,
    Refused(u16),
    Unreachable(String),
    TimedOut(String),
    Other(String),
}

impl Probed {
    fn of(probed: &Result<(), ModelError>) -> Self {
        match probed {
            Ok(()) => Self::Ok,
            Err(ModelError::Refused { status, .. }) => Self::Refused(*status),
            Err(ModelError::Unreachable(detail)) => Self::Unreachable(detail.clone()),
            Err(ModelError::TimedOut(sentence)) => Self::TimedOut(sentence.clone()),
            Err(other) => Self::Other(other.to_string()),
        }
    }

    fn again(&self) -> Result<(), ModelError> {
        match self {
            Self::Ok => Ok(()),
            Self::Refused(status) => Err(ModelError::Refused {
                status: *status,
                body: String::new(),
                retry_after_s: None,
            }),
            Self::Unreachable(detail) => Err(ModelError::Unreachable(detail.clone())),
            Self::TimedOut(sentence) => Err(ModelError::TimedOut(sentence.clone())),
            Self::Other(detail) => Err(ModelError::Stream(detail.clone())),
        }
    }
}

impl std::fmt::Debug for GatewayDoor {
    /// Hand-written so the key cannot reach a log through a derived `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayDoor")
            .field("base_url", &self.base_url)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// How long a connection to the gateway may take. The gateway is on the same network; ten
/// seconds is a gateway that is not there.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long the gateway may send nothing at all, headers or body.
///
/// A READ TIMEOUT, NOT A TOTAL ONE: a total timeout would cut a long, healthy stream off at its
/// deadline. The gateway sends a keep-alive every 10 s and ends an idle stream itself at 180 s
/// with a `stream_idle` 504 (`gateway-open-ai-gateway.md` §2), so silence past this is a gateway
/// that cannot answer, and the gateway's own sentence wins whenever it can still give one.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(200);

/// How long listing the catalogue may take before the gateway counts as not answering.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

impl GatewayDoor {
    pub fn new(base_url: impl Into<String>, key: impl Into<String>) -> Self {
        Self::with_timeouts(base_url, key, CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// `new`, with its two clocks set by the caller.
    ///
    /// Before these existed the client had none (`reqwest::Client::new()`), so a gateway that
    /// accepted the connection and never answered held the run for as long as the process
    /// lived, its lease renewed the whole time (#93).
    pub fn with_timeouts(
        base_url: impl Into<String>,
        key: impl Into<String>,
        connect: std::time::Duration,
        read: std::time::Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(connect)
            .read_timeout(read)
            .build()
            .map_err(|error| {
                tracing::error!(%error, "the gateway client could not be built");
                error.to_string()
            });
        Self {
            base_url: base_url.into(),
            key: key.into(),
            http,
            ready_seen: Mutex::new(None),
        }
    }

    /// The client, or the refusal every call gets when it could not be built. Nothing was sent,
    /// so it is `Unreachable`; the person reads the unreachable sentence, the log the reason.
    fn client(&self) -> Result<&reqwest::Client, ModelError> {
        self.http.as_ref().map_err(|why| {
            ModelError::Unreachable(format!("the gateway client could not start: {why}"))
        })
    }

    /// Ask the gateway whether it will take this door's key, without asking a model anything:
    /// `GET /v1/models`, which lists the catalogue and bills nothing.
    ///
    /// For boot and for `/ready`. `/health` answers for the event store and stays that way — it
    /// is the supervisor's liveness check, and a gateway outage must not restart this server — so
    /// a wrong OG_GATEWAY_TOKEN used to report ok:true until the first turn failed (#185).
    ///
    /// ITS OWN CLOCK, `PROBE_TIMEOUT`. On the door's 200 s read timeout a gateway that accepted
    /// and hung held the boot, and would hold every readiness check, for as long.
    pub async fn probe(&self) -> Result<(), ModelError> {
        let response = self
            .client()?
            .get(format!("{}/v1/models", self.base_url))
            .bearer_auth(&self.key)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(send_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(ModelError::Refused {
            status: status.as_u16(),
            body: String::new(),
            retry_after_s: None,
        })
    }
}

/// A request that did not come back, as the door reports it. The URL is dropped: it is the
/// internal gateway's address, and this text used to reach the chat whole (#185).
///
/// ONLY A FAILED CONNECT IS `Unreachable`, because only then is it certain nothing was sent —
/// which is what lets the loop ask again without billing twice. A request that went out and
/// then lost its connection may have reached a model, so it is not retried.
fn send_error(error: reqwest::Error) -> ModelError {
    let connect = error.is_connect();
    let timeout = error.is_timeout();
    let detail = error.without_url().to_string();
    if connect {
        ModelError::Unreachable(detail)
    } else if timeout {
        tracing::warn!(%detail, "the model gateway timed out");
        ModelError::TimedOut("the model gateway did not answer in time".to_string())
    } else {
        ModelError::Stream(detail)
    }
}

/// The seconds in a `Retry-After`. The HTTP-date form is not read: the gateway sends seconds.
fn retry_after_s(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The gateway's 402 names the scope in its own words ("the quota on this API key is
/// exhausted", "the monthly budget for this principal is exhausted"); the sentence a person reads
/// keeps those words and says what to do about them. A key cap does not reset — it is a wall at
/// the number written on it — so "raise it" is the only way through that one.
/// The 402 the gateway sent, as a sentence a person can act on.
///
/// THE UPSTREAM DETAIL IS BOUNDED AND FLATTENED, because this string is user-facing twice over:
/// it is what a transcript shows, and `skills::from_tape` answers 402 with it. `error.message` is
/// chosen by the far side — a gateway having a bad day answers with an HTML page, a stack trace
/// or an internal identifier, and all of it used to travel whole. `Refused` below has been
/// bounded to 500 characters since the day it was written; this is the one people read.
fn spend_cap_sentence(body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(bounded))
        .unwrap_or_else(|| "a spend cap is reached".to_string());
    format!(
        "This coworker cannot take a turn: {detail}. Raise its cap in the console (a key's cap \
         does not reset), or wait for a monthly budget to reset."
    )
}

use crate::model::bounded;

/// One `data:` frame of an OpenAI-dialect stream. Only the fields we act on are named; the rest
/// are ignored rather than rejected, because a provider adding a field must not break a run.
#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    /// Present on the last chunk of a completion. That is when a streamed tool
    /// call is actually finished — not the first fragment that named it.
    #[serde(default, rename = "finish_reason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// Where a provider exposes it; absent for most.
    #[serde(default, rename = "reasoning_content")]
    reasoning: Option<String>,
    /// The model asking to call tools. Some routes (Grok) send id, name and the
    /// complete JSON arguments in one chunk. Others (OpenAI-shaped streams) name
    /// the call on the first chunk and then send argument fragments keyed only
    /// by `index`. `SseParser` holds that index → id map across lines.
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallChunk>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallChunk {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionChunk>,
}

#[derive(Debug, Deserialize)]
struct FunctionChunk {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Assembles one OpenAI-dialect SSE stream into `ModelDelta`s.
///
/// Tool calls are the reason this is a struct rather than a function: the first
/// chunk names the call (`id`, `name`); later chunks send argument JSON keyed
/// only by `index`. Ending the call on that first chunk is what dropped Hexuria
/// `user_machine_shell` arguments and made NativeChat prompt with an empty command.
#[derive(Debug, Default)]
pub struct SseParser {
    by_index: HashMap<u32, String>,
    started: HashSet<String>,
    ended: HashSet<String>,
}

impl SseParser {
    /// One `data:` line. `[DONE]` and a `finish_reason` close every open tool call.
    ///
    /// An `{"error": …}` body is a failure, not a frame to skip. A gateway that answers 200 and
    /// then says the error in the body used to end the run silently: the line was unreadable, so
    /// it was dropped, and the client was handed a successful run with nothing in it.
    pub fn push_line(&mut self, line: &str) -> Result<Vec<ModelDelta>, ModelError> {
        let Some(payload) = line.strip_prefix("data: ") else {
            // Not a frame at all. A body that is not SSE still arrives here line by line, and an
            // error body is the one shape worth reading; anything else (a comment, a blank line,
            // an `event:` field) is skipped as before.
            return match error_sentence(line.trim()) {
                Some(message) => Err(ModelError::Stream(message)),
                None => Ok(Vec::new()),
            };
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        // The sentinel that ends every OpenAI-dialect stream. Not JSON, and parsing
        // it as JSON is the classic way to end a working stream with a spurious error.
        if payload == "[DONE]" {
            return Ok(self.close_open());
        }
        if let Some(message) = error_sentence(payload) {
            return Err(ModelError::Stream(message));
        }
        let Ok(chunk) = serde_json::from_str::<Chunk>(payload) else {
            // A frame we cannot read is skipped, not fatal: one malformed chunk must
            // not discard the reply that came before it.
            return Ok(Vec::new());
        };
        let mut deltas = Vec::new();
        for choice in chunk.choices {
            if let Some(reasoning) = choice.delta.reasoning.filter(|text| !text.is_empty()) {
                deltas.push(ModelDelta::Reasoning(reasoning));
            }
            if let Some(content) = choice.delta.content.filter(|text| !text.is_empty()) {
                deltas.push(ModelDelta::Text(content));
            }
            for call in choice.delta.tool_calls.into_iter().flatten() {
                let index = call.index.unwrap_or(0);
                if let Some(id) = call.id.filter(|id| !id.is_empty()) {
                    self.by_index.insert(index, id);
                }
                let Some(id) = self.by_index.get(&index).cloned() else {
                    continue;
                };
                let function = call.function.unwrap_or(FunctionChunk {
                    name: None,
                    arguments: None,
                });
                if let Some(name) = function.name.filter(|name| !name.is_empty())
                    && self.started.insert(id.clone())
                {
                    deltas.push(ModelDelta::ToolCallStart {
                        id: id.clone(),
                        name,
                    });
                }
                if let Some(arguments) = function.arguments.filter(|args| !args.is_empty()) {
                    deltas.push(ModelDelta::ToolCallArgs {
                        id: id.clone(),
                        delta: arguments,
                    });
                }
            }
            if choice
                .finish_reason
                .as_deref()
                .is_some_and(|reason| !reason.is_empty())
            {
                deltas.extend(self.close_open());
            }
        }
        Ok(deltas)
    }

    /// The HTTP body ended. Close any tool call that never got a `finish_reason`.
    pub fn finish(&mut self) -> Vec<ModelDelta> {
        self.close_open()
    }

    fn close_open(&mut self) -> Vec<ModelDelta> {
        let mut ids: Vec<(u32, String)> = self
            .by_index
            .iter()
            .map(|(index, id)| (*index, id.clone()))
            .collect();
        ids.sort_by_key(|(index, _)| *index);
        let mut deltas = Vec::new();
        for (_, id) in ids {
            if self.started.contains(&id) && self.ended.insert(id.clone()) {
                deltas.push(ModelDelta::ToolCallEnd { id });
            }
        }
        deltas
    }
}

/// The sentence in an `{"error": …}` body, when that is what this text is.
///
/// Both shapes providers use: `{"error": {"message": "…"}}` and `{"error": "…"}`. Anything else
/// — including a frame that simply has no `error` field — is not an error and returns `None`.
fn error_sentence(text: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    Some(if message.trim().is_empty() {
        "the model gateway reported an error with no message".to_string()
    } else {
        message
    })
}

/// Turn one SSE line into deltas, then close any tool call that line named.
///
/// For a whole stream — where argument fragments arrive on later lines — use
/// `SseParser` instead. Closing here is what a single-frame test expects.
pub fn parse_sse_line(line: &str) -> Result<Vec<ModelDelta>, ModelError> {
    let mut parser = SseParser::default();
    let mut deltas = parser.push_line(line)?;
    deltas.extend(parser.finish());
    Ok(deltas)
}

/// An opaque, stable id for the CONVERSATION this request belongs to, for the gateway's session
/// affinity — sent as `user`, which is the field its OpenAI-shaped parser reads
/// (`oag-proto/src/openai.rs`: `client_session: body["user"]`).
///
/// WHY IT MATTERS, and why the obvious reading of the gateway is wrong. `SessionKey::resolve`
/// (`oag-pool/src/sticky.rs`) has three tiers: the client's session, then the prompt blocks marked
/// cacheable, then `from_caller(api_key_id, model)`. Sending none of the first two does not fail —
/// tier three always yields a key — so affinity looks configured and is real, but it is ONE pin
/// for the whole harness. Every conversation is then herded onto a single credential, and every
/// provider scopes its prompt cache to the credential, so conversations evict each other's caches
/// while a busy deployment stacks on one key. Nothing errors, and the only symptom is a hit rate
/// that is quietly lower than it should be.
///
/// The pair, not the coworker alone: a coworker two people share holds two transcripts
/// (`gateway_transcript` keys on `(coworker, account)`), so they are two conversations with two
/// prefixes, and pinning them together would make each one's turns evict the other's.
///
/// HASHED, BECAUSE THIS VALUE LEAVES OUR BOUNDARY. `user` is forwarded upstream; a provider would
/// otherwise receive our internal coworker and account ids verbatim. The gateway namespaces the
/// key by principal itself (`from_client_session(principal_id, s)`), so this only has to be
/// unique within our own principal — a digest is enough, and identifies nobody off-box.
fn conversation_pin(request: &ModelRequest) -> Option<String> {
    use std::fmt::Write as _;

    use sha2::{Digest as _, Sha256};
    let (scope, actor) = (
        request.spend_scope.as_deref()?,
        request.spend_actor.as_deref()?,
    );
    let mut hasher = Sha256::new();
    // A separator that cannot occur in either id, so ("ab", "c") and ("a", "bc") differ.
    hasher.update(scope.as_bytes());
    hasher.update([0x1f]);
    hasher.update(actor.as_bytes());
    // Half the digest, which is plenty to keep conversations apart within one principal and keeps
    // the value short enough to read in a gateway log line.
    Some(
        hasher
            .finalize()
            .iter()
            .take(16)
            .fold(String::with_capacity(35), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
    )
}

/// A message's `content` on the wire: the bare string when it is only words, OpenAI content
/// parts when it carries images — a screenshot the model must see to act on the screen.
fn message_content(message: &ChatMessage) -> serde_json::Value {
    if message.images.is_empty() {
        return serde_json::Value::String(message.content.clone());
    }
    let mut parts = vec![serde_json::json!({ "type": "text", "text": message.content })];
    for image in &message.images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", image.mime, image.base64) },
        }));
    }
    serde_json::Value::Array(parts)
}

/// What an unanswered call is told: the call happened and nothing came back.
const NO_RESULT: &str = "(no result was recorded for this call)";

/// A message with its pictures as a user message: the only role the dialect lets carry them.
fn as_words(message: &ChatMessage) -> serde_json::Value {
    let words = ChatMessage {
        images: message.images.clone(),
        ..ChatMessage::text("user", message.as_text())
    };
    serde_json::json!({"role": "user", "content": message_content(&words)})
}

/// The request's conversation in the OpenAI chat dialect.
///
/// AN ASSISTANT'S CALLS AND THEIR RESULTS TRAVEL AS ONE BLOCK, and a provider refuses the turn
/// with a 400 when the block is broken: a `tool` message whose call is not in the assistant
/// message right before it, or a call no `tool` message answers. Both happen honestly — a result a
/// client sent for a call made in an earlier request, a call a stopped run never ran — so the
/// block is mended here rather than trusted: an orphan result goes as the words the loop always
/// wrote, an unanswered call is told it has none. Nothing is dropped.
///
/// A PICTURE CANNOT RIDE A TOOL RESULT in this dialect, so a round's screenshots follow its last
/// result as one user message — never between a call and its answer, which would break the block.
fn chat_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(request.messages.len() + 1);
    if let Some(system) = &request.system {
        out.push(serde_json::json!({"role": "system", "content": system}));
    }
    let mut messages = request.messages.iter().peekable();
    while let Some(message) = messages.next() {
        if message.role == "tool" {
            out.push(as_words(message));
            continue;
        }
        if message.tool_calls.is_empty() {
            out.push(serde_json::json!({
                "role": message.role,
                "content": message_content(message),
            }));
            continue;
        }
        let content = if message.content.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(message.content.clone())
        };
        let calls: Vec<serde_json::Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                serde_json::json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                })
            })
            .collect();
        out.push(serde_json::json!({"role": "assistant", "content": content, "tool_calls": calls}));
        let mut open: Vec<&str> = message
            .tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect();
        let mut pictures = Vec::new();
        let mut orphans = Vec::new();
        while let Some(result) = messages.next_if(|next| next.role == "tool") {
            let answered = result
                .tool_call_id
                .as_deref()
                .and_then(|id| open.iter().position(|call| *call == id));
            match answered {
                Some(at) => {
                    out.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": open.remove(at),
                        "content": result.content,
                    }));
                    pictures.extend(result.images.iter().cloned());
                }
                None => orphans.push(result),
            }
        }
        for id in open {
            out.push(serde_json::json!({"role": "tool", "tool_call_id": id, "content": NO_RESULT}));
        }
        if !pictures.is_empty() {
            let screen = ChatMessage {
                images: pictures,
                ..ChatMessage::text("user", "The screen after the tool calls above.")
            };
            out.push(serde_json::json!({"role": "user", "content": message_content(&screen)}));
        }
        out.extend(orphans.into_iter().map(as_words));
    }
    out
}

/// The body of one chat completion request.
fn chat_body(request: &ModelRequest) -> serde_json::Value {
    serde_json::json!({
        "model": request.model,
        "stream": true,
        "messages": chat_messages(request),
    })
}

#[async_trait::async_trait]
impl ModelDoor for GatewayDoor {
    /// `probe`, answered from the last one while it is younger than `READY_FOR`. Boot calls
    /// `probe` itself and always asks.
    async fn ready(&self) -> Option<Result<(), ModelError>> {
        let recent = self.ready_seen.lock().ok().and_then(|seen| {
            seen.as_ref()
                .filter(|(at, _)| at.elapsed() < READY_FOR)
                .map(|(_, probed)| probed.again())
        });
        if let Some(recent) = recent {
            return Some(recent);
        }
        let probed = self.probe().await;
        if let Ok(mut seen) = self.ready_seen.lock() {
            *seen = Some((std::time::Instant::now(), Probed::of(&probed)));
        }
        Some(probed)
    }

    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let mut payload = chat_body(&request);
        // WHICH CONVERSATION THIS IS, so the gateway can pin it to one credential and the
        // provider's prompt cache can actually hit. Omitted when the request carries no
        // scope/actor pair (a judge call, say), which simply leaves the gateway on its
        // coarser per-caller tier — the behaviour every request had before this line.
        if let Some(pin) = conversation_pin(&request)
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("user".to_string(), serde_json::json!(pin));
        }

        // Advertise the run's tools so the model can call them. Only when there are any — an empty
        // `tools: []` makes some gateways reject the request, and "no tools" is a plain chat turn.
        if !request.tools.is_empty()
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("tools".to_string(), serde_json::json!(request.tools));
            object.insert("tool_choice".to_string(), serde_json::json!("auto"));
        }

        // The coworker's own key when it has one; the deployment's otherwise. A key that could not
        // be produced refuses here, before any request: running on the deployment's key would step
        // around the cap the coworker's key exists to enforce.
        let key = match &request.gateway_key {
            None => self.key.as_str(),
            Some(crate::model::GatewayKey::Own(key)) => key.as_str(),
            Some(crate::model::GatewayKey::Unavailable(reason)) => {
                // HELD, NOT CAPPED: the key could not be produced, which is a fault on this side
                // or in the vault. The sentence says "held" and always did; the variant now
                // agrees with it.
                return Err(ModelError::Held(format!(
                    "This coworker's own gateway key could not be used: {reason}. Its turns are \
                     held rather than run on the deployment's key, which would step around its cap."
                )));
            }
        };
        let response = self
            .client()?
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(key)
            .json(&payload)
            .send()
            .await
            .map_err(send_error)?;

        let status = response.status();
        if !status.is_success() {
            let retry_after_s = retry_after_s(response.headers());
            let body = response.text().await.unwrap_or_default();
            // WHICH KEY WAS REFUSED, because nobody else can say. The gateway records nothing at
            // all for a rejected key — it proved that by presenting junk and watching its own log
            // stay flat — so this line is the only place in either system that can name the
            // credential a 401 was about. Without it every 401 reads identically whether the key
            // was the deployment's, a coworker's own, or one the gateway lost in a wipe, and on
            // 8 Sep 2026 that ambiguity cost an hour of four-way guessing between three sessions.
            //
            // THE PREFIX, NEVER THE KEY. `oag_live_f69df82` is exactly what names the row in the
            // gateway's `api_key` table, which is the whole question being asked, and is useless
            // to anyone who later reads the log.
            //
            // `owned` is the other half: a coworker's own key and the deployment's fail with the
            // same status and the same sentence, and which one it was decides where to look next.
            if status.as_u16() == 401 {
                tracing::error!(
                    key_prefix = %logged_prefix(key),
                    owned = request.gateway_key.is_some(),
                    "the gateway refused this key"
                );
            }
            if status.as_u16() == 402 {
                return Err(ModelError::SpendCap(spend_cap_sentence(&body)));
            }
            return Err(ModelError::Refused {
                status: status.as_u16(),
                // Bounded: an upstream error page must not become a megabyte in our logs.
                body: body.chars().take(500).collect(),
                retry_after_s,
            });
        }

        // Frames can split across chunks, so bytes are buffered and consumed line by line.
        // Tool-call argument fragments share one parser so a later chunk without `id`
        // still belongs to the call the first chunk named.
        let live = Arc::new(Mutex::new(SseState::default()));
        let body_state = live.clone();
        let body = response.bytes_stream().flat_map(move |chunk| {
            let events = match chunk {
                Err(error) => vec![Err(match send_error(error) {
                    // A body that stopped arriving is the model going quiet, not a gateway
                    // that could not be reached: nothing about the connect failed.
                    ModelError::Unreachable(detail) => ModelError::Stream(detail),
                    other => other,
                })],
                Ok(bytes) => {
                    let mut state = match body_state.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    state.buffer.push_str(&String::from_utf8_lossy(&bytes));
                    let mut out = Vec::new();
                    while let Some(index) = state.buffer.find('\n') {
                        let line: String = state.buffer.drain(..=index).collect();
                        match state.parser.push_line(line.trim_end()) {
                            Ok(deltas) => out.extend(deltas.into_iter().map(Ok)),
                            Err(error) => out.push(Err(error)),
                        }
                    }
                    out
                }
            };
            stream::iter(events)
        });
        let tail = futures::stream::once(async move {
            let mut state = match live.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let leftover = std::mem::take(&mut state.buffer);
            let mut out = Vec::new();
            if !leftover.trim().is_empty() {
                match state.parser.push_line(leftover.trim_end()) {
                    Ok(deltas) => out.extend(deltas.into_iter().map(Ok)),
                    Err(error) => out.push(Err(error)),
                }
            }
            out.extend(state.parser.finish().into_iter().map(Ok));
            out
        })
        .flat_map(stream::iter);

        Ok(Box::pin(body.chain(tail)))
    }
}

#[derive(Default)]
struct SseState {
    buffer: String,
    parser: SseParser,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/gateway_tests.rs"]
mod tests;
