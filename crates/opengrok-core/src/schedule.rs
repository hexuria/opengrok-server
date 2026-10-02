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
//! and the sweep never claims it (`next_due_ms` stays NULL). Its zone (#316) is held the same
//! way: a cron is read in the routine's own IANA zone, and one the zone database does not know
//! would be a clock nothing can read.
//!
//! FIRING IS AN EVENT because it is provenance: a run that no client started must say what started
//! it, and `Fired { run_id }` is that answer, in the same log as everything else. So is a firing
//! that was skipped because the person's own plan could not answer (`Skipped`): no run exists to
//! say so, and the routine's history must. Pausing exists (rather than delete-and-recreate)
//! because "stop for the weekend" should not cost the schedule its history. A webhook POST is not
//! the person's "run now": a paused webhook refuses, the same as the clock.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::id::{CoworkerId, RunId};
use crate::limits::RunLimits;

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

/// Who asked this firing to exist. A `Fired` event still stores two bools (`manual`, `webhook`)
/// so rows written before webhooks deserialize; a `Skipped` one stores this, by its wire word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FireCause {
    Clock,
    Manual,
    Webhook,
}

impl FireCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::Manual => "manual",
            Self::Webhook => "webhook",
        }
    }
}

/// A firing that did not run because the person's own plan could not answer it (#316): when, who
/// asked for it, and why, as the code the run history names it by (`relay_offline`, `proxy_down`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skip {
    pub cause: FireCause,
    pub code: String,
    pub at_ms: i64,
}

/// A skip's code, and the sentence its row says it in: by the way the plan goes, or for the relay
/// its person switched off with no fallback (#332).
pub const SKIPPED: [(&str, &str); 3] = [
    (
        "relay_offline",
        "Skipped: your computer was off, so your plan couldn't answer",
    ),
    ("proxy_down", "Skipped: your plan's proxy didn't answer"),
    ("relay_disabled", "Skipped: Relay is off for your plan"),
];

impl Skip {
    /// The sentence a skip's code is said in, on its history row and its `lastRun`.
    pub fn reason(code: &str) -> &'static str {
        let said = SKIPPED.iter().find(|(known, _)| *known == code);
        said.map_or("Skipped", |(_, why)| why)
    }
}

/// The zone of every routine written before routines had one, which is what they replay as.
pub const UTC: &str = "UTC";

fn utc() -> String {
    UTC.to_string()
}

/// A routine's zone, held to the IANA database exactly as an account's is, case and all.
pub fn zone(tz: &str) -> Result<chrono_tz::Tz, ScheduleError> {
    tz.parse()
        .map_err(|_| ScheduleError::UnknownTimeZone(tz.to_string()))
}

/// The `cron` crate's names for the days, by standard cron's numbers: 0 and 7 are both Sunday.
const WEEKDAYS: [&str; 8] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

/// Why a numbered day of the week was refused, after the item itself.
const NOT_A_DAY: &str = "is not a day of the week: a day is 0 to 7, where 0 and 7 are both \
                         Sunday, or SUN to SAT, and a range runs forward and steps by 1 to 7";

/// A cron expression the way people write them, 5 fields with standard cron's days of the week
/// (0 or 7 for Sunday, 1 for Monday), promoted to the 6-field form the parser wants (seconds
/// first) WITH THOSE DAYS NAMED: `0 9 * * 1` is stored as `0 0 9 * * MON`, 09:00 every Monday.
/// The crate counts Sunday as 1 and refuses 0, so the digits handed over as written fired a day
/// early (`1-5` ran Sunday to Thursday); a name is the same day to both countings.
///
/// A 6- or 7-field expression passes through untouched — which is also what lets tests schedule
/// in seconds — SO ITS DAY OF THE WEEK IS THE CRATE'S, Sunday as 1. Every stored expression is
/// one, and a row stored before this translation existed must fire where it always has.
pub fn normalized_cron(expression: &str) -> Result<String, ScheduleError> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    let [minute, hour, day_of_month, month, days] = fields[..] else {
        return Ok(expression.trim().to_string());
    };
    let days = named_weekdays(days)
        .map_err(|why| ScheduleError::BadCron(format!("{} ({why})", expression.trim())))?;
    Ok(format!("0 {minute} {hour} {day_of_month} {month} {days}"))
}

