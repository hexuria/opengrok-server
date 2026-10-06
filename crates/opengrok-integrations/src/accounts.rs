//! Several accounts for one service, and the one each Bot uses (#359): its pins, where a sign-in
//! lands, the label a new account starts with, and the accounts the deployment's own plugins use
//! on a turn. Beside the installs because a pasted token is one of these accounts too
//! (`installed::credential`), resolved by the same rule.
use std::collections::BTreeMap;

use opengrok_core::connection::{
    self, Connection, ConnectionCommand, ConnectionError, ConnectionKind, ConnectionView, Owner,
    Resolved,
};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_store::{PgStore, StoreError, StoreResult};
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// What a person reads for a connector. Its name is lowercase and sometimes clipped (`gdrive`),
/// and an app has nowhere else to learn "Google Drive"; a name the table lacks reads as itself
/// with a capital. A new account starts with it as its label (`new_label`).
pub fn label_of(name: &str) -> String {
    let label = match name {
        "gmail" => "Gmail",
        "gdrive" => "Google Drive",
        "gcal" => "Google Calendar",
        "gdocs" => "Google Docs",
        "gsheets" => "Google Sheets",
        "github" => "GitHub",
        "gitlab" => "GitLab",
        "onedrive" => "OneDrive",
        _ => {
            let mut chars = name.chars();
            let first = chars.next().map(|first| first.to_uppercase());
            return first.into_iter().flatten().chain(chars).collect();
        }
    };
    label.to_string()
}

/// Which account a Bot uses for a service, as `GET /connections/pins` lists it and
/// `PUT /coworkers/{id}/pins/{connector}` answers it, camelCase like the rest of the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub coworker_id: String,
    pub connector: String,
    pub connection_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PinError {
    /// Not a live Bot of the caller's. One answer for "none" and "not yours", so an id reveals
    /// nothing.
    #[error("no such coworker")]
    NoSuchBot,
    #[error("that account is not one this Bot can use for {0}")]
    Unusable(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Every pin on the caller's live Bots. An ARRAY, always: no pin is an answer.
pub async fn pins(store: &PgStore, account: &AccountId) -> StoreResult<Vec<Pin>> {
    let rows = sqlx::query(
        "select p.coworker_id, p.connector, p.connection_id from bot_connection_pin p
           join coworker_view c on c.id = p.coworker_id
          where c.account_id = $1 and not c.retired
          order by p.coworker_id, p.connector",
    )
    .bind(account.as_str())
    .fetch_all(store.pool())
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Pin {
                coworker_id: row.try_get("coworker_id")?,
                connector: row.try_get("connector")?,
                connection_id: row.try_get("connection_id")?,
            })
        })
        .collect()
}

/// Pins are the owner's to set. Even a member who may talk to a shared Bot cannot choose whose
/// account it acts as.
async fn own_bot(store: &PgStore, account: &AccountId, bot: &CoworkerId) -> Result<(), PinError> {
    let own = "select 1 from coworker_view where id = $1 and account_id = $2 and not retired";
    let found: Option<i32> = sqlx::query_scalar(own)
        .bind(bot.as_str())
        .bind(account.as_str())
        .fetch_optional(store.pool())
        .await
        .map_err(StoreError::from)?;
    found.map(|_| ()).ok_or(PinError::NoSuchBot)
}

/// Pin `bot` to one account for `connector`: only one its owner's own turn could use for that
/// service (`ConnectionView::usable_by`), so a pin never reaches past a loan, a scope or a kind.
pub async fn pin(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    connector: &str,
    connection_id: &str,
) -> Result<Pin, PinError> {
    own_bot(store, account, bot).await?;
    let candidates = store.connections_for(account, bot).await?;
    let usable = candidates.iter().any(|candidate| {
        candidate.id == connection_id
            && candidate.connector == connector
            && candidate.usable_by(bot, account)
    });
    if !usable {
        return Err(PinError::Unusable(label_of(connector)));
    }
    let mut tx = store.pool().begin().await.map_err(StoreError::from)?;
    // Hold the account row until the pin is written: disconnect updates that same row before
    // clearing pins, so a concurrent choice cannot put a removed account back on the Bot.
    let locked: Option<String> = sqlx::query_scalar(
        "select id from connection_view where id = $1 and not disconnected for update",
    )
    .bind(connection_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::from)?;
    if locked.is_none() {
        return Err(PinError::Unusable(label_of(connector)));
    }
    sqlx::query(
        "insert into bot_connection_pin (coworker_id, connector, connection_id) values ($1, $2, $3)
         on conflict (coworker_id, connector) do update set connection_id = excluded.connection_id",
    )
    .bind(bot.as_str())
    .bind(connector)
    .bind(connection_id)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::from)?;
    tx.commit().await.map_err(StoreError::from)?;
    Ok(Pin {
        coworker_id: bot.to_string(),
        connector: connector.to_string(),
        connection_id: connection_id.to_string(),
    })
}

