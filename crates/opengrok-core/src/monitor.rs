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
//!
//! "OUR OWN EVENT LOG" IS EVERY TENANT'S. A monitor sees only the streams that resolve to the
//! account that created it (`PgStore::stream_owner` has the table); a stream with no single owner
//! matches nobody.

use serde::{Deserialize, Serialize};

use crate::id::{CoworkerId, RunId};

/// The event types a monitor may watch — the published list a `watches` value is checked against.
///
/// Each is spelled exactly as its aggregate's `event_type()` spells it, and each lands on a stream
/// the sweep can name one owning account for (`PgStore::stream_owner`), so a monitor on any of
/// them can fire for its owner and nobody else. Left out on purpose:
/// - `run-emitted`: written once per streamed frame, so a monitor on it would fire a run per token.
/// - `run-tool-started`: written before every tool round — the journal's own bookkeeping, not a
///   fact about the run a coworker should be woken for.
/// - `run-resumed`: the sweep carrying a run on after a restart (#91). The run still ends in one
///   of the endings above, which is what a monitor wants; waking on the resume too would fire
///   twice for one run, and once more for every restart it lives through.
/// - `session-*`, `account-registered`, `credentials-set`: a sign-in, a refresh, a signup — noise
///   at best, and nothing a coworker should be woken to reason about.
/// - `org-*`: an org's stream has several members and no single owner, so it can match nobody.
/// - `monitor-*`: watching firings is the cascade `WatchingItself` exists to refuse.
///
/// A TYPO MAY ONLY NARROW. Before this list, `run-faild` was accepted and silently never fired; a
/// value not here is now a refusal that names what can be watched.
pub const WATCHABLE: &[&str] = &[
    // run/{id} — the account whose run it was
    "run-started",
    "run-suspended",
    "run-answered",
    "run-finished",
    "run-failed",
    "run-stopped",
    // coworker/{id} — the account that hired it
    "coworker-hired",
    "coworker-renamed",
    "coworker-repinned",
    "coworker-role-set",
    "coworker-visibility-set",
    "computer-assigned",
    "computer-released",
    "coworker-retired",
    "group-hired",
    "members-set",
    // connection/{id} — the signed-in account, or the account that hired the coworker it is for
    "connection-connected",
    "connection-refreshed",
    "connection-loaned",
    "connection-loan-revoked",
    "connection-disconnected",
    // schedule/{id} — the account that wrote the routine
    "schedule-created",
    "schedule-updated",
    "schedule-paused",
    "schedule-resumed",
    "schedule-deleted",
    "schedule-secret-rotated",
    "schedule-fired",
    // account/{id} — the account itself
    "plan-changed",
    "email-verified",
    "account-enabled",
    "account-disabled",
    "account-profile-updated",
    "account-password-changed",
];

