//! The schedule aggregate — a coworker told to act on its own clock.
//!
//! THIS IS THE MISSION'S OTHER HALF. Everything before this slice answers when a client asks; a
//! schedule is the server deciding, at a time the operator wrote down, that a coworker should take
//! a turn — laptop open or not.
//!
//! THE CRON EXPRESSION IS VALIDATED IN `decide`, NOT AT THE EDGE. A schedule whose expression
//! cannot be parsed would sit in the log as a row that never fires and never explains itself; the
//! aggregate refusing it makes "it was accepted" and "it will fire" the same claim.
//!
//! FIRING IS AN EVENT because it is provenance: a run that no client started must say what started
//! it, and `Fired { run_id }` is that answer, in the same log as everything else. Pausing exists
//! (rather than delete-and-recreate) because "stop for the weekend" should not cost the schedule
//! its history.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::id::{CoworkerId, RunId};

/// A cron expression the way people write them (5 fields), silently promoted to the 6-field form
/// the parser wants (seconds first) so `0 9 * * 1` means "09:00 every Monday" and not a parse
/// error. A 6- or 7-field expression passes through untouched — which is also what lets tests
/// schedule in seconds.
pub fn normalized_cron(expression: &str) -> String {
    let fields = expression.split_whitespace().count();
    if fields == 5 {
        format!("0 {}", expression.trim())
    } else {
        expression.trim().to_string()
    }
}

/// When a schedule next fires after `after_ms`, in epoch milliseconds. `None` for an expression
/// with no future occurrence (a fixed date already past) — and the caller must treat that as "done
/// firing", not as an error.
pub fn next_fire_ms(expression: &str, after_ms: i64) -> Option<i64> {
    let schedule = cron::Schedule::from_str(&normalized_cron(expression)).ok()?;
    let after = chrono::DateTime::from_timestamp_millis(after_ms)?;
    schedule
        .after(&after)
        .next()
        .map(|when| when.timestamp_millis())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ScheduleEvent {
    Created {
        coworker_id: CoworkerId,
        /// Already normalized; what `next_fire_ms` will be asked about ever after.
        cron: String,
        /// The user message every firing opens its run with.
        prompt: String,
        at_ms: i64,
    },
    Paused {
        at_ms: i64,
    },
    Resumed {
        at_ms: i64,
    },
    Deleted {
        at_ms: i64,
    },
    /// A run this schedule started. The run's own log holds what happened; this holds *why it
    /// exists*.
    Fired {
        run_id: RunId,
        at_ms: i64,
    },
}

impl ScheduleEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Created { .. } => "schedule-created",
            Self::Paused { .. } => "schedule-paused",
            Self::Resumed { .. } => "schedule-resumed",
            Self::Deleted { .. } => "schedule-deleted",
            Self::Fired { .. } => "schedule-fired",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    pub created: bool,
    pub deleted: bool,
    pub paused: bool,
    pub coworker_id: Option<CoworkerId>,
    pub cron: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no such schedule")]
    NotCreated,
    #[error("that schedule has been deleted")]
    Deleted,
    #[error("that schedule is already paused")]
    AlreadyPaused,
    #[error("that schedule is not paused")]
    NotPaused,
    #[error("that schedule is paused")]
    Paused,
    #[error("not a cron expression: {0}")]
    BadCron(String),
    #[error("a schedule needs something to say")]
    EmptyPrompt,
}

#[derive(Debug, Clone)]
pub enum ScheduleCommand {
    Create {
        coworker_id: CoworkerId,
        cron: String,
        prompt: String,
        at_ms: i64,
    },
    Pause {
        at_ms: i64,
    },
    Resume {
        at_ms: i64,
    },
    Delete {
        at_ms: i64,
    },
    Fire {
        run_id: RunId,
        at_ms: i64,
    },
}

