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

use std::collections::HashSet;

use crate::vault::{Sealed, Vault};
use opengrok_core::account::{Account, AccountEvent, AccountView, Plan};
use opengrok_core::connection::{Connection, ConnectionEvent, ConnectionView, Owner};
use opengrok_core::coworker::{Coworker, CoworkerEvent, CoworkerView};
use opengrok_core::id::{AccountId, BoxId, CoworkerId, RunId};
use opengrok_core::run::{Run, RunEvent, RunStatus, RunView};
use sqlx::{PgPool, Row};

use crate::{StoreError, StoreResult, account_stream};

#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

/// One run as a routine's history lists it: the stored status word (`running`,
/// `awaiting-approval`, `finished`, `failed`), when it began, when it last moved.
#[derive(Debug, Clone)]
pub struct ThreadRun {
    pub id: RunId,
    pub status: String,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One `run_view` row as a `ThreadRun`, shared by the two readers of a thread's history so they
/// cannot drift in what they make of a row. `started_at_ms` is absent on runs projected before
/// that column existed, and the last time the run moved is the closest honest answer for those.
fn thread_run_from_row(row: sqlx::postgres::PgRow) -> StoreResult<ThreadRun> {
    let updated_at_ms: i64 = row.try_get("updated_at_ms")?;
    Ok(ThreadRun {
        id: RunId::from_stored(row.try_get::<String, _>("id")?),
        status: row.try_get("status")?,
        started_at_ms: row
            .try_get::<Option<i64>, _>("started_at_ms")?
            .unwrap_or(updated_at_ms),
        updated_at_ms,
    })
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

        // Maintain the session index from the same events, in the same transaction. The current
        // hash is always findable. The just-rotated-away hash stays findable until
        // `grace_until_ms` (REFRESH_GRACE_MS after the rotation) so a concurrent refresh with
        // the old cookie can load the account; decide() still refuses it once the window has
        // passed, and the next rotate drops hashes that are neither current nor previous.
        for event in events {
            match event {
                AccountEvent::SessionIssued {
                    session_id,
                    refresh_token_hash,
                    ..
                } => {
                    sqlx::query(
                        "insert into session_view (refresh_token_hash, account_id, session_id, grace_until_ms)
                         values ($1, $2, $3, null)
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
                    previous_refresh_token_hash,
                    at_ms,
                } => {
                    sqlx::query(
                        "insert into session_view (refresh_token_hash, account_id, session_id, grace_until_ms)
                         values ($1, $2, $3, null)
                         on conflict (refresh_token_hash) do update set
                           account_id = excluded.account_id,
                           session_id = excluded.session_id,
                           grace_until_ms = null",
                    )
                    .bind(refresh_token_hash)
                    .bind(id.as_str())
                    .bind(session_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                    if let Some(previous) = previous_refresh_token_hash {
                        let grace_until = *at_ms + opengrok_core::REFRESH_GRACE_MS;
                        sqlx::query(
                            "insert into session_view (refresh_token_hash, account_id, session_id, grace_until_ms)
                             values ($1, $2, $3, $4)
                             on conflict (refresh_token_hash) do update set
                               account_id = excluded.account_id,
                               session_id = excluded.session_id,
                               grace_until_ms = excluded.grace_until_ms",
                        )
                        .bind(previous)
                        .bind(id.as_str())
                        .bind(session_id.as_str())
                        .bind(grace_until)
                        .execute(&mut *tx)
                        .await?;
                        sqlx::query(
                            "delete from session_view
                              where session_id = $1
                                and refresh_token_hash <> $2
                                and refresh_token_hash <> $3",
                        )
                        .bind(session_id.as_str())
                        .bind(refresh_token_hash)
                        .bind(previous)
                        .execute(&mut *tx)
                        .await?;
                    } else {
                        sqlx::query(
                            "delete from session_view
                              where session_id = $1
                                and refresh_token_hash <> $2",
                        )
                        .bind(session_id.as_str())
                        .bind(refresh_token_hash)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
                AccountEvent::SessionRevoked { session_id, .. } => {
                    sqlx::query("delete from session_view where session_id = $1")
                        .bind(session_id.as_str())
                        .execute(&mut *tx)
                        .await?;
                }
                AccountEvent::Registered { .. }
                | AccountEvent::PlanChanged { .. }
                | AccountEvent::CredentialsSet { .. }
                | AccountEvent::EmailVerified { .. }
                | AccountEvent::Enabled { .. }
                | AccountEvent::Disabled { .. }
                | AccountEvent::ProfileUpdated { .. }
                | AccountEvent::PasswordChanged { .. } => {}
            }
        }

        sqlx::query(
            "insert into account_view
               (id, email, plan, trial, updated_at_ms,
                password_hash, first_name, last_name, org_id, verified, enabled, avatar_url)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             on conflict (id) do update set
               email = excluded.email,
               plan = excluded.plan,
               trial = excluded.trial,
               updated_at_ms = excluded.updated_at_ms,
               password_hash = coalesce(excluded.password_hash, account_view.password_hash),
               first_name = excluded.first_name,
               last_name = excluded.last_name,
               org_id = coalesce(excluded.org_id, account_view.org_id),
               verified = excluded.verified or account_view.verified,
               enabled = excluded.enabled",
        )
        .bind(view.id.as_str())
        .bind(&view.email)
        .bind(view.plan.as_wire())
        .bind(view.trial)
        .bind(view.updated_at_ms)
        .bind(&view.password_hash)
        .bind(&view.first_name)
        .bind(&view.last_name)
        .bind(&view.org_id)
        .bind(view.verified)
        .bind(view.enabled)
        .bind(&view.avatar_url)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// Which account holds this refresh token, if any. The current hash, or the just-rotated-away
    /// hash whose `grace_until_ms` has not passed. After grace the previous hash is invisible here
    /// even if the row has not been swept yet.
    pub async fn account_by_refresh_hash(
        &self,
        hash: &str,
        at_ms: i64,
    ) -> StoreResult<Option<AccountId>> {
        let row = sqlx::query(
            "select account_id from session_view
              where refresh_token_hash = $1
                and (grace_until_ms is null or grace_until_ms >= $2)",
        )
        .bind(hash)
        .bind(at_ms)
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
            "select id, email, plan, trial, updated_at_ms, password_hash, first_name, last_name,\n                    org_id, verified, enabled, avatar_url\n             from account_view where email = $1",
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
                password_hash: row.try_get("password_hash")?,
                first_name: row.try_get("first_name")?,
                last_name: row.try_get("last_name")?,
                org_id: row.try_get("org_id")?,
                verified: row.try_get("verified")?,
                enabled: row.try_get("enabled")?,
                avatar_url: row.try_get("avatar_url")?,
            })
        })
        .transpose()
    }

    /// Every account belonging to an org — the admin's user list. Reads the projection, so it
    /// captures CLI-created accounts, signups and the admin alike, not only those who redeemed an
    /// invite. Ordered by email for a stable display.
    pub async fn accounts_by_org(&self, org_id: &str) -> StoreResult<Vec<AccountView>> {
        let rows = sqlx::query(
            "select id, email, plan, trial, updated_at_ms, password_hash, first_name, last_name,
                    org_id, verified, enabled, avatar_url
             from account_view where org_id = $1 order by email",
        )
        .bind(org_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(AccountView {
                    id: AccountId::from_stored(row.try_get::<String, _>("id")?),
                    email: row.try_get("email")?,
                    plan: Plan::from_wire(&row.try_get::<String, _>("plan")?),
                    trial: row.try_get("trial")?,
                    updated_at_ms: row.try_get("updated_at_ms")?,
                    password_hash: row.try_get("password_hash")?,
                    first_name: row.try_get("first_name")?,
                    last_name: row.try_get("last_name")?,
                    org_id: row.try_get("org_id")?,
                    verified: row.try_get("verified")?,
                    enabled: row.try_get("enabled")?,
                    avatar_url: row.try_get("avatar_url")?,
                })
            })
            .collect()
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
        // Whose run this is. `None` means nobody may read it back — an unowned run is not a
        // public one.
        account_id: Option<&AccountId>,
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
            "insert into run_view
               (id, thread_id, status, event_count, updated_at_ms, account_id, started_at_ms)
             values ($1, $2, $3, $4, $5, $6, $5)
             on conflict (id) do update set
               thread_id = excluded.thread_id,
               status = excluded.status,
               event_count = excluded.event_count,
               updated_at_ms = excluded.updated_at_ms,
               -- The owner is set once, by the first batch that names one, and never changed: a
               -- later batch without a session must not orphan the run, and a later batch from
               -- somebody else must not take it (a POST used to, with nothing but the run id).
               account_id = coalesce(run_view.account_id, excluded.account_id),
               -- The start is the first append's stamp, kept for good.
               started_at_ms = coalesce(run_view.started_at_ms, excluded.started_at_ms)",
        )
        .bind(view.id.as_str())
        .bind(&view.thread_id)
        .bind(view.status.as_str())
        .bind(view.event_count)
        .bind(view.updated_at_ms)
        .bind(account_id.map(|id| id.as_str()))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// What the projection says this run's status is, without replaying its log.
    ///
    /// A primary-key lookup rather than `load_run`, because this is asked at every step boundary of
    /// a running turn to find out whether somebody has stopped it: replaying the whole event stream
    /// to read one word would grow with the length of the conversation and be paid for on the hot
    /// path. `None` means the projection has never heard of this run.
    /// A run's status and how many frames it has emitted: one primary-key read, for a reader
    /// that follows a run and wants to reload it only when something changed.
    pub async fn run_progress(&self, id: &RunId) -> StoreResult<Option<(RunStatus, i64)>> {
        let row = sqlx::query("select status, event_count from run_view where id = $1")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some((
                RunStatus::from_stored(&row.try_get::<String, _>("status")?),
                row.try_get::<i64, _>("event_count")?,
            ))),
            None => Ok(None),
        }
    }

    pub async fn run_status(&self, id: &RunId) -> StoreResult<Option<RunStatus>> {
        let row = sqlx::query("select status from run_view where id = $1")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some(RunStatus::from_stored(
                &row.try_get::<String, _>("status")?,
            ))),
            None => Ok(None),
        }
    }

    /// Is this run readable by this account?
    ///
    /// LAYER 4 (`docs/PLAN.md` §4.5): whose records may this call touch. A run holds a whole
    /// conversation, so "anyone with the id may read it" would make a run id a password — and run
    /// ids appear in client URLs and logs. An unowned run is readable by nobody: `NULL` here means
    /// "no session started it", which must not read as "everybody's".
    /// The runs journaled under one thread, newest first — a routine's run history for the
    /// desktop's pane (every firing of one schedule shares the schedule's id as its thread).
    pub async fn runs_for_thread(
        &self,
        thread_id: &str,
        limit: i64,
    ) -> StoreResult<Vec<ThreadRun>> {
        let rows = sqlx::query(
            "select id, status, started_at_ms, updated_at_ms from run_view
             where thread_id = $1 order by updated_at_ms desc limit $2",
        )
        .bind(thread_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(thread_run_from_row).collect()
    }

    /// How many of this thread's runs have not ended.
    ///
    /// The webhook door caps firings on this: whoever holds a hook's key can press it as fast as
    /// they like, and every press would otherwise open a run that is billed and holds a recovery
    /// lease. Counted in the database rather than from `runs_for_thread`, because that reader is
    /// bounded by a limit and orders by when a run last MOVED — a run waiting days on a card
    /// would fall off the end of the page and out of the count.
    ///
    /// The terminal words come from `RunStatus` rather than being spelled here, so a sixth status
    /// cannot be invented in one place and forgotten in this query.
    pub async fn unfinished_runs_in_thread(&self, thread_id: &str) -> StoreResult<i64> {
        let ended: Vec<&str> = [RunStatus::Finished, RunStatus::Failed, RunStatus::Stopped]
            .iter()
            .map(RunStatus::as_str)
            .collect();
        let row = sqlx::query(
            "select count(*) as unfinished from run_view
             where thread_id = $1 and not (status = any($2))",
        )
        .bind(thread_id)
        .bind(&ended)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("unfinished")?)
    }

    /// Hide a run from every client of the account that owns it.
    ///
    /// Nothing is destroyed: the run, its frames and the coworker's memory of the turn are
    /// untouched. What changes is that no thread offers it again. Answers whether it was this
    /// account's run to hide, so a run belonging to somebody else reads as no such run.
    pub async fn hide_run(
        &self,
        run_id: &RunId,
        account: &AccountId,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let hidden = sqlx::query(
            "update run_view set hidden_at_ms = coalesce(hidden_at_ms, $3)
             where id = $1 and account_id = $2",
        )
        .bind(run_id.as_str())
        .bind(account.as_str())
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(hidden.rows_affected() > 0)
    }

    /// The runs of a thread this account has hidden, so a client can put the same turns out of
    /// sight in its own cache rather than painting what another machine deleted.
    ///
    /// Not rationed by the page size the turns themselves use: a client is holding its own copy
    /// of the whole thread, and a name it is not told stays on its screen. Names are cheap.
    pub async fn hidden_runs_in_thread(
        &self,
        thread_id: &str,
        account: &AccountId,
    ) -> StoreResult<Vec<String>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "select id from run_view
             where thread_id = $1 and account_id = $2 and hidden_at_ms is not null
             order by hidden_at_ms desc",
        )
        .bind(thread_id)
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The runs of one thread that THIS ACCOUNT may read, newest first.
    ///
    /// LAYER 4 (`docs/PLAN.md` §4.5) in the shape a thread needs it: the owner is a condition of
    /// the query rather than a check made after it, so a thread that does not exist and a thread
    /// that belongs to somebody else both come back empty and are not distinguishable here. That
    /// is what lets `GET /ag-ui/threads/{id}` answer `404` to both without first learning which it
    /// was — the same rule `run_owned_by` applies to a single run, for the same reason: a thread
    /// holds a whole conversation, and thread ids travel in client URLs and logs. A run with no
    /// owner is readable by nobody, so `account_id is null` fails this test rather than passing it.
    ///
    /// Ordered by when each run BEGAN, unlike `runs_for_thread` above, which orders by when a run
    /// last moved because the routines pane wants the latest firing at the top. A transcript is
    /// the order the turns were taken in: a long run still emitting frames would otherwise sort
    /// ahead of turns taken after it started, and the conversation would rearrange itself as it
    /// streamed. Run ids are UUIDv7 and therefore already in start order, which makes them the
    /// tie-break when two runs began in the same millisecond.
    pub async fn runs_for_thread_owned_by(
        &self,
        thread_id: &str,
        account: &AccountId,
        limit: i64,
    ) -> StoreResult<Vec<ThreadRun>> {
        let rows = sqlx::query(
            "select id, status, started_at_ms, updated_at_ms from run_view
             where thread_id = $1 and account_id = $2 and hidden_at_ms is null
             order by coalesce(started_at_ms, updated_at_ms) desc, id desc limit $3",
        )
        .bind(thread_id)
        .bind(account.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(thread_run_from_row).collect()
    }

    /// Whose run this is, if the projection knows. Recovery needs it: a run's aggregate carries
    /// its coworker but not its account, and a transcript is keyed on the pair.
    pub async fn run_account(&self, id: &RunId) -> StoreResult<Option<AccountId>> {
        let row = sqlx::query("select account_id from run_view where id = $1")
            .bind(id.as_str())
            .fetch_optional(self.pool())
            .await?;
        Ok(row
            .and_then(|row| {
                row.try_get::<Option<String>, _>("account_id")
                    .ok()
                    .flatten()
            })
            .map(AccountId::from_stored))
    }

    pub async fn run_owned_by(&self, id: &RunId, account: &AccountId) -> StoreResult<bool> {
        let row = sqlx::query("select account_id from run_view where id = $1")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(row) => {
                row.try_get::<Option<String>, _>("account_id")?
                    == Some(account.as_str().to_string())
            }
            None => false,
        })
    }

    /// Runs stopped waiting on a person, for whoever is being asked.
    ///
    /// A suspended run that nobody can find is a run nobody will ever answer, which is the same as
    /// a lost one — so this is not a convenience, it is what makes suspension safe.
    /// The same, minus the turns the person hid.
    ///
    /// `awaiting_approval` answers for the machinery — finding a parked run, resuming it,
    /// stopping it — which has to see every suspended run whatever the person did with the
    /// bubble. This answers for the person: a card is the loudest surface there is, and a turn
    /// they deleted must not come back asking to be looked at.
    pub async fn awaiting_approval_to_show(&self, account: &AccountId) -> StoreResult<Vec<RunId>> {
        let rows = sqlx::query(
            "select id from run_view
             where status = 'awaiting-approval' and account_id = $1 and hidden_at_ms is null
             order by updated_at_ms",
        )
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok(RunId::from_stored(row.try_get::<String, _>("id")?)))
            .collect()
    }

    pub async fn awaiting_approval(&self, account: &AccountId) -> StoreResult<Vec<RunId>> {
        let rows = sqlx::query(
            "select id from run_view
             where status = 'awaiting-approval' and account_id = $1
             order by updated_at_ms",
        )
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok(RunId::from_stored(row.try_get::<String, _>("id")?)))
            .collect()
    }

    /// Hold the lease on a run while a process is working on it.
    ///
    /// Renewed as the run progresses. The lease is what tells a *restart* apart from a run that is
    /// simply still going: a live process keeps pushing the expiry out, a dead one cannot.
    pub async fn hold_run(&self, id: &RunId, until_ms: i64) -> StoreResult<()> {
        sqlx::query("update run_view set leased_until_ms = $2 where id = $1")
            .bind(id.as_str())
            .bind(until_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Take ownership of runs abandoned by a restart.
    ///
    /// ONE STATEMENT, AND THAT IS THE POINT. `update … returning` claims and reports in the same
    /// breath, so two replicas booting together cannot both take the same run: the second one's
    /// `where` no longer matches. Selecting first and updating after would hand both of them the
    /// same list and run somebody's work twice.
    ///
    /// A run with no lease at all is claimable: it predates leases, or its holder died before
    /// writing one. BUT NOT A NEWBORN. The first journal batch inserts the row with no lease, and
    /// the holder's first renewal is an UPDATE that raced it — so for a moment every fresh run
    /// looks abandoned. A sweep that ticked in that window claimed a live run and failed it two
    /// seconds after birth (it ate a user_machine_shell suspension in production). A run whose
    /// last write is younger than the lease period has a process behind it; only silence that
    /// outlives a lease is abandonment.
    pub async fn claim_abandoned_runs(
        &self,
        now_ms: i64,
        lease_ms: i64,
        limit: i64,
    ) -> StoreResult<Vec<RunId>> {
        let rows = sqlx::query(
            "update run_view
                set leased_until_ms = $1 + $2
              where id in (
                    select id from run_view
                     where status = 'running'
                       and (leased_until_ms is null or leased_until_ms < $1)
                       and updated_at_ms < $1 - $2
                     order by updated_at_ms
                     limit $3
                     for update skip locked
              )
              returning id",
        )
        .bind(now_ms)
        .bind(lease_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| Ok(RunId::from_stored(row.try_get::<String, _>("id")?)))
            .collect()
    }

    /// Runs left `running`, whether or not their lease has expired. For diagnosis.
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

/// Coworkers: who works here, and which computer is theirs.
impl PgStore {
    pub async fn load_coworker(&self, id: &CoworkerId) -> StoreResult<(Coworker, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(crate::coworker_stream(id))
        .fetch_all(&self.pool)
        .await?;

        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: CoworkerEvent = serde_json::from_value(payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            events.push(event);
        }
        Ok((Coworker::replay(&events), seq))
    }

    pub async fn append_coworker(
        &self,
        id: &CoworkerId,
        account_id: &AccountId,
        expected_seq: i64,
        events: &[CoworkerEvent],
        view: &CoworkerView,
    ) -> StoreResult<i64> {
        let mut tx = self.pool.begin().await?;
        let stream = crate::coworker_stream(id);
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

        let members = serde_json::to_value(&view.members)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        sqlx::query(
            "insert into coworker_view
                (id, account_id, name, model, box_id, retired, updated_at_ms, members, role, visibility)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             on conflict (id) do update set
               name = excluded.name,
               model = excluded.model,
               box_id = excluded.box_id,
               retired = excluded.retired,
               updated_at_ms = excluded.updated_at_ms,
               members = excluded.members,
               role = excluded.role,
               visibility = excluded.visibility",
        )
        .bind(view.id.as_str())
        .bind(account_id.as_str())
        .bind(&view.name)
        .bind(&view.model)
        .bind(view.box_id.as_ref().map(|id| id.as_str()))
        .bind(view.retired)
        .bind(view.updated_at_ms)
        .bind(&members)
        .bind(&view.role)
        .bind(view.visibility.as_str())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// The roster, newest first — the order the client sorts by.
    /// Whether this person may TALK to this coworker: they own it, or it is shared with their
    /// org and they are in that org. Not whether they may manage it — management stays with the
    /// owner, and `coworkers_for` is what that is gated on.
    ///
    /// An empty `org_id` never matches, so two accounts with no org do not silently share.
    pub async fn may_use_coworker(
        &self,
        account_id: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 as ok
             from coworker_view c
             join account_view owner on owner.id = c.account_id
             join account_view caller on caller.id = $1
             where c.id = $2
               and (c.account_id = $1
                    or (c.visibility = 'org'
                        and coalesce(owner.org_id, '') <> ''
                        and owner.org_id = caller.org_id))
             limit 1",
        )
        .bind(account_id.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn coworkers_for(&self, account_id: &AccountId) -> StoreResult<Vec<CoworkerView>> {
        let rows = sqlx::query(
            "select id, name, model, box_id, retired, updated_at_ms, members, role, visibility
             from coworker_view
             where account_id = $1 and retired = false
             order by updated_at_ms desc",
        )
        .bind(account_id.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(coworker_view_row).collect()
    }

    /// Which of this account's coworkers they have hidden from their own sidebar.
    pub async fn hidden_coworker_ids(
        &self,
        account_id: &AccountId,
    ) -> StoreResult<HashSet<String>> {
        let rows = sqlx::query("select coworker_id from coworker_hidden where account_id = $1")
            .bind(account_id.as_str())
            .fetch_all(&self.pool)
            .await?;
        let mut ids = HashSet::new();
        for row in rows {
            ids.insert(row.try_get::<String, _>("coworker_id")?);
        }
        Ok(ids)
    }

    /// Hide or unhide a coworker on this account's sidebar. Idempotent.
    pub async fn set_coworker_hidden(
        &self,
        account_id: &AccountId,
        coworker_id: &CoworkerId,
        hidden: bool,
        at_ms: i64,
    ) -> StoreResult<()> {
        if hidden {
            sqlx::query(
                "insert into coworker_hidden (account_id, coworker_id, hidden_at_ms)
                 values ($1, $2, $3)
                 on conflict (account_id, coworker_id) do nothing",
            )
            .bind(account_id.as_str())
            .bind(coworker_id.as_str())
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query("delete from coworker_hidden where account_id = $1 and coworker_id = $2")
                .bind(account_id.as_str())
                .bind(coworker_id.as_str())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
}

/// Policy: who may make which coworker do what.
impl PgStore {
    /// Everything the policy engine needs for one principal and one coworker.
    ///
    /// A row that is missing comes back as `None` inside the context, and every `None` denies —
    /// so a lookup that finds nothing is a refusal, never a default-allow.
    pub async fn policy_for(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<opengrok_policy::Context> {
        let grant = sqlx::query(
            "select profile, needs_approval, revoked from grant_view
             where principal_id = $1 and coworker_id = $2",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?
        .map(|row| {
            let profile: serde_json::Value = row.try_get("profile")?;
            let needs_approval: serde_json::Value = row.try_get("needs_approval")?;
            Ok::<_, StoreError>(opengrok_policy::Grant {
                principal: principal.clone(),
                coworker: coworker.clone(),
                // An unreadable profile becomes `None` — the narrowest reading, per the rule that
                // a typo may only ever narrow access.
                profile: serde_json::from_value(profile).unwrap_or(opengrok_policy::ToolSet::None),
                // An unreadable approval list becomes `All`, which is the NARROW reading here:
                // every tool then needs a human yes. The direction flips because this field
                // restricts rather than grants, and a typo must still only ever narrow.
                needs_approval: serde_json::from_value(needs_approval)
                    .unwrap_or(opengrok_policy::ToolSet::All),
                revoked: row.try_get("revoked")?,
            })
        })
        .transpose()?;

        let ceiling = sqlx::query("select tools from ceiling_view where coworker_id = $1")
            .bind(coworker.as_str())
            .fetch_optional(&self.pool)
            .await?
            .map(|row| {
                let tools: serde_json::Value = row.try_get("tools")?;
                Ok::<_, StoreError>(opengrok_policy::Ceiling {
                    coworker: coworker.clone(),
                    tools: serde_json::from_value(tools).unwrap_or(opengrok_policy::ToolSet::None),
                })
            })
            .transpose()?;

        Ok(opengrok_policy::Context { grant, ceiling })
    }

    /// Record a grant and a ceiling together.
    ///
    /// Together because a grant without a ceiling can run nothing, and writing them separately
    /// leaves a window where a coworker exists that its own owner cannot use.
    pub async fn grant_access(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
        profile: &opengrok_policy::ToolSet,
        ceiling: &opengrok_policy::ToolSet,
        needs_approval: &opengrok_policy::ToolSet,
        at_ms: i64,
    ) -> StoreResult<()> {
        let profile_json = serde_json::to_value(profile)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let approval_json = serde_json::to_value(needs_approval)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let ceiling_json = serde_json::to_value(ceiling)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into grant_view
               (principal_id, coworker_id, profile, needs_approval, revoked, updated_at_ms)
             values ($1, $2, $3, $4, false, $5)
             on conflict (principal_id, coworker_id) do update set
               profile = excluded.profile,
               needs_approval = excluded.needs_approval,
               revoked = false,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .bind(profile_json)
        .bind(approval_json)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "insert into ceiling_view (coworker_id, tools, updated_at_ms)
             values ($1, $2, $3)
             on conflict (coworker_id) do update set
               tools = excluded.tools,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(coworker.as_str())
        .bind(ceiling_json)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Withdraw a grant. The row stays, so the log still says a grant existed and when it stopped.
    pub async fn revoke_access(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update grant_view set revoked = true, updated_at_ms = $3
             where principal_id = $1 and coworker_id = $2",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// What a connection write says about the credential itself.
///
/// Grouped because the three always travel together and always mean one thing: "here is the token
/// this write learned, and when it dies". `None` for the secret means the write is about the
/// connection rather than its credential — a lend, a revoke — and must leave the stored one alone.
#[derive(Debug, Clone, Copy)]
pub struct CredentialUpdate<'a> {
    pub secret: Option<&'a Sealed>,
    pub expires_at_ms: Option<i64>,
    pub at_ms: i64,
}

impl<'a> CredentialUpdate<'a> {
    /// A write that changes nothing about the credential.
    pub fn none(at_ms: i64) -> Self {
        Self {
            secret: None,
            expires_at_ms: None,
            at_ms,
        }
    }

    pub fn sealed(secret: &'a Sealed, expires_at_ms: Option<i64>, at_ms: i64) -> Self {
        Self {
            secret: Some(secret),
            expires_at_ms,
            at_ms,
        }
    }
}

/// Connections: an authentication that happened, and who may borrow it.
impl PgStore {
    pub async fn load_connection(&self, id: &str) -> StoreResult<(Connection, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(format!("connection/{id}"))
        .fetch_all(&self.pool)
        .await?;

        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: ConnectionEvent = serde_json::from_value(payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            events.push(event);
        }
        Ok((Connection::replay(&events), seq))
    }

    /// Append connection events, refresh the projections, and seal the credential — all in one
    /// transaction.
    ///
    /// The secret rides along rather than being written separately: a connection whose row exists
    /// without its credential is one that resolves, is chosen, and then fails at the moment of use.
    pub async fn append_connection(
        &self,
        id: &str,
        expected_seq: i64,
        events: &[ConnectionEvent],
        state: &Connection,
        update: &CredentialUpdate<'_>,
    ) -> StoreResult<i64> {
        let CredentialUpdate {
            secret,
            expires_at_ms,
            at_ms,
        } = *update;
        let mut tx = self.pool.begin().await?;
        let stream = format!("connection/{id}");
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

        let (scope, owner_id) = match &state.owner {
            Some(Owner::Global) => ("global", None),
            Some(Owner::User(account)) => ("user", Some(account.as_str().to_string())),
            Some(Owner::Bot(coworker)) => ("bot", Some(coworker.as_str().to_string())),
            None => ("global", None),
        };

        sqlx::query(
            "insert into connection_view
               (id, connector, scope, owner_id, label, disconnected, updated_at_ms, expires_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8)
             on conflict (id) do update set
               connector = excluded.connector,
               scope = excluded.scope,
               owner_id = excluded.owner_id,
               label = excluded.label,
               disconnected = excluded.disconnected,
               updated_at_ms = excluded.updated_at_ms,
               -- Kept when a caller has nothing newer to say, so a lend does not erase the expiry
               -- the last token exchange recorded.
               expires_at_ms = coalesce(excluded.expires_at_ms, connection_view.expires_at_ms)",
        )
        .bind(id)
        .bind(&state.connector)
        .bind(scope)
        .bind(owner_id)
        .bind(&state.label)
        .bind(state.disconnected)
        .bind(at_ms)
        .bind(expires_at_ms)
        .execute(&mut *tx)
        .await?;

        // Loans are rewritten wholesale from the aggregate rather than patched per event: the
        // aggregate is the truth, and reconstructing beats trying to keep two views in step.
        sqlx::query("delete from connection_loan where connection_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for coworker in &state.loans {
            sqlx::query(
                "insert into connection_loan (connection_id, coworker_id, updated_at_ms)
                 values ($1, $2, $3)",
            )
            .bind(id)
            .bind(coworker.as_str())
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        }

        if let Some(sealed) = secret {
            crate::vault_rows::write_sealed(&mut *tx, id, sealed, at_ms).await?;
        }

        // A disconnected connection keeps its record and loses its credential. The row saying it
        // once existed is worth keeping; the token is not.
        if state.disconnected {
            sqlx::query("delete from secret_store where id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(seq)
    }

    /// Every connection this coworker could possibly use, for `connection::resolve` to choose from.
    ///
    /// Deliberately returns candidates rather than picking: the choosing rule is pure, tested, and
    /// belongs in the domain — not in a SQL `order by` nobody can unit-test.
    pub async fn connections_for(
        &self,
        account: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<Vec<ConnectionView>> {
        let rows = sqlx::query(
            "select v.id, v.connector, v.scope, v.owner_id, v.label, v.updated_at_ms,
                    v.expires_at_ms
               from connection_view v
              where v.disconnected = false
                and ( v.scope = 'global'
                   or (v.scope = 'user' and v.owner_id = $1)
                   or (v.scope = 'bot'  and v.owner_id = $2) )
              order by v.updated_at_ms desc",
        )
        .bind(account.as_str())
        .bind(coworker.as_str())
        .fetch_all(&self.pool)
        .await?;

        let mut views = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let scope: String = row.try_get("scope")?;
            let owner_id: Option<String> = row.try_get("owner_id")?;
            let owner = match (scope.as_str(), owner_id) {
                ("user", Some(id)) => Owner::User(AccountId::from_stored(id)),
                ("bot", Some(id)) => Owner::Bot(CoworkerId::from_stored(id)),
                _ => Owner::Global,
            };

            let loans =
                sqlx::query("select coworker_id from connection_loan where connection_id = $1")
                    .bind(&id)
                    .fetch_all(&self.pool)
                    .await?
                    .into_iter()
                    .map(|row| {
                        Ok(CoworkerId::from_stored(
                            row.try_get::<String, _>("coworker_id")?,
                        ))
                    })
                    .collect::<StoreResult<_>>()?;

            views.push(ConnectionView {
                id,
                connector: row.try_get("connector")?,
                owner,
                label: row.try_get("label")?,
                loans,
                updated_at_ms: row.try_get("updated_at_ms")?,
                expires_at_ms: row.try_get("expires_at_ms")?,
            });
        }
        Ok(views)
    }

    /// Every connection this account owns, for showing a person what they have connected.
    pub async fn connections_owned_by(
        &self,
        account: &AccountId,
    ) -> StoreResult<Vec<ConnectionView>> {
        let rows = sqlx::query(
            "select id, connector, label, updated_at_ms, expires_at_ms from connection_view
              where scope = 'user' and owner_id = $1 and disconnected = false
              order by connector",
        )
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await?;

        let mut views = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let loans =
                sqlx::query("select coworker_id from connection_loan where connection_id = $1")
                    .bind(&id)
                    .fetch_all(&self.pool)
                    .await?
                    .into_iter()
                    .map(|row| {
                        Ok(CoworkerId::from_stored(
                            row.try_get::<String, _>("coworker_id")?,
                        ))
                    })
                    .collect::<StoreResult<_>>()?;

            views.push(ConnectionView {
                id,
                connector: row.try_get("connector")?,
                owner: Owner::User(account.clone()),
                label: row.try_get("label")?,
                loans,
                updated_at_ms: row.try_get("updated_at_ms")?,
                expires_at_ms: row.try_get("expires_at_ms")?,
            });
        }
        Ok(views)
    }

    /// Drop a sealed secret by id. Idempotent: a secret already gone is the outcome asked for.
    pub async fn delete_secret(&self, id: &str) -> StoreResult<()> {
        sqlx::query("delete from secret_store where id = $1")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Store a sealed secret on its own, outside a connection's transaction.
    ///
    /// Used for the refresh token, which lives in its own row: it outlives the access token, and
    /// keeping them apart means rotating one does not disturb the other.
    pub async fn put_secret(&self, id: &str, sealed: &Sealed, at_ms: i64) -> StoreResult<()> {
        crate::vault_rows::write_sealed(&self.pool, id, sealed, at_ms).await
    }

    /// Record a new expiry after a refresh, without touching the event log.
    ///
    /// A refresh is not a domain event: nothing about who owns the connection or who may borrow it
    /// changed, and writing one per hour would bury the events that matter.
    pub async fn touch_expiry(
        &self,
        id: &str,
        expires_at_ms: Option<i64>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update connection_view set expires_at_ms = $2, updated_at_ms = $3 where id = $1",
        )
        .bind(id)
        .bind(expires_at_ms)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A saved login may land only on a bot that is this person's own and shown to nobody
    /// else: an org-visible bot is driven by every member of the org, so a session left in its
    /// box would be theirs too.
    pub async fn coworker_is_private_and_owned_by(
        &self,
        account_id: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 as ok from coworker_view
             where id = $2 and account_id = $1 and visibility = 'private'",
        )
        .bind(account_id.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// The secret_store key of a saved site login's password. The account id sits in the
    /// key so the purge's substring sweep finds it.
    pub fn site_login_secret_id(account_id: &AccountId, id: &str) -> String {
        format!("site-login:{}:{id}", account_id.as_str())
    }

    /// The key of the row's authenticator-code seed (an `otpauth://` URI), beside the password.
    pub fn site_login_code_id(account_id: &AccountId, id: &str) -> String {
        format!("site-login-otp:{}:{id}", account_id.as_str())
    }

    /// The key of a passkey row's private key (PKCS#8, base64).
    pub fn site_login_passkey_id(account_id: &AccountId, id: &str) -> String {
        format!("site-login-passkey:{}:{id}", account_id.as_str())
    }

    /// A person's saved site logins, never the secrets. Ordered for a settings list.
    pub async fn site_logins(&self, account_id: &AccountId) -> StoreResult<Vec<SiteLoginRow>> {
        let rows = sqlx::query(
            "select id, origin, username, label, kind, notes, created_at_ms, updated_at_ms, last_used_at_ms,
                    passkey_credential_id, passkey_rp_id, passkey_user_handle
             from site_login where account_id = $1 order by label, origin, username",
        )
        .bind(account_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(site_login_row).collect()
    }

    /// Save (or replace) one site login: the row keyed by (account, origin, username), the
    /// password and the code seed sealed into secret_store under the row's id. A secret
    /// given as `None` is left as it was. Returns the row, which never holds a secret.
    pub async fn upsert_site_login(
        &self,
        vault: &Vault,
        account_id: &AccountId,
        login: &SiteLoginWrite<'_>,
        at_ms: i64,
    ) -> StoreResult<SiteLoginRow> {
        // The row first, in the transaction: on a conflict the existing id comes back, so two
        // saves racing for the same login end with one row and one secret under its id.
        let candidate = format!("sl_{}", uuid::Uuid::now_v7());
        let label = if login.label.trim().is_empty() {
            format!("{} on {}", login.username, login.origin)
        } else {
            login.label.trim().to_string()
        };
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "insert into site_login (id, account_id, origin, username, label, kind, notes, created_at_ms, updated_at_ms,
                                     passkey_credential_id, passkey_rp_id, passkey_user_handle)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $8, $9, $10, $11)
             on conflict (account_id, origin, username, kind) do update set
               label = excluded.label, notes = excluded.notes,
               updated_at_ms = excluded.updated_at_ms,
               passkey_credential_id = coalesce(excluded.passkey_credential_id, site_login.passkey_credential_id),
               passkey_rp_id = coalesce(excluded.passkey_rp_id, site_login.passkey_rp_id),
               passkey_user_handle = coalesce(excluded.passkey_user_handle, site_login.passkey_user_handle)
             returning id, origin, username, label, kind, notes, created_at_ms, updated_at_ms, last_used_at_ms,
                       passkey_credential_id, passkey_rp_id, passkey_user_handle",
        )
        .bind(&candidate)
        .bind(account_id.as_str())
        .bind(login.origin)
        .bind(login.username)
        .bind(&label)
        .bind(login.kind)
        .bind(login.notes)
        .bind(at_ms)
        .bind(login.passkey.as_ref().map(|p| p.credential_id_b64.as_str()))
        .bind(login.passkey.as_ref().map(|p| p.rp_id.as_str()))
        .bind(login.passkey.as_ref().map(|p| p.user_handle_b64.as_str()))
        .fetch_one(&mut *tx)
        .await?;
        let row = site_login_row(row)?;
        for (key, secret) in [
            (
                Self::site_login_secret_id(account_id, &row.id),
                login.password,
            ),
            (Self::site_login_code_id(account_id, &row.id), login.otpauth),
            (
                Self::site_login_passkey_id(account_id, &row.id),
                login.passkey.as_ref().map(|p| p.private_key_b64.as_str()),
            ),
        ] {
            let Some(secret) = secret else {
                continue;
            };
            let sealed = vault.seal(&key, secret)?;
            crate::vault_rows::write_sealed(&mut *tx, &key, &sealed, at_ms).await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    /// The title and notes of one of the person's own rows. False when the row is not theirs.
    pub async fn update_site_login(
        &self,
        account_id: &AccountId,
        id: &str,
        label: Option<&str>,
        notes: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let changed = sqlx::query(
            "update site_login set
               label = coalesce($3, label), notes = coalesce($4, notes), updated_at_ms = $5
             where account_id = $1 and id = $2",
        )
        .bind(account_id.as_str())
        .bind(id)
        .bind(label)
        .bind(notes)
        .bind(at_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed > 0)
    }

    /// A bot just used this login (a `savedLogin` fill landed).
    pub async fn touch_site_login_used(
        &self,
        account_id: &AccountId,
        id: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query("update site_login set last_used_at_ms = $3 where account_id = $1 and id = $2")
            .bind(account_id.as_str())
            .bind(id)
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete one of the person's own site logins, secrets included. False when there was
    /// no such row of theirs (another account's id is "no such row" too).
    pub async fn delete_site_login(&self, account_id: &AccountId, id: &str) -> StoreResult<bool> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("delete from site_login where account_id = $1 and id = $2")
            .bind(account_id.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Ok(false);
        }
        sqlx::query("delete from secret_store where id = $1 or id = $2 or id = $3")
            .bind(Self::site_login_secret_id(account_id, id))
            .bind(Self::site_login_code_id(account_id, id))
            .bind(Self::site_login_passkey_id(account_id, id))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The secrets of one of the person's own site logins, opened for their app alone.
    /// None when the row is not theirs.
    pub async fn open_site_login(
        &self,
        vault: &Vault,
        account_id: &AccountId,
        id: &str,
    ) -> StoreResult<Option<SiteLoginSecrets>> {
        let owned: Option<String> =
            sqlx::query_scalar("select id from site_login where account_id = $1 and id = $2")
                .bind(account_id.as_str())
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        if owned.is_none() {
            return Ok(None);
        }
        let password = self
            .open_credential(vault, &Self::site_login_secret_id(account_id, id))
            .await?;
        let otpauth = self
            .open_credential(vault, &Self::site_login_code_id(account_id, id))
            .await?;
        let passkey_key = self
            .open_credential(vault, &Self::site_login_passkey_id(account_id, id))
            .await?;
        Ok(Some(SiteLoginSecrets {
            password,
            otpauth,
            passkey_key,
        }))
    }

    /// Open a connection's credential. The one place a token is ever in plaintext.
    pub async fn open_credential(&self, vault: &Vault, id: &str) -> StoreResult<Option<String>> {
        let row = sqlx::query("select nonce, ciphertext, key_id from secret_store where id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| vault.open(id, &crate::vault_rows::sealed_of(&row)?))
            .transpose()
    }

    // ---- Per-org computer credentials (box.ascii.dev key, Windows 365 creds) ----
    //
    // Reuses the generic sealed `secret_store`, keyed `org-computer:{org}:{kind}`. The org admin
    // sets these on the admin dashboard; the server opens them to provision that org's boxes. The
    // plaintext key never leaves the server and is never returned to any client — only whether a
    // kind is configured.

    pub async fn set_org_computer_secret(
        &self,
        vault: &Vault,
        org_id: &str,
        kind: &str,
        plaintext: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        let id = format!("org-computer:{org_id}:{kind}");
        let sealed = vault.seal(&id, plaintext)?;
        crate::vault_rows::write_sealed(&self.pool, &id, &sealed, at_ms).await?;
        Ok(())
    }

    /// The plaintext credential for one org+kind — the one place it is in the clear, at provision
    /// time. `None` when the org has not configured that kind.
    pub async fn org_computer_secret(
        &self,
        vault: &Vault,
        org_id: &str,
        kind: &str,
    ) -> StoreResult<Option<String>> {
        self.open_credential(vault, &format!("org-computer:{org_id}:{kind}"))
            .await
    }

    pub async fn clear_org_computer_secret(&self, org_id: &str, kind: &str) -> StoreResult<()> {
        sqlx::query("delete from secret_store where id = $1")
            .bind(format!("org-computer:{org_id}:{kind}"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Computer sharing mode (org default + per-account override) ----

    /// Set a sharing mode for a scope. scope is "org" (the org default) or "account" (an override);
    /// mode is "per-org" | "per-account" | "per-bot".
    pub async fn set_sharing_mode(
        &self,
        scope: &str,
        scope_id: &str,
        mode: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into computer_sharing (scope, scope_id, mode, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (scope, scope_id) do update set
               mode = excluded.mode, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(mode)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn sharing_mode(&self, scope: &str, scope_id: &str) -> StoreResult<Option<String>> {
        let row =
            sqlx::query("select mode from computer_sharing where scope = $1 and scope_id = $2")
                .bind(scope)
                .bind(scope_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("mode"))
            .transpose()?)
    }

    pub async fn clear_sharing_mode(&self, scope: &str, scope_id: &str) -> StoreResult<()> {
        sqlx::query("delete from computer_sharing where scope = $1 and scope_id = $2")
            .bind(scope)
            .bind(scope_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Egress policy: the standing answer to the tunnel card, per computer scope ----

    /// The stored mode for a computer scope, or `None` when unset (the server reads that as
    /// `ask`, one card per run).
    pub async fn egress_policy_mode(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<String>> {
        let row = sqlx::query("select mode from egress_policy where scope = $1 and scope_id = $2")
            .bind(scope)
            .bind(scope_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("mode"))
            .transpose()?)
    }

    /// Set the mode for a computer scope; mode is "bypass" | "ask" | "never" (the server
    /// validates the word).
    pub async fn set_egress_policy_mode(
        &self,
        scope: &str,
        scope_id: &str,
        mode: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into egress_policy (scope, scope_id, mode, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (scope, scope_id) do update set
               mode = excluded.mode, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(mode)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Reverse-exec consent, per (account, machine). Raw pieces only — the server assembles the
    //      LocalExecPolicy and runs the gate; this crate stays free of that logic. ----

    /// The stored mode for a machine, or `None` when unset (the gate reads that as the default,
    /// `never` — the channel is off).
    pub async fn local_exec_mode(
        &self,
        account_id: &str,
        machine_id: &str,
    ) -> StoreResult<Option<String>> {
        let row = sqlx::query(
            "select mode from local_exec_policy where account_id = $1 and machine_id = $2",
        )
        .bind(account_id)
        .bind(machine_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("mode"))
            .transpose()?)
    }

    pub async fn set_local_exec_mode(
        &self,
        account_id: &str,
        machine_id: &str,
        mode: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into local_exec_policy (account_id, machine_id, mode, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (account_id, machine_id) do update set
               mode = excluded.mode, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(account_id)
        .bind(machine_id)
        .bind(mode)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The allow or deny patterns for a machine (`kind` = "allow" | "deny").
    pub async fn local_exec_rules(
        &self,
        account_id: &str,
        machine_id: &str,
        kind: &str,
    ) -> StoreResult<Vec<String>> {
        let rows = sqlx::query(
            "select pattern from local_exec_rule
             where account_id = $1 and machine_id = $2 and kind = $3 order by added_at_ms",
        )
        .bind(account_id)
        .bind(machine_id)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok(row.try_get::<String, _>("pattern")?))
            .collect()
    }

    pub async fn add_local_exec_rule(
        &self,
        account_id: &str,
        machine_id: &str,
        kind: &str,
        pattern: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into local_exec_rule (account_id, machine_id, kind, pattern, added_at_ms)
             values ($1, $2, $3, $4, $5)
             on conflict (account_id, machine_id, kind, pattern) do nothing",
        )
        .bind(account_id)
        .bind(machine_id)
        .bind(kind)
        .bind(pattern)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_local_exec_rule(
        &self,
        account_id: &str,
        machine_id: &str,
        kind: &str,
        pattern: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "delete from local_exec_rule
             where account_id = $1 and machine_id = $2 and kind = $3 and pattern = $4",
        )
        .bind(account_id)
        .bind(machine_id)
        .bind(kind)
        .bind(pattern)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Reverse-exec: enrolled machine daemons (token id only) and the audit log. ----

    /// Enrol (or re-enrol) a machine's daemon: store its token id, clear any prior revocation.
    pub async fn enrol_daemon(
        &self,
        account_id: &str,
        machine_id: &str,
        label: &str,
        jti: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into local_exec_daemon (account_id, machine_id, label, jti, enrolled_at_ms, revoked)
             values ($1, $2, $3, $4, $5, false)
             on conflict (account_id, machine_id) do update set
               label = excluded.label, jti = excluded.jti,
               enrolled_at_ms = excluded.enrolled_at_ms, revoked = false",
        )
        .bind(account_id)
        .bind(machine_id)
        .bind(label)
        .bind(jti)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The daemon's current token id and whether it is revoked, for verifying a presented token.
    pub async fn daemon_jti(
        &self,
        account_id: &str,
        machine_id: &str,
    ) -> StoreResult<Option<(String, bool)>> {
        let row = sqlx::query(
            "select jti, revoked from local_exec_daemon where account_id = $1 and machine_id = $2",
        )
        .bind(account_id)
        .bind(machine_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("jti")?,
                row.try_get::<bool, _>("revoked")?,
            ))
        })
        .transpose()
    }

    pub async fn revoke_daemon(&self, account_id: &str, machine_id: &str) -> StoreResult<()> {
        sqlx::query(
            "update local_exec_daemon set revoked = true where account_id = $1 and machine_id = $2",
        )
        .bind(account_id)
        .bind(machine_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The account's enrolled machines: (machine_id, label, enrolled_at_ms, revoked).
    #[allow(clippy::type_complexity)]
    pub async fn list_daemons(
        &self,
        account_id: &str,
    ) -> StoreResult<Vec<(String, String, i64, bool)>> {
        let rows = sqlx::query(
            "select machine_id, label, enrolled_at_ms, revoked from local_exec_daemon
             where account_id = $1 order by enrolled_at_ms desc",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("machine_id")?,
                    row.try_get::<String, _>("label")?,
                    row.try_get::<i64, _>("enrolled_at_ms")?,
                    row.try_get::<bool, _>("revoked")?,
                ))
            })
            .collect()
    }

    /// Write an audit row at enqueue time (before the command runs).
    #[allow(clippy::too_many_arguments)]
    pub async fn audit_local_exec(
        &self,
        id: &str,
        account_id: &str,
        machine_id: &str,
        origin: &str,
        command: &str,
        decision: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into local_exec_audit
               (id, account_id, machine_id, origin, command, decision, requested_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(account_id)
        .bind(machine_id)
        .bind(origin)
        .bind(command)
        .bind(decision)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a command's result on its audit row: the ShellResult `outcome` case (success /
    /// failure / timeout / rejected / spawnError / permissionDenied) and, when there is one, the
    /// process exit code. A refusal is a case with no exit code, not a non-zero exit.
    pub async fn finish_local_exec_audit(
        &self,
        id: &str,
        outcome: &str,
        exit_code: Option<i32>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update local_exec_audit
                set outcome = $2, exit_code = $3, finished_at_ms = $4
              where id = $1",
        )
        .bind(id)
        .bind(outcome)
        .bind(exit_code)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The account's recent audit rows, newest first (all machines).
    pub async fn local_exec_audit_log(
        &self,
        account_id: &str,
        limit: i64,
    ) -> StoreResult<Vec<serde_json::Value>> {
        let rows = sqlx::query(
            "select id, machine_id, origin, command, decision, requested_at_ms, outcome,
                    exit_code, finished_at_ms
             from local_exec_audit where account_id = $1
             order by requested_at_ms desc limit $2",
        )
        .bind(account_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(serde_json::json!({
                    "id": row.try_get::<String, _>("id")?,
                    "machineId": row.try_get::<String, _>("machine_id")?,
                    "origin": row.try_get::<String, _>("origin")?,
                    "command": row.try_get::<String, _>("command")?,
                    "decision": row.try_get::<String, _>("decision")?,
                    "requestedAtMs": row.try_get::<i64, _>("requested_at_ms")?,
                    "outcome": row.try_get::<Option<String>, _>("outcome")?,
                    "exitCode": row.try_get::<Option<i32>, _>("exit_code")?,
                    "finishedAtMs": row.try_get::<Option<i64>, _>("finished_at_ms")?,
                }))
            })
            .collect()
    }

    // ---- WebAuthn device registry (passkey step-up, slice 7) ----

    /// Register (or replace) a WebAuthn credential for an account. Upsert on the credential id so a
    /// re-registration of the same authenticator refreshes it rather than erroring; a re-register
    /// also clears a prior revocation, because registering it again IS re-authorising it.
    pub async fn register_webauthn_credential(
        &self,
        account_id: &str,
        credential_id: &str,
        public_key: &str,
        label: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into webauthn_credential
               (account_id, credential_id, public_key, sign_count, label, created_at_ms, revoked)
             values ($1, $2, $3, 0, $4, $5, false)
             on conflict (account_id, credential_id) do update
               set public_key = excluded.public_key,
                   label = excluded.label,
                   revoked = false",
        )
        .bind(account_id)
        .bind(credential_id)
        .bind(public_key)
        .bind(label)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// An account's registered devices, newest first. Includes revoked rows (the dashboard shows
    /// them as revoked); callers that verify an assertion filter to `!revoked` themselves.
    pub async fn webauthn_credentials(
        &self,
        account_id: &str,
    ) -> StoreResult<Vec<(String, String, i64, String, i64, Option<i64>, bool)>> {
        let rows = sqlx::query(
            "select credential_id, public_key, sign_count, label, created_at_ms,
                    last_used_at_ms, revoked
             from webauthn_credential where account_id = $1
             order by created_at_ms desc",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("credential_id")?,
                    row.try_get::<String, _>("public_key")?,
                    row.try_get::<i64, _>("sign_count")?,
                    row.try_get::<String, _>("label")?,
                    row.try_get::<i64, _>("created_at_ms")?,
                    row.try_get::<Option<i64>, _>("last_used_at_ms")?,
                    row.try_get::<bool, _>("revoked")?,
                ))
            })
            .collect()
    }

    /// Record a successful assertion: bump the stored sign_count (replay/cloning defence) and stamp
    /// last-used. Only touches a non-revoked row.
    pub async fn touch_webauthn_credential(
        &self,
        account_id: &str,
        credential_id: &str,
        sign_count: i64,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update webauthn_credential
                set sign_count = $3, last_used_at_ms = $4
              where account_id = $1 and credential_id = $2 and not revoked",
        )
        .bind(account_id)
        .bind(credential_id)
        .bind(sign_count)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Revoke a device from the registry — it can no longer satisfy a step-up. Not deleted, so the
    /// dashboard can still show it as revoked and a re-register can un-revoke it.
    pub async fn revoke_webauthn_credential(
        &self,
        account_id: &str,
        credential_id: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "update webauthn_credential set revoked = true
              where account_id = $1 and credential_id = $2",
        )
        .bind(account_id)
        .bind(credential_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Does this account have ANY registered, non-revoked device? The gate for "an unregistered
    /// device gets no remote control" — false ⇒ the control plane refuses the dangerous actions.
    pub async fn has_registered_device(&self, account_id: &str) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 as one from webauthn_credential
              where account_id = $1 and not revoked limit 1",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    // ---- A computer keyed by the scope that shares it (org / account / bot) ----

    pub async fn scoped_computer(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<(String, String)>> {
        let row = sqlx::query(
            "select box_id, kind from scoped_computer where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("box_id")?,
                row.try_get::<String, _>("kind")?,
            ))
        })
        .transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn set_scoped_computer(
        &self,
        scope: &str,
        scope_id: &str,
        box_id: &str,
        kind: &str,
        org_id: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into scoped_computer (scope, scope_id, box_id, kind, org_id, last_used_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $6)
             on conflict (scope, scope_id) do update set
               box_id = excluded.box_id, kind = excluded.kind, org_id = excluded.org_id,
               last_used_at_ms = excluded.last_used_at_ms, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(box_id)
        .bind(kind)
        .bind(org_id)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A scoped computer with its idle state — (box_id, kind, stopped).
    /// Every recorded box of one provider kind, for an admin's "update all" and its counts.
    pub async fn scoped_computers_of_kind(
        &self,
        kind: &str,
    ) -> StoreResult<Vec<(String, String, String, Option<String>)>> {
        let rows = sqlx::query(
            "select scope, scope_id, box_id, org_id from scoped_computer where kind = $1",
        )
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("scope")?,
                    row.try_get::<String, _>("scope_id")?,
                    row.try_get::<String, _>("box_id")?,
                    row.try_get::<Option<String>, _>("org_id")?,
                ))
            })
            .collect()
    }

    /// Start (or restart) the update record for a scope: phase set, clock reset, error cleared.
    pub async fn begin_box_update(
        &self,
        scope: &str,
        scope_id: &str,
        phase: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into box_update (scope, scope_id, phase, started_at_ms, updated_at_ms, error)
             values ($1, $2, $3, $4, $4, null)
             on conflict (scope, scope_id) do update set
               phase = excluded.phase, started_at_ms = excluded.started_at_ms,
               updated_at_ms = excluded.updated_at_ms, error = null",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(phase)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Move an update along, or end it in failure with the reason.
    pub async fn set_box_update_phase(
        &self,
        scope: &str,
        scope_id: &str,
        phase: &str,
        error: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update box_update set phase = $3, error = $4, updated_at_ms = $5
             where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(phase)
        .bind(error)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `(phase, started_at_ms, updated_at_ms, error)` for a scope's update, if one is recorded.
    pub async fn box_update(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<(String, i64, i64, Option<String>)>> {
        let row = sqlx::query(
            "select phase, started_at_ms, updated_at_ms, error from box_update
             where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("phase")?,
                row.try_get::<i64, _>("started_at_ms")?,
                row.try_get::<i64, _>("updated_at_ms")?,
                row.try_get::<Option<String>, _>("error")?,
            ))
        })
        .transpose()
    }

    pub async fn clear_box_update(&self, scope: &str, scope_id: &str) -> StoreResult<()> {
        sqlx::query("delete from box_update where scope = $1 and scope_id = $2")
            .bind(scope)
            .bind(scope_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn scoped_computer_full(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<(String, String, bool)>> {
        let row = sqlx::query(
            "select box_id, kind, stopped from scoped_computer where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("box_id")?,
                row.try_get::<String, _>("kind")?,
                row.try_get::<bool, _>("stopped")?,
            ))
        })
        .transpose()
    }

    /// Mark a scoped computer used now (and not stopped) — called on the run path.
    pub async fn mark_scoped_used(
        &self,
        scope: &str,
        scope_id: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update scoped_computer set last_used_at_ms = $3, stopped = false where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_scoped_stopped(&self, scope: &str, scope_id: &str) -> StoreResult<()> {
        sqlx::query("update scoped_computer set stopped = true where scope = $1 and scope_id = $2")
            .bind(scope)
            .bind(scope_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Running boxes idle since before `before_ms` — the sweep stops these. Returns
    /// (scope, scope_id, box_id, kind). A box never used yet (null last_used) is left alone.
    #[allow(clippy::type_complexity)]
    pub async fn idle_scoped_computers(
        &self,
        before_ms: i64,
    ) -> StoreResult<Vec<(String, String, String, String, Option<String>)>> {
        let rows = sqlx::query(
            "select scope, scope_id, box_id, kind, org_id from scoped_computer
             where stopped = false and last_used_at_ms is not null and last_used_at_ms < $1",
        )
        .bind(before_ms)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("scope")?,
                    row.try_get::<String, _>("scope_id")?,
                    row.try_get::<String, _>("box_id")?,
                    row.try_get::<String, _>("kind")?,
                    row.try_get::<Option<String>, _>("org_id")?,
                ))
            })
            .collect()
    }

    pub async fn clear_scoped_computer(&self, scope: &str, scope_id: &str) -> StoreResult<()> {
        sqlx::query("delete from scoped_computer where scope = $1 and scope_id = $2")
            .bind(scope)
            .bind(scope_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- The account's last computer-provisioning error ----

    pub async fn set_account_computer_error(
        &self,
        account_id: &str,
        code: &str,
        message: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into account_computer_error (account_id, code, message, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (account_id) do update set
               code = excluded.code, message = excluded.message, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(account_id)
        .bind(code)
        .bind(message)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The account's last provisioning error as (code, message, updated_at_ms), or None when it
    /// has none. The STAMP is part of the answer: without it an hours-old refusal is
    /// indistinguishable from one a second old, which is exactly how a stale `quota_exceeded` was
    /// read as the live state of a box on 5 Sep 2026.
    pub async fn account_computer_error(
        &self,
        account_id: &str,
    ) -> StoreResult<Option<(String, String, i64)>> {
        let row = sqlx::query(
            "select code, message, updated_at_ms from account_computer_error where account_id = $1",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("code")?,
                row.try_get::<String, _>("message")?,
                row.try_get::<i64, _>("updated_at_ms")?,
            ))
        })
        .transpose()
    }

    pub async fn clear_account_computer_error(&self, account_id: &str) -> StoreResult<()> {
        sqlx::query("delete from account_computer_error where account_id = $1")
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- The account's one shared computer (1 account = 1 computer) ----

    /// The account's computer, if it has one — the box id and its kind.
    pub async fn account_computer(
        &self,
        account_id: &str,
    ) -> StoreResult<Option<(String, String)>> {
        let row = sqlx::query("select box_id, kind from account_computer where account_id = $1")
            .bind(account_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("box_id")?,
                row.try_get::<String, _>("kind")?,
            ))
        })
        .transpose()
    }

    /// Record the account's computer (created on its first agent). One row per account.
    pub async fn set_account_computer(
        &self,
        account_id: &str,
        box_id: &str,
        kind: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into account_computer (account_id, box_id, kind, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (account_id) do update set
               box_id = excluded.box_id, kind = excluded.kind, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(account_id)
        .bind(box_id)
        .bind(kind)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forget the account's computer (its last agent was deleted and the box destroyed).
    pub async fn clear_account_computer(&self, account_id: &str) -> StoreResult<()> {
        sqlx::query("delete from account_computer where account_id = $1")
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Which computer kinds this org has a secret row for — the names only, never the secrets.
    /// A row can exist and still be unreadable (KEK rotated); callers that advertise "configured"
    /// to a person must use `org_computer_kinds_openable` so a dead blob is not a live computer.
    pub async fn org_computer_kinds(&self, org_id: &str) -> StoreResult<Vec<String>> {
        let prefix = format!("org-computer:{org_id}:");
        let rows = sqlx::query("select id from secret_store where id like $1")
            .bind(format!("{prefix}%"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.try_get::<String, _>("id").ok())
            .filter_map(|id| id.strip_prefix(&prefix).map(str::to_string))
            .collect())
    }

    /// Kinds whose secrets actually open with this vault. A ciphertext sealed under a lost KEK
    /// is not configured — listing it as ready is how a dead key looked like a live computer.
    pub async fn org_computer_kinds_openable(
        &self,
        vault: &Vault,
        org_id: &str,
    ) -> StoreResult<Vec<String>> {
        let mut openable = Vec::new();
        for kind in self.org_computer_kinds(org_id).await? {
            if matches!(
                self.org_computer_secret(vault, org_id, &kind).await,
                Ok(Some(_))
            ) {
                openable.push(kind);
            }
        }
        Ok(openable)
    }
}

fn coworker_view_row(row: &sqlx::postgres::PgRow) -> StoreResult<CoworkerView> {
    Ok(CoworkerView {
        id: CoworkerId::from_stored(row.try_get::<String, _>("id")?),
        name: row.try_get("name")?,
        model: row.try_get("model")?,
        box_id: row
            .try_get::<Option<String>, _>("box_id")?
            .map(BoxId::from_stored),
        retired: row.try_get("retired")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        members: serde_json::from_value(row.try_get::<serde_json::Value, _>("members")?)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        role: row.try_get("role")?,
        // An unrecognised string reads as private: the safe answer, and the only one that cannot
        // accidentally share a coworker nobody chose to share.
        visibility: row
            .try_get::<Option<String>, _>("visibility")?
            .and_then(|text| opengrok_core::coworker::Visibility::parse(&text))
            .unwrap_or_default(),
    })
}

// ---- Recipes: taught tasks, their versions, who they are shared with, which bots run them ----

/// A recipe as a list shows it, with the caller's relation to it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeRow {
    pub id: String,
    pub owner_id: String,
    pub org_id: Option<String>,
    pub name: String,
    pub description: String,
    pub screen_w: i32,
    pub screen_h: i32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
    /// The newest version number.
    pub latest_version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeVersionRow {
    pub recipe_id: String,
    pub version: i32,
    pub kind: String,
    pub body: serde_json::Value,
    pub note: String,
    pub created_by: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeShareRow {
    pub recipe_id: String,
    pub scope: String,
    pub scope_id: String,
    pub granted_by: String,
    pub granted_at_ms: i64,
    pub accepted_at_ms: Option<i64>,
    pub declined_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeGrantRow {
    pub recipe_id: String,
    pub coworker_id: String,
    pub granted_by: String,
    pub granted_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeRunRow {
    pub id: String,
    pub recipe_id: String,
    pub version: i32,
    pub coworker_id: String,
    pub run_id: Option<String>,
    pub ok: bool,
    pub stopped_at: Option<i32>,
    pub receipt: serde_json::Value,
    pub at_ms: i64,
}

fn recipe_row(row: &sqlx::postgres::PgRow) -> StoreResult<RecipeRow> {
    Ok(RecipeRow {
        id: row.try_get("id")?,
        owner_id: row.try_get("owner_id")?,
        org_id: row.try_get("org_id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        screen_w: row.try_get("screen_w")?,
        screen_h: row.try_get("screen_h")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        deleted_at_ms: row.try_get("deleted_at_ms")?,
        latest_version: row
            .try_get::<Option<i32>, _>("latest_version")?
            .unwrap_or(0),
    })
}

/// One `recipe_version` row, read the same way wherever it is read.
fn recipe_version_row(row: &sqlx::postgres::PgRow) -> StoreResult<RecipeVersionRow> {
    Ok(RecipeVersionRow {
        recipe_id: row.try_get("recipe_id")?,
        version: row.try_get("version")?,
        kind: row.try_get("kind")?,
        body: row.try_get("body")?,
        note: row.try_get("note")?,
        created_by: row.try_get("created_by")?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}

const RECIPE_SELECT: &str =
    "select r.id, r.owner_id, r.org_id, r.name, r.description, r.screen_w, r.screen_h,
        r.created_at_ms, r.updated_at_ms, r.deleted_at_ms,
        (select max(version) from recipe_version v where v.recipe_id = r.id) as latest_version
   from recipe r";

impl PgStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn create_recipe(
        &self,
        id: &str,
        owner_id: &str,
        org_id: Option<&str>,
        name: &str,
        description: &str,
        screen: (i32, i32),
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe (id, owner_id, org_id, name, description, screen_w, screen_h, created_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
        )
        .bind(id)
        .bind(owner_id)
        .bind(org_id)
        .bind(name)
        .bind(description)
        .bind(screen.0)
        .bind(screen.1)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn rename_recipe(
        &self,
        id: &str,
        name: &str,
        description: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update recipe set name = $2, description = $3, updated_at_ms = $4 where id = $1",
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn soft_delete_recipe(&self, id: &str, at_ms: i64) -> StoreResult<()> {
        sqlx::query("update recipe set deleted_at_ms = $2, updated_at_ms = $2 where id = $1")
            .bind(id)
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recipe(&self, id: &str) -> StoreResult<Option<RecipeRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT} where r.id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(recipe_row).transpose()
    }

    /// The recipes a person owns (not deleted).
    pub async fn recipes_owned_by(&self, owner_id: &str) -> StoreResult<Vec<RecipeRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT} where r.owner_id = $1 and r.deleted_at_ms is null order by r.updated_at_ms desc"
        )))
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(recipe_row).collect()
    }

    /// Recipes shared to this person directly or to their org, each with its share row for
    /// them (accepted, pending or declined). Excludes what they own and what is deleted.
    pub async fn recipes_shared_with(
        &self,
        account_id: &str,
        org_id: Option<&str>,
    ) -> StoreResult<Vec<(RecipeRow, RecipeShareRow)>> {
        // The recipe's columns and the share's, one row per share.
        let select = RECIPE_SELECT.replace(
            "   from recipe r",
            ", s.scope, s.scope_id, s.granted_by, s.granted_at_ms, s.accepted_at_ms, s.declined_at_ms
   from recipe r",
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{select}
              join recipe_share s on s.recipe_id = r.id
             where r.deleted_at_ms is null and r.owner_id <> $1
               and ((s.scope = 'account' and s.scope_id = $1)
                 or (s.scope = 'org' and $2 is not null and s.scope_id <> '' and s.scope_id = $2))
             order by r.updated_at_ms desc"
        )))
        .bind(account_id)
        // AN EMPTY ORG IS NO ORG, and it is checked on BOTH sides. `unwrap_or("")` used to
        // collapse "no org" into an org id of `''`, which matched any `recipe_share` row written
        // with the same empty id — every person in no org would have been shown every other
        // orgless person's shared recipes. A NULL matches nothing, and `scope_id <> ''`
        // neutralises a row an older build may already have written.
        .bind(org_id.filter(|org| !org.is_empty()))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for row in &rows {
            out.push((
                recipe_row(row)?,
                RecipeShareRow {
                    recipe_id: row.try_get("id")?,
                    scope: row.try_get("scope")?,
                    scope_id: row.try_get("scope_id")?,
                    granted_by: row.try_get("granted_by")?,
                    granted_at_ms: row.try_get("granted_at_ms")?,
                    accepted_at_ms: row.try_get("accepted_at_ms")?,
                    declined_at_ms: row.try_get("declined_at_ms")?,
                },
            ));
        }
        Ok(out)
    }

    /// What an org's admin sees: every recipe shared org-wide, with how many members accepted.
    pub async fn recipes_shared_to_org(&self, org_id: &str) -> StoreResult<Vec<(RecipeRow, i64)>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT}
              join recipe_share s on s.recipe_id = r.id and s.scope = 'org' and s.scope_id = $1
             where r.deleted_at_ms is null
             order by r.updated_at_ms desc"
        )))
        .bind(org_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for row in &rows {
            let recipe = recipe_row(row)?;
            let accepted: i64 = sqlx::query_scalar(
                "select count(*) from recipe_share
                  where recipe_id = $1 and scope = 'account' and granted_by = 'org:' || $2
                    and accepted_at_ms is not null",
            )
            .bind(&recipe.id)
            .bind(org_id)
            .fetch_one(&self.pool)
            .await?;
            out.push((recipe, accepted));
        }
        Ok(out)
    }

    pub async fn add_recipe_version(
        &self,
        recipe_id: &str,
        kind: &str,
        body: &serde_json::Value,
        note: &str,
        created_by: &str,
        at_ms: i64,
    ) -> StoreResult<i32> {
        let mut tx = self.pool.begin().await?;
        let next: i32 = sqlx::query_scalar(
            "select coalesce(max(version), 0) + 1 from recipe_version where recipe_id = $1",
        )
        .bind(recipe_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "insert into recipe_version (recipe_id, version, kind, body, note, created_by, created_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(recipe_id)
        .bind(next)
        .bind(kind)
        .bind(body)
        .bind(note)
        .bind(created_by)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update recipe set updated_at_ms = $2 where id = $1")
            .bind(recipe_id)
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(next)
    }

    /// Drop one version and the runs that played it. The runs go with it because history is
    /// read a version at a time: a run pointing at a version nobody can open is a row with no
    /// home. Returns what was deleted, so a caller can tell "gone" from "was never there".
    pub async fn delete_recipe_version(
        &self,
        recipe_id: &str,
        version: i32,
    ) -> StoreResult<Option<RecipeVersionRow>> {
        let Some(row) = self.recipe_version(recipe_id, version).await? else {
            return Ok(None);
        };
        sqlx::query("delete from recipe_run where recipe_id = $1 and version = $2")
            .bind(recipe_id)
            .bind(version)
            .execute(&self.pool)
            .await?;
        sqlx::query("delete from recipe_version where recipe_id = $1 and version = $2")
            .bind(recipe_id)
            .bind(version)
            .execute(&self.pool)
            .await?;
        Ok(Some(row))
    }

    /// One version of a recipe, by its number.
    pub async fn recipe_version(
        &self,
        recipe_id: &str,
        version: i32,
    ) -> StoreResult<Option<RecipeVersionRow>> {
        let row = sqlx::query(
            "select recipe_id, version, kind, body, note, created_by, created_at_ms
               from recipe_version where recipe_id = $1 and version = $2",
        )
        .bind(recipe_id)
        .bind(version)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(recipe_version_row).transpose()
    }

    pub async fn recipe_versions(&self, recipe_id: &str) -> StoreResult<Vec<RecipeVersionRow>> {
        let rows = sqlx::query(
            "select recipe_id, version, kind, body, note, created_by, created_at_ms
               from recipe_version where recipe_id = $1 order by version",
        )
        .bind(recipe_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(recipe_version_row).collect()
    }

    /// The version a run should use: the newest that is not the raw tape.
    pub async fn recipe_runnable_version(
        &self,
        recipe_id: &str,
    ) -> StoreResult<Option<RecipeVersionRow>> {
        Ok(self
            .recipe_versions(recipe_id)
            .await?
            .into_iter()
            .rfind(|version| version.kind != "raw"))
    }

    pub async fn share_recipe(
        &self,
        recipe_id: &str,
        scope: &str,
        scope_id: &str,
        granted_by: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe_share (recipe_id, scope, scope_id, granted_by, granted_at_ms)
             values ($1, $2, $3, $4, $5)
             on conflict (recipe_id, scope, scope_id) do update set
               granted_by = excluded.granted_by, granted_at_ms = excluded.granted_at_ms,
               accepted_at_ms = null, declined_at_ms = null",
        )
        .bind(recipe_id)
        .bind(scope)
        .bind(scope_id)
        .bind(granted_by)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn unshare_recipe(
        &self,
        recipe_id: &str,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "delete from recipe_share where recipe_id = $1 and scope = $2 and scope_id = $3",
        )
        .bind(recipe_id)
        .bind(scope)
        .bind(scope_id)
        .execute(&self.pool)
        .await?;
        // A person who accepted through the org keeps nothing once the org share is withdrawn.
        if scope == "org" {
            sqlx::query(
                "delete from recipe_share where recipe_id = $1 and scope = 'account' and granted_by = 'org:' || $2",
            )
            .bind(recipe_id)
            .bind(scope_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// The share rows of a recipe, for its owner's view.
    pub async fn recipe_shares(&self, recipe_id: &str) -> StoreResult<Vec<RecipeShareRow>> {
        let rows = sqlx::query(
            "select recipe_id, scope, scope_id, granted_by, granted_at_ms, accepted_at_ms, declined_at_ms
               from recipe_share where recipe_id = $1 order by granted_at_ms",
        )
        .bind(recipe_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RecipeShareRow {
                    recipe_id: row.try_get("recipe_id")?,
                    scope: row.try_get("scope")?,
                    scope_id: row.try_get("scope_id")?,
                    granted_by: row.try_get("granted_by")?,
                    granted_at_ms: row.try_get("granted_at_ms")?,
                    accepted_at_ms: row.try_get("accepted_at_ms")?,
                    declined_at_ms: row.try_get("declined_at_ms")?,
                })
            })
            .collect()
    }

    /// A person answers a share. Accepting through an org share writes their own account row
    /// (`granted_by = "org:<org>"`) so the answer is theirs alone.
    pub async fn answer_recipe_share(
        &self,
        recipe_id: &str,
        account_id: &str,
        org_id: Option<&str>,
        accept: bool,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let direct = sqlx::query(
            "select granted_by from recipe_share where recipe_id = $1 and scope = 'account' and scope_id = $2",
        )
        .bind(recipe_id)
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        // The org that answered, not merely whether one did: the insert below stamps it into
        // `granted_by`, and re-deriving it there is how `unwrap_or("")` got in. `scope_id <> ''`
        // neutralises a row an older build may already have written with an empty org.
        let via_org: Option<&str> = match org_id.filter(|org| !org.is_empty()) {
            Some(org) => sqlx::query(
                "select 1 from recipe_share
                  where recipe_id = $1 and scope = 'org' and scope_id <> '' and scope_id = $2",
            )
            .bind(recipe_id)
            .bind(org)
            .fetch_optional(&self.pool)
            .await?
            .map(|_| org),
            None => None,
        };
        if direct.is_none() && via_org.is_none() {
            return Ok(false);
        }
        let (accepted, declined) = if accept {
            (Some(at_ms), None::<i64>)
        } else {
            (None, Some(at_ms))
        };
        if direct.is_some() {
            sqlx::query(
                "update recipe_share set accepted_at_ms = $3, declined_at_ms = $4
                  where recipe_id = $1 and scope = 'account' and scope_id = $2",
            )
            .bind(recipe_id)
            .bind(account_id)
            .bind(accepted)
            .bind(declined)
            .execute(&self.pool)
            .await?;
        } else if let Some(org) = via_org {
            sqlx::query(
                "insert into recipe_share (recipe_id, scope, scope_id, granted_by, granted_at_ms, accepted_at_ms, declined_at_ms)
                 values ($1, 'account', $2, 'org:' || $3, $4, $5, $6)
                 on conflict (recipe_id, scope, scope_id) do update set
                   accepted_at_ms = excluded.accepted_at_ms, declined_at_ms = excluded.declined_at_ms",
            )
            .bind(recipe_id)
            .bind(account_id)
            .bind(org)
            .bind(at_ms)
            .bind(accepted)
            .bind(declined)
            .execute(&self.pool)
            .await?;
        }
        Ok(true)
    }

    /// Whether this person has accepted a share of the recipe (directly or through their org).
    pub async fn recipe_accepted_by(&self, recipe_id: &str, account_id: &str) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 from recipe_share where recipe_id = $1 and scope = 'account' and scope_id = $2
              and accepted_at_ms is not null",
        )
        .bind(recipe_id)
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn grant_recipe(
        &self,
        recipe_id: &str,
        coworker_id: &str,
        granted_by: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe_grant (recipe_id, coworker_id, granted_by, granted_at_ms)
             values ($1, $2, $3, $4) on conflict (recipe_id, coworker_id) do nothing",
        )
        .bind(recipe_id)
        .bind(coworker_id)
        .bind(granted_by)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn revoke_recipe_grant(&self, recipe_id: &str, coworker_id: &str) -> StoreResult<()> {
        sqlx::query("delete from recipe_grant where recipe_id = $1 and coworker_id = $2")
            .bind(recipe_id)
            .bind(coworker_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recipe_grants(&self, recipe_id: &str) -> StoreResult<Vec<RecipeGrantRow>> {
        let rows = sqlx::query(
            "select recipe_id, coworker_id, granted_by, granted_at_ms from recipe_grant
              where recipe_id = $1 order by granted_at_ms",
        )
        .bind(recipe_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RecipeGrantRow {
                    recipe_id: row.try_get("recipe_id")?,
                    coworker_id: row.try_get("coworker_id")?,
                    granted_by: row.try_get("granted_by")?,
                    granted_at_ms: row.try_get("granted_at_ms")?,
                })
            })
            .collect()
    }

    /// The recipes a bot may run: granted, and not deleted.
    pub async fn recipes_granted_to(&self, coworker_id: &str) -> StoreResult<Vec<RecipeRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT} join recipe_grant g on g.recipe_id = r.id
             where g.coworker_id = $1 and r.deleted_at_ms is null order by r.name"
        )))
        .bind(coworker_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(recipe_row).collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_recipe_run(
        &self,
        id: &str,
        recipe_id: &str,
        version: i32,
        coworker_id: &str,
        run_id: Option<&str>,
        ok: bool,
        stopped_at: Option<i32>,
        receipt: &serde_json::Value,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe_run (id, recipe_id, version, coworker_id, run_id, ok, stopped_at, receipt, at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(recipe_id)
        .bind(version)
        .bind(coworker_id)
        .bind(run_id)
        .bind(ok)
        .bind(stopped_at)
        .bind(receipt)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drop all but the newest `keep` runs of one version. Called after a run is written, so a
    /// long-lived recipe cannot grow an unbounded history nobody reads.
    /// Drop the artifacts of runs that are no longer there.
    ///
    /// Called after pruning, because an artifact outliving its run is a megabyte nobody can
    /// reach: history is read a run at a time, and a row pointing at a run that was pruned has
    /// no page to appear on. Soft, like every other delete here.
    pub async fn orphan_artifacts_of_gone_runs(
        &self,
        recipe_id: &str,
        at_ms: i64,
    ) -> StoreResult<u64> {
        let done = sqlx::query(
            "update artifact set deleted_at_ms = $2
              where recipe_id = $1 and deleted_at_ms is null and run_id is not null
                and run_id not in (select id from recipe_run where recipe_id = $1)",
        )
        .bind(recipe_id)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    pub async fn prune_recipe_runs(
        &self,
        recipe_id: &str,
        version: i32,
        keep: i64,
    ) -> StoreResult<u64> {
        let done = sqlx::query(
            "delete from recipe_run
              where recipe_id = $1 and version = $2
                and id not in (
                    select id from recipe_run
                     where recipe_id = $1 and version = $2
                     order by at_ms desc
                     limit $3
                )",
        )
        .bind(recipe_id)
        .bind(version)
        .bind(keep)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    pub async fn recipe_runs(&self, recipe_id: &str, limit: i64) -> StoreResult<Vec<RecipeRunRow>> {
        let rows = sqlx::query(
            "select id, recipe_id, version, coworker_id, run_id, ok, stopped_at, receipt, at_ms
               from recipe_run where recipe_id = $1 order by at_ms desc limit $2",
        )
        .bind(recipe_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RecipeRunRow {
                    id: row.try_get("id")?,
                    recipe_id: row.try_get("recipe_id")?,
                    version: row.try_get("version")?,
                    coworker_id: row.try_get("coworker_id")?,
                    run_id: row.try_get("run_id")?,
                    ok: row.try_get("ok")?,
                    stopped_at: row.try_get("stopped_at")?,
                    receipt: row.try_get("receipt")?,
                    at_ms: row.try_get("at_ms")?,
                })
            })
            .collect()
    }
}

/// An artifact (screenshot, recording, or file attachment) as a listing shows it.
/// Does not carry the bytes themselves — a listing must never carry megabytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRow {
    pub id: String,
    pub account_id: String,
    pub kind: String,
    pub mime: String,
    pub filename: String,
    pub size_bytes: i64,
    pub recipe_id: Option<String>,
    pub run_id: Option<String>,
    pub step_index: Option<i32>,
    pub thread_id: Option<String>,
    pub meta: serde_json::Value,
    pub created_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}

/// Decode one artifact row from the database, excluding bytes.
fn artifact_row(row: &sqlx::postgres::PgRow) -> StoreResult<ArtifactRow> {
    Ok(ArtifactRow {
        id: row.try_get("id")?,
        account_id: row.try_get("account_id")?,
        kind: row.try_get("kind")?,
        mime: row.try_get("mime")?,
        filename: row.try_get("filename")?,
        size_bytes: row.try_get("size_bytes")?,
        recipe_id: row.try_get("recipe_id")?,
        run_id: row.try_get("run_id")?,
        step_index: row.try_get("step_index")?,
        thread_id: row.try_get("thread_id")?,
        meta: row.try_get("meta")?,
        created_at_ms: row.try_get("created_at_ms")?,
        deleted_at_ms: row.try_get("deleted_at_ms")?,
    })
}

/// Artifacts: images, recordings, and files.
impl PgStore {
    /// Store an artifact's row and bytes together.
    ///
    /// Refuses an empty slice. Writes size_bytes from the actual byte length rather than trusting
    /// the caller.
    pub async fn put_artifact(&self, row: &ArtifactRow, bytes: &[u8]) -> StoreResult<()> {
        if bytes.is_empty() {
            return Err(StoreError::Database(
                "artifact bytes cannot be empty".to_string(),
            ));
        }

        sqlx::query(
            "insert into artifact
               (id, account_id, kind, mime, filename, size_bytes, bytes, recipe_id, run_id,
                step_index, thread_id, meta, created_at_ms, deleted_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             on conflict (id) do update set
               account_id = excluded.account_id,
               kind = excluded.kind,
               mime = excluded.mime,
               filename = excluded.filename,
               size_bytes = excluded.size_bytes,
               bytes = excluded.bytes,
               recipe_id = excluded.recipe_id,
               run_id = excluded.run_id,
               step_index = excluded.step_index,
               thread_id = excluded.thread_id,
               meta = excluded.meta,
               deleted_at_ms = excluded.deleted_at_ms",
        )
        .bind(&row.id)
        .bind(&row.account_id)
        .bind(&row.kind)
        .bind(&row.mime)
        .bind(&row.filename)
        .bind(bytes.len() as i64)
        .bind(bytes)
        .bind(&row.recipe_id)
        .bind(&row.run_id)
        .bind(row.step_index)
        .bind(&row.thread_id)
        .bind(&row.meta)
        .bind(row.created_at_ms)
        .bind(row.deleted_at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One artifact by id, without bytes. Ignores rows whose deleted_at_ms is set.
    pub async fn artifact(&self, id: &str) -> StoreResult<Option<ArtifactRow>> {
        let row = sqlx::query(
            "select id, account_id, kind, mime, filename, size_bytes, recipe_id, run_id,
                    step_index, thread_id, meta, created_at_ms, deleted_at_ms
               from artifact where id = $1 and deleted_at_ms is null",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(artifact_row).transpose()
    }

    /// One artifact by id, with bytes. Ignores rows whose deleted_at_ms is set.
    /// This is the ONLY read that selects the bytes column so a megabyte never rides along
    /// on a listing.
    pub async fn artifact_bytes(&self, id: &str) -> StoreResult<Option<(ArtifactRow, Vec<u8>)>> {
        let row = sqlx::query(
            "select id, account_id, kind, mime, filename, size_bytes, bytes, recipe_id, run_id,
                    step_index, thread_id, meta, created_at_ms, deleted_at_ms
               from artifact where id = $1 and deleted_at_ms is null",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            let bytes: Vec<u8> = row.try_get("bytes")?;
            // Decode the row without the bytes column.
            let artifact = ArtifactRow {
                id: row.try_get("id")?,
                account_id: row.try_get("account_id")?,
                kind: row.try_get("kind")?,
                mime: row.try_get("mime")?,
                filename: row.try_get("filename")?,
                size_bytes: row.try_get("size_bytes")?,
                recipe_id: row.try_get("recipe_id")?,
                run_id: row.try_get("run_id")?,
                step_index: row.try_get("step_index")?,
                thread_id: row.try_get("thread_id")?,
                meta: row.try_get("meta")?,
                created_at_ms: row.try_get("created_at_ms")?,
                deleted_at_ms: row.try_get("deleted_at_ms")?,
            };
            Ok((artifact, bytes))
        })
        .transpose()
    }

    /// Every artifact produced by a recipe run, ordered by step then creation time.
    /// Ignores rows whose deleted_at_ms is set.
    pub async fn artifacts_for_run(
        &self,
        recipe_id: &str,
        run_id: &str,
    ) -> StoreResult<Vec<ArtifactRow>> {
        let rows = sqlx::query(
            "select id, account_id, kind, mime, filename, size_bytes, recipe_id, run_id,
                    step_index, thread_id, meta, created_at_ms, deleted_at_ms
               from artifact
              where recipe_id = $1 and run_id = $2 and deleted_at_ms is null
              order by step_index, created_at_ms",
        )
        .bind(recipe_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(artifact_row).collect()
    }

    /// Every artifact attached to a thread, without bytes. Ignores rows whose deleted_at_ms is set.
    pub async fn artifacts_for_thread(
        &self,
        account_id: &str,
        thread_id: &str,
    ) -> StoreResult<Vec<ArtifactRow>> {
        let rows = sqlx::query(
            "select id, account_id, kind, mime, filename, size_bytes, recipe_id, run_id,
                    step_index, thread_id, meta, created_at_ms, deleted_at_ms
               from artifact
              where account_id = $1 and thread_id = $2 and deleted_at_ms is null",
        )
        .bind(account_id)
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(artifact_row).collect()
    }

    /// Soft-delete an artifact by setting deleted_at_ms. Idempotent.
    pub async fn soft_delete_artifact(&self, id: &str, at_ms: i64) -> StoreResult<()> {
        sqlx::query("update artifact set deleted_at_ms = $2 where id = $1")
            .bind(id)
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// One saved site login as the settings list shows it. Never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteLoginRow {
    pub id: String,
    pub origin: String,
    pub username: String,
    pub label: String,
    /// `password`, `code` (an authenticator code with no password) or `passkey`.
    pub kind: String,
    pub notes: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_used_at_ms: Option<i64>,
    /// A passkey row's public half: the credential id and user handle (base64) and the
    /// relying party. The key itself is sealed apart.
    pub passkey: Option<PasskeyMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasskeyMeta {
    pub credential_id_b64: String,
    pub rp_id: String,
    pub user_handle_b64: String,
}

/// What a save carries. A secret given as `None` is left as it was.
pub struct SiteLoginWrite<'a> {
    pub origin: &'a str,
    pub username: &'a str,
    pub label: &'a str,
    pub kind: &'a str,
    pub notes: &'a str,
    pub password: Option<&'a str>,
    pub otpauth: Option<&'a str>,
    pub passkey: Option<PasskeyWrite>,
}

/// A passkey to file: its public half and its private key (PKCS#8, base64).
#[derive(Clone, PartialEq, Eq)]
pub struct PasskeyWrite {
    pub credential_id_b64: String,
    pub rp_id: String,
    pub user_handle_b64: String,
    pub private_key_b64: String,
}

impl std::fmt::Debug for PasskeyWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyWrite")
            .field("rp_id", &self.rp_id)
            .field("private_key_b64", &"<redacted>")
            .finish()
    }
}

/// The secrets of one row, opened for the owner's app. Debug never shows them.
#[derive(Clone, PartialEq, Eq)]
pub struct SiteLoginSecrets {
    pub password: Option<String>,
    pub otpauth: Option<String>,
    pub passkey_key: Option<String>,
}

impl std::fmt::Debug for SiteLoginSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteLoginSecrets")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("otpauth", &self.otpauth.as_ref().map(|_| "<redacted>"))
            .field(
                "passkey_key",
                &self.passkey_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

fn site_login_row(row: sqlx::postgres::PgRow) -> StoreResult<SiteLoginRow> {
    Ok(SiteLoginRow {
        id: row.try_get("id")?,
        origin: row.try_get("origin")?,
        username: row.try_get("username")?,
        label: row.try_get("label")?,
        kind: row.try_get("kind")?,
        notes: row.try_get("notes")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        last_used_at_ms: row.try_get("last_used_at_ms")?,
        passkey: match (
            row.try_get::<Option<String>, _>("passkey_credential_id")?,
            row.try_get::<Option<String>, _>("passkey_rp_id")?,
            row.try_get::<Option<String>, _>("passkey_user_handle")?,
        ) {
            (Some(credential_id_b64), Some(rp_id), Some(user_handle_b64)) => Some(PasskeyMeta {
                credential_id_b64,
                rp_id,
                user_handle_b64,
            }),
            _ => None,
        },
    })
}
