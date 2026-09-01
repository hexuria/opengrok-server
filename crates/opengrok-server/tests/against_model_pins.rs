//! Per-coworker model pins, through the real router: hiring on a pin, changing it, and the rules
//! that keep a pin honest — never blank, never another account's to change, and never silently
//! the deployment's when somebody asked for a specific route.
//!
//! The catalogue is pointed at a STAND-IN gateway rather than mocked, so these tests assert the
//! request we actually make and the reply a browser actually receives — including that the reply
//! never carries the gateway key.
//!
//! Needs Postgres; skips loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
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
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

/// The secret the stand-in gateway is guarding. If it ever appears in a reply to the browser,
/// that is the ship-blocker these tests exist to catch.
const GATEWAY_KEY: &str = "oag_live_this_must_never_reach_a_browser";

/// A stand-in for the gateway's `/v1/models` and `/v1/chat/completions`.
async fn spawn_stand_in_gateway(probes: Arc<Mutex<Vec<String>>>) -> String {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async {
                Json(json!({
                    "object": "list",
                    "data": [{"id": "oag/auto"}, {"id": "openai/gpt-5.5"}],
                }))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(probes): State<Arc<Mutex<Vec<String>>>>, Json(body): Json<Value>| async move {
                    let model = body["model"].as_str().unwrap_or_default().to_string();
                    probes.lock().unwrap().push(model.clone());
                    // The gateway refuses a route it has no credential for, in its own words —
                    // the case the probe exists to surface before a pin is saved.
                    if model == "oag/auto" {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({"error": {"message":
                                "no credential available for provider anthropic on this route"}})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "model": model,
                            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
                        })),
                    )
                },
            ),
        )
        .with_state(probes);

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
    };
    let gateway_state = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        host_email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
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

#[tokio::test]
async fn a_coworker_is_hired_on_a_pin_and_can_be_repinned() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("pins-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email).await;
    let probes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let gateway = spawn_stand_in_gateway(probes.clone()).await;
    let (app, state) = app_with(store.clone(), &email, &gateway);
    let base = spawn(app).await;
    let token = token_for(&state, &account, &email);

    // Hired on the pin that was asked for, not the deployment's.
    let (status, hired) = call(
        &base,
        reqwest::Method::POST,
        "/coworkers",
        &token,
        Some(json!({ "name": "Ada", "model": "openai/gpt-5.5" })),
    )
    .await;
    assert_eq!(status, 201, "{hired}");
    let id = hired["id"].as_str().expect("id").to_string();

    let (_, listed) = call(&base, reqwest::Method::GET, "/coworkers", &token, None).await;
    let row = listed
        .as_array()
        .expect("roster")
        .iter()
        .find(|row| row["id"] == json!(id))
        .expect("the new coworker is on the roster")
        .clone();
    assert_eq!(row["model"], json!("openai/gpt-5.5"));

    // Repinned — the whole point of the slice.
    let (status, repinned) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/coworkers/{id}"),
        &token,
        Some(json!({ "model": "  oag/cheap  " })),
    )
    .await;
    assert_eq!(status, 200, "{repinned}");
    assert_eq!(
        repinned["model"],
        json!("oag/cheap"),
        "the pin is stored trimmed"
    );

    let (_, listed) = call(&base, reqwest::Method::GET, "/coworkers", &token, None).await;
    let row = listed
        .as_array()
        .expect("roster")
        .iter()
        .find(|row| row["id"] == json!(id))
        .expect("still on the roster")
        .clone();
    assert_eq!(row["model"], json!("oag/cheap"), "the change persisted");
    assert_eq!(row["name"], json!("Ada"), "repinning is not renaming");
}

