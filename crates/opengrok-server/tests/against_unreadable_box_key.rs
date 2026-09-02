//! A mapped ascii box whose org key will not decrypt is not "absent".
//!
//! After a reboot regenerated OG_CREDENTIAL_KEK, getForeverBoxStatus returned absent and
//! ensureForeverBox no-op'd in 20ms — the desktop then looped "Waking this computer up".
//! The mapping is still there; the status must carry computerError and Ensure must not
//! mark the box used.

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
use opengrok_server::gateway::GatewayState;
use opengrok_server::gateway::conversation::{BoxAction, box_control, box_status};
use opengrok_store::{PgStore, Vault};
use serde_json::json;

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

const SEAL_KEK: &str = "rIeYsJHlXEYIoRjZQfL73u7UuVMYxIrdlDT5tndh/kY=";
const LIVE_KEK: &str = "ZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTA=";

async fn store_from(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

#[tokio::test]
async fn unreadable_ascii_key_is_an_error_not_absent() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let email = format!("unreadable-{tag}@acme.test");
    let org = format!("org_{tag}");
    let account_id = AccountId::new();
    let at_ms = 1_i64;
    let hash = hash_password("password1").expect("hash");
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.clone(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: org.clone(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let account = Account::replay(&events);
    store
        .append_account(
            &account_id,
            0,
            &events,
            &AccountView {
                id: account_id.clone(),
                email: email.clone(),
                plan: Plan::Ultra,
                trial: false,
                updated_at_ms: at_ms,
                password_hash: Some(hash),
                first_name: "Test".to_string(),
                last_name: "User".to_string(),
                org_id: Some(org.clone()),
                verified: account.verified,
                enabled: account.enabled,
                avatar_url: None,
            },
        )
        .await
        .expect("append account");

    let coworker_id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let hire = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: "Hexuria".to_string(),
            model: "oag/cheap".to_string(),
            at_ms,
        })
        .expect("hire");
    for event in &hire {
        coworker.apply(event);
    }
    store
        .append_coworker(
            &coworker_id,
            &account_id,
            0,
            &hire,
            &CoworkerView {
                id: coworker_id.clone(),
                name: coworker.name.clone(),
                model: coworker.model.clone(),
                box_id: None,
                retired: false,
                members: Vec::new(),
                updated_at_ms: at_ms,
            },
        )
        .await
        .expect("append coworker");

    store
        .set_scoped_computer(
            "account",
            account_id.as_str(),
            "bx_unreadable",
            "ascii",
            Some(&org),
            100,
        )
        .await
        .expect("map box");
    store
        .mark_scoped_stopped("account", account_id.as_str())
        .await
        .expect("stopped");

    let seal_vault = Vault::from_base64_key(SEAL_KEK).expect("seal vault");
    store
        .set_org_computer_secret(&seal_vault, &org, "ascii", "box_old_key", at_ms)
        .await
        .expect("seal under the lost KEK");

    let live_vault = Vault::from_base64_key(LIVE_KEK).expect("live vault");
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(
            b"unreadable-box-key-test-secret-unreadable",
        )),
        email.clone(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: Some(Arc::new(live_vault)),
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
    };
    let gateway = GatewayState::new(agui, Some("test-bearer".into()), email.clone(), None);

    let args = json!({ "agentId": coworker_id.as_str() });
    let (code, body) = box_status(&gateway, &args, &email).await;
    assert_eq!(code, 200, "{body}");
    assert_ne!(
        body["state"].as_str(),
        Some("absent"),
        "a mapped box whose key will not open is not absent: {body}"
    );
    assert_eq!(body["computerError"]["code"].as_str(), Some("invalid_key"));
    assert!(
        body["computerError"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("cannot be opened")),
        "{body}"
    );

    let before = store
        .scoped_computer_full("account", account_id.as_str())
        .await
        .expect("read")
        .expect("mapped");
    assert!(before.2, "fixture starts stopped");

    let (code, ensured) = box_control(&gateway, &args, &email, BoxAction::Ensure).await;
    assert_eq!(code, 200, "{ensured}");
    assert_eq!(
        ensured["computerError"]["code"].as_str(),
        Some("invalid_key")
    );
    assert_ne!(ensured["state"].as_str(), Some("absent"));

    let after = store
        .scoped_computer_full("account", account_id.as_str())
        .await
        .expect("read after")
        .expect("still mapped");
    assert!(
        after.2,
        "ensure must not mark a box used when it could not talk to ascii"
    );
}
