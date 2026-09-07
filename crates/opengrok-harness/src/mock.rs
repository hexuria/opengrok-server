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

use futures::StreamExt as _;
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
    /// Forwards whatever the person typed to the `mock_fixture` tool, then speaks the tool's
    /// result back as an ordinary bubble. What makes `help` and every fixture reachable from the
    /// composer with no provider and no spend.
    ///
    /// The door stays dumb on purpose: it does not know which fixtures exist. The catalogue lives
    /// on the server side of the tool, so there is one list rather than two that can drift.
    catalogue: bool,
    /// The one room member (by the name the system prompt gives it) that reaches for the shell
    /// tool before it speaks — what a test needs to raise a card INSIDE a room and watch the
    /// round continue after the answer. The others behave as `room_speaker`.
    room_tool_asker: Option<String>,
    /// Wait this long before EACH delta, so a mock turn takes observable time.
    ///
    /// WITHOUT THIS THE MOCK DOOR CANNOT SHOW A RUNNING STATE AT ALL. Every path here ends in
    /// `stream::iter`, which is synchronous: the whole answer is already in memory, so a turn
    /// begins and ends inside the same millisecond and `isRunning` flips true then false with no
    /// roster frame in between. The client's green "working" dot is therefore never drawn, and
    /// neither is anything else that needs a turn to still be in flight when a frame is read —
    /// which includes watching an answer arrive progressively.
    ///
    /// `None` by default, and every test relies on that: pacing the suite would add real seconds
    /// to hundreds of turns for no assertion's benefit. It is opt-in for a dev server, where the
    /// point is to LOOK like a model is typing. Per delta rather than per turn because
    /// `echo_script` already emits one delta per word, so the same knob buys word-by-word
    /// arrival for free.
    per_delta_delay: Option<std::time::Duration>,
    /// Hold each model call open for at least this long before its first delta.
    ///
    /// PER-DELTA PACING CANNOT MAKE A SHORT ANSWER VISIBLE. A fixture turn is a tool call and a
    /// one-line acknowledgement — nine deltas, under a second at 90 ms each — and a one-line
    /// fixture is a single delta, so `isRunning` is true for less than one roster round-trip and
    /// the client's green "working" dot never paints. Measured on the packaged app: the row was
    /// complete before the first 330 ms sample. Only a long fixture's FIRST send in a chat ever
    /// held the state long enough to see, which nobody hits twice.
    ///
    /// So the floor is per model call, not per delta: however few deltas a branch emits, the turn
    /// is observably in flight. A tool round counts as a call too, so a fixture turn holds the
    /// state for two floors — which, for the purpose of watching a coworker think, is a feature.
    /// `None` by default and in every test; a dev server opts in through `OG_MOCK_MIN_TURN_MS`.
    turn_floor: Option<std::time::Duration>,
    /// The most time the PACING may add to one model call, however many deltas it has.
    ///
    /// A FLOOR AND A CEILING ANSWER DIFFERENT QUESTIONS. The floor exists because a short answer
    /// was over before the working state could paint. The ceiling exists because a long one is now
    /// chunked word-per-delta, so the same 90 ms that makes a one-liner type turns a 4,632-
    /// character help text into 800 deltas and 79 seconds of watching it arrive — measured on the
    /// dev server, where it read as a stuck turn and was reported as one.
    ///
    /// Lowering the pacing instead would fix the long case by ruining the short one, which is the
    /// case the pacing was added for. So the per-delta pause becomes
    /// `min(per_delta_delay, ceiling / deltas)`: a one-liner is unaffected because its share of the
    /// ceiling is larger than the pause, and a long answer scrolls in a bounded time instead of
    /// crawling. `None` by default and in tests; a dev server opts in with `OG_MOCK_MAX_TURN_MS`.
    turn_ceiling: Option<std::time::Duration>,
}

impl MockDoor {
    /// Pace every delta by `ms`, so a mock turn is observably in flight. See `per_delta_delay`.
    ///
    /// `0` disables it rather than sleeping zero, so a deployment can turn the pacing off by
    /// setting the variable to `0` instead of having to unset it — an unset and an explicit
    /// "off" should not behave differently.
    #[must_use]
    pub fn paced_by_ms(mut self, ms: u64) -> Self {
        self.per_delta_delay = (ms > 0).then(|| std::time::Duration::from_millis(ms));
        self
    }

