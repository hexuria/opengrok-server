//! The MCP door's OAuth flow, walked the way Claude Code walks it, over a real socket.
//!
//! Discovery (a 401 that names the metadata, RFC 9728 at both paths, RFC 8414), dynamic client
//! registration (RFC 7591, loopback or https redirects only), the authorization request (PKCE
//! S256, `resource` = our `/mcp`, unregistered redirect refused without a redirect), sign-in and
//! consent (the person's own coworkers only), the code (one-shot, bound to client + redirect +
//! challenge + resource, `iss` on the response), the token (a bot key with `aud`), and the door
//! accepting it — then the code refused again, a wrong verifier refused, and a key whose `aud`
//! is another server refused by the door.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent, the same bargain the other integration tests make.

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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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

/// Bind first, then build the state with the bound address as the public URL: the metadata and
/// the `resource` must name the address the client actually reaches.
async fn spawn(store: PgStore, email: &str) -> (String, AuthState) {
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
    .with_resend(None, base.clone())
    .with_cimd_loopback();
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
        Some(base.clone()),
    );
    let app = opengrok_server::router(agui, gateway);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (base, auth)
}

fn no_redirect() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
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

/// RFC 7636 appendix B.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const CALLBACK: &str = "http://localhost:8123/callback";

async fn initialize(base: &str, bearer: &str) -> (u16, Value) {
    let res = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {bearer}"))
        .json(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "handwritten-test-client", "version": "0" },
            }}),
        )
        .send()
        .await
        .expect("initialize");
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn claude_code_signs_in_through_the_browser_and_gets_a_key_the_door_accepts() {
    let database_url = database_or_skip!();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let email = format!("oauth-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email, "password1").await;
    let mine = seed_coworker(&store, &account, "Ada").await;
    let other_account = seed_account(&store, &format!("other-{email}"), "password1").await;
    let theirs = seed_coworker(&store, &other_account, "Nosy").await;
    let (base, _auth) = spawn(store.clone(), &email).await;
    let client = no_redirect();

    // Discovery: the door's 401 names the metadata; both metadata documents point at us.
    let res = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
        .send()
        .await
        .expect("bare initialize");
    assert_eq!(res.status(), 401);
    let challenge = res
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .expect("challenge")
        .to_string();
    assert!(
        challenge.contains(&format!(
            "resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
        )),
        "{challenge}"
    );
    assert!(challenge.contains("scope=\"mcp:tools\""), "{challenge}");
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let doc: Value = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("metadata")
            .json()
            .await
            .expect("json");
        assert_eq!(doc["resource"], format!("{base}/mcp"), "{path}: {doc}");
        assert_eq!(doc["authorization_servers"], json!([base]), "{path}: {doc}");
    }
    let server: Value = client
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .expect("as metadata")
        .json()
        .await
        .expect("json");
    assert_eq!(server["issuer"], base);
    assert_eq!(server["token_endpoint"], format!("{base}/oauth/mcp/token"));
    assert_eq!(
        server["registration_endpoint"],
        format!("{base}/oauth/mcp/register")
    );
    assert_eq!(server["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        server["authorization_response_iss_parameter_supported"],
        json!(true)
    );

    // Registration: loopback callback accepted; a network http callback refused.
    let res = client
        .post(format!("{base}/oauth/mcp/register"))
        .json(&json!({ "client_name": "Claude Code", "redirect_uris": ["http://tool.example/cb"] }))
        .send()
        .await
        .expect("register bad");
    assert_eq!(res.status(), 400);
    let res = client
        .post(format!("{base}/oauth/mcp/register"))
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
    assert!(client_id.starts_with("mc_"));
    assert_eq!(registered["token_endpoint_auth_method"], "none");

    let authorize = |extra: &str| {
        format!(
            "{base}/oauth/mcp/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=xyz&scope=mcp%3Atools&resource={}{extra}",
            urlencoding(CALLBACK),
            urlencoding(&format!("{base}/mcp")),
        )
    };

    // An unregistered redirect is refused on a page, never redirected to.
    let res = client
        .get(format!(
            "{base}/oauth/mcp/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&resource={}",
            urlencoding("http://localhost:9/elsewhere"),
            urlencoding(&format!("{base}/mcp")),
        ))
        .send()
        .await
        .expect("authorize bad redirect");
    assert_eq!(res.status(), 400);

    // A wrong resource is refused by redirect with the OAuth error and the state.
    let res = client
        .get(format!(
            "{base}/oauth/mcp/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=xyz&resource={}",
            urlencoding(CALLBACK),
            urlencoding("https://other.example/mcp"),
        ))
        .send()
        .await
        .expect("authorize bad resource");
    assert_eq!(res.status(), 303);
    let location = res.headers()["location"].to_str().unwrap().to_string();
    assert_eq!(
        query_param(&location, "error").as_deref(),
        Some("invalid_target")
    );
    assert_eq!(query_param(&location, "state").as_deref(), Some("xyz"));

    // No session: the sign-in card. Wrong password: the card again. Right password: consent,
    // listing only this person's coworkers.
    let res = client.get(authorize("")).send().await.expect("authorize");
    assert_eq!(res.status(), 200);
    let page = res.text().await.expect("page");
    assert!(page.contains("Claude Code"), "{page}");
    assert!(page.contains("name=password"), "{page}");
    let form_base = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.clone()),
        ("redirect_uri", CALLBACK.to_string()),
        ("code_challenge", CHALLENGE.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("state", "xyz".to_string()),
        ("scope", "mcp:tools".to_string()),
        ("resource", format!("{base}/mcp")),
    ];
    let mut wrong = form_base.clone();
    wrong.push(("email", email.clone()));
    wrong.push(("password", "nope".to_string()));
    let res = client
        .post(format!("{base}/oauth/mcp/authorize"))
        .form(&wrong)
        .send()
        .await
        .expect("bad login");
    assert!(
        res.text()
            .await
            .unwrap()
            .contains("Wrong email or password")
    );
    let mut login = form_base.clone();
    login.push(("email", email.clone()));
    login.push(("password", "password1".to_string()));
    let res = client
        .post(format!("{base}/oauth/mcp/authorize"))
        .form(&login)
        .send()
        .await
        .expect("login");
    assert_eq!(res.status(), 200);
    let consent_page = res.text().await.expect("consent page");
    assert!(consent_page.contains("Ada"), "{consent_page}");
    assert!(
        !consent_page.contains("Nosy"),
        "another account's coworker is not offered"
    );
    let consent = consent_of(&consent_page);

    // Choosing somebody else's coworker gets the card back, not a code.
    let mut steal = form_base.clone();
    steal.push(("consent", consent.clone()));
    steal.push(("coworker", theirs.as_str().to_string()));
    let res = client
        .post(format!("{base}/oauth/mcp/authorize"))
        .form(&steal)
        .send()
        .await
        .expect("steal");
    assert_eq!(
        res.status(),
        200,
        "no redirect for a coworker that is not theirs"
    );

    // Consent: the browser is sent back with a code, the state, and our issuer.
    let mut allow = form_base.clone();
    allow.push(("consent", consent.clone()));
    allow.push(("coworker", mine.as_str().to_string()));
    let res = client
        .post(format!("{base}/oauth/mcp/authorize"))
        .form(&allow)
        .send()
        .await
        .expect("consent");
    assert_eq!(
        res.status(),
        303,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let location = res.headers()["location"].to_str().unwrap().to_string();
    assert!(location.starts_with(CALLBACK), "{location}");
    let code = query_param(&location, "code").expect("code");
    assert_eq!(query_param(&location, "state").as_deref(), Some("xyz"));
    assert_eq!(
        query_param(&location, "iss").map(|s| percent_decode(&s)),
        Some(base.clone())
    );

    // A wrong verifier spends nothing but is refused; then the right one is refused too,
    // because the code was TAKEN by the first attempt — one exchange, ever.
    let token_form = |verifier: &str, code: &str| {
        vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", CALLBACK.to_string()),
            ("client_id", client_id.clone()),
            ("code_verifier", verifier.to_string()),
            ("resource", format!("{base}/mcp")),
        ]
    };
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&token_form("not-the-verifier", &code))
        .send()
        .await
        .expect("token wrong verifier");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["error"], "invalid_grant");
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&token_form(VERIFIER, &code))
        .send()
        .await
        .expect("token after take");
    assert_eq!(
        res.status(),
        400,
        "a code is one-shot even when the first exchange failed"
    );

    // A fresh consent, exchanged properly: the key.
    let res = client
        .post(format!("{base}/oauth/mcp/authorize"))
        .form(&allow)
        .send()
        .await
        .expect("consent again");
    let location = res.headers()["location"].to_str().unwrap().to_string();
    let code = query_param(&location, "code").expect("code");
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&token_form(VERIFIER, &code))
        .send()
        .await
        .expect("token");
    assert_eq!(
        res.status(),
        200,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let issued: Value = res.json().await.expect("json");
    assert_eq!(issued["token_type"], "Bearer");
    assert_eq!(issued["scope"], "mcp:tools");
    assert_eq!(issued["expires_in"], json!(24 * 60 * 60));
    let key = issued["access_token"].as_str().expect("key").to_string();
    let refresh = issued["refresh_token"]
        .as_str()
        .expect("refresh")
        .to_string();

    // The door accepts it as the chosen coworker; the key is listed and revocable like any
    // other bot key.
    let (status, init) = initialize(&base, &key).await;
    assert_eq!(status, 200, "{init}");
    let keys = store.bot_keys_for(&account, &mine).await.expect("keys");
    assert!(
        keys.iter().any(|k| k.label.contains("Claude Code")),
        "the OAuth key is a bot key on the coworker's list: {keys:?}"
    );

    // The same code cannot be exchanged twice.
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&token_form(VERIFIER, &code))
        .send()
        .await
        .expect("replay");
    assert_eq!(res.status(), 400);

    // A key minted for another server's /mcp does not open this one.
    let foreign = opengrok_server::auth::bot_keys::mint(
        &store,
        &_auth.minter,
        &account,
        &mine,
        "foreign",
        Some("https://other.example/mcp"),
        3600,
    )
    .await
    .expect("mint")
    .token;
    let (status, body) = initialize(&base, &foreign).await;
    assert_eq!(status, 401, "{body}");

    // Refresh: the pair rotates. The old refresh token is spent, the old key is dead, the new
    // pair works, and presenting the spent token again revokes the new key too — a replay means
    // somebody else has it.
    let refresh_form = |token: &str| {
        vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", token.to_string()),
            ("client_id", client_id.clone()),
            ("resource", format!("{base}/mcp")),
        ]
    };
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&refresh_form(&refresh))
        .send()
        .await
        .expect("refresh");
    assert_eq!(
        res.status(),
        200,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let rotated: Value = res.json().await.expect("json");
    let key2 = rotated["access_token"].as_str().expect("key2").to_string();
    let refresh2 = rotated["refresh_token"]
        .as_str()
        .expect("refresh2")
        .to_string();
    assert_ne!(key2, key);
    assert_ne!(refresh2, refresh);
    let (status, _) = initialize(&base, &key).await;
    assert_eq!(status, 401, "the rotated-out key is revoked");
    let (status, init) = initialize(&base, &key2).await;
    assert_eq!(status, 200, "{init}");

    // The owner revokes the rotated key from the key list: its refresh token dies with it in
    // the same transaction, so a refresh afterwards mints nothing and the key stays dead.
    let jti2 = store
        .bot_keys_for(&account, &mine)
        .await
        .expect("keys")
        .into_iter()
        .find(|k| !k.revoked && k.label.contains("Claude Code"))
        .expect("the rotated key is listed")
        .jti;
    let owner = _auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-test",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("owner access");
    let res = reqwest::Client::new()
        .delete(format!("{base}/coworkers/{}/keys/{jti2}", mine.as_str()))
        .header("Authorization", format!("Bearer {owner}"))
        .send()
        .await
        .expect("owner revoke");
    assert_eq!(res.status(), 204);
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&refresh_form(&refresh2))
        .send()
        .await
        .expect("refresh after owner revoke");
    assert_eq!(
        res.status(),
        400,
        "a revoked key's refresh token mints nothing"
    );
    let (status, _) = initialize(&base, &key2).await;
    assert_eq!(status, 401, "and the key stays dead");

    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&refresh_form(&refresh))
        .send()
        .await
        .expect("replayed refresh");
    assert_eq!(res.status(), 400, "a spent refresh token is refused");
    let (status, _) = initialize(&base, &key2).await;
    assert_eq!(status, 401, "a replayed refresh revokes the whole family");
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&refresh_form(&refresh2))
        .send()
        .await
        .expect("refresh after replay");
    assert_eq!(res.status(), 400, "the family is dead");

    // Client ID Metadata Documents: a client that is a URL, no registration. The document must
    // name itself; its redirect_uris are the registration; the consent card shows its host.
    let doc_server = spawn_cimd_document().await;
    let good = format!("{doc_server}/good/client.json");
    let bad_id = format!("{doc_server}/mismatch/client.json");
    let missing = format!("{doc_server}/nowhere/client.json");
    let authorize_cimd = |cid: &str| {
        format!(
            "{base}/oauth/mcp/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=c&resource={}",
            urlencoding(cid),
            urlencoding(CALLBACK),
            urlencoding(&format!("{base}/mcp")),
        )
    };
    let res = client
        .get(authorize_cimd(&missing))
        .send()
        .await
        .expect("missing doc");
    assert_eq!(res.status(), 400);
    let res = client
        .get(authorize_cimd(&bad_id))
        .send()
        .await
        .expect("mismatched doc");
    assert_eq!(res.status(), 400);
    let res = client
        .get(authorize_cimd(&good))
        .header(
            "cookie",
            format!("og_access={}", {
                let res = reqwest::Client::new()
                    .post(format!("{base}/auth/login"))
                    .json(&json!({ "email": email, "password": "password1" }))
                    .send()
                    .await
                    .expect("cookie login");
                cookie_value(&res, "og_access").expect("cookie")
            }),
        )
        .send()
        .await
        .expect("cimd authorize");
    assert_eq!(res.status(), 200);
    let page = res.text().await.expect("page");
    assert!(
        page.contains("<b>Doc Tool</b>") && page.contains("From <code>127.0.0.1</code>"),
        "the card names the document's host as its own element: {page}"
    );
    // A document that dresses its name up as a host: the name is cut first and the real host
    // is still its own element, so the card cannot be made to say "evil.example".
    let res = client
        .get(authorize_cimd(&format!("{doc_server}/spoof/client.json")))
        .send()
        .await
        .expect("spoof authorize");
    assert_eq!(res.status(), 200);
    let spoof = res.text().await.expect("page");
    assert!(spoof.contains("From <code>127.0.0.1</code>"), "{spoof}");
    assert!(
        !spoof.contains("evil.example"),
        "the host pushed past the cut: {spoof}"
    );
    assert!(
        !spoof.contains('\u{202e}'),
        "bidi override stripped: {spoof}"
    );
    // Documents that are too big — declared, or streamed without a length — are refused
    // with the same sentence as every other failed fetch.
    for path in ["/big/client.json", "/stream/client.json"] {
        let res = client
            .get(authorize_cimd(&format!("{doc_server}{path}")))
            .send()
            .await
            .expect("big doc");
        assert_eq!(res.status(), 400, "{path}");
        let text = res.text().await.expect("text");
        assert!(
            text.contains("could not be used") && !text.contains("KB") && !text.contains("JSON"),
            "one sentence, no oracle: {text}"
        );
    }
    let consent = consent_of(&page);
    let mut allow = vec![
        ("response_type", "code".to_string()),
        ("client_id", good.clone()),
        ("redirect_uri", CALLBACK.to_string()),
        ("code_challenge", CHALLENGE.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("state", "c".to_string()),
        ("resource", format!("{base}/mcp")),
    ];
    allow.push(("consent", consent));
    allow.push(("coworker", mine.as_str().to_string()));
    let res = client
        .post(format!("{base}/oauth/mcp/authorize"))
        .form(&allow)
        .send()
        .await
        .expect("cimd consent");
    assert_eq!(
        res.status(),
        303,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let code = query_param(res.headers()["location"].to_str().unwrap(), "code").expect("code");
    let res = client
        .post(format!("{base}/oauth/mcp/token"))
        .form(&[
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", CALLBACK.to_string()),
            ("client_id", good.clone()),
            ("code_verifier", VERIFIER.to_string()),
            ("resource", format!("{base}/mcp")),
        ])
        .send()
        .await
        .expect("cimd token");
    assert_eq!(res.status(), 200);
    let cimd_pair: Value = res.json().await.expect("json");
    let cimd_refresh = cimd_pair["refresh_token"]
        .as_str()
        .expect("refresh")
        .to_string();
    let (status, init) = initialize(&base, cimd_pair["access_token"].as_str().unwrap()).await;
    assert_eq!(
        status, 200,
        "a document client's key opens the door: {init}"
    );
    // Two presentations of one refresh token at the same instant: the claim is a single
    // statement, so exactly one mints. The loser reads a spent token — the replay case.
    let cimd_refresh_form = |token: &str| {
        vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", token.to_string()),
            ("client_id", good.clone()),
            ("resource", format!("{base}/mcp")),
        ]
    };
    let (first, second) = tokio::join!(
        client
            .post(format!("{base}/oauth/mcp/token"))
            .form(&cimd_refresh_form(&cimd_refresh))
            .send(),
        client
            .post(format!("{base}/oauth/mcp/token"))
            .form(&cimd_refresh_form(&cimd_refresh))
            .send(),
    );
    let statuses = [
        first.expect("first").status().as_u16(),
        second.expect("second").status().as_u16(),
    ];
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "exactly one of two concurrent refreshes mints: {statuses:?}"
    );
    assert!(statuses.contains(&400), "{statuses:?}");

    // A console session skips the sign-in card: the consent card comes straight up.
    let res = reqwest::Client::new()
        .post(format!("{base}/auth/login"))
        .json(&json!({ "email": email, "password": "password1" }))
        .send()
        .await
        .expect("cookie login");
    let cookie = cookie_value(&res, "og_access").expect("cookie");
    let res = client
        .get(authorize(""))
        .header("cookie", format!("og_access={cookie}"))
        .send()
        .await
        .expect("authorize with session");
    let page = res.text().await.expect("page");
    assert!(page.contains("name=consent"), "{page}");
    assert!(!page.contains("name=password"), "{page}");
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &value[i + 1..i + 3];
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A stand-in for a tool that publishes its client id metadata document. `/good/client.json`
/// names itself and the loopback callback; `/mismatch/client.json` names somebody else.
async fn spawn_cimd_document() -> String {
    use axum::routing::get;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let good = base.clone();
    let spoof = base.clone();
    let big = base.clone();
    let app = axum::Router::new()
        .route(
            "/good/client.json",
            get(move || {
                let id = format!("{good}/good/client.json");
                async move {
                    axum::Json(json!({
                        "client_id": id,
                        "client_name": "Doc Tool",
                        "redirect_uris": [CALLBACK],
                        "token_endpoint_auth_method": "none",
                    }))
                }
            }),
        )
        .route(
            "/mismatch/client.json",
            get(|| async {
                axum::Json(json!({
                    "client_id": "https://somebody.else/client.json",
                    "redirect_uris": [CALLBACK],
                }))
            }),
        )
        .route(
            "/spoof/client.json",
            get(move || {
                let id = format!("{spoof}/spoof/client.json");
                async move {
                    axum::Json(json!({
                        "client_id": id,
                        "client_name": format!("{}\u{202e} (evil.example)", "A".repeat(300)),
                        "redirect_uris": [CALLBACK],
                        "token_endpoint_auth_method": "none",
                    }))
                }
            }),
        )
        .route(
            "/big/client.json",
            get(move || {
                let id = format!("{big}/big/client.json");
                async move {
                    axum::Json(json!({
                        "client_id": id,
                        "client_name": "Big",
                        "pad": "x".repeat(6 * 1024),
                        "redirect_uris": [CALLBACK],
                    }))
                }
            }),
        )
        .route(
            "/stream/client.json",
            get(|| async {
                // Chunked, no Content-Length: only a cap applied while reading catches it.
                let chunks = (0..8).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024]));
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from_stream(futures::stream::iter(chunks)))
                    .expect("response")
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    base
}
