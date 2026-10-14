//! Accounts: who signs in, and the reads behind their views.

use super::*;

impl PgStore {
    /// Replay an account's log. Returns the state and the sequence it was read at.
    pub async fn load_account(&self, id: &AccountId) -> StoreResult<(Account, i64)> {
        let (events, seq) = self.stream::<AccountEvent>(&account_stream(id)).await?;
        Ok((Account::replay(&events), seq))
    }

    /// Append events and refresh the projection, atomically.
    ///
    /// `expected_seq` must be what `load_account` returned. A concurrent writer that appended in
    /// between makes the unique index fire, which surfaces as `StoreError::Conflict`.
    pub async fn append_account(
        &self,
        id: &AccountId,
        expected_seq: i64,
        events: &[AccountEvent],
        view: &AccountView,
    ) -> StoreResult<i64> {
        let mut tx = self.pool.begin().await?;
        let stream = account_stream(id);
        let mut seq = expected_seq;

        for event in events {
            seq += 1;
            let payload = serde_json::to_value(event)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            sqlx::query(
                "insert into events (stream_id, stream_seq, event_type, payload)
                 values ($1, $2, $3, $4)",
            )
            .bind(&stream)
            .bind(seq)
            .bind(event.event_type())
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        }

        // Maintain the session index from the same events, in the same transaction. The current
        // hash is always findable. The just-rotated-away hash stays findable until
        // `grace_until_ms` (REFRESH_GRACE_MS after the rotation) so a concurrent refresh with
        // the old cookie can load the account; decide() still refuses it once the window has
        // passed, and the next rotate drops hashes that are neither current nor previous.
        for event in events {
            match event {
                AccountEvent::SessionIssued {
                    session_id,
                    refresh_token_hash,
                    ..
                } => {
                    sqlx::query(
                        "insert into session_view (refresh_token_hash, account_id, session_id, grace_until_ms)
                         values ($1, $2, $3, null)
                         on conflict (refresh_token_hash) do nothing",
                    )
                    .bind(refresh_token_hash)
                    .bind(id.as_str())
                    .bind(session_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                }
                AccountEvent::SessionRefreshed {
                    session_id,
                    refresh_token_hash,
                    previous_refresh_token_hash,
                    at_ms,
                } => {
                    sqlx::query(
                        "insert into session_view (refresh_token_hash, account_id, session_id, grace_until_ms)
                         values ($1, $2, $3, null)
                         on conflict (refresh_token_hash) do update set
                           account_id = excluded.account_id,
                           session_id = excluded.session_id,
                           grace_until_ms = null",
                    )
                    .bind(refresh_token_hash)
                    .bind(id.as_str())
                    .bind(session_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                    if let Some(previous) = previous_refresh_token_hash {
                        let grace_until = *at_ms + opengrok_core::REFRESH_GRACE_MS;
                        sqlx::query(
                            "insert into session_view (refresh_token_hash, account_id, session_id, grace_until_ms)
                             values ($1, $2, $3, $4)
                             on conflict (refresh_token_hash) do update set
                               account_id = excluded.account_id,
                               session_id = excluded.session_id,
                               grace_until_ms = excluded.grace_until_ms",
                        )
                        .bind(previous)
                        .bind(id.as_str())
                        .bind(session_id.as_str())
                        .bind(grace_until)
                        .execute(&mut *tx)
                        .await?;
                        sqlx::query(
                            "delete from session_view
                              where session_id = $1
                                and refresh_token_hash <> $2
                                and refresh_token_hash <> $3",
                        )
                        .bind(session_id.as_str())
                        .bind(refresh_token_hash)
                        .bind(previous)
                        .execute(&mut *tx)
                        .await?;
                    } else {
                        sqlx::query(
                            "delete from session_view
                              where session_id = $1
                                and refresh_token_hash <> $2",
                        )
                        .bind(session_id.as_str())
                        .bind(refresh_token_hash)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
                AccountEvent::SessionRevoked { session_id, .. } => {
                    sqlx::query("delete from session_view where session_id = $1")
                        .bind(session_id.as_str())
                        .execute(&mut *tx)
                        .await?;
                }
                AccountEvent::Registered { .. }
                | AccountEvent::PlanChanged { .. }
                | AccountEvent::CredentialsSet { .. }
                | AccountEvent::EmailVerified { .. }
                | AccountEvent::Enabled { .. }
                | AccountEvent::Disabled { .. }
                | AccountEvent::ProfileUpdated { .. }
                | AccountEvent::PasswordChanged { .. }
                | AccountEvent::InferenceSourceSet { .. }
                | AccountEvent::TimeZoneSet { .. } => {}
            }
        }

        sqlx::query(
            "insert into account_view
               (id, email, plan, trial, updated_at_ms,
                password_hash, first_name, last_name, org_id, verified, enabled, avatar_url)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             on conflict (id) do update set
               email = excluded.email,
               plan = excluded.plan,
               trial = excluded.trial,
               updated_at_ms = excluded.updated_at_ms,
               password_hash = coalesce(excluded.password_hash, account_view.password_hash),
               first_name = excluded.first_name,
               last_name = excluded.last_name,
               org_id = coalesce(excluded.org_id, account_view.org_id),
               verified = excluded.verified or account_view.verified,
               enabled = excluded.enabled",
        )
        .bind(view.id.as_str())
        .bind(&view.email)
        .bind(view.plan.as_wire())
        .bind(view.trial)
        .bind(view.updated_at_ms)
        .bind(&view.password_hash)
        .bind(&view.first_name)
        .bind(&view.last_name)
        .bind(&view.org_id)
        .bind(view.verified)
        .bind(view.enabled)
        .bind(&view.avatar_url)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// Which account holds this refresh token, if any. The current hash, or the just-rotated-away
    /// hash whose `grace_until_ms` has not passed. After grace the previous hash is invisible here
    /// even if the row has not been swept yet.
    pub async fn account_by_refresh_hash(
        &self,
        hash: &str,
        at_ms: i64,
    ) -> StoreResult<Option<AccountId>> {
        let row = sqlx::query(
            "select account_id from session_view
              where refresh_token_hash = $1
                and (grace_until_ms is null or grace_until_ms >= $2)",
        )
        .bind(hash)
        .bind(at_ms)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(AccountId::from_stored(
                row.try_get::<String, _>("account_id")?,
            ))
        })
        .transpose()
    }

    /// The read side: answered from the projection, never by replaying a log.
    pub async fn account_by_email(&self, email: &str) -> StoreResult<Option<AccountView>> {
        let row = sqlx::query(
            "select id, email, plan, trial, updated_at_ms, password_hash, first_name, last_name,\n                    org_id, verified, enabled, avatar_url\n             from account_view where email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            Ok(AccountView {
                id: AccountId::from_stored(row.try_get::<String, _>("id")?),
                email: row.try_get("email")?,
                plan: Plan::from_wire(&row.try_get::<String, _>("plan")?),
                trial: row.try_get("trial")?,
                updated_at_ms: row.try_get("updated_at_ms")?,
                password_hash: row.try_get("password_hash")?,
                first_name: row.try_get("first_name")?,
                last_name: row.try_get("last_name")?,
                org_id: row.try_get("org_id")?,
                verified: row.try_get("verified")?,
                enabled: row.try_get("enabled")?,
                avatar_url: row.try_get("avatar_url")?,
            })
        })
        .transpose()
    }

    /// Every account belonging to an org — the admin's user list. Reads the projection, so it
    /// captures CLI-created accounts, signups and the admin alike, not only those who redeemed an
    /// invite. Ordered by email for a stable display.
    pub async fn accounts_by_org(&self, org_id: &str) -> StoreResult<Vec<AccountView>> {
        let rows = sqlx::query(
            "select id, email, plan, trial, updated_at_ms, password_hash, first_name, last_name,
                    org_id, verified, enabled, avatar_url
             from account_view where org_id = $1 order by email",
        )
        .bind(org_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(AccountView {
                    id: AccountId::from_stored(row.try_get::<String, _>("id")?),
                    email: row.try_get("email")?,
                    plan: Plan::from_wire(&row.try_get::<String, _>("plan")?),
                    trial: row.try_get("trial")?,
                    updated_at_ms: row.try_get("updated_at_ms")?,
                    password_hash: row.try_get("password_hash")?,
                    first_name: row.try_get("first_name")?,
                    last_name: row.try_get("last_name")?,
                    org_id: row.try_get("org_id")?,
                    verified: row.try_get("verified")?,
                    enabled: row.try_get("enabled")?,
                    avatar_url: row.try_get("avatar_url")?,
                })
            })
            .collect()
    }
}
