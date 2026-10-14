//! The journal and the sink: every event a client saw is recorded; a refused write is told.

use super::*;

/// A run that cannot be recorded must not proceed: it would produce work a reconnect can never
/// reproduce, which is the failure this design exists to prevent.
#[tokio::test]
async fn a_run_that_cannot_be_recorded_does_not_run() {
    struct BrokenJournal;
    #[async_trait::async_trait]
    impl RunJournal for BrokenJournal {
        async fn record(&self, _run_id: &str, _events: &[Event]) -> Result<(), JournalError> {
            Err(JournalError::Unwritable("the disk is gone".to_string()))
        }
    }

    let events = run_conversation(
        &MockDoor::echoing(),
        None,
        &BrokenJournal,
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(
        events.last().unwrap().event_type,
        opengrok_wire::agui::EventType::RunError
    );
    // Nothing was said: the model was never called.
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == opengrok_wire::agui::EventType::TextMessageContent)
    );
}

/// Everything a client saw is in the journal — that is what makes a replay complete.
#[tokio::test]
async fn every_event_a_client_saw_reached_the_journal() {
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &MockDoor::echoing(),
        None,
        &journal,
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(journal.event_count(), events.len());
}

/// Exactly one ending, however the run went — two would double-render in a consumer.
#[tokio::test]
async fn a_run_has_exactly_one_ending() {
    for door in [MockDoor::echoing(), MockDoor::failing_with("nope")] {
        let events = run_turn(&door, request("hello"), "t1", "r1", 1).await;
        let endings = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    EventType::RunFinished | EventType::RunError
                )
            })
            .count();
        assert_eq!(endings, 1, "{:?}", events.last());
    }
}

/// NativeChat paints POST /ag-ui from this sink. If the sink is a subset of the Vec, the
/// HTTP body is missing the opening, the close, or both — which looks like a client that
/// never streamed.
#[tokio::test]
async fn a_streaming_sink_sees_every_event_the_run_produced() {
    struct Collect(std::sync::Mutex<Vec<Event>>);
    #[async_trait::async_trait]
    impl EventSink for Collect {
        async fn emit(&self, events: &[Event]) {
            if let Ok(mut seen) = self.0.lock() {
                seen.extend(events.iter().cloned());
            }
        }
    }

    let sink = Collect(std::sync::Mutex::new(Vec::new()));
    let events = run_conversation_streaming(
        &MockDoor::echoing(),
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
        &sink,
    )
    .await;
    let sunk = sink.0.lock().unwrap().clone();
    let produced: Vec<_> = events.iter().map(|event| event.event_type).collect();
    let live: Vec<_> = sunk.iter().map(|event| event.event_type).collect();
    assert_eq!(live, produced, "sink={live:?} vec={produced:?}");
}

