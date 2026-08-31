//! Store operations for the desktop gateway's transcript and its acceptance ledger.
//!
//! THIS IS A WIRE-FORMAT PROJECTION, NOT DOMAIN TRUTH. The entries here are stored in the exact
//! JSON shape the client renders (`client-grok-bot.md` §3.1), because the client is the only
//! reader and reshaping on every read would re-derive the same bytes forever. The runs journal
//! remains the account of what actually happened.

use opengrok_core::id::CoworkerId;
use serde_json::Value;
use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

impl PgStore {
    /// Append one client-shaped entry, returning the sequence it landed at.
    ///
    /// The sequence is allocated inside the insert — `coalesce(max(seq),0)+1` under the primary
    /// key — so two writers cannot mint the same seq; one of them retries on the conflict.
    pub async fn append_gateway_entry(
        &self,
        coworker: &CoworkerId,
        entry: &Value,
        at_ms: i64,
    ) -> StoreResult<i64> {
        for _ in 0..3 {
            let row = sqlx::query(
                "insert into gateway_entry (coworker_id, seq, entry, at_ms)
                 select $1, coalesce(max(seq), 0) + 1, $2, $3 from gateway_entry
                 where coworker_id = $1
                 on conflict do nothing
                 returning seq",
            )
            .bind(coworker.as_str())
            .bind(entry)
            .bind(at_ms)
            .fetch_optional(self.pool())
            .await?;
            if let Some(row) = row {
                return Ok(row.try_get("seq")?);
            }
        }
        Err(crate::StoreError::Corrupt(
            "could not allocate a transcript sequence after three attempts".to_string(),
        ))
    }

