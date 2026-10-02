//! The `bot_message` outbox and the Bots' timeline rows (#314): one Bot's message to another of
//! its person's, written down before anything acts on it, and the `messaged` row the sender's main
//! chat shows, in the same transaction (`formal/tla/PairDelivery.tla`).
//!
//! A ROW IS A MESSAGE, AND ITS RUN IS NAMED WHEN IT IS WRITTEN. The receiver's turn is started
//! from the row by its run id, whose `Started` commits once, so however often a drain or the sweep
//! reaches for it, a message starts one run. Who sent it, its chain and its hop are columns, never
//! read back from words.

use opengrok_core::run::RunStatus;
use serde_json::Value;
use sqlx::Row as _;

use crate::StoreResult;
use crate::postgres::PgStore;

/// One person's sends at a time, so the caps count every row a concurrent send has written.
const SENDS_LOCK_CLASS: i32 = 0x4253_4E44;
/// One pair's claim at a time, so its look and its claim see one another's commits.
const PAIR_LOCK_CLASS: i32 = 0x5041_4952;

/// A message as the outbox keeps it. `state` is `queued` until a drain claims it, `started` after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotMessageRow {
    pub id: String,
    pub owner_id: String,
    pub sender_id: String,
    pub receiver_id: String,
    pub thread_id: String,
    pub sender_run_id: String,
    pub call_id: String,
    pub chain_id: String,
    pub hop: i32,
    pub body: String,
    pub run_id: String,
    pub state: String,
    pub created_at_ms: i64,
}

fn message_row(row: &sqlx::postgres::PgRow) -> StoreResult<BotMessageRow> {
    Ok(BotMessageRow {
        id: row.try_get("id")?,
        owner_id: row.try_get("owner_id")?,
        sender_id: row.try_get("sender_id")?,
        receiver_id: row.try_get("receiver_id")?,
        thread_id: row.try_get("thread_id")?,
        sender_run_id: row.try_get("sender_run_id")?,
        call_id: row.try_get("call_id")?,
        chain_id: row.try_get("chain_id")?,
        hop: row.try_get("hop")?,
        body: row.try_get("body")?,
        run_id: row.try_get("run_id")?,
        state: row.try_get("state")?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}

/// One receiver of a call: who, the pair thread, and the ids a new row would take.
pub struct Receiver<'a> {
    pub receiver_id: &'a str,
    pub thread_id: String,
    pub message_id: String,
    pub run_id: String,
}

/// One `message_bot` call, as the outbox writes it.
pub struct Send<'a> {
    pub owner_id: &'a str,
    pub sender_id: &'a str,
    pub sender_run_id: &'a str,
    pub call_id: &'a str,
    pub chain_id: &'a str,
    pub hop: i32,
    pub body: &'a str,
    pub to: &'a [Receiver<'a>],
    /// The `messaged` row for the sender's main chat, and its id.
    pub entry: (&'a str, &'a Value),
    /// At most this many messages in the chain, and in the owner's last hour, this call's counted.
    pub caps: (i64, i64),
    pub at_ms: i64,
}

