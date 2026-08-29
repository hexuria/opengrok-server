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
            "insert into run_view (id, thread_id, status, event_count, updated_at_ms, account_id)
             values ($1, $2, $3, $4, $5, $6)
             on conflict (id) do update set
               thread_id = excluded.thread_id,
               status = excluded.status,
               event_count = excluded.event_count,
               updated_at_ms = excluded.updated_at_ms,
               -- The owner is set once and never overwritten with NULL: a later batch that arrives
               -- without a session must not orphan a run somebody owns.
               account_id = coalesce(excluded.account_id, run_view.account_id)",
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
    /// writing one.
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

        sqlx::query(
            "insert into coworker_view (id, account_id, name, model, box_id, retired, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7)
             on conflict (id) do update set
               name = excluded.name,
               model = excluded.model,
               box_id = excluded.box_id,
               retired = excluded.retired,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(view.id.as_str())
        .bind(account_id.as_str())
        .bind(&view.name)
        .bind(&view.model)
        .bind(view.box_id.as_ref().map(|id| id.as_str()))
        .bind(view.retired)
        .bind(view.updated_at_ms)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// The roster, newest first — the order the client sorts by.
    pub async fn coworkers_for(&self, account_id: &AccountId) -> StoreResult<Vec<CoworkerView>> {
        let rows = sqlx::query(
            "select id, name, model, box_id, retired, updated_at_ms
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
        secret: Option<&Sealed>,
        at_ms: i64,
    ) -> StoreResult<i64> {
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
               (id, connector, scope, owner_id, label, disconnected, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7)
             on conflict (id) do update set
               connector = excluded.connector,
               scope = excluded.scope,
               owner_id = excluded.owner_id,
               label = excluded.label,
               disconnected = excluded.disconnected,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(id)
        .bind(&state.connector)
        .bind(scope)
        .bind(owner_id)
        .bind(&state.label)
        .bind(state.disconnected)
        .bind(at_ms)
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
            "select v.id, v.connector, v.scope, v.owner_id, v.label, v.updated_at_ms
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
            "select id, connector, label, updated_at_ms from connection_view
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
            });
        }
        Ok(views)
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
}
