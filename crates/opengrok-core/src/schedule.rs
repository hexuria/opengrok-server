//! The schedule aggregate — a coworker told to act on its own clock.
//!
//! THIS IS THE MISSION'S OTHER HALF. Everything before this slice answers when a client asks; a
//! schedule is the server deciding, at a time the operator wrote down, that a coworker should take
//! a turn — laptop open or not.
//!
//! THE CRON EXPRESSION IS VALIDATED IN `decide`, NOT AT THE EDGE. A schedule whose expression
//! cannot be parsed would sit in the log as a row that never fires and never explains itself; the
//! aggregate refusing it makes "it was accepted" and "it will fire" the same claim. A webhook
//! wake is the other accepted kind: it has no expression, so `decide` does not ask the clock,
//! and the sweep never claims it (`next_due_ms` stays NULL).
//!
//! FIRING IS AN EVENT because it is provenance: a run that no client started must say what started
//! it, and `Fired { run_id }` is that answer, in the same log as everything else. Pausing exists
//! (rather than delete-and-recreate) because "stop for the weekend" should not cost the schedule
//! its history. A webhook POST is not the person's "run now": a paused webhook refuses, the same
//! as the clock.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::id::{CoworkerId, RunId};

/// How a schedule wakes. Cron is the original and the default for events written before webhooks
/// existed — a missing `kind` must replay as a clock, never as a hook that has no secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WakeKind {
    #[default]
    Cron,
    Webhook,
}

impl WakeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cron => "cron",
            Self::Webhook => "webhook",
        }
    }

    pub fn from_stored(value: &str) -> Self {
        match value {
            "webhook" => Self::Webhook,
            _ => Self::Cron,
        }
    }
}

/// What `Create` / `Update` install as the wake. Cron still has to parse; a webhook carries the
/// public hook id and the bearer the owner will paste into an external app. The hash is what
/// `POST /hooks/{id}` compares; the key is stored so a later list can show it without minting
/// again. Rotating writes `SecretRotated`, which replaces both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    Cron {
        cron: String,
    },
    Webhook {
        hook_id: String,
        secret_hash: String,
        webhook_key: String,
    },
}

/// Who asked this firing to exist. The event still stores two bools (`manual`, `webhook`) so
/// rows written before webhooks deserialize; this enum is the command's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireCause {
    Clock,
    Manual,
    Webhook,
}

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

/// The inverse for the wire: the desktop's routine editor writes and re-reads the 5-field form,
/// so a stored `0 0 9 * * 1` goes back out as `0 9 * * 1`. A 6-field expression whose seconds are
/// not `0` (tests scheduling in seconds) is returned as it is — there is no 5-field form for it.
pub fn display_cron(normalized: &str) -> String {
    let fields: Vec<&str> = normalized.split_whitespace().collect();
    if fields.len() == 6 && fields[0] == "0" {
        fields[1..].join(" ")
    } else {
        normalized.to_string()
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
        /// Already normalized; what `next_fire_ms` will be asked about ever after. Empty on a
        /// webhook wake, which has no clock.
        cron: String,
        /// The user message every firing opens its run with.
        prompt: String,
        /// What the person called it — the desktop's Routines pane lists by name. Absent on rows
        /// written before names existed, which replay as unnamed rather than as corrupt.
        #[serde(default)]
        name: String,
        /// Absent on rows written before webhooks existed; those replay as cron.
        #[serde(default)]
        kind: WakeKind,
        /// Public id in `POST /hooks/{id}`. Empty on a cron wake.
        #[serde(default)]
        hook_id: String,
        /// SHA-256 hex of the bearer. Empty on a cron wake. POST compares against this, not the
        /// plaintext, so a leaked listing of hashes is not a working Authorization header.
        #[serde(default)]
        secret_hash: String,
        /// The bearer the owner pastes into an external app. Stored so create/update/list can
        /// show it; rotating replaces it. Absent on cron wakes and on rows from before webhooks.
        #[serde(default)]
        webhook_key: String,
        at_ms: i64,
    },
    /// The person edited the routine in place. An edit is not delete-and-create: the schedule
    /// keeps its id, its history and its runs.
    Updated {
        name: String,
        /// Already normalized, re-validated in `decide`. Empty when the wake is a webhook.
        cron: String,
        prompt: String,
        at_ms: i64,
        /// `None` on rows written before webhooks: apply leaves the existing kind/hook/secret
        /// alone, so an old prompt-only edit cannot turn a webhook into a clock.
        #[serde(default)]
        kind: Option<WakeKind>,
        #[serde(default)]
        hook_id: Option<String>,
        #[serde(default)]
        secret_hash: Option<String>,
        #[serde(default)]
        webhook_key: Option<String>,
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
    /// Replace the inbound bearer. The hook id (and so the POST URL) stays; the old key 401s.
    SecretRotated {
        secret_hash: String,
        webhook_key: String,
        at_ms: i64,
    },
    /// A run this schedule started. The run's own log holds what happened; this holds *why it
    /// exists*.
    Fired {
        run_id: RunId,
        /// `true` when a person pressed "run now" rather than the clock firing it. The Routines
        /// pane shows the two differently; absent on rows written before the distinction existed.
        #[serde(default)]
        manual: bool,
        /// `true` when `POST /hooks/{id}` started it. Absent on rows written before webhooks;
        /// those replay as a clock firing unless `manual` is set.
        #[serde(default)]
        webhook: bool,
        at_ms: i64,
    },
}

impl ScheduleEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Created { .. } => "schedule-created",
            Self::Updated { .. } => "schedule-updated",
            Self::Paused { .. } => "schedule-paused",
            Self::Resumed { .. } => "schedule-resumed",
            Self::Deleted { .. } => "schedule-deleted",
            Self::SecretRotated { .. } => "schedule-secret-rotated",
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
    pub name: String,
    pub kind: WakeKind,
    pub hook_id: String,
    pub secret_hash: String,
    /// Plaintext bearer for the owner to copy. Empty on a cron wake.
    pub webhook_key: String,
    /// Runs a person started with "run now", by id — so a listing can label them `manual`.
    pub manual_runs: std::collections::BTreeSet<String>,
    /// Runs an inbound POST started, by id — so a listing can label them `webhook`.
    pub webhook_runs: std::collections::BTreeSet<String>,
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
    #[error("a webhook needs a hook id and a signing secret")]
    BadWebhook,
    #[error("that schedule is not a webhook")]
    NotWebhook,
}

