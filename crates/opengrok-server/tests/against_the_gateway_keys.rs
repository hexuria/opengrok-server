//! The org-admin gateway-key surface, through the real router, against a STAND-IN gateway.
//!
//! The stand-in is a small Axum app speaking the gateway's admin contract, not a mock object: it
//! is what lets these tests assert the requests we actually send (the derived principal address,
//! the member's label, the cap) instead of asserting that our own code called our own trait.
//!
//! What is pinned here is the part that is OURS: only an org admin may mint or revoke, a member
//! cannot, another org's key id is 404 rather than 403, the secret appears exactly once, and the
//! local attribution row is what a listing reads. The gateway's own behaviour (that a minted key
//! authenticates, that revocation kills it) is pinned in the gateway's repo.
//!
//! Needs Postgres; skips loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, OrgId};
use opengrok_core::org::{Org, OrgCommand};
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

/// What the stand-in gateway was asked to do, so a test can assert the request we sent.
#[derive(Default)]
struct GatewayLog {
    principals: Vec<String>,
    mints: Vec<(String, String, Option<String>)>,
    revoked: Vec<String>,
    budgets: Vec<(String, Option<String>)>,
    quotas: Vec<(String, Option<String>)>,
    /// Every key the stand-in minted: (id, principal, name). Listed by GET /admin/api/keys.
    minted_ids: Vec<(String, String, String)>,
    /// Keys the test plants in the gateway that the console never minted (attribution lost).
    planted: Vec<(String, String, String)>,
}

type SharedLog = Arc<Mutex<GatewayLog>>;