    /// Hold every model call open for at least `ms` before its first delta. See `turn_floor`.
    /// `0` is off, identical to unset, for the same reason as `paced_by_ms`.
    #[must_use]
    pub fn min_turn_ms(mut self, ms: u64) -> Self {
        self.turn_floor = (ms > 0).then(|| std::time::Duration::from_millis(ms));
        self
    }

    /// Cap what the pacing may add to one model call. See `turn_ceiling`. `0` is off.
    #[must_use]
    pub fn max_turn_ms(mut self, ms: u64) -> Self {
        self.turn_ceiling = (ms > 0).then(|| std::time::Duration::from_millis(ms));
        self
    }

    /// The script as a stream, paced and — unless this is a judge request — floored. Every path
    /// in `stream` ends here, so a new answer shape cannot forget either.
    ///
    /// THE FLOOR IS FOR CALLS A PERSON IS WATCHING. The auto-review judge is a second model call
    /// per tool call, invisible in the transcript, and flooring it multiplies the wait by
    /// something nobody can see: a fixture turn is ask-the-tool, judge, say-it-back, so a 2.5 s
    /// floor became 7.5 s of "Working" for one reply. Excluding the judge makes the visible wait
    /// the number the operator actually set.
    ///
    /// KEYED ON THE REQUEST, NOT ON THE CONFIGURATION. The first version exempted only the
    /// canned-verdict branch, so with no `OG_AUTO_REVIEW_MOCK_VERDICT` — the dev server's actual
    /// state — a judge request fell through to the echo/catalogue branch and paid the floor
    /// after all: the exemption was dead on the one server it was measured on. A judge request is
    /// recognisable by its system prompt whatever branch answers it, so that is what decides.
    fn emit(&self, request: &ModelRequest, script: Vec<ModelDelta>) -> DeltaStream {
        let is_judge = request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with(crate::review::JUDGE_MARKER));
        let floor = if is_judge { None } else { self.turn_floor };
        // THE CEILING IS SHARED OUT ACROSS THE DELTAS, which is why it is computed here rather
        // than counted down as the stream runs: the whole script is already in hand, so the pause
        // that fits can be decided once instead of measured against a clock that a slow consumer
        // would skew. A script of one delta gets the whole ceiling and is therefore unaffected;
        // only a script long enough for its share to fall below the pacing is slowed less.
        let delay = match (self.per_delta_delay, self.turn_ceiling) {
            (Some(pace), Some(ceiling)) if !script.is_empty() => {
                Some(pace.min(ceiling / script.len() as u32))
            }
            (pace, _) => pace,
        };
        if delay.is_none() && floor.is_none() {
            return Box::pin(stream::iter(script.into_iter().map(Ok)));
        }
        Box::pin(
            stream::iter(script)
                .enumerate()
                .then(move |(index, delta)| async move {
                    // The floor is paid once, before the first delta; the pacing before each.
                    if index == 0
                        && let Some(floor) = floor
                    {
                        tokio::time::sleep(floor).await;
                    }
                    if let Some(delay) = delay {
                        tokio::time::sleep(delay).await;
                    }
                    Ok(delta)
                }),
        )
    }

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
            ..Self::default()
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
            // Asks once, then answers — like a turn that actually ends.
            once_then_answer: true,
            ..Self::default()
        }
    }

    /// A door whose every turn hands the person's own words to the `mock_fixture` tool and then
    /// says back what it answered. `OG_MODEL_DOOR=mock-cards` selects it.
    pub fn serving_fixtures() -> Self {
        Self {
            catalogue: true,
            ..Self::default()
        }
    }

    /// The person's last words, which the catalogue door forwards verbatim as a fixture name.
    fn last_user_message(request: &ModelRequest) -> String {
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.trim().to_string())
            .unwrap_or_default()
    }

    /// What the tool answered, read back out of the conversation the harness appended it to.
    fn result_of(request: &ModelRequest, call_id: &str) -> Option<String> {
        let marker = format!("[tool {call_id} result]");
        request.messages.iter().rev().find_map(|message| {
            message
                .content
                .split_once(&marker)
                .map(|(_, tail)| tail.trim().to_string())
        })
    }

    pub fn failing_with(message: impl Into<String>) -> Self {
        Self {
            fail_with: Some(message.into()),
            ..Self::default()
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
            return Ok(self.emit(&request, vec![ModelDelta::Text(word)]));
        }
        // Has this conversation already seen its tool result? The harness appends one as a user
        // message, so the conversation itself is the state.
        let already_ran = request
            .messages
            .iter()
            .any(|message| message.content.contains("[tool "));

        if self.echo_system {
            let said = request.system.clone().unwrap_or_default();
            return Ok(self.emit(&request, vec![ModelDelta::Text(said)]));
        }

        // The catalogue door: ask the fixture tool for whatever the person typed, then speak its
        // answer. Two rounds, keyed off the conversation exactly as `once_then_answer` is — a
        // counter here would be per-process on a shared `Arc` and would work only the first time.
        if self.catalogue {
            const CALL: &str = "mock-fixture-1";
            let script = match Self::result_of(&request, CALL) {
                // The tool has answered; say it back as an ordinary bubble so `help` reads as
                // chat rather than as a tool card.
                //
                // WORD BY WORD, like `echo_script`. One delta carrying the whole fixture makes the
                // pacing knob almost useless here: a single pause before the text is shorter than
                // a roster round-trip plus a paint, so the running state never becomes visible and
                // neither does the answer arriving. Splitting changes nothing a reader can see —
                // the deltas are concatenated on the way to the transcript — and it buys the same
                // observable window the echoing door has.
                Some(answer) => answer
                    .split_inclusive(' ')
                    .map(|word| ModelDelta::Text(word.to_string()))
                    .collect(),
                None => {
                    let asked = Self::last_user_message(&request);
                    vec![
                        ModelDelta::ToolCallStart {
                            id: CALL.to_string(),
                            name: "mock_fixture".to_string(),
                        },
                        ModelDelta::ToolCallArgs {
                            id: CALL.to_string(),
                            delta: serde_json::json!({ "fixture": asked }).to_string(),
                        },
                        ModelDelta::ToolCallEnd {
                            id: CALL.to_string(),
                        },
                    ]
                }
            };
            return Ok(self.emit(&request, script));
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
        Ok(self.emit(&request, script))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn request(text: &str) -> ModelRequest {
        ModelRequest {
            gateway_key: None,
            spend_scope: None,
            spend_actor: None,
            model: "mock".to_string(),
            system: None,
            tools: Vec::new(),
            messages: vec![crate::model::ChatMessage {
                role: "user".to_string(),
                content: text.to_string(),
            }],
        }
    }

    /// Pacing is real, and off unless asked for.
    ///
    /// Asserts a FLOOR on elapsed time, never a ceiling: a sleep may overrun on a loaded machine
    /// but cannot fire early, so this cannot flake the way a "finishes within N ms" assertion
    /// would. The unpaced half is the half that matters for the suite — every other test in the
    /// workspace depends on the default door being instant.
    #[tokio::test]
    async fn a_paced_door_takes_time_and_an_unpaced_one_does_not() {
        let script = vec![
            ModelDelta::Text("one ".to_string()),
            ModelDelta::Text("two ".to_string()),
            ModelDelta::Text("three".to_string()),
        ];

        let started = std::time::Instant::now();
        let paced: Vec<_> = MockDoor::with_script(script.clone())
            .paced_by_ms(20)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        let elapsed = started.elapsed();

        assert_eq!(paced.len(), 3, "pacing must not change what is emitted");
        assert!(
            elapsed >= std::time::Duration::from_millis(60),
            "three deltas at 20ms each cannot arrive in {elapsed:?}"
        );

        // Zero is an explicit off, not a zero-length sleep: an operator turning the pacing off by
        // setting the variable to 0 must get exactly the unset behaviour.
        let started = std::time::Instant::now();
        let instant: Vec<_> = MockDoor::with_script(script)
            .paced_by_ms(0)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        assert_eq!(instant.len(), 3);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "an unpaced door must stay instant — the whole suite depends on it"
        );
    }

    /// The floor holds a one-delta answer open — which per-delta pacing never could.
    #[tokio::test]
    async fn a_floored_door_holds_even_a_one_delta_answer_open() {
        let one = vec![ModelDelta::Text("done".to_string())];

        let started = std::time::Instant::now();
        let out: Vec<_> = MockDoor::with_script(one.clone())
            .min_turn_ms(80)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        assert_eq!(out.len(), 1, "the floor must not change what is emitted");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(80),
            "a single delta cannot arrive before the floor: {:?}",
            started.elapsed()
        );

        let started = std::time::Instant::now();
        let out: Vec<_> = MockDoor::with_script(one)
            .min_turn_ms(0)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        assert_eq!(out.len(), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "zero is off, exactly as unset — the suite depends on it"
        );
    }

    /// The ceiling bounds a long answer without touching a short one.
    ///
    /// This is the whole trade: 90 ms per delta is what makes a one-liner type, and what makes a
    /// 4,632-character help text take 79 seconds once the catalogue answer is chunked
    /// word-per-delta. Lowering the pacing would fix the second by ruining the first, so the pause
    /// is shared out instead.
    #[tokio::test]
    async fn the_ceiling_bounds_a_long_answer_and_leaves_a_short_one_alone() {
        // 200 deltas at 50 ms would be 10 s; the ceiling says 400 ms, so each pause becomes 2 ms.
        let long: Vec<_> = (0..200)
            .map(|i| ModelDelta::Text(format!("w{i} ")))
            .collect();
        let started = std::time::Instant::now();
        let out: Vec<_> = MockDoor::with_script(long)
            .paced_by_ms(50)
            .max_turn_ms(400)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        let elapsed = started.elapsed();
        assert_eq!(
            out.len(),
            200,
            "the ceiling must not change what is emitted"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "200 deltas under a 400ms ceiling cannot take {elapsed:?} — unpaced they would be 10s"
        );

        // A SHORT ANSWER IS UNAFFECTED, which is the half that is easy to lose: its share of the
        // ceiling is far larger than the pause, so `min` picks the pause and it still types.
        let started = std::time::Instant::now();
        let out: Vec<_> = MockDoor::with_script(vec![
            ModelDelta::Text("one ".to_string()),
            ModelDelta::Text("two".to_string()),
        ])
        .paced_by_ms(50)
        .max_turn_ms(4_000)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect()
        .await;
        assert_eq!(out.len(), 2);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(100),
            "two deltas at 50ms still pace: {:?}",
            started.elapsed()
        );

        // Zero is off, exactly as unset — the pacing stands alone.
        let started = std::time::Instant::now();
        let _: Vec<_> = MockDoor::with_script(vec![ModelDelta::Text("x".to_string()); 4])
            .paced_by_ms(40)
            .max_turn_ms(0)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(160),
            "with the ceiling off, four deltas at 40ms is the full 160ms: {:?}",
            started.elapsed()
        );
    }

    /// The judge is never floored, however high the floor is set.
    ///
    /// It is a second model call per tool call and it is invisible in the transcript, so flooring
    /// it multiplies a person's wait by something they cannot see. Without this exclusion a
    /// fixture turn — ask the tool, judge, say it back — pays the floor three times.
    #[tokio::test]
    async fn the_auto_review_judge_is_never_floored() {
        let door = MockDoor::echoing()
            .with_judge_verdict("allow")
            .min_turn_ms(3_000);

        let mut judged = request("anything");
        judged.system = Some(format!(
            "{}\nrest of the prompt",
            crate::review::JUDGE_MARKER
        ));

        let started = std::time::Instant::now();
        let out: Vec<_> = door.stream(judged).await.expect("stream").collect().await;
        assert_eq!(out.len(), 1, "the judge answers in one word");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "the judge must not pay the floor: took {:?}",
            started.elapsed()
        );

        // And an ordinary call on the same door still does.
        let started = std::time::Instant::now();
        let _ = MockDoor::with_script(vec![ModelDelta::Text("hi".to_string())])
            .min_turn_ms(120)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(120),
            "a visible call still pays it"
        );
    }

    /// A judge request is exempt from the floor WHETHER OR NOT a verdict is canned.
    ///
    /// The first version keyed the exemption on the `judge_verdict` branch — so with no
    /// `OG_AUTO_REVIEW_MOCK_VERDICT` (the dev server's actual state) a judge request fell through
    /// to the echo/catalogue branch and paid the floor after all. The exemption was dead on the
    /// one server it was measured on, and the shorter "Working" window seen there came from the
    /// lowered default alone. The exemption has to read the REQUEST, not the configuration.
    #[tokio::test]
    async fn a_judge_request_is_never_floored_even_without_a_canned_verdict() {
        let door = MockDoor::echoing().min_turn_ms(3_000); // no with_judge_verdict on purpose
        let mut judged = request("anything");
        judged.system = Some(format!("{}\nrest", crate::review::JUDGE_MARKER));

        let started = std::time::Instant::now();
        let out: Vec<_> = door.stream(judged).await.expect("stream").collect().await;
        assert!(!out.is_empty(), "the echo branch still answers");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "a judge request must not pay the floor, canned verdict or not: took {:?}",
            started.elapsed()
        );
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
