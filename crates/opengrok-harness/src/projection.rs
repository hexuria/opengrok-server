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

use opengrok_wire::agui::{Event, EventType};

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
#[derive(Debug)]
pub struct Projection {
    thread_id: String,
    run_id: String,
    at_ms: i64,
    open: Open,
    started: bool,
    finished: bool,
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
    pub fn push_tool_result(&mut self, result: &opengrok_tools::ToolResult) -> Vec<Event> {
        let mut events = self.start();
        // A result belongs after the call it answers, never inside an open message.
        events.extend(self.close_open());
        events.push(
            self.event(EventType::ToolCallResult)
                .with("toolCallId", result.call_id.clone())
                .with("content", result.content.clone())
                // A refusal is a result the model reads, so whether it succeeded must be legible
                // rather than inferred from the wording.
                .with("ok", result.ok),
        );
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
    ) -> Vec<Event> {
        let mut events = self.start();
        if self.finished {
            return Vec::new();
        }
        events.extend(self.close_open());
        // Deliberately NOT setting `finished`: the run has not ended, and a later answer must be
        // able to add to it.
        events.push(
            self.event(EventType::Custom)
                .with("name", "run-awaiting-approval")
                .with("threadId", self.thread_id.clone())
                .with("runId", self.run_id.clone())
                // WHICH call, and with what arguments. A person asked to approve "shell" without
                // seeing the command is being asked to approve nothing, and an answer that cannot
                // name its call cannot be exactly-once.
                .with("callId", waiting.id.clone())
                .with("tool", waiting.name.clone())
                .with("arguments", waiting.arguments.clone())
                // WHY: which card the gateway raises and which verb may answer it. Absent on rows
                // written before reasons existed, which the reader treats as exec-consent.
                .with("reason", reason.as_str()),
        );
        events
    }

    /// End the run cleanly.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut events = self.start();
        if self.finished {
            return Vec::new();
        }
        events.extend(self.close_open());
        self.finished = true;
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
mod tests {
    use super::*;

    fn types(events: &[Event]) -> Vec<EventType> {
        events.iter().map(|event| event.event_type).collect()
    }

    fn run(deltas: Vec<ModelDelta>) -> Vec<Event> {
        let mut projection = Projection::new("t1", "r1", 100);
        let mut events = Vec::new();
        for delta in deltas {
            events.extend(projection.push(delta));
        }
        events.extend(projection.finish());
        events
    }

    #[test]
    fn a_single_text_fragment_becomes_a_whole_message() {
        let events = run(vec![ModelDelta::Text("hello".to_string())]);
        assert_eq!(
            types(&events),
            vec![
                EventType::RunStarted,
                EventType::TextMessageStart,
                EventType::TextMessageContent,
                EventType::TextMessageEnd,
                EventType::RunFinished,
            ]
        );
    }

    /// The point of streaming: many fragments, one message — not one message per fragment.
    #[test]
    fn consecutive_text_fragments_share_one_message() {
        let events = run(vec![
            ModelDelta::Text("one ".to_string()),
            ModelDelta::Text("two ".to_string()),
            ModelDelta::Text("three".to_string()),
        ]);
        assert_eq!(
            types(&events),
            vec![
                EventType::RunStarted,
                EventType::TextMessageStart,
                EventType::TextMessageContent,
                EventType::TextMessageContent,
                EventType::TextMessageContent,
                EventType::TextMessageEnd,
                EventType::RunFinished,
            ]
        );
        let ids: Vec<_> = events
            .iter()
            .filter_map(|event| event.extra.get("messageId"))
            .collect();
        assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    }

    /// A tool line inside an unterminated bubble is a rendering bug in every consumer.
    #[test]
    fn a_tool_call_closes_the_open_text_message_first() {
        let events = run(vec![
            ModelDelta::Text("thinking about it".to_string()),
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: "{\"cmd\":".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "c1".to_string(),
                delta: "\"ls\"}".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
        ]);
        assert_eq!(
            types(&events),
            vec![
                EventType::RunStarted,
                EventType::TextMessageStart,
                EventType::TextMessageContent,
                EventType::TextMessageEnd,
                EventType::ToolCallStart,
                EventType::ToolCallArgs,
                EventType::ToolCallArgs,
                EventType::ToolCallEnd,
                EventType::RunFinished,
            ]
        );
    }

    /// Text after a tool call is a NEW message, not a resumption of the closed one.
    #[test]
    fn text_after_a_tool_call_opens_a_second_message() {
        let events = run(vec![
            ModelDelta::Text("before".to_string()),
            ModelDelta::ToolCallStart {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "c1".to_string(),
            },
            ModelDelta::Text("after".to_string()),
        ]);
        let ids: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == EventType::TextMessageStart)
            .filter_map(|event| event.extra.get("messageId").and_then(|id| id.as_str()))
            .collect();
        assert_eq!(ids.len(), 2, "two messages");
        assert_ne!(ids[0], ids[1], "and they must be told apart");
    }