/// A stand-in for open-ai-gateway's admin API — the same paths and shapes, nothing else.
async fn spawn_stand_in_gateway(log: SharedLog) -> String {
    let app =
        Router::new()
            .route(
                "/admin/api/principals",
                post(
                    |State(log): State<SharedLog>, Json(body): Json<Value>| async move {
                        let email = body["email"].as_str().unwrap_or_default().to_string();
                        log.lock().unwrap().principals.push(email.clone());
                        Json(json!({ "id": "01a0-principal", "email": email }))
                    },
                ),
            )
            .route(
                "/admin/api/keys",
                post(
                    |State(log): State<SharedLog>, Json(body): Json<Value>| async move {
                        let principal = body["principal_email"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        let name = body["name"].as_str().unwrap_or_default().to_string();
                        let quota = body["quota_usd"].as_str().map(str::to_string);
                        let id = format!("key-{}", uuid::Uuid::now_v7().simple());
                        let mut guard = log.lock().unwrap();
                        guard.mints.push((principal.clone(), name.clone(), quota));
                        guard.minted_ids.push((id.clone(), principal, name));
                        drop(guard);
                        Json(json!({
                            "id": id,
                            "key_prefix": "oag_live_abc1234",
                            "key": "oag_live_abc1234567890_the_only_time_this_exists",
                        }))
                    },
                )
                .get(|State(log): State<SharedLog>| async move {
                    // The gateway's listing: every key, its principal, and whether it still
                    // authenticates — the shape `GatewayAdmin::org_keys` reads.
                    let guard = log.lock().unwrap();
                    let rows: Vec<Value> = guard
                        .minted_ids
                        .iter()
                        .chain(guard.planted.iter())
                        .map(|(id, principal, name)| {
                            json!({
                                "id": id,
                                "name": name,
                                "key_prefix": "oag_live_abc1234",
                                "principal": principal,
                                "route": "openai/gpt-5.5",
                                "active": !guard.revoked.contains(id),
                                "admin": false,
                                "last_used_at": Value::Null,
                            })
                        })
                        .collect();
                    Json(Value::Array(rows))
                }),
            )
            .route(
                "/admin/api/keys/{id}/revoke",
                post(
                    |State(log): State<SharedLog>, Path(id): Path<String>| async move {
                        log.lock().unwrap().revoked.push(id.clone());
                        Json(json!({ "id": id, "active": false }))
                    },
                ),
            )
            .route(
                "/admin/api/keys/{id}/quota",
                patch(
                    |State(log): State<SharedLog>,
                     Path(id): Path<String>,
                     Json(body): Json<Value>| async move {
                        let quota = body["quotaUsd"]
                            .as_str()
                            .or_else(|| body["quota_usd"].as_str())
                            .map(str::to_string);
                        log.lock().unwrap().quotas.push((id.clone(), quota));
                        Json(json!({ "id": id }))
                    },
                ),
            )
            .route(
                "/admin/api/principals/{email}/budget",
                patch(
                    |State(log): State<SharedLog>,
                     Path(email): Path<String>,
                     Json(body): Json<Value>| async move {
                        let budget = body["monthly_budget_usd"].as_str().map(str::to_string);
                        log.lock().unwrap().budgets.push((email.clone(), budget));
                        Json(json!({ "email": email }))
                    },
                ),
            )
            .route(
                "/admin/api/principals/{email}/usage",
                get(|Path(email): Path<String>| async move {
                    Json(json!({
                        "id": "01a0-principal",
                        "email": email,
                        "monthly_budget_usd": "50.000000",
                        "month_to_date_usd": "1.250000",
                        "requests": 7,
                    }))
                }),
            )
            .with_state(log);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

async fn seed_account(store: &PgStore, email: &str, org_id: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: org_id.to_string(),
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
        org_id: Some(org_id.to_string()),
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

/// An org with an admin and one plain member.
async fn seed_org(store: &PgStore) -> (OrgId, AccountId, AccountId) {
    let org_id = OrgId::new();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let admin = seed_account(store, &format!("admin-{tag}@acme.test"), org_id.as_str()).await;
    let member = seed_account(store, &format!("member-{tag}@acme.test"), org_id.as_str()).await;
    let at_ms = now_ms();
    let events = Org::default()
        .decide(OrgCommand::Create {
            name: "Acme".to_string(),
            admin: admin.clone(),
            domains: vec!["acme.test".to_string()],
            at_ms,
        })
        .expect("create org");
    let state = Org::replay(&events);
    store
        .append_org(&org_id, 0, &events, &state, at_ms)
        .await
        .expect("append org");
    (org_id, admin, member)
}

fn app_with(store: PgStore, host_email: &str, gateway: &str) -> (Router, AgUiState) {
    // The stand-in is INJECTED, not exported into the process environment: `unsafe` is forbidden
    // workspace-wide (so `set_var` is not available in edition 2024), and a seam beats a global.
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"gateway-keys-test-secret-gateway-keys")),
        host_email.to_string(),
    )
    .with_gateway_admin(Some(opengrok_server::gateway_admin::GatewayAdmin::new(
        gateway,
        "oag_live_admin_for_tests",
    )));
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
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        host_email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    (opengrok_server::router(agui.clone(), gateway), agui)
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

/// The whole admin flow, and the shape of what we asked the gateway for.
#[tokio::test]
async fn an_admin_mints_a_member_a_key_and_the_secret_is_shown_once() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let (org_id, admin, member) = seed_org(&store).await;
    let log: SharedLog = Arc::new(Mutex::new(GatewayLog::default()));
    let gateway = spawn_stand_in_gateway(log.clone()).await;
    let (app, state) = app_with(store.clone(), "host@acme.test", &gateway);
    let base = spawn(app).await;
    let admin_token = token_for(&state, &admin, "admin@acme.test");

    // Mint, with a per-member cap.
    let (status, minted) = call(
        &base,
        reqwest::Method::POST,
        "/admin/gateway/keys",
        &admin_token,
        Some(json!({ "memberId": member.as_str(), "quotaUsd": "5.00" })),
    )
    .await;
    assert_eq!(status, 201, "{minted}");
    assert!(
        minted["key"]
            .as_str()
            .unwrap_or_default()
            .starts_with("oag_live_"),
        "the plaintext is in the mint reply: {minted}"
    );
    assert_eq!(minted["memberId"], json!(member.as_str()));

    // We asked the gateway for the DERIVED principal, and labelled the key with the member.
    {
        let log = log.lock().unwrap();
        assert_eq!(
            log.principals,
            vec![format!("org-{}@gateway.local", org_id.as_str())],
            "the org's principal address is derived from its id"
        );
        let (principal, name, quota) = log.mints.first().expect("one mint").clone();
        assert_eq!(principal, format!("org-{}@gateway.local", org_id.as_str()));
        assert!(
            name.contains("member-"),
            "the key is labelled with the member: {name}"
        );
        assert_eq!(quota.as_deref(), Some("5.00"), "the cap was passed through");
    }

    // The listing has the key — WITHOUT the secret, which exists nowhere now.
    let (status, listed) = call(
        &base,
        reqwest::Method::GET,
        "/admin/gateway/keys",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(status, 200);
    let keys = listed["keys"].as_array().expect("keys");
    assert_eq!(keys.len(), 1, "{listed}");
    assert_eq!(keys[0]["memberId"], json!(member.as_str()));
    assert!(
        keys[0].get("key").is_none(),
        "a listing must never carry the secret: {listed}"
    );
    assert!(!listed.to_string().contains("the_only_time_this_exists"));

    // Usage is read live from the gateway, not mirrored here.
    let (status, usage) = call(
        &base,
        reqwest::Method::GET,
        "/admin/gateway/usage",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(usage["monthToDateUsd"], json!("1.250000"));
    assert_eq!(usage["provisioned"], json!(true));

    // Revoke goes to the gateway AND flips our row.
    let key_id = keys[0]["id"].as_str().expect("id").to_string();
    let (status, _) = call(
        &base,
        reqwest::Method::DELETE,
        &format!("/admin/gateway/keys/{key_id}"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(log.lock().unwrap().revoked, vec![key_id.clone()]);
    let (_, listed) = call(
        &base,
        reqwest::Method::GET,
        "/admin/gateway/keys",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(
        listed["keys"][0]["revoked"],
        json!(true),
        "the local row mirrors the revocation: {listed}"
    );
}

/// Authority: a member is not an admin, and another org's key does not exist as far as they know.
#[tokio::test]
async fn a_member_cannot_mint_and_another_orgs_key_is_not_found() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let (_org_a, admin_a, member_a) = seed_org(&store).await;
    let (_org_b, admin_b, _member_b) = seed_org(&store).await;
    let log: SharedLog = Arc::new(Mutex::new(GatewayLog::default()));
    let gateway = spawn_stand_in_gateway(log.clone()).await;
    let (app, state) = app_with(store.clone(), "host@acme.test", &gateway);
    let base = spawn(app).await;

    // A plain member of org A may not mint, set a budget, or read the org's spend.
    let member_token = token_for(&state, &member_a, "member@acme.test");
    for (method, path, body) in [
        (
            reqwest::Method::POST,
            "/admin/gateway/keys",
            Some(json!({ "memberId": member_a.as_str() })),
        ),
        (
            reqwest::Method::PUT,
            "/admin/gateway/budget",
            Some(json!({ "monthlyBudgetUsd": "999.00" })),
        ),
        (reqwest::Method::GET, "/admin/gateway/usage", None),
        (reqwest::Method::GET, "/admin/gateway/keys", None),
    ] {
        let (status, body) = call(&base, method, path, &member_token, body).await;
        assert_eq!(status, 403, "a member must be refused {path}: {body}");
    }
    assert!(
        log.lock().unwrap().mints.is_empty(),
        "a refused request must never reach the gateway"
    );

    // Org A's admin mints a key...
    let admin_a_token = token_for(&state, &admin_a, "admin-a@acme.test");
    let (status, minted) = call(
        &base,
        reqwest::Method::POST,
        "/admin/gateway/keys",
        &admin_a_token,
        Some(json!({ "memberId": member_a.as_str() })),
    )
    .await;
    assert_eq!(status, 201, "{minted}");
    let key_id = minted["id"].as_str().expect("id").to_string();

    // ...and org B's admin cannot see it, revoke it, or cap it: 404, never 403. A 403 would
    // confirm the key exists, which is exactly what probing is for.
    let admin_b_token = token_for(&state, &admin_b, "admin-b@acme.test");
    let (status, listed) = call(
        &base,
        reqwest::Method::GET,
        "/admin/gateway/keys",
        &admin_b_token,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        listed["keys"].as_array().expect("keys").is_empty(),
        "another org's keys are not listed: {listed}"
    );
    let (status, _) = call(
        &base,
        reqwest::Method::DELETE,
        &format!("/admin/gateway/keys/{key_id}"),
        &admin_b_token,
        None,
    )
    .await;
    assert_eq!(
        status, 404,
        "another org's key id is not found, not forbidden"
    );
    let (status, _) = call(
        &base,
        reqwest::Method::PUT,
        &format!("/admin/gateway/keys/{key_id}/quota"),
        &admin_b_token,
        Some(json!({ "quotaUsd": "1.00" })),
    )
    .await;
    assert_eq!(status, 404);
    assert!(
        log.lock().unwrap().revoked.is_empty(),
        "no cross-org request reached the gateway"
    );

    // And minting for somebody who is not in the org is equally not-found.
    let (status, _) = call(
        &base,
        reqwest::Method::POST,
        "/admin/gateway/keys",
        &admin_b_token,
        Some(json!({ "memberId": member_a.as_str() })),
    )
    .await;
    assert_eq!(status, 404, "an admin cannot mint for another org's member");
}

/// 17.later: a press whose reply was lost must not mint a second real key, and the listing must
/// tell the truth the gateway holds — a revoke that missed our mirror reads revoked, and a key the
/// gateway has for this org that we never recorded is shown, not hidden.
#[tokio::test]
async fn a_repeated_press_finds_its_key_and_the_listing_heals_against_the_gateway() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let (org_id, admin, member) = seed_org(&store).await;
    let log: SharedLog = Arc::new(Mutex::new(GatewayLog::default()));
    let gateway = spawn_stand_in_gateway(log.clone()).await;
    let (app, state) = app_with(store.clone(), "host@acme.test", &gateway);
    let base = spawn(app).await;
    let admin_token = token_for(&state, &admin, "admin@acme.test");
    let mint = |nonce: &'static str| {
        let base = base.clone();
        let admin_token = admin_token.clone();
        let member = member.clone();
        async move {
            call(
                &base,
                reqwest::Method::POST,
                "/admin/gateway/keys",
                &admin_token,
                Some(json!({ "memberId": member.as_str(), "clientNonce": nonce })),
            )
            .await
        }
    };

    // First press: a real key, shown once.
    let (status, first) = mint("press-1").await;
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["alreadyMinted"], json!(false));
    let first_id = first["id"].as_str().expect("id").to_string();
    assert!(first["key"].as_str().is_some_and(|k| !k.is_empty()));

    // The same press again (the reply was lost): the same key, no secret, no second mint.
    let (status, again) = mint("press-1").await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["alreadyMinted"], json!(true));
    assert_eq!(again["id"], first_id);
    assert_eq!(again["key"], Value::Null);
    assert_eq!(log.lock().unwrap().mints.len(), 1, "one press, one mint");

    // A different press mints a different key.
    let (status, second) = mint("press-2").await;
    assert_eq!(status, 201, "{second}");
    let second_id = second["id"].as_str().expect("id").to_string();
    assert_ne!(second_id, first_id);
    assert_eq!(log.lock().unwrap().mints.len(), 2);

    // The gateway revokes the first key behind our back (a mirror that failed, an operator in
    // the gateway's own console): the listing reads revoked, and heals the row.
    log.lock().unwrap().revoked.push(first_id.clone());
    let (status, listed) = call(
        &base,
        reqwest::Method::GET,
        "/admin/gateway/keys",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["reconciled"], json!(true));
    let keys = listed["keys"].as_array().expect("keys");
    let first_row = keys
        .iter()
        .find(|k| k["id"] == first_id)
        .expect("first key listed");
    assert_eq!(
        first_row["revoked"],
        json!(true),
        "healed against the gateway: {first_row}"
    );
    let healed = store
        .gateway_key_in_org(&first_id, org_id.as_str())
        .await
        .expect("row")
        .expect("row exists");
    assert!(
        healed.revoked,
        "the row itself was corrected, not only the reply"
    );
    let second_row = keys
        .iter()
        .find(|k| k["id"] == second_id)
        .expect("second key listed");
    assert_eq!(second_row["revoked"], json!(false));
    assert_eq!(second_row["unattributed"], json!(false));

    // A key the gateway holds for THIS org that we never recorded is shown, unattributed;
    // another org's key is not.
    let ours = format!("org-{}@gateway.local", org_id.as_str());
    {
        let mut guard = log.lock().unwrap();
        guard.planted.push((
            "key-lost-attribution".to_string(),
            ours,
            "lost@acme.test".to_string(),
        ));
        guard.planted.push((
            "key-somebody-elses".to_string(),
            "org-other@gateway.local".to_string(),
            "them".to_string(),
        ));
    }
    let (_, listed) = call(
        &base,
        reqwest::Method::GET,
        "/admin/gateway/keys",
        &admin_token,
        None,
    )
    .await;
    let keys = listed["keys"].as_array().expect("keys");
    let lost = keys
        .iter()
        .find(|k| k["id"] == "key-lost-attribution")
        .expect("the unattributed key is shown");
    assert_eq!(lost["unattributed"], json!(true));
    assert_eq!(lost["memberId"], Value::Null);
    assert_eq!(lost["label"], "lost@acme.test");
    assert!(
        keys.iter().all(|k| k["id"] != "key-somebody-elses"),
        "another org's key never leaves the filter: {listed}"
    );
}
