//! A turn that names a coworker must say who is asking.
//!
//! `forwardedProps.coworkerId` is sent by the client, so anyone can send it — including a caller
//! with no credential at all. That pair could only ever fail, and before this it failed late and
//! in the wrong words: every gate downstream is keyed on having BOTH a coworker and a principal,
//! so the policy check, the coworker's model and role, its tools and its persona were all skipped,
//! and the turn reached the spend guard carrying a scope with no payer.
//!
//! The guard held it, correctly, with "This turn does not say whose spend it is … This is a server
//! bug, not a limit you have hit." It was our bug, and it reached a person as red text in their
//! transcript that read like a quota they had hit. Observed on 17 Sep 2026 against a live server,
//! twice, with `auth_len=0` in the request log both times.
//!
//! Anonymous turns are still allowed. What is refused is naming somebody else's coworker while
//! declining to say who you are.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_store::PgStore;
use serde_json::{Value, json};

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

/// The AG-UI body, with `coworkerId` optional so one helper serves both halves of the claim.
fn turn(coworker: Option<&str>) -> Value {
    let mut props = json!({});
    if let Some(coworker) = coworker {
        props["coworkerId"] = json!(coworker);
    }
    json!({
        "threadId": format!("thr-{}", now_ms()),
        "runId": uuid::Uuid::now_v7().to_string(),
        "messages": [{ "id": "m1", "role": "user", "content": "hello" }],
        "forwardedProps": props,
    })
}

#[tokio::test]
async fn a_turn_that_names_a_coworker_needs_a_caller_we_can_name_back() {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let email = format!("nameless-{}@og.local", now_ms());
    seed_account(&store, &email).await;
    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"nameless-secret")),
            email.clone(),
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
    };
    let app = opengrok_server::router(agui);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let client = reqwest::Client::new();

    // A coworker named by nobody: refused at the door, not carried to the spend guard.
    let res = client
        .post(format!("{base}/ag-ui"))
        .header("content-type", "application/json")
        .body(turn(Some("cw_someone_elses")).to_string())
        .send()
        .await
        .expect("post");
    assert_eq!(
        res.status().as_u16(),
        401,
        "a named coworker needs a caller"
    );
    let said = res.text().await.expect("body");
    assert!(
        said.contains("names a coworker"),
        "the refusal must say what to do about it, got: {said}"
    );
    // AND IT MUST NOT READ AS A LIMIT. The whole point of refusing here is that the person no
    // longer meets the spend guard's sentence, which describes a quota they never hit.
    assert!(
        !said.to_ascii_lowercase().contains("limit"),
        "a missing credential must not be dressed as a spend limit, got: {said}"
    );

    // The other half of the claim: an anonymous turn that names nobody is still served, so this
    // refusal narrows exactly one shape rather than closing the anonymous door.
    let res = client
        .post(format!("{base}/ag-ui"))
        .header("content-type", "application/json")
        .body(turn(None).to_string())
        .send()
        .await
        .expect("post");
    assert_eq!(
        res.status().as_u16(),
        200,
        "an anonymous turn naming nobody is still allowed"
    );
}