pub fn is_watchable(event_type: &str) -> bool {
    WATCHABLE.contains(&event_type)
}

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
        /// log alone. Empty on a person's run now, which matched nothing.
        matched_stream: String,
        /// `true` when a person pressed "run now". Absent on firings written before it existed,
        /// which were all the log's doing.
        #[serde(default)]
        manual: bool,
        at_ms: i64,
    },
    /// The person edited the monitor in place: same id, same thread, same firings.
    Updated {
        watches: String,
        prompt: String,
        /// `None` keeps the coworker it had.
        #[serde(default)]
        coworker_id: Option<CoworkerId>,
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
            Self::Updated { .. } => "monitor-updated",
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
    /// Runs a person started with "run now", by id, so the history can label them `manual`.
    pub manual_runs: std::collections::BTreeSet<String>,
    /// Runs a matched log event started, by id. Kept rather than inferred as "not manual": the
    /// monitor's thread can hold a person's replies too, and those are no firing at all.
    pub event_runs: std::collections::BTreeSet<String>,
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
    #[error("{0:?} is not an event a monitor can watch; watchable events are: {list}", list = WATCHABLE.join(", "))]
    NotWatchable(String),
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
        /// A person's run now: the one wake a pause does not refuse.
        manual: bool,
        at_ms: i64,
    },
    Update {
        watches: String,
        prompt: String,
        /// `None` keeps the coworker it has.
        coworker_id: Option<CoworkerId>,
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
            MonitorEvent::Fired { run_id, manual, .. } => {
                let runs = if *manual {
                    &mut self.manual_runs
                } else {
                    &mut self.event_runs
                };
                runs.insert(run_id.as_str().to_string());
            }
            MonitorEvent::Updated {
                watches,
                prompt,
                coworker_id,
                ..
            } => {
                self.watches = watches.clone();
                self.prompt = prompt.clone();
                if let Some(coworker_id) = coworker_id {
                    self.coworker_id = Some(coworker_id.clone());
                }
            }
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

    /// What a monitor may watch and say, checked the same way on create and on every edit: an
    /// edit is the easier place to type `monitor-fired`, so it must be refused there too.
    fn checked(watches: String, prompt: &str) -> Result<String, MonitorError> {
        let watches = watches.trim().to_string();
        if watches.is_empty() {
            return Err(MonitorError::NothingWatched);
        }
        // One monitor watching `monitor-fired` turns every other monitor's firing into its
        // trigger — a cascade the per-monitor guard cannot see, because the runs are not its own.
        // Refused at the root instead.
        if watches == "monitor-fired" {
            return Err(MonitorError::WatchingItself);
        }
        if !is_watchable(&watches) {
            return Err(MonitorError::NotWatchable(watches));
        }
        if prompt.trim().is_empty() {
            return Err(MonitorError::EmptyPrompt);
        }
        Ok(watches)
    }

    pub fn decide(&self, command: MonitorCommand) -> Result<Vec<MonitorEvent>, MonitorError> {
        match command {
            MonitorCommand::Create {
                coworker_id,
                watches,
                prompt,
                at_ms,
            } => {
                let watches = Self::checked(watches, &prompt)?;
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
                manual,
                at_ms,
            } => {
                self.alive()?;
                // The pause is what keeps the log from waking it. A person pressing "run now"
                // asked, paused or not — the same exception a schedule makes.
                if self.paused && !manual {
                    return Err(MonitorError::Paused);
                }
                Ok(vec![MonitorEvent::Fired {
                    run_id,
                    matched_stream,
                    manual,
                    at_ms,
                }])
            }

            MonitorCommand::Update {
                watches,
                prompt,
                coworker_id,
                at_ms,
            } => {
                self.alive()?;
                let watches = Self::checked(watches, &prompt)?;
                Ok(vec![MonitorEvent::Updated {
                    watches,
                    prompt,
                    coworker_id,
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

    /// A typo is not a monitor that never fires: it is a refusal that names what can be watched.
    #[test]
    fn an_unknown_watch_is_refused() {
        let create = |watches: &str| {
            Monitor::default().decide(MonitorCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                watches: watches.to_string(),
                prompt: "hi".to_string(),
                at_ms: 0,
            })
        };
        for refused in [
            "run-faild",
            "not-an-event",
            "run-emitted",
            "session-refreshed",
        ] {
            assert!(
                matches!(create(refused), Err(MonitorError::NotWatchable(ref named)) if named == refused),
                "{refused} must be refused"
            );
        }
        let said = create("run-faild").expect_err("refused").to_string();
        assert!(
            said.contains("run-failed"),
            "the refusal lists what can be watched: {said}"
        );
        assert!(create("run-failed").is_ok());
        assert!(create(" connection-disconnected ").is_ok());
    }

    #[test]
    fn a_paused_monitor_cannot_fire() {
        let mut monitor = created();
        monitor.apply(&MonitorEvent::Paused { at_ms: 2 });
        assert!(matches!(
            monitor.decide(MonitorCommand::Fire {
                run_id: RunId::from_stored("run_1"),
                matched_stream: "run/run_0".to_string(),
                manual: false,
                at_ms: 3,
            }),
            Err(MonitorError::Paused)
        ));
    }

    fn update(watches: &str, prompt: &str, coworker: Option<&str>) -> MonitorCommand {
        MonitorCommand::Update {
            watches: watches.to_string(),
            prompt: prompt.to_string(),
            coworker_id: coworker.map(CoworkerId::from_stored),
            at_ms: 2,
        }
    }

    /// An edit keeps the monitor — its id, its thread, the runs it fired — and changes what it
    /// watches, what it says and who takes it.
    #[test]
    fn an_update_rewrites_watches_prompt_and_coworker() {
        let mut monitor = created();
        let events = monitor
            .decide(update(
                "connection-disconnected",
                "reconnect it",
                Some("cw_2"),
            ))
            .expect("update");
        for event in &events {
            monitor.apply(event);
        }
        assert_eq!(monitor.watches, "connection-disconnected");
        assert_eq!(monitor.prompt, "reconnect it");
        assert_eq!(monitor.coworker_id, Some(CoworkerId::from_stored("cw_2")));

        let events = monitor
            .decide(update("connection-disconnected", "reconnect it", None))
            .expect("update");
        for event in &events {
            monitor.apply(event);
        }
        assert_eq!(
            monitor.coworker_id,
            Some(CoworkerId::from_stored("cw_2")),
            "an edit naming no coworker keeps the one it had"
        );
    }

    /// Whatever create refuses, an edit refuses: the loop guard's root rule most of all.
    #[test]
    fn an_update_refuses_what_create_refuses() {
        let monitor = created();
        assert!(matches!(
            monitor.decide(update("monitor-fired", "watch the watchers", None)),
            Err(MonitorError::WatchingItself)
        ));
        assert!(matches!(
            monitor.decide(update("run-faild", "hi", None)),
            Err(MonitorError::NotWatchable(_))
        ));
        assert!(matches!(
            monitor.decide(update("run-failed", "  ", None)),
            Err(MonitorError::EmptyPrompt)
        ));
        assert!(matches!(
            monitor.decide(update(" ", "hi", None)),
            Err(MonitorError::NothingWatched)
        ));
        let mut deleted = created();
        deleted.apply(&MonitorEvent::Deleted { at_ms: 2 });
        assert!(matches!(
            deleted.decide(update("run-failed", "hi", None)),
            Err(MonitorError::Deleted)
        ));
    }

    /// A PERSON'S RUN NOW IS THE ONE WAKE A PAUSE DOES NOT REFUSE, as for schedules; and the
    /// monitor stays paused. Every firing is remembered by cause, so its history can label it.
    #[test]
    fn a_manual_fire_runs_while_paused_and_is_remembered_as_manual() {
        let mut monitor = created();
        monitor.apply(&MonitorEvent::Paused { at_ms: 2 });
        let events = monitor
            .decide(MonitorCommand::Fire {
                run_id: RunId::from_stored("run_by_hand"),
                matched_stream: String::new(),
                manual: true,
                at_ms: 3,
            })
            .expect("a person's run now fires while paused");
        for event in &events {
            monitor.apply(event);
        }
        assert!(monitor.paused, "and the monitor is still paused");
        assert!(monitor.manual_runs.contains("run_by_hand"));
        assert!(!monitor.event_runs.contains("run_by_hand"));

        let mut live = created();
        let events = live
            .decide(MonitorCommand::Fire {
                run_id: RunId::from_stored("run_by_event"),
                matched_stream: "run/run_0".to_string(),
                manual: false,
                at_ms: 3,
            })
            .expect("fire");
        for event in &events {
            live.apply(event);
        }
        assert!(live.event_runs.contains("run_by_event"));
    }

    /// Events written before edits and manual firings existed replay as they always did.
    #[test]
    fn old_fired_events_replay_as_event_firings() {
        let fired: MonitorEvent = serde_json::from_str(
            r#"{"type":"fired","run_id":"run_old","matched_stream":"run/run_0","at_ms":5}"#,
        )
        .expect("an old fired event");
        let monitor = Monitor::replay(&[
            MonitorEvent::Created {
                coworker_id: CoworkerId::from_stored("cw_1"),
                watches: "run-failed".to_string(),
                prompt: "p".to_string(),
                at_ms: 1,
            },
            fired,
        ]);
        assert!(monitor.event_runs.contains("run_old"));
        let updated: MonitorEvent = serde_json::from_str(
            r#"{"type":"updated","watches":"run-failed","prompt":"q","at_ms":6}"#,
        )
        .expect("an updated event with no coworker");
        let mut monitor = monitor;
        monitor.apply(&updated);
        assert_eq!(monitor.coworker_id, Some(CoworkerId::from_stored("cw_1")));
    }

    #[test]
    fn a_deleted_monitor_refuses_everything() {
        let mut monitor = created();
        monitor.apply(&MonitorEvent::Deleted { at_ms: 2 });
        assert!(matches!(
            monitor.decide(MonitorCommand::Fire {
                run_id: RunId::from_stored("run_1"),
                matched_stream: "run/run_0".to_string(),
                manual: false,
                at_ms: 3,
            }),
            Err(MonitorError::Deleted)
        ));
    }
}
