//! Store calls only tests make: mounted from `src/postgres.rs` with `#[path]`, so they compile into
//! the crate the integration tests link while nothing the server serves reaches for them — and they
//! are not counted as the store (`scripts/crate-size.sh`). Anything here that a route comes to need
//! moves back into `src/` with that route.

use sqlx::Row as _;

use super::{PgStore, RecipeRunRow, ThreadRun, recipe_run_row, thread_run_from_row};
use crate::StoreResult;

impl PgStore {
    /// Every run of a recipe, whoever played it. A person is only ever shown their own
    /// (`recipe_runs_for_account`); a test checks them all.
    pub async fn recipe_runs(&self, recipe_id: &str, limit: i64) -> StoreResult<Vec<RecipeRunRow>> {
        let rows = sqlx::query(
            "select id, recipe_id, version, coworker_id, run_id, ok, stopped_at, receipt, at_ms,
                    lease_until_ms
               from recipe_run where recipe_id = $1 order by at_ms desc limit $2",
        )
        .bind(recipe_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(recipe_run_row).collect()
    }

    /// The template a coworker was hired from, if any.
    pub async fn template_of(
        &self,
        coworker: &opengrok_core::id::CoworkerId,
    ) -> StoreResult<Option<String>> {
        let row =
            sqlx::query("select template_id from coworker_template_use where coworker_id = $1")
                .bind(coworker.as_str())
                .fetch_optional(self.pool())
                .await?;
        row.map(|row| row.try_get("template_id").map_err(Into::into))
            .transpose()
    }

    /// The runs journaled under one thread, newest first by when each last moved, whoever owns
    /// them — a routine's firings, as a test counts them. What a person reads is
    /// `runs_for_thread_owned_by`, which filters by owner and orders by when each began.
    pub async fn runs_for_thread(
        &self,
        thread_id: &str,
        limit: i64,
    ) -> StoreResult<Vec<ThreadRun>> {
        let rows = sqlx::query(
            "select id, status, started_at_ms, updated_at_ms from run_view
             where thread_id = $1 order by updated_at_ms desc limit $2",
        )
        .bind(thread_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(thread_run_from_row).collect()
    }

    /// Withdraw a grant. The row stays, so the log still says a grant existed and when it stopped.
    /// No route withdraws one yet; `against_visibility.rs` does, to show that a member's access
    /// to a shared coworker goes with its owner's.
    pub async fn revoke_access(
        &self,
        principal: &opengrok_core::id::AccountId,
        coworker: &opengrok_core::id::CoworkerId,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update grant_view set revoked = true, updated_at_ms = $3
             where principal_id = $1 and coworker_id = $2",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    // ---- WebAuthn device registry (passkey step-up, slice 7) ----
    //
    // The foundation slice 7 laid; no route registers, lists or checks a device yet, so its only
    // caller is `against_webauthn_store.rs`. The table and its purge stay in `src/`.

    /// Register (or replace) a WebAuthn credential for an account. Upsert on the credential id so a
    /// re-registration of the same authenticator refreshes it rather than erroring; a re-register
    /// also clears a prior revocation, because registering it again IS re-authorising it.
    pub async fn register_webauthn_credential(
        &self,
        account_id: &str,
        credential_id: &str,
        public_key: &str,
        label: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into webauthn_credential
               (account_id, credential_id, public_key, sign_count, label, created_at_ms, revoked)
             values ($1, $2, $3, 0, $4, $5, false)
             on conflict (account_id, credential_id) do update
               set public_key = excluded.public_key,
                   label = excluded.label,
                   revoked = false",
        )
        .bind(account_id)
        .bind(credential_id)
        .bind(public_key)
        .bind(label)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// An account's registered devices, newest first. Includes revoked rows (the dashboard shows
    /// them as revoked); callers that verify an assertion filter to `!revoked` themselves.
    pub async fn webauthn_credentials(
        &self,
        account_id: &str,
    ) -> StoreResult<Vec<(String, String, i64, String, i64, Option<i64>, bool)>> {
        let rows = sqlx::query(
            "select credential_id, public_key, sign_count, label, created_at_ms,
                    last_used_at_ms, revoked
             from webauthn_credential where account_id = $1
             order by created_at_ms desc",
        )
        .bind(account_id)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("credential_id")?,
                    row.try_get::<String, _>("public_key")?,
                    row.try_get::<i64, _>("sign_count")?,
                    row.try_get::<String, _>("label")?,
                    row.try_get::<i64, _>("created_at_ms")?,
                    row.try_get::<Option<i64>, _>("last_used_at_ms")?,
                    row.try_get::<bool, _>("revoked")?,
                ))
            })
            .collect()
    }

    /// Record a successful assertion: bump the stored sign_count (replay/cloning defence) and stamp
    /// last-used. Only touches a non-revoked row.
    pub async fn touch_webauthn_credential(
        &self,
        account_id: &str,
        credential_id: &str,
        sign_count: i64,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update webauthn_credential
                set sign_count = $3, last_used_at_ms = $4
              where account_id = $1 and credential_id = $2 and not revoked",
        )
        .bind(account_id)
        .bind(credential_id)
        .bind(sign_count)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Revoke a device from the registry — it can no longer satisfy a step-up. Not deleted, so the
    /// dashboard can still show it as revoked and a re-register can un-revoke it.
    pub async fn revoke_webauthn_credential(
        &self,
        account_id: &str,
        credential_id: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "update webauthn_credential set revoked = true
              where account_id = $1 and credential_id = $2",
        )
        .bind(account_id)
        .bind(credential_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    // ---- A scope's computer, as tests read and seed it ----

    /// The scope's box and its kind: `scoped_computer_full` without the idle flag.
    pub async fn scoped_computer(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<(String, String)>> {
        Ok(self
            .scoped_computer_full(scope, scope_id)
            .await?
            .map(|(box_id, kind, _)| (box_id, kind)))
    }

    /// Write a scope's box over whatever it had: a test's seed. Nothing the server runs may, as a
    /// later create overwriting an earlier one is how #302's boxes ran untracked; it claims
    /// (`claim_scoped_computer`).
    pub async fn set_scoped_computer(
        &self,
        scope: &str,
        scope_id: &str,
        box_id: &str,
        kind: &str,
        org_id: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into scoped_computer (scope, scope_id, box_id, kind, org_id, last_used_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $6)
             on conflict (scope, scope_id) do update set
               box_id = excluded.box_id, kind = excluded.kind, org_id = excluded.org_id,
               last_used_at_ms = excluded.last_used_at_ms, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(box_id)
        .bind(kind)
        .bind(org_id)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Does this account have ANY registered, non-revoked device? The gate for "an unregistered
    /// device gets no remote control" — false ⇒ the control plane refuses the dangerous actions.
    pub async fn has_registered_device(&self, account_id: &str) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 as one from webauthn_credential
              where account_id = $1 and not revoked limit 1",
        )
        .bind(account_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.is_some())
    }

    /// Revoke a bot key, if the caller owns it: the key's row alone. The route revokes a key with
    /// its refresh tokens in one transaction (`revoke_bot_key_with_refresh`); a test that only
    /// needs a revoked key revokes it here.
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

    /// One queued send this account owns, whatever its status. `None` for another account's id.
    /// A route reads a thread's queue (`pending_user_messages`); a test reads one row back.
    pub async fn pending_user_message(
        &self,
        id: &str,
        account: &opengrok_core::id::AccountId,
    ) -> StoreResult<Option<crate::PendingUserMessageRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{} where id = $1 and account_id = $2",
            crate::pending::PENDING_SELECT
        )))
        .bind(id)
        .bind(account.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(crate::pending::pending_row).transpose()
    }
}

impl crate::vault::Vault {
    /// Build from a base64 KEK of exactly 32 bytes, with no retired keys. The server always
    /// builds its vault with the retired list (`from_base64_keys`); only tests want one key.
    pub fn from_base64_key(kek: &str) -> StoreResult<Self> {
        Self::from_base64_keys(kek, &[])
    }
}
