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
//! INVITES (#237). One code admits one person. Signups racing for one code, and a signup racing
//! any other write to the org's stream, used to lose the redemption silently: the account was
//! created, the org append was refused as a conflict and ignored, and the code stayed open.
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
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;

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
    (router_for(auth.clone()), auth)
}

fn router_for(auth: AuthState) -> axum::Router {
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
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
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
    let (org_id, admin_email) = seed_org(&store, &vouched, "adminpass1").await;

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

    // Two admins at once: a write decided against a stale org is refused by the store, which is
    // what stops a second concurrent claim from silently replacing the first admin's token.
    let (org, stale_seq) = store.load_org(&org_id).await.expect("load");
    let events = org
        .decide(OrgCommand::ClaimDomain {
            domain: format!("race-{stamp}.test"),
            token: "dv_first".to_string(),
            at_ms: now_ms(),
        })
        .expect("claim");
    let mut first = org.clone();
    for event in &events {
        first.apply(event);
    }
    store
        .append_org(&org_id, stale_seq, &events, &first, now_ms())
        .await
        .expect("the first writer lands");
    let second = store
        .append_org(&org_id, stale_seq, &events, &first, now_ms())
        .await;
    assert!(
        matches!(second, Err(opengrok_store::StoreError::Conflict)),
        "the second writer at the same seq must conflict: {second:?}"
    );

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

/// A mailer whose every send fails, with no network: a bearer that is not a valid header value
/// makes the request unbuildable, so `send` answers false before it dials anybody. That is the
/// 20 Sep shape without a Resend account — `RESEND_FROM_EMAIL` left on a domain the account never
/// verified rejects every send the same way.
const MAILER_THAT_NEVER_DELIVERS: &str = "re_never\ndelivers";

async fn issue_invite(client: &reqwest::Client, base: &str, cookie: &str) -> String {
    let res = client
        .post(format!("{base}/admin/invites"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("invite");
    assert_eq!(res.status(), 201);
    let body: serde_json::Value = res.json().await.expect("json");
    body["code"].as_str().expect("code").to_string()
}

/// A MAIL THAT NEVER ARRIVED USED TO STRAND THE ACCOUNT FOR GOOD. With a mailer configured, signup
/// registers `verified = false` and sends one link; a failed send is only logged, the link dies in
/// 24 hours, a second signup is refused as a duplicate, and the admin's Enable flips only
/// `enabled` — so the person got "not verified" forever while the console said "enabled". The
/// admin now vouches for the address explicitly, in their own org and nobody else's.
#[tokio::test]
async fn a_member_whose_verification_mail_never_arrived_can_be_verified_by_their_admin() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let stamp = uuid::Uuid::now_v7().simple().to_string();
    let domain = format!("strand-{stamp}.test");
    let (_org, admin_email) = seed_org(&store, &domain, "adminpass1").await;
    let (_other_org, other_admin) =
        seed_org(&store, &format!("elsewhere-{stamp}.test"), "adminpass1").await;

    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"stranded-signup-secret")),
        "host@og.local".to_string(),
    )
    .with_resend(
        Some(MAILER_THAT_NEVER_DELIVERS.to_string()),
        "http://127.0.0.1".to_string(),
    );
    let base = spawn(router_for(auth)).await;
    let client = reqwest::Client::new();
    let cookie = cookie_login(&client, &base, &admin_email, "adminpass1").await;

    // The signup succeeds and says, truthfully, that no mail went out.
    let code = issue_invite(&client, &base, &cookie).await;
    let jo = format!("jo@{domain}");
    let res = client
        .post(format!("{base}/auth/signup"))
        .json(&serde_json::json!({ "email": jo, "password": "password1", "code": code }))
        .send()
        .await
        .expect("signup");
    assert_eq!(res.status(), 201);
    let reply: serde_json::Value = res.json().await.expect("json");
    assert_eq!(reply["verification_email_sent"], false);
    assert_eq!(reply["verified"], false);
    let jo_id = reply["account_id"].as_str().expect("id").to_string();

    // The console shows why Jo cannot sign in.
    let users: serde_json::Value = client
        .get(format!("{base}/admin/users"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("users")
        .json()
        .await
        .expect("json");
    let row = users["users"]
        .as_array()
        .expect("users")
        .iter()
        .find(|user| user["id"] == jo_id.as_str())
        .expect("jo listed")
        .clone();
    assert_eq!(row["verified"], false);

    // Enable alone still leaves the address unproven — the admin has not said they vouch for it.
    let res = client
        .post(format!("{base}/admin/users/{jo_id}/enable"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("enable");
    assert_eq!(res.status(), 200);
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "email": jo, "password": "password1" }))
        .send()
        .await
        .expect("login");
    assert_eq!(res.status(), 403);
    let text = res.text().await.expect("text");
    assert!(text.contains("not verified"), "{text}");
    assert!(
        text.contains("administrator"),
        "the refusal names a way out: {text}"
    );

    // Another org's admin cannot reach Jo at all — not to verify, not to enable or disable.
    let other_cookie = cookie_login(&client, &base, &other_admin, "adminpass1").await;
    for action in ["verify", "enable", "disable"] {
        let res = client
            .post(format!("{base}/admin/users/{jo_id}/{action}"))
            .header("cookie", &other_cookie)
            .send()
            .await
            .expect("cross-org");
        assert_eq!(res.status(), 404, "another org's admin may not {action} Jo");
    }

    // Jo's own admin vouches for the address; Jo signs in.
    let res = client
        .post(format!("{base}/admin/users/{jo_id}/verify"))
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
    let verified: serde_json::Value = res.json().await.expect("json");
    assert_eq!(verified["verified"], true);
    assert_eq!(verified["enabled"], true);
    // Twice is the same answer, not a second event.
    let res = client
        .post(format!("{base}/admin/users/{jo_id}/verify"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("verify again");
    assert_eq!(res.status(), 200);
    let jo_cookie = cookie_login(&client, &base, &jo, "password1").await;

    // A member is not an admin.
    let res = client
        .post(format!("{base}/admin/users/{jo_id}/verify"))
        .header("cookie", &jo_cookie)
        .send()
        .await
        .expect("member verify");
    assert_eq!(res.status(), 403);
    // ...and its own account is theirs to use, not to administer: still 403, not the admin's 409.
    let res = client
        .post(format!("{base}/admin/users/{jo_id}/disable"))
        .header("cookie", &jo_cookie)
        .send()
        .await
        .expect("member self-disable");
    assert_eq!(res.status(), 403);

    // The styled form tells the truth about the mail instead of "check your email".
    let code = issue_invite(&client, &base, &cookie).await;
    let res = client
        .post(format!("{base}/signup"))
        .form(&[
            ("email", format!("sam@{domain}").as_str()),
            ("password", "password1"),
            ("code", code.as_str()),
        ])
        .send()
        .await
        .expect("form signup");
    assert_eq!(res.status(), 200);
    let page = res.text().await.expect("page");
    assert!(!page.contains("Check your email"), "{page}");
    assert!(page.contains("could not be sent"), "{page}");
    assert!(page.contains("administrator"), "{page}");
}

/// A stand-in for Resend's send endpoint: every body posted to it, in order.
async fn spawn_mailbox() -> (String, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
    let mail = Arc::new(std::sync::Mutex::new(Vec::new()));
    let inbox = mail.clone();
    let app = axum::Router::new().route(
        "/emails",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let inbox = inbox.clone();
            async move {
                inbox.lock().expect("inbox").push(body);
                axum::Json(serde_json::json!({ "id": "stand-in" }))
            }
        }),
    );
    let base = spawn(app).await;
    (format!("{base}/emails"), mail)
}

