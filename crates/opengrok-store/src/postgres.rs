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
               -- The owner is set once and never overwritten with NULL: a later batch that arrives
               -- without a session must not orphan a run somebody owns.
               account_id = coalesce(excluded.account_id, run_view.account_id),
               -- The start is the first append's stamp, kept for good.
               started_at_ms = coalesce(run_view.started_at_ms, excluded.started_at_ms)",
        )
        .bind(view.id.as_str())
        .bind(&view.thread_id)
        .bind(match view.status {
            RunStatus::Running => "running",
            RunStatus::AwaitingApproval => "awaiting-approval",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
        })
        .bind(view.event_count)
        .bind(view.updated_at_ms)
        .bind(account_id.map(|id| id.as_str()))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
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
        rows.into_iter()
            .map(|row| {
                let updated_at_ms: i64 = row.try_get("updated_at_ms")?;
                Ok(ThreadRun {
                    id: RunId::from_stored(row.try_get::<String, _>("id")?),
                    status: row.try_get("status")?,
                    started_at_ms: row
                        .try_get::<Option<i64>, _>("started_at_ms")?
                        .unwrap_or(updated_at_ms),
                    updated_at_ms,
                })
            })
            .collect()
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
                (id, account_id, name, model, box_id, retired, updated_at_ms, members, role)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             on conflict (id) do update set
               name = excluded.name,
               model = excluded.model,
               box_id = excluded.box_id,
               retired = excluded.retired,
               updated_at_ms = excluded.updated_at_ms,
               members = excluded.members,
               role = excluded.role",
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
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// The roster, newest first — the order the client sorts by.
    pub async fn coworkers_for(&self, account_id: &AccountId) -> StoreResult<Vec<CoworkerView>> {
        let rows = sqlx::query(
            "select id, name, model, box_id, retired, updated_at_ms, members, role
             from coworker_view
             where account_id = $1 and retired = false
             order by updated_at_ms desc",
        )
        .bind(account_id.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(CoworkerView {
                    id: CoworkerId::from_stored(row.try_get::<String, _>("id")?),
                    name: row.try_get("name")?,
                    model: row.try_get("model")?,
                    box_id: row
                        .try_get::<Option<String>, _>("box_id")?
                        .map(BoxId::from_stored),
                    retired: row.try_get("retired")?,
                    updated_at_ms: row.try_get("updated_at_ms")?,
                    members: serde_json::from_value(
                        row.try_get::<serde_json::Value, _>("members")?,
                    )
                    .map_err(|error| StoreError::Corrupt(error.to_string()))?,
                    role: row.try_get("role")?,
                })
            })
            .collect()
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
            sqlx::query(
                "insert into secret_store (id, nonce, ciphertext, updated_at_ms)
                 values ($1, $2, $3, $4)
                 on conflict (id) do update set
                   nonce = excluded.nonce,
                   ciphertext = excluded.ciphertext,
                   updated_at_ms = excluded.updated_at_ms",
            )
            .bind(id)
            .bind(&sealed.nonce)
            .bind(&sealed.ciphertext)
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
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

    /// Store a sealed secret on its own, outside a connection's transaction.
    ///
    /// Used for the refresh token, which lives in its own row: it outlives the access token, and
    /// keeping them apart means rotating one does not disturb the other.
    /// Drop a sealed secret by id. Idempotent: a secret already gone is the outcome asked for.
    pub async fn delete_secret(&self, id: &str) -> StoreResult<()> {
        sqlx::query("delete from secret_store where id = $1")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn put_secret(&self, id: &str, sealed: &Sealed, at_ms: i64) -> StoreResult<()> {
        sqlx::query(
            "insert into secret_store (id, nonce, ciphertext, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (id) do update set
               nonce = excluded.nonce,
               ciphertext = excluded.ciphertext,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(id)
        .bind(&sealed.nonce)
        .bind(&sealed.ciphertext)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Open a connection's credential. The one place a token is ever in plaintext.
    pub async fn open_credential(&self, vault: &Vault, id: &str) -> StoreResult<Option<String>> {
        let row = sqlx::query("select nonce, ciphertext from secret_store where id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        row.map(|row| {
            let sealed = Sealed {
                nonce: row.try_get("nonce")?,
                ciphertext: row.try_get("ciphertext")?,
            };
            vault.open(id, &sealed)
        })
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
        sqlx::query(
            "insert into secret_store (id, nonce, ciphertext, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (id) do update set
               nonce = excluded.nonce,
               ciphertext = excluded.ciphertext,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(&id)
        .bind(&sealed.nonce)
        .bind(&sealed.ciphertext)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
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

    /// The account's last provisioning error as (code, message), or None when it has none.
    pub async fn account_computer_error(
        &self,
        account_id: &str,
    ) -> StoreResult<Option<(String, String)>> {
        let row =
            sqlx::query("select code, message from account_computer_error where account_id = $1")
                .bind(account_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("code")?,
                row.try_get::<String, _>("message")?,
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
