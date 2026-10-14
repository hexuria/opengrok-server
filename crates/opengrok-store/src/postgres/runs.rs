//! Runs: the durable half of the promise that work survives a client.

use super::*;

/// One run as a routine's history lists it: the stored status word (`running`,
/// `awaiting-approval`, `finished`, `failed`), when it began, when it last moved.
#[derive(Debug, Clone)]
pub struct ThreadRun {
    pub id: RunId,
    pub status: String,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One conversation as `GET /ag-ui/threads` lists it (`PgStore::threads_owned_by`).
#[derive(Debug, Clone)]
pub struct ThreadListing {
    pub thread_id: String,
    /// The last run's; `None` for a turn with no coworker or one never started.
    pub coworker_id: Option<CoworkerId>,
    pub last_run_id: RunId,
    /// The stored status word, as `ThreadRun::status`.
    pub last_status: String,
    pub updated_at_ms: i64,
    /// The first started run's journaled prompt, verbatim; `None` when none was journaled.
    pub first_prompt: Option<Vec<serde_json::Value>>,
    /// The caller's own routine this thread is, and its name (a monitor has none). Another
    /// account's routine id used as a chat thread is `None`.
    pub routine: Option<(crate::FiredBy, String)>,
}

/// One `run_view` row as a `ThreadRun`, shared by the two readers of a thread's history so they
/// cannot drift in what they make of a row. `started_at_ms` is absent on runs projected before
/// that column existed, and the last time the run moved is the closest honest answer for those.
pub(super) fn thread_run_from_row(row: sqlx::postgres::PgRow) -> StoreResult<ThreadRun> {
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

/// Runs: the durable half of the promise that work survives a client.
impl PgStore {
    /// Replay a run's log.
    pub async fn load_run(&self, id: &RunId) -> StoreResult<(Run, i64)> {
        let (events, seq) = self.stream::<RunEvent>(&crate::run_stream(id)).await?;
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

        let owner = sqlx::query_scalar::<_, Option<String>>(
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
               started_at_ms = coalesce(run_view.started_at_ms, excluded.started_at_ms)
             returning account_id",
        )
        .bind(view.id.as_str())
        .bind(&view.thread_id)
        .bind(view.status.as_str())
        .bind(view.event_count)
        .bind(view.updated_at_ms)
        .bind(account_id.map(|id| id.as_str()))
        .fetch_one(&mut *tx)
        .await?;

        // LAST here and in `append_schedule`: the account's head lock is held to the commit.
        // The owner is the view's, not the call's.
        opengrok_events::run_appended(&mut tx, events, view, owner.as_deref()).await?;
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
        Ok(sqlx::query_scalar(
            "select id from run_view
             where thread_id = $1 and account_id = $2 and hidden_at_ms is not null
             order by hidden_at_ms desc",
        )
        .bind(thread_id)
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await?)
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
    /// Ordered by when each run BEGAN, unlike the tests' `runs_for_thread` (`test_only.rs`), which
    /// orders by when a run last moved, the latest firing at the top. A transcript is
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

    /// The caller's conversations, newest first — `GET /ag-ui/threads`.
    ///
    /// OWNER-FILTERED IN EVERY PART: on an org-shared coworker two members write the same
    /// `thread_id`, and a lateral without `account_id` hands one the other's last run or first
    /// words. The routine joins match the owner too, so a guessed routine id lends nothing.
    ///
    /// THE CURSOR IS ON THE THREAD, IN `having`: on each run it kept a thread's older runs past the
    /// cursor and listed the thread again. With no thread id, `''` makes it a plain "older than".
    ///
    /// `audit_prefix` is the MCP door's, passed in so the literal lives once; the exclusion is
    /// exact against the last run's own coworker, never a prefix (`mcp_door::is_mcp_audit_run`).
    pub async fn threads_owned_by(
        &self,
        account: &AccountId,
        coworker: Option<&CoworkerId>,
        before: Option<(i64, Option<&str>)>,
        audit_prefix: &str,
        limit: i64,
    ) -> StoreResult<Vec<ThreadListing>> {
        let rows = sqlx::query(
            "select page.*, first.prompt as first_prompt, coalesce(s.name, '') as routine_name,
                    case when m.id is not null then 'monitor' else s.kind end as routine
             from (
               select t.thread_id, t.updated_at_ms, last.id as last_run_id,
                      last.status as last_status, started.payload->>'coworker_id' as coworker_id
               from (
                 select thread_id, max(updated_at_ms) as updated_at_ms from run_view
                 where account_id = $1 and hidden_at_ms is null group by thread_id
                 having $2::bigint is null
                     or (max(updated_at_ms), thread_id) < ($2::bigint, coalesce($3::text, ''))
               ) t
               join (
                 select distinct on (thread_id) thread_id, id, status from run_view
                 where account_id = $1 and hidden_at_ms is null
                 order by thread_id, coalesce(started_at_ms, updated_at_ms) desc, id desc
               ) last on last.thread_id = t.thread_id
               left join lateral (select payload from events where stream_id = 'run/' || last.id
                 and stream_seq = 1 and event_type = 'run-started') started on true
               where ($4::text is null or started.payload->>'coworker_id' = $4::text)
                 and not coalesce(t.thread_id = $5::text || (started.payload->>'coworker_id'), false)
               order by t.updated_at_ms desc, t.thread_id desc limit $6
             ) page
             left join lateral (
               select e.payload->'prompt' as prompt from run_view f join events e
                 on e.stream_id = 'run/' || f.id and e.stream_seq = 1 and e.event_type = 'run-started'
               where f.thread_id = page.thread_id and f.account_id = $1 and f.hidden_at_ms is null
               order by coalesce(f.started_at_ms, f.updated_at_ms), f.id limit 1
             ) first on true
             left join schedule_view s on s.id = page.thread_id and s.account_id = $1
             left join monitor_view m on m.id = page.thread_id and m.account_id = $1
             order by page.updated_at_ms desc, page.thread_id desc",
        )
        .bind(account.as_str())
        .bind(before.map(|(at_ms, _)| at_ms))
        .bind(before.and_then(|(_, thread)| thread))
        .bind(coworker.map(CoworkerId::as_str))
        .bind(audit_prefix)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let name: String = row.try_get("routine_name")?;
                let routine = match row.try_get::<Option<String>, _>("routine")?.as_deref() {
                    Some("monitor") => Some(crate::FiredBy::Monitor),
                    Some("webhook") => Some(crate::FiredBy::Webhook),
                    Some(_) => Some(crate::FiredBy::Schedule),
                    None => None,
                };
                Ok(ThreadListing {
                    thread_id: row.try_get("thread_id")?,
                    coworker_id: row
                        .try_get::<Option<String>, _>("coworker_id")?
                        .map(CoworkerId::from_stored),
                    last_run_id: RunId::from_stored(row.try_get::<String, _>("last_run_id")?),
                    last_status: row.try_get("last_status")?,
                    updated_at_ms: row.try_get("updated_at_ms")?,
                    first_prompt: row
                        .try_get::<Option<serde_json::Value>, _>("first_prompt")?
                        .and_then(|prompt| prompt.as_array().cloned()),
                    routine: routine.map(|by| (by, name)),
                })
            })
            .collect()
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

    /// Parked runs whose last write falls in `(after_ms, before_ms]`, with whose they are. The
    /// form-hold deadline sweep reads one such window per tick across every account, so a run a
    /// person is slow to answer is looked at once, not on every tick for as long as it waits.
    pub async fn parked_between(
        &self,
        after_ms: i64,
        before_ms: i64,
    ) -> StoreResult<Vec<(RunId, AccountId)>> {
        let rows = sqlx::query(
            "select id, account_id from run_view
             where status = 'awaiting-approval' and account_id is not null
               and updated_at_ms > $1 and updated_at_ms <= $2
             order by updated_at_ms",
        )
        .bind(after_ms)
        .bind(before_ms)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    RunId::from_stored(row.try_get::<String, _>("id")?),
                    AccountId::from_stored(row.try_get::<String, _>("account_id")?),
                ))
            })
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
}
