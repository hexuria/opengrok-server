//! The doors that take no credential, each spent and refused with a `Retry-After`.
//!
//! Password reset (per address and per mailbox), wrong passwords (failures only — a right
//! password from a fresh address still signs in), domain verification (per org) and dynamic
//! client registration (per address): the budget in `auth/budget.rs`, walked over a real socket
//! so the header and the sentence are what a client actually sees.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, OrgId};
use opengrok_core::org::{Org, OrgCommand};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::budget::{CLIENT_REGISTRATION, DOMAIN_VERIFY, FORGOT, LOGIN_FAILURES};
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::domain_proof::StaticDns;
use opengrok_server::gateway::GatewayState;
use opengrok_store::PgStore;
use serde_json::json;

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

async fn seed_account(
    store: &PgStore,
    email: &str,
    password: &str,
    org_id: Option<&str>,
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
            org_id: org_id.unwrap_or("").to_string(),
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
        org_id: org_id.map(str::to_string),
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

async fn seed_org(store: &PgStore, domain: &str, password: &str) -> (OrgId, String) {
    let org_id = OrgId::new();
    let admin_email = format!("admin@{domain}");
    let admin = seed_account(store, &admin_email, password, Some(org_id.as_str())).await;
    let at_ms = now_ms();
    let events = Org::default()
        .decide(OrgCommand::Create {
            name: "Acme".to_string(),
            admin,
            domains: vec![domain.to_string()],
            at_ms,
        })
        .expect("create org");
    let state = Org::replay(&events);
    store
        .append_org(&org_id, 0, &events, &state, at_ms)
        .await
        .expect("append org");
    (org_id, admin_email)
}

/// One server, its own fresh budgets: the table is per replica, so every test starts at zero.
async fn spawn(store: PgStore) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"rate-limit-test-secret")),
        "host@og.local".to_string(),
    )
    .with_resend(None, base.clone())
    .with_dns(Arc::new(StaticDns::default()));
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
        "host@og.local".to_string(),
        Some(base.clone()),
    );
    let app = opengrok_server::router(agui, gateway);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    base
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

/// The refusal every spent door gives: 429, a `Retry-After` inside the window, and a sentence.
async fn assert_spent(res: reqwest::Response, window_secs: u64) -> String {
    assert_eq!(res.status(), 429);
    let retry_after: u64 = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .expect("Retry-After header")
        .to_str()
        .expect("ascii")
        .parse()
        .expect("seconds");
    assert!(
        (1..=window_secs).contains(&retry_after),
        "Retry-After {retry_after} inside the window"
    );
    res.text().await.expect("body")
}

#[tokio::test]
async fn password_reset_is_budgeted_per_address_and_per_mailbox() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let base = spawn(store).await;
    let client = reqwest::Client::new();
    let forgot = |peer: &str, email: &str| {
        client
            .post(format!("{base}/auth/password/forgot"))
            .header("x-forwarded-for", peer)
            .json(&json!({ "email": email }))
            .send()
    };
    let window = u64::try_from(FORGOT.window_ms / 1_000).expect("secs");

    // The constant reply, budget times — no mailer is wired, and the budget does not care.
    for _ in 0..FORGOT.per_window {
        let res = forgot("10.0.0.1", "someone@example.test")
            .await
            .expect("forgot");
        assert_eq!(res.status(), 202);
    }
    // Spent for this address, whatever the mailbox…
    let body = assert_spent(
        forgot("10.0.0.1", "other@example.test")
            .await
            .expect("forgot"),
        window,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Too many password-reset requests"),
        "a sentence, not a code: {body}"
    );
    assert!(json["retryAfterSecs"].as_u64().unwrap_or(0) >= 1);
    // …and for this mailbox, whatever the address (or how it was typed).
    assert_spent(
        forgot("10.0.0.2", "  SOMEONE@Example.test ")
            .await
            .expect("forgot"),
        window,
    )
    .await;
    // A fresh address for a fresh mailbox is not touched by either.
    let res = forgot("10.0.0.2", "third@example.test")
        .await
        .expect("forgot");
    assert_eq!(res.status(), 202);

    // The styled form shares the budget and refuses as a page.
    let res = client
        .post(format!("{base}/forgot-password"))
        .header("x-forwarded-for", "10.0.0.1")
        .form(&[("email", "someone@example.test")])
        .send()
        .await
        .expect("form");
    let body = assert_spent(res, window).await;
    assert!(body.contains("Too many requests"), "page: {body}");
}

