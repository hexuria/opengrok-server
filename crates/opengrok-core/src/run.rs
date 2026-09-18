//! The run aggregate — one turn a coworker takes, and the reason this project exists.
//!
//! NOTHING THAT MATTERS LIVES IN A CLIENT (CLAUDE.md #5). A run is a row before it is a stream:
//! every event the client will see is appended to the log *first*, so closing the tab, losing the
//! network or killing the process loses the connection and never the work. The prior product got
//! this wrong and it is what created this repo (`research/lessons-opensesame.md` §4).
//!
//! What that buys, concretely:
//!   - a client that reconnects replays from the log instead of asking the model again;
//!   - a run interrupted by a restart is `Running` in the log, not silently lost — it can be
//!     resumed or failed deliberately, by something that can see it;
//!   - two clients watching one run see the same thing, because there is one truth.
//!
//! The events here are OURS, not AG-UI's. AG-UI is a rendering protocol and it changes on its own
//! schedule; the log outlives it. `payload` carries the rendered event so a replay is byte-exact,
//! but the aggregate's own vocabulary is what a future reader reasons about.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{CoworkerId, RunId};

/// Where a run got to. A run that is `Running` with no process behind it is the interesting case:
/// it means a restart interrupted it, and something must decide what to do about that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    Running,
    /// Stopped, waiting on a person. NOT a terminal state: the run can still be finished, which is
    /// what makes an approval days later possible.
    AwaitingApproval,
    Finished,
    Failed,
    /// A person changed their mind and stopped it. Terminal, and DELIBERATELY NOT `Failed`: a stop
    /// is somebody pressing a button, not the run going wrong. Flattening the two would make every
    /// later answer to "why did this run fail" name a failure that never happened, and would leave
    /// the one number that matters — how often a coworker actually breaks — unreadable.
    Stopped,
}

impl RunStatus {
    /// Has this run ended for good? A terminal run accepts no further ending: it cannot be
    /// finished, failed or stopped again, whatever arrives late.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Stopped)
    }

    /// The word the wire and the projection use. One place, because three readers spelling a
    /// status differently is how a client comes to believe a run is still going.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// From the stored word. Anything unrecognised reads as `Running`, which is the open reading:
    /// a status we cannot understand must not be mistaken for an ending that lets work be dropped.
    #[must_use]
    pub fn from_stored(word: &str) -> Self {
        match word {
            "awaiting-approval" => Self::AwaitingApproval,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => Self::Running,
        }
    }
}

/// WHY a run is waiting. Two different cards can now come from the same tool — the machine owner's
/// consent for a command, or the auto-review judge's "ask" — so the answer path has to know which
/// question was asked: the wrong verb must not settle the other card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SuspendReason {
    /// The remote-control gate wants the machine owner's consent for this command (the
    /// `local-tool-permission` card). THE DEFAULT ON PURPOSE: every suspension recorded before
    /// reasons existed meant exactly this, so an old row in the append-only log replays unchanged.
    #[default]
    ExecConsent,
    /// The coworker's policy grant marks this tool `needs_approval`.
    PolicyApproval,
    /// The auto-review judge said "ask" (the `auto-review-approval` card).
    AutoReview,
    /// The bot asked the person to fill an in-chat `user-form` (`request_user_form`). Not an
    /// approval of a tool that will then run: submit types into the box outside `computer_use`,
    /// and the tool result is synthesised so a secret never re-enters the executor.
    UserForm,
    /// The bot asked the client to fill a saved site login (`credential.request`). Not an
    /// approval that then runs a tool: NativeChat fills the box and POSTs a status. Site
    /// passwords never enter the vault, the journal, or a tool result.
    Credential,
}

