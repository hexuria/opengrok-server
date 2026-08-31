//! The /mcp door, driven by a HAND-WRITTEN JSON-RPC client over plain HTTP. Deliberately not
//! rmcp-to-rmcp: a client and server sharing one SDK only prove they share an interpretation —
//! the lesson that caught `Bearer Bearer` on the client side. Covers the auth boundary (only a
//! live bot key opens the door), the computerless coworker (empty toolbox, healthy handshake,
//! failed-closed call), and revocation. The full toolbox against a real computer is
//! `scripts/slice20-mcp-door-smoke.sh`, where a Docker daemon exists. Needs Postgres; skips
//! loudly without.

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
use opengrok_store::PgStore;
use serde_json::{Value, json};

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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

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
    let account = Account::replay(&events);
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
        verified: account.verified,
        enabled: account.enabled,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
    id
}

/// A coworker with NO computer: the door must still shake hands and list an empty toolbox.
async fn seed_computerless_coworker(store: &PgStore, account: &AccountId) -> CoworkerId {
    let id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let events = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: "Doorman".to_string(),
            model: "oag/cheap".to_string(),
            at_ms: 1,
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
        updated_at_ms: 2,
    };
    store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append coworker");
    id
}

fn app_with(store: PgStore, host_email: &str) -> (axum::Router, AgUiState) {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"mcp-door-test-secret-mcp-door-test!!")),
        host_email.to_string(),
    );
    let agui = AgUiState {
        auth,
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
        host_email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    (opengrok_server::router(agui.clone(), gateway), agui)
}

async fn spawn(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// The hand-written client: one JSON-RPC request over plain POST, no SDK. Accepts either a
/// plain-JSON reply or an SSE-framed one, because the transport may pick either and a client
/// must not care.
async fn rpc(base: &str, bearer: Option<&str>, id: i64, method: &str, params: Value) -> Value {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    if let Some(token) = bearer {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.expect("request");
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("body");
    if content_type.starts_with("text/event-stream") {
        // The last data: line carries the response message.
        let data = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .next_back()
            .unwrap_or("null");
        serde_json::from_str(data.trim()).expect("sse json")
    } else {
        serde_json::from_str(&body).unwrap_or(Value::Null)
    }
}

fn mint_access_for(state: &AgUiState, account: &AccountId, email: &str) -> String {
    state
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-test",
            email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access")
}

async fn mint_bot_key(base: &str, access: &str, coworker: &CoworkerId) -> (String, String) {
    let response = reqwest::Client::new()
        .post(format!("{base}/coworkers/{}/keys", coworker.as_str()))
        .header("Authorization", format!("Bearer {access}"))
        .send()
        .await
        .expect("mint");
    assert_eq!(response.status().as_u16(), 201, "bot key mint");
    let body: Value = response.json().await.expect("mint body");
    (
        body["key"].as_str().expect("key").to_string(),
        body["jti"].as_str().expect("jti").to_string(),
    )
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": { "name": "handwritten-test-client", "version": "0" },
    })
}

#[tokio::test]
async fn a_bot_key_shakes_hands_and_a_computerless_coworker_lists_an_empty_toolbox() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_computerless_coworker(&store, &account).await;
    let (app, state) = app_with(store.clone(), &host_email);
    let base = spawn(app).await;
    let access = mint_access_for(&state, &account, &host_email);
    let (bot_key, _) = mint_bot_key(&base, &access, &coworker).await;

    let init = rpc(&base, Some(&bot_key), 1, "initialize", initialize_params()).await;
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "tools capability advertised: {init}"
    );

    let list = rpc(&base, Some(&bot_key), 2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.is_empty(),
        "a coworker with no computer offers no tools, not broken ones: {list}"
    );

    // A call still fails CLOSED, with a reason, not a hang or a success.
    let call = rpc(
        &base,
        Some(&bot_key),
        3,
        "tools/call",
        json!({ "name": "shell", "arguments": { "command": "echo hi" } }),
    )
    .await;
    assert_eq!(call["result"]["isError"], json!(true), "{call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("no computer"),
        "the refusal names the reason: {text}"
    );
}

#[tokio::test]
async fn only_a_live_bot_key_opens_the_door() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_computerless_coworker(&store, &account).await;
    let (app, state) = app_with(store.clone(), &host_email);
    let base = spawn(app).await;
    let access = mint_access_for(&state, &account, &host_email);

    // No bearer at all: refused with guidance, not a panic and not a listing.
    let anonymous = rpc(&base, None, 1, "tools/list", json!({})).await;
    let message = anonymous["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("bot key"),
        "the refusal says what credential to use: {anonymous}"
    );

    // An account access token is a person, not a coworker: told how to mint the right thing.
    let person = rpc(&base, Some(&access), 2, "tools/list", json!({})).await;
    let message = person["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("bot key") && message.contains("/coworkers/"),
        "a person is pointed at the mint, not guessed a coworker for: {person}"
    );

    // A revoked key answers revoked — never a silent downgrade to anonymous.
    let (bot_key, jti) = mint_bot_key(&base, &access, &coworker).await;
    store
        .revoke_bot_key(&account, &jti)
        .await
        .expect("revoke");
    let revoked = rpc(&base, Some(&bot_key), 3, "tools/list", json!({})).await;
    let message = revoked["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("revoked"),
        "a revoked key is named revoked: {revoked}"
    );
}