/// Back to the only account the Bot may use, or to a choice when it may use several. A Bot with
/// no pin for `connector` is already there, so this is not an error.
pub async fn unpin(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    connector: &str,
) -> Result<(), PinError> {
    own_bot(store, account, bot).await?;
    sqlx::query("delete from bot_connection_pin where coworker_id = $1 and connector = $2")
        .bind(bot.as_str())
        .bind(connector)
        .execute(store.pool())
        .await
        .map_err(StoreError::from)?;
    Ok(())
}

/// A Bot's pins, by connector.
async fn pins_of(store: &PgStore, bot: &CoworkerId) -> StoreResult<BTreeMap<String, String>> {
    let pinned = "select connector, connection_id from bot_connection_pin where coworker_id = $1";
    let rows: Vec<(String, String)> = sqlx::query_as(pinned)
        .bind(bot.as_str())
        .fetch_all(store.pool())
        .await?;
    Ok(rows.into_iter().collect())
}

/// The account the deployment's own plugins use for each service on this turn: the Bot's pin, or
/// the only one it may use (`connection::resolve`).
///
/// SIGN-INS ONLY. A pasted token serves the installed plugin it was pasted for and the person's
/// other installs (`installed::values_for_installation`), never the deployment's plugins, which
/// never saw one; and an installed plugin never sees a sign-in. A service with several accounts
/// and no pin sits this turn out, as one whose server will not connect does, until #360 asks the
/// person which.
pub async fn for_operator_plugins(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
) -> Vec<ConnectionView> {
    let read = async {
        let candidates = store.connections_for(account, bot).await?;
        let owner = store.coworker_owner(bot).await?;
        Ok::<_, StoreError>((candidates, owner, pins_of(store, bot).await?))
    };
    let (candidates, owner, pins) = match read.await {
        Ok((candidates, Some(owner), pins)) => (candidates, owner, pins),
        Ok((_, None, _)) => return Vec::new(),
        Err(error) => {
            tracing::warn!(%error, %bot, "a Bot's accounts could not be read; its plugins go on without them");
            return Vec::new();
        }
    };
    let candidates: Vec<ConnectionView> = candidates
        .into_iter()
        .filter(|candidate| candidate.kind == ConnectionKind::Oauth)
        .collect();
    let connectors: std::collections::BTreeSet<&str> = candidates
        .iter()
        .map(|candidate| candidate.connector.as_str())
        .collect();
    let mut chosen = Vec::new();
    for connector in connectors {
        let pin = pins.get(connector).map(String::as_str);
        match connection::resolve(&candidates, connector, bot, &owner, pin) {
            Resolved::Use(account) => chosen.push(account.clone()),
            Resolved::NeedsChoice(several) => {
                let accounts = several.len();
                tracing::info!(%bot, connector, accounts, "a Bot may use several accounts for a service and none is pinned; its plugins go without it this turn");
            }
            Resolved::None => {}
        }
    }
    chosen
}

/// Where a sign-in lands (#359): the account a reconnect named, to be refreshed with its label
/// kept, or a new one with `label`.
pub struct Landing {
    pub id: String,
    pub connection: Connection,
    pub seq: i64,
    pub label: String,
}

/// A plain sign-in always adds an account. A reconnect refreshes the one it named, unless that one
/// went while the person was at the provider: then it adds one rather than reviving what they
/// removed.
pub async fn landing(
    store: &PgStore,
    claims: &crate::oauth::StateClaims,
    owner: &Owner,
) -> StoreResult<Landing> {
    let connector = claims.connector.as_str();
    if let Some(id) = claims.connection.as_deref() {
        let (connection, seq) = store.load_connection(id).await?;
        let same = connection.connected
            && !connection.disconnected
            && connection.kind == ConnectionKind::Oauth
            && connection.connector == connector
            && connection.owner.as_ref() == Some(owner);
        if same {
            let label = connection.label.clone();
            let id = id.to_string();
            return Ok(Landing {
                id,
                connection,
                seq,
                label,
            });
        }
    }
    Ok(Landing {
        id: new_id(connector, owner),
        connection: Connection::default(),
        seq: 0,
        label: new_label(store.pool(), owner, connector).await?,
    })
}

/// A new account's id. Its owner's id stays in it, as it was in every id before #359: the purge
/// finds a connection's sealed secrets by it (`opengrok/src/purge.rs`).
pub fn new_id(connector: &str, owner: &Owner) -> String {
    let owner = match owner {
        Owner::User(account) => account.to_string(),
        Owner::Bot(bot) => bot.to_string(),
        Owner::Global => "global".to_string(),
    };
    format!("conn_{connector}_{owner}_{}", uuid::Uuid::now_v7().simple())
}

