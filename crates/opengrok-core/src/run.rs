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
}

impl SuspendReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExecConsent => "exec-consent",
            Self::PolicyApproval => "policy-approval",
            Self::AutoReview => "auto-review",
        }
    }

    /// From the wire word; anything unrecognised is the default, which is the closed reading
    /// (an exec-consent card asks the machine owner, the strictest of the three).
    pub fn from_stored(word: &str) -> Self {
        match word {
            "policy-approval" => Self::PolicyApproval,
            "auto-review" => Self::AutoReview,
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
    pub status: RunStatus,
    /// The rendered events, in order — what a reconnecting client replays.
    pub emitted: Vec<Value>,
    pub failure: Option<String>,
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
            status: RunStatus::Running,
            emitted: Vec::new(),
            failure: None,
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
                ..
            } => {
                self.started = true;
                self.thread_id = thread_id.clone();
                self.coworker_id = coworker_id.clone();
                self.model = model.clone();
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
        }
    }

    /// The sequence the next emitted event will carry.
    pub fn next_seq(&self) -> i64 {
        self.emitted.len() as i64
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
                at_ms,
            } => Ok(vec![RunEvent::Started {
                thread_id,
                coworker_id,
                model,
                at_ms,
            }]),

            RunCommand::Emit { payload, at_ms } => {
                if !self.started {
                    return Err(RunError::NotStarted);
                }
                // Appending to a finished run would let a late frame arrive after the ending a
                // client already acted on. A SUSPENDED run may still be appended to — that is the
                // whole point of suspending rather than ending.
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
                if matches!(self.status, RunStatus::Finished | RunStatus::Failed) {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Finished { at_ms }])
            }

            RunCommand::Fail { reason, at_ms } => {
                if matches!(self.status, RunStatus::Finished | RunStatus::Failed) {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Failed { reason, at_ms }])
            }

            RunCommand::Suspend {
                call_id,
                tool,
                arguments,
                reason,
                at_ms,
            } => {
                if matches!(self.status, RunStatus::Finished | RunStatus::Failed) {
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
                at_ms: 1,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        run
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
