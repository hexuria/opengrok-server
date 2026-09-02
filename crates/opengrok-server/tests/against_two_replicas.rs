//! Two replicas on one database. What one registers, the other completes; what one mints,
//! the other spends — once. The three maps that lived in a single process (browser logins,
//! OAuth codes, answered MCP yeses) are rows now (`opengrok_store::replica`), and this is the
//! evidence: two servers, two pools, one Postgres, every request deliberately sent to the
//! replica that did NOT create the state it needs.
//!
//! Needs Postgres, so it skips — loudly — when OG_DATABASE_URL is absent.

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

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const CALLBACK: &str = "http://localhost:8123/callback";

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Its own pool: a replica shares the database, never the connection.
async fn store_from(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

fn mint_access(auth: &AuthState, account: &AccountId, email: &str) -> String {
    auth.minter
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

async fn mint_bot_key(base: &str, access: &str, coworker: &CoworkerId) -> String {
    let response = reqwest::Client::new()
        .post(format!("{base}/coworkers/{}/keys", coworker.as_str()))
        .header("Authorization", format!("Bearer {access}"))
        .send()
        .await
        .expect("mint");
    assert_eq!(response.status().as_u16(), 201, "bot key mint");
    let body: Value = response.json().await.expect("mint body");
    body["key"].as_str().expect("key").to_string()
}

async fn seed_account(store: &PgStore, email: &str, password: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password(password).expect("hash");
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
    let mut coworker = Coworker::default();
    let events = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: name.to_string(),
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
        members: Vec::new(),
        updated_at_ms: 2,
    };
    store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append coworker");
    id
}

fn no_redirect() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

/// A query parameter out of a Location header.
fn query_param(location: &str, name: &str) -> Option<String> {
    let query = location.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// The consent token out of the consent card.
fn consent_of(page: &str) -> String {
    let marker = "name=consent value=\"";
    let start = page.find(marker).expect("a consent input") + marker.len();
    let end = page[start..].find('"').expect("closing quote") + start;
    page[start..end].to_string()
}

/// The hand-written client: one JSON-RPC request over plain POST, no SDK. Returns the HTTP status
/// AND the parsed body — auth is enforced at the transport edge now, so the status is the point of
/// the auth tests, not a JSON-RPC error. Accepts either a plain-JSON reply or an SSE-framed one,
/// because the transport may pick either and a client must not care.
async fn rpc(
    base: &str,
    bearer: Option<&str>,
    id: i64,
    method: &str,
    params: Value,
) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    if let Some(token) = bearer {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.expect("request");
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("body");
    // A non-2xx from the guard is plain text, not JSON — keep it as a string so a test can assert
    // on both the status and the message.
    let value = if content_type.starts_with("text/event-stream") {
        let data = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .next_back()
            .unwrap_or("null");
        serde_json::from_str(data.trim()).unwrap_or(Value::Null)
    } else {
        serde_json::from_str(&body).unwrap_or(Value::String(body))
    };
    (status, value)
}

/// Bind first, then build the state with the bound address as the public URL: the metadata and
/// the `resource` must name the address the client actually reaches.
/// One replica. `public_url` is the ONE address the outside world uses for the whole fleet —
/// in production a load balancer's; here replica A's socket, handed to B as well.
async fn spawn(store: PgStore, email: &str, public_url: Option<&str>) -> (String, AuthState) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"mcp-oauth-test-secret")),
        email.to_string(),
    )
    .with_resend(
        None,
        public_url.map_or_else(|| base.clone(), str::to_string),
    );
    let agui = AgUiState {
        auth: auth.clone(),
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
        Some(public_url.map_or_else(|| base.clone(), str::to_string)),
    );
    let app = opengrok_server::router(agui, gateway);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (base, auth)
}

