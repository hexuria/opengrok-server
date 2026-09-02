//! A turn that fails says why in the bubble. A gateway refusal is one sentence the person can
//! act on ("no subscription credential for xai on this route" — the dev gateway on 2 Sep 2026,
//! seats bound to another principal); before this the bubble said "Its run log has the reason"
//! and the reason was a database dig away. Needs Postgres; skips loudly without OG_DATABASE_URL.

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

const REFUSAL: &str = r#"the model gateway refused: 503 {"type":"error","error":{"type":"no_credential_of_kind","message":"no subscription credential for xai on this route"}}"#;

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

#[tokio::test]
async fn a_refused_turn_says_the_gateways_sentence_in_the_bubble() {
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
    let email = format!("failed-turn-{}@og.local", now_ms());
    seed_account(&store, &email).await;
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"failed-turn-secret")),
        email.clone(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::failing_with(REFUSAL)),
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
        email,
        Some("http://opengrok.lan:1447".to_string()),
    );
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

    let (status, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let agent = created["agent"]["id"].as_str().expect("id").to_string();
    let (status, sent) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");

    let mut answer = String::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, tail) = api(
            &client,
            &base,
            "getAgentTranscriptTail",
            json!({ "id": agent, "limit": 50 }),
        )
        .await;
        let found = tail["entries"].as_array().and_then(|entries| {
            entries.iter().rev().find_map(|e| {
                (e["kind"] == "send-message" && e["streaming"] != json!(true))
                    .then(|| e["message"]["content"].as_str().unwrap_or("").to_string())
                    .filter(|content| !content.is_empty())
            })
        });
        if let Some(found) = found {
            answer = found;
            break;
        }
    }
    assert!(
        answer.starts_with("The turn failed: "),
        "the bubble names the failure: {answer:?}"
    );
    assert!(
        answer.contains("no subscription credential for xai on this route"),
        "the gateway's own sentence, not its whole body: {answer:?}"
    );
    assert!(
        !answer.contains("no_credential_of_kind") && !answer.contains("run log"),
        "{answer:?}"
    );
}
