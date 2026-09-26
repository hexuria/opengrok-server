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
