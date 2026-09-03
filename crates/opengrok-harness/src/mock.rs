//! A door that answers without spending anything.
//!
//! `OG_MODEL_DOOR=mock` runs the whole stack — endpoint, harness, projection, SSE — with no
//! provider, no key and no subscription. It exists for three reasons, in increasing order of
//! importance: it is free; it makes CI able to exercise the streaming path at all (a test that
//! needs a live key is a test that gets deleted); and it can produce what a live call cannot be
//! asked for — a truncated stream, a tool call split across ten fragments, a provider that fails
//! halfway.
//!
//! It emits `ModelDelta`s, the same vocabulary the real door emits, and gets no private path
//! through the projection. A bug this hides is therefore a bug in the door, not in anything
//! downstream of it.

use futures::stream;

use crate::model::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};

/// Replays a script.
#[derive(Debug, Clone, Default)]
pub struct MockDoor {
    script: Vec<ModelDelta>,
    /// Fails after the script, to exercise the error path on demand.
    fail_with: Option<String>,
    /// When set, the door asks for its tool until it can see the result in the conversation, then
    /// answers in words.
    ///
    /// KEYED OFF THE CONVERSATION, NOT A COUNTER. A counter here would be per-process, and the
    /// door is one shared `Arc` — so the first run would ask for a tool and every run after it
    /// would silently skip that path. That is exactly the bug this comment exists to prevent a
    /// second time: state that belongs to a conversation must be read from the conversation.
    once_then_answer: bool,
    /// The one word to answer an auto-review judge request with (a request whose system prompt
    /// starts with `JUDGE_MARKER`). Ordinary requests are unaffected, so a mock-driven turn can
    /// reach every rung of the ladder with no provider and no spend.
    judge_verdict: Option<String>,
    /// A group member: on a room turn (a system prompt that begins "You are …, one participant")
    /// it says "{name} here" through the `SendMessage` tool, then stops. Outside a room it
    /// echoes. What a test needs to see two members speak in turn, distinguishably.
    room_speaker: bool,
    /// Answers with the system prompt it was given, so a test can assert what the model was
    /// actually told rather than what the code meant to tell it.
    echo_system: bool,
    /// The one room member (by the name the system prompt gives it) that reaches for the shell
    /// tool before it speaks — what a test needs to raise a card INSIDE a room and watch the
    /// round continue after the answer. The others behave as `room_speaker`.
    room_tool_asker: Option<String>,
}

impl MockDoor {
    /// Answer judge requests with this word ("allow" | "block" | "ask"); anything else parses to
    /// `Unavailable`, which is also a rung worth reaching.
    #[must_use]
    pub fn with_judge_verdict(mut self, word: impl Into<String>) -> Self {
        self.judge_verdict = Some(word.into());
        self
    }

    /// The default script: word-by-word, so a client's streaming is visibly exercised rather than
    /// arriving as one indivisible blob that would also pass a non-streaming implementation.
    pub fn echoing() -> Self {
        Self::default()
    }

    pub fn with_script(script: Vec<ModelDelta>) -> Self {
        Self {
            script,
            fail_with: None,
            once_then_answer: false,
            judge_verdict: None,
            room_speaker: false,
            room_tool_asker: None,
            echo_system: false,
        }
    }

    /// A door that behaves as a group member: on a room turn it delivers "{name} here" with the
    /// room's `SendMessage` tool and then stops; anywhere else it echoes. The name comes from the
    /// system prompt the orchestrator wrote, so two members speak distinguishably from ONE door.
    /// A door that says back its own system prompt. The composition of identity, standing role
    /// and machine discipline is only correct if it ARRIVES, and every other door hides it.
    pub fn echoing_the_system_prompt() -> Self {
        Self {
            echo_system: true,
            ..Self::default()
        }
    }

    pub fn room_speaker() -> Self {
        Self {
            room_speaker: true,
            ..Self::default()
        }
    }

    /// `room_speaker`, except that the member named `asker` asks to run `asking_for_a_tool`'s
    /// shell command first and speaks — "{name} here, after the tool" — only once that call's
    /// result is in the conversation. A refusal is a result too: the member speaks after a no.
    pub fn room_speaker_asking_for_a_tool(asker: impl Into<String>) -> Self {
        Self {
            room_speaker: true,
            room_tool_asker: Some(asker.into()),
            ..Self::default()
        }
    }

    /// The shell call `asking_for_a_tool` makes, as a script.
    fn shell_script() -> Vec<ModelDelta> {
        vec![
            ModelDelta::Text("let me check that".to_string()),
            ModelDelta::ToolCallStart {
                id: "mock-call-1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "mock-call-1".to_string(),
                // Writes a marker a test can look for on the box, which is the only way to
                // prove the command ran *there* rather than being reported as run.
                delta: r#"{"command":"echo opengrok-tool-ran > /tmp/opengrok-tool-ran"}"#
                    .to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "mock-call-1".to_string(),
            },
        ]
    }

    /// Whether the conversation already holds the result of this call (`[tool <id> result]`).
    fn saw_result(request: &ModelRequest, call_id: &str) -> bool {
        let marker = format!("[tool {call_id} result]");
        request
            .messages
            .iter()
            .any(|message| message.content.contains(&marker))
    }

    /// "You are Ada, one participant in a group chat …" → `Ada`.
    fn room_member_name(request: &ModelRequest) -> Option<String> {
        let system = request.system.as_deref()?;
        let rest = system.strip_prefix("You are ")?;
        let (name, tail) = rest.split_once(',')?;
        tail.trim_start()
            .starts_with("one participant")
            .then(|| name.to_string())
    }