/// A paced door must deliver the first word while the run is still in flight. If this
/// fires only after `run_conversation_streaming` joins, POST /ag-ui would still look
/// like a buffered JSON response.
#[tokio::test]
async fn a_paced_sink_receives_text_while_the_model_is_still_talking() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    struct FirstText(std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    #[async_trait::async_trait]
    impl EventSink for FirstText {
        async fn emit(&self, events: &[Event]) {
            if events
                .iter()
                .any(|event| event.event_type == EventType::TextMessageContent)
                && let Some(tx) = self.0.lock().ok().and_then(|mut slot| slot.take())
            {
                let _ = tx.send(());
            }
        }
    }

    let sink = FirstText(std::sync::Mutex::new(Some(tx)));
    let handle = tokio::spawn(async move {
        run_conversation_streaming(
            &MockDoor::echoing().paced_by_ms(40),
            None,
            &MemoryJournal::new(),
            request("hello from a paced mock door"),
            "t1",
            "r1",
            1,
            &sink,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("first text should arrive before the run ends")
        .unwrap();
    assert!(
        !handle.is_finished(),
        "text arrived only after the turn finished — the HTTP body would dump at the end"
    );
    let events = handle.await.unwrap();
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
}

/// Refuses every write that carries the event `refuse` picks, and takes the rest.
struct Refusing(fn(&Event) -> bool);
#[async_trait::async_trait]
impl RunJournal for Refusing {
    async fn record(&self, _run_id: &str, events: &[Event]) -> Result<(), JournalError> {
        if events.iter().any(self.0) {
            return Err(JournalError::Unwritable("the disk is full".to_string()));
        }
        Ok(())
    }
}

/// AN ENDING THE LOG REFUSED IS NEVER SHOWN (`formal/tla/HarnessLoop.tla` ToldIsTrue). The ending
/// used to be emitted before its write and the write's error ignored, so a person was told the
/// run finished while the log still said it was running. Now they are told the one true thing,
/// once, and the words already said stay said.
#[tokio::test]
async fn an_ending_the_journal_refused_is_told_as_unrecorded() {
    let journal = Refusing(|event| {
        matches!(
            event.event_type,
            EventType::RunFinished | EventType::RunError
        )
    });
    let sink = Collect(Mutex::new(Vec::new()));
    let events = run_conversation_streaming(
        &MockDoor::echoing(),
        None,
        &journal,
        request("hello"),
        "t1",
        "r1",
        1,
        &sink,
    )
    .await;
    let live = sink.0.lock().unwrap().clone();
    for seen in [&events, &live] {
        let ends = endings(seen);
        assert_eq!(ends.len(), 1, "exactly one ending: {seen:?}");
        assert_eq!(ends[0].event_type, EventType::RunError, "{seen:?}");
        let message = ends[0].extra.get("message").and_then(|m| m.as_str());
        assert!(
            message.is_some_and(|m| m.contains("could not be recorded")),
            "{message:?}"
        );
    }
    assert!(assistant_text(&live).contains("hello"), "{live:?}");
}

/// A CARD THE LOG NEVER GOT IS NEVER SHOWN. Answering a card whose suspension was not recorded
/// is a 409, so a card painted from an unrecorded park was a button that could not work.
#[tokio::test]
async fn a_park_the_journal_refused_shows_no_card() {
    let journal = Refusing(is_awaiting_card);
    let sink = Collect(Mutex::new(Vec::new()));
    let events = run_conversation_streaming(
        &MockDoor::asking_for_stacked_user_forms(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
        &sink,
    )
    .await;
    let live = sink.0.lock().unwrap().clone();
    for seen in [&events, &live] {
        assert!(!seen.iter().any(is_awaiting_card), "no card: {seen:?}");
        assert_eq!(endings(seen).len(), 1, "{seen:?}");
        assert_eq!(seen.last().unwrap().event_type, EventType::RunError);
    }
}

/// A PARKED ROUND AND ITS CARD ARE ONE WRITE (`formal/tla/HarnessLoop.tla`
/// RoundNeverWithoutEnding). They were two, so a failed second write left the log holding the
/// tool call that asked for a person with no `Suspended` to answer.
#[tokio::test]
async fn a_parked_round_and_its_card_are_journaled_together() {
    let journal = MemoryJournal::new();
    run_conversation(
        &MockDoor::asking_for_stacked_user_forms(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
    )
    .await;
    let batches = journal.batches();
    let last = batches.last().expect("a last write");
    assert!(
        last.iter()
            .any(|event| event.event_type == EventType::ToolCallStart),
        "the round that asked: {batches:?}"
    );
    assert!(
        last.iter().any(is_awaiting_card),
        "and its card: {batches:?}"
    );
}

/// A CHART ENDS THE RUN, IN ONE WRITE. NativeChat paints bar_chart from its TOOL_CALL frames, so
/// the run finishes after it. The round was journaled before the ending, and the ending after.
#[tokio::test]
async fn a_chart_round_and_its_ending_are_journaled_together() {
    let journal = MemoryJournal::new();
    let door = MockDoor::with_script(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "bar_chart".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: r#"{"title":"spend"}"#.to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);
    let events = run_conversation(
        &door,
        Some(&tool_runner()),
        &journal,
        request("chart it"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(endings(&events).len(), 1, "{events:?}");
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let batches = journal.batches();
    let last = batches.last().expect("a last write");
    assert!(
        last.iter()
            .any(|event| event.event_type == EventType::ToolCallStart)
            && last
                .iter()
                .any(|event| event.event_type == EventType::RunFinished),
        "the chart and the finish in one write: {batches:?}"
    );
}

/// A chart round's preamble is not painted: the chart is what the round showed. The blank-turn
/// fallback painted "I'll draw a chart" as a bubble above it, because the run had no
/// TEXT_MESSAGE yet and the chart's TOOL_CALL frames did not count as saying something.
#[tokio::test]
async fn a_chart_rounds_preamble_is_not_painted() {
    let door = MockDoor::with_script(vec![
        ModelDelta::Text("I'll draw a chart of your spending.".to_string()),
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "bar_chart".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: r#"{"title":"spend"}"#.to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);
    let events = run_conversation(
        &door,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("chart my spending"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::ToolCallStart),
        "{events:?}"
    );
    assert_eq!(assistant_text(&events), "", "{events:?}");
}

/// A DOOR THAT WILL NOT OPEN ENDS THE RUN ONCE, SAYING WHY, and asks nothing further.
#[tokio::test]
async fn a_door_that_will_not_open_ends_the_run_once() {
    struct ClosedDoor(Arc<Mutex<usize>>);
    #[async_trait::async_trait]
    impl ModelDoor for ClosedDoor {
        async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
            if let Ok(mut calls) = self.0.lock() {
                *calls += 1;
            }
            Err(ModelError::Stream("the gateway is down".to_string()))
        }
    }
    let calls = Arc::new(Mutex::new(0usize));
    let journal = MemoryJournal::new();
    let events = run_conversation(
        &ClosedDoor(calls.clone()),
        Some(&tool_runner()),
        &journal,
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(*calls.lock().unwrap(), 1);
    let ends = endings(&events);
    assert_eq!(ends.len(), 1, "{events:?}");
    assert_eq!(ends[0].event_type, EventType::RunError);
    assert!(
        journal
            .batches()
            .concat()
            .iter()
            .any(|event| event.event_type == EventType::RunError),
        "and the log holds it"
    );
}

/// A door that fails with `first` on its first call and answers "back" after.
struct FailsOnce {
    first: Mutex<Option<ModelError>>,
    calls: Mutex<usize>,
}

impl FailsOnce {
    fn with(error: ModelError) -> Self {
        Self {
            first: Mutex::new(Some(error)),
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl ModelDoor for FailsOnce {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        *self.calls.lock().unwrap() += 1;
        if let Some(error) = self.first.lock().unwrap().take() {
            return Err(error);
        }
        Ok(Box::pin(futures::stream::iter([Ok(ModelDelta::Text(
            "back".to_string(),
        ))])))
    }
}

/// A gateway restarting refuses connections for a moment. Nothing was sent, so nothing can be
/// billed twice, and the turn used to fail on the spot (#185).
#[tokio::test]
async fn a_refused_connection_is_retried() {
    let door = FailsOnce::with(ModelError::Unreachable("connection refused".to_string()));
    let events = run_conversation(
        &door,
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(door.calls(), 2);
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(assistant_text(&events), "back");
}

/// A short Retry-After is honoured once.
#[tokio::test]
async fn a_rate_limit_with_a_short_retry_after_is_asked_again() {
    let door = FailsOnce::with(ModelError::Refused {
        status: 429,
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .to_string(),
        retry_after_s: Some(0),
    });
    let events = run_conversation(
        &door,
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(door.calls(), 2);
    assert_eq!(assistant_text(&events), "back");
}

/// A request the gateway could not take is not asked twice, and the person reads the reason,
/// not the gateway's JSON.
#[tokio::test]
async fn a_rejected_request_ends_with_a_sentence_not_json() {
    let door = FailsOnce::with(ModelError::Refused {
        status: 400,
        body: r#"{"type":"error","error":{"type":"upstream_error","message":"This model's maximum context length is 128000 tokens."}}"#
            .to_string(),
        retry_after_s: None,
    });
    let events = run_conversation(
        &door,
        None,
        &MemoryJournal::new(),
        request("hello"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(door.calls(), 1);
    let last = events.last().unwrap();
    assert_eq!(last.event_type, EventType::RunError);
    let message = last.extra["message"].as_str().unwrap_or_default();
    assert!(!message.contains('{'), "{message}");
    assert!(message.contains("maximum context length"), "{message}");
}

/// A PARK ASKS `stopped` TOO (`formal/tla/HarnessLoop.tla` StopIsHonoured, the verifier's trace).
/// A Stop pressed while the tool ran used to end the run on a card: its `Suspended` is refused on
/// a stopped run, so the card's answer was a 409.
#[tokio::test]
async fn a_park_after_a_stop_ends_the_run_stopped_with_no_card() {
    // Not stopped at the top of the round or before the tools; stopped by the close.
    let journal = StoppingJournal::saying_stop_after(2);
    let events = run_conversation(
        &MockDoor::asking_for_stacked_user_forms(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert!(!events.iter().any(is_awaiting_card), "no card: {events:?}");
    assert!(events.iter().any(is_run_stopped), "{events:?}");
    assert_eq!(endings(&events).len(), 1, "{events:?}");
}

/// A WRITE THAT FAILED MAY HAVE LANDED, SO IT IS NOT WRITTEN AGAIN (`formal/tla/JournalAppend.tla`
/// NoDuplicate). After the durable write of a round came back an error, the close used to write
/// the round again with its ending — twice in the log when the first commit had landed and only
/// its reply was lost.
#[tokio::test]
async fn a_round_whose_write_failed_is_not_written_again() {
    /// Keeps every batch, and answers the first round's write with an error anyway.
    struct LostReply {
        kept: Mutex<Vec<Vec<Event>>>,
    }
    #[async_trait::async_trait]
    impl RunJournal for LostReply {
        async fn record(&self, _run_id: &str, events: &[Event]) -> Result<(), JournalError> {
            let mut kept = self.kept.lock().map_err(|_| {
                JournalError::Unwritable("the journal's lock was poisoned".to_string())
            })?;
            let first_round = events
                .iter()
                .any(|event| event.event_type == EventType::ToolCallResult)
                && !kept
                    .concat()
                    .iter()
                    .any(|event| event.event_type == EventType::ToolCallResult);
            kept.push(events.to_vec());
            if first_round {
                return Err(JournalError::Unwritable("the reply was lost".to_string()));
            }
            Ok(())
        }
    }
    let journal = LostReply {
        kept: Mutex::new(Vec::new()),
    };
    let events = run_conversation(
        &CountingToolDoor(Arc::new(Mutex::new(0usize))),
        Some(&tool_runner()),
        &journal,
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(endings(&events).len(), 1, "{events:?}");
    let written = journal.kept.lock().unwrap().concat();
    let calls = written
        .iter()
        .filter(|event| event.event_type == EventType::ToolCallStart)
        .count();
    assert_eq!(calls, 1, "the round is in the log once: {written:?}");
}

/// A STOP THAT LANDS BETWEEN THE CLOSE'S QUESTION AND ITS WRITE STILL WINS (the peer review's
/// trace). The park's write found the run stopped: its `Suspended` was refused, the rest was
/// written, and the card went out with nothing behind it. The write now refuses the whole batch
/// as `Ended`, and the round goes in again with the stop's ending.
#[tokio::test]
async fn a_park_whose_write_finds_the_run_stopped_ends_stopped_with_no_card() {
    /// Never answers "stopped"; refuses, whole, any batch that opens a card.
    struct StoppedUnderTheWrite {
        kept: Mutex<Vec<Vec<Event>>>,
    }
    #[async_trait::async_trait]
    impl RunJournal for StoppedUnderTheWrite {
        async fn record(&self, _run_id: &str, events: &[Event]) -> Result<(), JournalError> {
            if events.iter().any(is_awaiting_card) {
                return Err(JournalError::Ended("a Stop got there first".to_string()));
            }
            self.kept
                .lock()
                .map_err(|_| JournalError::Unwritable("poisoned".to_string()))?
                .push(events.to_vec());
            Ok(())
        }
    }
    let journal = StoppedUnderTheWrite {
        kept: Mutex::new(Vec::new()),
    };
    let sink = Collect(Mutex::new(Vec::new()));
    let events = run_conversation_streaming(
        &MockDoor::asking_for_stacked_user_forms(),
        Some(&tool_runner()),
        &journal,
        request("sign in"),
        "t1",
        "r1",
        1,
        &sink,
    )
    .await;
    let live = sink.0.lock().unwrap().clone();
    let written = journal.kept.lock().unwrap().concat();
    for seen in [&events, &live, &written] {
        assert!(!seen.iter().any(is_awaiting_card), "no card: {seen:?}");
        assert!(seen.iter().any(is_run_stopped), "{seen:?}");
        assert_eq!(endings(seen).len(), 1, "{seen:?}");
    }
    assert!(
        written
            .iter()
            .any(|event| event.event_type == EventType::ToolCallStart),
        "the round that asked is still in the log: {written:?}"
    );
}

/// THE MODEL SEES WHAT IT CALLED. After a round of two parallel calls the next request carries
/// the assistant's own `tool_calls` message and then one `tool` message per call, keyed by the id
/// it gave each one — not two user lines naming ids it was never shown (#189).
#[tokio::test]
async fn two_parallel_calls_come_back_as_a_call_message_and_one_tool_message_each() {
    struct SpyDoor {
        round: Mutex<usize>,
        second: Mutex<Vec<serde_json::Value>>,
    }
    #[async_trait::async_trait]
    impl ModelDoor for SpyDoor {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            let round = {
                let mut count = self.round.lock().unwrap();
                *count += 1;
                *count
            };
            if round == 2 {
                *self.second.lock().unwrap() = request
                    .messages
                    .iter()
                    .map(|message| serde_json::to_value(message).unwrap())
                    .collect();
            }
            let call = |id: &str, command: &str| {
                vec![
                    ModelDelta::ToolCallStart {
                        id: id.to_string(),
                        name: "shell".to_string(),
                    },
                    ModelDelta::ToolCallArgs {
                        id: id.to_string(),
                        delta: format!(r#"{{"command":"{command}"}}"#),
                    },
                    ModelDelta::ToolCallEnd { id: id.to_string() },
                ]
            };
            let script = if round == 1 {
                [call("c1", "echo a"), call("c2", "echo b")].concat()
            } else {
                vec![ModelDelta::Text("both ran".to_string())]
            };
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    let door = SpyDoor {
        round: Mutex::new(0),
        second: Mutex::new(Vec::new()),
    };
    let events = run_conversation(
        &door,
        Some(&tool_runner()),
        &MemoryJournal::new(),
        request("go"),
        "t1",
        "r1",
        1,
    )
    .await;
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    let second = door.second.lock().unwrap().clone();
    let at = second
        .iter()
        .position(|message| message["role"] == "assistant");
    assert!(
        at.is_some(),
        "no assistant message after the tool round: {second:?}"
    );
    let at = at.unwrap();
    let calls = second[at]["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let named: Vec<(&str, &str, &str)> = calls
        .iter()
        .map(|call| {
            (
                call["id"].as_str().unwrap_or(""),
                call["name"].as_str().unwrap_or(""),
                call["arguments"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        named,
        [
            ("c1", "shell", r#"{"command":"echo a"}"#),
            ("c2", "shell", r#"{"command":"echo b"}"#)
        ],
        "{second:?}"
    );
    for (offset, id) in ["c1", "c2"].iter().enumerate() {
        let answer = &second[at + 1 + offset];
        assert_eq!(answer["role"], "tool", "{second:?}");
        assert_eq!(answer["tool_call_id"], *id, "{second:?}");
    }
}