/// Whether a day-of-week item counts its days by number: a digit before any step. Only those
/// mean different days to the two countings. `*`, `?` and a name do not, and nor do their steps
/// (`*/2` is Sunday, Tuesday, Thursday and Saturday to both).
fn numbers_a_day(item: &str) -> bool {
    let base = item.split_once('/').map_or(item, |(base, _)| base);
    base.bytes().any(|b| b.is_ascii_digit())
}

/// A standard day-of-week field in the crate's names, item by item, so a list keeps its shape:
/// `1,3,5` is `MON,WED,FRI` and `1-5/2` is `MON-FRI/2`. A lone number with a step runs to the
/// field's end, 7, as the crate reads one in every field: `1/2` is Mon, Wed, Fri and Sun.
fn named_weekdays(field: &str) -> Result<String, String> {
    let mut named = Vec::new();
    for item in field.split(',') {
        if !numbers_a_day(item) {
            named.push(item.to_string());
            continue;
        }
        let base = item.split_once('/').map_or(item, |(base, _)| base);
        let step = item.split_once('/').map(|(_, step)| step);
        let refused = || format!("{item} {NOT_A_DAY}");
        let number = |text: &str| match text.parse::<usize>() {
            Ok(n) if n <= 7 && text.bytes().all(|b| b.is_ascii_digit()) => Ok(n),
            _ => Err(refused()),
        };
        let (first, last) = match base.split_once('-') {
            Some((first, last)) => (number(first)?, number(last)?),
            None => {
                let day = number(base)?;
                (day, if step.is_some() { 7 } else { day })
            }
        };
        let every = step.map_or(Ok(1), number)?;
        if first > last || every == 0 {
            return Err(refused());
        }
        // THE CRATE TAKES NO RANGE THAT WRAPS (`FRI-SUN` is its 6 to its 1, and refused), so a
        // range ending on 7 stops at Saturday, and Sunday follows when the step lands on it.
        let sunday = last == 7 && (1..7).contains(&first) && (7 - first) % every == 0;
        let last = if first < 7 { last.min(6) } else { last };
        let step = step.map_or(String::new(), |step| format!("/{step}"));
        named.push(if first == last {
            WEEKDAYS[first].to_string()
        } else {
            format!("{}-{}{step}", WEEKDAYS[first], WEEKDAYS[last])
        });
        if sunday {
            named.push("SUN".to_string());
        }
    }
    Ok(named.join(","))
}

/// Whether `expression` can wake more often than once a minute: a seconds field, which only a 6-
/// or 7-field expression has, that is anything but `0` (#315's floor), read on the normalized
/// form. The callers that store a routine refuse it, outside tests (`OG_ROUTINE_SECOND_CRON`). A
/// five-field one never can; one `normalized_cron` refuses is refused as a cron, not as this.
pub fn under_a_minute(expression: &str) -> bool {
    let normalized = normalized_cron(expression);
    normalized.is_ok_and(|cron| cron.split_whitespace().next() != Some("0"))
}

/// The inverse, as a client reads a stored expression back (NativeChat's `from_server_cron`
/// drops the seconds the same way): `0 0 9 * * MON` reads as `0 9 * * MON`, the same day to
/// either counting. A 6-field expression whose seconds are not `0` (tests scheduling in seconds)
/// is returned as it is — there is no 5-field form for it — and so is one that numbers its days
/// of the week: those are the crate's numbers, and read as 5 fields they would land a day later.
pub fn display_cron(normalized: &str) -> String {
    let fields: Vec<&str> = normalized.split_whitespace().collect();
    match fields[..] {
        ["0", minute, hour, day_of_month, month, days] if !days.split(',').any(numbers_a_day) => {
            format!("{minute} {hour} {day_of_month} {month} {days}")
        }
        _ => normalized.to_string(),
    }
}

