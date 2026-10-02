//! Turning a model's deltas into a well-formed AG-UI run.
//!
//! THIS IS A STATE MACHINE, NOT A MAP. A model emits fragments in whatever order it likes; AG-UI
//! requires brackets — a message must be started before it is filled and ended before anything
//! else begins, and a run must be closed however it ends. Nothing upstream guarantees that, so it
//! is guaranteed here, and this is the file where the bugs would live if it were done inline.
//!
//! The rules, each of which a test holds:
//!   - `RUN_STARTED` first, exactly once.
//!   - A text fragment opens a message if none is open; every later fragment reuses that message.
//!   - A tool call closes any open text message first — a consumer cannot render a tool line
//!     inside an unterminated bubble.
//!   - Whatever is open when the run ends is closed, in reverse order.
//!   - `RUN_FINISHED` or `RUN_ERROR` last, exactly once, whatever happened. A consumer holds its
//!     spinner open on that promise, so a stream that dies mid-sentence still gets an ending.

use opengrok_wire::agui::{DURATION_MS, Event, EventType};

use crate::model::ModelDelta;

/// What is currently open, so it can be closed before something else opens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Open {
    Nothing,
    Text { message_id: String },
    Reasoning { message_id: String },
    ToolCall { id: String },
}

/// Accumulates deltas and emits correctly-bracketed AG-UI events.
#[derive(Debug, Clone)]
pub struct Projection {
    thread_id: String,
    run_id: String,
    at_ms: i64,
    open: Open,
    started: bool,
    finished: bool,
    /// Said on `RUN_FINISHED` when the run was not simply done (#244).
    finish_reason: Option<opengrok_core::run::FinishReason>,
    /// Said beside the message on `RUN_ERROR`, when the failure has a code (`ModelError::code`).
    fail_code: Option<&'static str>,
    /// Distinguishes the messages of one run from each other.
    message_seq: u32,
}

impl Projection {
    /// A projection for a run that has ALREADY started.
    ///
    /// A resumed run must not emit `RUN_STARTED` a second time: a consumer would draw a new run,
    /// and the log would say a run began twice. `message_seq` continues from where the first half
    /// left off so the two halves cannot collide on a message id.
    pub fn resumed(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        at_ms: i64,
        message_seq: u32,
    ) -> Self {
        let mut projection = Self::new(thread_id, run_id, at_ms);
        projection.started = true;
        projection.message_seq = message_seq;
        projection
    }

