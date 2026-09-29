use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::*;
use opengrok_core::account::{Account, AccountCommand, AccountEvent, Plan};
use opengrok_core::id::SessionId;
use serde::{Serialize, de::DeserializeOwned};

// The sync event store and the account helpers over it. Nothing the server runs calls them — it
// reads and appends through `PgStore` — so they live beside the only tests that do (#270).

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