#[derive(Debug, Clone)]
pub enum ScheduleCommand {
    Create {
        coworker_id: CoworkerId,
        prompt: String,
        name: String,
        wake: Wake,
        at_ms: i64,
    },
    Update {
        name: String,
        prompt: String,
        wake: Wake,
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
    RotateWebhookSecret {
        secret_hash: String,
        webhook_key: String,
        at_ms: i64,
    },
    Fire {
        run_id: RunId,
        cause: FireCause,
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
                name,
                kind,
                hook_id,
                secret_hash,
                webhook_key,
                ..
            } => {
                self.created = true;
                self.coworker_id = Some(coworker_id.clone());
                self.cron = cron.clone();
                self.prompt = prompt.clone();
                self.name = name.clone();
                self.kind = *kind;
                self.hook_id = hook_id.clone();
                self.secret_hash = secret_hash.clone();
                self.webhook_key = webhook_key.clone();
            }
            ScheduleEvent::Updated {
                name,
                cron,
                prompt,
                kind,
                hook_id,
                secret_hash,
                webhook_key,
                ..
            } => {
                self.name = name.clone();
                self.cron = cron.clone();
                self.prompt = prompt.clone();
                if let Some(kind) = kind {
                    self.kind = *kind;
                }
                if let Some(hook_id) = hook_id {
                    self.hook_id = hook_id.clone();
                }
                if let Some(secret_hash) = secret_hash {
                    self.secret_hash = secret_hash.clone();
                }
                if let Some(webhook_key) = webhook_key {
                    self.webhook_key = webhook_key.clone();
                }
            }
            ScheduleEvent::Paused { .. } => self.paused = true,
            ScheduleEvent::Resumed { .. } => self.paused = false,
            ScheduleEvent::Deleted { .. } => self.deleted = true,
            ScheduleEvent::SecretRotated {
                secret_hash,
                webhook_key,
                ..
            } => {
                self.secret_hash = secret_hash.clone();
                self.webhook_key = webhook_key.clone();
            }
            ScheduleEvent::Fired {
                run_id,
                manual,
                webhook,
                ..
            } => {
                if *webhook {
                    self.webhook_runs.insert(run_id.as_str().to_string());
                } else if *manual {
                    self.manual_runs.insert(run_id.as_str().to_string());
                }
            }
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

    fn created_from_wake(
        coworker_id: CoworkerId,
        prompt: String,
        name: String,
        wake: Wake,
        at_ms: i64,
    ) -> Result<ScheduleEvent, ScheduleError> {
        if prompt.trim().is_empty() {
            return Err(ScheduleError::EmptyPrompt);
        }
        let name = name.trim().to_string();
        match wake {
            Wake::Cron { cron } => {
                let cron = normalized_cron(&cron);
                // Accepted must mean "will fire": an unparseable expression, or one with no
                // future occurrence at all, is refused here rather than stored as a dead row.
                if next_fire_ms(&cron, at_ms).is_none() {
                    return Err(ScheduleError::BadCron(cron));
                }
                Ok(ScheduleEvent::Created {
                    coworker_id,
                    cron,
                    prompt,
                    name,
                    kind: WakeKind::Cron,
                    hook_id: String::new(),
                    secret_hash: String::new(),
                    webhook_key: String::new(),
                    at_ms,
                })
            }
            Wake::Webhook {
                hook_id,
                secret_hash,
                webhook_key,
            } => {
                if hook_id.trim().is_empty()
                    || secret_hash.trim().is_empty()
                    || webhook_key.trim().is_empty()
                {
                    return Err(ScheduleError::BadWebhook);
                }
                Ok(ScheduleEvent::Created {
                    coworker_id,
                    cron: String::new(),
                    prompt,
                    name,
                    kind: WakeKind::Webhook,
                    hook_id: hook_id.trim().to_string(),
                    secret_hash: secret_hash.trim().to_string(),
                    webhook_key,
                    at_ms,
                })
            }
        }
    }

    fn updated_from_wake(
        name: String,
        prompt: String,
        wake: Wake,
        at_ms: i64,
    ) -> Result<ScheduleEvent, ScheduleError> {
        if prompt.trim().is_empty() {
            return Err(ScheduleError::EmptyPrompt);
        }
        let name = name.trim().to_string();
        match wake {
            Wake::Cron { cron } => {
                let cron = normalized_cron(&cron);
                if next_fire_ms(&cron, at_ms).is_none() {
                    return Err(ScheduleError::BadCron(cron));
                }
                Ok(ScheduleEvent::Updated {
                    name,
                    cron,
                    prompt,
                    at_ms,
                    kind: Some(WakeKind::Cron),
                    hook_id: Some(String::new()),
                    secret_hash: Some(String::new()),
                    webhook_key: Some(String::new()),
                })
            }
            Wake::Webhook {
                hook_id,
                secret_hash,
                webhook_key,
            } => {
                if hook_id.trim().is_empty()
                    || secret_hash.trim().is_empty()
                    || webhook_key.trim().is_empty()
                {
                    return Err(ScheduleError::BadWebhook);
                }
                Ok(ScheduleEvent::Updated {
                    name,
                    cron: String::new(),
                    prompt,
                    at_ms,
                    kind: Some(WakeKind::Webhook),
                    hook_id: Some(hook_id.trim().to_string()),
                    secret_hash: Some(secret_hash.trim().to_string()),
                    webhook_key: Some(webhook_key),
                })
            }
        }
    }

    pub fn decide(&self, command: ScheduleCommand) -> Result<Vec<ScheduleEvent>, ScheduleError> {
        match command {
            ScheduleCommand::Create {
                coworker_id,
                prompt,
                name,
                wake,
                at_ms,
            } => Ok(vec![Self::created_from_wake(
                coworker_id,
                prompt,
                name,
                wake,
                at_ms,
            )?]),

            ScheduleCommand::Update {
                name,
                prompt,
                wake,
                at_ms,
            } => {
                self.alive()?;
                Ok(vec![Self::updated_from_wake(name, prompt, wake, at_ms)?])
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

            ScheduleCommand::RotateWebhookSecret {
                secret_hash,
                webhook_key,
                at_ms,
            } => {
                self.alive()?;
                if self.kind != WakeKind::Webhook {
                    return Err(ScheduleError::NotWebhook);
                }
                if secret_hash.trim().is_empty() || webhook_key.trim().is_empty() {
                    return Err(ScheduleError::BadWebhook);
                }
                Ok(vec![ScheduleEvent::SecretRotated {
                    secret_hash: secret_hash.trim().to_string(),
                    webhook_key,
                    at_ms,
                }])
            }

            ScheduleCommand::Fire {
                run_id,
                cause,
                at_ms,
            } => {
                self.alive()?;
                let (manual, webhook) = match cause {
                    FireCause::Clock => (false, false),
                    FireCause::Manual => (true, false),
                    FireCause::Webhook => (false, true),
                };
                // A paused schedule refusing to fire is the whole point of pause. The sweep should
                // never ask (paused rows are not claimed), so this firing twice as a guard is
                // deliberate: the projection being wrong must not be enough to fire a run. A
                // person's "run now" is the one exception: they asked, paused or not. An inbound
                // webhook is not that exception — a paused routine must not run because a todo
                // app POSTed.
                if self.paused && !manual {
                    return Err(ScheduleError::Paused);
                }
                Ok(vec![ScheduleEvent::Fired {
                    run_id,
                    manual,
                    webhook,
                    at_ms,
                }])
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
    pub name: String,
    pub active: bool,
    pub next_due_ms: Option<i64>,
    pub created_at_ms: i64,
    /// When it last fired (clock, "run now", or webhook); `None` until it has.
    pub last_fired_ms: Option<i64>,
    /// Cron or webhook. Rows from before the column existed read as cron.
    #[serde(default)]
    pub kind: WakeKind,
    /// Public hook id when `kind` is webhook; empty otherwise.
    #[serde(default)]
    pub hook_id: String,
    /// SHA-256 hex of the bearer, so an inbound POST can be refused from the projection without
    /// replaying the stream. EMPTY on a webhook row projected before the column existed — a
    /// reader must ask the aggregate for those rather than read the emptiness as "no key".
    #[serde(default)]
    pub secret_hash: String,
    /// The bearer itself, so a listing can show the owner theirs without replaying the stream.
    /// Empty on a cron row, and on a webhook row projected before the column existed.
    #[serde(default)]
    pub webhook_key: String,
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
            name: "queue check".to_string(),
            kind: WakeKind::Cron,
            hook_id: String::new(),
            secret_hash: String::new(),
            webhook_key: String::new(),
            at_ms: 1_000,
        }])
    }

    fn webhook() -> Schedule {
        Schedule::replay(&[ScheduleEvent::Created {
            coworker_id: CoworkerId::from_stored("cw_1"),
            cron: String::new(),
            prompt: "handle the ping".to_string(),
            name: "todo ping".to_string(),
            kind: WakeKind::Webhook,
            hook_id: "hook_abc".to_string(),
            secret_hash: "hash".to_string(),
            webhook_key: "og_secret".to_string(),
            at_ms: 1_000,
        }])
    }

    fn cron_wake(cron: &str) -> Wake {
        Wake::Cron {
            cron: cron.to_string(),
        }
    }

    #[test]
    fn five_field_cron_is_promoted_and_six_field_is_kept() {
        assert_eq!(normalized_cron("*/5 * * * *"), "0 */5 * * * *");
        assert_eq!(normalized_cron("*/2 * * * * *"), "*/2 * * * * *");
    }

    #[test]
    fn an_update_keeps_the_id_and_revalidates_the_cron() {
        let mut schedule = created();
        assert!(matches!(
            schedule.decide(ScheduleCommand::Update {
                name: "x".to_string(),
                prompt: "y".to_string(),
                wake: cron_wake("not cron"),
                at_ms: 2,
            }),
            Err(ScheduleError::BadCron(_))
        ));
        let events = schedule
            .decide(ScheduleCommand::Update {
                name: "Monday report".to_string(),
                prompt: "write the weekly report".to_string(),
                wake: cron_wake("0 9 * * 1"),
                at_ms: 2,
            })
            .expect("update");
        for event in &events {
            schedule.apply(event);
        }
        assert_eq!(schedule.name, "Monday report");
        assert_eq!(schedule.cron, "0 0 9 * * 1");
        assert_eq!(display_cron(&schedule.cron), "0 9 * * 1");
        assert_eq!(display_cron("*/2 * * * * *"), "*/2 * * * * *");
        assert_eq!(schedule.prompt, "write the weekly report");
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
                prompt: "hi".to_string(),
                name: String::new(),
                wake: cron_wake("every tuesday probably"),
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
                prompt: "   ".to_string(),
                name: String::new(),
                wake: cron_wake("*/2 * * * * *"),
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
                cause: FireCause::Clock,
                at_ms: 3_000,
            })
            .expect_err("paused must not fire");
        // A person's "run now" is the exception: they asked.
        let events = schedule
            .decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored("run_manual"),
                cause: FireCause::Manual,
                at_ms: 3_000,
            })
            .expect("a manual fire on a paused schedule");
        for event in &events {
            schedule.apply(event);
        }
        assert!(schedule.manual_runs.contains("run_manual"));
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
                cause: FireCause::Clock,
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
                cause: FireCause::Clock,
                at_ms: 3,
            }),
            Err(ScheduleError::Deleted)
        ));
        assert!(matches!(
            schedule.decide(ScheduleCommand::Pause { at_ms: 3 }),
            Err(ScheduleError::Deleted)
        ));
    }

    #[test]
    fn a_webhook_create_skips_the_clock_and_keeps_the_secret() {
        let events = Schedule::default()
            .decide(ScheduleCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                prompt: "handle the ping".to_string(),
                name: "todo ping".to_string(),
                wake: Wake::Webhook {
                    hook_id: "hook_abc".to_string(),
                    secret_hash: "hash".to_string(),
                    webhook_key: "og_secret".to_string(),
                },
                at_ms: 1,
            })
            .expect("webhook create");
        let schedule = Schedule::replay(&events);
        assert_eq!(schedule.kind, WakeKind::Webhook);
        assert!(schedule.cron.is_empty());
        assert_eq!(schedule.hook_id, "hook_abc");
        assert_eq!(schedule.webhook_key, "og_secret");
        assert_eq!(schedule.secret_hash, "hash");
    }

    #[test]
    fn a_webhook_without_a_secret_is_refused() {
        let error = Schedule::default()
            .decide(ScheduleCommand::Create {
                coworker_id: CoworkerId::from_stored("cw_1"),
                prompt: "handle the ping".to_string(),
                name: "todo ping".to_string(),
                wake: Wake::Webhook {
                    hook_id: "hook_abc".to_string(),
                    secret_hash: String::new(),
                    webhook_key: "og_secret".to_string(),
                },
                at_ms: 1,
            })
            .expect_err("empty hash");
        assert!(matches!(error, ScheduleError::BadWebhook));
    }

    #[test]
    fn a_paused_webhook_refuses_an_inbound_fire_but_not_run_now() {
        let mut schedule = webhook();
        schedule.apply(&ScheduleEvent::Paused { at_ms: 2 });
        assert!(matches!(
            schedule.decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored("run_hook"),
                cause: FireCause::Webhook,
                at_ms: 3,
            }),
            Err(ScheduleError::Paused)
        ));
        let events = schedule
            .decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored("run_manual"),
                cause: FireCause::Manual,
                at_ms: 3,
            })
            .expect("run now on a paused webhook");
        for event in &events {
            schedule.apply(event);
        }
        assert!(schedule.manual_runs.contains("run_manual"));
        assert!(schedule.webhook_runs.is_empty());
    }

    #[test]
    fn rotating_the_secret_replaces_the_key_and_keeps_the_hook_id() {
        let mut schedule = webhook();
        let events = schedule
            .decide(ScheduleCommand::RotateWebhookSecret {
                secret_hash: "hash2".to_string(),
                webhook_key: "og_new".to_string(),
                at_ms: 2,
            })
            .expect("rotate");
        for event in &events {
            schedule.apply(event);
        }
        assert_eq!(schedule.hook_id, "hook_abc");
        assert_eq!(schedule.secret_hash, "hash2");
        assert_eq!(schedule.webhook_key, "og_new");
        assert!(matches!(
            created().decide(ScheduleCommand::RotateWebhookSecret {
                secret_hash: "x".to_string(),
                webhook_key: "og_y".to_string(),
                at_ms: 2,
            }),
            Err(ScheduleError::NotWebhook)
        ));
    }

    #[test]
    fn a_webhook_fire_is_labelled_webhook_not_manual() {
        let mut schedule = webhook();
        let events = schedule
            .decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored("run_hook"),
                cause: FireCause::Webhook,
                at_ms: 2,
            })
            .expect("webhook fire");
        for event in &events {
            schedule.apply(event);
        }
        assert!(schedule.webhook_runs.contains("run_hook"));
        assert!(!schedule.manual_runs.contains("run_hook"));
    }

    #[test]
    fn old_created_events_replay_as_cron() {
        let event: ScheduleEvent = serde_json::from_str(
            r#"{"type":"created","coworker_id":"cw_1","cron":"0 */5 * * * *","prompt":"x","name":"n","at_ms":1}"#,
        )
        .expect("old created");
        let schedule = Schedule::replay(&[event]);
        assert_eq!(schedule.kind, WakeKind::Cron);
        assert!(schedule.hook_id.is_empty());
        assert!(schedule.webhook_key.is_empty());
    }
}