/// Wait for the mailbox to hold `count` mails — the send is on its own task by design.
async fn mail_count_reaches(mail: &std::sync::Mutex<Vec<serde_json::Value>>, count: usize) -> bool {
    for _ in 0..100 {
        if mail.lock().expect("inbox").len() >= count {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// The `token=` a mail's link carries.
fn token_in(mail: &serde_json::Value) -> String {
    let html = mail["html"].as_str().expect("html");
    let start = html.find("token=").expect("a link") + "token=".len();
    html[start..].split('"').next().expect("token").to_string()
}

/// A LINK THAT EXPIRED OR NEVER ARRIVED HAD NO WAY BACK but the admin. The person asks for a new
/// one: a waiting account gets a fresh 24-hour link that works; an unknown or already-verified
/// address gets the same 202 and no mail; and the door is budgeted per mailbox (so one inbox
/// cannot be flooded from many peers) and per peer (so one peer cannot walk the address book).
#[tokio::test]
async fn a_waiting_member_can_ask_for_a_new_verification_link_and_the_reply_never_says_who() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let stamp = uuid::Uuid::now_v7().simple().to_string();
    let domain = format!("resend-{stamp}.test");
    let (_org, admin_email) = seed_org(&store, &domain, "adminpass1").await;
    let (endpoint, mail) = spawn_mailbox().await;
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"verify-resend-secret")),
        "host@og.local".to_string(),
    )
    .with_resend(
        Some("re_stand_in".to_string()),
        "http://og.test".to_string(),
    )
    .with_resend_endpoint(endpoint);
    let base = spawn(router_for(auth)).await;
    let client = reqwest::Client::new();
    let cookie = cookie_login(&client, &base, &admin_email, "adminpass1").await;

    // Jo signs up; the first mail goes out and is lost.
    let code = issue_invite(&client, &base, &cookie).await;
    let jo = format!("jo@{domain}");
    let res = client
        .post(format!("{base}/auth/signup"))
        .json(&serde_json::json!({ "email": jo, "password": "password1", "code": code }))
        .send()
        .await
        .expect("signup");
    assert_eq!(res.status(), 201);
    assert!(mail_count_reaches(&mail, 1).await, "the signup mail");

    let resend = |email: &str, peer: &str| {
        client
            .post(format!("{base}/auth/verify/resend"))
            .header("x-forwarded-for", peer)
            .json(&serde_json::json!({ "email": email }))
            .send()
    };
    let constant = serde_json::json!({ "accepted": true, "mailer": true });

    // Jo asks again and gets a second, working link.
    let res = resend(&jo, "10.1.1.1").await.expect("resend");
    assert_eq!(res.status(), 202);
    assert_eq!(
        res.json::<serde_json::Value>().await.expect("json"),
        constant
    );
    assert!(mail_count_reaches(&mail, 2).await, "the resent mail");
    let second = mail.lock().expect("inbox")[1].clone();
    assert_eq!(second["to"], serde_json::json!([jo]));
    let page = client
        .get(format!("{base}/auth/verify?token={}", token_in(&second)))
        .send()
        .await
        .expect("verify");
    assert_eq!(page.status(), 200);
    assert!(page.text().await.expect("page").contains("verified"));
    let jo_view = store
        .account_by_email(&jo)
        .await
        .expect("read")
        .expect("jo");
    assert!(jo_view.verified, "the resent link verified the address");

    // A verified address and a stranger's guess: the same reply, and no mail for either.
    for email in [jo.clone(), format!("nobody-{stamp}@{domain}")] {
        let res = resend(&email, "10.1.1.1").await.expect("resend");
        assert_eq!(res.status(), 202);
        assert_eq!(
            res.json::<serde_json::Value>().await.expect("json"),
            constant
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        mail.lock().expect("inbox").len(),
        2,
        "no mail to a verified or unknown address"
    );

    // Per mailbox: five from five peers, then the sixth is refused whoever sends it.
    let target = format!("target-{stamp}@{domain}");
    for n in 1..=5 {
        let res = resend(&target, &format!("10.2.0.{n}"))
            .await
            .expect("resend");
        assert_eq!(res.status(), 202, "hit {n}");
    }
    let res = resend(&target, "10.2.0.6").await.expect("resend");
    assert_eq!(res.status(), 429, "one mailbox, many peers");
    assert!(res.headers().contains_key(reqwest::header::RETRY_AFTER));
    let body: serde_json::Value = res.json().await.expect("json");
    assert!(
        body["retryAfterSecs"].as_u64().is_some_and(|secs| secs > 0),
        "{body}"
    );

    // Per peer: five mailboxes from one address, then the sixth is refused.
    for n in 1..=5 {
        let res = resend(&format!("walk-{n}-{stamp}@{domain}"), "10.3.0.1")
            .await
            .expect("resend");
        assert_eq!(res.status(), 202, "mailbox {n}");
    }
    let res = resend(&format!("walk-6-{stamp}@{domain}"), "10.3.0.1")
        .await
        .expect("resend");
    assert_eq!(res.status(), 429, "one peer, many mailboxes");

    // The sign-in card links to the styled resend card, which is a form while a mailer is wired.
    let login = client
        .get(format!(
            "{base}/loginDeepControl?challenge=c&uuid=u-{stamp}"
        ))
        .send()
        .await
        .expect("login page")
        .text()
        .await
        .expect("text");
    assert!(login.contains("/resend-verification"), "{login}");
    let card = client
        .get(format!("{base}/resend-verification"))
        .send()
        .await
        .expect("card")
        .text()
        .await
        .expect("text");
    assert!(card.contains("action=\"/resend-verification\""), "{card}");
}