    /// Replace the entry at a known sequence — how a streamed answer becomes its final self.
    /// Update a transcript entry by its `id` (the entry's own `"id"` field), not its seq — for
    /// re-emitting an entry whose seq the caller did not keep (e.g. the approval card, flipped from
    /// pending to its outcome status).
    pub async fn update_gateway_entry_by_id(
        &self,
        coworker: &CoworkerId,
        entry_id: &str,
        entry: &Value,
    ) -> StoreResult<()> {
        sqlx::query(
            "update gateway_entry set entry = $3
              where coworker_id = $1 and entry->>'id' = $2",
        )
        .bind(coworker.as_str())
        .bind(entry_id)
        .bind(entry)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn update_gateway_entry(
        &self,
        coworker: &CoworkerId,
        seq: i64,
        entry: &Value,
    ) -> StoreResult<()> {
        sqlx::query("update gateway_entry set entry = $3 where coworker_id = $1 and seq = $2")
            .bind(coworker.as_str())
            .bind(seq)
            .bind(entry)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// The newest `limit` entries at or before `before_seq` (absent = the very end), oldest
    /// first — the shape every tail/window/page command feeds from. The second element is
    /// `nextBeforeSeq`: the seq to ask for next, absent when the top has been reached.
    pub async fn gateway_tail(
        &self,
        coworker: &CoworkerId,
        before_seq: Option<i64>,
        limit: i64,
    ) -> StoreResult<(Vec<Value>, Option<i64>)> {
        let rows = sqlx::query(
            "select seq, entry from gateway_entry
             where coworker_id = $1 and ($2::bigint is null or seq < $2)
             order by seq desc limit $3",
        )
        .bind(coworker.as_str())
        .bind(before_seq)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        let mut entries: Vec<(i64, Value)> = rows
            .into_iter()
            .map(|row| Ok::<_, crate::StoreError>((row.try_get("seq")?, row.try_get("entry")?)))
            .collect::<Result<_, _>>()?;
        entries.reverse();

        let oldest = entries.first().map(|(seq, _)| *seq);
        // Anything strictly older than the oldest row returned is the next page.
        let next_before = match oldest {
            Some(seq) if seq > 1 => Some(seq),
            _ => None,
        };
        Ok((
            entries.into_iter().map(|(_, entry)| entry).collect(),
            next_before,
        ))
    }

    /// A page of (seq, entry) pairs, oldest first — seam B ships the sequence itself.
    pub async fn gateway_page(
        &self,
        coworker: &CoworkerId,
        before_seq: Option<i64>,
        limit: i64,
    ) -> StoreResult<Vec<(i64, Value)>> {
        let rows = sqlx::query(
            "select seq, entry from gateway_entry
             where coworker_id = $1 and ($2::bigint is null or seq < $2)
             order by seq desc limit $3",
        )
        .bind(coworker.as_str())
        .bind(before_seq)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        let mut page: Vec<(i64, Value)> = rows
            .into_iter()
            .map(|row| Ok::<_, crate::StoreError>((row.try_get("seq")?, row.try_get("entry")?)))
            .collect::<Result<_, _>>()?;
        page.reverse();
        Ok(page)
    }

    /// Seam B's extra profile fields for a coworker (description, title, avatar shape/colour).
    pub async fn seamb_profile(&self, coworker: &CoworkerId) -> StoreResult<Option<Value>> {
        let row = sqlx::query("select profile from seamb_profile where coworker_id = $1")
            .bind(coworker.as_str())
            .fetch_optional(self.pool())
            .await?;
        row.map(|row| Ok(row.try_get("profile")?)).transpose()
    }

    pub async fn put_seamb_profile(
        &self,
        coworker: &CoworkerId,
        profile: &Value,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into seamb_profile (coworker_id, profile, updated_at_ms)
             values ($1, $2, $3)
             on conflict (coworker_id) do update set
               profile = excluded.profile, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(coworker.as_str())
        .bind(profile)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The whole transcript, oldest first. Unbounded on purpose — `getAgentTranscript` is.
    pub async fn gateway_transcript(&self, coworker: &CoworkerId) -> StoreResult<Vec<Value>> {
        let rows =
            sqlx::query("select entry from gateway_entry where coworker_id = $1 order by seq")
                .bind(coworker.as_str())
                .fetch_all(self.pool())
                .await?;
        rows.into_iter()
            .map(|row| Ok(row.try_get("entry")?))
            .collect()
    }

    /// Find one entry by its client-visible id. A scan by jsonb key, indexed well enough by the
    /// per-coworker primary key range for transcripts of human size.
    pub async fn find_gateway_entry(
        &self,
        coworker: &CoworkerId,
        entry_id: &str,
    ) -> StoreResult<Option<(i64, Value)>> {
        let row = sqlx::query(
            "select seq, entry from gateway_entry
             where coworker_id = $1 and entry->>'id' = $2
             order by seq desc limit 1",
        )
        .bind(coworker.as_str())
        .bind(entry_id)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| Ok((row.try_get("seq")?, row.try_get("entry")?)))
            .transpose()
    }

    /// Delete entries by client id, answering which ids actually went away.
    pub async fn delete_gateway_entries(
        &self,
        coworker: &CoworkerId,
        ids: &[String],
    ) -> StoreResult<Vec<String>> {
        let rows = sqlx::query(
            "delete from gateway_entry
             where coworker_id = $1 and entry->>'id' = any($2)
             returning entry->>'id' as id",
        )
        .bind(coworker.as_str())
        .bind(ids)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(|row| Ok(row.try_get("id")?)).collect()
    }

    /// Record an accepted prompt. Answers what the ledger now holds for the nonce:
    /// `Ok(record)` — either freshly written or the identical earlier acceptance —
    /// or `Err(())` when the nonce exists with a DIFFERENT digest, which the client treats as
    /// `NONCE_DIGEST_MISMATCH` and must never be silently absorbed.
    #[allow(clippy::result_unit_err)]
    pub async fn accept_nonce(
        &self,
        account_slot: &str,
        nonce: &str,
        digest: &str,
        record: &Value,
        at_ms: i64,
    ) -> StoreResult<Result<Value, ()>> {
        let inserted = sqlx::query(
            "insert into gateway_nonce (account_slot, nonce, digest, record, at_ms)
             values ($1, $2, $3, $4, $5)
             on conflict do nothing",
        )
        .bind(account_slot)
        .bind(nonce)
        .bind(digest)
        .bind(record)
        .bind(at_ms)
        .execute(self.pool())
        .await?;

        if inserted.rows_affected() == 1 {
            return Ok(Ok(record.clone()));
        }
        let existing = sqlx::query(
            "select digest, record from gateway_nonce where account_slot = $1 and nonce = $2",
        )
        .bind(account_slot)
        .bind(nonce)
        .fetch_one(self.pool())
        .await?;
        let stored_digest: String = existing.try_get("digest")?;
        if stored_digest == digest {
            Ok(Ok(existing.try_get("record")?))
        } else {
            Ok(Err(()))
        }
    }

    /// Replace a nonce's record once the fact it was holding a place for is settled.
    pub async fn overwrite_nonce_record(
        &self,
        account_slot: &str,
        nonce: &str,
        digest: &str,
        record: &Value,
    ) -> StoreResult<()> {
        sqlx::query(
            "update gateway_nonce set record = $4
             where account_slot = $1 and nonce = $2 and digest = $3",
        )
        .bind(account_slot)
        .bind(nonce)
        .bind(digest)
        .bind(record)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// What the ledger holds for a nonce — `promptAcceptanceStatus`.
    pub async fn nonce_record(
        &self,
        account_slot: &str,
        nonce: &str,
    ) -> StoreResult<Option<Value>> {
        let row =
            sqlx::query("select record from gateway_nonce where account_slot = $1 and nonce = $2")
                .bind(account_slot)
                .bind(nonce)
                .fetch_optional(self.pool())
                .await?;
        row.map(|row| Ok(row.try_get("record")?)).transpose()
    }
}

/// One bot key, as the list endpoint reports it. The signed token itself is never stored —
/// it is shown once at mint, like every credential worth having.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BotKeyView {
    pub jti: String,
    pub coworker_id: String,
    pub label: String,
    pub revoked: bool,
    pub created_at_ms: i64,
}

impl PgStore {
    pub async fn insert_bot_key(
        &self,
        jti: &str,
        account: &opengrok_core::id::AccountId,
        coworker: &CoworkerId,
        label: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into bot_key_view (jti, account_id, coworker_id, label, revoked, created_at_ms)
             values ($1, $2, $3, $4, false, $5)",
        )
        .bind(jti)
        .bind(account.as_str())
        .bind(coworker.as_str())
        .bind(label)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Is this key still good? Missing and revoked answer alike: no.
    pub async fn bot_key_live(&self, jti: &str) -> StoreResult<bool> {
        let row = sqlx::query("select 1 as one from bot_key_view where jti = $1 and not revoked")
            .bind(jti)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.is_some())
    }

    pub async fn bot_keys_for(
        &self,
        account: &opengrok_core::id::AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<Vec<BotKeyView>> {
        let rows = sqlx::query(
            "select jti, coworker_id, label, revoked, created_at_ms from bot_key_view
             where account_id = $1 and coworker_id = $2 order by created_at_ms desc",
        )
        .bind(account.as_str())
        .bind(coworker.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(BotKeyView {
                    jti: row.try_get("jti")?,
                    coworker_id: row.try_get("coworker_id")?,
                    label: row.try_get("label")?,
                    revoked: row.try_get("revoked")?,
                    created_at_ms: row.try_get("created_at_ms")?,
                })
            })
            .collect()
    }

    /// Revoke, if the caller owns it. Answers whether anything changed — the 404-vs-204 fact.
    pub async fn revoke_bot_key(
        &self,
        account: &opengrok_core::id::AccountId,
        jti: &str,
    ) -> StoreResult<bool> {
        let done = sqlx::query(
            "update bot_key_view set revoked = true where jti = $1 and account_id = $2",
        )
        .bind(jti)
        .bind(account.as_str())
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() == 1)
    }
}
