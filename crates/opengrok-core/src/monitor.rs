//! The monitor aggregate — a coworker that reacts to what happens, not to what a clock says.
//!
//! A MONITOR WATCHES OUR OWN EVENT LOG. Every fact this server records — a run failing, a
//! connection disconnecting, a coworker being hired — is already an append-only row in `events`;
//! a monitor is a standing question against that stream ("when a `run-failed` appears, have this
//! coworker look at it"), which means event-based reaction costs no new infrastructure and can
//! never see anything the log did not record.
//!
//! THE LOOP GUARD IS A DOMAIN RULE, NOT AN IMPLEMENTATION DETAIL. A fired run writes events; a
//! monitor that matched its own firings would fire on them, forever, at the sweep's pace. So every
//! firing is recorded (`Fired { run_id }`), and the sweep must never match an event that
//! originates from a run this monitor started or from the monitor's own stream. The aggregate
//! keeps the record; the store enforces the exclusion.

use serde::{Deserialize, Serialize};

use crate::id::{CoworkerId, RunId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MonitorEvent {
    Created {
        coworker_id: CoworkerId,
        /// The `event_type` this monitor reacts to — exact match, e.g. `run-failed`.
        watches: String,
        /// The user message every firing opens its run with. The matched event is appended to it
        /// so the coworker knows what it was woken for.
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
    /// A run this monitor started, and the reason. The run id is what the loop guard excludes.
    Fired {
        run_id: RunId,
        /// The stream the matched event came from, so "why did this fire" is answerable from the
        /// log alone.
        matched_stream: String,
        at_ms: i64,
    },
}

impl MonitorEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Created { .. } => "monitor-created",
            Self::Paused { .. } => "monitor-paused",
            Self::Resumed { .. } => "monitor-resumed",
            Self::Deleted { .. } => "monitor-deleted",
            Self::Fired { .. } => "monitor-fired",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Monitor {
    pub created: bool,
    pub deleted: bool,
    pub paused: bool,
    pub coworker_id: Option<CoworkerId>,
    pub watches: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MonitorError {
    #[error("no such monitor")]
    NotCreated,
    #[error("that monitor has been deleted")]
    Deleted,
    #[error("that monitor is already paused")]
    AlreadyPaused,
    #[error("that monitor is not paused")]
    NotPaused,
    #[error("that monitor is paused")]
    Paused,
    #[error("a monitor needs an event type to watch")]
    NothingWatched,
    #[error("a monitor needs something to say")]
    EmptyPrompt,
    #[error("a monitor may not watch monitor firings: that is the loop it exists to avoid")]
    WatchingItself,
}

#[derive(Debug, Clone)]
pub enum MonitorCommand {
    Create {
        coworker_id: CoworkerId,
        watches: String,
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
        matched_stream: String,
        at_ms: i64,
    },
}

impl Monitor {
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a MonitorEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &MonitorEvent) {
        match event {
            MonitorEvent::Created {
                coworker_id,
                watches,
                prompt,
                ..
            } => {
                self.created = true;
                self.coworker_id = Some(coworker_id.clone());
                self.watches = watches.clone();
                self.prompt = prompt.clone();
            }
            MonitorEvent::Paused { .. } => self.paused = true,
            MonitorEvent::Resumed { .. } => self.paused = false,
            MonitorEvent::Deleted { .. } => self.deleted = true,
            MonitorEvent::Fired { .. } => {}
        }
    }

    fn alive(&self) -> Result<(), MonitorError> {
        if !self.created {
            return Err(MonitorError::NotCreated);
        }
        if self.deleted {
            return Err(MonitorError::Deleted);
        }
        Ok(())
    }

    pub fn decide(&self, command: MonitorCommand) -> Result<Vec<MonitorEvent>, MonitorError> {
        match command {
            MonitorCommand::Create {
                coworker_id,
                watches,
                prompt,
                at_ms,
            } => {
                let watches = watches.trim().to_string();
                if watches.is_empty() {
                    return Err(MonitorError::NothingWatched);
                }
                // One monitor watching `monitor-fired` turns every other monitor's firing into
                // its trigger — a cascade the per-monitor guard cannot see, because the runs are
                // not its own. Refused at the root instead.
                if watches == "monitor-fired" {
                    return Err(MonitorError::WatchingItself);
                }
                if prompt.trim().is_empty() {
                    return Err(MonitorError::EmptyPrompt);
                }
                Ok(vec![MonitorEvent::Created {
                    coworker_id,
                    watches,
                    prompt,
                    at_ms,
                }])
            }

            MonitorCommand::Pause { at_ms } => {
                self.alive()?;
                if self.paused {
                    return Err(MonitorError::AlreadyPaused);
                }
                Ok(vec![MonitorEvent::Paused { at_ms }])
            }

            MonitorCommand::Resume { at_ms } => {
                self.alive()?;
                if !self.paused {
                    return Err(MonitorError::NotPaused);
                }
                Ok(vec![MonitorEvent::Resumed { at_ms }])
            }

            MonitorCommand::Delete { at_ms } => {
                self.alive()?;
                Ok(vec![MonitorEvent::Deleted { at_ms }])
            }

            MonitorCommand::Fire {
                run_id,
                matched_stream,
                at_ms,
            } => {
                self.alive()?;
                if self.paused {
                    return Err(MonitorError::Paused);
                }
                Ok(vec![MonitorEvent::Fired {
                    run_id,
                    matched_stream,
                    at_ms,
                }])
            }
        }
    }
}

/// One row of `monitor_view`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorView {
    pub id: String,
    pub coworker_id: CoworkerId,
    pub watches: String,
    pub prompt: String,
    pub active: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;

    fn created() -> Monitor {
        Monitor::replay(&[MonitorEvent::Created {
            coworker_id: CoworkerId::from_stored("cw_1"),
            watches: "run-failed".to_string(),
            prompt: "a run failed; find out why".to_string(),
            at_ms: 1,
        }])
    }

    #[test]
    fn watching_monitor_firings_is_refused() {
        let error = Monitor::default()
            .decide(MonitorCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                watches: "monitor-fired".to_string(),
                prompt: "watch the watchers".to_string(),
                at_ms: 0,
            })
            .expect_err("must refuse the cascade");
        assert!(matches!(error, MonitorError::WatchingItself));
    }

    #[test]
    fn empty_watch_and_empty_prompt_are_refused() {
        assert!(matches!(
            Monitor::default().decide(MonitorCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                watches: "  ".to_string(),
                prompt: "hi".to_string(),
                at_ms: 0,
            }),
            Err(MonitorError::NothingWatched)
        ));
        assert!(matches!(
            Monitor::default().decide(MonitorCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                watches: "run-failed".to_string(),
                prompt: "".to_string(),
                at_ms: 0,
            }),
            Err(MonitorError::EmptyPrompt)
        ));
    }

    #[test]
    fn a_paused_monitor_cannot_fire() {
        let mut monitor = created();
        monitor.apply(&MonitorEvent::Paused { at_ms: 2 });
        assert!(matches!(
            monitor.decide(MonitorCommand::Fire {
                run_id: RunId::from_stored("run_1"),
                matched_stream: "run/run_0".to_string(),
                at_ms: 3,
            }),
            Err(MonitorError::Paused)
        ));
    }

    #[test]
    fn a_deleted_monitor_refuses_everything() {
        let mut monitor = created();
        monitor.apply(&MonitorEvent::Deleted { at_ms: 2 });
        assert!(matches!(
            monitor.decide(MonitorCommand::Fire {
                run_id: RunId::from_stored("run_1"),
                matched_stream: "run/run_0".to_string(),
                at_ms: 3,
            }),
            Err(MonitorError::Deleted)
        ));
    }
}