/// When a schedule next fires after `after_ms`, in epoch milliseconds, reading the expression in
/// the IANA zone `tz` (`0 9 * * *` in Asia/Manila is 01:00 UTC). `None` for an expression with no
/// future occurrence (a fixed date already past) — and the caller must treat that as "done
/// firing", not as an error — or for a zone that is not one, which `decide` never stores.
pub fn next_fire_ms(expression: &str, tz: &str, after_ms: i64) -> Option<i64> {
    let schedule = cron::Schedule::from_str(&normalized_cron(expression).ok()?).ok()?;
    let after = chrono::DateTime::from_timestamp_millis(after_ms)?.with_timezone(&zone(tz).ok()?);
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
        /// The routine's own limits on every run it starts, under its org's ceiling. Absent on
        /// rows written before limits existed, which set none.
        #[serde(default)]
        run_limits: RunLimits,
        /// The IANA zone its cron is read in (#316). Absent on rows written before routines had
        /// one, whose crons were always read in UTC — so that is what they replay as.
        #[serde(default = "utc")]
        tz: String,
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
        /// The coworker the routine was handed to. `None` keeps the one it had — which is also
        /// what every `schedule-updated` written before the field existed replays as.
        #[serde(default)]
        coworker_id: Option<CoworkerId>,
        /// The routine's limits, replaced whole. `None` keeps the ones it had, as every
        /// `schedule-updated` written before limits existed does.
        #[serde(default)]
        run_limits: Option<RunLimits>,
        /// Its zone, when the edit moved it; `None` keeps it, as every older edit does.
        #[serde(default)]
        tz: Option<String>,
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
    /// A firing that started no run: its coworker answers on the person's own plan, and the
    /// plan could not answer then (#316). Not caught up later; the clock moves on as it would.
    Skipped(Skip),
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
            Self::Skipped(_) => "schedule-skipped",
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
    /// Runs the clock started, by id. Kept rather than inferred as "neither of the above": the
    /// routine's thread takes a person's replies too, and those are no firing at all.
    pub clock_runs: std::collections::BTreeSet<String>,
    /// What every run this routine starts may spend, at most. Its org's ceiling still binds at
    /// run time, so a ceiling lowered after these were saved narrows them.
    pub run_limits: RunLimits,
    /// The IANA zone its cron is read in; `UTC` for a routine from before zones.
    pub tz: String,
    /// Every firing it skipped, oldest first, for its run history.
    pub skipped: Vec<Skip>,
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
    /// The account's own sentence for the same mistake (#322), so a person reads one.
    #[error("{}", crate::account::AccountError::UnknownTimeZone(.0.clone()))]
    UnknownTimeZone(String),
}

#[derive(Debug, Clone)]
pub enum ScheduleCommand {
    Create {
        coworker_id: CoworkerId,
        prompt: String,
        name: String,
        wake: Wake,
        /// Already held to the server's budget and the org's ceiling by the caller, which can
        /// read both; the aggregate can read neither.
        run_limits: RunLimits,
        /// The IANA zone its cron is read in.
        tz: String,
        at_ms: i64,
    },
    Update {
        name: String,
        prompt: String,
        wake: Wake,
        /// `None` keeps the coworker it has.
        coworker_id: Option<CoworkerId>,
        /// `None` keeps the limits it has; checked by the caller as `Create`'s are.
        run_limits: Option<RunLimits>,
        /// `None` keeps the zone it has.
        tz: Option<String>,
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
    /// Record a firing that will start no run, under the same rules a firing is held to.
    Skip(Skip),
}

/// A wake as an event stores it: kind, normalized cron, hook id, hash, key.
type Stored = (WakeKind, String, String, String, String);

impl ScheduleCommand {
    /// What a firing writes: its `Fire`, or the `Skip` a plan with nobody to answer it gave the
    /// code and the words for (opengrok-server `autonomy::unreachable`).
    pub fn firing(
        skip: Option<(&str, &str)>,
        cause: FireCause,
        run_id: &RunId,
        at_ms: i64,
    ) -> Self {
        match skip {
            Some((code, _)) => Self::Skip(Skip {
                cause,
                code: code.to_string(),
                at_ms,
            }),
            None => Self::Fire {
                run_id: run_id.clone(),
                cause,
                at_ms,
            },
        }
    }
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
                run_limits,
                tz,
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
                self.run_limits = *run_limits;
                self.tz = tz.clone();
            }
            ScheduleEvent::Updated {
                name,
                cron,
                prompt,
                kind,
                hook_id,
                secret_hash,
                webhook_key,
                coworker_id,
                run_limits,
                tz,
                ..
            } => {
                self.name = name.clone();
                if let Some(coworker_id) = coworker_id {
                    self.coworker_id = Some(coworker_id.clone());
                }
                if let Some(run_limits) = run_limits {
                    self.run_limits = *run_limits;
                }
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
                if let Some(tz) = tz {
                    self.tz = tz.clone();
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
                } else {
                    self.clock_runs.insert(run_id.as_str().to_string());
                }
            }
            ScheduleEvent::Skipped(skip) => self.skipped.push(skip.clone()),
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