/// Issue `code` straight into the org's stream, as the console's invite button does.
async fn issue_in_store(store: &PgStore, org_id: &OrgId, code: &str) {
    let (org, seq) = store.load_org(org_id).await.expect("load org");
    let at_ms = now_ms();
    let events = org
        .decide(OrgCommand::IssueInvite {
            code: code.to_string(),
            at_ms,
        })
        .expect("issue");
    let mut state = org;
    for event in &events {
        state.apply(event);
    }
    store
        .append_org(org_id, seq, &events, &state, at_ms)
        .await
        .expect("append invite");
}

/// The common trigger: the org's stream moves on between the signup's read and its write (an
/// admin issuing another code, a domain claim). The redemption must land on the org as it now
/// is, not be dropped as a conflict.
#[tokio::test]
async fn an_invite_redeemed_while_the_org_moved_on_is_still_spent() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let stamp = uuid::Uuid::now_v7().simple().to_string();
    let domain = format!("stale-{stamp}.test");
    let (org_id, _) = seed_org(&store, &domain, "adminpass1").await;
    let code = format!("code-a-{stamp}");
    issue_in_store(&store, &org_id, &code).await;

    let read_by_the_signup = store.load_org(&org_id).await.expect("load org");
    issue_in_store(&store, &org_id, &format!("code-b-{stamp}")).await;

    let account = AccountId::new();
    let redeemed = opengrok_server::auth::identity::redeem_invite(
        &store,
        &org_id,
        read_by_the_signup,
        &code,
        &domain,
        &account,
    )
    .await;
    assert!(redeemed.is_ok(), "{redeemed:?}");
    let (org, _) = store.load_org(&org_id).await.expect("load org");
    assert_eq!(
        org.invites.get(&code),
        Some(&opengrok_core::org::InviteState::Redeemed(account.clone())),
        "the redemption was dropped: {:?}",
        org.invites
    );
    assert!(org.members.contains(&account));
}

