//! `secret_store` as a whole: which key sealed each row, whether this server can still open them,
//! and moving them to the current key.
//!
//! EVERY WRITE GOES THROUGH `write_sealed`. A writer that set a new ciphertext but kept the old
//! row's key id would leave a row that never opens again, so there is one upsert and no other.

use std::collections::BTreeMap;

use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;
use crate::vault::{Sealed, Vault};

pub(crate) async fn write_sealed<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    id: &str,
    sealed: &Sealed,
    at_ms: i64,
) -> StoreResult<()> {
    sqlx::query(
        "insert into secret_store (id, nonce, ciphertext, key_id, updated_at_ms)
         values ($1, $2, $3, $4, $5)
         on conflict (id) do update set
           nonce = excluded.nonce, ciphertext = excluded.ciphertext,
           key_id = excluded.key_id, updated_at_ms = excluded.updated_at_ms",
    )
    .bind(id)
    .bind(&sealed.nonce)
    .bind(&sealed.ciphertext)
    .bind(sealed.key_id.as_deref())
    .bind(at_ms)
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) fn sealed_of(row: &sqlx::postgres::PgRow) -> StoreResult<Sealed> {
    Ok(Sealed {
        nonce: row.try_get("nonce")?,
        ciphertext: row.try_get("ciphertext")?,
        key_id: row.try_get("key_id")?,
    })
}

/// How `secret_store` stands against the keys this server holds. Read at boot and on every
/// `/health`, so it is one query and at most one decrypt per key group: the newest row of each
/// group is the canary for the rest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VaultCheck {
    /// Whether this server has a key at all.
    pub configured: bool,
    /// Rows under the current key.
    pub current: i64,
    /// Rows under a retired key this server still holds: they open, and a reseal moves them.
    pub retired: i64,
    /// Rows sealed before key ids were recorded.
    pub legacy: i64,
    /// Rows under a key this server does not hold, by key id.
    pub lost: BTreeMap<String, i64>,
    /// Held key ids whose newest row did not open: altered or moved rows.
    pub failed: Vec<String>,
    /// The newest row without a key id opened under none of the held keys.
    pub legacy_unopenable: bool,
}

impl VaultCheck {
    pub fn to_reseal(&self) -> i64 {
        self.retired + self.legacy
    }

    /// One sentence for an operator, or `None` when every group's canary opened. No counts and
    /// no ids: `/health` prints it, unauthenticated. Rows that merely want a reseal are not a
    /// problem — they open.
    pub fn problem(&self) -> Option<&'static str> {
        let total = self.current + self.retired + self.legacy + self.lost.values().sum::<i64>();
        if !self.configured {
            return (total > 0).then_some(
                "credentials are stored but OG_CREDENTIAL_KEK is not set, so none of them open; \
                 set it to the key they were sealed with",
            );
        }
        if !self.lost.is_empty() {
            return Some(
                "some stored credentials were sealed with a key this server no longer has; put \
                 that key in OG_CREDENTIAL_KEK_OLD and run `opengrok vault reseal`, or have their \
                 owners save them again",
            );
        }
        if self.legacy_unopenable {
            return Some(
                "stored credentials from before key ids open with none of this server's keys: \
                 they were sealed with a key this server no longer has, or altered; put the old \
                 key in OG_CREDENTIAL_KEK_OLD and run `opengrok vault reseal`",
            );
        }
        (!self.failed.is_empty()).then_some(
            "a stored credential does not open under the key it names: it was altered or moved \
             to another row; `opengrok vault reseal` lists it",
        )
    }
}

/// What a reseal did. Counts, never a value.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResealReport {
    pub resealed: u64,
    pub already_current: u64,
    /// Rewritten or deleted by the running server between the read and the write, and left as that
    /// write made it. A server restarted onto the current key sealed it there; a second run catches
    /// it otherwise.
    pub raced: u64,
    /// Rows no held key opens, by id. Kept, never deleted: putting the key back opens them.
    pub unopenable: Vec<String>,
}

impl PgStore {
    /// The boot canary and `/health`'s vault verdict. With no vault, every row is counted and
    /// none is opened.
    pub async fn vault_check(&self, vault: Option<&Vault>) -> StoreResult<VaultCheck> {
        let rows = sqlx::query(
            "select distinct on (key_id) key_id, id, nonce, ciphertext,
                    count(*) over (partition by key_id) as rows
             from secret_store order by key_id, updated_at_ms desc, id",
        )
        .fetch_all(self.pool())
        .await?;
        let mut check = VaultCheck {
            configured: vault.is_some(),
            ..VaultCheck::default()
        };
        for row in rows {
            let count: i64 = row.try_get("rows")?;
            let (id, sealed): (String, _) = (row.try_get("id")?, sealed_of(&row)?);
            let opens = vault.map(|v| v.open(&id, &sealed).is_ok());
            match (&sealed.key_id, vault) {
                (None, _) => {
                    check.legacy += count;
                    check.legacy_unopenable = opens == Some(false);
                }
                (Some(key_id), Some(vault)) if vault.holds(key_id) => {
                    if key_id == vault.key_id() {
                        check.current += count;
                    } else {
                        check.retired += count;
                    }
                    if opens == Some(false) {
                        check.failed.push(key_id.clone());
                    }
                }
                (Some(key_id), _) => {
                    check.lost.insert(key_id.clone(), count);
                }
            }
        }
        Ok(check)
    }

    /// Re-seal every row under `prefix` (all of them for `""`) with the current key. Resumable:
    /// rows already under the current key are skipped, so a second run after a crash picks up
    /// where the first stopped. Safe beside a running server: each write is conditional on the
    /// nonce it read, so a token the server refreshed meanwhile is never put back to its old value.
    pub async fn reseal_secrets(&self, vault: &Vault, prefix: &str) -> StoreResult<ResealReport> {
        let mut report = ResealReport::default();
        let mut after = String::new();
        loop {
            let rows = sqlx::query(
                "select id, nonce, ciphertext, key_id from secret_store
                 where id > $1 and left(id, length($2)) = $2 order by id limit 256",
            )
            .bind(&after)
            .bind(prefix)
            .fetch_all(self.pool())
            .await?;
            let Some(last) = rows.last() else {
                return Ok(report);
            };
            after = last.try_get("id")?;
            for row in rows {
                let id: String = row.try_get("id")?;
                let sealed = sealed_of(&row)?;
                if sealed.key_id.as_deref() == Some(vault.key_id()) {
                    report.already_current += 1;
                    continue;
                }
                let Ok(plaintext) = vault.open(&id, &sealed) else {
                    report.unopenable.push(id);
                    continue;
                };
                // The row id stays the associated data, byte for byte, or the blob stops opening.
                let fresh = vault.seal(&id, &plaintext)?;
                let moved = sqlx::query(
                    "update secret_store set nonce = $2, ciphertext = $3, key_id = $4
                     where id = $1 and nonce = $5",
                )
                .bind(&id)
                .bind(&fresh.nonce)
                .bind(&fresh.ciphertext)
                .bind(fresh.key_id.as_deref())
                .bind(&sealed.nonce)
                .execute(self.pool())
                .await?
                .rows_affected();
                if moved == 1 {
                    report.resealed += 1;
                } else {
                    report.raced += 1;
                }
            }
        }
    }
}
