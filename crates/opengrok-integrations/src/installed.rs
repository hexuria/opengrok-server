//! Persistence is account scoped at every read and write. There is no lending operation.
use crate::registry::{Catalog, Entry};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_plugins::bundle::Bundle;
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
    })
}
pub async fn list(store: &PgStore, account: &AccountId) -> StoreResult<Vec<Installation>> {
    sqlx::query("select * from plugin_installation where account_id = $1 order by name")
        .bind(account.as_str())
        .fetch_all(store.pool())
        .await?
        .into_iter()
        .map(read)
        .collect()
}
pub async fn for_turn(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
) -> StoreResult<Vec<Installation>> {
    // A shared bot's owner is checked in SQL against the *driving* account. Merely having a
    // UseCoworker grant cannot lend the owner's account installations or credentials.
    sqlx::query(
        "select p.* from plugin_installation p join coworker_view c on c.account_id = p.account_id
        where p.account_id = $1 and c.id = $2 and c.retired = false order by p.name",
    )
    .bind(account.as_str())
    .bind(bot.as_str())
    .fetch_all(store.pool())
    .await?
    .into_iter()
    .map(read)
    .collect()
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
    // POST never changes an existing pin, even on a retry against a newer registry. Updates
    // explicitly uninstall then install, so old secrets cannot be sent to a replacement URL.
    sqlx::query("insert into plugin_installation (account_id,name,registry,registry_revision,repository,revision,bundle,installed_at_ms)
        values ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(account.as_str()).bind(&entry.name).bind(&catalog.registry).bind(&catalog.revision)
        .bind(&entry.repository).bind(&entry.revision).bind(value).bind(at_ms).execute(store.pool()).await?;
    Ok(())
}
pub async fn uninstall(store: &PgStore, account: &AccountId, name: &str) -> StoreResult<bool> {
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

pub async fn values(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    name: &str,
) -> StoreResult<std::collections::BTreeMap<String, String>> {
    read_values(store, vault, account, name, None).await
}

/// Ciphertext and snapshot eligibility are read together. Reading secret ids first and opening
/// them later allowed uninstall/reinstall to replace the secret between those two statements.
pub async fn values_for_installation(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    installation: &Installation,
) -> StoreResult<std::collections::BTreeMap<String, String>> {
    let expected = serde_json::to_value(&installation.bundle)
        .map_err(|_| StoreError::Corrupt("invalid installation snapshot".into()))?;
    read_values(
        store,
        vault,
        account,
        &installation.name,
        Some((installation.installed_at_ms, expected)),
    )
    .await
}

async fn read_values(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    name: &str,
    expected: Option<(i64, serde_json::Value)>,
) -> StoreResult<std::collections::BTreeMap<String, String>> {
    let (at_ms, bundle) = match expected {
        Some((at, bundle)) => (Some(at), Some(bundle)),
        None => (None, None),
    };
    let rows = sqlx::query("select c.connector,c.secret_id,s.nonce,s.ciphertext,s.key_id
        from plugin_credential c join plugin_installation p on p.account_id=c.account_id and p.name=c.plugin_name
        join secret_store s on s.id=c.secret_id
        where c.account_id=$1 and c.plugin_name=$2
        and ($3::bigint is null or p.installed_at_ms=$3) and ($4::jsonb is null or p.bundle=$4)")
        .bind(account.as_str()).bind(name).bind(at_ms).bind(bundle).fetch_all(store.pool()).await?;
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