impl Schedule {
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a ScheduleEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &ScheduleEvent) {
        match event {
            ScheduleEvent::Created {
                coworker_id,
                cron,
                prompt,
                ..
            } => {
                self.created = true;
                self.coworker_id = Some(coworker_id.clone());
                self.cron = cron.clone();
                self.prompt = prompt.clone();
            }
            ScheduleEvent::Paused { .. } => self.paused = true,
            ScheduleEvent::Resumed { .. } => self.paused = false,
            ScheduleEvent::Deleted { .. } => self.deleted = true,
            ScheduleEvent::Fired { .. } => {}
        }
    }

    fn alive(&self) -> Result<(), ScheduleError> {
        if !self.created {
            return Err(ScheduleError::NotCreated);
        }
        if self.deleted {
            return Err(ScheduleError::Deleted);
        }
        Ok(())
    }

    pub fn decide(&self, command: ScheduleCommand) -> Result<Vec<ScheduleEvent>, ScheduleError> {
        match command {
            ScheduleCommand::Create {
                coworker_id,
                cron,
                prompt,
                at_ms,
            } => {
                let cron = normalized_cron(&cron);
                // Accepted must mean "will fire": an unparseable expression, or one with no
                // future occurrence at all, is refused here rather than stored as a dead row.
                if next_fire_ms(&cron, at_ms).is_none() {
                    return Err(ScheduleError::BadCron(cron));
                }
                if prompt.trim().is_empty() {
                    return Err(ScheduleError::EmptyPrompt);
                }
                Ok(vec![ScheduleEvent::Created {
                    coworker_id,
                    cron,
                    prompt,
                    at_ms,
                }])
            }

            ScheduleCommand::Pause { at_ms } => {
                self.alive()?;
                if self.paused {
                    return Err(ScheduleError::AlreadyPaused);
                }
                Ok(vec![ScheduleEvent::Paused { at_ms }])
            }

            ScheduleCommand::Resume { at_ms } => {
                self.alive()?;
                if !self.paused {
                    return Err(ScheduleError::NotPaused);
                }
                Ok(vec![ScheduleEvent::Resumed { at_ms }])
            }

            ScheduleCommand::Delete { at_ms } => {
                self.alive()?;
                Ok(vec![ScheduleEvent::Deleted { at_ms }])
            }

            ScheduleCommand::Fire { run_id, at_ms } => {
                self.alive()?;
                // A paused schedule refusing to fire is the whole point of pause. The sweep should
                // never ask (paused rows are not claimed), so this firing twice as a guard is
                // deliberate: the projection being wrong must not be enough to fire a run.
                if self.paused {
                    return Err(ScheduleError::Paused);
                }
                Ok(vec![ScheduleEvent::Fired { run_id, at_ms }])
            }
        }
    }
}

/// One row of `schedule_view` — what a list endpoint returns and the sweep claims from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleView {
    pub id: String,
    pub coworker_id: CoworkerId,
    pub cron: String,
    pub prompt: String,
    pub active: bool,
    pub next_due_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;

    fn created() -> Schedule {
        Schedule::replay(&[ScheduleEvent::Created {
            coworker_id: CoworkerId::from_stored("cw_1"),
            cron: "0 */5 * * * *".to_string(),
            prompt: "check the queue".to_string(),
            at_ms: 1_000,
        }])
    }

    #[test]
    fn five_field_cron_is_promoted_and_six_field_is_kept() {
        assert_eq!(normalized_cron("*/5 * * * *"), "0 */5 * * * *");
        assert_eq!(normalized_cron("*/2 * * * * *"), "*/2 * * * * *");
    }

    #[test]
    fn next_fire_is_strictly_after_the_given_moment() {
        // Every 2 seconds from t=0: the next fire after t=0 is t=2s, not t=0 again — `after` must
        // be exclusive or a claimed schedule would be claimed forever.
        let next = next_fire_ms("*/2 * * * * *", 0).expect("a next occurrence");
        assert_eq!(next, 2_000);
        let after_that = next_fire_ms("*/2 * * * * *", next).expect("another");
        assert_eq!(after_that, 4_000);
    }

    #[test]
    fn a_bad_expression_is_refused_at_create() {
        let error = Schedule::default()
            .decide(ScheduleCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                cron: "every tuesday probably".to_string(),
                prompt: "hi".to_string(),
                at_ms: 0,
            })
            .expect_err("should refuse");
        assert!(matches!(error, ScheduleError::BadCron(_)));
    }

    #[test]
    fn an_empty_prompt_is_refused() {
        let error = Schedule::default()
            .decide(ScheduleCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                cron: "*/2 * * * * *".to_string(),
                prompt: "   ".to_string(),
                at_ms: 0,
            })
            .expect_err("should refuse");
        assert!(matches!(error, ScheduleError::EmptyPrompt));
    }

    #[test]
    fn a_paused_schedule_cannot_fire() {
        let mut schedule = created();
        schedule.apply(&ScheduleEvent::Paused { at_ms: 2_000 });
        let error = schedule
            .decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored("run_1"),
                at_ms: 3_000,
            })
            .expect_err("paused must not fire");
        assert!(matches!(error, ScheduleError::Paused));
    }

    #[test]
    fn pause_resume_fire_round_trip() {
        let mut schedule = created();
        schedule.apply(&ScheduleEvent::Paused { at_ms: 2 });
        let events = schedule
            .decide(ScheduleCommand::Resume { at_ms: 3 })
            .expect("resume");
        for event in &events {
            schedule.apply(event);
        }
        schedule
            .decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored("run_1"),
                at_ms: 4,
            })
            .expect("a resumed schedule fires");
    }

    #[test]
    fn a_deleted_schedule_refuses_everything() {
        let mut schedule = created();
        schedule.apply(&ScheduleEvent::Deleted { at_ms: 2 });
        assert!(matches!(
            schedule.decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored("run_1"),
                at_ms: 3,
            }),
            Err(ScheduleError::Deleted)
        ));
        assert!(matches!(
            schedule.decide(ScheduleCommand::Pause { at_ms: 3 }),
            Err(ScheduleError::Deleted)
        ));
    }
}