    /// A door that asks to run a shell command, then stops.
    ///
    /// Exists because the echoing door never reaches for a tool, so every test using it exercises
    /// the *talking* path and none of the *doing* path — which is how a tool bug hides behind a
    /// green suite. `OG_MODEL_DOOR=mock-tools` selects it.
    pub fn asking_for_a_tool() -> Self {
        Self {
            script: Self::shell_script(),
            fail_with: None,
            // Asks once, then answers — like a turn that actually ends.
            once_then_answer: true,
            judge_verdict: None,
            room_speaker: false,
            room_tool_asker: None,
            echo_system: false,
        }
    }

    pub fn failing_with(message: impl Into<String>) -> Self {
        Self {
            script: Vec::new(),
            fail_with: Some(message.into()),
            once_then_answer: false,
            judge_verdict: None,
            room_speaker: false,
            room_tool_asker: None,
            echo_system: false,
        }
    }

    /// What the default door says back, split so the stream has several frames.
    fn echo_script(request: &ModelRequest) -> Vec<ModelDelta> {
        let asked = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap_or_else(|| "nothing".to_string());

        // NAMING THE MODEL IS THE POINT, not decoration. Which model a turn was asked for is
        // otherwise invisible to every test: a run that quietly substituted the deployment's model
        // for the coworker's answered exactly like a correct one, and did so for weeks.
        let model = &request.model;
        let reply = format!(
            "You said: {asked}. This is the mock door standing in for {model} — no model was called."
        );
        reply
            .split_inclusive(' ')
            .map(|word| ModelDelta::Text(word.to_string()))
            .collect()
    }
}

#[async_trait::async_trait]
impl ModelDoor for MockDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        if let Some(message) = &self.fail_with {
            let error = ModelError::Stream(message.clone());
            return Ok(Box::pin(stream::once(async move { Err(error) })));
        }
        if let Some(word) = &self.judge_verdict
            && request
                .system
                .as_deref()
                .is_some_and(|system| system.starts_with(crate::review::JUDGE_MARKER))
        {
            let word = word.clone();
            return Ok(Box::pin(stream::once(
                async move { Ok(ModelDelta::Text(word)) },
            )));
        }
        // Has this conversation already seen its tool result? The harness appends one as a user
        // message, so the conversation itself is the state.
        let already_ran = request
            .messages
            .iter()
            .any(|message| message.content.contains("[tool "));

        if self.echo_system {
            let said = request.system.clone().unwrap_or_default();
            return Ok(Box::pin(stream::once(
                async move { Ok(ModelDelta::Text(said)) },
            )));
        }
        let script = if self.room_speaker
            && let Some(name) = Self::room_member_name(&request)
        {
            let send_id = format!("room-{}", name.to_lowercase());
            let asks_first = self.room_tool_asker.as_deref() == Some(name.as_str());
            if Self::saw_result(&request, &send_id) {
                // Its own message is delivered; the turn is over.
                vec![ModelDelta::Text("(that is all from me)".to_string())]
            } else if asks_first && !Self::saw_result(&request, "mock-call-1") {
                Self::shell_script()
            } else {
                let line = if asks_first {
                    format!("{name} here, after the tool")
                } else {
                    format!("{name} here")
                };
                vec![
                    ModelDelta::ToolCallStart {
                        id: send_id.clone(),
                        name: "SendMessage".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: send_id.clone(),
                        delta: serde_json::json!({ "content": line }).to_string(),
                    },
                    ModelDelta::ToolCallEnd { id: send_id },
                ]
            }
        } else if self.script.is_empty() {
            Self::echo_script(&request)
        } else if self.once_then_answer && already_ran {
            // The second round reads the tool result and replies, which is what ends the run.
            vec![ModelDelta::Text(
                "the command ran; that is all I needed".to_string(),
            )]
        } else {
            self.script.clone()
        };
        Ok(Box::pin(stream::iter(script.into_iter().map(Ok))))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn request(text: &str) -> ModelRequest {
        ModelRequest {
            gateway_key: None,
            spend_scope: None,
            model: "mock".to_string(),
            system: None,
            tools: Vec::new(),
            messages: vec![crate::model::ChatMessage {
                role: "user".to_string(),
                content: text.to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn the_default_door_streams_more_than_one_frame() {
        let door = MockDoor::echoing();
        let deltas: Vec<_> = door.stream(request("hello")).await.unwrap().collect().await;
        assert!(deltas.len() > 1, "a single frame would not test streaming");
        let text: String = deltas
            .into_iter()
            .filter_map(|delta| match delta {
                Ok(ModelDelta::Text(text)) => Some(text),
                _ => None,
            })
            .collect();
        assert!(text.contains("hello"), "{text}");
    }

    #[tokio::test]
    async fn a_scripted_door_replays_exactly_what_it_was_given() {
        let script = vec![
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ];
        let door = MockDoor::with_script(script.clone());
        let deltas: Vec<_> = door.stream(request("x")).await.unwrap().collect().await;
        let got: Vec<_> = deltas.into_iter().map(|delta| delta.unwrap()).collect();
        assert_eq!(got, script);
    }

    #[tokio::test]
    async fn a_failing_door_yields_an_error_the_harness_must_handle() {
        let door = MockDoor::failing_with("upstream hung up");
        let deltas: Vec<_> = door.stream(request("x")).await.unwrap().collect().await;
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].is_err());
    }
}
