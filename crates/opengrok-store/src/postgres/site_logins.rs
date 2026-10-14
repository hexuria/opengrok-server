//! Site logins: a person’s saved passwords and passkeys, sealed by account.

use super::*;

impl PgStore {
    /// The secret_store key of a saved site login's password. The account id sits in the
    /// key so the purge's substring sweep finds it.
    pub fn site_login_secret_id(account_id: &AccountId, id: &str) -> String {
        format!("site-login:{}:{id}", account_id.as_str())
    }

    /// The key of the row's authenticator-code seed (an `otpauth://` URI), beside the password.
    pub fn site_login_code_id(account_id: &AccountId, id: &str) -> String {
        format!("site-login-otp:{}:{id}", account_id.as_str())
    }

    /// The key of a passkey row's private key (PKCS#8, base64).
    pub fn site_login_passkey_id(account_id: &AccountId, id: &str) -> String {
        format!("site-login-passkey:{}:{id}", account_id.as_str())
    }

    /// A person's saved site logins, never the secrets. Ordered for a settings list.
    pub async fn site_logins(&self, account_id: &AccountId) -> StoreResult<Vec<SiteLoginRow>> {
        let rows = sqlx::query(
            "select id, origin, username, label, kind, notes, created_at_ms, updated_at_ms, last_used_at_ms,
                    passkey_credential_id, passkey_rp_id, passkey_user_handle
             from site_login where account_id = $1 order by label, origin, username",
        )
        .bind(account_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(site_login_row).collect()
    }

    /// Save (or replace) one site login: the row keyed by (account, origin, username), the
    /// password and the code seed sealed into secret_store under the row's id. A secret
    /// given as `None` is left as it was. Returns the row, which never holds a secret.
    pub async fn upsert_site_login(
        &self,
        vault: &Vault,
        account_id: &AccountId,
        login: &SiteLoginWrite<'_>,
        at_ms: i64,
    ) -> StoreResult<SiteLoginRow> {
        // The row first, in the transaction: on a conflict the existing id comes back, so two
        // saves racing for the same login end with one row and one secret under its id.
        let candidate = format!("sl_{}", uuid::Uuid::now_v7());
        let label = if login.label.trim().is_empty() {
            format!("{} on {}", login.username, login.origin)
        } else {
            login.label.trim().to_string()
        };
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "insert into site_login (id, account_id, origin, username, label, kind, notes, created_at_ms, updated_at_ms,
                                     passkey_credential_id, passkey_rp_id, passkey_user_handle)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $8, $9, $10, $11)
             on conflict (account_id, origin, username, kind) do update set
               label = excluded.label, notes = excluded.notes,
               updated_at_ms = excluded.updated_at_ms,
               passkey_credential_id = coalesce(excluded.passkey_credential_id, site_login.passkey_credential_id),
               passkey_rp_id = coalesce(excluded.passkey_rp_id, site_login.passkey_rp_id),
               passkey_user_handle = coalesce(excluded.passkey_user_handle, site_login.passkey_user_handle)
             returning id, origin, username, label, kind, notes, created_at_ms, updated_at_ms, last_used_at_ms,
                       passkey_credential_id, passkey_rp_id, passkey_user_handle",
        )
        .bind(&candidate)
        .bind(account_id.as_str())
        .bind(login.origin)
        .bind(login.username)
        .bind(&label)
        .bind(login.kind)
        .bind(login.notes)
        .bind(at_ms)
        .bind(login.passkey.as_ref().map(|p| p.credential_id_b64.as_str()))
        .bind(login.passkey.as_ref().map(|p| p.rp_id.as_str()))
        .bind(login.passkey.as_ref().map(|p| p.user_handle_b64.as_str()))
        .fetch_one(&mut *tx)
        .await?;
        let row = site_login_row(row)?;
        for (key, secret) in [
            (
                Self::site_login_secret_id(account_id, &row.id),
                login.password,
            ),
            (Self::site_login_code_id(account_id, &row.id), login.otpauth),
            (
                Self::site_login_passkey_id(account_id, &row.id),
                login.passkey.as_ref().map(|p| p.private_key_b64.as_str()),
            ),
        ] {
            let Some(secret) = secret else {
                continue;
            };
            let sealed = vault.seal(&key, secret)?;
            crate::vault_rows::write_sealed(&mut *tx, &key, &sealed, at_ms).await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    /// The title and notes of one of the person's own rows. False when the row is not theirs.
    pub async fn update_site_login(
        &self,
        account_id: &AccountId,
        id: &str,
        label: Option<&str>,
        notes: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let changed = sqlx::query(
            "update site_login set
               label = coalesce($3, label), notes = coalesce($4, notes), updated_at_ms = $5
             where account_id = $1 and id = $2",
        )
        .bind(account_id.as_str())
        .bind(id)
        .bind(label)
        .bind(notes)
        .bind(at_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed > 0)
    }

    /// A bot just used this login (a `savedLogin` fill landed).
    pub async fn touch_site_login_used(
        &self,
        account_id: &AccountId,
        id: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query("update site_login set last_used_at_ms = $3 where account_id = $1 and id = $2")
            .bind(account_id.as_str())
            .bind(id)
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete one of the person's own site logins, secrets included. False when there was
    /// no such row of theirs (another account's id is "no such row" too).
    pub async fn delete_site_login(&self, account_id: &AccountId, id: &str) -> StoreResult<bool> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("delete from site_login where account_id = $1 and id = $2")
            .bind(account_id.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Ok(false);
        }
        sqlx::query("delete from secret_store where id = $1 or id = $2 or id = $3")
            .bind(Self::site_login_secret_id(account_id, id))
            .bind(Self::site_login_code_id(account_id, id))
            .bind(Self::site_login_passkey_id(account_id, id))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Share one of the person's own logins with one of their Bots, or take it back. `false` when
    /// the login is not theirs (nothing written).
    pub async fn set_site_login_shared(
        &self,
        account_id: &AccountId,
        login_id: &str,
        coworker_id: &str,
        shared: bool,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let theirs = sqlx::query("select 1 from site_login where account_id = $1 and id = $2")
            .bind(account_id.as_str())
            .bind(login_id)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if !theirs {
            return Ok(false);
        }
        if shared {
            sqlx::query(
                "insert into site_login_share (login_id, coworker_id, account_id, shared_at_ms)
                 values ($1, $2, $3, $4) on conflict (login_id, coworker_id) do nothing",
            )
            .bind(login_id)
            .bind(coworker_id)
            .bind(account_id.as_str())
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "delete from site_login_share
                 where login_id = $1 and coworker_id = $2 and account_id = $3",
            )
            .bind(login_id)
            .bind(coworker_id)
            .bind(account_id.as_str())
            .execute(&self.pool)
            .await?;
        }
        Ok(true)
    }

    /// The Bots one of the person's logins is shared with.
    pub async fn site_login_bots(
        &self,
        account_id: &AccountId,
        login_id: &str,
    ) -> StoreResult<Vec<String>> {
        let rows = sqlx::query(
            "select coworker_id from site_login_share
             where account_id = $1 and login_id = $2 order by shared_at_ms",
        )
        .bind(account_id.as_str())
        .bind(login_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| row.try_get::<String, _>("coworker_id").map_err(Into::into))
            .collect()
    }

    /// The person's logins shared with one of their Bots: all of them under the all-Bots switch.
    pub async fn logins_shared_with(
        &self,
        account_id: &AccountId,
        coworker_id: &str,
    ) -> StoreResult<Vec<String>> {
        let rows = sqlx::query(
            "select login_id from site_login_share where account_id = $1 and coworker_id = $2
             union select id from site_login where account_id = $1
               and exists (select 1 from login_sharing where account_id = $1)",
        )
        .bind(account_id.as_str())
        .bind(coworker_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| row.try_get::<String, _>("login_id").map_err(Into::into))
            .collect()
    }

    /// The person's all-Bots login switch (9 Oct 2026), set first when `set` is `(on, at_ms)`.
    pub async fn logins_for_all_bots(
        &self,
        account: &AccountId,
        set: Option<(bool, i64)>,
    ) -> StoreResult<bool> {
        let account = account.as_str();
        if let Some((all, at_ms)) = set {
            let sql = match all {
                true => "insert into login_sharing values ($1, $2) on conflict do nothing",
                false => "delete from login_sharing where account_id = $1 and shared_at_ms <= $2",
            };
            let query = sqlx::query(sql).bind(account).bind(at_ms);
            query.execute(&self.pool).await?;
        }
        let query =
            sqlx::query_scalar("select exists (select 1 from login_sharing where account_id = $1)");
        Ok(query.bind(account).fetch_one(&self.pool).await?)
    }

    /// The secrets of one of the person's own site logins, opened for their app alone.
    /// None when the row is not theirs.
    pub async fn open_site_login(
        &self,
        vault: &Vault,
        account_id: &AccountId,
        id: &str,
    ) -> StoreResult<Option<SiteLoginSecrets>> {
        let owned: Option<String> =
            sqlx::query_scalar("select id from site_login where account_id = $1 and id = $2")
                .bind(account_id.as_str())
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        if owned.is_none() {
            return Ok(None);
        }
        let password = self
            .open_credential(vault, &Self::site_login_secret_id(account_id, id))
            .await?;
        let otpauth = self
            .open_credential(vault, &Self::site_login_code_id(account_id, id))
            .await?;
        let passkey_key = self
            .open_credential(vault, &Self::site_login_passkey_id(account_id, id))
            .await?;
        Ok(Some(SiteLoginSecrets {
            password,
            otpauth,
            passkey_key,
        }))
    }

    /// Open a connection's credential. The one place a token is ever in plaintext.
    pub async fn open_credential(&self, vault: &Vault, id: &str) -> StoreResult<Option<String>> {
        let row = sqlx::query("select nonce, ciphertext, key_id from secret_store where id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            crate::vault_rows::sealed_of(&row)
                .and_then(|sealed| vault.open(id, &sealed).map_err(Into::into))
        })
        .transpose()
    }
}

