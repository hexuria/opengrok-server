//! Store operations for auto-review policy: two tiers (global, coworker), one row per scope,
//! every field tri-state. Design: `docs/AUTO-REVIEW.md`.
//!
//! This module only persists rows and fetches the (at most two) that apply to one coworker.
//! Precedence is NOT decided here — it lives in the server's `auto_review::resolve`, so the rule
//! "coworker beats global" exists in exactly one place and the store never pre-resolves a view
//! the settings UI would then have to un-resolve.

use crate::StoreResult;
use crate::postgres::PgStore;

/// One tier's row as stored. `None` on a field means "inherit from the tier below"; `Some("")` on
/// an instruction is an explicit "none" that stops inheritance. Both must survive a round trip —
/// collapsing them would silently re-enable a global rule the user overrode away.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AutoReviewRow {
    pub scope_kind: String,
    pub scope_id: String,
    pub enabled: Option<bool>,
    pub allow_instructions: Option<String>,
    pub ask_instructions: Option<String>,
    pub block_instructions: Option<String>,
    pub updated_at_ms: i64,
}

impl PgStore {
    /// Every tier row the account has written, for the settings surfaces to render inheritance.
    pub async fn auto_review_rows(&self, account_id: &str) -> StoreResult<Vec<AutoReviewRow>> {
        Ok(sqlx::query_as(
            "select scope_kind, scope_id, enabled, allow_instructions, ask_instructions,
                    block_instructions, updated_at_ms
               from auto_review_policy
              where account_id = $1
              order by scope_kind, scope_id",
        )
        .bind(account_id)
        .fetch_all(self.pool())
        .await?)
    }

    /// The rows that apply to one decision: the global row and this coworker's row — whichever
    /// exist. One query, at most two rows; the caller resolves precedence. An absent coworker
    /// binds `''`, which no coworker row can carry (the PUT refuses an empty scope id for that
    /// kind), so it simply matches nothing. Rows of any other scope kind are never returned.
    pub async fn auto_review_tiers(
        &self,
        account_id: &str,
        coworker_id: Option<&str>,
    ) -> StoreResult<Vec<AutoReviewRow>> {
        Ok(sqlx::query_as(
            "select scope_kind, scope_id, enabled, allow_instructions, ask_instructions,
                    block_instructions, updated_at_ms
               from auto_review_policy
              where account_id = $1
                and (scope_kind = 'global'
                     or (scope_kind = 'coworker' and scope_id = $2))",
        )
        .bind(account_id)
        .bind(coworker_id.unwrap_or(""))
        .fetch_all(self.pool())
        .await?)
    }

    /// Upsert one tier's whole row. Every field is written every time — `None` stores null
    /// (inherit), never "keep what was there": the settings UI sends the complete row, and a
    /// partial merge would make "clear this override" impossible to express.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_auto_review_policy(
        &self,
        account_id: &str,
        scope_kind: &str,
        scope_id: &str,
        enabled: Option<bool>,
        allow_instructions: Option<&str>,
        ask_instructions: Option<&str>,
        block_instructions: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into auto_review_policy
                (account_id, scope_kind, scope_id, enabled, allow_instructions,
                 ask_instructions, block_instructions, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8)
             on conflict (account_id, scope_kind, scope_id) do update
                set enabled            = excluded.enabled,
                    allow_instructions = excluded.allow_instructions,
                    ask_instructions   = excluded.ask_instructions,
                    block_instructions = excluded.block_instructions,
                    updated_at_ms      = excluded.updated_at_ms",
        )
        .bind(account_id)
        .bind(scope_kind)
        .bind(scope_id)
        .bind(enabled)
        .bind(allow_instructions)
        .bind(ask_instructions)
        .bind(block_instructions)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Remove a tier's row entirely — full inheritance for that scope from then on.
    pub async fn delete_auto_review_policy(
        &self,
        account_id: &str,
        scope_kind: &str,
        scope_id: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "delete from auto_review_policy
              where account_id = $1 and scope_kind = $2 and scope_id = $3",
        )
        .bind(account_id)
        .bind(scope_kind)
        .bind(scope_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
