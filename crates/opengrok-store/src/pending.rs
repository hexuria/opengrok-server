//! Durable pending user messages: NativeChat follow-ups that have not yet become a run.
//!
//! Mutable rows, not an event stream. Cancel is a delete and edit is an update because the
//! product is "this send must never fire" / "fire this text instead", and an append-only log
//! of those two would still need a projection that looks exactly like this table. WHO may
//! touch a row is decided in the server, the same way skills are: every function here takes an
//! account the caller has already authenticated, never one that arrived in a body.

use opengrok_core::id::AccountId;
use serde_json::Value;
use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

/// One queued send, as the store keeps it. `status` is `pending` or `drained`; cancelled rows
/// are deleted rather than kept, so a bubble can be queued again after the person takes it back.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingUserMessageRow {
    pub id: String,
    pub thread_id: String,
    pub account_id: String,
    pub content: String,
    pub reply_to: Option<Value>,
    pub recipe_id: Option<String>,
    pub recipe_values: Option<Value>,
    pub skill_id: Option<String>,
    pub client_message_id: Option<String>,
    pub status: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub drained_at_ms: Option<i64>,
    pub drained_run_id: Option<String>,
}

const PENDING_SELECT: &str = "select id, thread_id, account_id, content, reply_to, recipe_id,
        recipe_values, skill_id, client_message_id, status, created_at_ms, updated_at_ms,
        drained_at_ms, drained_run_id
   from pending_user_message";

fn pending_row(row: &sqlx::postgres::PgRow) -> StoreResult<PendingUserMessageRow> {
    Ok(PendingUserMessageRow {
        id: row.try_get("id")?,
        thread_id: row.try_get("thread_id")?,
        account_id: row.try_get("account_id")?,
        content: row.try_get("content")?,
        reply_to: row.try_get("reply_to")?,
        recipe_id: row.try_get("recipe_id")?,
        recipe_values: row.try_get("recipe_values")?,
        skill_id: row.try_get("skill_id")?,
        client_message_id: row.try_get("client_message_id")?,
        status: row.try_get("status")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        drained_at_ms: row.try_get("drained_at_ms")?,
        drained_run_id: row.try_get("drained_run_id")?,
    })
}

/// What a create writes, minus timestamps and status. Gathered so `account_id` and `thread_id`
/// cannot be swapped at a call site — both are `&str` and either permutation would compile.
#[derive(Debug, Clone)]
pub struct NewPendingUserMessage<'a> {
    pub id: &'a str,
    pub thread_id: &'a str,
    pub account_id: &'a str,
    pub content: &'a str,
    pub reply_to: Option<&'a Value>,
    pub recipe_id: Option<&'a str>,
    pub recipe_values: Option<&'a Value>,
    pub skill_id: Option<&'a str>,
    pub client_message_id: Option<&'a str>,
}

/// Fields an edit may change. `None` keeps the stored value; `Some(None)` clears an optional.
#[derive(Debug, Clone)]
pub struct PendingUserMessagePatch<'a> {
    pub content: Option<&'a str>,
    pub reply_to: Option<Option<&'a Value>>,
    pub recipe_id: Option<Option<&'a str>>,
    pub recipe_values: Option<Option<&'a Value>>,
    pub skill_id: Option<Option<&'a str>>,
}

/// Create against a client message id: return the live row, or refuse to resurrect a drained one.
#[derive(Debug, Clone, PartialEq)]
pub enum EnqueueResult {
    Created(PendingUserMessageRow),
    Existing(PendingUserMessageRow),
    AlreadyConsumed(PendingUserMessageRow),
}

/// Take a pending row for a turn. `AlreadyThisRun` is a retried `POST /ag-ui` with the same
/// run id: the first request already consumed it, and starting the turn again is the existing
/// AG-UI retry, not a second fire of the queue. `Stale` is a send whose words or options differ
/// from the row it names, whether still pending or drained by this run; the row is untouched.
#[derive(Debug, Clone, PartialEq)]
pub enum DrainResult {
    Drained(PendingUserMessageRow),
    AlreadyThisRun(PendingUserMessageRow),
    AlreadyConsumed(PendingUserMessageRow),
    Stale(PendingUserMessageRow),
    Missing,
}

impl PgStore {
    /// Whether this account has ever owned a run on this thread — including hidden ones.
    ///
    /// Hidden is still ownership: a person who deleted every turn still owns the conversation,
    /// and must still be able to cancel a follow-up they queued before they hid it. A thread
    /// they have never run is "no such thread", the same answer `GET /ag-ui/threads/{id}` gives.
    pub async fn account_owns_thread(
        &self,
        thread_id: &str,
        account: &AccountId,
    ) -> StoreResult<bool> {
        let found: Option<i32> = sqlx::query_scalar(
            "select 1 from run_view where thread_id = $1 and account_id = $2 limit 1",
        )
        .bind(thread_id)
        .bind(account.as_str())
        .fetch_optional(self.pool())
        .await?;
        Ok(found.is_some())
    }