/// One saved site login as the settings list shows it. Never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteLoginRow {
    pub id: String,
    pub origin: String,
    pub username: String,
    pub label: String,
    /// `password`, `code` (an authenticator code with no password) or `passkey`.
    pub kind: String,
    pub notes: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_used_at_ms: Option<i64>,
    /// A passkey row's public half: the credential id and user handle (base64) and the
    /// relying party. The key itself is sealed apart.
    pub passkey: Option<PasskeyMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasskeyMeta {
    pub credential_id_b64: String,
    pub rp_id: String,
    pub user_handle_b64: String,
}

/// What a save carries. A secret given as `None` is left as it was.
pub struct SiteLoginWrite<'a> {
    pub origin: &'a str,
    pub username: &'a str,
    pub label: &'a str,
    pub kind: &'a str,
    pub notes: &'a str,
    pub password: Option<&'a str>,
    pub otpauth: Option<&'a str>,
    pub passkey: Option<PasskeyWrite>,
}

/// A passkey to file: its public half and its private key (PKCS#8, base64).
#[derive(Clone, PartialEq, Eq)]
pub struct PasskeyWrite {
    pub credential_id_b64: String,
    pub rp_id: String,
    pub user_handle_b64: String,
    pub private_key_b64: String,
}

impl std::fmt::Debug for PasskeyWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyWrite")
            .field("rp_id", &self.rp_id)
            .field("private_key_b64", &"<redacted>")
            .finish()
    }
}

