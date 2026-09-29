//! A connection's owner is written and read back for every scope (#267).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use opengrok_core::connection::Owner;
use opengrok_core::id::{AccountId, CoworkerId};
use serde_json::json;

#[test]
fn every_owner_is_written_with_one_id_key() {
    let cases = [
        (Owner::Global, json!({ "scope": "global" })),
        (
            Owner::User(AccountId::from_stored("acct_1".to_string())),
            json!({ "scope": "user", "id": "acct_1" }),
        ),
        (
            Owner::Bot(CoworkerId::from_stored("cw_1".to_string())),
            json!({ "scope": "bot", "id": "cw_1" }),
        ),
    ];
    for (owner, wire) in cases {
        assert_eq!(serde_json::to_value(&owner).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Owner>(wire).unwrap(), owner);
    }
}

/// Only `Global` could ever be written before #267, and it reads exactly as it was stored.
#[test]
fn a_global_owner_from_the_old_log_still_reads() {
    let old: Owner = serde_json::from_str(r#"{"scope":"global"}"#).unwrap();
    assert_eq!(old, Owner::Global);
}