    /// Live follow-ups for this account on this thread, oldest first — queue order.
    pub async fn pending_user_messages(
        &self,
        thread_id: &str,
        account: &AccountId,
    ) -> StoreResult<Vec<PendingUserMessageRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{PENDING_SELECT} where thread_id = $1 and account_id = $2 and status = 'pending'
              order by created_at_ms, id"
        )))
        .bind(thread_id)
        .bind(account.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(pending_row).collect()
    }

    /// One row this account owns, whatever its status. `None` for another account's id.
    pub async fn pending_user_message(
        &self,
        id: &str,
        account: &AccountId,
    ) -> StoreResult<Option<PendingUserMessageRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{PENDING_SELECT} where id = $1 and account_id = $2"
        )))
        .bind(id)
        .bind(account.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(pending_row).transpose()
    }

    /// Insert, or return the row the client message id already names.
    ///
    /// THE UNIQUE INDEX IS THE IDEMPOTENCY. Two replicas posting the same bubble both insert;
    /// one lands and the other reads it. A drained row is not updated: resurrecting it would
    /// be a second fire of a send that already became a run.
    pub async fn enqueue_pending_user_message(
        &self,
        new: NewPendingUserMessage<'_>,
        at_ms: i64,
    ) -> StoreResult<EnqueueResult> {
        let inserted = sqlx::query(
            "insert into pending_user_message (
                id, thread_id, account_id, content, reply_to, recipe_id, recipe_values,
                skill_id, client_message_id, status, created_at_ms, updated_at_ms
             ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', $10, $10)
             on conflict (account_id, thread_id, client_message_id)
                where client_message_id is not null
             do nothing
             returning id, thread_id, account_id, content, reply_to, recipe_id, recipe_values,
                       skill_id, client_message_id, status, created_at_ms, updated_at_ms,
                       drained_at_ms, drained_run_id",
        )
        .bind(new.id)
        .bind(new.thread_id)
        .bind(new.account_id)
        .bind(new.content)
        .bind(new.reply_to.cloned())
        .bind(new.recipe_id)
        .bind(new.recipe_values.cloned())
        .bind(new.skill_id)
        .bind(new.client_message_id)
        .bind(at_ms)
        .fetch_optional(self.pool())
        .await?;
        if let Some(row) = inserted.as_ref() {
            return Ok(EnqueueResult::Created(pending_row(row)?));
        }
        let Some(client_message_id) = new.client_message_id else {
            // No client id, so the partial unique index did not apply: this is a primary-key
            // collision on an id we minted, which is a lost race, not an idempotent retry.
            return Err(crate::StoreError::Conflict);
        };
        let existing = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{PENDING_SELECT} where account_id = $1 and thread_id = $2 and client_message_id = $3"
        )))
        .bind(new.account_id)
        .bind(new.thread_id)
        .bind(client_message_id)
        .fetch_optional(self.pool())
        .await?;
        match existing.as_ref().map(pending_row).transpose()? {
            Some(row) if row.status == "pending" => Ok(EnqueueResult::Existing(row)),
            Some(row) => Ok(EnqueueResult::AlreadyConsumed(row)),
            None => {
                // Conflict then gone: the winner cancelled between our insert and this read.
                // Retry once as a fresh insert of a new id would be a different bubble; tell
                // the caller to POST again by surfacing conflict.
                Err(crate::StoreError::Conflict)
            }
        }
    }

    /// Edit a live follow-up. `None` when it is not this account's pending row — drained,
    /// cancelled, or somebody else's, which must all read as no such message.
    ///
    /// ONE STATEMENT, AND EACH OMITTED FIELD IS READ FROM THE ROW INSIDE IT. A read in Rust and a
    /// write of every field put back whatever the read saw, so two edits to different fields
    /// lost one of them. Here a waiting writer re-reads the row the winner committed.
    pub async fn update_pending_user_message(
        &self,
        id: &str,
        account: &AccountId,
        thread_id: &str,
        patch: PendingUserMessagePatch<'_>,
        at_ms: i64,
    ) -> StoreResult<Option<PendingUserMessageRow>> {
        let updated = sqlx::query(
            "update pending_user_message
                set content = coalesce($4, content),
                    reply_to = case when $5 then $6 else reply_to end,
                    recipe_id = case when $7 then $8 else recipe_id end,
                    recipe_values = case when $9 then $10 else recipe_values end,
                    skill_id = case when $11 then $12 else skill_id end,
                    updated_at_ms = $13
              where id = $1 and account_id = $2 and thread_id = $3 and status = 'pending'
              returning id, thread_id, account_id, content, reply_to, recipe_id, recipe_values,
                        skill_id, client_message_id, status, created_at_ms, updated_at_ms,
                        drained_at_ms, drained_run_id",
        )
        .bind(id)
        .bind(account.as_str())
        .bind(thread_id)
        .bind(patch.content)
        .bind(patch.reply_to.is_some())
        .bind(patch.reply_to.flatten().cloned())
        .bind(patch.recipe_id.is_some())
        .bind(patch.recipe_id.flatten())
        .bind(patch.recipe_values.is_some())
        .bind(patch.recipe_values.flatten().cloned())
        .bind(patch.skill_id.is_some())
        .bind(patch.skill_id.flatten())
        .bind(at_ms)
        .fetch_optional(self.pool())
        .await?;
        updated.as_ref().map(pending_row).transpose()
    }

    /// Cancel. `true` when a pending row was deleted. Drained and missing both return `false`,
    /// which the HTTP layer maps to the same 200 + `op: canceled` — cancel is idempotent.
    pub async fn delete_pending_user_message(
        &self,
        id: &str,
        account: &AccountId,
        thread_id: &str,
    ) -> StoreResult<bool> {
        let done = sqlx::query(
            "delete from pending_user_message
              where id = $1 and account_id = $2 and thread_id = $3 and status = 'pending'",
        )
        .bind(id)
        .bind(account.as_str())
        .bind(thread_id)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Compare and consume under the same row lock; stale clients cannot fire edited sends.
    pub async fn drain_pending_user_message(
        &self,
        id: &str,
        account: &AccountId,
        thread_id: &str,
        run_id: &str,
        at_ms: i64,
        matches: impl FnOnce(&PendingUserMessageRow) -> bool + Send,
    ) -> StoreResult<DrainResult> {
        self.checked_drain(DrainKey::Id(id), account, thread_id, run_id, at_ms, matches)
            .await
    }

    /// A missing client bubble is an ordinary, unqueued turn.
    pub async fn drain_pending_user_message_by_client_id(
        &self,
        client_message_id: &str,
        account: &AccountId,
        thread_id: &str,
        run_id: &str,
        at_ms: i64,
        matches: impl FnOnce(&PendingUserMessageRow) -> bool + Send,
    ) -> StoreResult<DrainResult> {
        self.checked_drain(
            DrainKey::ClientMessageId(client_message_id),
            account,
            thread_id,
            run_id,
            at_ms,
            matches,
        )
        .await
    }

    async fn checked_drain(
        &self,
        key: DrainKey<'_>,
        account: &AccountId,
        thread_id: &str,
        run_id: &str,
        at_ms: i64,
        matches: impl FnOnce(&PendingUserMessageRow) -> bool + Send,
    ) -> StoreResult<DrainResult> {
        let (column, value) = match key {
            DrainKey::Id(id) => ("id", id),
            DrainKey::ClientMessageId(id) => ("client_message_id", id),
        };
        let mut tx = self.pool().begin().await?;
        let current = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{PENDING_SELECT} where {column} = $1 and account_id = $2 and thread_id = $3
              for update"
        )))
        .bind(value)
        .bind(account.as_str())
        .bind(thread_id)
        .fetch_optional(&mut *tx)
        .await?;
        let outcome = match current.as_ref().map(pending_row).transpose()? {
            None => DrainResult::Missing,
            Some(row)
                if row.status != "pending" && row.drained_run_id.as_deref() != Some(run_id) =>
            {
                DrainResult::AlreadyConsumed(row)
            }
            Some(row) if !matches(&row) => DrainResult::Stale(row),
            Some(row) if row.status != "pending" => DrainResult::AlreadyThisRun(row),
            Some(row) => {
                let drained = sqlx::query(
                    "update pending_user_message
                        set status = 'drained', updated_at_ms = $2, drained_at_ms = $2,
                            drained_run_id = $3
                      where id = $1
                      returning id, thread_id, account_id, content, reply_to, recipe_id,
                                recipe_values, skill_id, client_message_id, status,
                                created_at_ms, updated_at_ms, drained_at_ms, drained_run_id",
                )
                .bind(&row.id)
                .bind(at_ms)
                .bind(run_id)
                .fetch_one(&mut *tx)
                .await?;
                DrainResult::Drained(pending_row(&drained)?)
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }
}

/// Which natural key names the row to drain. The column name is interpolated into SQL, so it
/// comes from this closed set and never from a request.
enum DrainKey<'a> {
    Id(&'a str),
    ClientMessageId(&'a str),
}