impl PgStore {
    /// Write a call's messages and its `messaged` row, all or none: each receiver's row, in the
    /// order `to` names them and whether this call wrote it, or `None` when the caps refuse it.
    ///
    /// A CALL CARRIED OUT AGAIN WRITES NOTHING. Its rows are unique on (sender run, call,
    /// receiver), so it gets the rows the first time wrote, uncounted, and its row is the first
    /// one's (`PairDelivery_nokey`). The caps are read under the owner's lock, as its own
    /// statement first (`start_recipe_run` says why), so two sends at once both count each other.
    pub async fn enqueue_bot_messages(
        &self,
        send: &Send<'_>,
    ) -> StoreResult<Option<Vec<(BotMessageRow, bool)>>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("select pg_advisory_xact_lock($1, hashtext($2))")
            .bind(SENDS_LOCK_CLASS)
            .bind(send.owner_id)
            .execute(&mut *tx)
            .await?;
        let found =
            sqlx::query("select * from bot_message where sender_run_id = $1 and call_id = $2")
                .bind(send.sender_run_id)
                .bind(send.call_id)
                .fetch_all(&mut *tx)
                .await?;
        let found: Vec<BotMessageRow> =
            found.iter().map(message_row).collect::<StoreResult<_>>()?;
        let fresh = send
            .to
            .iter()
            .filter(|to| !found.iter().any(|row| row.receiver_id == to.receiver_id));
        let fresh: Vec<&Receiver<'_>> = fresh.collect();
        if !fresh.is_empty() {
            let (chain, hour): (i64, i64) = sqlx::query_as(
                "select count(*) filter (where chain_id = $2), count(*) filter (where created_at_ms > $3)
                   from bot_message where owner_id = $1",
            )
            .bind(send.owner_id)
            .bind(send.chain_id)
            .bind(send.at_ms - 3_600_000)
            .fetch_one(&mut *tx)
            .await?;
            let more = i64::try_from(fresh.len()).unwrap_or(i64::MAX);
            if chain + more > send.caps.0 || hour + more > send.caps.1 {
                return Ok(None);
            }
        }
        let mut rows = Vec::with_capacity(send.to.len());
        for to in send.to {
            if let Some(row) = found.iter().find(|row| row.receiver_id == to.receiver_id) {
                rows.push((row.clone(), false));
                continue;
            }
            let row = sqlx::query(
                "insert into bot_message (id, owner_id, sender_id, receiver_id, thread_id,
                     sender_run_id, call_id, chain_id, hop, body, run_id, created_at_ms)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) returning *",
            )
            .bind(&to.message_id)
            .bind(send.owner_id)
            .bind(send.sender_id)
            .bind(to.receiver_id)
            .bind(&to.thread_id)
            .bind(send.sender_run_id)
            .bind(send.call_id)
            .bind(send.chain_id)
            .bind(send.hop)
            .bind(send.body)
            .bind(&to.run_id)
            .bind(send.at_ms)
            .fetch_one(&mut *tx)
            .await?;
            rows.push((message_row(&row)?, true));
        }
        sqlx::query(
            "insert into timeline_view (id, coworker_id, source_key, entry, at_ms)
             values ($1, $2, $3, $4, $5) on conflict (source_key) do nothing",
        )
        .bind(send.entry.0)
        .bind(send.sender_id)
        .bind(format!("messaged/{}/{}", send.sender_run_id, send.call_id))
        .bind(send.entry.1)
        .bind(send.at_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(rows))
    }

    /// The message a run was started for, if a message started it: its sender, chain and hop.
    pub async fn bot_message_of_run(&self, run_id: &str) -> StoreResult<Option<BotMessageRow>> {
        let row = sqlx::query("select * from bot_message where run_id = $1")
            .bind(run_id)
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(message_row).transpose()
    }

    /// The pair's next message to start, claimed, or `None` while one of its runs is in flight
    /// (claimed and its run not ended: not begun, running, or parked on a card) or none waits.
    ///
    /// THE LOCK, AS ITS OWN STATEMENT, BEFORE THE LOOK. Without it two drains each find nothing
    /// in flight and claim in turn, and the pair has two runs at once (`PairDelivery_noclaim`).
    pub async fn claim_pair_message(
        &self,
        thread_id: &str,
        at_ms: i64,
    ) -> StoreResult<Option<BotMessageRow>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("select pg_advisory_xact_lock($1, hashtext($2))")
            .bind(PAIR_LOCK_CLASS)
            .bind(thread_id)
            .execute(&mut *tx)
            .await?;
        let ended =
            [RunStatus::Finished, RunStatus::Failed, RunStatus::Stopped].map(|s| s.as_str());
        let busy: Option<i32> = sqlx::query_scalar(
            "select 1 from bot_message m left join run_view v on v.id = m.run_id
              where m.thread_id = $1 and m.state = 'started'
                and (v.status is null or v.status <> all($2)) limit 1",
        )
        .bind(thread_id)
        .bind(&ended[..])
        .fetch_optional(&mut *tx)
        .await?;
        if busy.is_some() {
            return Ok(None);
        }
        let row = sqlx::query(
            "update bot_message set state = 'started', started_at_ms = $2
              where id = (select id from bot_message where thread_id = $1 and state = 'queued'
                          order by created_at_ms, id limit 1)
             returning *",
        )
        .bind(thread_id)
        .bind(at_ms)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.as_ref().map(message_row).transpose()
    }

    /// What the sweep has to do: every pair with a message waiting, and every claimed message
    /// whose run has not begun `grace_ms` after its claim, its drain having died or stalled. The
    /// longest waiting first, so a quiet pair is not left behind a busy batch (review of #325).
    pub async fn pairs_to_sweep(
        &self,
        at_ms: i64,
        grace_ms: i64,
    ) -> StoreResult<(Vec<String>, Vec<BotMessageRow>)> {
        let threads: Vec<String> = sqlx::query_scalar(
            "select thread_id from bot_message where state = 'queued' group by thread_id
              order by min(created_at_ms) limit 100",
        )
        .fetch_all(self.pool())
        .await?;
        let stalled = sqlx::query(
            "select m.* from bot_message m left join run_view v on v.id = m.run_id
              where m.state = 'started' and v.id is null and m.started_at_ms < $1
              order by m.started_at_ms limit 100",
        )
        .bind(at_ms - grace_ms)
        .fetch_all(self.pool())
        .await?;
        let stalled = stalled.iter().map(message_row);
        Ok((threads, stalled.collect::<StoreResult<_>>()?))
    }

    /// A Bot's timeline rows, its newest `limit`, oldest first: each entry as it was written, so a
    /// kind this build does not know is answered as it was stored (CLAUDE.md #2).
    pub async fn timeline(&self, coworker_id: &str, limit: i64) -> StoreResult<Vec<Value>> {
        let entries: Vec<Value> = sqlx::query_scalar(
            "select entry from (select entry, at_ms, id from timeline_view where coworker_id = $1
                                 order by at_ms desc, id desc limit $2) newest
              order by at_ms, id",
        )
        .bind(coworker_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(entries)
    }
}
