//! Store operations for schedules and monitors — the autonomy slice.
//!
//! Same contract as everything in `postgres.rs`: the events stream is the truth, the view is a
//! projection written in the same transaction, and `expected_seq` is the concurrency check.
//!
//! CLAIMING ADVANCES THE CLOCK BEFORE THE WORK HAPPENS. `claim_due_schedules` moves `next_due_ms`
//! to the following occurrence *inside the claiming update's transaction*. A process that dies
//! between claiming and firing therefore skips one occurrence — the same bargain the run lease
//! makes, and the right one: a schedule that might double-fire is worse than one that might skip,
//! because firing runs commands and skipping runs nothing.

use opengrok_core::id::{AccountId, CoworkerId, MonitorId, RunId, ScheduleId};
use opengrok_core::monitor::{Monitor, MonitorEvent, MonitorView};
use opengrok_core::schedule::{Schedule, ScheduleEvent, ScheduleView, next_fire_ms};
use sqlx::Row;

use crate::postgres::PgStore;
use crate::{StoreError, StoreResult, monitor_stream, schedule_stream};

/// A schedule the sweep has claimed and must now fire.
#[derive(Debug, Clone)]
pub struct DueSchedule {
    pub id: ScheduleId,
    pub account_id: AccountId,
    pub coworker_id: CoworkerId,
    pub prompt: String,
    pub name: String,
}

/// One row of the event log, as the monitor sweep reads it.
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub stream_id: String,
    pub event_type: String,
}

impl PgStore {
    pub async fn load_schedule(&self, id: &ScheduleId) -> StoreResult<(Schedule, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(schedule_stream(id))
        .fetch_all(self.pool())
        .await?;

        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: ScheduleEvent = serde_json::from_value(payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            events.push(event);
        }
        Ok((Schedule::replay(&events), seq))
    }

    pub async fn append_schedule(
        &self,
        id: &ScheduleId,
        account_id: &AccountId,
        expected_seq: i64,
        events: &[ScheduleEvent],
        state: &Schedule,
        at_ms: i64,
    ) -> StoreResult<i64> {
        let mut tx = self.pool().begin().await?;
        let stream = schedule_stream(id);
        let mut seq = expected_seq;

        for event in events {
            seq += 1;
            let payload = serde_json::to_value(event)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            sqlx::query(
                "insert into events (stream_id, stream_seq, event_type, payload)
                 values ($1, $2, $3, $4)",
            )
            .bind(&stream)
            .bind(seq)
            .bind(event.event_type())
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        }

        if state.deleted {
            // The log keeps the history; the view only lists what still exists.
            sqlx::query("delete from schedule_view where id = $1")
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?;
        } else {
            let active = state.created && !state.paused;
            // Recomputed from *now* on every append. Resuming therefore skips everything missed
            // while paused rather than backfilling it — pause means "do not act", not "act later".
            let next_due_ms = if active {
                next_fire_ms(&state.cron, at_ms)
            } else {
                None
            };
            let coworker = state
                .coworker_id
                .as_ref()
                .map(|c| c.as_str().to_string())
                .unwrap_or_default();
            // A firing in this batch stamps last_fired_ms; anything else leaves it alone.
            let fired_at = events.iter().find_map(|event| match event {
                ScheduleEvent::Fired { at_ms, .. } => Some(*at_ms),
                _ => None,
            });
            sqlx::query(
                "insert into schedule_view
                   (id, account_id, coworker_id, cron, prompt, name, active, next_due_ms,
                    updated_at_ms, created_at_ms, last_fired_ms)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, $10)
                 on conflict (id) do update set
                   cron = excluded.cron,
                   prompt = excluded.prompt,
                   name = excluded.name,
                   active = excluded.active,
                   next_due_ms = excluded.next_due_ms,
                   updated_at_ms = excluded.updated_at_ms,
                   created_at_ms = coalesce(schedule_view.created_at_ms, excluded.created_at_ms),
                   last_fired_ms = coalesce(excluded.last_fired_ms, schedule_view.last_fired_ms)",
            )
            .bind(id.as_str())
            .bind(account_id.as_str())
            .bind(&coworker)
            .bind(&state.cron)
            .bind(&state.prompt)
            .bind(&state.name)
            .bind(active)
            .bind(next_due_ms)
            .bind(at_ms)
            .bind(fired_at)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(seq)
    }

