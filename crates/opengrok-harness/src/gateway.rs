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
        let mut messages: Vec<serde_json::Value> = Vec::new();
        if let Some(system) = &request.system {
            messages.push(serde_json::json!({"role": "system", "content": system}));
        }
        for message in &request.messages {
            messages.push(serde_json::json!({
                "role": message.role,
                "content": message_content(message),
            }));
        }

        let mut payload = serde_json::json!({
            "model": request.model,
            "stream": true,
            "messages": messages,
        });
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
mod tests {
    use super::*;

    /// A client that could not be built answers every call as an unreachable gateway. The old
    /// fallback, `reqwest::Client::new()`, panics on the failure it was there to survive.
    #[tokio::test]
    async fn a_door_whose_client_did_not_build_is_unreachable_not_a_panic() {
        let door = GatewayDoor {
            base_url: "http://gateway.invalid".to_string(),
            key: "k".to_string(),
            http: Err("no TLS backend".to_string()),
            ready_seen: Mutex::new(None),
        };
        let error = door
            .stream(ModelRequest::default())
            .await
            .err()
            .expect("no client, no stream");
        assert!(matches!(error, ModelError::Unreachable(_)), "{error:?}");
        assert!(error.to_string().contains("no TLS backend"), "{error}");
        assert!(matches!(
            door.probe().await,
            Err(ModelError::Unreachable(_))
        ));
    }