    pub fn new(thread_id: impl Into<String>, run_id: impl Into<String>, at_ms: i64) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            at_ms,
            open: Open::Nothing,
            started: false,
            finished: false,
            finish_reason: None,
            fail_code: None,
            message_seq: 0,
        }
    }

    fn event(&self, event_type: EventType) -> Event {
        Event::new(event_type, self.at_ms)
    }

    fn next_message_id(&mut self) -> String {
        self.message_seq += 1;
        format!("msg_{}_{}", self.run_id, self.message_seq)
    }

    /// Emit `RUN_STARTED`. Idempotent: calling twice does not produce two openings.
    pub fn start(&mut self) -> Vec<Event> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![
            self.event(EventType::RunStarted)
                .with("threadId", self.thread_id.clone())
                .with("runId", self.run_id.clone()),
        ]
    }

    /// Close whatever is open, emitting the events that end it.
    fn close_open(&mut self) -> Vec<Event> {
        let events = match &self.open {
            Open::Nothing => Vec::new(),
            Open::Text { message_id } => {
                vec![
                    self.event(EventType::TextMessageEnd)
                        .with("messageId", message_id.clone()),
                ]
            }
            Open::Reasoning { message_id } => {
                vec![
                    self.event(EventType::ReasoningMessageEnd)
                        .with("messageId", message_id.clone()),
                ]
            }
            Open::ToolCall { id } => {
                vec![
                    self.event(EventType::ToolCallEnd)
                        .with("toolCallId", id.clone()),
                ]
            }
        };
        self.open = Open::Nothing;
        events
    }

    /// Feed one delta. Returns the events a client should see for it.
    pub fn push(&mut self, delta: ModelDelta) -> Vec<Event> {
        let mut events = self.start();

        match delta {
            ModelDelta::Text(text) => {
                let message_id = match &self.open {
                    Open::Text { message_id } => message_id.clone(),
                    _ => {
                        events.extend(self.close_open());
                        let message_id = self.next_message_id();
                        events.push(
                            self.event(EventType::TextMessageStart)
                                .with("messageId", message_id.clone())
                                .with("role", "assistant"),
                        );
                        self.open = Open::Text {
                            message_id: message_id.clone(),
                        };
                        message_id
                    }
                };
                events.push(
                    self.event(EventType::TextMessageContent)
                        .with("messageId", message_id)
                        .with("delta", text),
                );
            }

            ModelDelta::Reasoning(text) => {
                let message_id = match &self.open {
                    Open::Reasoning { message_id } => message_id.clone(),
                    _ => {
                        events.extend(self.close_open());
                        let message_id = self.next_message_id();
                        events.push(
                            self.event(EventType::ReasoningMessageStart)
                                .with("messageId", message_id.clone()),
                        );
                        self.open = Open::Reasoning {
                            message_id: message_id.clone(),
                        };
                        message_id
                    }
                };
                events.push(
                    self.event(EventType::ReasoningMessageContent)
                        .with("messageId", message_id)
                        .with("delta", text),
                );
            }

            ModelDelta::ToolCallStart { id, name } => {
                events.extend(self.close_open());
                events.push(
                    self.event(EventType::ToolCallStart)
                        .with("toolCallId", id.clone())
                        .with("toolCallName", name),
                );
                self.open = Open::ToolCall { id };
            }

            ModelDelta::ToolCallArgs { id, delta } => {
                events.push(
                    self.event(EventType::ToolCallArgs)
                        .with("toolCallId", id)
                        .with("delta", delta),
                );
            }

            ModelDelta::ToolCallEnd { id } => {
                // Only clear `open` if this is the call that is open — a stray end for another id
                // must not silently close the wrong thing.
                if self.open == (Open::ToolCall { id: id.clone() }) {
                    self.open = Open::Nothing;
                }
                events.push(self.event(EventType::ToolCallEnd).with("toolCallId", id));
            }
        }

        events
    }

    /// Emit a tool's result.
    ///
    /// `TOOL_CALL_RESULT` carries the tool's own id so a consumer can attach the output to the
    /// call it already drew, rather than showing it as a loose message from nowhere.
    ///
    /// `duration_ms` is the call's own time (`DURATION_MS`, #305), journaled with the frame.
    /// `None` leaves the key out, for a result nothing timed: a number made up for one would
    /// read as a call that ran.
    pub fn push_tool_result(
        &mut self,
        result: &opengrok_tools::ToolResult,
        duration_ms: Option<u64>,
    ) -> Vec<Event> {
        let mut events = self.start();
        // A result belongs after the call it answers, never inside an open message.
        events.extend(self.close_open());
        let mut event = self
            .event(EventType::ToolCallResult)
            .with("toolCallId", result.call_id.clone())
            .with("content", result.content.clone())
            // A refusal is a result the model reads, so whether it succeeded must be legible
            // rather than inferred from the wording.
            .with("ok", result.ok);
        if let Some(ms) = duration_ms {
            event = event.with(DURATION_MS, ms);
        }
        // A screenshot rides the frame. `visibility` tells a client whether this PNG is a
        // chat event it must persist (`transcript` / `failure` / `end`) or only the model's
        // eyes + Computer pane (`agent`). Absent on rows written before this field: treat
        // as `transcript` — those PNGs were already first-class events.
        if let Some(image) = &result.image {
            event = event.with(
                "image",
                serde_json::json!({
                    "mime": image.mime,
                    "base64": image.base64,
                    "width": image.width,
                    "height": image.height,
                    "visibility": image.visibility.as_str(),
                }),
            );
        }
        events.push(event);
        events
    }

    /// Pause: a tool is waiting on a person.
    ///
    /// NOT `finish` AND NOT `fail`. A finished run tells the client there is nothing more coming;
    /// a failed one tells it to give up. This says "stop watching, come back" — the run stays
    /// `running` in the log so it can be picked up when the answer arrives.
    pub fn awaiting_approval(
        &mut self,
        waiting: &opengrok_tools::ToolCall,
        reason: opengrok_tools::AwaitingReason,
        // The sentence the gate gave (a policy grant's reason, the judge's) — what the card
        // shows a person under the summary. `None` when the gate said nothing.
        why: Option<&str>,
    ) -> Vec<Event> {
        let mut events = self.start();
        if self.finished {
            return Vec::new();
        }
        events.extend(self.close_open());
        // Deliberately NOT setting `finished`: the run has not ended, and a later answer must be
        // able to add to it.
        let arguments = if reason == opengrok_tools::AwaitingReason::UserForm {
            opengrok_tools::user_form::sanitize_arguments(&waiting.arguments)
        } else {
            waiting.arguments.clone()
        };
        let event = self
            .event(EventType::Custom)
            .with("name", "run-awaiting-approval")
            .with("threadId", self.thread_id.clone())
            .with("runId", self.run_id.clone())
            // WHICH call, and with what arguments. A person asked to approve "shell" without
            // seeing the command is being asked to approve nothing, and an answer that cannot
            // name its call cannot be exactly-once.
            .with("callId", waiting.id.clone())
            .with("tool", waiting.name.clone())
            .with("arguments", arguments.clone())
            // WHY: which card the gateway raises and which verb may answer it. Absent on rows
            // written before reasons existed, which the reader treats as exec-consent.
            .with("reason", reason.as_str())
            // The gate's own words, for the card. Empty when it had none.
            .with("why", why.unwrap_or_default());
        events.push(event);
        events
    }

    /// A CUSTOM frame that does not close the run. Used for operator metadata
    /// (`run-timing`) that must sit *before* `RUN_FINISHED` / `RUN_ERROR`.
    pub fn custom(&self, name: &str, value: serde_json::Value) -> Event {
        self.event(EventType::Custom)
            .with("name", name)
            .with("threadId", self.thread_id.clone())
            .with("runId", self.run_id.clone())
            .with("value", value)
    }

    /// End the run cleanly.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut events = self.start();
        if self.finished {
            return Vec::new();
        }
        events.extend(self.close_open());
        self.finished = true;
        let mut finished = self
            .event(EventType::RunFinished)
            .with("threadId", self.thread_id.clone())
            .with("runId", self.run_id.clone());
        if let Some(reason) = self.finish_reason {
            finished = finished.with("reason", reason.as_str());
        }
        events.push(finished);
        events
    }

    /// The `RUN_FINISHED` this projection closes with will say why (#244). The journal reads the
    /// word off the frame into the run's `Finished`, so the log and the stream say the same.
    pub fn finishing_because(&mut self, reason: opengrok_core::run::FinishReason) {
        self.finish_reason = Some(reason);
    }

    /// The `RUN_ERROR` this projection fails with will carry `code` beside its message, as AG-UI's
    /// `RunErrorEvent` does: a client says which of a relay's failures it was (`relay_offline`…).
    pub fn failing_with(&mut self, code: Option<&'static str>) {
        self.fail_code = code;
    }

    /// One frame before the first box-bound tool of a turn starts a sleeping box, so a client
    /// can say "waking the computer" for the wait instead of "working". Closes an open message
    /// first, like every other frame that is not text.
    pub fn box_waking(&mut self, coworker_id: &str) -> Vec<Event> {
        let mut events = self.start();
        if self.finished {
            return Vec::new();
        }
        events.extend(self.close_open());
        events.push(
            self.event(EventType::Custom)
                .with("name", "box-waking")
                .with("threadId", self.thread_id.clone())
                .with("runId", self.run_id.clone())
                .with("coworkerId", coworker_id.to_string()),
        );
        events
    }

    /// End the run because a person stopped it.
    ///
    /// TWO FRAMES, AND BOTH ARE NEEDED. AG-UI has no `RUN_STOPPED`, and the two endings it does
    /// have both say the wrong thing on their own: `RUN_ERROR` paints a failure the coworker did
    /// not commit, and a bare `RUN_FINISHED` claims the turn ran to completion. So the reason
    /// travels as a `CUSTOM` frame — the same way `run-awaiting-approval` does — and `RUN_FINISHED`
    /// follows it to close the stream, because a consumer holds its spinner open on that promise
    /// and a stop that leaves the dots turning is not a stop anybody can see.
    pub fn stopped(&mut self) -> Vec<Event> {
        let mut events = self.start();
        if self.finished {
            return Vec::new();
        }
        events.extend(self.close_open());
        self.finished = true;
        events.push(
            self.event(EventType::Custom)
                .with("name", "run-stopped")
                .with("threadId", self.thread_id.clone())
                .with("runId", self.run_id.clone()),
        );
        events.push(
            self.event(EventType::RunFinished)
                .with("threadId", self.thread_id.clone())
                .with("runId", self.run_id.clone()),
        );
        events
    }

    /// End the run badly. Still closes what is open first: a consumer that never receives the end
    /// of a message it was told about renders a bubble that streams forever.
    pub fn fail(&mut self, message: impl Into<String>) -> Vec<Event> {
        let mut events = self.start();
        if self.finished {
            return Vec::new();
        }
        events.extend(self.close_open());
        self.finished = true;
        let mut failed = self
            .event(EventType::RunError)
            .with("threadId", self.thread_id.clone())
            .with("runId", self.run_id.clone())
            .with("message", message.into());
        if let Some(code) = self.fail_code {
            failed = failed.with("code", code);
        }
        events.push(failed);
        events
    }

    /// Replace an ending the log refused with the one that is true: the run could not be recorded.
    ///
    /// NOT A SECOND ENDING. `close` emits an ending only once the log holds it, so the refused one
    /// was never shown and this is the only ending the run gets. The brackets it closed stay
    /// closed and its `run-timing` stays (one CUSTOM, then the closer); its cards and its stop
    /// notice go — a card for a suspension the log never got answers 409 — and its terminal
    /// becomes the one `RUN_ERROR` (`formal/lean/Harness.lean` Close.unrecorded_one_terminal).
    pub fn unrecorded(&self, refused: Vec<Event>, message: impl Into<String>) -> Vec<Event> {
        let mut events: Vec<Event> = refused
            .into_iter()
            .filter(|event| match event.event_type {
                EventType::TextMessageEnd
                | EventType::ReasoningMessageEnd
                | EventType::ToolCallEnd => true,
                EventType::Custom => {
                    event.extra.get("name").and_then(|name| name.as_str())
                        == Some(crate::timing::RUN_TIMING_NAME)
                }
                _ => false,
            })
            .collect();
        events.push(
            self.event(EventType::RunError)
                .with("threadId", self.thread_id.clone())
                .with("runId", self.run_id.clone())
                .with("message", message.into()),
        );
        events
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/projection_tests.rs"]
mod tests;
