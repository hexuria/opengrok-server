//! User lines aimed at a run that is already going.
//!
//! A steer is not a stop and not a new run. The row sits until the harness
//! appends the text to that run's next model request, then it is acked so the
//! following round does not say it again. WHO may write a row is decided in
//! the server: `account_id` is the caller the route already authenticated.

use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSteerRow {
    pub id: String,
    pub content: String,
}

impl PgStore {
    /// Insert one line. A repeat of the same `client_message_id` on this run
    /// is a no-op, so a client retry does not double the model's next request.
    pub async fn enqueue_run_steer(
        &self,
        id: &str,
        run_id: &str,
        account_id: &str,
        content: &str,
        client_message_id: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into run_steer (
                id, run_id, account_id, content, client_message_id, created_at_ms
             ) values ($1, $2, $3, $4, $5, $6)
             on conflict (run_id, client_message_id) where client_message_id is not null
             do nothing",
        )
        .bind(id)
        .bind(run_id)
        .bind(account_id)
        .bind(content)
        .bind(client_message_id)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Lines not yet folded into the run, oldest first.
    pub async fn open_run_steer(&self, run_id: &str) -> StoreResult<Vec<RunSteerRow>> {
        let rows = sqlx::query(
            "select id, content from run_steer
              where run_id = $1 and consumed_at_ms is null
              order by created_at_ms",
        )
        .bind(run_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(RunSteerRow {
                    id: row.try_get("id")?,
                    content: row.try_get("content")?,
                })
            })
            .collect()
    }

    /// The loop has put these lines on the model request.
    pub async fn ack_run_steer(&self, run_id: &str, ids: &[String], at_ms: i64) -> StoreResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "update run_steer
                set consumed_at_ms = $3
              where run_id = $1 and id = any($2) and consumed_at_ms is null",
        )
        .bind(run_id)
        .bind(ids)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
