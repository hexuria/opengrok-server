//! The Postgres event store.
//!
//! Async, unlike the `EventStore` trait, which is deliberately sync so the pure in-memory store
//! needs no runtime. Rather than making the trait async (and infecting every caller with a boxed
//! future for the sake of a HashMap), the async surface is stated plainly here and the service
//! layer takes whichever it was handed.
//!
//! APPEND AND PROJECT IN ONE TRANSACTION. If the log and the view can drift, a caller can be told
//! "signed in" and then read a projection that has never heard of them. They commit together or
//! neither commits.

use opengrok_core::account::{Account, AccountEvent, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{Run, RunEvent, RunStatus, RunView};
use sqlx::{PgPool, Row};

use crate::{StoreError, StoreResult, account_stream};

#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Replay an account's log. Returns the state and the sequence it was read at.
    pub async fn load_account(&self, id: &AccountId) -> StoreResult<(Account, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(account_stream(id))
        .fetch_all(&self.pool)
        .await?;

        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: AccountEvent = serde_json::from_value(payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            events.push(event);
        }
        Ok((Account::replay(&events), seq))
    }

    /// Append events and refresh the projection, atomically.
    ///
    /// `expected_seq` must be what `load_account` returned. A concurrent writer that appended in
    /// between makes the unique index fire, which surfaces as `StoreError::Conflict`.
    pub async fn append_account(
        &self,
        id: &AccountId,
        expected_seq: i64,
        events: &[AccountEvent],
        view: &AccountView,
    ) -> StoreResult<i64> {
        let mut tx = self.pool.begin().await?;
        let stream = account_stream(id);
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

        // Maintain the session index from the same events, in the same transaction. A refresh that
        // rotated the token must make the OLD hash unfindable at the instant the new one appears —
        // if these could drift, a rotated token would stay usable for the width of the gap.
        for event in events {
            match event {
                AccountEvent::SessionIssued {
                    session_id,
                    refresh_token_hash,
                    ..
                } => {
                    sqlx::query(
                        "insert into session_view (refresh_token_hash, account_id, session_id)
                         values ($1, $2, $3)
                         on conflict (refresh_token_hash) do nothing",
                    )
                    .bind(refresh_token_hash)
                    .bind(id.as_str())
                    .bind(session_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                }
                AccountEvent::SessionRefreshed {
                    session_id,
                    refresh_token_hash,
                    ..
                } => {
                    sqlx::query("delete from session_view where session_id = $1")
                        .bind(session_id.as_str())
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query(
                        "insert into session_view (refresh_token_hash, account_id, session_id)
                         values ($1, $2, $3)",
                    )
                    .bind(refresh_token_hash)
                    .bind(id.as_str())
                    .bind(session_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                }
                AccountEvent::SessionRevoked { session_id, .. } => {
                    sqlx::query("delete from session_view where session_id = $1")
                        .bind(session_id.as_str())
                        .execute(&mut *tx)
                        .await?;
                }
                AccountEvent::Registered { .. } | AccountEvent::PlanChanged { .. } => {}
            }
        }

        sqlx::query(
            "insert into account_view (id, email, plan, trial, updated_at_ms)
             values ($1, $2, $3, $4, $5)
             on conflict (id) do update set
               email = excluded.email,
               plan = excluded.plan,
               trial = excluded.trial,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(view.id.as_str())
        .bind(&view.email)
        .bind(view.plan.as_wire())
        .bind(view.trial)
        .bind(view.updated_at_ms)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// Which account holds this refresh token, if any. The read side of the rotation check.
    pub async fn account_by_refresh_hash(&self, hash: &str) -> StoreResult<Option<AccountId>> {
        let row = sqlx::query("select account_id from session_view where refresh_token_hash = $1")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok(AccountId::from_stored(
                row.try_get::<String, _>("account_id")?,
            ))
        })
        .transpose()
    }

    /// The read side: answered from the projection, never by replaying a log.
    pub async fn account_by_email(&self, email: &str) -> StoreResult<Option<AccountView>> {
        let row = sqlx::query(
            "select id, email, plan, trial, updated_at_ms from account_view where email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            Ok(AccountView {
                id: AccountId::from_stored(row.try_get::<String, _>("id")?),
                email: row.try_get("email")?,
                plan: Plan::from_wire(&row.try_get::<String, _>("plan")?),
                trial: row.try_get("trial")?,
                updated_at_ms: row.try_get("updated_at_ms")?,
            })
        })
        .transpose()
    }
}

/// Runs: the durable half of the promise that work survives a client.
impl PgStore {
    /// Replay a run's log.
    pub async fn load_run(&self, id: &RunId) -> StoreResult<(Run, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(crate::run_stream(id))
        .fetch_all(&self.pool)
        .await?;

        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: RunEvent = serde_json::from_value(payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            events.push(event);
        }
        Ok((Run::replay(&events), seq))
    }

    /// Append run events and refresh the projection, atomically.
    ///
    /// The ordering that matters is at the CALLER: an event is appended here *before* it is
    /// written to the client's socket. A client that received a frame we never stored would be
    /// showing work that a reconnect cannot reproduce.
    pub async fn append_run(
        &self,
        id: &RunId,
        expected_seq: i64,
        events: &[RunEvent],
        view: &RunView,
    ) -> StoreResult<i64> {
        let mut tx = self.pool.begin().await?;
        let stream = crate::run_stream(id);
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

        sqlx::query(
            "insert into run_view (id, thread_id, status, event_count, updated_at_ms)
             values ($1, $2, $3, $4, $5)
             on conflict (id) do update set
               thread_id = excluded.thread_id,
               status = excluded.status,
               event_count = excluded.event_count,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(view.id.as_str())
        .bind(&view.thread_id)
        .bind(match view.status {
            RunStatus::Running => "running",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
        })
        .bind(view.event_count)
        .bind(view.updated_at_ms)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// Runs left `running` by a restart. Nothing consumes this yet; it is the query that makes an
    /// interrupted run findable rather than merely absent, and slice 5's resumption starts here.
    pub async fn interrupted_runs(&self, limit: i64) -> StoreResult<Vec<RunId>> {
        let rows = sqlx::query(
            "select id from run_view where status = 'running' order by updated_at_ms limit $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok(RunId::from_stored(row.try_get::<String, _>("id")?)))
            .collect()
    }
}