    /// The wake as an event stores it, or why it cannot be stored. ACCEPTED MUST MEAN "WILL
    /// FIRE": an unparseable expression, or one with no future occurrence in `tz` at all, is
    /// refused here rather than stored as a dead row. The zone is asked first, so an unknown one
    /// is not reported as a bad cron.
    fn checked(wake: Wake, tz: &str, at_ms: i64) -> Result<Stored, ScheduleError> {
        zone(tz)?;
        match wake {
            Wake::Cron { cron } => {
                let cron = normalized_cron(&cron)?;
                if next_fire_ms(&cron, tz, at_ms).is_none() {
                    return Err(ScheduleError::BadCron(cron));
                }
                let none = String::new;
                Ok((WakeKind::Cron, cron, none(), none(), none()))
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
                let (hook_id, hash) = (hook_id.trim().to_string(), secret_hash.trim().to_string());
                Ok((WakeKind::Webhook, String::new(), hook_id, hash, webhook_key))
            }
        }
    }

    /// Whether a firing for `cause` may be recorded, run or skipped. A paused schedule refusing
    /// to fire is the whole point of pause. The sweep should never ask (paused rows are not
    /// claimed), so this firing twice as a guard is deliberate: the projection being wrong must
    /// not be enough to fire a run. A person's "run now" is the one exception: they asked, paused
    /// or not. An inbound webhook is not that exception — a paused routine must not run because a
    /// todo app POSTed.
    fn may_fire(&self, cause: FireCause) -> Result<(), ScheduleError> {
        self.alive()?;
        if self.paused && cause != FireCause::Manual {
            return Err(ScheduleError::Paused);
        }
        Ok(())
    }

    pub fn decide(&self, command: ScheduleCommand) -> Result<Vec<ScheduleEvent>, ScheduleError> {
        let has_prompt = |prompt: &str| {
            (!prompt.trim().is_empty())
                .then_some(())
                .ok_or(ScheduleError::EmptyPrompt)
        };
        match command {
            ScheduleCommand::Create {
                coworker_id,
                prompt,
                name,
                wake,
                run_limits,
                tz,
                at_ms,
            } => {
                has_prompt(&prompt)?;
                let (kind, cron, hook_id, secret_hash, webhook_key) =
                    Self::checked(wake, &tz, at_ms)?;
                Ok(vec![ScheduleEvent::Created {
                    coworker_id,
                    cron,
                    prompt,
                    name: name.trim().to_string(),
                    kind,
                    hook_id,
                    secret_hash,
                    webhook_key,
                    run_limits,
                    tz,
                    at_ms,
                }])
            }

            ScheduleCommand::Update {
                name,
                prompt,
                wake,
                coworker_id,
                run_limits,
                tz,
                at_ms,
            } => {
                self.alive()?;
                has_prompt(&prompt)?;
                let (kind, cron, hook_id, secret_hash, webhook_key) =
                    Self::checked(wake, tz.as_deref().unwrap_or(&self.tz), at_ms)?;
                Ok(vec![ScheduleEvent::Updated {
                    name: name.trim().to_string(),
                    cron,
                    prompt,
                    at_ms,
                    kind: Some(kind),
                    hook_id: Some(hook_id),
                    secret_hash: Some(secret_hash),
                    webhook_key: Some(webhook_key),
                    coworker_id,
                    run_limits,
                    tz,
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
                self.may_fire(cause)?;
                Ok(vec![ScheduleEvent::Fired {
                    run_id,
                    manual: cause == FireCause::Manual,
                    webhook: cause == FireCause::Webhook,
                    at_ms,
                }])
            }

            ScheduleCommand::Skip(skip) => {
                self.may_fire(skip.cause)?;
                Ok(vec![ScheduleEvent::Skipped(skip)])
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
    /// The routine's own limits, projected so a listing need not replay a stream that grows by
    /// one `Fired` per firing. Empty on rows projected before the column existed, as the
    /// routines behind them are: none could set limits then.
    #[serde(default)]
    pub run_limits: RunLimits,
    /// The IANA zone its cron is read in; `UTC` on rows projected before zones.
    #[serde(default = "utc")]
    pub tz: String,
    /// The firing it skipped last, so a listing can say so without replaying the stream.
    #[serde(default)]
    pub last_skip: Option<Skip>,
}

#[cfg(test)]
#[path = "../tests/unit/schedule.rs"]
mod tests;