/// Eight people with one code: one account, one redemption, seven refusals.
#[tokio::test]
async fn one_invite_admits_one_signup_however_many_race_for_it() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let stamp = uuid::Uuid::now_v7().simple().to_string();
    let domain = format!("race-{stamp}.test");
    let (org_id, admin_email) = seed_org(&store, &domain, "adminpass1").await;
    let (app, _) = app_with(store.clone(), b"invite-race-secret", None);
    let base = spawn(app).await;
    let client = reqwest::Client::new();
    let cookie = cookie_login(&client, &base, &admin_email, "adminpass1").await;
    let code = issue_invite(&client, &base, &cookie).await;

    let signups = (0..8).map(|n| {
        let client = client.clone();
        let url = format!("{base}/auth/signup");
        let body = serde_json::json!({
            "email": format!("racer{n}@{domain}"),
            "password": "password1",
            "code": code,
        });
        async move {
            let res = client.post(url).json(&body).send().await.expect("signup");
            res.status().as_u16()
        }
    });
    let statuses = futures::future::join_all(signups).await;
    assert_eq!(
        statuses.iter().filter(|s| **s == 201).count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| *s == 201 || *s == 403),
        "{statuses:?}"
    );
    let accounts = store
        .accounts_by_org(org_id.as_str())
        .await
        .expect("accounts");
    assert_eq!(
        accounts.len(),
        2,
        "the admin and one member: {:?}",
        accounts.iter().map(|a| &a.email).collect::<Vec<_>>()
    );
    let listed: serde_json::Value = client
        .get(format!("{base}/admin/invites"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("invites")
        .json()
        .await
        .expect("json");
    let row = listed["invites"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["code"] == code.as_str()))
        .cloned()
        .unwrap_or_default();
    assert_eq!(row["state"], "redeemed", "{listed}");
}
