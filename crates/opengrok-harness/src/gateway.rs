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
            deltas
        })
        .collect()
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

        let response = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.key)
            .json(&serde_json::json!({
                "model": request.model,
                "stream": true,
                "messages": messages,
            }))
            .send()
            .await
            .map_err(|error| ModelError::Unreachable(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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

    /// The key must not be printable, however it is logged.
    #[test]
    fn the_door_does_not_print_its_key() {
        let door = GatewayDoor::new("http://localhost:29080", "oag_live_secret");
        let printed = format!("{door:?}");
        assert!(!printed.contains("oag_live_secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }
}
