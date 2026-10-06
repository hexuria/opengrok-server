//! Sign-ins that have not finished (#359): the "Needs Auth" an app shows beside a person's accounts,
//! and the one a Reopen resumes. Every read and write names the account that started it, so another
//! person's attempt is the same nothing as none.
use opengrok_core::connection::{self, Owner};
use opengrok_core::id::AccountId;
use opengrok_store::{PgStore, StoreResult};
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// One unfinished sign-in, as `GET /connections/attempts` lists it. `status` is `pending` while the
/// person may still be at the provider (or closed the window, which nothing reports) and `failed`
/// once the provider refused or the code could not be exchanged, with `error` saying why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub id: String,
    pub connector: String,
    pub label: String,
    pub coworker_id: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub updated_at_ms: i64,
    /// The installed plugin an MCP sign-in is for (#364); absent for a sign-in to a service this
    /// deployment configured.
    pub plugin: Option<String>,
}

fn read(row: sqlx::postgres::PgRow) -> StoreResult<Attempt> {
    Ok(Attempt {
        id: row.try_get("id")?,
        connector: row.try_get("connector")?,
        label: row.try_get("label")?,
        coworker_id: row.try_get("coworker_id")?,
        status: row.try_get("status")?,
        error: row.try_get("error")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        plugin: row.try_get("plugin_name")?,
    })
}

/// Every column a reply reads, spelled once. A macro so each query stays one literal string.
macro_rules! columns {
    () => {
        "id, connector, label, coworker_id, status, error, updated_at_ms, plugin_name"
    };
}

/// Start a sign-in that adds an account. Its label is the one the account will get, numbered past
/// the person's live accounts AND their other unfinished sign-ins for the service, so two "Gmail"
/// rows never sit side by side waiting.
pub async fn start(
    store: &PgStore,
    account: &AccountId,
    connector: &str,
    coworker: Option<&str>,
    at_ms: i64,
) -> StoreResult<Attempt> {
    let owner = match coworker {
        Some(bot) => Owner::Bot(opengrok_core::id::CoworkerId::from_stored(bot.to_string())),
        None => Owner::User(account.clone()),
    };
    let (scope, owner_id) = match &owner {
        Owner::Bot(bot) => ("bot", bot.to_string()),
        _ => ("user", account.to_string()),
    };
    let taken: Vec<String> = sqlx::query_scalar(
        "select label from connection_view
          where scope = $1 and owner_id = $2 and connector = $3 and not disconnected
         union all
         select label from connection_attempt
          where account_id = $4 and connector = $3 and coworker_id is not distinct from $5",
    )
    .bind(scope)
    .bind(&owner_id)
    .bind(connector)
    .bind(account.as_str())
    .bind(coworker)
    .fetch_all(store.pool())
    .await?;
    let label = connection::default_label(
        &crate::accounts::label_of(connector),
        taken.iter().map(String::as_str),
    );
    let id = format!("attempt_{}", uuid::Uuid::now_v7().simple());
    let row = sqlx::query(concat!(
        "insert into connection_attempt
           (id, account_id, connector, coworker_id, label, status, updated_at_ms)
         values ($1, $2, $3, $4, $5, 'pending', $6)
         returning ",
        columns!(),
        ""
    ))
    .bind(&id)
    .bind(account.as_str())
    .bind(connector)
    .bind(coworker)
    .bind(&label)
    .bind(at_ms)
    .fetch_one(store.pool())
    .await?;
    read(row)
}

pub async fn list(store: &PgStore, account: &AccountId) -> StoreResult<Vec<Attempt>> {
    let rows = sqlx::query(concat!(
        "select ",
        columns!(),
        " from connection_attempt where account_id = $1
          order by connector, updated_at_ms, id"
    ))
    .bind(account.as_str())
    .fetch_all(store.pool())
    .await?;
    rows.into_iter().map(read).collect()
}

pub async fn get(store: &PgStore, account: &AccountId, id: &str) -> StoreResult<Option<Attempt>> {
    let row = sqlx::query(concat!(
        "select ",
        columns!(),
        " from connection_attempt where id = $1 and account_id = $2"
    ))
    .bind(id)
    .bind(account.as_str())
    .fetch_optional(store.pool())
    .await?;
    row.map(read).transpose()
}

/// Back to pending for a Reopen: the same attempt, so the account it adds keeps its label and no
/// second row appears.
pub async fn reopen(
    store: &PgStore,
    account: &AccountId,
    id: &str,
    at_ms: i64,
) -> StoreResult<Option<Attempt>> {
    let row = sqlx::query(concat!(
        "update connection_attempt set status = 'pending', error = null, updated_at_ms = $3
          where id = $1 and account_id = $2
         returning ",
        columns!(),
        ""
    ))
    .bind(id)
    .bind(account.as_str())
    .bind(at_ms)
    .fetch_optional(store.pool())
    .await?;
    row.map(read).transpose()
}

/// The provider refused, or the code it sent could not be exchanged: "Needs Auth", and why.
pub async fn fail(
    store: &PgStore,
    account: &AccountId,
    id: &str,
    why: &str,
    at_ms: i64,
) -> StoreResult<()> {
    sqlx::query(
        "update connection_attempt set status = 'failed', error = $3, updated_at_ms = $4
          where id = $1 and account_id = $2",
    )
    .bind(id)
    .bind(account.as_str())
    .bind(why)
    .bind(at_ms)
    .execute(store.pool())
    .await?;
    Ok(())
}

