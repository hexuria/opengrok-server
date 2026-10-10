//! `doc_session` rows: one open Office document per (box, path).
//!
//! The row is the session, not the document — the bytes stay on the box and snapshots go to
//! `artifact`. What lives here is what a turn boundary would otherwise erase: the content hash
//! the session last saw, the mutation count, and the edits still proposed rather than applied.
//! `proposals` is a JSON array of the shape the office_* tools exchange (one object per pending
//! proposal); the store treats it as opaque and the desk owns its schema, the same way `meta`
//! belongs to its writers.

use sqlx::Row;

use crate::postgres::PgStore;
use crate::{StoreError, StoreResult};

/// One `doc_session` row.
#[derive(Debug, Clone, PartialEq)]
pub struct DocSessionRow {
    pub id: String,
    pub account_id: String,
    pub coworker_id: String,
    pub box_id: String,
    pub path: String,
    pub kind: String,
    pub content_sha256: String,
    pub version: i64,
    pub proposals: serde_json::Value,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

fn doc_session_row(row: &sqlx::postgres::PgRow) -> StoreResult<DocSessionRow> {
    Ok(DocSessionRow {
        id: row.try_get("id")?,
        account_id: row.try_get("account_id")?,
        coworker_id: row.try_get("coworker_id")?,
        box_id: row.try_get("box_id")?,
        path: row.try_get("path")?,
        kind: row.try_get("kind")?,
        content_sha256: row.try_get("content_sha256")?,
        version: row.try_get("version")?,
        proposals: row.try_get("proposals")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

impl PgStore {
    /// One session by its `odoc_` id.
    pub async fn doc_session(&self, id: &str) -> StoreResult<Option<DocSessionRow>> {
        let row = sqlx::query(
            "select id, account_id, coworker_id, box_id, path, kind, content_sha256, version,
                    proposals, created_at_ms, updated_at_ms
               from doc_session where id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(doc_session_row).transpose()
    }

    /// The session a (box, path) pair already has, if any — `office_open` calls this first, so
    /// a second open of the same file returns the same handle rather than a second row.
    pub async fn doc_session_by_path(
        &self,
        box_id: &str,
        path: &str,
    ) -> StoreResult<Option<DocSessionRow>> {
        let row = sqlx::query(
            "select id, account_id, coworker_id, box_id, path, kind, content_sha256, version,
                    proposals, created_at_ms, updated_at_ms
               from doc_session where box_id = $1 and path = $2",
        )
        .bind(box_id)
        .bind(path)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(doc_session_row).transpose()
    }

    /// Every session an account's coworker holds open — the fetch routes' guard reads these so
    /// a document page is only ever served to the account that opened it.
    pub async fn doc_sessions_for(
        &self,
        account_id: &str,
        coworker_id: &str,
    ) -> StoreResult<Vec<DocSessionRow>> {
        let rows = sqlx::query(
            "select id, account_id, coworker_id, box_id, path, kind, content_sha256, version,
                    proposals, created_at_ms, updated_at_ms
               from doc_session
              where account_id = $1 and coworker_id = $2
              order by updated_at_ms desc",
        )
        .bind(account_id)
        .bind(coworker_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(doc_session_row).collect()
    }

    /// Insert a new session. The (box_id, path) unique index answers a racing open with
    /// `Conflict` — the caller re-reads by path and uses the winner's handle.
    pub async fn put_doc_session(&self, row: &DocSessionRow) -> StoreResult<()> {
        sqlx::query(
            "insert into doc_session
               (id, account_id, coworker_id, box_id, path, kind, content_sha256, version,
                proposals, created_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&row.id)
        .bind(&row.account_id)
        .bind(&row.coworker_id)
        .bind(&row.box_id)
        .bind(&row.path)
        .bind(&row.kind)
        .bind(&row.content_sha256)
        .bind(row.version)
        .bind(&row.proposals)
        .bind(row.created_at_ms)
        .bind(row.updated_at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Write the session's moving fields back — the hash and version the last read or write
    /// left, the proposals now pending — but ONLY if the row still carries `expected_version`.
    /// Two calls mutating one session cannot both succeed: the loser gets `Conflict` and
    /// re-reads rather than writing over the winner's new version (`expected_seq` is the same
    /// trick on the event log).
    pub async fn save_doc_session(
        &self,
        id: &str,
        expected_version: i64,
        content_sha256: &str,
        version: i64,
        proposals: &serde_json::Value,
        updated_at_ms: i64,
    ) -> StoreResult<()> {
        let done = sqlx::query(
            "update doc_session
                set content_sha256 = $3, version = $4, proposals = $5, updated_at_ms = $6
              where id = $1 and version = $2",
        )
        .bind(id)
        .bind(expected_version)
        .bind(content_sha256)
        .bind(version)
        .bind(proposals)
        .bind(updated_at_ms)
        .execute(self.pool())
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// End a session — the row is the session, so closing deletes it. `office_close` is
    /// terminal: proposals pending on it die with it, and the file's snapshots already live in
    /// `artifact`, which is why a delete rather than a flag is honest.
    pub async fn close_doc_session(&self, id: &str) -> StoreResult<bool> {
        let done = sqlx::query("delete from doc_session where id = $1")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }
}
