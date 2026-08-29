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

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{CoworkerId, RunId};

/// Where a run got to. A run that is `Running` with no process behind it is the interesting case:
/// it means a restart interrupted it, and something must decide what to do about that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    Running,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RunEvent {
    Started {
        thread_id: String,
        coworker_id: Option<CoworkerId>,
        at_ms: i64,
    },
    /// One rendered protocol event, stored verbatim so a replay is byte-exact rather than
    /// re-derived — a re-derivation would drift the moment the projection changed.
    Emitted {
        seq: i64,
        payload: Value,
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
    pub status: RunStatus,
    /// The rendered events, in order — what a reconnecting client replays.
    pub emitted: Vec<Value>,
    pub failure: Option<String>,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            started: false,
            thread_id: String::new(),
            coworker_id: None,
            status: RunStatus::Running,
            emitted: Vec::new(),
            failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("that run has already ended")]
    AlreadyEnded,
    #[error("that run has not started")]
    NotStarted,
}

#[derive(Debug, Clone)]
pub enum RunCommand {
    Start {
        thread_id: String,
        coworker_id: Option<CoworkerId>,
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
                ..
            } => {
                self.started = true;
                self.thread_id = thread_id.clone();
                self.coworker_id = coworker_id.clone();
                self.status = RunStatus::Running;
            }
            RunEvent::Emitted { payload, .. } => self.emitted.push(payload.clone()),
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

    pub fn decide(&self, command: RunCommand) -> Result<Vec<RunEvent>, RunError> {
        match command {
            RunCommand::Start {
                thread_id,
                coworker_id,
                at_ms,
            } => Ok(vec![RunEvent::Started {
                thread_id,
                coworker_id,
                at_ms,
            }]),

            RunCommand::Emit { payload, at_ms } => {
                if !self.started {
                    return Err(RunError::NotStarted);
                }
                // Appending to a finished run would let a late frame arrive after the ending a
                // client already acted on.
                if self.status != RunStatus::Running {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Emitted {
                    seq: self.next_seq(),
                    payload,
                    at_ms,
                }])
            }

            RunCommand::Finish { at_ms } => {
                if self.status != RunStatus::Running {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Finished { at_ms }])
            }

            RunCommand::Fail { reason, at_ms } => {
                if self.status != RunStatus::Running {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Failed { reason, at_ms }])
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
}
