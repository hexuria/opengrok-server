//! Drives 12.later over a real HTTP socket: domain ownership proof and password reset.
//!
//! DOMAINS. A console admin claims a domain, is handed a TXT record, and nothing changes until it
//! resolves — a signup under the claimed domain is still refused. The lookup is answered by a
//! `StaticDns` the test controls, so "publishing the record" is one call and the whole path
//! (claim → refused signup → publish → verify → admitted signup) runs with no domain owned. A
//! state with no resolver bound answers 503, not "no record" — an outage is not a wrong record.
//!
//! PASSWORD RESET. The token is minted directly (there is no mailbox here) and walked through the
//! page: a bad password is refused, a good one changes the login, the SAME link presented again is
//! refused as used, and a tampered token is refused as invalid. The forgot endpoint answers 202
//! and says `mailer: false` because no Resend key is configured.
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
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::domain_proof::{StaticDns, TxtLookup};
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

/// A ready credential account (verified + enabled), optionally inside an org.
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

/// An org with one operator-vouched domain and its admin, both fresh.
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

fn app_with(
    store: PgStore,
    secret: &[u8],
    dns: Option<Arc<dyn TxtLookup>>,
) -> (axum::Router, AuthState) {
    let mut auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(secret)),
        "host@og.local".to_string(),
    );
    if let Some(dns) = dns {
        auth = auth.with_dns(dns);
    }
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
        Some("http://opengrok.lan:1447".to_string()),
    );
    (opengrok_server::router(agui, gateway), auth)
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

/// Sign in by cookie and hand back the `Cookie:` header value the console would carry.
async fn cookie_login(client: &reqwest::Client, base: &str, email: &str, password: &str) -> String {
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .expect("login");
    assert_eq!(
        res.status(),
        200,
        "login: {}",
        res.text().await.unwrap_or_default()
    );
    let access = cookie_value(&res, "og_access").expect("access cookie");
    format!("og_access={access}")
}

