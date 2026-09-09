//! A revoked key is not a key — at every place that asks "can this coworker be metered?"
//!
//! `PgStore::coworker_key` returns the row whatever its state, deliberately: the row must outlive
//! revocation so a member's month still counts toward their pool, and `against_member_keys.rs`
//! asserts exactly that. The consequence is that every CALLER meaning "does this coworker have a
//! usable credential" has to say so, and five of eight did not.
//!
//! Retiring a coworker revokes its keys (`spend.rs`, `mark_coworker_keys_revoked`). After that,
//! an unfiltered caller reports a credential the gateway has already been told to reject: the mint
//! check says `Minted`, dispatch presents it and earns a 401 with no explanation, the meter holds a
//! turn on a reading that can never arrive, and — worst, because it is the guard against exactly
//! this — `set_limit` accepts a cap it cannot count.
//!
//! That last one shipped in #76, in the function written to refuse uncountable caps. It is the
//! reason this file leads with it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_store::PgStore;
use opengrok_store::gateway::CoworkerKeyView;
use serde_json::json;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: String::new(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let view = AccountView {
        id: id.clone(),
        email: email.to_string(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
        org_id: None,
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
    id
}

async fn seed_coworker(store: &PgStore, account: &AccountId, name: &str) -> CoworkerId {
    let id = CoworkerId::new();
    let at_ms = now_ms();
    let mut coworker = Coworker::default();
    let events = coworker
        .decide(CoworkerCommand::Hire {
            name: name.to_string(),
            model: "oag/cheap".to_string(),
            at_ms,
        })
        .expect("hire");
    for event in &events {
        coworker.apply(event);
    }
    let view = CoworkerView {
        id: id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: None,
        retired: false,
        members: Vec::new(),
        updated_at_ms: at_ms,
        role: None,
        visibility: Default::default(),
    };
    store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append coworker");
    id
}

/// A key row in the state retirement leaves behind: present, and revoked.
///
/// Minted and then revoked through the real path rather than fabricated with `revoked_at_ms` set
/// — `insert_coworker_key` is a mint-time insert and does not carry that column at all, so a
/// hand-built "revoked" row would silently be a live one. This is also exactly how production
/// reaches this state: retiring a coworker calls `mark_coworker_keys_revoked`.
async fn give_it_a_revoked_key(store: &PgStore, coworker: &CoworkerId, account: &AccountId) {
    store
        .insert_coworker_key(&CoworkerKeyView {
            coworker_id: coworker.as_str().to_string(),
            account_id: account.as_str().to_string(),
            key_id: format!("key-{}", coworker.as_str()),
            key_prefix: "oag_live_revoked".to_string(),
            quota_usd: None,
            created_at_ms: now_ms() - 60_000,
            revoked_at_ms: None,
            secret_scoped: true,
        })
        .await
        .expect("mint the key");
    let revoked = store
        .mark_coworker_keys_revoked(coworker, now_ms())
        .await
        .expect("revoke it the way retirement does");
    assert_eq!(revoked.len(), 1, "the revocation must have found the row");
}

fn state_over(store: PgStore, email: &str) -> AgUiState {
    AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"revoked-key-secret")),
            email.to_string(),
        ),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
    }
}

async fn connect() -> Option<PgStore> {
    let database_url = std::env::var("OG_DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    Some(PgStore::new(pool))
}

#[tokio::test]
async fn a_revoked_key_is_not_a_usable_credential_anywhere() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let email = format!("revoked-{stamp}@og.local");
    let account = seed_account(&store, &email).await;
    let state = state_over(store.clone(), &email);

    let quill = seed_coworker(&store, &account, "Quill").await;
    give_it_a_revoked_key(&store, &quill, &account).await;

    // ---- 0. THE PROPERTY THAT MUST NOT REGRESS ----
    //
    // The row survives revocation. This is why the filter belongs at the call sites and must never
    // be pushed into the query: a member's month still counts toward their pool from rows exactly
    // like this one. If this assertion ever fails, the fix below was done in the wrong place.
    let row = store
        .coworker_key(&quill, &account)
        .await
        .expect("read")
        .expect("the row stays, marked");
    assert!(
        row.revoked_at_ms.is_some(),
        "the fixture must actually be revoked, or nothing below proves anything"
    );

    // ---- 1. THE CAP GUARD, which shipped in #76 without this filter ----
    //
    // The function whose entire subject is refusing a cap it cannot count was itself reading a
    // revoked row as "has a key". It accepted the cap, and every later turn was then held with the
    // sentence this branch exists to avoid.
    let refusal =
        opengrok_server::points::set_limit(&state, &account, &quill, &json!({"cap": 5000})).await;
    let (code, sentence) = refusal.expect_err("a revoked key cannot count a cap");
    assert_eq!(code, 409, "{sentence}");
    assert!(
        sentence.contains("no gateway key of its own"),
        "the refusal must name the cause: {sentence}"
    );
    assert!(
        !opengrok_server::points::effective(&store, &quill, &account)
            .await
            .expect("read limits")
            .is_limited(),
        "and the cap must not be stored"
    );

    // ---- 2. THE MINT CHECK ----
    //
    // `ensure_key_for` is idempotent by returning early when a key already exists. A revoked one
    // is not a key, so it must not short-circuit — it reports why it cannot mint instead of
    // handing back a prefix the gateway rejects.
    let outcome = opengrok_server::spend::ensure_key_for(&state, &account, &quill, "Quill").await;
    match outcome {
        opengrok_server::spend::KeyOutcome::Minted { key_prefix } => {
            panic!("a revoked key was reported as this coworker's credential: {key_prefix}")
        }
        opengrok_server::spend::KeyOutcome::Unavailable(reason) => {
            // No org and no vault on this fixture, so it cannot mint a replacement — which is the
            // honest answer, and the point is that it did NOT answer `Minted`.
            assert!(!reason.is_empty(), "a refusal must say why");
        }
    }

    // ---- 3. DISPATCH ----
    //
    // The one that matters most: `key_for` is what a turn authenticates with. A revoked row
    // reaching it presents a credential the gateway has been told to reject — a 401 at the far end
    // with nothing explaining it.
    let for_dispatch = opengrok_server::spend::key_for(&state, &quill, &account).await;
    assert!(
        for_dispatch.is_none(),
        "dispatch must not be handed a revoked credential: {for_dispatch:?}"
    );
}