impl SuspendReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExecConsent => "exec-consent",
            Self::PolicyApproval => "policy-approval",
            Self::AutoReview => "auto-review",
            Self::UserForm => "user-form",
            Self::Credential => "credential",
        }
    }

    /// From the wire word; anything unrecognised is the default, which is the closed reading
    /// (an exec-consent card asks the machine owner, the strictest of the kinds).
    pub fn from_stored(word: &str) -> Self {
        match word {
            "policy-approval" => Self::PolicyApproval,
            "auto-review" => Self::AutoReview,
            "user-form" => Self::UserForm,
            "credential" => Self::Credential,
            _ => Self::ExecConsent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RunEvent {
    Started {
        thread_id: String,
        coworker_id: Option<CoworkerId>,
        /// The pin this turn captured. A resume must not reload the coworker and pick up a
        /// pin that moved while we were waiting. Absent on logs written before this field
        /// existed (`#[serde(default)]`); those keep the old behaviour — the current pin.
        #[serde(default)]
        model: Option<String>,
        /// The system message this turn opened with. A resume must not recompose it from a
        /// coworker whose role or title moved while a person was answering an approval card —
        /// the turn would change identity halfway through, at the moment somebody intervened.
        /// Same reasoning as `model` above, and absent on logs written before this field.
        #[serde(default)]
        system: Option<String>,
        at_ms: i64,
    },
    /// One rendered protocol event, stored verbatim so a replay is byte-exact rather than
    /// re-derived — a re-derivation would drift the moment the projection changed.
    Emitted {
        seq: i64,
        payload: Value,
        at_ms: i64,
    },
    /// A tool call is waiting on a human yes. Records exactly WHICH call, because that is what a
    /// later approval has to be about — "approve the run" would be ambiguous the moment a turn
    /// asks for two things.
    Suspended {
        call_id: String,
        tool: String,
        arguments: Value,
        #[serde(default)]
        reason: SuspendReason,
        at_ms: i64,
    },
    /// A person answered. `approved` false is a refusal, which is also an answer and also ends the
    /// waiting.
    Answered {
        call_id: String,
        approved: bool,
        /// Who said so. A decision nobody is attached to cannot be audited.
        by: String,
        at_ms: i64,
    },
    Finished {
        at_ms: i64,
    },
    Failed {
        reason: String,
        at_ms: i64,
    },
    /// A person stopped it. Its own event rather than a `Failed` with a special reason, because a
    /// reason string is not a status: everything that counts failures — the routines pane, a
    /// future reliability number, whoever is asked why a coworker keeps breaking — reads the
    /// status and would count this one. `by` is recorded for the same reason `Answered` records
    /// it: a decision nobody is attached to cannot be audited.
    Stopped {
        by: String,
        at_ms: i64,
    },
}

impl RunEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Started { .. } => "run-started",
            Self::Emitted { .. } => "run-emitted",
            Self::Suspended { .. } => "run-suspended",
            Self::Answered { .. } => "run-answered",
            Self::Finished { .. } => "run-finished",
            Self::Failed { .. } => "run-failed",
            Self::Stopped { .. } => "run-stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub started: bool,
    pub thread_id: String,
    pub coworker_id: Option<CoworkerId>,
    /// Captured at start. See `RunEvent::Started::model`.
    pub model: Option<String>,
    /// Captured at start. See `RunEvent::Started::system`.
    pub system: Option<String>,
    pub status: RunStatus,
    /// The rendered events, in order — what a reconnecting client replays.
    pub emitted: Vec<Value>,
    pub failure: Option<String>,
    /// Who stopped it, when somebody did. `None` on every other run, which is how a reader tells
    /// "nobody stopped this" from "stopped by somebody we did not write down".
    pub stopped_by: Option<String>,
    /// The call waiting on a person, if any.
    pub pending: Option<PendingApproval>,
    /// Calls that have already been answered.
    ///
    /// THIS IS WHAT MAKES APPROVAL EXACTLY-ONCE. A second yes for the same call is refused by the
    /// aggregate, so a retried request, a double-clicked button and two devices answering together
    /// all converge on one answer instead of running the tool twice.
    pub answered: BTreeSet<String>,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            started: false,
            thread_id: String::new(),
            coworker_id: None,
            model: None,
            system: None,
            status: RunStatus::Running,
            emitted: Vec::new(),
            failure: None,
            stopped_by: None,
            pending: None,
            answered: BTreeSet::new(),
        }
    }
}

/// What a person is being asked to approve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
    #[serde(default)]
    pub reason: SuspendReason,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("that run has already ended")]
    AlreadyEnded,
    #[error("that run has not started")]
    NotStarted,
    #[error("that run is not waiting for an answer")]
    NotAwaiting,
    #[error("that call has already been answered")]
    AlreadyAnswered,
}

