//! The sidebar preview must survive a fresh read, not only a live push.
//!
//! `summaries.rs` hard-coded `lastEntry`, `lastMessageId`, `lastMessagePreview` and
//! `newestEntryId` to null while `conversation.rs` and `group.rs` pushed them live — so a row
//! gained a preview when a turn ended and lost it on the next boot.
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

async fn serve(store: PgStore, email: &str) -> String {
    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"preview-secret")),
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
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        email.to_string(),
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
    base
}

/// The roster row for one coworker.
async fn row(client: &reqwest::Client, base: &str, agent: &str) -> Value {
    let (_, list) = api(client, base, "listAgents", json!({})).await;
    list.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)).cloned())
        .expect("the coworker must be on the roster")
}

/// The sidebar preview survives a fresh read, which is the whole bug.
///
/// `summaries.rs` hard-coded `lastEntry`, `lastMessageId`, `lastMessagePreview` and
/// `newestEntryId` to null, while `conversation.rs` and `group.rs` pushed them live — so a row
/// gained a preview when a turn ended and lost it on the next `listAgents`. A preview that comes
/// and goes with a restart reads as the server forgetting the conversation.
///
/// So this asserts on a FRESH read rather than on a live frame: that is the path that was broken,
/// and the only one that can tell the fix from the push that was already there.
#[tokio::test]
async fn the_row_carries_its_last_message_on_a_fresh_read() {
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
    let email = format!("preview-{stamp}@og.local");
    let account = seed_account(&store, &email).await;
    let base = serve(store.clone(), &email).await;
    let client = reqwest::Client::new();

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{stamp}") }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    // A coworker that has never spoken previews nothing — not an empty string.
    let fresh = row(&client, &base, &agent).await;
    assert_eq!(fresh["lastMessagePreview"], Value::Null, "{fresh}");
    assert_eq!(fresh["lastEntry"], Value::Null, "{fresh}");

    // It speaks, and the row carries it on a read that touches no live frame at all.
    let (status, _) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{stamp}") }),
    )
    .await;
    assert_eq!(status, 200);
    let mut said = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let row = row(&client, &base, &agent).await;
        if row["lastMessagePreview"].is_string() && row["isRunning"] == json!(false) {
            said = row;
            break;
        }
    }
    let preview = said["lastMessagePreview"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        preview.contains("pong?"),
        "the row must preview what the coworker actually said: {said}"
    );
    assert_eq!(
        said["lastEntry"],
        json!({ "kind": "text", "text": preview }),
        "the structured half must agree with the string half: {said}"
    );
    assert!(
        said["lastMessageId"].is_string() && said["newestEntryId"].is_string(),
        "both ids must be served: {said}"
    );

    // THE STREAMING PLACEHOLDER MUST NOT WIN. It is a `send-message` with empty content that
    // exists for the length of a turn; previewing it would blank the row for exactly as long as
    // the coworker is talking.
    store
        .append_gateway_entry(
            &coworker,
            &account,
            &json!({
                "kind": "send-message",
                "id": format!("placeholder-{stamp}"),
                "message": { "type": "text", "content": "" },
                "timestampMs": now_ms(),
                "streaming": true,
            }),
            now_ms(),
        )
        .await
        .expect("append");
    let during = row(&client, &base, &agent).await;
    assert_eq!(
        during["lastMessagePreview"].as_str().unwrap_or_default(),
        preview,
        "an empty streaming placeholder must not replace the last real message: {during}"
    );
    // ...but it IS the newest entry, which is a different question and the client asks both.
    assert_eq!(
        during["newestEntryId"],
        json!(format!("placeholder-{stamp}")),
        "newestEntryId is the newest row of ANY kind: {during}"
    );

    // A card is newest too, and equally must not become the preview.
    store
        .append_gateway_entry(
            &coworker,
            &account,
            &json!({
                "kind": "send-message",
                "id": format!("card-{stamp}"),
                "message": { "type": "permission-request", "permission": { "title": "run it" } },
                "timestampMs": now_ms(),
            }),
            now_ms(),
        )
        .await
        .expect("append");
    let after_card = row(&client, &base, &agent).await;
    assert_eq!(
        after_card["lastMessagePreview"]
            .as_str()
            .unwrap_or_default(),
        preview,
        "a card has no words, so the preview stays on the last thing that did: {after_card}"
    );
}

/// A long answer is cut to the same length the live push uses.
///
/// Two different truncations would make a row change length depending on whether it was pushed or
/// read, which is the kind of difference nobody thinks to look for.
#[tokio::test]
async fn the_preview_is_cut_to_the_same_length_the_live_push_uses() {
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
    let email = format!("preview-long-{stamp}@og.local");
    let account = seed_account(&store, &email).await;
    let base = serve(store.clone(), &email).await;
    let client = reqwest::Client::new();

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{stamp}") }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    let long = "x".repeat(500);
    store
        .append_gateway_entry(
            &coworker,
            &account,
            &json!({
                "kind": "send-message",
                "id": format!("long-{stamp}"),
                "message": { "type": "text", "content": long },
                "timestampMs": now_ms(),
            }),
            now_ms(),
        )
        .await
        .expect("append");

    let row = row(&client, &base, &agent).await;
    assert_eq!(
        row["lastMessagePreview"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .count(),
        120,
        "120 characters, matching `text.chars().take(120)` on the live push: {row}"
    );
}
