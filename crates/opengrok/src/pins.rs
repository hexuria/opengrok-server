//! The one-time pin (#318, the owner's call): every bot with no `source` of its own went wherever
//! its owner's account setting sent it, and is pinned ONCE to that door and model, so no hidden
//! account `kind` decides anything about a bot afterwards. A bot its owner hands back to none
//! after the pin is a choice, and this never runs again to undo it.
//!
//! A BOOT CHORE IN THE BINARY, NOT A STORE MIGRATION. It writes each pin as coworker events through
//! the aggregate (`Repin`, `SetSource`, as an owner's PATCH does) and the one projection mapping
//! (`CoworkerView::of`), at each bot's own seq; it reads each owner's replayed setting; and its one
//! exception is decided by `subscription_model`, the one allowlist. SQL in `SCHEMA` could do none
//! of the three without a second spelling of the events, the row and the list. Its record is still
//! a row in `schema_migrations` (docs/setup/postgres.md), written once every bot is pinned.

use std::collections::HashMap;

use opengrok_core::coworker::{CoworkerCommand, CoworkerError, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_core::inference::{InferenceSource, SourceKind, Via, subscription_model};
use opengrok_store::{PgStore, StoreError};

/// The pass's row in `schema_migrations`: present, it has run, and it never runs again.
pub const MARKER: &str = "null-source-bots-are-pinned";

/// Held around the pass, so two replicas booting at once pin once: the second waits, then finds
/// the row. Its own key, never the schema's (`opengrok_store::migrations`), released before this.
const LOCK: i64 = 0x6F_67_70_69_6E_73; // "ogpins"

/// Pin every live bot with no `source`, unless `marker`'s row says this ran, and write the row once
/// each is pinned. How many it pinned, or `None` when it had run. Run before the listener, so this
/// process serves nobody mid-pass; a pass that stops early writes no row, and the next boot
/// finishes it, each bot pinned already having a source by then.
pub async fn pin_once(store: &PgStore, marker: &str) -> Result<Option<usize>, StoreError> {
    let mut session = store.pool().acquire().await?;
    let lock = sqlx::query("select pg_advisory_lock($1)").bind(LOCK);
    lock.execute(&mut *session).await?;
    let pinned = pass(store, &mut session, marker).await;
    // Released even when the pass failed: held, it would stall every other replica's boot.
    let unlock = sqlx::query("select pg_advisory_unlock($1)").bind(LOCK);
    let released = unlock.execute(&mut *session).await;
    let pinned = pinned?;
    released?;
    Ok(pinned)
}

async fn pass(
    store: &PgStore,
    session: &mut sqlx::PgConnection,
    marker: &str,
) -> Result<Option<usize>, StoreError> {
    let ran = "select exists (select 1 from schema_migrations where name = $1)";
    let ran: bool = sqlx::query_scalar(ran)
        .bind(marker)
        .fetch_one(&mut *session)
        .await?;
    if ran {
        return Ok(None);
    }
    // The roster's word for which bots have none, and its sort key, which a pin must not move:
    // each is read again from its own log before anything is decided.
    let bots: Vec<(String, String, i64)> = sqlx::query_as(
        "select id, account_id, updated_at_ms from coworker_view
          where source is null and not retired order by updated_at_ms",
    )
    .fetch_all(&mut *session)
    .await?;
    let count = bots.len();
    tracing::info!(
        bots = count,
        "pin: pinning each bot with no source to its door, once (#318)"
    );
    let mut settings = HashMap::new();
    let mut pinned = 0;
    for (id, owner, stamp) in bots {
        let (id, owner) = (CoworkerId::from_stored(id), AccountId::from_stored(owner));
        let done = match setting_of(store, &mut settings, &owner).await {
            Ok(setting) => pin(store, (&id, &owner), stamp, &setting).await,
            Err(error) => Err(error),
        };
        match done {
            Ok(done) => pinned += usize::from(done),
            // A log that cannot be read back fails the same way at every boot. Waited on, it would
            // keep the row unwritten, and every boot would pin again what owners had since chosen.
            Err(StoreError::Corrupt(why)) => {
                tracing::error!(coworker = %id, owner = %owner, %why, "pin: a bot with no source could not be read, so it is not pinned, and is not tried again");
            }
            Err(error) => return Err(error),
        }
    }
    let done = sqlx::query("insert into schema_migrations (name) values ($1)").bind(marker);
    done.execute(&mut *session).await?;
    tracing::info!(
        pinned,
        "pin: every bot with no source is pinned; the pass never runs again"
    );
    Ok(Some(pinned))
}

/// An owner's setting, read from their log once for all their bots.
async fn setting_of(
    store: &PgStore,
    settings: &mut HashMap<AccountId, InferenceSource>,
    owner: &AccountId,
) -> Result<InferenceSource, StoreError> {
    if let Some(setting) = settings.get(owner) {
        return Ok(setting.clone());
    }
    let (account, _) = store.load_account(owner).await?;
    settings.insert(owner.clone(), account.inference_source.clone());
    Ok(account.inference_source)
}

/// One bot, decided on its own log, which is the truth: pinned only while it is alive, not a group
/// and still with no source, at the roster `stamp` it had. A write that lost to another is read and
/// decided again. Whether it was pinned.
async fn pin(
    store: &PgStore,
    (id, owner): (&CoworkerId, &AccountId),
    stamp: i64,
    setting: &InferenceSource,
) -> Result<bool, StoreError> {
    let refused = |error: CoworkerError| StoreError::Corrupt(format!("it refuses a pin: {error}"));
    for _ in 0..3 {
        let (bot, seq) = store.load_coworker(id).await?;
        if !bot.hired || bot.retired || bot.is_group() || bot.source.is_some() {
            return Ok(false);
        }
        let (source, model, why) = door(setting, &bot.model);
        let at_ms = chrono::Utc::now().timestamp_millis();
        let mut events = Vec::new();
        if model != bot.model {
            let repin = CoworkerCommand::Repin {
                model: model.clone(),
                at_ms,
            };
            events.extend(bot.decide(repin).map_err(refused)?);
        }
        let set = CoworkerCommand::SetSource {
            source: Some(source),
            at_ms,
        };
        events.extend(bot.decide(set).map_err(refused)?);
        let mut after = bot.clone();
        for event in &events {
            after.apply(event);
        }
        let view = CoworkerView::of(id.clone(), &after, stamp);
        match store.append_coworker(id, owner, seq, &events, &view).await {
            Err(StoreError::Conflict) => continue,
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        let (source, was) = (source.as_str(), &bot.model);
        match why {
            None => {
                tracing::info!(coworker = %id, owner = %owner, source, %model, %was, "pin: pinned a bot to the door its turns took")
            }
            Some(why) => {
                tracing::warn!(coworker = %id, owner = %owner, %model, %why, "pin: pinned a bot to the gateway on its own pin, as its owner's plan model cannot be pinned")
            }
        }
        return Ok(true);
    }
    Err(StoreError::Conflict)
}

/// The door and model a bot with no source of its own had under its owner's `setting`, resolved as
/// a turn that picks nothing resolves (`InferenceSource::resolve`): the gateway on its own `pin`,
/// or the person's plan on the setting's model for the way it went, `localModel` on the loopback
/// and `relay.localModel` by the Mac. A plan model a subscription may not answer, or none at all,
/// cannot be pinned: the bot goes to the gateway on its own pin instead, and the third value says
/// why. How hard it thinks is never touched.
fn door(setting: &InferenceSource, pin: &str) -> (SourceKind, String, Option<String>) {
    let model = match setting.resolve(None) {
        (SourceKind::Gateway, _) => return (SourceKind::Gateway, pin.to_string(), None),
        (SourceKind::LocalProxy, Via::Loopback) => setting.local_model.as_deref(),
        (SourceKind::LocalProxy, Via::Mac) => setting.relay_model.as_deref(),
    };
    let model = model.unwrap_or_default();
    match subscription_model(model) {
        Ok(()) => (SourceKind::LocalProxy, model.trim().to_string(), None),
        Err(why) => (SourceKind::Gateway, pin.to_string(), Some(why)),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[path = "../tests/unit/pins.rs"]
mod tests;