    #[test]
    fn reasoning_and_text_do_not_share_a_message() {
        let events = run(vec![
            ModelDelta::Reasoning("hmm".to_string()),
            ModelDelta::Text("answer".to_string()),
        ]);
        assert_eq!(
            types(&events),
            vec![
                EventType::RunStarted,
                EventType::ReasoningMessageStart,
                EventType::ReasoningMessageContent,
                EventType::ReasoningMessageEnd,
                EventType::TextMessageStart,
                EventType::TextMessageContent,
                EventType::TextMessageEnd,
                EventType::RunFinished,
            ]
        );
    }

    /// A stream that dies mid-sentence must still close its message and end its run, or the
    /// consumer streams a bubble forever.
    #[test]
    fn a_failure_closes_what_is_open_and_still_ends_the_run() {
        let mut projection = Projection::new("t1", "r1", 100);
        let mut events = projection.push(ModelDelta::Text("half a sen".to_string()));
        events.extend(projection.fail("upstream hung up"));
        assert_eq!(
            types(&events),
            vec![
                EventType::RunStarted,
                EventType::TextMessageStart,
                EventType::TextMessageContent,
                EventType::TextMessageEnd,
                EventType::RunError,
            ]
        );
        let last = events.last().unwrap();
        assert_eq!(last.extra.get("message").unwrap(), "upstream hung up");
    }

    /// A run with nothing in it is still a run. An empty stream must not hang a client.
    #[test]
    fn an_empty_run_still_opens_and_closes() {
        let events = run(vec![]);
        assert_eq!(
            types(&events),
            vec![EventType::RunStarted, EventType::RunFinished]
        );
    }

    #[test]
    fn finishing_twice_does_not_end_the_run_twice() {
        let mut projection = Projection::new("t1", "r1", 100);
        let first = projection.finish();
        let second = projection.finish();
        assert_eq!(first.len(), 2, "started + finished");
        assert!(second.is_empty(), "{second:?}");
    }

    /// Once a run has ended, a late error must not append a second ending.
    #[test]
    fn a_failure_after_finishing_is_ignored() {
        let mut projection = Projection::new("t1", "r1", 100);
        projection.finish();
        assert!(projection.fail("too late").is_empty());
    }

    /// A suspended run is neither finished nor failed, and must still be addable to.
    #[test]
    fn awaiting_approval_leaves_the_run_open() {
        let mut projection = Projection::new("t1", "r1", 100);
        projection.push(ModelDelta::Text("about to run something".to_string()));
        let waiting = projection.awaiting_approval(
            &opengrok_tools::ToolCall {
                id: "c1".to_string(),
                name: "shell".to_string(),
                arguments: serde_json::Value::Null,
            },
            opengrok_tools::AwaitingReason::ExecConsent,
        );

        // The open message is closed, so nothing streams forever.
        assert!(types(&waiting).contains(&EventType::TextMessageEnd));
        assert_eq!(waiting.last().unwrap().event_type, EventType::Custom);
        assert_eq!(
            waiting.last().unwrap().extra.get("name").unwrap(),
            "run-awaiting-approval"
        );

        // And the run can still be finished later, when the answer arrives.
        let finished = projection.finish();
        assert_eq!(
            finished.last().unwrap().event_type,
            EventType::RunFinished,
            "a suspended run must still be finishable"
        );
    }

    /// A stray end for a call that is not open must not close whatever is.
    #[test]
    fn an_unmatched_tool_call_end_does_not_close_an_open_message() {
        let mut projection = Projection::new("t1", "r1", 100);
        projection.push(ModelDelta::Text("open".to_string()));
        projection.push(ModelDelta::ToolCallEnd {
            id: "not-open".to_string(),
        });
        // The text message is still open, so finishing must close it.
        let tail = projection.finish();
        assert_eq!(
            types(&tail),
            vec![EventType::TextMessageEnd, EventType::RunFinished]
        );
    }

    /// Every run carries the client's ids on both ends, so a consumer can correlate them.
    #[test]
    fn the_run_is_bracketed_by_the_clients_ids() {
        let events = run(vec![ModelDelta::Text("x".to_string())]);
        for event in [events.first().unwrap(), events.last().unwrap()] {
            assert_eq!(event.extra.get("threadId").unwrap(), "t1");
            assert_eq!(event.extra.get("runId").unwrap(), "r1");
        }
    }
}
