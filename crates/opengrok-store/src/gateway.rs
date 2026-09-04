//! Store operations for the desktop gateway's transcript and its acceptance ledger.
//!
//! THIS IS A WIRE-FORMAT PROJECTION, NOT DOMAIN TRUTH. The entries here are stored in the exact
//! JSON shape the client renders (`client-grok-bot.md` §3.1), because the client is the only
//! reader and reshaping on every read would re-derive the same bytes forever. The runs journal
//! remains the account of what actually happened.

use opengrok_core::id::{AccountId, CoworkerId};
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
        account: &AccountId,
        entry: &Value,
        at_ms: i64,
    ) -> StoreResult<i64> {
        for _ in 0..3 {
            let row = sqlx::query(
                "insert into gateway_entry (coworker_id, seq, entry, at_ms, account_id)
                 select $1, coalesce(max(seq), 0) + 1, $2, $3, $4 from gateway_entry
                 where coworker_id = $1
                 on conflict do nothing
                 returning seq",
            )
            .bind(coworker.as_str())
            .bind(entry)
            .bind(at_ms)
            .bind(account.as_str())
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
        account: &AccountId,
        entry_id: &str,
        entry: &Value,
    ) -> StoreResult<()> {
        sqlx::query(
            "update gateway_entry set entry = $3
              where coworker_id = $1 and entry->>'id' = $2 and account_id = $4",
        )
        .bind(coworker.as_str())
        .bind(entry_id)
        .bind(entry)
        .bind(account.as_str())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Flip ONLY `message.ask.status` on an approval-card entry, returning the updated entry —
    /// for the path that heals an orphaned card. A whole-entry replace here would need the
    /// original `target` back from the caller, and the caller (a resolve for a dead run) never
    /// had it; touching one field keeps the command the user was shown intact.
    pub async fn set_gateway_ask_status(
        &self,
        coworker: &CoworkerId,
        account: &AccountId,
        entry_id: &str,
        status: &str,
    ) -> StoreResult<Option<Value>> {
        let row = sqlx::query(
            "update gateway_entry
                set entry = jsonb_set(entry::jsonb, '{message,ask,status}', to_jsonb($3::text))::jsonb
              where coworker_id = $1 and entry->>'id' = $2 and account_id = $4
                and entry->'message'->'ask' is not null
              returning entry",
        )
        .bind(coworker.as_str())
        .bind(entry_id)
        .bind(status)
        .bind(account.as_str())
        .fetch_optional(self.pool())
        .await?;
        Ok(match row {
            Some(row) => Some(row.try_get("entry")?),
            None => None,
        })
    }

    /// The same surgical flip as `set_gateway_ask_status`, on the auto-review card's own path
    /// (`message.approval.status`). Touching one field keeps the summary/reason/command/
    /// proposedRule the user was shown intact — a whole-entry rewrite would need them back from a
    /// caller that never had them.
    pub async fn set_gateway_approval_status(
        &self,
        coworker: &CoworkerId,
        account: &AccountId,
        entry_id: &str,
        status: &str,
    ) -> StoreResult<Option<Value>> {
        let row = sqlx::query(
            "update gateway_entry
                set entry = jsonb_set(entry::jsonb, '{message,approval,status}', to_jsonb($3::text))::jsonb
              where coworker_id = $1 and entry->>'id' = $2 and account_id = $4
                and entry->'message'->'approval' is not null
              returning entry",
        )
        .bind(coworker.as_str())
        .bind(entry_id)
        .bind(status)
        .bind(account.as_str())
        .fetch_optional(self.pool())
        .await?;
        Ok(match row {
            Some(row) => Some(row.try_get("entry")?),
            None => None,
        })
    }

    pub async fn update_gateway_entry(
        &self,
        coworker: &CoworkerId,
        account: &AccountId,
        seq: i64,
        entry: &Value,
    ) -> StoreResult<()> {
        sqlx::query(
            "update gateway_entry set entry = $3
              where coworker_id = $1 and seq = $2 and account_id = $4",
        )
        .bind(coworker.as_str())
        .bind(seq)
        .bind(entry)
        .bind(account.as_str())
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
        account: &AccountId,
        before_seq: Option<i64>,
        limit: i64,
    ) -> StoreResult<(Vec<Value>, Option<i64>)> {
        let rows = sqlx::query(
            "select seq, entry from gateway_entry
             where coworker_id = $1 and ($2::bigint is null or seq < $2)
               and account_id = $4
             order by seq desc limit $3",
        )
        .bind(coworker.as_str())
        .bind(before_seq)
        .bind(limit)
        .bind(account.as_str())
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
        account: &AccountId,
        before_seq: Option<i64>,
        limit: i64,
    ) -> StoreResult<Vec<(i64, Value)>> {
        let rows = sqlx::query(
            "select seq, entry from gateway_entry
             where coworker_id = $1 and ($2::bigint is null or seq < $2)
               and account_id = $4
             order by seq desc limit $3",
        )
        .bind(coworker.as_str())
        .bind(before_seq)
        .bind(limit)
        .bind(account.as_str())
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
    pub async fn gateway_transcript(
        &self,
        coworker: &CoworkerId,
        account: &AccountId,
    ) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "select entry from gateway_entry
             where coworker_id = $1 and account_id = $2 order by seq",
        )
        .bind(coworker.as_str())
        .bind(account.as_str())
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
        account: &AccountId,
        entry_id: &str,
    ) -> StoreResult<Option<(i64, Value)>> {
        let row = sqlx::query(
            "select seq, entry from gateway_entry
             where coworker_id = $1 and entry->>'id' = $2 and account_id = $3
             order by seq desc limit 1",
        )
        .bind(coworker.as_str())
        .bind(entry_id)
        .bind(account.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| Ok((row.try_get("seq")?, row.try_get("entry")?)))
            .transpose()
    }

    /// Delete entries by client id, answering which ids actually went away.
    pub async fn delete_gateway_entries(
        &self,
        coworker: &CoworkerId,
        account: &AccountId,
        ids: &[String],
    ) -> StoreResult<Vec<String>> {
        let rows = sqlx::query(
            "delete from gateway_entry
             where coworker_id = $1 and entry->>'id' = any($2) and account_id = $3
             returning entry->>'id' as id",
        )
        .bind(coworker.as_str())
        .bind(ids)
        .bind(account.as_str())
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

/// One MCP door call as the audit reports it. `arguments` are already redacted — the row never
/// held the raw ones. `outcome`: `ok`, `failed` (the tool ran and said no — a policy refusal
/// reads the same way to the model), `refused` (the door itself said no: reverse-exec, no
/// computer), `awaiting` (a card is up), `error` (the computer or the store could not answer).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallView {
    pub call_id: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub outcome: String,
    pub request_id: String,
    pub at_ms: i64,
}

/// One door call to record, arguments ALREADY redacted by the caller.
#[derive(Debug, Clone)]
pub struct NewMcpCall<'a> {
    pub call_id: &'a str,
    pub tool: &'a str,
    pub arguments: serde_json::Value,
    pub outcome: &'a str,
    pub request_id: &'a str,
    pub at_ms: i64,
}