    /// A gateway that answers every request with `status_line`, counting the requests.
    async fn a_gateway_answering(status_line: &'static str) -> (String, Arc<Mutex<usize>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(0usize));
        let counted = asked.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                *counted.lock().unwrap() += 1;
                let reply = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                     content-length: 11\r\nconnection: close\r\n\r\n{{\"data\":[]}}"
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, asked)
    }

    /// `/ready` asks through `ready`, and a tight prober must not become the gateway's traffic:
    /// a second ask inside `READY_FOR` is answered from the first, refusal included. Boot's
    /// `probe` always asks.
    #[tokio::test]
    async fn readiness_is_asked_of_the_gateway_once_per_window() {
        let (url, asked) = a_gateway_answering("200 OK").await;
        let door = GatewayDoor::new(url, "k");
        assert!(matches!(door.ready().await, Some(Ok(()))));
        assert!(matches!(door.ready().await, Some(Ok(()))));
        assert_eq!(*asked.lock().unwrap(), 1);
        assert!(door.probe().await.is_ok());
        assert_eq!(*asked.lock().unwrap(), 2);

        let (url, asked) = a_gateway_answering("401 Unauthorized").await;
        let door = GatewayDoor::new(url, "k");
        for _ in 0..3 {
            assert!(matches!(
                door.ready().await,
                Some(Err(ModelError::Refused { status: 401, .. }))
            ));
        }
        assert_eq!(*asked.lock().unwrap(), 1);
    }

    /// The internal gateway address is not the person's business. An unreachable gateway used
    /// to print the reqwest error whole, URL and path included, into the chat.
    #[tokio::test]
    async fn an_unreachable_gateway_does_not_leak_its_url() {
        let door = GatewayDoor::new("http://127.0.0.1:1", "k");
        let error = door
            .stream(ModelRequest::default())
            .await
            .err()
            .expect("nothing listens on port 1");
        for text in [error.to_string(), error.sentence()] {
            assert!(!text.contains("127.0.0.1:1"), "{text}");
            assert!(!text.contains("/v1/chat/completions"), "{text}");
        }
        assert!(
            matches!(error, ModelError::Unreachable(_)),
            "a refused connection sent nothing, which is what makes it safe to retry: {error:?}"
        );
    }

    /// Answers every connection with one fixed HTTP response.
    async fn answering(response: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0u8; 8192];
                let _ = socket.read(&mut buffer).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), task)
    }

    /// A 429 carries how long to wait; the door keeps it so the loop can honour it.
    #[tokio::test]
    async fn a_rate_limit_keeps_its_retry_after() {
        let (url, task) = answering(
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 3\r\ncontent-type: application/json\r\ncontent-length: 74\r\nconnection: close\r\n\r\n{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}",
        )
        .await;
        let error = GatewayDoor::new(url, "k")
            .stream(ModelRequest::default())
            .await
            .err()
            .expect("a 429 is not a stream");
        assert!(
            matches!(
                error,
                ModelError::Refused {
                    status: 429,
                    retry_after_s: Some(3),
                    ..
                }
            ),
            "{error:?}"
        );
        task.abort();
    }

    /// A wrong OG_GATEWAY_TOKEN is found at boot, not on the first turn.
    #[tokio::test]
    async fn the_boot_probe_names_a_refused_key() {
        let (url, task) = answering(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await;
        let probed = GatewayDoor::new(url, "oag_live_wrong").probe().await;
        assert!(
            matches!(probed, Err(ModelError::Refused { status: 401, .. })),
            "{probed:?}"
        );
        task.abort();
    }

    /// A probe of a gateway that accepts and never answers ends on its own clock, not on the
    /// door's 200 s read timeout.
    #[tokio::test]
    async fn a_probe_of_a_hung_gateway_ends_on_its_own_clock() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        let door = GatewayDoor::new(format!("http://{address}"), "k");
        let probed = tokio::time::timeout(PROBE_TIMEOUT * 2, door.ready())
            .await
            .expect("the probe has its own clock");
        assert!(
            matches!(probed, Some(Err(ModelError::TimedOut(_)))),
            "{probed:?}"
        );
        held.abort();
    }

    /// A gateway that accepts the connection and never answers — a hung process, a proxy with
    /// nothing behind it — used to hold the call, and the run, for as long as the process lived.
    #[tokio::test]
    async fn a_gateway_that_accepts_and_never_answers_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        let door = GatewayDoor::with_timeouts(
            format!("http://{address}"),
            "k",
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(200),
        );
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            door.stream(ModelRequest::default()),
        )
        .await
        .expect("the door gives up on its own");
        assert!(
            answered.is_err(),
            "a silent gateway is an error, not a stream"
        );
        held.abort();
    }

    /// WHAT MAY BE WRITTEN DOWN ABOUT A REFUSED KEY. The gateway records nothing at all for a
    /// key it rejects, so our log is the only place that can ever name the credential a 401 was
    /// about — but a log that carried the whole key would trade one problem for a worse one.
    #[test]
    fn a_logged_key_names_its_row_and_nothing_else() {
        // A real key: `oag_live_` plus seven characters is exactly `api_key.key_prefix` on the
        // gateway, which is the whole question a 401 is asking.
        let key = "oag_live_f69df82cafe1234567890abcdef";
        assert_eq!(logged_prefix(key), "oag_live_f69df82");

        // THE HALF THAT MATTERS: the secret does not travel. Asserted as "the tail is absent"
        // rather than "the head is right", because a prefix that silently grew to swallow the
        // whole key would still satisfy the equality above if that were the only check.
        assert!(
            !logged_prefix(key).contains("cafe1234567890abcdef"),
            "the secret tail reached the log: {}",
            logged_prefix(key)
        );
    }

    /// A key is opaque to us: we neither mint it nor validate its shape. A byte-index slice would
    /// panic on a multi-byte character, and losing a turn to a logging call is an absurd way to
    /// fail — so the rule is defined on characters and every odd input has to survive it.
    #[test]
    fn logging_a_strange_key_cannot_panic() {
        for odd in [
            "",
            "short",
            "oag_live_",
            "ключ-которого-не-бывает",
            "🔑🔑🔑",
        ] {
            let logged = logged_prefix(odd);
            assert!(
                logged.chars().count() <= KEY_PREFIX_LEN,
                "{odd:?} logged {} characters",
                logged.chars().count()
            );
            assert!(
                odd.starts_with(&logged),
                "{odd:?} -> {logged:?} is not a prefix"
            );
        }
    }

    #[test]
    fn a_tool_call_frame_becomes_start_args_end() {
        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#;
        assert_eq!(
            parse_sse_line(line).expect("a tool call frame is not an error"),
            vec![
                ModelDelta::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "shell".to_string()
                },
                ModelDelta::ToolCallArgs {
                    id: "call_1".to_string(),
                    delta: "{\"command\":\"ls\"}".to_string()
                },
                ModelDelta::ToolCallEnd {
                    id: "call_1".to_string()
                },
            ]
        );
    }

    /// OpenAI-shaped streams name the call on the first chunk (often with empty
    /// `arguments`) and send the JSON on later chunks that have `index` but no `id`.
    /// Closing the call on that first chunk is how Hexuria Ask cards showed no command.
    #[test]
    fn streamed_tool_call_arguments_are_assembled_across_chunks() {
        let mut parser = SseParser::default();
        assert_eq!(
            parser
                .push_line(
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"user_machine_shell","arguments":""}}]}}]}"#
                )
                .expect("a tool call frame is not an error"),
            vec![ModelDelta::ToolCallStart {
                id: "call_1".to_string(),
                name: "user_machine_shell".to_string()
            }]
        );
        assert_eq!(
            parser
                .push_line(
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"ls /Volumes/goldcoders\"}"}}]}}]}"#
                )
                .expect("an arguments frame is not an error"),
            vec![ModelDelta::ToolCallArgs {
                id: "call_1".to_string(),
                delta: "{\"command\":\"ls /Volumes/goldcoders\"}".to_string()
            }]
        );
        assert_eq!(
            parser
                .push_line(r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
                .expect("a finish frame is not an error"),
            vec![ModelDelta::ToolCallEnd {
                id: "call_1".to_string()
            }]
        );
    }

    #[test]
    fn a_content_frame_becomes_a_text_delta() {
        let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
        assert_eq!(
            parse_sse_line(line).expect("a content frame is not an error"),
            vec![ModelDelta::Text("hello".to_string())]
        );
    }

    /// The sentinel is not JSON. Parsing it as JSON is the classic way to end a working stream
    /// with a spurious error.
    #[test]
    fn the_done_sentinel_is_not_an_error() {
        assert!(
            parse_sse_line("data: [DONE]")
                .expect("a sentinel")
                .is_empty()
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        assert!(parse_sse_line(": ping").expect("a comment").is_empty());
        assert!(parse_sse_line("").expect("a blank line").is_empty());
        assert!(parse_sse_line("data: ").expect("an empty frame").is_empty());
    }

    /// AN EMPTY SUCCESS IS THE DANGEROUS REPLY (CLAUDE.md). A gateway that answers 200 and puts
    /// the error in the body left the run with no deltas at all, which reached the person as the
    /// coworker having nothing to say.
    #[test]
    fn an_error_frame_breaks_the_stream_instead_of_being_skipped() {
        let line = r#"data: {"error":{"message":"upstream provider is out of credit","type":"insufficient_quota"}}"#;
        let error = parse_sse_line(line).expect_err("an error frame is not deltas");
        assert!(
            matches!(&error, ModelError::Stream(message) if message == "upstream provider is out of credit"),
            "{error:?}"
        );
    }

    /// The other shape, and the same reasoning: a bare string under `error`.
    #[test]
    fn an_error_frame_with_a_bare_string_still_breaks_the_stream() {
        let error = parse_sse_line(r#"data: {"error":"model not found"}"#)
            .expect_err("an error frame is not deltas");
        assert!(
            matches!(&error, ModelError::Stream(message) if message == "model not found"),
            "{error:?}"
        );
    }

    /// A 200 whose body is not SSE at all reaches the parser as ordinary lines.
    #[test]
    fn an_error_body_that_is_not_sse_breaks_the_stream() {
        let error = parse_sse_line(r#"{"error":{"message":"bad request"}}"#)
            .expect_err("an error body is not deltas");
        assert!(
            matches!(&error, ModelError::Stream(message) if message == "bad request"),
            "{error:?}"
        );
    }

    /// One bad frame must not discard the reply that came before it.
    #[test]
    fn a_malformed_frame_is_skipped_rather_than_fatal() {
        assert!(
            parse_sse_line("data: {not json")
                .expect("a malformed frame")
                .is_empty()
        );
    }

    /// An empty content string is a keepalive, not a word — emitting it would open a message for
    /// nothing.
    #[test]
    fn an_empty_content_delta_produces_nothing() {
        let line = r#"data: {"choices":[{"delta":{"content":""}}]}"#;
        assert!(parse_sse_line(line).expect("a keepalive").is_empty());
    }

    #[test]
    fn a_frame_with_no_choices_produces_nothing() {
        assert!(
            parse_sse_line(r#"data: {"choices":[]}"#)
                .expect("a frame")
                .is_empty()
        );
        assert!(
            parse_sse_line(r#"data: {"id":"x","object":"chunk"}"#)
                .expect("a frame")
                .is_empty()
        );
    }

    #[test]
    fn reasoning_arrives_before_the_content_of_the_same_frame() {
        let line =
            r#"data: {"choices":[{"delta":{"reasoning_content":"hmm","content":"answer"}}]}"#;
        assert_eq!(
            parse_sse_line(line).expect("a reasoning frame is not an error"),
            vec![
                ModelDelta::Reasoning("hmm".to_string()),
                ModelDelta::Text("answer".to_string()),
            ]
        );
    }

    /// A field a provider adds tomorrow must not break a run today.
    #[test]
    fn unknown_fields_do_not_break_a_frame() {
        let line = r#"data: {"choices":[{"delta":{"content":"hi","somethingNew":42}}],"extra":1}"#;
        assert_eq!(
            parse_sse_line(line).expect("an unknown field is not an error"),
            vec![ModelDelta::Text("hi".to_string())]
        );
    }

    fn pinned(scope: Option<&str>, actor: Option<&str>) -> Option<String> {
        conversation_pin(&ModelRequest {
            model: "m".to_string(),
            messages: Vec::new(),
            system: None,
            tools: Vec::new(),
            gateway_key: None,
            spend_scope: scope.map(str::to_string),
            spend_actor: actor.map(str::to_string),
        })
    }

    /// The three properties the gateway's tier-1 affinity actually depends on.
    ///
    /// Stable, or it pins nothing and every turn picks a fresh credential. Distinct per
    /// conversation, or two conversations share a credential and evict each other's prompt cache.
    /// Absent when there is no pair, because falling back to the gateway's coarser per-caller tier
    /// is the behaviour every request had before this existed, and is not a failure.
    #[test]
    fn the_conversation_pin_is_stable_distinct_and_optional() {
        let a = pinned(Some("cw-1"), Some("acct-1")).expect("a pair pins");
        assert_eq!(a, pinned(Some("cw-1"), Some("acct-1")).expect("stable"));

        // A shared coworker holds one transcript PER PERSON, so those are two conversations with
        // two prefixes; one pin between them would have each evicting the other's cache.
        assert_ne!(
            a,
            pinned(Some("cw-1"), Some("acct-2")).expect("other person")
        );
        assert_ne!(
            a,
            pinned(Some("cw-2"), Some("acct-1")).expect("other coworker")
        );

        // The separator earns its place: without it these two would hash identically.
        assert_ne!(pinned(Some("ab"), Some("c")), pinned(Some("a"), Some("bc")));

        assert_eq!(pinned(None, Some("acct-1")), None);
        assert_eq!(pinned(Some("cw-1"), None), None);

        // It leaves our boundary, so it must not carry the ids themselves.
        assert!(!a.contains("cw-1") && !a.contains("acct-1"), "{a}");
    }

    /// The key must not be printable, however it is logged.
    #[test]
    fn the_door_does_not_print_its_key() {
        let door = GatewayDoor::new("http://localhost:29080", "oag_live_secret");
        let printed = format!("{door:?}");
        assert!(!printed.contains("oag_live_secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }

    #[test]
    fn a_message_without_images_is_a_bare_string_on_the_wire() {
        let message = ChatMessage {
            role: "user".into(),
            content: "hello".into(),
            images: Vec::new(),
        };
        assert_eq!(message_content(&message), serde_json::json!("hello"));
    }

    #[test]
    fn a_message_with_a_screenshot_is_text_then_image_url_parts() {
        let message = ChatMessage {
            role: "user".into(),
            content: "[tool c1 result] screenshot attached".into(),
            images: vec![crate::ImagePart {
                mime: "image/png".into(),
                base64: "AAAA".into(),
            }],
        };
        let parts = message_content(&message);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "[tool c1 result] screenshot attached");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }
}
