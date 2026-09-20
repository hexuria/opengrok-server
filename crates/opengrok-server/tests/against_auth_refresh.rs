//! Concurrent refresh with the same `og_refresh` cookie / oauth refresh_token.
//!
//! NativeChat can fire several `POST /auth/refresh` at once. The first rotates; a loser inside
//! `REFRESH_GRACE_MS` must 200 with the already-minted current pair, not 401 (which would
//! `clear_session`). After grace, the old hash is still dead — that is covered in the account
//! unit test with a synthetic clock. These tests need Postgres.

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
        .expect("append");
    id
}

fn app_with(store: PgStore) -> axum::Router {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"refresh-grace-test-secret-bytes!!")),
        "host@og.local".to_string(),
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
        host_settings: None,
    };
    opengrok_server::router(agui)
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

async fn login(client: &reqwest::Client, base: &str, email: &str) -> String {
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": email, "password": "password1" }))
        .send()
        .await
        .expect("login");
    assert_eq!(res.status(), 200, "login");
    cookie_value(&res, "og_refresh").expect("og_refresh")
}

#[tokio::test]
async fn a_second_refresh_with_the_old_cookie_reuses_the_current_pair() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("grace-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &email).await;
    let base = spawn(app_with(store)).await;
    let client = reqwest::Client::new();
    let original = login(&client, &base, &email).await;

    let first = client
        .post(format!("{base}/auth/refresh"))
        .header(reqwest::header::COOKIE, format!("og_refresh={original}"))
        .send()
        .await
        .expect("first");
    assert_eq!(first.status(), 200, "first rotate");
    let current = cookie_value(&first, "og_refresh").expect("rotated refresh");
    assert_ne!(current, original, "first request rotates");

    let second = client
        .post(format!("{base}/auth/refresh"))
        .header(reqwest::header::COOKIE, format!("og_refresh={original}"))
        .send()
        .await
        .expect("second");
    assert_eq!(
        second.status(),
        200,
        "grace reuse must not 401: {}",
        second.text().await.unwrap_or_default()
    );
    let reused = cookie_value(&second, "og_refresh").expect("reused refresh");
    assert_eq!(
        reused, current,
        "loser must receive the already-minted current refresh, not a second rotation"
    );
    assert!(
        cookie_value(&second, "og_access").is_some(),
        "grace reuse mints a fresh access cookie"
    );
}

#[tokio::test]
async fn concurrent_cookie_refreshes_share_one_rotation() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("race-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &email).await;
    let base = spawn(app_with(store)).await;
    let client = reqwest::Client::new();
    let original = login(&client, &base, &email).await;

    let (a, b) = tokio::join!(
        client
            .post(format!("{base}/auth/refresh"))
            .header(reqwest::header::COOKIE, format!("og_refresh={original}"))
            .send(),
        client
            .post(format!("{base}/auth/refresh"))
            .header(reqwest::header::COOKIE, format!("og_refresh={original}"))
            .send(),
    );
    let a = a.expect("a");
    let b = b.expect("b");
    assert_eq!(a.status(), 200, "concurrent a");
    assert_eq!(b.status(), 200, "concurrent b");
    let ra = cookie_value(&a, "og_refresh").expect("a refresh");
    let rb = cookie_value(&b, "og_refresh").expect("b refresh");
    assert_eq!(ra, rb, "both responses carry the same current refresh");
    assert_ne!(ra, original, "the pair did rotate once");
}

#[tokio::test]
async fn oauth_token_refresh_reuses_within_grace() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("oauth-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &email).await;
    let base = spawn(app_with(store)).await;
    let client = reqwest::Client::new();
    let original = login(&client, &base, &email).await;

    let first = client
        .post(format!("{base}/oauth/token"))
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": original,
        }))
        .send()
        .await
        .expect("first");
    assert_eq!(first.status(), 200, "oauth rotate");
    let body: serde_json::Value = first.json().await.expect("json");
    let current = body["refresh_token"].as_str().expect("refresh").to_string();
    assert_ne!(current, original);

    let second = client
        .post(format!("{base}/oauth/token"))
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": original,
        }))
        .send()
        .await
        .expect("second");
    assert_eq!(second.status(), 200, "oauth grace reuse");
    let body: serde_json::Value = second.json().await.expect("json");
    assert_eq!(
        body["refresh_token"].as_str(),
        Some(current.as_str()),
        "oauth loser reuses the current refresh"
    );
    assert!(
        body["access_token"].as_str().is_some_and(|t| !t.is_empty()),
        "fresh access token"
    );
}

#[tokio::test]
async fn an_unknown_refresh_token_is_still_rejected() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("unknown-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &email).await;
    let base = spawn(app_with(store)).await;
    let client = reqwest::Client::new();

    let cookie = client
        .post(format!("{base}/auth/refresh"))
        .header(reqwest::header::COOKIE, "og_refresh=ogr_not_a_real_token")
        .send()
        .await
        .expect("cookie");
    assert_eq!(cookie.status(), 401);

    let oauth = client
        .post(format!("{base}/oauth/token"))
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": "ogr_not_a_real_token",
        }))
        .send()
        .await
        .expect("oauth");
    assert_eq!(oauth.status(), 401);
}
