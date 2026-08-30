//! Drives the web console's cookie login leg over a real HTTP socket.
//!
//! The browser console does not send a `Bearer` header on navigation, so it signs in at
//! `POST /auth/login` and the server hands the session back as httpOnly cookies. These tests are
//! the browser: they POST credentials, capture the `Set-Cookie` headers exactly as a browser
//! would, then reach `GET /account` carrying only those cookies — proving `account_api::caller`
//! authenticates from the cookie with no `Authorization` header anywhere. The desktop client's
//! header path is untouched and covered elsewhere.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent, the same bargain the gRPC integration test makes.

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
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

/// Mint a credential account directly through the store. `verified`/`enabled` let a test choose a
/// ready account (both true) or one still behind a gate.
async fn seed_account(
    store: &PgStore,
    email: &str,
    password: &str,
    verified: bool,
    enabled: bool,
) -> AccountId {
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
            verified,
            enabled,
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

fn app_with(store: PgStore, secret: &[u8]) -> axum::Router {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(secret)),
        "host@og.local".to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
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
        "host@og.local".to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    opengrok_server::router(agui, gateway)
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

/// Pull one cookie's value out of a response's `Set-Cookie` headers.
fn cookie_value(res: &reqwest::Response, name: &str) -> Option<String> {
    for header in res.headers().get_all(reqwest::header::SET_COOKIE) {
        let text = header.to_str().ok()?;
        let first = text.split(';').next().unwrap_or("");
        if let Some((key, value)) = first.split_once('=')
            && key.trim() == name
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

#[tokio::test]
async fn a_browser_logs_in_by_cookie_and_reaches_its_account_with_no_bearer() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("cookie-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &email, "password1", true, true).await;

    let base = spawn(app_with(store, b"web-console-test-secret-web-console")).await;
    let client = reqwest::Client::new();

    // Sign in. The reply carries the email and NOT a token; the tokens are in httpOnly cookies.
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": email, "password": "password1" }))
        .send()
        .await
        .expect("login request");
    assert_eq!(res.status(), 200, "login should succeed");
    let access = cookie_value(&res, "og_access").expect("og_access cookie set");
    let refresh = cookie_value(&res, "og_refresh").expect("og_refresh cookie set");
    assert!(!access.is_empty() && !refresh.is_empty());
    let body: serde_json::Value = res.json().await.expect("json body");
    assert_eq!(body["email"], email);
    assert!(body.get("accessToken").is_none(), "no token in the body");

    // Reach /account carrying ONLY the cookie — no Authorization header.
    let me = client
        .get(format!("{base}/account"))
        .header(reqwest::header::COOKIE, format!("og_access={access}"))
        .send()
        .await
        .expect("account request");
    assert_eq!(me.status(), 200, "cookie should authenticate /account");
    let profile: serde_json::Value = me.json().await.expect("json");
    assert_eq!(profile["email"], email);

    // No cookie, no header ⇒ 401.
    let anon = client
        .get(format!("{base}/account"))
        .send()
        .await
        .expect("anon");
    assert_eq!(anon.status(), 401);

    // Rotate: the refresh cookie yields a fresh access cookie.
    let rotated = client
        .post(format!("{base}/auth/refresh"))
        .header(reqwest::header::COOKIE, format!("og_refresh={refresh}"))
        .send()
        .await
        .expect("refresh");
    assert_eq!(rotated.status(), 200, "refresh should rotate");
    assert!(
        cookie_value(&rotated, "og_access").is_some(),
        "refresh re-sets og_access"
    );
}

#[tokio::test]
async fn bad_credentials_and_gated_accounts_set_no_cookie_and_say_which() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let good = format!("good-{}@og.local", uuid::Uuid::now_v7().simple());
    let pending = format!("pending-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &good, "password1", true, true).await;
    // Verified is false ⇒ the login is refused as unverified, distinctly.
    seed_account(&store, &pending, "password1", false, true).await;

    let base = spawn(app_with(store, b"web-console-test-secret-web-console")).await;
    let client = reqwest::Client::new();

    // Wrong password: 401, no cookie, and the ambiguous message (no account enumeration).
    let wrong = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": good, "password": "WRONG" }))
        .send()
        .await
        .expect("wrong");
    assert_eq!(wrong.status(), 401);
    assert!(
        cookie_value(&wrong, "og_access").is_none(),
        "a failed login sets no cookie"
    );
    let wrong_body: serde_json::Value = wrong.json().await.expect("json");
    assert_eq!(wrong_body["error"], "Wrong email or password.");

    // Unverified: a distinct 403 the SPA can show verbatim.
    let unverified = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": pending, "password": "password1" }))
        .send()
        .await
        .expect("unverified");
    assert_eq!(unverified.status(), 403);
    assert!(cookie_value(&unverified, "og_access").is_none());
    let unverified_body: serde_json::Value = unverified.json().await.expect("json");
    assert!(
        unverified_body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not verified"),
        "message should name the reason: {unverified_body}"
    );

    // Logout clears the cookies regardless.
    let out = client
        .post(format!("{base}/auth/logout"))
        .send()
        .await
        .expect("logout");
    assert_eq!(out.status(), 200);
    // The clear is a Max-Age=0 Set-Cookie for og_access.
    let cleared = out
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .any(|h| {
            h.to_str()
                .map(|t| t.contains("og_access=") && t.contains("Max-Age=0"))
                .unwrap_or(false)
        });
    assert!(cleared, "logout expires og_access");
}
