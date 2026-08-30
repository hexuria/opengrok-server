//! The event store, and the projections read from it.
//!
//! ONE APPEND-ONLY TABLE IS THE WHOLE OF THE EVENT SOURCING. No framework: a transcript, a run and
//! an account are all already sequences of things that happened, so the log is the natural grain
//! rather than an imposed one. Reads never replay it — a projection row is updated in the SAME
//! transaction as the append, so a query cannot observe an event that has not yet reached the view.
//!
//! `expected_seq` is what makes concurrent writers safe: two requests refreshing the same session
//! both read version 4, both try to append version 5, and the unique index means exactly one wins.
//! The loser is told `Conflict` and retries against fresh state. Losing that check would let a
//! rotated refresh token be rotated twice.
//!
//! Queries are `sqlx::query` rather than `query!` on purpose: the macros need a live database at
//! COMPILE time, which would put Postgres in the path of `cargo check` and CI for everyone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use opengrok_core::account::{Account, AccountEvent, AccountView};
use opengrok_core::id::AccountId;
use serde::{Serialize, de::DeserializeOwned};

pub mod autonomy;
pub mod migrations;
pub mod postgres;
pub mod vault;

pub use autonomy::{DueSchedule, LogEvent};
pub use postgres::{CredentialUpdate, PgStore};
pub use vault::{Sealed, Vault};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("another writer got there first; re-read and retry")]
    Conflict,
    #[error("the database refused: {0}")]
    Database(String),
    #[error("a stored event could not be read back: {0}")]
    Corrupt(String),
    #[error("the store's lock was poisoned")]
    Poisoned,
}

pub type StoreResult<T> = Result<T, StoreError>;

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        // A unique violation on (stream_id, stream_seq) is not a database fault — it is the
        // optimistic-concurrency check doing its job, and the caller must be able to tell the
        // difference in order to retry rather than fail the request.
        if let sqlx::Error::Database(ref db) = error
            && db.code().as_deref() == Some("23505")
        {
            return Self::Conflict;
        }
        Self::Database(error.to_string())
    }
}

/// One event as it sits in the log.
#[derive(Debug, Clone)]
pub struct StoredEvent<E> {
    pub stream_seq: i64,
    pub event: E,
}

/// Append-only storage for one aggregate's events.
///
/// Generic over the event type so the next domain (runs, transcripts) reuses this rather than
/// growing a second store.
pub trait EventStore: Send + Sync {
    /// Every event for a stream, in order.
    fn read<E: DeserializeOwned>(&self, stream_id: &str) -> StoreResult<Vec<StoredEvent<E>>>;

    /// Append after `expected_seq`. `expected_seq` is the highest sequence the caller saw; 0 means
    /// "the stream does not exist yet". Returns the new highest sequence.
    fn append<E: Serialize>(
        &self,
        stream_id: &str,
        expected_seq: i64,
        events: &[(&str, &E)],
    ) -> StoreResult<i64>;
}

/// One row as the in-memory store keeps it: sequence, event type, payload.
type MemoryRow = (i64, String, serde_json::Value);

/// For tests and for `cargo test` with no database in sight.
#[derive(Debug, Clone, Default)]
pub struct MemoryEventStore {
    streams: Arc<Mutex<HashMap<String, Vec<MemoryRow>>>>,
}

impl MemoryEventStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventStore for MemoryEventStore {
    fn read<E: DeserializeOwned>(&self, stream_id: &str) -> StoreResult<Vec<StoredEvent<E>>> {
        let streams = self.streams.lock().map_err(|_| StoreError::Poisoned)?;
        let Some(rows) = streams.get(stream_id) else {
            return Ok(Vec::new());
        };
        rows.iter()
            .map(|(seq, _, payload)| {
                serde_json::from_value(payload.clone())
                    .map(|event| StoredEvent {
                        stream_seq: *seq,
                        event,
                    })
                    .map_err(|error| StoreError::Corrupt(error.to_string()))
            })
            .collect()
    }

    fn append<E: Serialize>(
        &self,
        stream_id: &str,
        expected_seq: i64,
        events: &[(&str, &E)],
    ) -> StoreResult<i64> {
        let mut streams = self.streams.lock().map_err(|_| StoreError::Poisoned)?;
        let rows = streams.entry(stream_id.to_string()).or_default();
        let current = rows.last().map_or(0, |(seq, _, _)| *seq);
        if current != expected_seq {
            return Err(StoreError::Conflict);
        }
        let mut seq = current;
        for (event_type, event) in events {
            seq += 1;
            let payload = serde_json::to_value(event)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            rows.push((seq, (*event_type).to_string(), payload));
        }
        Ok(seq)
    }
}