/// Renamed before it finished: the account it adds takes the new label.
pub async fn rename(
    store: &PgStore,
    account: &AccountId,
    id: &str,
    label: &str,
    at_ms: i64,
) -> StoreResult<Option<Attempt>> {
    let row = sqlx::query(concat!(
        "update connection_attempt set label = $3, updated_at_ms = $4
          where id = $1 and account_id = $2
         returning ",
        columns!(),
        ""
    ))
    .bind(id)
    .bind(account.as_str())
    .bind(label)
    .bind(at_ms)
    .fetch_optional(store.pool())
    .await?;
    row.map(read).transpose()
}

/// Gone: the account connected (the callback), or the person gave up on it (`DELETE`).
pub async fn remove(store: &PgStore, account: &AccountId, id: &str) -> StoreResult<bool> {
    let done = sqlx::query("delete from connection_attempt where id = $1 and account_id = $2")
        .bind(id)
        .bind(account.as_str())
        .execute(store.pool())
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Start an MCP sign-in for an installed plugin's service (#364): an unfinished sign-in carrying
/// what its callback needs. A reconnect (`target`) keeps the account's label and is deleted, not
/// marked failed, when refused: the account it would refresh still works.
#[allow(clippy::too_many_arguments)]
pub async fn start_mcp(
    store: &PgStore,
    account: &AccountId,
    plugin: &str,
    connector: &str,
    target: Option<(&str, &str)>,
    pkce_verifier: &str,
    metadata: &crate::mcp_oauth::Metadata,
    client_id: &str,
    at_ms: i64,
) -> StoreResult<Attempt> {
    let label = match target {
        Some((_, label)) => label.to_string(),
        None => {
            let taken: Vec<String> = sqlx::query_scalar(
                "select label from connection_view
                  where scope = 'user' and owner_id = $1 and connector = $2 and not disconnected
                 union all
                 select label from connection_attempt where account_id = $1 and connector = $2",
            )
            .bind(account.as_str())
            .bind(connector)
            .fetch_all(store.pool())
            .await?;
            connection::default_label(
                &crate::accounts::label_of(connector),
                taken.iter().map(String::as_str),
            )
        }
    };
    let id = format!("attempt_{}", uuid::Uuid::now_v7().simple());
    let row = sqlx::query(concat!(
        "insert into connection_attempt
           (id, account_id, connector, label, status, updated_at_ms, plugin_name, pkce_verifier,
            issuer, token_endpoint, client_id, resource, target_connection)
         values ($1, $2, $3, $4, 'pending', $5, $6, $7, $8, $9, $10, $11, $12)
         returning ",
        columns!()
    ))
    .bind(&id)
    .bind(account.as_str())
    .bind(connector)
    .bind(&label)
    .bind(at_ms)
    .bind(plugin)
    .bind(pkce_verifier)
    .bind(&metadata.issuer)
    .bind(&metadata.token_endpoint)
    .bind(client_id)
    .bind(&metadata.resource)
    .bind(target.map(|(id, _)| id))
    .fetch_one(store.pool())
    .await?;
    read(row)
}

/// What an MCP sign-in's callback needs, from its unfinished sign-in. `None` for one that is not an
/// MCP sign-in, or not this account's.
pub async fn mcp_pending(
    store: &PgStore,
    account: &AccountId,
    id: &str,
    redirect_uri: &str,
) -> StoreResult<Option<crate::mcp_oauth::Pending>> {
    let row = sqlx::query(
        "select connector, label, plugin_name, pkce_verifier, issuer, token_endpoint, client_id,
                resource, target_connection
           from connection_attempt
          where id = $1 and account_id = $2 and plugin_name is not null",
    )
    .bind(id)
    .bind(account.as_str())
    .fetch_optional(store.pool())
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(crate::mcp_oauth::Pending {
        plugin: row.try_get("plugin_name")?,
        connector: row.try_get("connector")?,
        label: row.try_get("label")?,
        verifier: row
            .try_get::<Option<String>, _>("pkce_verifier")?
            .unwrap_or_default(),
        issuer: row
            .try_get::<Option<String>, _>("issuer")?
            .unwrap_or_default(),
        token_endpoint: row
            .try_get::<Option<String>, _>("token_endpoint")?
            .unwrap_or_default(),
        client_id: row
            .try_get::<Option<String>, _>("client_id")?
            .unwrap_or_default(),
        resource: row
            .try_get::<Option<String>, _>("resource")?
            .unwrap_or_default(),
        target: row.try_get("target_connection")?,
        redirect_uri: redirect_uri.to_string(),
    }))
}

/// A reopened MCP sign-in starts over at the provider: a new PKCE pair, and the client and
/// endpoints as discovered now.
pub async fn renew_mcp(
    store: &PgStore,
    account: &AccountId,
    id: &str,
    pkce_verifier: &str,
    metadata: &crate::mcp_oauth::Metadata,
    client_id: &str,
    at_ms: i64,
) -> StoreResult<bool> {
    let done = sqlx::query(
        "update connection_attempt set status = 'pending', error = null, updated_at_ms = $3,
           pkce_verifier = $4, issuer = $5, token_endpoint = $6, client_id = $7, resource = $8
          where id = $1 and account_id = $2 and plugin_name is not null",
    )
    .bind(id)
    .bind(account.as_str())
    .bind(at_ms)
    .bind(pkce_verifier)
    .bind(&metadata.issuer)
    .bind(&metadata.token_endpoint)
    .bind(client_id)
    .bind(&metadata.resource)
    .execute(store.pool())
    .await?;
    Ok(done.rows_affected() > 0)
}