#[derive(Debug, Clone)]
pub enum RunCommand {
    Start {
        thread_id: String,
        coworker_id: Option<CoworkerId>,
        model: Option<String>,
        /// The composed system message this turn opens with, captured so a resume speaks with
        /// the same identity and standing role the turn began with.
        system: Option<String>,
        at_ms: i64,
    },
    Emit {
        payload: Value,
        at_ms: i64,
    },
    Finish {
        at_ms: i64,
    },
    Fail {
        reason: String,
        at_ms: i64,
    },
    /// Stop and wait for a person.
    Suspend {
        call_id: String,
        tool: String,
        arguments: Value,
        reason: SuspendReason,
        at_ms: i64,
    },
    /// A person changed their mind. Ends the run without calling it a failure.
    Stop {
        /// Who pressed the button.
        by: String,
        at_ms: i64,
    },
    /// A person answered. Refused if that call was already answered — the exactly-once check.
    Answer {
        call_id: String,
        approved: bool,
        by: String,
        at_ms: i64,
    },
}

impl Run {
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a RunEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &RunEvent) {
        match event {
            RunEvent::Started {
                thread_id,
                coworker_id,
                model,
                system,
                ..
            } => {
                self.started = true;
                self.thread_id = thread_id.clone();
                self.coworker_id = coworker_id.clone();
                self.model = model.clone();
                self.system.clone_from(system);
                self.status = RunStatus::Running;
            }
            RunEvent::Emitted { payload, .. } => self.emitted.push(payload.clone()),
            RunEvent::Suspended {
                call_id,
                tool,
                arguments,
                reason,
                ..
            } => {
                self.status = RunStatus::AwaitingApproval;
                self.pending = Some(PendingApproval {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    arguments: arguments.clone(),
                    reason: *reason,
                });
            }
            RunEvent::Answered { call_id, .. } => {
                self.answered.insert(call_id.clone());
                self.pending = None;
                // Back to running whether the answer was yes OR no, and the sameness is deliberate:
                // a refusal still has to be delivered to the model so it can choose something else,
                // and delivering it is a turn. `approved` decides what the model is told, not
                // whether the run continues.
                self.status = RunStatus::Running;
            }
            RunEvent::Finished { .. } => self.status = RunStatus::Finished,
            RunEvent::Failed { reason, .. } => {
                self.status = RunStatus::Failed;
                self.failure = Some(reason.clone());
            }
            RunEvent::Stopped { by, .. } => {
                self.status = RunStatus::Stopped;
                self.stopped_by = Some(by.clone());
                // The card is moot. A person who stopped the run is not going to answer the
                // question it was waiting on, and leaving it pending would keep the run in the
                // approvals list of somebody who has already said no to the whole thing.
                self.pending = None;
            }
        }
    }

    /// The sequence the next emitted event will carry.
    pub fn next_seq(&self) -> i64 {
        self.emitted.len() as i64
    }

    /// The system message a resume must speak with. A captured one wins; a log written before
    /// this was stored has none, and the caller composes a fresh one — the old behaviour.
    #[must_use]
    pub fn system_for_resume(&self) -> Option<String> {
        self.system.clone().filter(|text| !text.is_empty())
    }

    /// The pin a resume must think with. A captured start pin wins; a log written before
    /// pins were stored falls back to the coworker's current one (the old behaviour).
    pub fn pin_for_resume(&self, current: &str) -> String {
        self.model
            .as_deref()
            .filter(|pin| !pin.is_empty())
            .unwrap_or(current)
            .to_string()
    }

    pub fn decide(&self, command: RunCommand) -> Result<Vec<RunEvent>, RunError> {
        match command {
            RunCommand::Start {
                thread_id,
                coworker_id,
                model,
                system,
                at_ms,
            } => Ok(vec![RunEvent::Started {
                thread_id,
                coworker_id,
                model,
                system,
                at_ms,
            }]),

            RunCommand::Emit { payload, at_ms } => {
                if !self.started {
                    return Err(RunError::NotStarted);
                }
                // Appending to a finished run would let a late frame arrive after the ending a
                // client already acted on. A SUSPENDED run may still be appended to — that is the
                // whole point of suspending rather than ending.
                //
                // A STOPPED RUN MAY STILL BE APPENDED TO, and the asymmetry with `Finished` is
                // deliberate. `Finished` and `Failed` are the run declaring its OWN ending: by the
                // time either is in the log, nothing is still producing frames. A stop is declared
                // from outside, by a person, while the turn is mid-step — and the turn is journaled
                // a whole round at a time, so the round it was in the middle of arrives *after* the
                // stop lands. Refusing it would drop exactly the frames the person was looking at
                // when they pressed the button. What a stopped run refuses is another ENDING, which
                // is what makes it terminal.
                if matches!(self.status, RunStatus::Finished | RunStatus::Failed) {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Emitted {
                    seq: self.next_seq(),
                    payload,
                    at_ms,
                }])
            }

            RunCommand::Finish { at_ms } => {
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Finished { at_ms }])
            }

            RunCommand::Fail { reason, at_ms } => {
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Failed { reason, at_ms }])
            }

            // THE PERSON PRESSED THE BUTTON; WHETHER THEY WON THE RACE WITH THE MODEL IS NOT THEIR
            // PROBLEM. `AlreadyEnded` on a run that has already ended is what the caller turns into
            // a success, so stopping twice, stopping a run that finished a moment ago, and stopping
            // one a restart failed all mean the same thing to whoever asked: it is not running.
            RunCommand::Stop { by, at_ms } => {
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Stopped { by, at_ms }])
            }

            RunCommand::Suspend {
                call_id,
                tool,
                arguments,
                reason,
                at_ms,
            } => {
                // A stopped run counts here too: a run nobody is going to carry on must not open a
                // card asking somebody to decide whether to carry it on.
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Suspended {
                    call_id,
                    tool,
                    arguments,
                    reason,
                    at_ms,
                }])
            }

            RunCommand::Answer {
                call_id,
                approved,
                by,
                at_ms,
            } => {
                // EXACTLY ONCE. A retried request, a double-clicked button, two devices answering
                // together — all land here, and only the first produces an event. The store's
                // sequence check makes the concurrent case safe too: the loser gets Conflict and
                // re-reads to find the call already answered.
                if self.answered.contains(&call_id) {
                    return Err(RunError::AlreadyAnswered);
                }
                let Some(pending) = &self.pending else {
                    return Err(RunError::NotAwaiting);
                };
                if pending.call_id != call_id {
                    return Err(RunError::NotAwaiting);
                }
                Ok(vec![RunEvent::Answered {
                    call_id,
                    approved,
                    by,
                    at_ms,
                }])
            }
        }
    }
}

