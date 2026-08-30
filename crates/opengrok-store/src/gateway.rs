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
