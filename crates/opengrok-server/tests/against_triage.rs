//! `POST /triage`, through the real router, against a stand-in gateway: a fault is weighed by one
//! plain completion, the verdict comes back bounded, and anything short of a well-formed answer is
//! said so the desktop app can fall back to the person filling the report in.
//!
//! Needs Postgres; skips loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
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
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

/// The secret the stand-in gateway guards; it must never reach the app.
const GATEWAY_KEY: &str = "oag_live_this_must_never_reach_a_browser";

/// What the stand-in gateway was asked: each request's body.
type Asked = Arc<Mutex<Vec<Value>>>;

/// A stand-in gateway whose answer depends on the model asked for: `good` answers a fenced
/// verdict, `rambling` answers prose, `refusing` refuses in words that echo the key.
async fn spawn_stand_in_gateway(asked: Asked) -> String {
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(asked): State<Asked>, Json(body): Json<Value>| async move {
                asked.lock().unwrap().push(body.clone());
                let content = match body["model"].as_str().unwrap_or_default() {
                    "rambling" => "I think this is probably a bug, hard to say.".to_string(),
                    "refusing" => {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            Json(json!({"error": {"message": format!(
                                "rejected request with Authorization: Bearer {GATEWAY_KEY}"
                            )}})),
                        );
                    }
                    _ => "```json\n{\"verdict\":\"bug\",\"confidence\":0.86,\"title\":\"Usage fails when the gateway returns 502\",\"summary\":\"The usage read got a 502.\",\"evidence\":[\"status 502\",\"other reads ok\"],\"suspect\":\"src/state.rs:8258\"}\n```".to_string(),
                };
                (
                    axum::http::StatusCode::OK,
                    Json(json!({
                        "model": body["model"],
                        "choices": [{"message": {"role": "assistant", "content": content}}],
                    })),
                )
            }),
        )
        .with_state(asked);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
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

fn app_with(store: PgStore, host_email: &str, gateway: &str) -> (Router, AgUiState) {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"model-pins-test-secret-model-pins!!!")),
        host_email.to_string(),
    )
    .with_model_catalogue(Some(Arc::new(
        opengrok_server::models::ModelCatalogue::new(gateway, GATEWAY_KEY),
    )));
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "deployment/default".to_string(),
        auto_review_model: "deployment/default".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let gateway_state = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    (opengrok_server::router(agui.clone(), gateway_state), agui)
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn token_for(state: &AgUiState, account: &AccountId, email: &str) -> String {
    state
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess",
            email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access")
}

async fn call(
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("request");
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn pack(model: Option<&str>) -> Value {
    json!({
        "report": {
            "schema": 1, "kind": "fault", "component": "desktop", "place": "usage",
            "endpoint": "GET /coworkers/{id}/usage", "status": 502,
            "raisedAt": "src/state.rs:8258", "count": 3,
            "said": "Could not load this bot's usage.", "fingerprint": "3fa91c0e2b7d",
        },
        "neighbours": [{ "place": "tools", "status": 200, "secondsApart": 1 }],
        "model": model,
    })
}

async fn set_up(tag: &str) -> (String, String, Asked) {
    let database_url = std::env::var("OG_DATABASE_URL").expect("checked by the caller");
    let database_url = opengrok_store::gate_database_or_panic(database_url);
    let store = store_from(&database_url).await;
    let email = format!("{tag}-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email).await;
    let asked: Asked = Arc::new(Mutex::new(Vec::new()));
    let gateway = spawn_stand_in_gateway(asked.clone()).await;
    let (app, state) = app_with(store, &email, &gateway);
    let base = spawn(app).await;
    let token = token_for(&state, &account, &email);
    (base, token, asked)
}

/// A fault is weighed by one plain completion on the cheap route, with no tools; the verdict
/// comes back as the model gave it, fence and all stripped.
#[tokio::test]
async fn a_fault_is_weighed_by_one_plain_completion() {
    let _ = database_or_skip!();
    let (base, token, asked) = set_up("triage").await;
    let (status, verdict) = call(
        &base,
        reqwest::Method::POST,
        "/triage",
        &token,
        Some(pack(None)),
    )
    .await;
    assert_eq!(status, 200, "{verdict}");
    assert_eq!(verdict["verdict"], json!("bug"));
    assert_eq!(verdict["confidence"], json!(0.86));
    assert_eq!(
        verdict["title"],
        json!("Usage fails when the gateway returns 502")
    );
    assert_eq!(verdict["evidence"], json!(["status 502", "other reads ok"]));

    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0]["model"],
        json!(opengrok_server::triage::DEFAULT_MODEL)
    );
    assert!(
        asked[0].get("tools").is_none(),
        "a triage offers no tools: {}",
        asked[0]
    );
    let user = asked[0]["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(user.contains("GET /coworkers/{id}/usage"), "{user}");
}

/// A model that answers prose, and a gateway that refuses, are said as such, never as a verdict,
/// and the gateway's echo of the key does not reach the app.
#[tokio::test]
async fn anything_short_of_a_verdict_is_said_so() {
    let _ = database_or_skip!();
    let (base, token, _) = set_up("triage-short").await;
    let (status, said) = call(
        &base,
        reqwest::Method::POST,
        "/triage",
        &token,
        Some(pack(Some("rambling"))),
    )
    .await;
    assert_eq!(status, 422, "{said}");

    tokio::time::sleep(std::time::Duration::from_millis(5_100)).await;
    let (status, said) = call(
        &base,
        reqwest::Method::POST,
        "/triage",
        &token,
        Some(pack(Some("refusing"))),
    )
    .await;
    assert_eq!(status, 502, "{said}");
    assert!(!said.to_string().contains(GATEWAY_KEY), "{said}");
}

/// Triage is real money on the deployment's key: a second one straight after the first is
/// refused, and so is a pack no redacted report could be.
#[tokio::test]
async fn a_loop_and_a_huge_pack_are_refused() {
    let _ = database_or_skip!();
    let (base, token, asked) = set_up("triage-loop").await;
    let (first, _) = call(
        &base,
        reqwest::Method::POST,
        "/triage",
        &token,
        Some(pack(None)),
    )
    .await;
    assert_eq!(first, 200);
    let (second, _) = call(
        &base,
        reqwest::Method::POST,
        "/triage",
        &token,
        Some(pack(None)),
    )
    .await;
    assert_eq!(second, 429);

    let mut huge = pack(None);
    huge["report"]["said"] = json!("x".repeat(20_000));
    let (status, _) = call(&base, reqwest::Method::POST, "/triage", &token, Some(huge)).await;
    assert_eq!(status, 413);
    assert_eq!(
        asked.lock().unwrap().len(),
        1,
        "neither refusal reached the gateway"
    );
}

/// Signed out is refused before anything is spent.
#[tokio::test]
async fn signed_out_is_refused() {
    let _ = database_or_skip!();
    let (base, _, asked) = set_up("triage-anon").await;
    let (status, _) = call(
        &base,
        reqwest::Method::POST,
        "/triage",
        "not-a-token",
        Some(pack(None)),
    )
    .await;
    assert_eq!(status, 401);
    assert!(asked.lock().unwrap().is_empty());
}