#[tokio::test]
async fn a_claimed_domain_admits_nobody_until_its_txt_record_resolves() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let stamp = uuid::Uuid::now_v7().simple().to_string();
    let vouched = format!("acme-{stamp}.test");
    let (_, admin_email) = seed_org(&store, &vouched, "adminpass1").await;

    let dns = Arc::new(StaticDns::new());
    let lookup: Arc<dyn TxtLookup> = dns.clone();
    let (app, _) = app_with(store.clone(), b"domain-proof-secret", Some(lookup));
    let base = spawn(app).await;
    let client = reqwest::Client::new();
    let cookie = cookie_login(&client, &base, &admin_email, "adminpass1").await;

    // The vouched domain is listed verified from the start.
    let listed: serde_json::Value = client
        .get(format!("{base}/admin/domains"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    assert_eq!(listed["domains"][0]["domain"], vouched);
    assert_eq!(listed["domains"][0]["status"], "verified");

    // Claim: pending, and the exact record to publish comes back.
    let claimed = format!("Proof-{stamp}.test");
    let res = client
        .post(format!("{base}/admin/domains"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "domain": claimed }))
        .send()
        .await
        .expect("claim");
    assert_eq!(res.status(), 201);
    let claim: serde_json::Value = res.json().await.expect("json");
    let domain = claim["domain"].as_str().expect("domain").to_string();
    assert_eq!(domain, claimed.to_lowercase(), "normalized on the way in");
    assert_eq!(claim["status"], "pending");
    let record_name = claim["record"]["name"].as_str().expect("name").to_string();
    let record_value = claim["record"]["value"]
        .as_str()
        .expect("value")
        .to_string();
    assert_eq!(record_name, format!("_opengrok-verify.{domain}"));
    assert!(
        record_value.starts_with("opengrok-verify=dv_"),
        "{record_value}"
    );

    // Nothing published yet: verify says so (409), and a signup under the domain is refused.
    let res = client
        .post(format!("{base}/admin/domains/{domain}/verify"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("verify");
    assert_eq!(res.status(), 409);
    let body: serde_json::Value = res.json().await.expect("json");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("no TXT record found"),
        "{body}"
    );

    let invite: serde_json::Value = client
        .post(format!("{base}/admin/invites"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("invite")
        .json()
        .await
        .expect("json");
    let code = invite["code"].as_str().expect("code").to_string();
    let signup = |email: String, code: String| {
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/auth/signup"))
                .json(&serde_json::json!({ "email": email, "password": "password1", "code": code }))
                .send()
                .await
                .expect("signup")
        }
    };
    let res = signup(format!("jo@{domain}"), code.clone()).await;
    assert_eq!(res.status(), 403, "a claim is not a proof");

    // A record with the wrong value is not proof either, and the reason names the gap.
    dns.publish(
        &record_name,
        vec!["opengrok-verify=dv_somebody_else".to_string()],
    )
    .await;
    let res = client
        .post(format!("{base}/admin/domains/{domain}/verify"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("verify");
    assert_eq!(res.status(), 409);
    let body: serde_json::Value = res.json().await.expect("json");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("none of its values"),
        "{body}"
    );

    // Publish the real record: verified, and the same invite now admits the signup.
    dns.publish(
        &record_name,
        vec!["v=spf1 -all".to_string(), format!("\"{record_value}\"")],
    )
    .await;
    let res = client
        .post(format!("{base}/admin/domains/{domain}/verify"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("verify");
    assert_eq!(
        res.status(),
        200,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let res = signup(format!("jo@{domain}"), code).await;
    assert_eq!(
        res.status(),
        201,
        "{}",
        res.text().await.unwrap_or_default()
    );

    // The projection agrees: verified, nothing pending.
    let listed: serde_json::Value = client
        .get(format!("{base}/admin/domains"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    let entries = listed["domains"].as_array().expect("array");
    assert!(
        entries
            .iter()
            .any(|e| e["domain"] == domain && e["status"] == "verified"),
        "{listed}"
    );
    assert!(
        entries.iter().all(|e| e["status"] == "verified"),
        "{listed}"
    );

    // Re-claiming a verified domain is refused; a non-domain is refused up front.
    let res = client
        .post(format!("{base}/admin/domains"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "domain": domain }))
        .send()
        .await
        .expect("claim");
    assert_eq!(res.status(), 409);
    let res = client
        .post(format!("{base}/admin/domains"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "domain": "not a domain" }))
        .send()
        .await
        .expect("claim");
    assert_eq!(res.status(), 422);

    // A withdrawn claim is gone.
    let typo = format!("typo-{stamp}.test");
    let res = client
        .post(format!("{base}/admin/domains"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "domain": typo }))
        .send()
        .await
        .expect("claim");
    assert_eq!(res.status(), 201);
    let res = client
        .delete(format!("{base}/admin/domains/{typo}"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("withdraw");
    assert_eq!(res.status(), 204);
    let res = client
        .delete(format!("{base}/admin/domains/{typo}"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("withdraw again");
    assert_eq!(res.status(), 404);

    // No resolver bound: the claim stands, the check is 503 — never a false "not there".
    let (bare, _) = app_with(store.clone(), b"domain-proof-secret", None);
    let bare_base = spawn(bare).await;
    let pending = format!("later-{stamp}.test");
    let res = client
        .post(format!("{bare_base}/admin/domains"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "domain": pending }))
        .send()
        .await
        .expect("claim");
    assert_eq!(res.status(), 201);
    let res = client
        .post(format!("{bare_base}/admin/domains/{pending}/verify"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("verify");
    assert_eq!(res.status(), 503);
}

#[tokio::test]
async fn a_reset_link_changes_the_password_once_and_only_once() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("reset-{}@og.local", uuid::Uuid::now_v7().simple());
    let id = seed_account(&store, &email, "oldpass123", None).await;
    let (app, auth) = app_with(store.clone(), b"reset-secret", None);
    let base = spawn(app).await;
    let client = reqwest::Client::new();

    // No mailer: forgot is honest about it, and never about the address.
    let res = client
        .post(format!("{base}/auth/password/forgot"))
        .json(&serde_json::json!({ "email": "nobody@og.local" }))
        .send()
        .await
        .expect("forgot");
    assert_eq!(res.status(), 202);
    let body: serde_json::Value = res.json().await.expect("json");
    assert_eq!(
        body,
        serde_json::json!({ "accepted": true, "mailer": false })
    );
    let page = client
        .get(format!("{base}/forgot-password"))
        .send()
        .await
        .expect("page")
        .text()
        .await
        .expect("text");
    assert!(page.contains("not set up to send email"), "{page}");

    // The link, minted the way the mail would carry it.
    let (account, _) = store.load_account(&id).await.expect("load");
    let hash = account.password_hash.clone().expect("hash");
    let token =
        opengrok_server::auth::password_reset::mint_reset_token(&auth, &id, &hash, now_ms())
            .expect("token");
    let page = client
        .get(format!("{base}/reset-password?token={token}"))
        .send()
        .await
        .expect("page");
    assert_eq!(page.status(), 200);
    assert!(
        page.text()
            .await
            .expect("text")
            .contains("Choose a new password")
    );

    let post = |token: String, password: &'static str, confirm: &'static str| {
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/reset-password"))
                .form(&[
                    ("token", token.as_str()),
                    ("password", password),
                    ("confirm", confirm),
                ])
                .send()
                .await
                .expect("post")
        }
    };
    let res = post(token.clone(), "short", "short").await;
    assert!(
        res.text()
            .await
            .expect("text")
            .contains("at least 8 characters")
    );
    let res = post(token.clone(), "newpass456", "different1").await;
    assert!(res.text().await.expect("text").contains("do not match"));
    let res = post(token.clone(), "newpass456", "newpass456").await;
    assert_eq!(res.status(), 200);
    assert!(res.text().await.expect("text").contains("Password updated"));

    // The same link again is spent, not a second change.
    let res = post(token.clone(), "thirdpass789", "thirdpass789").await;
    assert_eq!(res.status(), 400);
    assert!(
        res.text()
            .await
            .expect("text")
            .contains("already been used")
    );
    let res = client
        .get(format!("{base}/reset-password?token={token}"))
        .send()
        .await
        .expect("page");
    assert_eq!(res.status(), 400);

    // A tampered token is invalid, not "used".
    let mut tampered = token.clone();
    tampered.pop();
    let res = post(tampered, "fourthpass0", "fourthpass0").await;
    assert_eq!(res.status(), 400);
    assert!(
        res.text()
            .await
            .expect("text")
            .contains("invalid or has expired")
    );

    // The login moved: old refused, new accepted.
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": email, "password": "oldpass123" }))
        .send()
        .await
        .expect("login");
    assert_ne!(res.status(), 200, "the old password still works");
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": email, "password": "newpass456" }))
        .send()
        .await
        .expect("login");
    assert_eq!(res.status(), 200);
    assert!(cookie_value(&res, "og_access").is_some());
}
