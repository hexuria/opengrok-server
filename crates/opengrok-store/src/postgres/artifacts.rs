//! Artifacts: images, recordings, and files.

use super::*;

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

    /// One row per kept version of a doc session, newest first — the chain the document
    /// window's version picker lists and `/office/docs/{id}/…?version=` resolves through.
    /// A version's bytes are the artifact's: `snapshot` writes them under `via`
    /// `office_open`/`office_accept`/`external`, and an attached file's session marks its
    /// upload as v0. Export artifacts carry the same meta keys but are derivatives — a
    /// proposal's would-be bytes or a copy of a version already listed — never a version
    /// themselves, which is why the `via` filter keeps them out. Two artifacts on one
    /// version keep the earliest: the first snapshot is the canonical one.
    ///
    /// Scoped to the session's account: `docSessionId` is a meta key anybody's upload could
    /// name, and a chain that answered across accounts would let a stranger graft a version
    /// onto — or read a version out of — a document that is not theirs.
    pub async fn doc_version_artifacts(
        &self,
        account_id: &str,
        doc_session_id: &str,
    ) -> StoreResult<Vec<ArtifactRow>> {
        let rows = sqlx::query(
            "select distinct on ((meta->>'version')::bigint)
                    id, account_id, kind, mime, filename, size_bytes, recipe_id, run_id,
                    step_index, thread_id, meta, created_at_ms, deleted_at_ms
               from artifact
              where meta->>'docSessionId' = $1
                and account_id = $2
                and meta ? 'version'
                and meta->>'via' in ('office_open', 'office_accept', 'external',
                                     'office_restore')
                and deleted_at_ms is null
              order by (meta->>'version')::bigint desc, created_at_ms asc",
        )
        .bind(doc_session_id)
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(artifact_row).collect()
    }

    /// Record that an artifact IS a version of a doc session — the upload an attached
    /// Office file's session started on, which was written before the session existed and
    /// so never went through `snapshot`. A merge, not a set: `messageId` and any later
    /// keys survive.
    pub async fn mark_artifact_doc_version(
        &self,
        id: &str,
        doc_session_id: &str,
        version: i64,
        via: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "update artifact
                set meta = coalesce(meta, '{}'::jsonb)
                         || jsonb_build_object('docSessionId', to_jsonb($2::text),
                                               'version', to_jsonb($3::bigint),
                                               'via', to_jsonb($4::text))
              where id = $1 and deleted_at_ms is null",
        )
        .bind(id)
        .bind(doc_session_id)
        .bind(version)
        .bind(via)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The paths a doc session has `office_export`ed to. Export's "may rewrite its own copy"
    /// check reads this: the artifact already holds every copy ever shipped to the path, so
    /// a re-export there loses nothing and the delivered file keeps one name.
    ///
    /// Scoped to the session's account like `doc_version_artifacts`: `exportPath` is the
    /// claim that lets export rewrite a file, and a claim another account could plant must
    /// not count.
    pub async fn doc_export_paths(
        &self,
        account_id: &str,
        doc_session_id: &str,
    ) -> StoreResult<Vec<String>> {
        let rows: Vec<(Option<String>,)> = sqlx::query_as(
            "select distinct meta->>'exportPath'
               from artifact
              where meta->>'docSessionId' = $1
                and account_id = $2
                and meta->>'via' = 'office_export'
                and meta ? 'exportPath'
                and deleted_at_ms is null",
        )
        .bind(doc_session_id)
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(|(path,)| path).collect())
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

    /// Mark a person's upload as sent in `message_id` on `thread_id` (#229): the conversation's
    /// replay draws it in that bubble (`GET /artifacts?threadId=` and `meta.messageId`). Only the
    /// owner's row moves; the thread it was sent on wins, as the replay that must find it.
    pub async fn attach_artifact(
        &self,
        id: &str,
        account_id: &str,
        thread_id: &str,
        message_id: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "update artifact
                set thread_id = $3,
                    meta = jsonb_set(coalesce(meta, '{}'::jsonb), '{messageId}', to_jsonb($4::text))
              where id = $1 and account_id = $2 and deleted_at_ms is null",
        )
        .bind(id)
        .bind(account_id)
        .bind(thread_id)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(())
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
