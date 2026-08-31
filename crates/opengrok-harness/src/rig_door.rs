//! The same door, opened with Rig.
//!
//! `GatewayDoor` speaks the gateway's OpenAI-compatible route directly, in about eighty lines. This
//! one goes through [`rig-core`], which is the operator's stated choice, and the point of having
//! both is that `ModelDoor` makes it a runtime decision rather than a rewrite: `OG_MODEL_DOOR=rig`
//! swaps them and nothing downstream — projection, journal, tools — notices.
//!
//! WHAT RIG BUYS, HONESTLY. Today: one provider (ours), so the abstraction is not yet earning its
//! keep, and the direct door remains the default because it is the one whose failure modes we have
//! read line by line. What it will buy: twenty other providers behind the same trait if a coworker
//! is ever pinned somewhere the gateway does not route, and a tool-calling surface we would
//! otherwise hand-roll per dialect. Keeping both doors is cheap; discovering later that the
//! abstraction does not fit is not.
//!
//! THE CREDENTIAL IS STILL OURS AND STILL A ROUTE. Rig is pointed at open-ai-gateway's base URL
//! with an `oag_live_` key, so CLAUDE.md #4 holds exactly as it does for the direct door: a
//! provider secret never enters this process.

use futures::StreamExt;
use rig_core::client::CompletionClient;
use rig_core::completion::CompletionModel;
use rig_core::message::Message;
use rig_core::providers::openai;
use rig_core::streaming::StreamedAssistantContent;

use crate::model::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};

pub struct RigDoor {
    base_url: String,
    key: String,
}

impl std::fmt::Debug for RigDoor {
    /// Hand-written so the key cannot reach a log through a derived `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RigDoor")
            .field("base_url", &self.base_url)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl RigDoor {
    pub fn new(base_url: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            key: key.into(),
        }
    }
}

#[async_trait::async_trait]
impl ModelDoor for RigDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        // Built per call rather than held: the client is cheap, and a long-lived one would have to
        // be rebuilt anyway the moment a coworker's route or key differed from the last caller's.
        let client = openai::Client::builder()
            .api_key(self.key.clone())
            .base_url(self.base_url.clone())
            .build()
            .map_err(|error| ModelError::Unreachable(error.to_string()))?
            // The completions API, not Responses: it is what the gateway serves.
            .completions_api();

        let model = client.completion_model(&request.model);

        // Rig wants a prompt plus history. The last user message is the prompt; everything before
        // it is history, which is how a tool result from the previous round reaches the model.
        let (prompt, history) = split_prompt(&request);

        let mut builder = model.completion_request(prompt);
        if let Some(system) = &request.system {
            builder = builder.preamble(system.clone());
        }
        if !history.is_empty() {
            builder = builder.messages(history);
        }

        let response = builder
            .stream()
            .await
            .map_err(|error| ModelError::Stream(error.to_string()))?;

        // Rig's vocabulary into ours. Anything we do not recognise is carried as nothing rather
        // than guessed at: an unrecognised piece must never become text a person reads as the
        // model's words.
        //
        // `flat_map` rather than `map` because one Rig item can be several of ours — Rig delivers a
        // COMPLETE tool call where a raw stream sends fragments, so it becomes start, args and end
        // together. The projection brackets it identically either way, which is the seam earning
        // its keep.
        let deltas = response.flat_map(|item| {
            let mapped: Vec<Result<ModelDelta, ModelError>> = match item {
                Ok(StreamedAssistantContent::Text(text)) => {
                    vec![Ok(ModelDelta::Text(text.text))]
                }
                Ok(StreamedAssistantContent::Reasoning { reasoning, .. }) => {
                    let text = reasoning_text(&reasoning);
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![Ok(ModelDelta::Reasoning(text))]
                    }
                }
                Ok(StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                    let id = tool_call.id.to_string();
                    vec![
                        Ok(ModelDelta::ToolCallStart {
                            id: id.clone(),
                            name: tool_call.function.name.clone(),
                        }),
                        Ok(ModelDelta::ToolCallArgs {
                            id: id.clone(),
                            delta: tool_call.function.arguments.to_string(),
                        }),
                        Ok(ModelDelta::ToolCallEnd { id }),
                    ]
                }
                Ok(_) => Vec::new(),
                Err(error) => vec![Err(ModelError::Stream(error.to_string()))],
            };
            futures::stream::iter(mapped)
        });

        Ok(Box::pin(deltas))
    }
}

/// The readable part of a reasoning block.
///
/// Encrypted and redacted blocks are deliberately dropped rather than rendered: they are opaque
/// provider payloads, and showing one to a person as the model's thinking would be showing them
/// noise.
fn reasoning_text(reasoning: &rig_core::message::Reasoning) -> String {
    reasoning
        .content
        .iter()
        .filter_map(|block| match block {
            rig_core::message::ReasoningContent::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The last user message is the prompt; everything before it is history.
///
/// Rig's request takes them separately, and getting this wrong is the difference between a model
/// that sees the conversation and one asked the same question every round.
fn split_prompt(request: &ModelRequest) -> (Message, Vec<Message>) {
    let mut history: Vec<Message> = Vec::new();
    let mut prompt = None;

    for message in &request.messages {
        let converted = match message.role.as_str() {
            "assistant" => Message::assistant(message.content.clone()),
            _ => Message::user(message.content.clone()),
        };
        if message.role == "user" {
            // Keep only the LAST user message as the prompt; earlier ones belong to history.
            if let Some(previous) = prompt.replace(converted) {
                history.push(previous);
            }
        } else {
            history.push(converted);
        }
    }

    (
        prompt.unwrap_or_else(|| Message::user(String::new())),
        history,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::model::ChatMessage;

    fn request(messages: &[(&str, &str)]) -> ModelRequest {
        ModelRequest {
            model: "oag/cheap".to_string(),
            system: None,
            tools: Vec::new(),
            messages: messages
                .iter()
                .map(|(role, content)| ChatMessage {
                    role: (*role).to_string(),
                    content: (*content).to_string(),
                })
                .collect(),
        }
    }

    /// A model asked the same question every round would loop forever; the history is what stops it.
    #[test]
    fn the_last_user_message_is_the_prompt_and_the_rest_is_history() {
        let (_, history) = split_prompt(&request(&[
            ("user", "first"),
            ("assistant", "an answer"),
            ("user", "second"),
        ]));
        // "first" and the assistant reply are history; "second" is the prompt.
        assert_eq!(history.len(), 2, "{history:?}");
    }

    /// A turn that is only tool results still has to reach the model somehow.
    #[test]
    fn a_conversation_with_no_user_message_still_produces_a_prompt() {
        let (_, history) = split_prompt(&request(&[("assistant", "thinking")]));
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn the_door_does_not_print_its_key() {
        let door = RigDoor::new("http://localhost:29080", "oag_live_secret");
        let printed = format!("{door:?}");
        assert!(!printed.contains("oag_live_secret"), "{printed}");
        assert!(printed.contains("<redacted>"));
    }
}