/// The secrets of one row, opened for the owner's app. Debug never shows them.
#[derive(Clone, PartialEq, Eq)]
pub struct SiteLoginSecrets {
    pub password: Option<String>,
    pub otpauth: Option<String>,
    pub passkey_key: Option<String>,
}

impl std::fmt::Debug for SiteLoginSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteLoginSecrets")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("otpauth", &self.otpauth.as_ref().map(|_| "<redacted>"))
            .field(
                "passkey_key",
                &self.passkey_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

fn site_login_row(row: sqlx::postgres::PgRow) -> StoreResult<SiteLoginRow> {
    Ok(SiteLoginRow {
        id: row.try_get("id")?,
        origin: row.try_get("origin")?,
        username: row.try_get("username")?,
        label: row.try_get("label")?,
        kind: row.try_get("kind")?,
        notes: row.try_get("notes")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        last_used_at_ms: row.try_get("last_used_at_ms")?,
        passkey: match (
            row.try_get::<Option<String>, _>("passkey_credential_id")?,
            row.try_get::<Option<String>, _>("passkey_rp_id")?,
            row.try_get::<Option<String>, _>("passkey_user_handle")?,
        ) {
            (Some(credential_id_b64), Some(rp_id), Some(user_handle_b64)) => Some(PasskeyMeta {
                credential_id_b64,
                rp_id,
                user_handle_b64,
            }),
            _ => None,
        },
    })
}
