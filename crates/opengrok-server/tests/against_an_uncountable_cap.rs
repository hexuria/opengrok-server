//! A points limit that cannot be counted is refused where it is set, not where it bites.
//!
//! Found by the client session surveying mock-door coverage. `PUT /coworkers/{id}/limit` was a
//! pure store write: it reported 200 and stored the cap. But `GuardedDoor` wraps every door — the
//! mocks included, deliberately, so they meter exactly like the real one — so the moment
//! `is_limited()` is true a turn needs a key of the coworker's own to count against and an admin
//! connection to read that meter with. Without either, every later turn was held with a sentence
//! naming a gateway the person may never have been using.
//!
//! So: a person sets a cap in the settings pane, is told it worked, and the coworker stops
//! answering. The write reported success, so nothing about it was self-diagnosing. That is the
//! "an empty success is the dangerous reply" rule with a settings panel in front of it.
//!
//! WHAT IS NOT CHANGED, on purpose: holding an unmeterable turn is correct, and `GuardedDoor`
//! still wraps the mocks. A coworker under a cap that cannot be counted must not spend uncounted,
//! and no-oping the guard under a mock would spend a deliberate property to hide a bug.
//!
//! `set_limit` is called directly rather than over HTTP because the route hands its `(code,
//! sentence)` to the client verbatim (`agui/routes.rs`, `StatusCode::from_u16(code)` and
//! `{"error": …}`), so the decision under test is entirely here.
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

fn state_over(store: PgStore, email: &str) -> AgUiState {
    AgUiState {
        // `AuthState::new` reads `gateway_admin` from the environment, and `cargo test` does not
        // source `.env`, so this is `None` — which is the deployment shape under test.
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"uncountable-cap-secret")),
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
async fn a_cap_nobody_can_count_is_refused_before_it_is_stored() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let email = format!("uncountable-{stamp}@og.local");
    let account = seed_account(&store, &email).await;
    let state = state_over(store.clone(), &email);

    // ---- 1. no key of its own: refused, and nothing is stored ----
    let quill = seed_coworker(&store, &account, "Quill").await;
    let refusal =
        opengrok_server::points::set_limit(&state, &account, &quill, &json!({"cap": 5000})).await;

    let (code, sentence) = refusal.expect_err("a cap that cannot be counted must be refused");
    assert_eq!(
        code, 409,
        "the write conflicts with the deployment's state; it is not a malformed request: {sentence}"
    );
    assert!(
        sentence.contains("Quill"),
        "the refusal must name the coworker the person was looking at: {sentence}"
    );
    assert!(
        sentence.contains("no gateway key of its own"),
        "it must give the same cause the turn would have given: {sentence}"
    );
    assert!(
        sentence.contains("Nothing was saved"),
        "and say that the cap was not applied, or the person cannot tell: {sentence}"
    );

    // THE HALF THAT MATTERS. A refusal that still wrote the row would leave the coworker exactly
    // as broken as before, while reading like a fix.
    let after = opengrok_server::points::effective(&store, &quill, &account)
        .await
        .expect("read limits");
    assert!(
        !after.is_limited(),
        "a refused cap must not be stored — the coworker has to keep answering"
    );

    // ---- 2. a key, but no admin to read the meter with: also refused ----
    //
    // The second gate. The runtime reaches it only after the first passes, so a test that stopped
    // at the key would leave it unproven.
    let scribe = seed_coworker(&store, &account, "Scribe").await;
    store
        .insert_coworker_key(&CoworkerKeyView {
            coworker_id: scribe.as_str().to_string(),
            account_id: account.as_str().to_string(),
            key_id: format!("key-{stamp}"),
            key_prefix: "oag_live_test".to_string(),
            quota_usd: None,
            created_at_ms: now_ms(),
            revoked_at_ms: None,
            secret_scoped: true,
        })
        .await
        .expect("insert the key");

    let refusal =
        opengrok_server::points::set_limit(&state, &account, &scribe, &json!({"cap": 5000})).await;
    let (code, sentence) = refusal.expect_err("no admin connection means no meter to read");
    assert_eq!(code, 409, "{sentence}");
    assert!(
        sentence.contains("OG_GATEWAY_ADMIN_URL"),
        "the sentence must name what is missing, not just that something is: {sentence}"
    );
    assert!(
        !opengrok_server::points::effective(&store, &scribe, &account)
            .await
            .expect("read limits")
            .is_limited(),
        "nothing may be stored on this path either"
    );

    // ---- 3. CLEARING IS NEVER REFUSED ----
    //
    // The way out, and the reason the check is on the resulting state rather than on the request.
    // A coworker already held — by a cap stored before this check existed, or by its payer's pool
    // — must be able to have the cap taken off. A check that refused an unmeterable coworker's
    // clear would strand it permanently, which is a worse bug than the one being fixed.
    store
        .put_points_limit(
            opengrok_store::points::PointsScope::Coworker,
            quill.as_str(),
            opengrok_store::points::PointsLimit {
                month_points: Some(5000),
                day_points: None,
            },
            account.as_str(),
            now_ms(),
        )
        .await
        .expect("store a cap the way the old code would have");
    assert!(
        opengrok_server::points::effective(&store, &quill, &account)
            .await
            .expect("read limits")
            .is_limited(),
        "the fixture must actually be stuck, or the escape hatch proves nothing"
    );

    let cleared =
        opengrok_server::points::set_limit(&state, &account, &quill, &json!({"cap": null})).await;
    assert!(
        cleared.is_ok(),
        "clearing must always be allowed: {:?}",
        cleared.err()
    );
    assert!(
        !opengrok_server::points::effective(&store, &quill, &account)
            .await
            .expect("read limits")
            .is_limited(),
        "and it must actually take the cap off"
    );
}
