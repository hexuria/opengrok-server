//! Persistence is account scoped at every read and write. There is no lending operation: a token
//! pasted for an install is an account of its person's (#359), which never serves anyone else.
use crate::accounts;
use crate::registry::{Catalog, Entry};
use opengrok_core::connection::{
    self, Connection, ConnectionCommand, ConnectionKind, ConnectionView, Owner, Resolved,
};
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
/// Which of `account`'s pasted accounts each install holds, as `(plugin, connector, connection)`
/// (#359): a token account is one install's, and `GET /connections` cannot say whose. Live accounts
/// only, in a stable order.
pub async fn bindings(
    store: &PgStore,
    account: &AccountId,
) -> StoreResult<Vec<(String, String, String)>> {
    let rows = sqlx::query_as(
        "select c.plugin_name, c.connector, c.connection_id
           from plugin_credential c
           join connection_view v on v.id = c.connection_id and not v.disconnected
          where c.account_id = $1
          order by c.plugin_name, c.connector, v.updated_at_ms, c.connection_id",
    )
    .bind(account.as_str())
    .fetch_all(store.pool())
    .await?;
    Ok(rows)
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
/// `for_turn`, reading only each plugin's skills: `(name, revision, skills)`.
pub async fn skills_for_turn(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
) -> StoreResult<Vec<(String, String, std::collections::BTreeMap<String, String>)>> {
    let rows = sqlx::query(
        "select p.name, p.revision, p.bundle->'skills' as skills from plugin_installation p
        join coworker_view c on c.account_id = p.account_id
        where p.account_id = $1 and c.id = $2 and c.retired = false order by p.name",
    )
    .bind(account.as_str())
    .bind(bot.as_str())
    .fetch_all(store.pool())
    .await?;
    let mut out = Vec::new();
    for row in rows {
        let name: String = row.try_get("name")?;
        let skills = serde_json::from_value(row.try_get("skills")?);
        match skills {
            Ok(skills) => out.push((name, row.try_get("revision")?, skills)),
            Err(error) => {
                tracing::warn!(name, %error, "an installed plugin's skills are unreadable")
            }
        }
    }
    Ok(out)
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
    // The accounts pasted for this install go with it, as its credentials always did (#356): an
    // update is an uninstall then an install, and a token pasted for one URL must not reach a
    // replacement's. Removed, each loses its secret and the pins naming it (`append_connection_in`).
    let bound: Vec<String> = sqlx::query_scalar(
        "select connection_id from plugin_credential
          where account_id = $1 and plugin_name = $2 and connection_id is not null",
    )
    .bind(account.as_str())
    .bind(name)
    .fetch_all(&mut *tx)
    .await?;
    for id in bound {
        accounts::remove_in(store, &mut tx, &id, at_ms).await?;
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
         join coworker_view c on c.id = g.coworker_id where c.account_id = $1
         order by g.coworker_id, g.principal_id for update of g",
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
         join coworker_view c on c.id = t.coworker_id where c.account_id = $1
         order by t.coworker_id for update of t",
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

/// Save a token pasted for one of an install's connectors: an account of the person's for that
/// service, of kind `token` (#359), which the install's binding names. Pasted again it refreshes
/// that account; pasted after the person removed that account, it starts a new one.
///
/// ALL UNDER THE INSTALL'S ROW LOCK, the account's events, its secret and the binding, so an
/// uninstall cannot land between them and leave a live account whose install is gone.
pub async fn credential(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    name: &str,
    connector: &str,
    token: &str,
    at_ms: i64,
) -> StoreResult<bool> {
    let saved = save(
        store,
        vault,
        account,
        name,
        connector,
        token,
        at_ms,
        Save::Replace(None),
    );
    Ok(saved.await?.is_some())
}

/// How a pasted token lands (#359, D3): adding always makes another account, as a sign-in does,
/// and replacing names the account whose token it is. `Replace(None)` is the route before several
/// accounts per service: the only account, or the first.
pub enum Save<'a> {
    Add,
    Replace(Option<&'a str>),
}

/// Save a pasted token for an installed plugin's connector. `None` when there is no such install,
/// connector or (for a named replace) account of this install's; `Conflict` for an unnamed replace
/// when the install has several accounts for the connector, since which one is meant cannot be
/// told. The reply is the account's id.
#[allow(clippy::too_many_arguments)]
pub async fn save(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    name: &str,
    connector: &str,
    token: &str,
    at_ms: i64,
    how: Save<'_>,
) -> StoreResult<Option<String>> {
    let mut tx = store.pool().begin().await?;
    let row = sqlx::query(
        "select bundle from plugin_installation where account_id = $1 and name = $2 for update",
    )
    .bind(account.as_str())
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let bundle: Bundle = serde_json::from_value(row.try_get("bundle")?)
        .map_err(|_| StoreError::Corrupt("invalid bundle".into()))?;
    if !bundle.connectors().iter().any(|c| c == connector) {
        return Ok(None);
    }
    let bound: Vec<(Option<String>, String)> = sqlx::query_as(
        "select connection_id, secret_id from plugin_credential
          where account_id = $1 and plugin_name = $2 and connector = $3
          order by secret_id",
    )
    .bind(account.as_str())
    .bind(name)
    .bind(connector)
    .fetch_all(&mut *tx)
    .await?;
    // The binding this token replaces, if any: its account and the secret id it is sealed under.
    let replacing = match how {
        Save::Add => None,
        Save::Replace(Some(target)) => {
            match bound.iter().find(|(id, _)| id.as_deref() == Some(target)) {
                Some(found) => Some(found.clone()),
                None => return Ok(None),
            }
        }
        Save::Replace(None) => match bound.as_slice() {
            [] => None,
            [only] => Some(only.clone()),
            _ => return Err(StoreError::Conflict),
        },
    };
    let (mut id, mut connection, mut seq) = (String::new(), Connection::default(), 0);
    if let Some((Some(bound), _)) = &replacing {
        (connection, seq) = store.load_connection(bound).await?;
        id = bound.clone();
    }
    let command = if connection.connected && !connection.disconnected {
        ConnectionCommand::Refresh { at_ms }
    } else {
        // A removed account keeps its history to itself; this token starts a new one.
        let owner = Owner::User(account.clone());
        (id, connection, seq) = (
            accounts::new_id(connector, &owner),
            Connection::default(),
            0,
        );
        let label = accounts::new_label(&mut *tx, &owner, connector).await?;
        let (connector, owner) = (connector.to_string(), account.clone());
        ConnectionCommand::ConnectToken {
            connector,
            owner,
            label,
            at_ms,
        }
    };
    let events = connection
        .decide(command)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    for event in &events {
        connection.apply(event);
    }
    let none = opengrok_store::CredentialUpdate::none(at_ms);
    PgStore::append_connection_in(&mut tx, &id, seq, &events, &connection, &none).await?;
    // A replaced token is sealed under the id it always had, which its binding names: the seal's
    // associated data, so a token saved before #359 opens where it lies. An added account gets an
    // id of its own beside them, the first under the id an install's only token always had.
    let secret_id = match &replacing {
        Some((_, secret_id)) => secret_id.clone(),
        None if bound.is_empty() => format!("plugin/{account}/{name}/{connector}"),
        None => format!("plugin/{account}/{name}/{connector}/{id}"),
    };
    let sealed = vault.seal(&secret_id, token)?;
    sqlx::query("insert into secret_store(id,nonce,ciphertext,key_id,updated_at_ms) values($1,$2,$3,$4,$5)
        on conflict(id) do update set nonce=excluded.nonce,ciphertext=excluded.ciphertext,key_id=excluded.key_id,updated_at_ms=excluded.updated_at_ms")
        .bind(&secret_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(sealed.key_id).bind(at_ms).execute(&mut *tx).await?;
    sqlx::query("insert into plugin_credential(account_id,plugin_name,connector,secret_id,connection_id) values($1,$2,$3,$4,$5)
        on conflict (secret_id) do update set connection_id = excluded.connection_id")
        .bind(account.as_str()).bind(name).bind(connector).bind(&secret_id).bind(&id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// An account a "Which account?" card lists: `(id, label, kind)`.
pub type AccountChoice = (String, String, String);

/// What one turn of a Bot gets for an install: a token for each connector that resolves, and the
/// connectors where several accounts would do and no pin says which (#360 asks the person).
#[derive(Debug, Default)]
pub struct TurnCredentials {
    pub values: std::collections::BTreeMap<String, String>,
    pub needs_choice: Vec<String>,
    /// For each connector in `needs_choice`, the accounts the person chooses between, as a card
    /// lists them: `(id, label, kind)`. Never a secret.
    pub choices: Vec<(String, Vec<AccountChoice>)>,
    /// The connectors this Bot has no account for at all.
    pub missing: Vec<String>,
}

/// The tokens `bot` uses for `installation`'s connectors (#359): of the person's pasted accounts
/// for each service, the Bot's pin or the only one (`connection::resolve`), and never a sign-in,
/// which an installed plugin's third-party server must not be handed.
///
/// None once the install has been uninstalled, even if the same name was installed again since,
/// and none on a Bot `account` does not own: a member's turn on a shared Bot uses no account-owned
/// token, the owner's or the member's. Accounts, ciphertext and snapshot eligibility are read in
/// one statement. Reading secret ids first and opening them later allowed uninstall/reinstall to
/// replace the secret between those two statements.
pub async fn values_for_installation(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    bot: &CoworkerId,
    installation: &Installation,
) -> StoreResult<TurnCredentials> {
    let none = std::collections::BTreeMap::new();
    values_choosing(store, vault, account, bot, installation, &none).await
}

/// [`values_for_installation`] with the account the person picked on a "Which account?" card for
/// THIS turn, by connector (`forwardedProps.pluginAccounts`). It outranks the Bot's pin for the
/// turn and is never stored; Remember on the card stores a pin through its own route. A pick that
/// is not one of the candidates read below (somebody else's id, a removed account) picks nothing.
pub async fn values_choosing(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    bot: &CoworkerId,
    installation: &Installation,
    chosen: &std::collections::BTreeMap<String, String>,
) -> StoreResult<TurnCredentials> {
    let connectors = installation.bundle.connectors();
    let rows = sqlx::query(
        "select v.id, v.connector, v.label, v.updated_at_ms, v.expires_at_ms, v.kind, c.secret_id,
                s.nonce, s.ciphertext, s.key_id,
                (select b.connection_id from bot_connection_pin b
                  where b.coworker_id = w.id and b.connector = v.connector) as pinned
           from plugin_installation p
           join coworker_view w on w.id = $4 and w.account_id = p.account_id and not w.retired
           join connection_view v on v.scope = 'user' and v.owner_id = p.account_id
                and v.kind in ('token', 'mcp') and not v.disconnected
           join plugin_credential c on c.connection_id = v.id
                and c.account_id = p.account_id and c.plugin_name = p.name
           join secret_store s on s.id = c.secret_id
          where p.account_id = $1 and p.name = $2 and p.incarnation = $3
            and v.connector = any($5)",
    )
    .bind(account.as_str())
    .bind(&installation.name)
    .bind(&installation.incarnation)
    .bind(bot.as_str())
    .bind(&connectors)
    .fetch_all(store.pool())
    .await?;
    let (mut candidates, mut sealed, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        let id: String = row.try_get("id")?;
        let connector: String = row.try_get("connector")?;
        if let Some(pinned) = row.try_get::<Option<String>, _>("pinned")? {
            pins.push((connector.clone(), pinned));
        }
        let secret = opengrok_store::Sealed {
            nonce: row.try_get("nonce")?,
            ciphertext: row.try_get("ciphertext")?,
            key_id: row.try_get("key_id")?,
        };
        sealed.push((id.clone(), row.try_get::<String, _>("secret_id")?, secret));
        candidates.push(ConnectionView {
            id,
            connector,
            owner: Owner::User(account.clone()),
            label: row.try_get("label")?,
            loans: Default::default(),
            updated_at_ms: row.try_get("updated_at_ms")?,
            expires_at_ms: row.try_get("expires_at_ms")?,
            kind: ConnectionKind::from_word(&row.try_get::<String, _>("kind")?)
                .unwrap_or(ConnectionKind::Token),
        });
    }
    let mut turn = TurnCredentials::default();
    for connector in &connectors {
        let pin = pins.iter().find(|(pinned, _)| pinned == connector);
        let pin = chosen
            .get(connector)
            .map(String::as_str)
            .or(pin.map(|(_, id)| id.as_str()));
        // The Bot is `account`'s: the statement above read nothing for one it is not.
        match connection::resolve(&candidates, connector, bot, account, pin) {
            Resolved::Use(chosen) => {
                if let Some((_, secret_id, secret)) =
                    sealed.iter().find(|(id, ..)| *id == chosen.id)
                {
                    let token = vault.open(secret_id, secret)?;
                    turn.values
                        .insert(opengrok_plugins::token_key(connector), token);
                }
            }
            Resolved::NeedsChoice(eligible) => {
                turn.needs_choice.push(connector.clone());
                let listed = eligible.iter().map(|view| {
                    let kind = view.kind.word().to_string();
                    (view.id.clone(), view.label.clone(), kind)
                });
                // Oldest first, the same every time (ids are time-ordered): a card that shuffled
                // its accounts between two asks would put a different one under the same tap.
                let mut listed: Vec<AccountChoice> = listed.collect();
                listed.sort_by(|a, b| a.0.cmp(&b.0));
                turn.choices.push((connector.clone(), listed));
            }
            Resolved::None => turn.missing.push(connector.clone()),
        }
    }
    Ok(turn)
}

#[cfg(test)]
#[path = "../tests/unit/installed.rs"]
mod tests;