/// The label a new account of `owner`'s for `connector` starts with (`connection::default_label`).
pub async fn new_label<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    owner: &Owner,
    connector: &str,
) -> StoreResult<String> {
    let (scope, owner_id) = match owner {
        Owner::User(account) => ("user", Some(account.as_str())),
        Owner::Bot(bot) => ("bot", Some(bot.as_str())),
        Owner::Global => ("global", None),
    };
    let taken: Vec<String> = sqlx::query_scalar(
        "select label from connection_view
          where scope = $1 and owner_id is not distinct from $2 and connector = $3
            and not disconnected",
    )
    .bind(scope)
    .bind(owner_id)
    .bind(connector)
    .fetch_all(executor)
    .await?;
    let taken = taken.iter().map(String::as_str);
    Ok(connection::default_label(&label_of(connector), taken))
}

/// What `GET /connections/{id}/reconnect` may send a person to refresh (#359).
pub enum Reconnect {
    /// A live sign-in of the caller's, or a Bot's own for that Bot's owner: its connector, and the
    /// Bot when it is a Bot's.
    Link {
        connector: String,
        bot: Option<CoworkerId>,
    },
    /// None, gone, or somebody else's: one answer, so an id reveals nothing.
    NotFound,
    /// A pasted token, which is replaced by pasting another, never by signing in.
    Token,
}

pub async fn reconnectable(
    store: &PgStore,
    caller: &AccountId,
    id: &str,
) -> StoreResult<Reconnect> {
    let (connection, _) = store.load_connection(id).await?;
    let bot = match &connection.owner {
        Some(Owner::User(owner)) if owner == caller => None,
        Some(Owner::Bot(bot)) => Some(bot.clone()),
        _ => return Ok(Reconnect::NotFound),
    };
    let bot_owner = match &bot {
        Some(bot) => store.coworker_owner(bot).await?,
        None => Some(caller.clone()),
    };
    if !connection.connected || connection.disconnected || bot_owner.as_ref() != Some(caller) {
        return Ok(Reconnect::NotFound);
    }
    if connection.kind == ConnectionKind::Token {
        return Ok(Reconnect::Token);
    }
    let connector = connection.connector;
    Ok(Reconnect::Link { connector, bot })
}

#[derive(Debug, thiserror::Error)]
pub enum RenameError {
    /// None, or somebody else's: one answer, so an id reveals nothing.
    #[error("no such connection")]
    NotFound,
    #[error(transparent)]
    Refused(#[from] ConnectionError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Rename `caller`'s account (#359), so they can tell two for one service apart. The aggregate
/// trims the label and refuses one that is empty or too long (`connection::tidy_label`). The
/// reply is the account as `GET /connections` lists it.
pub async fn rename(
    store: &PgStore,
    caller: &AccountId,
    id: &str,
    label: String,
    at_ms: i64,
) -> Result<ConnectionView, RenameError> {
    let (mut connection, seq) = store.load_connection(id).await?;
    // The owner check every connection route makes (the server's `mutate`): an id alone must
    // never be enough to rename somebody else's account.
    let owned = matches!(&connection.owner, Some(Owner::User(owner)) if owner == caller);
    if !connection.connected || !owned {
        return Err(RenameError::NotFound);
    }
    let events = connection.decide(ConnectionCommand::Rename { label, at_ms })?;
    for event in &events {
        connection.apply(event);
    }
    let none = opengrok_store::CredentialUpdate::none(at_ms);
    store
        .append_connection(id, seq, &events, &connection, &none)
        .await?;
    let listed = store.connections_owned_by(caller).await?;
    let renamed = listed.into_iter().find(|view| view.id == id);
    renamed.ok_or(RenameError::NotFound)
}

/// Remove an account in `tx`, the secret and every pin naming it with it
/// (`PgStore::append_connection_in`). One already removed has nothing left to take. Shared by an
/// uninstall, which takes the tokens pasted for its install, and a provider's revocation.
pub async fn remove_in(
    store: &PgStore,
    tx: &mut sqlx::PgConnection,
    id: &str,
    at_ms: i64,
) -> StoreResult<()> {
    let (mut connection, seq) = store.load_connection(id).await?;
    let Ok(events) = connection.decide(ConnectionCommand::Disconnect { at_ms }) else {
        return Ok(());
    };
    for event in &events {
        connection.apply(event);
    }
    let none = opengrok_store::CredentialUpdate::none(at_ms);
    PgStore::append_connection_in(tx, id, seq, &events, &connection, &none).await?;
    Ok(())
}
