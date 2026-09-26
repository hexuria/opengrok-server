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
    /// Answers with nothing at all: no words, no tool calls, no error. What a provider that
    /// returns an empty completion looks like from here, which is the case a run must not
    /// finish silently on.
    silent: bool,
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
    /// Answers with the system prompt it was given, so a test can assert what the model was
    /// actually told rather than what the code meant to tell it.
    echo_system: bool,
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
    /// When set with a user-form script: a later turn whose last user message is not a
    /// sign-in ask is echoed as steer instead of raising the form again.
    echo_steer: bool,
    /// The most time the PACING may add to one model call, however many deltas it has.
    ///
    /// A FLOOR AND A CEILING ANSWER DIFFERENT QUESTIONS. The floor exists because a short answer
    /// was over before the working state could paint. The ceiling exists because a long one is now
    /// chunked word-per-delta, so the same 90 ms that makes a one-liner type turns a 4,632-
    /// character help text into ~795 deltas. The pacing alone is ~72 s of that; the 79 s actually
    /// measured on the dev server is the whole turn, floors and tool round included. It read as a
    /// stuck turn and was reported as one.
    ///
    /// Lowering the pacing instead would fix the long case by ruining the short one, which is the
    /// case the pacing was added for. So the per-delta pause becomes
    /// `min(per_delta_delay, ceiling / deltas)` — see `paced_pause`, which owns the arithmetic and
    /// documents what it does NOT promise. `None` by default and in tests; a dev server opts in
    /// with `OG_MOCK_MAX_TURN_MS`.
    ///
    /// IT CAPS THE PACING, NOT THE CALL. The floor is paid on top, so a floored call's worst case
    /// is `floor + ceiling`, and a turn is several calls. The name is the shortest true thing;
    /// `serve.sh` prints both numbers for the same reason.
    turn_ceiling: Option<std::time::Duration>,
}

