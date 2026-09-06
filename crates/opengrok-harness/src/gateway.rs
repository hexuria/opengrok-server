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

use futures::{StreamExt, stream};
use serde::Deserialize;

use crate::model::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};

pub struct GatewayDoor {
    base_url: String,
    key: String,
    http: reqwest::Client,
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

impl GatewayDoor {
    pub fn new(base_url: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            key: key.into(),
            http: reqwest::Client::new(),
        }
    }
}

/// The gateway's 402 names the scope in its own words ("the quota on this API key is
/// exhausted", "the monthly budget for this principal is exhausted"); the sentence a person reads
/// keeps those words and says what to do about them. A key cap does not reset — it is a wall at
/// the number written on it — so "raise it" is the only way through that one.
fn spend_cap_sentence(body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| "a spend cap is reached".to_string());
    format!(
        "This coworker cannot take a turn: {detail}. Raise its cap in the console (a key's cap \
         does not reset), or wait for a monthly budget to reset."
    )
}

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
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// Where a provider exposes it; absent for most.
    #[serde(default, rename = "reasoning_content")]
    reasoning: Option<String>,
    /// The model asking to call tools. This gateway delivers each call WHOLE in one chunk (id, name
    /// and the complete JSON arguments together), so a per-line parse can emit the full start/args/end
    /// without cross-chunk state. A provider that fragments arguments across chunks is not handled
    /// here — this one does not.
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallChunk>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallChunk {
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

/// Turn one SSE line into deltas. Pure, so the parsing rules are tested without a socket.
pub fn parse_sse_line(line: &str) -> Vec<ModelDelta> {
    let Some(payload) = line.strip_prefix("data: ") else {
        return Vec::new();
    };
    let payload = payload.trim();
    // The sentinel that ends every OpenAI-dialect stream. Not JSON, and parsing it as JSON is the
    // classic way to end a working stream with a spurious error.
    if payload == "[DONE]" || payload.is_empty() {
        return Vec::new();
    }
    let Ok(chunk) = serde_json::from_str::<Chunk>(payload) else {
        // A frame we cannot read is skipped, not fatal: one malformed chunk must not discard the
        // reply that came before it.
        return Vec::new();
    };
    chunk
        .choices
        .into_iter()
        .flat_map(|choice| {
            let mut deltas = Vec::new();
            if let Some(reasoning) = choice.delta.reasoning.filter(|text| !text.is_empty()) {
                deltas.push(ModelDelta::Reasoning(reasoning));
            }
            if let Some(content) = choice.delta.content.filter(|text| !text.is_empty()) {
                deltas.push(ModelDelta::Text(content));
            }
            // Each tool call arrives whole: emit its start, its complete arguments, and its end
            // together, keyed by the provider's own call id so `collect_tool_calls` can pair them.
            for call in choice.delta.tool_calls.into_iter().flatten() {
                let Some(id) = call.id.filter(|id| !id.is_empty()) else {
                    continue;
                };
                let function = call.function.unwrap_or(FunctionChunk {
                    name: None,
                    arguments: None,
                });
                if let Some(name) = function.name.filter(|name| !name.is_empty()) {
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
                deltas.push(ModelDelta::ToolCallEnd { id });
            }
            deltas
        })
        .collect()
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

#[async_trait::async_trait]
impl ModelDoor for GatewayDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        if let Some(system) = &request.system {
            messages.push(serde_json::json!({"role": "system", "content": system}));
        }
        for message in &request.messages {
            messages.push(serde_json::json!({
                "role": message.role,
                "content": message.content,
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
                return Err(ModelError::SpendCap(format!(
                    "This coworker's own gateway key could not be used: {reason}. Its turns are \
                     held rather than run on the deployment's key, which would step around its cap."
                )));
            }
        };
        let response = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(key)
            .json(&payload)
            .send()
            .await
            .map_err(|error| ModelError::Unreachable(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if status.as_u16() == 402 {
                return Err(ModelError::SpendCap(spend_cap_sentence(&body)));
            }
            return Err(ModelError::Refused {
                status: status.as_u16(),
                // Bounded: an upstream error page must not become a megabyte in our logs.
                body: body.chars().take(500).collect(),
            });
        }

        // Frames can split across chunks, so bytes are buffered and consumed line by line.
        let mut buffer = String::new();
        let deltas = response.bytes_stream().flat_map(move |chunk| {
            let events = match chunk {
                Err(error) => vec![Err(ModelError::Stream(error.to_string()))],
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    let mut out = Vec::new();
                    while let Some(index) = buffer.find('\n') {
                        let line: String = buffer.drain(..=index).collect();
                        out.extend(parse_sse_line(line.trim_end()).into_iter().map(Ok));
                    }
                    out
                }
            };
            stream::iter(events)
        });

        Ok(Box::pin(deltas))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_call_frame_becomes_start_args_end() {
        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#;
        assert_eq!(
            parse_sse_line(line),
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

    #[test]
    fn a_content_frame_becomes_a_text_delta() {
        let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
        assert_eq!(
            parse_sse_line(line),
            vec![ModelDelta::Text("hello".to_string())]
        );
    }

    /// The sentinel is not JSON. Parsing it as JSON is the classic way to end a working stream
    /// with a spurious error.
    #[test]
    fn the_done_sentinel_is_not_an_error() {
        assert!(parse_sse_line("data: [DONE]").is_empty());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        assert!(parse_sse_line(": ping").is_empty());
        assert!(parse_sse_line("").is_empty());
        assert!(parse_sse_line("data: ").is_empty());
    }

    /// One bad frame must not discard the reply that came before it.
    #[test]
    fn a_malformed_frame_is_skipped_rather_than_fatal() {
        assert!(parse_sse_line("data: {not json").is_empty());
    }

    /// An empty content string is a keepalive, not a word — emitting it would open a message for
    /// nothing.
    #[test]
    fn an_empty_content_delta_produces_nothing() {
        let line = r#"data: {"choices":[{"delta":{"content":""}}]}"#;
        assert!(parse_sse_line(line).is_empty());
    }

    #[test]
    fn a_frame_with_no_choices_produces_nothing() {
        assert!(parse_sse_line(r#"data: {"choices":[]}"#).is_empty());
        assert!(parse_sse_line(r#"data: {"id":"x","object":"chunk"}"#).is_empty());
    }

    #[test]
    fn reasoning_arrives_before_the_content_of_the_same_frame() {
        let line =
            r#"data: {"choices":[{"delta":{"reasoning_content":"hmm","content":"answer"}}]}"#;
        assert_eq!(
            parse_sse_line(line),
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
            parse_sse_line(line),
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
}