#[tokio::test]
async fn a_pin_is_never_blank_and_never_another_accounts_to_change() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let mine = format!("mine-{}@og.local", uuid::Uuid::now_v7().simple());
    let theirs = format!("theirs-{}@og.local", uuid::Uuid::now_v7().simple());
    let my_account = seed_account(&store, &mine).await;
    let their_account = seed_account(&store, &theirs).await;
    let probes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let gateway = spawn_stand_in_gateway(probes.clone()).await;
    let (app, state) = app_with(store.clone(), &mine, &gateway);
    let base = spawn(app).await;
    let my_token = token_for(&state, &my_account, &mine);
    let their_token = token_for(&state, &their_account, &theirs);

    // A blank pin is refused at hire — it would otherwise be asked of the gateway verbatim.
    let (status, refused) = call(
        &base,
        reqwest::Method::POST,
        "/coworkers",
        &my_token,
        Some(json!({ "name": "Blank", "model": "   " })),
    )
    .await;
    assert_eq!(
        status, 201,
        "a blank pin falls back to the default: {refused}"
    );
    assert_eq!(
        refused["model"],
        json!("deployment/default"),
        "blank means 'none given', so the deployment default applies"
    );

    let (_, hired) = call(
        &base,
        reqwest::Method::POST,
        "/coworkers",
        &my_token,
        Some(json!({ "name": "Ada", "model": "openai/gpt-5.5" })),
    )
    .await;
    let id = hired["id"].as_str().expect("id").to_string();

    // A blank REPIN is refused outright: it is an explicit request to think with nothing.
    let (status, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/coworkers/{id}"),
        &my_token,
        Some(json!({ "model": "  " })),
    )
    .await;
    assert_eq!(status, 400, "a blank repin is refused");

    // Somebody else's coworker does not exist as far as this caller is concerned.
    let (status, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/coworkers/{id}"),
        &their_token,
        Some(json!({ "model": "oag/cheap" })),
    )
    .await;
    assert_eq!(status, 404, "another account's coworker is 404, not 403");

    // ...and the pin is untouched.
    let (_, listed) = call(&base, reqwest::Method::GET, "/coworkers", &my_token, None).await;
    let row = listed
        .as_array()
        .expect("roster")
        .iter()
        .find(|row| row["id"] == json!(id))
        .expect("still mine")
        .clone();
    assert_eq!(row["model"], json!("openai/gpt-5.5"));
}

#[tokio::test]
async fn the_catalogue_lists_the_gateways_routes_and_never_its_key() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("cat-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email).await;
    let probes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let gateway = spawn_stand_in_gateway(probes.clone()).await;
    let (app, state) = app_with(store.clone(), &email, &gateway);
    let base = spawn(app).await;
    let token = token_for(&state, &account, &email);

    let (status, listing) = call(&base, reqwest::Method::GET, "/models", &token, None).await;
    assert_eq!(status, 200, "{listing}");
    let ids: Vec<&str> = listing["models"]
        .as_array()
        .expect("models")
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["oag/auto", "openai/gpt-5.5"]);
    // THE SHIP-BLOCKER: the browser learns ids, never the credential that fetched them.
    assert!(
        !listing.to_string().contains("oag_live_"),
        "the gateway key reached the client: {listing}"
    );

    // Signing out is enough to lose access to it.
    let anonymous = reqwest::Client::new()
        .get(format!("{base}/models"))
        .send()
        .await
        .expect("request");
    assert_eq!(anonymous.status().as_u16(), 401);

    // The probe answers with the GATEWAY'S own words when a route cannot be served — the case
    // that would otherwise only surface at the first real turn.
    let (status, refused) = call(
        &base,
        reqwest::Method::POST,
        "/models/probe",
        &token,
        Some(json!({ "model": "oag/auto" })),
    )
    .await;
    assert_eq!(
        status, 200,
        "a refusal is an answer, not an error: {refused}"
    );
    assert_eq!(refused["ok"], json!(false));
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("no credential available"),
        "the gateway's own sentence survives: {refused}"
    );

    let (_, served) = call(
        &base,
        reqwest::Method::POST,
        "/models/probe",
        &token,
        Some(json!({ "model": "openai/gpt-5.5" })),
    )
    .await;
    assert_eq!(served["ok"], json!(true), "{served}");
    assert_eq!(served["served"], json!("openai/gpt-5.5"));
    assert!(!served.to_string().contains("oag_live_"));

    assert_eq!(
        *probes.lock().unwrap(),
        vec!["oag/auto".to_string(), "openai/gpt-5.5".to_string()],
        "the probe asked the gateway for exactly the candidate pins"
    );
}