/// The pause before each delta: the pacing, reduced so the whole script fits the ceiling.
///
/// SEPARATE AND PURE SO IT CAN BE TESTED EXACTLY. Asserting this through the clock means asserting
/// a CEILING on elapsed time, which this file's own convention forbids — a sleep may overrun on a
/// loaded machine but can never fire early, so a "finished within N" assertion can only fail
/// spuriously. As arithmetic it is checked precisely instead, and a threshold loose enough not to
/// flake is not needed.
///
/// SHARED OUT, NOT COUNTED DOWN: the whole script is in hand before the first delta, so the pause
/// that fits is decided once rather than measured against a clock a slow consumer would skew.
///
/// TWO THINGS THIS DOES NOT PROMISE, both of which an earlier comment here claimed:
///
/// - A one-liner is unaffected only while `ceiling >= pace`. Set a ceiling BELOW the pacing and
///   every answer is hurried, including the short one the pacing exists for — `min` has no opinion
///   about which knob the operator meant. That is a legitimate way to configure it and the
///   arithmetic is honest, but it is not "one-liners are never touched".
/// - The total is not an exact bound. `tokio::time::sleep` rounds each deadline up to the next
///   timer tick, so a share below that tick is realised as the tick: 4,000 deltas sharing 6 s is
///   1.5 ms each, realised as ~2 ms, which overshoots by a third. It bounds the runaway case it
///   was added for — 79 s down to seconds — and it is not a guarantee.
fn paced_pause(
    pace: Option<std::time::Duration>,
    ceiling: Option<std::time::Duration>,
    deltas: usize,
) -> Option<std::time::Duration> {
    let pace = pace?;
    let Some(ceiling) = ceiling else {
        return Some(pace);
    };
    // An empty script is reachable — a door can answer with no deltas at all — and `Duration / 0`
    // panics, which is denied workspace-wide. It also has nothing to pace, so the pacing is simply
    // whatever it was.
    let deltas = u32::try_from(deltas).unwrap_or(u32::MAX);
    if deltas == 0 {
        return Some(pace);
    }
    Some(pace.min(ceiling / deltas))
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

    /// The script as a stream: paced per delta, reduced to fit the ceiling, and — unless this is
    /// a judge request — floored once before the first delta. Every path in `stream` ends here, so
    /// a new answer shape cannot forget any of the three.
    ///
    /// THE FLOOR IS FOR CALLS A PERSON IS WATCHING. The auto-review judge is a second model call
    /// per tool call, invisible in the transcript, and flooring it multiplies the wait by
    /// something nobody can see: a tool turn is ask-the-tool, judge, say-it-back, so a 2.5 s
    /// floor became 7.5 s of "Working" for one reply. Excluding the judge makes the visible wait
    /// the number the operator actually set.
    ///
    /// KEYED ON THE REQUEST, NOT ON THE CONFIGURATION. The first version exempted only the
    /// canned-verdict branch, so with no `OG_AUTO_REVIEW_MOCK_VERDICT` — the dev server's actual
    /// state — a judge request fell through to the echo branch and paid the floor
    /// after all: the exemption was dead on the one server it was measured on. A judge request is
    /// recognisable by its system prompt whatever branch answers it, so that is what decides.
    fn emit(&self, request: &ModelRequest, script: Vec<ModelDelta>) -> DeltaStream {
        let is_judge = request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with(crate::review::JUDGE_MARKER));
        let floor = if is_judge { None } else { self.turn_floor };
        let delay = paced_pause(self.per_delta_delay, self.turn_ceiling, script.len());
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

    /// A door that says back its own system prompt. The composition of identity, standing role
    /// and machine discipline is only correct if it ARRIVES, and every other door hides it.
    pub fn echoing_the_system_prompt() -> Self {
        Self {
            echo_system: true,
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

    /// A door that raises an in-chat `user-form`, then stops until the person answers.
    pub fn asking_for_user_form() -> Self {
        Self {
            script: Self::user_form_script(),
            once_then_answer: true,
            ..Self::default()
        }
    }

    /// Same form card, but a later turn whose last user text is not a sign-in ask is
    /// echoed as steer — what a new `sendPrompt` / AG-UI message while Waiting must do.
    pub fn asking_for_user_form_until_steered() -> Self {
        Self {
            script: Self::user_form_script(),
            once_then_answer: true,
            echo_steer: true,
            ..Self::default()
        }
    }

    /// Three `request_user_form` TOOL_CALLs in one completion — NativeChat stacked
    /// Website login cards. Each must get its own gateway `entryId`.
    pub fn asking_for_stacked_user_forms() -> Self {
        Self {
            script: Self::stacked_user_form_script(),
            once_then_answer: true,
            ..Self::default()
        }
    }

    /// Two same-title Website logins with provider-style parallel ids (`call-…` / `call-…-1`).
    /// NativeChat painted the second card with the raw tool-call id when live TOOL_CALL
    /// frames streamed before the gateway `e_*` stamp.
    pub fn asking_for_two_website_logins() -> Self {
        Self {
            script: Self::two_website_login_script(),
            once_then_answer: true,
            ..Self::default()
        }
    }

    fn user_form_script() -> Vec<ModelDelta> {
        let mut script = vec![ModelDelta::Text("I need you to sign in".to_string())];
        script.extend(Self::user_form_call(
            "mock-form-1",
            "Google account",
            "Enter the address and password.",
        ));
        script
    }

    fn stacked_user_form_script() -> Vec<ModelDelta> {
        let mut script = vec![ModelDelta::Text("I need you to sign in".to_string())];
        for index in 1..=3 {
            script.extend(Self::user_form_call(
                &format!("mock-form-{index}"),
                "Website login",
                "Enter the address and password.",
            ));
        }
        script
    }

    fn two_website_login_script() -> Vec<ModelDelta> {
        let mut script = vec![ModelDelta::Text("I need you to sign in".to_string())];
        script.extend(Self::user_form_call(
            "call-42628be6",
            "Website login",
            "Enter the address and password.",
        ));
        script.extend(Self::user_form_call(
            "call-42628be6-1",
            "Website login",
            "Enter the address and password.",
        ));
        script
    }

    fn user_form_call(id: &str, title: &str, instruction: &str) -> Vec<ModelDelta> {
        vec![
            ModelDelta::ToolCallStart {
                id: id.to_string(),
                name: opengrok_tools::REQUEST_USER_FORM.to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: id.to_string(),
                // `values` is smuggled the way a model might; the card and the run log
                // must drop it so a password never sits on the entry.
                delta: serde_json::json!({
                    "title": title,
                    "instruction": instruction,
                    "fields": [
                        {
                            "id": "email",
                            "label": "Email",
                            "type": "email",
                            "required": true
                        },
                        {
                            "id": "password",
                            "label": "Password",
                            "type": "password",
                            "required": true
                        }
                    ],
                    "liveHost": "accounts.google.com",
                    "values": { "password": "s3cret-should-never-land" }
                })
                .to_string(),
            },
            ModelDelta::ToolCallEnd { id: id.to_string() },
        ]
    }

    /// The person's last words, trimmed.
    fn last_user_message(request: &ModelRequest) -> String {
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.trim().to_string())
            .unwrap_or_default()
    }

    pub fn failing_with(message: impl Into<String>) -> Self {
        Self {
            fail_with: Some(message.into()),
            ..Self::default()
        }
    }

    /// A door that opens and says nothing. Not the same as a door that fails: the HTTP call
    /// succeeded, and the run has to decide what an empty answer means.
    pub fn silent() -> Self {
        Self {
            silent: true,
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
        if self.silent {
            return Ok(Box::pin(stream::empty()));
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
        // Has this conversation already seen its tool result? The harness appends one after each
        // round, so the conversation itself is the state. Read in the words a tool result has
        // always been written in, so a result a client sent as text counts the same.
        let already_ran = request
            .messages
            .iter()
            .any(|message| message.as_text().contains("[tool "));

        if self.echo_system {
            let said = request.system.clone().unwrap_or_default();
            return Ok(self.emit(&request, vec![ModelDelta::Text(said)]));
        }

        let script = if self.script.is_empty()
            || (self.echo_steer
                && !Self::last_user_message(&request)
                    .to_ascii_lowercase()
                    .contains("sign in"))
        {
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
            context_tokens: None,
            model: "mock".to_string(),
            system: None,
            tools: Vec::new(),
            messages: vec![crate::model::ChatMessage::text("user", text.to_string())],
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

    /// The pause the ceiling produces, checked as arithmetic rather than by the clock.
    ///
    /// The earlier version of this test asserted `elapsed < 4s` for 200 deltas. That is a CEILING
    /// on elapsed time, which the test above forbids in as many words — a sleep can overrun on a
    /// loaded machine but never fire early, so such an assertion can only fail spuriously. It was
    /// also too loose to be worth the risk: with 0.4 s expected and 10 s unfixed, a 4 s threshold
    /// caught only the ceiling being ignored ENTIRELY, and would have passed a regression that
    /// divided by `len/2`, or used `max` instead of `min`, or overshot fivefold.
    ///
    /// The arithmetic is exact and has no clock in it, so it can assert the precise value.
    #[test]
    fn the_pause_is_the_pacing_reduced_to_fit_the_ceiling() {
        let ms = std::time::Duration::from_millis;

        // The case this exists for: 200 deltas that would take 10 s, held to 400 ms.
        assert_eq!(paced_pause(Some(ms(50)), Some(ms(400)), 200), Some(ms(2)));

        // A SHORT ANSWER KEEPS ITS PACING. `min` picks the pause because the share is bigger —
        // and this is the assertion that distinguishes the fix from its inverse: with `max` the
        // answer would be 2 s, and with the ceiling ignored it would still be 50 ms, so only an
        // exact check tells the three apart.
        assert_eq!(paced_pause(Some(ms(50)), Some(ms(4_000)), 2), Some(ms(50)));

        // Each knob alone behaves as it did before the other existed.
        assert_eq!(paced_pause(Some(ms(40)), None, 4), Some(ms(40)));
        assert_eq!(paced_pause(None, Some(ms(400)), 4), None);
        assert_eq!(paced_pause(None, None, 4), None);

        // An empty script is REACHABLE — a door can answer with no deltas — and `Duration / 0`
        // panics, which is denied workspace-wide.
        assert_eq!(paced_pause(Some(ms(40)), Some(ms(400)), 0), Some(ms(40)));

        // A ceiling BELOW the pacing hurries everything, including a one-liner. Honest arithmetic
        // rather than a promise: an operator who tightens the ceiling to fix long answers does
        // reach the short ones, and this pins it so nobody has to rediscover it.
        assert_eq!(paced_pause(Some(ms(90)), Some(ms(50)), 1), Some(ms(50)));
    }

    /// The ceiling still bounds a real stream, and a short one still takes its time.
    ///
    /// Only FLOORS on elapsed time here, per this file's convention: the long case asserts the
    /// deltas all arrived (which the arithmetic test cannot observe), and the short case asserts
    /// it was not hurried. Neither can fail on a slow machine.
    #[tokio::test]
    async fn the_ceiling_leaves_a_short_answer_paced() {
        let long: Vec<_> = (0..200)
            .map(|i| ModelDelta::Text(format!("w{i} ")))
            .collect();
        let out: Vec<_> = MockDoor::with_script(long)
            .paced_by_ms(50)
            .max_turn_ms(400)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        assert_eq!(
            out.len(),
            200,
            "the ceiling must not change what is emitted"
        );

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
            "a short answer under a generous ceiling still paces: {:?}",
            started.elapsed()
        );
    }

    /// The shipped configuration: a floor AND a ceiling, which is what `serve.sh` sets and the
    /// only combination a dev server ever runs — and which no test covered.
    #[tokio::test]
    async fn a_floored_and_capped_call_pays_the_floor_on_top_of_the_capped_pacing() {
        let script: Vec<_> = (0..100)
            .map(|i| ModelDelta::Text(format!("w{i} ")))
            .collect();
        let started = std::time::Instant::now();
        let out: Vec<_> = MockDoor::with_script(script)
            .paced_by_ms(50)
            .max_turn_ms(200)
            .min_turn_ms(150)
            .stream(request("hi"))
            .await
            .expect("stream")
            .collect()
            .await;
        assert_eq!(out.len(), 100);
        // THE FLOOR IS PAID ON TOP OF THE CEILING, not inside it — the knob caps the pacing, not
        // the call. A floor assertion, so a slow machine cannot fail it.
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(150),
            "the floor is still paid when a ceiling is set: {:?}",
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
    /// to the echo branch and paid the floor after all. The exemption was dead on the
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
