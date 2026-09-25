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
    app_with_dev_sign_in(store, secret, false)
}

/// `dev_sign_in` is `OG_DEV_SIGN_IN=1` — what the smokes' servers run with and nothing else does.
fn app_with_dev_sign_in(store: PgStore, secret: &[u8], dev_sign_in: bool) -> axum::Router {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(secret)),
        "host@og.local".to_string(),
    )
    .with_dev_sign_in(dev_sign_in);
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

#[tokio::test]
async fn logging_in_does_not_clobber_the_account_projection() {
    // A regression guard for a real bug: mint_session/rotate used to write a bare "session_only"
    // view, which append_account upserts over the projection — silently wiping the person's name
    // and enabled flag on every sign-in. Invisible to GET /account (it reads the aggregate) but
    // corrupting to the admin user list, which reads the projection.
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let email = format!("keep-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &email, "password1", true, true).await;

    let base = spawn(app_with(
        store.clone(),
        b"web-console-test-secret-web-console",
    ))
    .await;
    let client = reqwest::Client::new();

    // Sign in twice — under the old code the second login would already have disabled the row.
    for _ in 0..2 {
        let res = client
            .post(format!("{base}/auth/login"))
            .json(&serde_json::json!({ "email": email, "password": "password1" }))
            .send()
            .await
            .expect("login");
        assert_eq!(res.status(), 200);
    }

    // The projection still carries the real profile and the enabled flag.
    let view = store
        .account_by_email(&email)
        .await
        .expect("query")
        .expect("account exists");
    assert!(
        view.enabled,
        "login must not disable the account in the projection"
    );
    assert!(
        view.verified,
        "login must not un-verify the account in the projection"
    );
    assert_eq!(view.first_name, "Test");
    assert_eq!(view.last_name, "User");
}

/// Serve the way `crates/opengrok/src/main.rs` does, with the socket peer attached — the one fact
/// about a caller that a request cannot write for itself.
async fn spawn_with_peers(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// THE DEV SIGN-IN MINTS A SESSION WITH NO PASSWORD, so who may call it is the whole of its
/// security. It used to read only the `Host` header — which the caller writes — so any machine
/// that could reach the port (a coworker's own Docker box on the bridge among them) sent
/// `Host: 127.0.0.1` and walked off with a session for any email, the org admin's included.
#[tokio::test]
async fn the_dev_sign_in_answers_only_a_local_caller_and_never_takes_over_a_real_account() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let admin = format!("devadmin-{}@og.local", uuid::Uuid::now_v7().simple());
    seed_account(&store, &admin, "password1", true, true).await;
    let secret = b"web-console-test-secret-web-console";
    let client = reqwest::Client::new();
    let dev = |base: &str, email: &str| {
        format!("{base}/auth/cursor_dev_session_token?plan=pro&email={email}")
    };

    // A server that cannot see its peer cannot vouch for one: a forged loopback Host is not
    // enough on its own.
    let blind = spawn(app_with_dev_sign_in(store.clone(), secret, true)).await;
    let fresh = format!("dev-{}@og.local", uuid::Uuid::now_v7().simple());
    let res = client
        .get(dev(&blind, &fresh))
        .header(reqwest::header::HOST, "127.0.0.1")
        .send()
        .await
        .expect("blind request");
    assert_eq!(res.status(), 401, "no peer is a remote peer");
    let body = res.text().await.expect("body");
    assert!(
        !body.contains("accessToken"),
        "no token without a peer: {body}"
    );

    let base = spawn_with_peers(app_with_dev_sign_in(store.clone(), secret, true)).await;

    // The smokes' path: a loopback caller, a fresh throwaway email.
    let res = client.get(dev(&base, &fresh)).send().await.expect("fresh");
    assert_eq!(res.status(), 200, "a local caller signs a fresh email in");
    let body: serde_json::Value = res.json().await.expect("json");
    assert!(body["accessToken"].is_string());
    // Idempotent per email (slice 1 re-signs the same address).
    let again = client.get(dev(&base, &fresh)).send().await.expect("again");
    assert_eq!(again.status(), 200, "the same dev email signs in twice");

    // An account with a password is a person's, and only their password signs them in.
    let res = client
        .get(dev(&base, &admin))
        .send()
        .await
        .expect("takeover");
    assert_eq!(
        res.status(),
        401,
        "dev sign-in must not take over a real account"
    );
    let body = res.text().await.expect("body");
    assert!(!body.contains("accessToken"), "{body}");
    assert!(body.contains("password"), "the refusal says why: {body}");

    // A front on this machine (Caddy, `docs/setup/tls.md`) connects from loopback for every LAN
    // caller; the header it adds is what tells them apart.
    let res = client
        .get(dev(&base, &fresh))
        .header("x-forwarded-for", "192.168.1.9")
        .send()
        .await
        .expect("proxied");
    assert_eq!(res.status(), 401, "a proxied caller is not a local one");

    // A non-loopback Host is still refused, as slice 16 has always checked.
    let res = client
        .get(dev(&base, &fresh))
        .header(reqwest::header::HOST, "192.168.1.9:1447")
        .send()
        .await
        .expect("lan host");
    assert_eq!(res.status(), 401, "a LAN Host is refused");
}

/// A peer on another machine — or a container on the Docker bridge — with a forged loopback
/// `Host`. This is the request that minted any session before; the socket says who it is.
#[tokio::test]
async fn a_remote_peer_with_a_forged_loopback_host_gets_no_dev_session() {
    use tower::ServiceExt;
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let app = app_with_dev_sign_in(store, b"web-console-test-secret-web-console", true);
    let peer: std::net::SocketAddr = "172.17.0.2:40000".parse().expect("addr");
    let request = axum::http::Request::builder()
        .uri("/auth/cursor_dev_session_token?plan=pro&email=bridge@og.local")
        .header(axum::http::header::HOST, "127.0.0.1:1447")
        .extension(axum::extract::ConnectInfo(peer))
        .body(axum::body::Body::empty())
        .expect("request");
    let res = app.oneshot(request).await.expect("oneshot");
    assert_eq!(res.status(), 401, "a bridge peer is not loopback");
}

/// LOCAL IS NOT ENOUGH. On Docker Desktop a coworker box's traffic to the host arrives through a
/// host process — loopback peer, loopback Host, no forwarding header — so the local-caller check
/// cannot tell it from the developer's shell. The switch is the only thing that can, and a
/// deployment that never set it mints nothing, even for the caller the smokes are.
#[tokio::test]
async fn the_dev_sign_in_mints_nothing_unless_the_deployment_opted_in() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let base = spawn_with_peers(app_with(store, b"web-console-test-secret-web-console")).await;
    let fresh = format!("off-{}@og.local", uuid::Uuid::now_v7().simple());
    let res = reqwest::Client::new()
        .get(format!(
            "{base}/auth/cursor_dev_session_token?plan=pro&email={fresh}"
        ))
        .send()
        .await
        .expect("local caller");
    assert_eq!(res.status(), 401, "off by default, even for a local peer");
    let body: serde_json::Value = res.json().await.expect("json");
    assert_eq!(
        body["shouldLogout"], true,
        "the SessionRejected shape: {body}"
    );
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("OG_DEV_SIGN_IN"),
        "names the switch: {error}"
    );
    assert!(body.get("accessToken").is_none(), "{body}");
}