#[tokio::test]
async fn a_browser_login_started_on_one_replica_completes_on_the_other() {
    let database_url = database_or_skip!();
    let store_a = store_from(&database_url).await;
    let store_b = store_from(&database_url).await;
    let email = format!("replica-login-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store_a, &email, "password1").await;
    let (a, _) = spawn(store_a, &email, None).await;
    let (b, _) = spawn(store_b, &email, Some(&a)).await;
    let client = no_redirect();
    let uuid = format!("u-{}", uuid::Uuid::now_v7().simple());

    // The app opens the page on A: the challenge is registered, unauthenticated.
    let res = client
        .get(format!(
            "{a}/loginDeepControl?challenge={CHALLENGE}&uuid={uuid}&mode=login"
        ))
        .send()
        .await
        .expect("page");
    assert_eq!(res.status(), 200);
    // Polling B before anybody signed in: pending, whatever the verifier.
    let res = client
        .get(format!("{b}/auth/poll?uuid={uuid}&verifier={VERIFIER}"))
        .send()
        .await
        .expect("poll early");
    assert_eq!(res.status(), 404);

    // The person submits credentials to B (the balancer moved them).
    let res = client
        .post(format!("{b}/loginDeepControl"))
        .form(&[
            ("challenge", CHALLENGE),
            ("uuid", uuid.as_str()),
            ("email", email.as_str()),
            ("password", "password1"),
        ])
        .send()
        .await
        .expect("submit");
    assert_eq!(res.status(), 200);
    assert!(res.text().await.expect("body").contains("Signed in"));

    // A wrong verifier on A still reads as pending — nothing to learn from it.
    let res = client
        .get(format!(
            "{a}/auth/poll?uuid={uuid}&verifier=not-the-verifier"
        ))
        .send()
        .await
        .expect("poll wrong");
    assert_eq!(res.status(), 404);
    // The right one on A releases the session, once.
    let res = client
        .get(format!("{a}/auth/poll?uuid={uuid}&verifier={VERIFIER}"))
        .send()
        .await
        .expect("poll");
    assert_eq!(res.status(), 200);
    let tokens: Value = res.json().await.expect("tokens");
    assert!(
        !tokens["accessToken"].as_str().unwrap_or("").is_empty(),
        "{tokens}"
    );
    let res = client
        .get(format!("{b}/auth/poll?uuid={uuid}&verifier={VERIFIER}"))
        .send()
        .await
        .expect("poll again");
    assert_eq!(
        res.status(),
        404,
        "a completed login is taken, on every replica"
    );
}

#[tokio::test]
async fn an_oauth_code_minted_on_one_replica_is_exchanged_once_on_the_other() {
    let database_url = database_or_skip!();
    let store_a = store_from(&database_url).await;
    let store_b = store_from(&database_url).await;
    let email = format!("replica-oauth-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store_a, &email, "password1").await;
    let mine = seed_coworker(&store_a, &account, "Ada").await;
    let (a, _) = spawn(store_a, &email, None).await;
    let (b, _) = spawn(store_b, &email, Some(&a)).await;
    let client = no_redirect();

    // Registration and consent on A.
    let res = client
        .post(format!("{a}/oauth/mcp/register"))
        .json(&json!({
            "client_name": "Claude Code",
            "redirect_uris": [CALLBACK],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .await
        .expect("register");
    assert_eq!(res.status(), 201);
    let registered: Value = res.json().await.expect("json");
    let client_id = registered["client_id"]
        .as_str()
        .expect("client_id")
        .to_string();
    let form_base = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.clone()),
        ("redirect_uri", CALLBACK.to_string()),
        ("code_challenge", CHALLENGE.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("state", "xyz".to_string()),
        ("scope", "mcp:tools".to_string()),
        ("resource", format!("{a}/mcp")),
    ];
    let mut login = form_base.clone();
    login.push(("email", email.clone()));
    login.push(("password", "password1".to_string()));
    let res = client
        .post(format!("{a}/oauth/mcp/authorize"))
        .form(&login)
        .send()
        .await
        .expect("login");
    assert_eq!(res.status(), 200);
    let consent = consent_of(&res.text().await.expect("consent page"));
    let mut allow = form_base.clone();
    allow.push(("consent", consent));
    allow.push(("coworker", mine.as_str().to_string()));
    let res = client
        .post(format!("{a}/oauth/mcp/authorize"))
        .form(&allow)
        .send()
        .await
        .expect("consent");
    assert_eq!(res.status(), 303);
    let location = res.headers()["location"].to_str().unwrap().to_string();
    let code = query_param(&location, "code").expect("code");

    // The exchange lands on B: the code is found, checked, and spent there.
    let token_form = |code: &str| {
        vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", CALLBACK.to_string()),
            ("client_id", client_id.clone()),
            ("code_verifier", VERIFIER.to_string()),
            ("resource", format!("{a}/mcp")),
        ]
    };
    let res = client
        .post(format!("{b}/oauth/mcp/token"))
        .form(&token_form(&code))
        .send()
        .await
        .expect("token on b");
    assert_eq!(
        res.status(),
        200,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let tokens: Value = res.json().await.expect("tokens");
    let bearer = tokens["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();
    // Presented again, to A this time: gone.
    let res = client
        .post(format!("{a}/oauth/mcp/token"))
        .form(&token_form(&code))
        .send()
        .await
        .expect("token on a");
    assert_eq!(res.status(), 400);
    let refused: Value = res.json().await.expect("json");
    assert_eq!(refused["error"], "invalid_grant", "{refused}");
    // And the key B issued opens A's door — one deployment, one audience.
    let (status, init) = rpc(
        &a,
        Some(&bearer),
        1,
        "initialize",
        json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } }),
    )
    .await;
    assert_eq!(status, 200, "{init}");
}

#[tokio::test]
async fn a_yes_answered_on_one_replica_is_spent_by_the_retry_on_the_other() {
    let database_url = database_or_skip!();
    let store_a = store_from(&database_url).await;
    let store_b = store_from(&database_url).await;
    let email = format!("replica-yes-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store_a, &email, "password1").await;
    let coworker = seed_coworker(&store_a, &account, "Doorman").await;
    let (a, auth_a) = spawn(store_a.clone(), &email, None).await;
    let (b, _) = spawn(store_b.clone(), &email, Some(&a)).await;

    // The remembered yes itself: written through A's store, taken through B's, by VALUE.
    let arguments = json!({ "command": "echo hi", "cwd": "/tmp" });
    opengrok_server::mcp_door::remember_mcp_allow_once(
        &store_a,
        &coworker,
        "shell",
        &arguments,
        "mcp_yes_1",
        true,
    )
    .await
    .expect("remember");
    assert_eq!(
        opengrok_server::mcp_door::take_mcp_allow_once(
            &store_b,
            &coworker,
            "shell",
            &json!({ "cwd": "/tmp", "command": "echo hi" }),
        )
        .await,
        Some(("mcp_yes_1".to_string(), true)),
        "taken on the other replica, key order notwithstanding"
    );
    assert_eq!(
        opengrok_server::mcp_door::take_mcp_allow_once(&store_a, &coworker, "shell", &arguments)
            .await,
        None,
        "one-shot on every replica"
    );

    // Through the door: a yes remembered via A, the retry arriving at B. The coworker has no
    // computer, so B refuses — under the yes's call id, and gives the yes back.
    opengrok_server::mcp_door::remember_mcp_allow_once(
        &store_a,
        &coworker,
        "shell",
        &arguments,
        "mcp_yes_2",
        true,
    )
    .await
    .expect("remember");
    let access = mint_access(&auth_a, &account, &email);
    let bot_key = mint_bot_key(&a, &access, &coworker).await;
    let (_, call) = rpc(
        &b,
        Some(&bot_key),
        3,
        "tools/call",
        json!({ "name": "shell", "arguments": arguments }),
    )
    .await;
    assert_eq!(call["result"]["isError"], json!(true), "{call}");
    let rows: Value = reqwest::Client::new()
        .get(format!("{a}/coworkers/{}/mcp-calls", coworker.as_str()))
        .header("Authorization", format!("Bearer {access}"))
        .send()
        .await
        .expect("mcp-calls")
        .json()
        .await
        .expect("json");
    assert_eq!(
        rows[0]["callId"],
        json!("mcp_yes_2"),
        "the retry on B spent A's yes: {rows}"
    );
    assert_eq!(rows[0]["outcome"], json!("refused"), "{rows}");
    assert_eq!(
        opengrok_server::mcp_door::take_mcp_allow_once(&store_a, &coworker, "shell", &arguments)
            .await,
        Some(("mcp_yes_2".to_string(), true)),
        "a call that did not run gives the yes back, visible from A"
    );
}