#[tokio::test]
async fn only_wrong_passwords_spend_the_login_budget() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("login-{}@example.test", AccountId::new().as_str());
    seed_account(&store, &email, "rightpass1", None).await;
    let base = spawn(store).await;
    let client = reqwest::Client::new();
    let login = |peer: &str, password: &str| {
        client
            .post(format!("{base}/auth/login"))
            .header("x-forwarded-for", peer)
            .json(&json!({ "email": &email, "password": password }))
            .send()
    };
    let window = u64::try_from(LOGIN_FAILURES.window_ms / 1_000).expect("secs");

    // Right passwords do not count: more of them than the budget, all fine.
    for _ in 0..=LOGIN_FAILURES.per_window {
        let res = login("10.0.0.1", "rightpass1").await.expect("login");
        assert_eq!(res.status(), 200);
        assert!(cookie_value(&res, "og_access").is_some());
    }
    // Wrong ones do, and are refused with the ordinary sentence until the budget is spent.
    for _ in 0..LOGIN_FAILURES.per_window {
        let res = login("10.0.0.1", "wrongpass").await.expect("login");
        assert_eq!(res.status(), 401);
    }
    // Spent: even the right password is not looked at from this address.
    let body = assert_spent(
        login("10.0.0.1", "rightpass1").await.expect("login"),
        window,
    )
    .await;
    assert!(body.contains("Too many failed sign-in attempts"), "{body}");
    // Another address is another budget.
    let res = login("10.0.0.9", "rightpass1").await.expect("login");
    assert_eq!(res.status(), 200);

    // The desktop's form shares the budget: refused as the styled page, credentials untouched.
    let res = client
        .post(format!("{base}/loginDeepControl"))
        .header("x-forwarded-for", "10.0.0.1")
        .form(&[
            ("email", email.as_str()),
            ("password", "rightpass1"),
            ("challenge", "c"),
            ("uuid", "u"),
        ])
        .send()
        .await
        .expect("form");
    let body = assert_spent(res, window).await;
    assert!(
        body.contains("Too many failed sign-in attempts"),
        "page: {body}"
    );
}

#[tokio::test]
async fn domain_verification_is_budgeted_per_org() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let tag = now_ms().to_string();
    let (_, admin_email) = seed_org(&store, &format!("vouched-{tag}.test"), "adminpass1").await;
    let base = spawn(store).await;
    let client = reqwest::Client::new();
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&json!({ "email": admin_email, "password": "adminpass1" }))
        .send()
        .await
        .expect("login");
    assert_eq!(res.status(), 200);
    let cookie = format!(
        "og_access={}",
        cookie_value(&res, "og_access").expect("access cookie")
    );
    let claimed = format!("claimed-{tag}.test");
    let res = client
        .post(format!("{base}/admin/domains"))
        .header("cookie", &cookie)
        .json(&json!({ "domain": claimed }))
        .send()
        .await
        .expect("claim");
    assert_eq!(res.status(), 201);

    // Nothing published: 409 each time, budget times, then the org is out of lookups.
    let verify = || {
        client
            .post(format!("{base}/admin/domains/{claimed}/verify"))
            .header("cookie", &cookie)
            .send()
    };
    for _ in 0..DOMAIN_VERIFY.per_window {
        let res = verify().await.expect("verify");
        assert_eq!(res.status(), 409);
    }
    let window = u64::try_from(DOMAIN_VERIFY.window_ms / 1_000).expect("secs");
    let body = assert_spent(verify().await.expect("verify"), window).await;
    assert!(body.contains("too many verification attempts"), "{body}");
}

#[tokio::test]
async fn client_registration_is_budgeted_per_address() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let base = spawn(store).await;
    let client = reqwest::Client::new();
    let register = |peer: &str| {
        client
            .post(format!("{base}/oauth/mcp/register"))
            .header("x-forwarded-for", peer)
            .json(&json!({
                "client_name": "budget test",
                "redirect_uris": ["http://localhost:4242/callback"],
            }))
            .send()
    };
    for _ in 0..CLIENT_REGISTRATION.per_window {
        let res = register("10.0.0.1").await.expect("register");
        assert_eq!(res.status(), 201);
    }
    let window = u64::try_from(CLIENT_REGISTRATION.window_ms / 1_000).expect("secs");
    let body = assert_spent(register("10.0.0.1").await.expect("register"), window).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["error"], "invalid_client_metadata");
    assert!(
        json["error_description"]
            .as_str()
            .unwrap_or_default()
            .starts_with("too many registrations from this address"),
        "{body}"
    );
    let res = register("10.0.0.2").await.expect("register");
    assert_eq!(res.status(), 201);
}