/// The read model: what a client asking "what happened in this run" is answered from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunView {
    pub id: RunId,
    pub thread_id: String,
    pub status: RunStatus,
    pub event_count: i64,
    pub updated_at_ms: i64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started() -> Run {
        let mut run = Run::default();
        for event in run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                system: None,
                at_ms: 1,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        run
    }

    /// The turn's identity survives the wait. A role edited while a person answered an
    /// approval card must not change the coworker halfway through the turn it interrupted.
    #[test]
    #[allow(clippy::expect_used)]
    fn a_resume_speaks_with_the_system_message_the_turn_opened_with() {
        let mut run = Run::default();
        for event in run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: Some("openai/gpt-5.6-luna".to_string()),
                system: Some("You are Ada.".to_string()),
                at_ms: 1,
            })
            .expect("start")
        {
            run.apply(&event);
        }
        assert_eq!(run.system_for_resume().as_deref(), Some("You are Ada."));
        // A log written before this was captured has none, and the caller composes afresh.
        let mut old = Run::default();
        for event in old
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                system: None,
                at_ms: 1,
            })
            .expect("start")
        {
            old.apply(&event);
        }
        assert_eq!(old.system_for_resume(), None);
        // An empty capture is treated as none rather than as an empty prompt.
        let mut blank = Run::default();
        for event in blank
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                system: Some(String::new()),
                at_ms: 1,
            })
            .expect("start")
        {
            blank.apply(&event);
        }
        assert_eq!(blank.system_for_resume(), None);
    }

    #[test]
    fn emitted_events_are_numbered_in_order() {
        let mut run = started();
        for index in 0..3 {
            let events = run
                .decide(RunCommand::Emit {
                    payload: json!({ "n": index }),
                    at_ms: 10,
                })
                .unwrap();
            assert!(matches!(events[0], RunEvent::Emitted { seq, .. } if seq == index));
            for event in &events {
                run.apply(event);
            }
        }
        assert_eq!(run.emitted.len(), 3);
    }

    /// The replay guarantee: what a reconnecting client sees is exactly what was sent.
    #[test]
    fn replaying_the_log_reproduces_every_emitted_event_in_order() {
        let mut run = started();
        let mut log = vec![RunEvent::Started {
            thread_id: "t1".to_string(),
            coworker_id: None,
            model: None,
            system: None,
            at_ms: 1,
        }];
        for index in 0..5 {
            for event in run
                .decide(RunCommand::Emit {
                    payload: json!({ "n": index }),
                    at_ms: 10,
                })
                .unwrap()
            {
                run.apply(&event);
                log.push(event);
            }
        }
        let replayed = Run::replay(&log);
        assert_eq!(replayed.emitted, run.emitted);
        assert_eq!(replayed.emitted[3], json!({ "n": 3 }));
    }

    /// A late frame after the ending would arrive after a client already acted on it.
    #[test]
    fn a_finished_run_refuses_further_events() {
        let mut run = started();
        for event in run.decide(RunCommand::Finish { at_ms: 20 }).unwrap() {
            run.apply(&event);
        }
        assert_eq!(
            run.decide(RunCommand::Emit {
                payload: json!({}),
                at_ms: 30
            }),
            Err(RunError::AlreadyEnded)
        );
    }

    #[test]
    fn a_run_cannot_end_twice() {
        let mut run = started();
        for event in run.decide(RunCommand::Finish { at_ms: 20 }).unwrap() {
            run.apply(&event);
        }
        assert_eq!(
            run.decide(RunCommand::Finish { at_ms: 21 }),
            Err(RunError::AlreadyEnded)
        );
        assert_eq!(
            run.decide(RunCommand::Fail {
                reason: "late".to_string(),
                at_ms: 22
            }),
            Err(RunError::AlreadyEnded)
        );
    }

    #[test]
    fn a_failure_is_recorded_with_its_reason() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Fail {
                reason: "upstream hung up".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.failure.as_deref(), Some("upstream hung up"));
    }

    /// The interesting case after a restart: still `Running`, with no process behind it.
    #[test]
    fn an_interrupted_run_replays_as_running() {
        let log = vec![
            RunEvent::Started {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                system: None,
                at_ms: 1,
            },
            RunEvent::Emitted {
                seq: 0,
                payload: json!({ "type": "RUN_STARTED" }),
                at_ms: 2,
            },
        ];
        let run = Run::replay(&log);
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.emitted.len(), 1, "and nothing it emitted was lost");
    }

    fn suspended() -> Run {
        let mut run = started();
        for event in run
            .decide(RunCommand::Suspend {
                call_id: "c1".to_string(),
                tool: "shell".to_string(),
                arguments: json!({"command": "rm -rf /"}),
                reason: SuspendReason::default(),
                at_ms: 10,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        run
    }

    #[test]
    fn suspending_records_which_call_is_waiting() {
        let run = suspended();
        assert_eq!(run.status, RunStatus::AwaitingApproval);
        let pending = run.pending.as_ref().unwrap();
        assert_eq!(pending.call_id, "c1");
        assert_eq!(pending.tool, "shell");
        // The arguments are kept, because a person approving needs to see what they are approving.
        assert_eq!(pending.arguments["command"], "rm -rf /");
    }

    /// A suspended run is NOT ended: it must still accept events, which is what lets it be
    /// finished when the answer arrives days later.
    #[test]
    fn a_suspended_run_can_still_be_added_to_and_finished() {
        let mut run = suspended();
        let events = run
            .decide(RunCommand::Emit {
                payload: json!({"type": "TEXT_MESSAGE_CONTENT"}),
                at_ms: 20,
            })
            .unwrap();
        for event in &events {
            run.apply(event);
        }
        assert_eq!(run.emitted.len(), 1);
        assert!(run.decide(RunCommand::Finish { at_ms: 30 }).is_ok());
    }

    /// THE EXACTLY-ONCE PROPERTY. A second answer for the same call produces no event, so the tool
    /// cannot run twice however many times the request is retried.
    #[test]
    fn a_call_can_only_be_answered_once() {
        let mut run = suspended();
        let first = run
            .decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap();
        assert_eq!(first.len(), 1);
        for event in &first {
            run.apply(event);
        }

        // Every later attempt, however it arrives.
        for _ in 0..3 {
            assert_eq!(
                run.decide(RunCommand::Answer {
                    call_id: "c1".to_string(),
                    approved: true,
                    by: "acct_1".to_string(),
                    at_ms: 21,
                }),
                Err(RunError::AlreadyAnswered)
            );
        }
    }

    /// Answering a call that is not the one waiting must not release the one that is.
    #[test]
    fn answering_the_wrong_call_does_nothing() {
        let run = suspended();
        assert_eq!(
            run.decide(RunCommand::Answer {
                call_id: "some-other-call".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 20,
            }),
            Err(RunError::NotAwaiting)
        );
        // And the real one is still waiting.
        assert_eq!(run.status, RunStatus::AwaitingApproval);
    }

    /// A run nobody suspended cannot be answered — an answer is a reply, not a command.
    #[test]
    fn a_running_run_cannot_be_answered() {
        assert_eq!(
            started().decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 20,
            }),
            Err(RunError::NotAwaiting)
        );
    }

    /// A refusal is an answer too: it ends the waiting and the run continues, because the model
    /// still has to be told no.
    #[test]
    fn a_refusal_also_releases_the_run() {
        let mut run = suspended();
        for event in run
            .decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: false,
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Running);
        assert!(run.pending.is_none());
    }

    /// Replay reaches the same conclusion, which is what makes the guarantee survive a restart:
    /// a process that comes back mid-approval must not accept a second answer.
    #[test]
    fn exactly_once_survives_a_replay() {
        let log = vec![
            RunEvent::Started {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                system: None,
                at_ms: 1,
            },
            RunEvent::Suspended {
                call_id: "c1".to_string(),
                tool: "shell".to_string(),
                arguments: json!({}),
                reason: SuspendReason::default(),
                at_ms: 2,
            },
            RunEvent::Answered {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 3,
            },
        ];
        let run = Run::replay(&log);
        assert_eq!(
            run.decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 4,
            }),
            Err(RunError::AlreadyAnswered),
            "a restarted process must not accept a second answer"
        );
    }

    /// A STOP IS NOT A FAILURE. The status says stopped and nothing is recorded as a failure
    /// reason, so "why did this run fail" never has to answer for somebody changing their mind.
    #[test]
    fn stopping_is_its_own_ending_and_not_a_failure() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Stopped);
        assert_eq!(run.failure, None, "a stop has no failure to report");
        assert_eq!(run.stopped_by.as_deref(), Some("acct_1"));
        assert!(run.status.is_terminal());
    }

    /// Idempotence, from the aggregate's side: every attempt after the first says the run has
    /// already ended, which is what the route turns into a success.
    #[test]
    fn a_stopped_run_refuses_every_further_ending() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        for attempt in [
            RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 21,
            },
            RunCommand::Finish { at_ms: 22 },
            RunCommand::Fail {
                reason: "late".to_string(),
                at_ms: 23,
            },
        ] {
            assert_eq!(run.decide(attempt), Err(RunError::AlreadyEnded));
        }
        assert_eq!(
            run.status,
            RunStatus::Stopped,
            "and the outcome is unchanged"
        );
    }

    /// Stopping a run that already ended must not rewrite what happened to it.
    #[test]
    fn stopping_a_finished_run_leaves_its_outcome_alone() {
        let mut run = started();
        for event in run.decide(RunCommand::Finish { at_ms: 20 }).unwrap() {
            run.apply(&event);
        }
        assert_eq!(
            run.decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 21,
            }),
            Err(RunError::AlreadyEnded)
        );
        assert_eq!(run.status, RunStatus::Finished);

        let mut failed = started();
        for event in failed
            .decide(RunCommand::Fail {
                reason: "upstream hung up".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            failed.apply(&event);
        }
        assert_eq!(
            failed.decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 21,
            }),
            Err(RunError::AlreadyEnded)
        );
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.failure.as_deref(), Some("upstream hung up"));
    }

    /// THE FRAMES THE TURN WAS MID-ROUND ON ARE STILL THE RECORD. The loop journals a whole round
    /// at a time, so the round in progress when somebody pressed stop lands after the stop does;
    /// refusing it would end the transcript one step before the moment the person was watching.
    #[test]
    fn frames_still_in_flight_when_the_stop_landed_are_kept() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        let events = run
            .decide(RunCommand::Emit {
                payload: json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "half a sentence"}),
                at_ms: 21,
            })
            .unwrap();
        for event in &events {
            run.apply(event);
        }
        assert_eq!(run.emitted.len(), 1);
        assert_eq!(
            run.status,
            RunStatus::Stopped,
            "a late frame does not un-stop the run"
        );
    }

    /// Stopping while a card is open is the case the button exists for as much as any other, and
    /// the card must not outlive the run it was asking about.
    #[test]
    fn stopping_a_run_that_was_waiting_closes_its_card() {
        let mut run = suspended();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Stopped);
        assert!(run.pending.is_none(), "the question is moot");
        assert_eq!(
            run.decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 21,
            }),
            Err(RunError::NotAwaiting),
            "and answering it cannot restart the run"
        );
    }

    /// The whole point of the log: a process that comes back must reach the same conclusion, or a
    /// stop is undone by a restart.
    #[test]
    fn a_stop_survives_a_replay() {
        let log = vec![
            RunEvent::Started {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                system: None,
                at_ms: 1,
            },
            RunEvent::Emitted {
                seq: 0,
                payload: json!({ "type": "RUN_STARTED" }),
                at_ms: 2,
            },
            RunEvent::Stopped {
                by: "acct_1".to_string(),
                at_ms: 3,
            },
        ];
        let run = Run::replay(&log);
        assert_eq!(run.status, RunStatus::Stopped);
        assert_eq!(run.emitted.len(), 1, "and nothing it emitted was lost");
        assert_eq!(
            run.decide(RunCommand::Finish { at_ms: 4 }),
            Err(RunError::AlreadyEnded),
            "a restarted process must not finish a run somebody stopped"
        );
    }

    /// The stored word round-trips, because the projection is read back as a status and a
    /// misreading would put a stopped run back in front of the recovery sweep.
    #[test]
    fn the_stored_status_word_round_trips() {
        for status in [
            RunStatus::Running,
            RunStatus::AwaitingApproval,
            RunStatus::Finished,
            RunStatus::Failed,
            RunStatus::Stopped,
        ] {
            assert_eq!(RunStatus::from_stored(status.as_str()), status);
        }
        assert_eq!(
            RunStatus::from_stored("something we have never heard of"),
            RunStatus::Running,
            "an unreadable status must not be mistaken for an ending"
        );
    }

    #[test]
    fn the_stored_suspend_reason_word_round_trips() {
        for reason in [
            SuspendReason::ExecConsent,
            SuspendReason::PolicyApproval,
            SuspendReason::AutoReview,
            SuspendReason::UserForm,
            SuspendReason::Credential,
        ] {
            assert_eq!(SuspendReason::from_stored(reason.as_str()), reason);
        }
        assert_eq!(
            SuspendReason::from_stored("something we have never heard of"),
            SuspendReason::ExecConsent,
            "an unreadable reason is the closed reading"
        );
    }

    #[test]
    fn emitting_before_starting_is_refused() {
        assert_eq!(
            Run::default().decide(RunCommand::Emit {
                payload: json!({}),
                at_ms: 1
            }),
            Err(RunError::NotStarted)
        );
    }

    #[test]
    fn a_started_run_remembers_its_pin() {
        let mut run = Run::default();
        for event in run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: Some("openai/gpt-5.5".to_string()),
                system: None,
                at_ms: 1,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.model.as_deref(), Some("openai/gpt-5.5"));
        assert_eq!(run.pin_for_resume("oag/auto"), "openai/gpt-5.5");
    }

    #[test]
    fn a_log_without_a_pin_resumes_on_the_current_one() {
        let event: RunEvent = serde_json::from_str(
            r#"{"type":"started","thread_id":"t1","coworker_id":null,"at_ms":1}"#,
        )
        .unwrap();
        let run = Run::replay([&event]);
        assert_eq!(run.model, None);
        assert_eq!(run.pin_for_resume("oag/auto"), "oag/auto");
    }
}