    pub async fn schedules_for(&self, account_id: &AccountId) -> StoreResult<Vec<ScheduleView>> {
        let rows = sqlx::query(
            "select id, coworker_id, cron, prompt, name, active, next_due_ms, updated_at_ms,
                    created_at_ms, last_fired_ms
             from schedule_view where account_id = $1 order by updated_at_ms desc",
        )
        .bind(account_id.as_str())
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|row| {
                let updated_at_ms: i64 = row.try_get("updated_at_ms")?;
                Ok(ScheduleView {
                    id: row.try_get("id")?,
                    coworker_id: CoworkerId::from_stored(row.try_get::<String, _>("coworker_id")?),
                    cron: row.try_get("cron")?,
                    prompt: row.try_get("prompt")?,
                    name: row.try_get("name")?,
                    active: row.try_get("active")?,
                    next_due_ms: row.try_get("next_due_ms")?,
                    // Rows from before the column existed: the best "created" we have is the
                    // last update.
                    created_at_ms: row
                        .try_get::<Option<i64>, _>("created_at_ms")?
                        .unwrap_or(updated_at_ms),
                    last_fired_ms: row.try_get("last_fired_ms")?,
                })
            })
            .collect()
    }

    /// Whose schedule this is — for the ownership check that answers 404 either way.
    pub async fn schedule_owner(&self, id: &ScheduleId) -> StoreResult<Option<AccountId>> {
        let row = sqlx::query("select account_id from schedule_view where id = $1")
            .bind(id.as_str())
            .fetch_optional(self.pool())
            .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("account_id"))
            .transpose()?
            .map(AccountId::from_stored))
    }

    /// Claim every schedule that is due, advancing each one's clock in the same transaction.
    ///
    /// `for update skip locked` is what lets two replicas sweep at once without either blocking:
    /// each claims a disjoint set, and a row one replica holds is invisible to the other.
    pub async fn claim_due_schedules(
        &self,
        now_ms: i64,
        limit: i64,
    ) -> StoreResult<Vec<DueSchedule>> {
        let mut tx = self.pool().begin().await?;

        let rows = sqlx::query(
            "select id, account_id, coworker_id, cron, prompt, name from schedule_view
             where active and next_due_ms is not null and next_due_ms <= $1
             order by next_due_ms limit $2
             for update skip locked",
        )
        .bind(now_ms)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;

        let mut claimed = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let cron: String = row.try_get("cron")?;
            // The next occurrence after NOW, not after the missed slot — a schedule that was due
            // an hour ago fires once, not sixty times.
            let next = next_fire_ms(&cron, now_ms);
            sqlx::query("update schedule_view set next_due_ms = $2 where id = $1")
                .bind(&id)
                .bind(next)
                .execute(&mut *tx)
                .await?;

            claimed.push(DueSchedule {
                id: ScheduleId::from_stored(id),
                account_id: AccountId::from_stored(row.try_get::<String, _>("account_id")?),
                coworker_id: CoworkerId::from_stored(row.try_get::<String, _>("coworker_id")?),
                prompt: row.try_get("prompt")?,
                name: row.try_get("name")?,
            });
        }

        tx.commit().await?;
        Ok(claimed)
    }

    pub async fn load_monitor(&self, id: &MonitorId) -> StoreResult<(Monitor, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(monitor_stream(id))
        .fetch_all(self.pool())
        .await?;

        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: MonitorEvent = serde_json::from_value(payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            events.push(event);
        }
        Ok((Monitor::replay(&events), seq))
    }

    pub async fn append_monitor(
        &self,
        id: &MonitorId,
        account_id: &AccountId,
        expected_seq: i64,
        events: &[MonitorEvent],
        state: &Monitor,
        at_ms: i64,
    ) -> StoreResult<i64> {
        let mut tx = self.pool().begin().await?;
        let stream = monitor_stream(id);
        let mut seq = expected_seq;

        for event in events {
            seq += 1;
            let payload = serde_json::to_value(event)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            sqlx::query(
                "insert into events (stream_id, stream_seq, event_type, payload)
                 values ($1, $2, $3, $4)",
            )
            .bind(&stream)
            .bind(seq)
            .bind(event.event_type())
            .bind(payload)
            .execute(&mut *tx)
            .await?;

            // The loop guard's memory, written with the event that creates the fact. If the
            // firing is recorded at all, the exclusion exists — there is no window in which a
            // monitor could see its own run.
            if let MonitorEvent::Fired { run_id, .. } = event {
                sqlx::query(
                    "insert into monitor_firing (monitor_id, run_id) values ($1, $2)
                     on conflict do nothing",
                )
                .bind(id.as_str())
                .bind(run_id.as_str())
                .execute(&mut *tx)
                .await?;
            }
        }

        if state.deleted {
            sqlx::query("delete from monitor_view where id = $1")
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?;
        } else {
            let active = state.created && !state.paused;
            let coworker = state
                .coworker_id
                .as_ref()
                .map(|c| c.as_str().to_string())
                .unwrap_or_default();
            sqlx::query(
                "insert into monitor_view
                   (id, account_id, coworker_id, watches, prompt, active, updated_at_ms)
                 values ($1, $2, $3, $4, $5, $6, $7)
                 on conflict (id) do update set
                   watches = excluded.watches,
                   prompt = excluded.prompt,
                   active = excluded.active,
                   updated_at_ms = excluded.updated_at_ms",
            )
            .bind(id.as_str())
            .bind(account_id.as_str())
            .bind(&coworker)
            .bind(&state.watches)
            .bind(&state.prompt)
            .bind(active)
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(seq)
    }

    pub async fn monitors_for(&self, account_id: &AccountId) -> StoreResult<Vec<MonitorView>> {
        let rows = sqlx::query(
            "select id, coworker_id, watches, prompt, active
             from monitor_view where account_id = $1 order by updated_at_ms desc",
        )
        .bind(account_id.as_str())
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(MonitorView {
                    id: row.try_get("id")?,
                    coworker_id: CoworkerId::from_stored(row.try_get::<String, _>("coworker_id")?),
                    watches: row.try_get("watches")?,
                    prompt: row.try_get("prompt")?,
                    active: row.try_get("active")?,
                })
            })
            .collect()
    }

    pub async fn monitor_owner(&self, id: &MonitorId) -> StoreResult<Option<AccountId>> {
        let row = sqlx::query("select account_id from monitor_view where id = $1")
            .bind(id.as_str())
            .fetch_optional(self.pool())
            .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("account_id"))
            .transpose()?
            .map(AccountId::from_stored))
    }

    /// Every active monitor, for the sweep to match events against.
    pub async fn active_monitors(
        &self,
    ) -> StoreResult<Vec<(MonitorId, AccountId, CoworkerId, String, String)>> {
        let rows = sqlx::query(
            "select id, account_id, coworker_id, watches, prompt from monitor_view where active",
        )
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok((
                    MonitorId::from_stored(row.try_get::<String, _>("id")?),
                    AccountId::from_stored(row.try_get::<String, _>("account_id")?),
                    CoworkerId::from_stored(row.try_get::<String, _>("coworker_id")?),
                    row.try_get("watches")?,
                    row.try_get("prompt")?,
                ))
            })
            .collect()
    }

    /// Read the next span of the event log and advance the cursor past it, atomically.
    ///
    /// SEEDED AT THE LOG'S END, NOT ITS BEGINNING. The first call on a fresh deployment writes the
    /// cursor at `max(events.id)` and returns nothing: a monitor reacts to what happens after it
    /// exists, and replaying months of history into a brand-new monitor would fire a run per
    /// ancient event.
    ///
    /// The cursor moves in the same transaction that reads, under a row lock — so a crash after
    /// commit skips the span (the schedule bargain again), and two replicas can never process the
    /// same events twice.
    pub async fn next_log_span(&self, batch: i64) -> StoreResult<Vec<LogEvent>> {
        let mut tx = self.pool().begin().await?;

        sqlx::query(
            "insert into monitor_cursor (id, last_event_id)
             select 1, coalesce(max(id), 0) from events
             on conflict (id) do nothing",
        )
        .execute(&mut *tx)
        .await?;

        let cursor: i64 =
            sqlx::query("select last_event_id from monitor_cursor where id = 1 for update")
                .fetch_one(&mut *tx)
                .await?
                .try_get("last_event_id")?;

        let rows = sqlx::query(
            "select id, stream_id, event_type from events where id > $1 order by id limit $2",
        )
        .bind(cursor)
        .bind(batch)
        .fetch_all(&mut *tx)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        let mut last = cursor;
        for row in rows {
            last = row.try_get("id")?;
            events.push(LogEvent {
                stream_id: row.try_get("stream_id")?,
                event_type: row.try_get("event_type")?,
            });
        }

        if last != cursor {
            sqlx::query("update monitor_cursor set last_event_id = $1 where id = 1")
                .bind(last)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(events)
    }

    /// How many runs are live right now — the gateway's `/health` reports it as `isBusy`.
    pub async fn running_runs(&self) -> StoreResult<i64> {
        let row = sqlx::query("select count(*) as n from run_view where status = 'running'")
            .fetch_one(self.pool())
            .await?;
        Ok(row.try_get("n")?)
    }

    /// The loop guard's question: did this monitor start that run?
    pub async fn was_fired_by(&self, monitor: &MonitorId, run: &RunId) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 as one from monitor_firing where monitor_id = $1 and run_id = $2",
        )
        .bind(monitor.as_str())
        .bind(run.as_str())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.is_some())
    }
}