/// A coworker's own gateway key as WE record it: which key, whose coworker, and the cap as last
/// set. The secret is sealed elsewhere (`secret_store`); this row is what the console and the
/// run path look up first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoworkerKeyView {
    pub coworker_id: String,
    pub account_id: String,
    pub key_id: String,
    pub key_prefix: String,
    pub quota_usd: Option<String>,
    pub created_at_ms: i64,
    /// Set at retirement. The row stays: its month's points still count toward the owner's
    /// pool, so retire-and-rehire does not reset a member's month.
    pub revoked_at_ms: Option<i64>,
    /// `true` when the sealed secret is at the per-(coworker, member) id; `false` for a row
    /// written before there was more than one key per coworker, whose secret is at the older
    /// per-coworker id. The vault binds the id into the ciphertext, so the old rows cannot be
    /// moved — they are read where they are. Never infer this from the account: a member whose
    /// secret went missing would then be handed the owner's.
    pub secret_scoped: bool,
}

/// A freshly minted key to record — attribution only; the secret is not here and never was.
#[derive(Debug, Clone)]
pub struct NewGatewayKey<'a> {
    pub key_id: &'a str,
    pub org_id: &'a str,
    pub member_account_id: &'a str,
    pub key_prefix: &'a str,
    pub label: &'a str,
    /// The console's nonce for this press, so a repeat of the same press finds this row instead
    /// of minting again. `None` for mints that carried none.
    pub mint_nonce: Option<&'a str>,
    pub at_ms: i64,
}