/// One stream per coworker.
pub fn coworker_stream(id: &opengrok_core::id::CoworkerId) -> String {
    format!("coworker/{id}")
}

/// One stream per run. Runs and accounts share the `events` table and never the same stream.
pub fn run_stream(id: &opengrok_core::id::RunId) -> String {
    format!("run/{id}")
}

/// One stream per schedule.
pub fn schedule_stream(id: &opengrok_core::id::ScheduleId) -> String {
    format!("schedule/{id}")
}

/// The stream a monitor's events live on.
pub fn monitor_stream(id: &opengrok_core::id::MonitorId) -> String {
    format!("monitor/{id}")
}

/// The account stream's id. One stream per account, keyed by the account id.
pub fn account_stream(id: &AccountId) -> String {
    format!("account/{id}")
}

/// Load an account by replaying its log. Returns the state and the sequence it was read at, which
/// the caller must hand back to `append` — that pairing is the concurrency check.
pub fn load_account<S: EventStore>(store: &S, id: &AccountId) -> StoreResult<(Account, i64)> {
    let stored: Vec<StoredEvent<AccountEvent>> = store.read(&account_stream(id))?;
    let seq = stored.last().map_or(0, |row| row.stream_seq);
    let events: Vec<AccountEvent> = stored.into_iter().map(|row| row.event).collect();
    Ok((Account::replay(&events), seq))
}

/// Append account events at the sequence they were decided against.
pub fn append_account<S: EventStore>(
    store: &S,
    id: &AccountId,
    expected_seq: i64,
    events: &[AccountEvent],
) -> StoreResult<i64> {
    let typed: Vec<(&str, &AccountEvent)> = events
        .iter()
        .map(|event| (event.event_type(), event))
        .collect();
    store.append(&account_stream(id), expected_seq, &typed)
}

/// The projection the read side answers from.
#[derive(Debug, Clone)]
pub struct AccountProjection {
    pub view: AccountView,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use opengrok_core::account::{AccountCommand, Plan};
    use opengrok_core::id::SessionId;

    fn sign_in(at_ms: i64) -> AccountCommand {
        AccountCommand::SignIn {
            email: "a@b.c".to_string(),
            plan: Plan::Pro,
            trial: false,
            session_id: SessionId::from_stored("sess_1"),
            refresh_token_hash: "hash-1".to_string(),
            at_ms,
        }
    }

    #[test]
    fn an_empty_stream_replays_to_an_unregistered_account() {
        let store = MemoryEventStore::new();
        let (account, seq) = load_account(&store, &AccountId::from_stored("acct_1")).unwrap();
        assert!(!account.registered);
        assert_eq!(seq, 0);
    }

    #[test]
    fn appended_events_replay_into_the_same_state() {
        let store = MemoryEventStore::new();
        let id = AccountId::from_stored("acct_1");
        let (account, seq) = load_account(&store, &id).unwrap();
        let events = account.decide(sign_in(10)).unwrap();
        let new_seq = append_account(&store, &id, seq, &events).unwrap();
        assert_eq!(new_seq, 2);

        let (reloaded, seq) = load_account(&store, &id).unwrap();
        assert_eq!(seq, 2);
        assert!(reloaded.registered);
        assert_eq!(reloaded.email, "a@b.c");
    }

    /// The check that stops one refresh token being rotated twice.
    #[test]
    fn appending_at_a_stale_sequence_conflicts() {
        let store = MemoryEventStore::new();
        let id = AccountId::from_stored("acct_1");
        let (account, seq) = load_account(&store, &id).unwrap();
        let events = account.decide(sign_in(10)).unwrap();
        append_account(&store, &id, seq, &events).unwrap();

        // A second writer that read at the same (now stale) sequence must lose.
        let err = append_account(&store, &id, seq, &events).unwrap_err();
        assert!(matches!(err, StoreError::Conflict));
    }

    #[test]
    fn streams_do_not_bleed_into_each_other() {
        let store = MemoryEventStore::new();
        let first = AccountId::from_stored("acct_1");
        let second = AccountId::from_stored("acct_2");
        let (account, seq) = load_account(&store, &first).unwrap();
        append_account(&store, &first, seq, &account.decide(sign_in(10)).unwrap()).unwrap();

        let (other, seq) = load_account(&store, &second).unwrap();
        assert!(!other.registered);
        assert_eq!(seq, 0);
    }
}
