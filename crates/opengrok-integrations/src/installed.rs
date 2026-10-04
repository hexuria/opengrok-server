//! Persistence is account scoped at every read and write. There is no lending operation.
use crate::registry::{Catalog, Entry};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_plugins::bundle::Bundle;
use opengrok_policy::ToolSet;
use opengrok_store::{PgStore, StoreError, StoreResult, Vault};
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Installation {
    pub name: String,
    pub registry: String,
    pub registry_revision: String,
    pub repository: String,
    pub revision: String,
    pub installed_at_ms: i64,
    pub bundle: Bundle,
    /// Which install this snapshot was (`values_for_installation`). Never sent: it is a server
    /// handle, not something a client could act on.
    #[serde(skip)]
    pub incarnation: String,
}
fn read(row: sqlx::postgres::PgRow) -> StoreResult<Installation> {
    Ok(Installation {
        name: row.try_get("name")?,
        registry: row.try_get("registry")?,
        registry_revision: row.try_get("registry_revision")?,
        repository: row.try_get("repository")?,
        revision: row.try_get("revision")?,
        installed_at_ms: row.try_get("installed_at_ms")?,
        bundle: serde_json::from_value(row.try_get("bundle")?)
            .map_err(|_| StoreError::Corrupt("invalid installed bundle".into()))?,
        incarnation: row.try_get("incarnation")?,
    })
}
/// ONE UNREADABLE ROW IS ONE MISSING PLUGIN, logged by name, never every plugin and every screen
/// that lists them: collecting into one `Result` made a single bad bundle a 503 on the ceiling,
/// where the owner could not even switch it off. That row's ceiling switch still shows, as a
/// plugin no longer loaded (`ceiling::rows`). A database that does not answer is still an error.
fn readable(rows: Vec<sqlx::postgres::PgRow>) -> Vec<Installation> {
    rows.into_iter()
        .filter_map(|row| {
            let name: Option<String> = row.try_get("name").ok();
            read(row)
                .inspect_err(
                    |error| tracing::warn!(?name, %error, "an installed plugin is unreadable"),
                )
                .ok()
        })
        .collect()
}
pub async fn list(store: &PgStore, account: &AccountId) -> StoreResult<Vec<Installation>> {
    let rows = sqlx::query("select * from plugin_installation where account_id = $1 order by name")
        .bind(account.as_str())
        .fetch_all(store.pool())
        .await?;
    Ok(readable(rows))
}
pub async fn for_turn(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
) -> StoreResult<Vec<Installation>> {
    // A shared bot's owner is checked in SQL against the *driving* account. Merely having a
    // UseCoworker grant cannot lend the owner's account installations or credentials.
    let rows = sqlx::query(
        "select p.* from plugin_installation p join coworker_view c on c.account_id = p.account_id
        where p.account_id = $1 and c.id = $2 and c.retired = false order by p.name",
    )
    .bind(account.as_str())
    .bind(bot.as_str())
    .fetch_all(store.pool())
    .await?;
    Ok(readable(rows))
}
pub async fn install(
    store: &PgStore,
    account: &AccountId,
    catalog: &Catalog,
    entry: &Entry,
    bundle: &Bundle,
    at_ms: i64,
) -> StoreResult<()> {
    let value = serde_json::to_value(bundle)
        .map_err(|_| StoreError::Corrupt("bundle could not be saved".into()))?;
    let mut tx = store.pool().begin().await?;
    // A NEW INSTALL STARTS SWITCHED OFF on every Bot, whatever a ceiling still says about the
    // name: an entry left by an earlier install of it would otherwise switch on a different
    // revision, URL and tool list that nobody agreed to.
    switch_off(&mut tx, account, &entry.name, at_ms).await?;
    // POST never changes an existing pin, even on a retry against a newer registry. Updates
    // explicitly uninstall then install, so old secrets cannot be sent to a replacement URL.
    sqlx::query("insert into plugin_installation (account_id,name,registry,registry_revision,repository,revision,bundle,installed_at_ms)
        values ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(account.as_str()).bind(&entry.name).bind(&catalog.registry).bind(&catalog.revision)
        .bind(&entry.repository).bind(&entry.revision).bind(value).bind(at_ms).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn uninstall(
    store: &PgStore,
    account: &AccountId,
    name: &str,
    at_ms: i64,
) -> StoreResult<bool> {
    let mut tx = store.pool().begin().await?;
    let exists = sqlx::query(
        "select name from plugin_installation where account_id = $1 and name = $2 for update",
    )
    .bind(account.as_str())
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    if !exists {
        return Ok(false);
    }
    switch_off(&mut tx, account, name, at_ms).await?;
    sqlx::query("delete from secret_store where id in (select secret_id from plugin_credential where account_id = $1 and plugin_name = $2)")
        .bind(account.as_str()).bind(name).execute(&mut *tx).await?;
    sqlx::query("delete from plugin_installation where account_id = $1 and name = $2")
        .bind(account.as_str())
        .bind(name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// A stored tool set without `plugin`: its whole-plugin entry and any of its tools named one by
/// one. `None` when there was nothing of it there (or nothing readable), so nothing is written.
fn without(stored: serde_json::Value, plugin: &str) -> StoreResult<Option<serde_json::Value>> {
    let Ok(ToolSet::Only(names)) = serde_json::from_value::<ToolSet>(stored) else {
        return Ok(None);
    };
    let under = format!("{plugin}.");
    if !names.iter().any(|n| n.starts_with(&under)) {
        return Ok(None);
    }
    let kept: std::collections::BTreeSet<String> = names
        .into_iter()
        .filter(|n| !n.starts_with(&under))
        .collect();
    let kept = if kept.is_empty() {
        ToolSet::None
    } else {
        ToolSet::Only(kept)
    };
    serde_json::to_value(kept)
        .map(Some)
        .map_err(|error| StoreError::Corrupt(error.to_string()))
}

/// Take `plugin` out of the ceiling and every grant of each Bot `account` owns, in `tx`. Grants
/// first, the order `set_ceiling` and `grant_access` lock them in; a changed ceiling bumps its
/// `version`, so a screen saving what it read before this is refused, not undoing it.
async fn switch_off(
    tx: &mut sqlx::PgConnection,
    account: &AccountId,
    plugin: &str,
    at_ms: i64,
) -> StoreResult<()> {
    let grants = sqlx::query(
        "select g.principal_id, g.coworker_id, g.profile from grant_view g
         join coworker_view c on c.id = g.coworker_id where c.account_id = $1 for update of g",
    )
    .bind(account.as_str())
    .fetch_all(&mut *tx)
    .await?;
    for row in grants {
        if let Some(kept) = without(row.try_get("profile")?, plugin)? {
            sqlx::query(
                "update grant_view set profile = $3, updated_at_ms = $4
                 where principal_id = $1 and coworker_id = $2",
            )
            .bind(row.try_get::<String, _>("principal_id")?)
            .bind(row.try_get::<String, _>("coworker_id")?)
            .bind(kept)
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        }
    }
    let ceilings = sqlx::query(
        "select t.coworker_id, t.tools from ceiling_view t
         join coworker_view c on c.id = t.coworker_id where c.account_id = $1 for update of t",
    )
    .bind(account.as_str())
    .fetch_all(&mut *tx)
    .await?;
    for row in ceilings {
        if let Some(kept) = without(row.try_get("tools")?, plugin)? {
            sqlx::query(
                "update ceiling_view set tools = $2, updated_at_ms = $3, version = version + 1
                 where coworker_id = $1",
            )
            .bind(row.try_get::<String, _>("coworker_id")?)
            .bind(kept)
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        }
    }
    Ok(())
}

pub async fn credential(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    name: &str,
    connector: &str,
    token: &str,
    at_ms: i64,
) -> StoreResult<bool> {
    let mut tx = store.pool().begin().await?;
    let row = sqlx::query(
        "select bundle from plugin_installation where account_id = $1 and name = $2 for update",
    )
    .bind(account.as_str())
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let bundle: Bundle = serde_json::from_value(row.try_get("bundle")?)
        .map_err(|_| StoreError::Corrupt("invalid bundle".into()))?;
    if !bundle.connectors().iter().any(|c| c == connector) {
        return Ok(false);
    }
    let id = format!("plugin/{account}/{name}/{connector}");
    let sealed = vault.seal(&id, token)?;
    sqlx::query("insert into secret_store(id,nonce,ciphertext,key_id,updated_at_ms) values($1,$2,$3,$4,$5)
        on conflict(id) do update set nonce=excluded.nonce,ciphertext=excluded.ciphertext,key_id=excluded.key_id,updated_at_ms=excluded.updated_at_ms")
        .bind(&id).bind(sealed.nonce).bind(sealed.ciphertext).bind(sealed.key_id).bind(at_ms).execute(&mut *tx).await?;
    sqlx::query("insert into plugin_credential(account_id,plugin_name,connector,secret_id) values($1,$2,$3,$4) on conflict do nothing")
        .bind(account.as_str()).bind(name).bind(connector).bind(&id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

/// The credentials of exactly the install `installation` was read from: none once it has been
/// uninstalled, even if the same name was installed again since. Ciphertext and snapshot
/// eligibility are read together. Reading secret ids first and opening them later allowed
/// uninstall/reinstall to replace the secret between those two statements.
pub async fn values_for_installation(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    installation: &Installation,
) -> StoreResult<std::collections::BTreeMap<String, String>> {
    let rows = sqlx::query("select c.connector,c.secret_id,s.nonce,s.ciphertext,s.key_id
        from plugin_credential c join plugin_installation p on p.account_id=c.account_id and p.name=c.plugin_name
        join secret_store s on s.id=c.secret_id
        where c.account_id=$1 and c.plugin_name=$2 and p.incarnation=$3")
        .bind(account.as_str()).bind(&installation.name).bind(&installation.incarnation)
        .fetch_all(store.pool()).await?;
    let mut values = std::collections::BTreeMap::new();
    for row in rows {
        let connector: String = row.try_get("connector")?;
        let id: String = row.try_get("secret_id")?;
        let sealed = opengrok_store::Sealed {
            nonce: row.try_get("nonce")?,
            ciphertext: row.try_get("ciphertext")?,
            key_id: row.try_get("key_id")?,
        };
        values.insert(
            opengrok_plugins::token_key(&connector),
            vault.open(&id, &sealed)?,
        );
    }
    Ok(values)
}

#[cfg(test)]
#[path = "../tests/unit/installed.rs"]
mod tests;