/// One org member's open-ai-gateway key, as WE record it. The secret is not here and never was:
/// the gateway keeps its hash, and the plaintext existed only in the reply that minted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayKeyView {
    pub key_id: String,
    pub org_id: String,
    pub member_account_id: String,
    pub key_prefix: String,
    pub label: String,
    pub revoked: bool,
    pub created_at_ms: i64,
}

fn coworker_key_row(row: sqlx::postgres::PgRow) -> StoreResult<CoworkerKeyView> {
    Ok(CoworkerKeyView {
        coworker_id: row.try_get("coworker_id")?,
        account_id: row.try_get("account_id")?,
        key_id: row.try_get("key_id")?,
        key_prefix: row.try_get("key_prefix")?,
        quota_usd: row.try_get("quota_usd")?,
        created_at_ms: row.try_get("created_at_ms")?,
        revoked_at_ms: row.try_get("revoked_at_ms")?,
        secret_scoped: row.try_get("secret_scoped")?,
    })
}

fn gateway_key_row(row: sqlx::postgres::PgRow) -> StoreResult<GatewayKeyView> {
    Ok(GatewayKeyView {
        key_id: row.try_get("key_id")?,
        org_id: row.try_get("org_id")?,
        member_account_id: row.try_get("member_account_id")?,
        key_prefix: row.try_get("key_prefix")?,
        label: row.try_get("label")?,
        revoked: row.try_get("revoked")?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
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

    /// Record which gateway key belongs to which org member. The secret is NOT stored — the
    /// gateway holds its hash and the plaintext was shown once.
    pub async fn insert_mcp_call(
        &self,
        account: &opengrok_core::id::AccountId,
        coworker: &CoworkerId,
        call: &NewMcpCall<'_>,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into mcp_call_audit
                (account_id, coworker_id, call_id, tool, arguments, outcome, request_id, at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(account.as_str())
        .bind(coworker.as_str())
        .bind(call.call_id)
        .bind(call.tool)
        .bind(&call.arguments)
        .bind(call.outcome)
        .bind(call.request_id)
        .bind(call.at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn insert_coworker_key(&self, view: &CoworkerKeyView) -> StoreResult<()> {
        sqlx::query(
            "insert into coworker_gateway_key
                (coworker_id, account_id, key_id, key_prefix, quota_usd, created_at_ms,
                 secret_scoped)
             values ($1, $2, $3, $4, $5, $6, $7)
             on conflict (coworker_id, account_id) do update
                set key_id = excluded.key_id,
                    key_prefix = excluded.key_prefix, quota_usd = excluded.quota_usd,
                    created_at_ms = excluded.created_at_ms,
                    secret_scoped = excluded.secret_scoped",
        )
        .bind(&view.coworker_id)
        .bind(&view.account_id)
        .bind(&view.key_id)
        .bind(&view.key_prefix)
        .bind(&view.quota_usd)
        .bind(view.created_at_ms)
        .bind(view.secret_scoped)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// One coworker's door calls, newest first, scoped by the owning account so another
    /// account's coworker id answers nothing rather than somebody else's history.
    pub async fn mcp_calls_for(
        &self,
        account: &opengrok_core::id::AccountId,
        coworker: &CoworkerId,
        limit: i64,
    ) -> StoreResult<Vec<McpCallView>> {
        let rows = sqlx::query(
            "select call_id, tool, arguments, outcome, request_id, at_ms from mcp_call_audit
             where account_id = $1 and coworker_id = $2
             order by at_ms desc, id desc limit $3",
        )
        .bind(account.as_str())
        .bind(coworker.as_str())
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(McpCallView {
                    call_id: row.try_get("call_id")?,
                    tool: row.try_get("tool")?,
                    arguments: row.try_get("arguments")?,
                    outcome: row.try_get("outcome")?,
                    request_id: row.try_get("request_id")?,
                    at_ms: row.try_get("at_ms")?,
                })
            })
            .collect()
    }

    /// The key this PERSON uses to talk to this coworker. Two arguments, not one: a shared
    /// coworker has a key per member, and the coworker id alone would hand back whichever row
    /// the planner reached first — somebody else's credential, billed to somebody else.
    pub async fn coworker_key(
        &self,
        coworker: &CoworkerId,
        account: &AccountId,
    ) -> StoreResult<Option<CoworkerKeyView>> {
        let row = sqlx::query(
            "select coworker_id, account_id, key_id, key_prefix, quota_usd, created_at_ms,
                    revoked_at_ms, secret_scoped
             from coworker_gateway_key where coworker_id = $1 and account_id = $2",
        )
        .bind(coworker.as_str())
        .bind(account.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.map(coworker_key_row).transpose()
    }

    /// Every key ever minted for this coworker, one per member who has talked to it. Retirement
    /// revokes all of them; the console's "is it metered" read asks about one person's.
    pub async fn coworker_keys(&self, coworker: &CoworkerId) -> StoreResult<Vec<CoworkerKeyView>> {
        let rows = sqlx::query(
            "select coworker_id, account_id, key_id, key_prefix, quota_usd, created_at_ms,
                    revoked_at_ms, secret_scoped
             from coworker_gateway_key where coworker_id = $1 order by created_at_ms",
        )
        .bind(coworker.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(coworker_key_row).collect()
    }

    /// Mark EVERY key this coworker has revoked (retirement); the caller revokes each on the
    /// gateway and drops the sealed secrets. The rows STAY: each member's month still counts
    /// toward their own pool. Returns what was live so the caller knows which keys to revoke —
    /// empty when there were none or they were already marked. A retirement that revoked only
    /// the owner's key would leave every other member's credential live on a retired coworker.
    pub async fn mark_coworker_keys_revoked(
        &self,
        coworker: &CoworkerId,
        at_ms: i64,
    ) -> StoreResult<Vec<CoworkerKeyView>> {
        let rows = sqlx::query(
            "update coworker_gateway_key set revoked_at_ms = $2
             where coworker_id = $1 and revoked_at_ms is null
             returning coworker_id, account_id, key_id, key_prefix, quota_usd, created_at_ms,
                       revoked_at_ms, secret_scoped",
        )
        .bind(coworker.as_str())
        .bind(at_ms)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(coworker_key_row).collect()
    }

    /// Every key row an account's coworkers ever had, revoked ones included — the pool read
    /// sums over all of them.
    pub async fn coworker_keys_for_account(
        &self,
        account: &AccountId,
    ) -> StoreResult<Vec<CoworkerKeyView>> {
        let rows = sqlx::query(
            "select coworker_id, account_id, key_id, key_prefix, quota_usd, created_at_ms,
                    revoked_at_ms, secret_scoped
             from coworker_gateway_key where account_id = $1 order by created_at_ms",
        )
        .bind(account.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(coworker_key_row).collect()
    }

    pub async fn insert_gateway_key(&self, key: &NewGatewayKey<'_>) -> StoreResult<()> {
        sqlx::query(
            "insert into gateway_key_view
                (key_id, org_id, member_account_id, key_prefix, label, revoked, created_at_ms,
                 mint_nonce)
             values ($1, $2, $3, $4, $5, false, $6, $7)",
        )
        .bind(key.key_id)
        .bind(key.org_id)
        .bind(key.member_account_id)
        .bind(key.key_prefix)
        .bind(key.label)
        .bind(key.at_ms)
        .bind(key.mint_nonce)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The key an earlier press with this nonce minted, if any — the idempotency lookup.
    pub async fn gateway_key_by_nonce(
        &self,
        org_id: &str,
        mint_nonce: &str,
    ) -> StoreResult<Option<GatewayKeyView>> {
        let row = sqlx::query(
            "select key_id, org_id, member_account_id, key_prefix, label, revoked, created_at_ms
             from gateway_key_view where org_id = $1 and mint_nonce = $2",
        )
        .bind(org_id)
        .bind(mint_nonce)
        .fetch_optional(self.pool())
        .await?;
        row.map(gateway_key_row).transpose()
    }

    /// One org's keys, newest first. Scoped by org so a listing can never show another org's.
    pub async fn gateway_keys_for_org(&self, org_id: &str) -> StoreResult<Vec<GatewayKeyView>> {
        let rows = sqlx::query(
            "select key_id, org_id, member_account_id, key_prefix, label, revoked, created_at_ms
             from gateway_key_view where org_id = $1 order by created_at_ms desc",
        )
        .bind(org_id)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(gateway_key_row).collect()
    }

    /// A key, only if THIS org owns it. Answering `None` for somebody else's key is what lets the
    /// caller 404 rather than confirm another org's key exists.
    pub async fn gateway_key_in_org(
        &self,
        key_id: &str,
        org_id: &str,
    ) -> StoreResult<Option<GatewayKeyView>> {
        let row = sqlx::query(
            "select key_id, org_id, member_account_id, key_prefix, label, revoked, created_at_ms
             from gateway_key_view where key_id = $1 and org_id = $2",
        )
        .bind(key_id)
        .bind(org_id)
        .fetch_optional(self.pool())
        .await?;
        row.map(gateway_key_row).transpose()
    }

    /// Mirror the gateway's revocation locally. The gateway is the authority on whether the key
    /// still authenticates; this keeps the console's listing honest.
    pub async fn mark_gateway_key_revoked(&self, key_id: &str, org_id: &str) -> StoreResult<bool> {
        let done = sqlx::query(
            "update gateway_key_view set revoked = true where key_id = $1 and org_id = $2",
        )
        .bind(key_id)
        .bind(org_id)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() == 1)
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

/// An OAuth client registered against the MCP door's authorization server (RFC 7591). Public:
/// no secret exists to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub created_at_ms: i64,
}

impl PgStore {
    pub async fn insert_oauth_client(&self, client: &OAuthClient) -> StoreResult<()> {
        let uris = serde_json::to_value(&client.redirect_uris)
            .map_err(|error| crate::StoreError::Corrupt(error.to_string()))?;
        sqlx::query(
            "insert into oauth_client (client_id, client_name, redirect_uris, created_at_ms)
             values ($1, $2, $3, $4)",
        )
        .bind(&client.client_id)
        .bind(&client.client_name)
        .bind(uris)
        .bind(client.created_at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn oauth_client(&self, client_id: &str) -> StoreResult<Option<OAuthClient>> {
        let row = sqlx::query(
            "select client_id, client_name, redirect_uris, created_at_ms
             from oauth_client where client_id = $1",
        )
        .bind(client_id)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            Ok(OAuthClient {
                client_id: row.try_get("client_id")?,
                client_name: row.try_get("client_name")?,
                redirect_uris: serde_json::from_value(row.try_get("redirect_uris")?)
                    .map_err(|error| crate::StoreError::Corrupt(error.to_string()))?,
                created_at_ms: row.try_get("created_at_ms")?,
            })
        })
        .transpose()
    }

    /// How many clients have registered — the ceiling on an unauthenticated endpoint.
    pub async fn oauth_client_count(&self) -> StoreResult<i64> {
        let row = sqlx::query("select count(*) as n from oauth_client")
            .fetch_one(self.pool())
            .await?;
        Ok(row.try_get::<i64, _>("n")?)
    }
}

fn refresh_row(row: sqlx::postgres::PgRow) -> StoreResult<RefreshTokenRow> {
    Ok(RefreshTokenRow {
        token_hash: row.try_get("token_hash")?,
        jti: row.try_get("jti")?,
        client_id: row.try_get("client_id")?,
        account_id: row.try_get("account_id")?,
        coworker_id: row.try_get("coworker_id")?,
        created_at_ms: row.try_get("created_at_ms")?,
        expires_at_ms: row.try_get("expires_at_ms")?,
        revoked: row.try_get("revoked")?,
        family: row.try_get("family")?,
    })
}

/// A refresh token as the MCP door's authorization server stores it: the hash, never the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshTokenRow {
    pub token_hash: String,
    /// The access key this refresh belongs to — revoked together, rotated together.
    pub jti: String,
    pub client_id: String,
    pub account_id: String,
    pub coworker_id: String,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub revoked: bool,
    /// The chain this token belongs to: the first access key's jti, carried through rotations.
    pub family: String,
}

/// What claiming a presented refresh token found. `Claimed` is the one caller that may mint;
/// `Spent` is a replay (the row exists and was already spent by a rotation); `Unknown` is a
/// token this server never issued, or one whose chain was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshClaim {
    Claimed(RefreshTokenRow),
    Spent(RefreshTokenRow),
    Unknown,
}

impl PgStore {
    /// SPEND a refresh token in one statement: `revoked = true` only where it was `false`, so
    /// of any number of concurrent presentations exactly one is `Claimed`. The others read the
    /// row back and find it `Spent`, which is the replay case the chain kill exists for.
    pub async fn claim_refresh_token(&self, token_hash: &str) -> StoreResult<RefreshClaim> {
        let claimed = sqlx::query(
            "update oauth_refresh_token set revoked = true
             where token_hash = $1 and revoked = false
             returning token_hash, jti, client_id, account_id, coworker_id, created_at_ms,
                       expires_at_ms, revoked, family",
        )
        .bind(token_hash)
        .fetch_optional(self.pool())
        .await?;
        if let Some(row) = claimed {
            return refresh_row(row).map(RefreshClaim::Claimed);
        }
        Ok(self
            .refresh_token(token_hash)
            .await?
            .map_or(RefreshClaim::Unknown, RefreshClaim::Spent))
    }

    pub async fn insert_refresh_token(&self, row: &RefreshTokenRow) -> StoreResult<()> {
        sqlx::query(
            "insert into oauth_refresh_token
               (token_hash, jti, client_id, account_id, coworker_id, created_at_ms, expires_at_ms,
                revoked, family)
             values ($1, $2, $3, $4, $5, $6, $7, false, $8)",
        )
        .bind(&row.token_hash)
        .bind(&row.jti)
        .bind(&row.client_id)
        .bind(&row.account_id)
        .bind(&row.coworker_id)
        .bind(row.created_at_ms)
        .bind(row.expires_at_ms)
        .bind(&row.family)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// End a rotation chain: every refresh token of the family is REMOVED and every access key
    /// they belonged to revoked, in one transaction. Removed rather than marked, so a later
    /// presentation of any of them reads as unknown — a chain that was ended on purpose (an
    /// owner's revoke, a replay already handled) is not a fresh replay alarm each time a stale
    /// client retries. Only a rotation marks a row spent, and only a spent row is a replay.
    pub async fn revoke_refresh_family(&self, family: &str) -> StoreResult<Vec<String>> {
        let mut tx = self.pool().begin().await?;
        let rows = sqlx::query("delete from oauth_refresh_token where family = $1 returning jti")
            .bind(family)
            .fetch_all(&mut *tx)
            .await?;
        let mut jtis = Vec::with_capacity(rows.len());
        for row in rows {
            let jti: String = row.try_get("jti")?;
            sqlx::query("update bot_key_view set revoked = true where jti = $1")
                .bind(&jti)
                .execute(&mut *tx)
                .await?;
            jtis.push(jti);
        }
        tx.commit().await?;
        Ok(jtis)
    }

    /// An owner revoking a bot key: the key and its refresh tokens go together, in one
    /// transaction, or neither does. `false` when the key is not this account's. Answering
    /// "revoked" while a refresh token could still mint a successor is the lie this exists to
    /// prevent.
    pub async fn revoke_bot_key_with_refresh(
        &self,
        account: &opengrok_core::id::AccountId,
        jti: &str,
    ) -> StoreResult<bool> {
        let mut tx = self.pool().begin().await?;
        let done = sqlx::query(
            "update bot_key_view set revoked = true where jti = $1 and account_id = $2",
        )
        .bind(jti)
        .bind(account.as_str())
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query("delete from oauth_refresh_token where jti = $1")
            .bind(jti)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The row for a presented refresh token, whatever its state — the caller decides what an
    /// expired or revoked one means (a revoked one presented again is a replay worth logging).
    pub async fn refresh_token(&self, token_hash: &str) -> StoreResult<Option<RefreshTokenRow>> {
        let row = sqlx::query(
            "select token_hash, jti, client_id, account_id, coworker_id, created_at_ms,
                    expires_at_ms, revoked, family
             from oauth_refresh_token where token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(self.pool())
        .await?;
        row.map(refresh_row).transpose()
    }

    /// Revoke a bot key by id regardless of who owns it — for the authorization server rotating
    /// its own keys, where the owner is the row's account by construction.
    pub async fn revoke_bot_key_by_jti(&self, jti: &str) -> StoreResult<bool> {
        let done = sqlx::query("update bot_key_view set revoked = true where jti = $1")
            .bind(jti)
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() == 1)
    }
}
