//! `updatedAt` is the roster's sort key, and it has to mean when something HAPPENED.
//!
//! The row's `updatedAt` is `CoworkerView::updated_at_ms`, which only moves when the coworker
//! record changes — hired, renamed, repinned. A conversation never touched it. The sidebar
//! reordered anyway, because `set_running` stamped `now_ms()` onto every push it made, which
//! quietly redefined the field as "when the server last sent you this row": a turn that produced
//! no message still hauled its row to the top, twice, once starting and once finishing.
//!
//! Now the row builder raises `updatedAt` to `max(at_ms)` over the pair's own entries, so it
//! answers the question the sort is actually asking and stays per-viewer on a shared coworker.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
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

async fn api(client: &reqwest::Client, base: &str, method: &str, body: Value) -> (u16, Value) {
    let res = client
        .post(format!("{base}/api/{method}"))
        .header("authorization", "Bearer test-bearer")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("api call");
    let status = res.status().as_u16();
    let text = res.text().await.expect("body");
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn row(client: &reqwest::Client, base: &str, agent: &str) -> Value {
    let (_, list) = api(client, base, "listAgents", json!({})).await;
    list.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)).cloned())
        .expect("the coworker must be on the roster")
}

#[tokio::test]
async fn the_sort_key_moves_with_activity_not_with_pushes() {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let stamp = now_ms();
    let email = format!("sortkey-{stamp}@og.local");
    let account = seed_account(&store, &email).await;

    let agui = AgUiState {
        auth: AuthState::new(
            store.clone(),
            Arc::new(TokenMinter::new(b"sort-key-secret")),
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
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        email.clone(),
        Some("http://opengrok.lan:1447".to_string()),
    )
    .allowing_identity_fallback();
    let app = opengrok_server::router(agui, gateway);
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

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Sorted", "clientNonce": format!("hire-{stamp}") }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();

    // ---- 1. the stamp holds still while nothing happens ----
    //
    // Read twice, because "when we last pushed you this row" and "when something happened" are
    // indistinguishable from a single sample — both look like a plausible recent number. Only a
    // second read separates them: a push stamp moves just because it was asked again.
    //
    // A freshly hired coworker already carries a `lastActivityAt` (the hire lands in the pair's
    // entries), so this is not the empty case; it is the quiet one, which is the case a sidebar
    // spends almost all of its time in.
    let quiet = row(&client, &base, &agent).await;
    let hired_at = quiet["updatedAt"].as_i64().expect("updatedAt is a number");
    let quiet_again = row(&client, &base, &agent).await;
    assert_eq!(
        quiet_again["updatedAt"].as_i64(),
        Some(hired_at),
        "reading the roster is not an event — the sort key must not move because we answered \
         twice: {quiet_again}"
    );

    // ---- 2. a message dated into the FUTURE moves the sort key ----
    //
    // Dated forward on purpose. A stamp that merely tracked "now" would satisfy any assertion
    // about a message appended a moment ago — `now_ms()` and the entry's own time are
    // indistinguishable at that distance, which is exactly the confusion being fixed. A future
    // stamp can only be reported by code that reads the ENTRY.
    let future = now_ms() + 3_600_000;
    store
        .append_gateway_entry(
            &CoworkerId::from_stored(agent.clone()),
            &account,
            &json!({
                "kind": "send-message",
                "id": format!("said-{stamp}"),
                "message": { "type": "text", "content": "hello" },
                "timestampMs": future,
            }),
            future,
        )
        .await
        .expect("append the entry");

    let after = row(&client, &base, &agent).await;
    assert_eq!(
        after["updatedAt"].as_i64(),
        Some(future),
        "the sort key must be the newest entry's own time, not the time we answered: {after}"
    );
    assert_eq!(
        after["lastActivityAt"].as_i64(),
        Some(future),
        "and it must agree with the activity stamp it is derived from: {after}"
    );
    assert!(
        after["updatedAt"].as_i64().unwrap_or(0) > hired_at,
        "the row has to actually reorder, or the sidebar never moves: {after}"
    );
}
