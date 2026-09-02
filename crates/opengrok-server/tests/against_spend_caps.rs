//! Per-coworker spend limits (`docs/plan-spend-policy.md`), over a STAND-IN gateway that does
//! what the real one does at the seams we depend on: mints a key on a principal, meters a chat
//! completion against it, reports a key's usage over the three windows with when each frees
//! up, and revokes. The server under test wraps the REAL `GatewayDoor` in the spend guard, so
//! what is asserted is what the gateway actually receives — which bearer opened the door for a
//! coworker's turn, and whether the meter was read at all — and what a person actually reads
//! when a window stops one.
//!
//! Needs Postgres; skips loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId, OrgId};
use opengrok_core::org::{Org, OrgCommand};
use opengrok_harness::GatewayDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_server::gateway_admin::GatewayAdmin;
use opengrok_server::spend::GuardedDoor;
use opengrok_store::{PgStore, Vault};
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

const DEPLOYMENT_KEY: &str = "oag_live_deployment_key";
const ADMIN_TOKEN: &str = "admin-token";
/// 32 zero bytes, base64: a test vault. The key never leaves this process.
const KEK: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// One minted key as the stand-in keeps it: its ledger is a list of (when, cost).
#[derive(Debug, Clone)]
struct StandInKey {
    id: String,
    prefix: String,
    key: String,
    name: String,
    principal: String,
    events: Vec<(i64, f64)>,
    revoked: bool,
}

#[derive(Debug, Default)]
struct StandIn {
    keys: Vec<StandInKey>,
    /// Every bearer a chat completion arrived with, in order.
    bearers: Vec<String>,
    /// How many times the usage endpoint was read — a coworker without limits must never.
    usage_reads: usize,
    /// When true the usage endpoint answers 500: the meter is down.
    meter_down: bool,
}

const FIVE_HOURS_MS: i64 = 5 * 60 * 60 * 1_000;
const SEVEN_DAYS_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .expect("a timestamp")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The three windows over a ledger, the way the gateway computes them: the sum since an
/// instant, and when the window next frees up (its oldest spend ageing out).
fn windows(events: &[(i64, f64)], now: i64) -> serde_json::Value {
    let window = |len: i64| {
        let inside: Vec<&(i64, f64)> = events.iter().filter(|(at, _)| *at >= now - len).collect();
        let used: f64 = inside.iter().map(|(_, cost)| cost).sum();
        let frees = inside.iter().map(|(at, _)| at + len).min().map(rfc3339);
        (money(used), frees)
    };
    let (five, five_frees) = window(FIVE_HOURS_MS);
    let (seven, seven_frees) = window(SEVEN_DAYS_MS);
    let month: f64 = events.iter().map(|(_, cost)| cost).sum();
    json!({
        "five_hour_usd": five, "five_hour_frees_at": five_frees,
        "seven_day_usd": seven, "seven_day_frees_at": seven_frees,
        "month_to_date_usd": money(month), "month_resets_at": "2026-10-01T00:00:00Z",
        "spent_usd": money(month), "requests": events.len(),
    })
}

type Shared = Arc<Mutex<StandIn>>;

fn bearer_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string()
}

fn money(value: f64) -> String {
    // An empty sum is -0.0 to IEEE and to Rust; the gateway prints money, not signs.
    format!("{:.6}", value + 0.0)
}

/// The gateway's admin and inference surfaces, as far as spend caps touch them.
async fn spawn_stand_in(shared: Shared) -> String {
    let admin_only = |headers: &axum::http::HeaderMap| bearer_of(headers) == ADMIN_TOKEN;
    let app = Router::new()
        .route(
            "/admin/api/keys",
            post(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let mut stand_in = shared.lock().unwrap();
                    let n = stand_in.keys.len() + 1;
                    let key = StandInKey {
                        id: uuid::Uuid::now_v7().to_string(),
                        prefix: format!("oag_live_stand{n:03}"),
                        key: format!("oag_live_stand{n:03}_secret_{n}"),
                        name: body["name"].as_str().unwrap_or_default().to_string(),
                        principal: body["principal_email"].as_str().unwrap_or_default().to_string(),
                        events: Vec::new(),
                        revoked: false,
                    };
                    let reply = json!({ "id": key.id, "key_prefix": key.prefix, "key": key.key });
                    stand_in.keys.push(key);
                    (StatusCode::CREATED, Json(reply))
                },
            ),
        )
        .route(
            "/admin/api/keys/{id}/quota",
            axum::routing::patch(
                |headers: axum::http::HeaderMap, Path(id): Path<String>, Json(body): Json<Value>| async move {
                    // The server never writes a quota any more (one enforcer per rule); a
                    // stand-in that still accepts it would hide a regression, so it refuses.
                    let _ = (bearer_of(&headers), id, body);
                    (StatusCode::NOT_IMPLEMENTED, Json(json!({"error": "quota is not written by the server"})))
                },
            ),
        )
        .route(
            "/admin/api/keys/{id}/usage",
            get(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Path(id): Path<String>| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let mut stand_in = shared.lock().unwrap();
                    stand_in.usage_reads += 1;
                    if stand_in.meter_down {
                        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "the meter is down"})));
                    }
                    let Some(key) = stand_in.keys.iter().find(|k| k.id == id) else {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "no key with that id"})));
                    };
                    let mut body = windows(&key.events, now_ms());
                    body["id"] = json!(key.id);
                    body["name"] = json!(key.name);
                    body["key_prefix"] = json!(key.prefix);
                    body["principal"] = json!(key.principal);
                    body["active"] = json!(!key.revoked);
                    body["quota_usd"] = Value::Null;
                    (StatusCode::OK, Json(body))
                },
            ),
        )
        .route(
            "/admin/api/keys/{id}/revoke",
            post(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Path(id): Path<String>| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let mut stand_in = shared.lock().unwrap();
                    let Some(key) = stand_in.keys.iter_mut().find(|k| k.id == id) else {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "no key with that id"})));
                    };
                    key.revoked = true;
                    (StatusCode::OK, Json(json!({"id": id, "revoked": true})))
                },
            ),
        )
        .route(
            "/v1/models",
            get(|| async { Json(json!({ "object": "list", "data": [{"id": "oag/cheap"}] })) }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Json(_body): Json<Value>| async move {
                    let bearer = bearer_of(&headers);
                    let mut stand_in = shared.lock().unwrap();
                    stand_in.bearers.push(bearer.clone());
                    if bearer != DEPLOYMENT_KEY {
                        let Some(key) = stand_in.keys.iter_mut().find(|k| k.key == bearer && !k.revoked) else {
                            return axum::response::Response::builder()
                                .status(StatusCode::UNAUTHORIZED)
                                .header("content-type", "application/json")
                                .body(axum::body::Body::from(
                                    json!({"error": {"type": "authentication_error", "message": "unknown key"}}).to_string(),
                                ))
                                .unwrap();
                        };
                        // Every completion costs a dollar, on the key's ledger, now.
                        key.events.push((now_ms(), 1.0));
                    }
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                        ))
                        .unwrap()
                },
            ),
        )
        .with_state(shared);
    let _ = admin_only;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
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

struct Harness {
    base: String,
    agui: AgUiState,
    store: PgStore,
    stand_in: Shared,
    client: reqwest::Client,
}

/// A server whose model door is the REAL `GatewayDoor` pointed at the stand-in, whose admin
/// connection is the stand-in, and whose vault is a test key — the three things a cap needs.
async fn harness(database_url: &str, host_email: &str) -> Harness {
    let store = store_from(database_url).await;
    let stand_in: Shared = Arc::new(Mutex::new(StandIn::default()));
    let gateway = spawn_stand_in(stand_in.clone()).await;
    let mut auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"spend-caps-test-secret")),
        host_email.to_string(),
    );
    auth.gateway_admin = Some(GatewayAdmin::new(&gateway, ADMIN_TOKEN));
    let agui = AgUiState {
        auth,
        // The real door under the real guard, reading the meter on every call so the tests can
        // count reads and see a limit bite on the very next turn.
        door: Arc::new(
            GuardedDoor::new(
                Arc::new(GatewayDoor::new(&gateway, DEPLOYMENT_KEY)),
                store.clone(),
                Some(GatewayAdmin::new(&gateway, ADMIN_TOKEN)),
            )
            .with_fresh_ms(0),
        ),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: Some(Arc::new(Vault::from_base64_key(KEK).expect("vault"))),
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
    let app = opengrok_server::router(agui.clone(), gateway_state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        agui,
        store,
        stand_in,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    fn access_token(&self, account: &AccountId, email: &str) -> String {
        self.agui
            .auth
            .minter
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

    /// `POST /api/{method}` with the gateway bearer — how the desktop's coordinator calls.
    async fn api(&self, method: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/api/{method}", self.base))
            .header("authorization", "Bearer test-bearer")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("api call");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn spend(&self, access: &str, coworker: &str) -> (u16, Value) {
        let res = self
            .client
            .get(format!("{}/coworkers/{coworker}/spend", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("spend");
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    /// Wait until the coworker's gateway thread has `expected` runs and the newest has settled;
    /// hand back that run's status and failure. Counting is what keeps a fast poll from
    /// answering with the previous turn's run.
    async fn settled_run(&self, coworker: &str, expected: usize) -> (String, Option<String>) {
        let thread = format!("gateway-{coworker}");
        for _ in 0..100 {
            let runs = self.store.runs_for_thread(&thread, 50).await.expect("runs");
            if runs.len() >= expected
                && let Some(newest) = runs.iter().max_by_key(|run| run.started_at_ms)
            {
                let (loaded, _) = self.store.load_run(&newest.id).await.expect("load run");
                let status = format!("{:?}", loaded.status).to_lowercase();
                if status.contains("finished") || status.contains("failed") {
                    return (status, loaded.failure.clone());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the run did not settle in 10s");
    }
}

impl Harness {
    async fn put_limit(&self, access: &str, path: &str, body: Value) -> (u16, String) {
        let res = self
            .client
            .put(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .json(&body)
            .send()
            .await
            .expect("put limit");
        let status = res.status().as_u16();
        (status, res.text().await.unwrap_or_default())
    }

    async fn turn(&self, coworker: &str, n: usize) -> (String, Option<String>) {
        let (status, sent) = self
            .api(
                "sendPrompt",
                // A nonce dedupes a press per payload; two coworkers' first turns must not share one.
                json!({ "agentId": coworker, "prompt": format!("turn {n}"), "clientNonce": format!("n{n}-{coworker}") }),
            )
            .await;
        assert_eq!(status, 200, "{sent}");
        self.settled_run(coworker, n).await
    }

    fn usage_reads(&self) -> usize {
        self.stand_in.lock().unwrap().usage_reads
    }
}

#[tokio::test]
async fn a_limited_coworker_thinks_on_its_own_key_until_a_window_stops_it_in_plain_words() {
    let database_url = database_or_skip!();
    let tag = now_ms().to_string();
    let domain = format!("caps-{tag}.test");
    let store = store_from(&database_url).await;
    let (org_id, admin_email) = seed_org(&store, &domain, "adminpass1").await;
    let account_id = store
        .account_by_email(&admin_email)
        .await
        .expect("load")
        .expect("the admin exists")
        .id;
    let h = harness(&database_url, &admin_email).await;
    let access = h.access_token(&account_id, &admin_email);

    // Hire: the coworker gets a key of its own on the org's principal, named after it.
    let res = h
        .client
        .post(format!("{}/coworkers", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .json(&json!({ "name": "Ada" }))
        .send()
        .await
        .expect("hire");
    assert_eq!(
        res.status(),
        201,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let hired: Value = res.json().await.expect("json");
    let coworker = hired["id"].as_str().expect("id").to_string();
    {
        let stand_in = h.stand_in.lock().unwrap();
        assert_eq!(stand_in.keys.len(), 1, "one key minted at hire");
        assert_eq!(stand_in.keys[0].name, "coworker: Ada");
        assert_eq!(
            stand_in.keys[0].principal,
            GatewayAdmin::org_principal_email(org_id.as_str()),
            "minted on the org's principal"
        );
    }
    let (status, spend) = h.spend(&access, &coworker).await;
    assert_eq!(status, 200, "{spend}");
    assert_eq!(spend["metered"], json!(true), "{spend}");
    assert!(
        spend["limits"]["fiveHourUsd"].is_null()
            && spend["limits"]["sevenDayUsd"].is_null()
            && spend["limits"]["monthUsd"].is_null(),
        "nothing set anywhere: {spend}"
    );
    assert_eq!(spend["windows"][0]["window"], json!("5h"), "{spend}");
    assert_eq!(spend["windows"][0]["usedUsd"], json!("0.000000"), "{spend}");
    let prefix = spend["keyPrefix"].as_str().expect("prefix").to_string();
    let listed = store
        .gateway_keys_for_org(org_id.as_str())
        .await
        .expect("keys");
    assert!(
        listed
            .iter()
            .any(|k| k.key_prefix == prefix && k.label == "coworker: Ada"),
        "the org's key listing attributes it: {listed:?}"
    );

    // No limits at any layer: a turn goes out on the coworker's own key and NEVER reads the
    // meter — with unlimited as the shipped default, a meter blip must not be everyone's outage.
    let reads_before = h.usage_reads();
    let (state, failure) = h.turn(&coworker, 1).await;
    assert_eq!(state, "finished", "{failure:?}");
    assert_eq!(
        h.usage_reads(),
        reads_before,
        "an unlimited coworker does not touch the meter"
    );
    {
        let stand_in = h.stand_in.lock().unwrap();
        assert_eq!(stand_in.bearers, vec![stand_in.keys[0].key.clone()]);
    }

    // The admin writes the org default: two dollars per five hours, fifty a month. A bad amount
    // is refused in words; a member who is not the admin is refused outright.
    let (status, text) = h
        .put_limit(
            &access,
            "/admin/spend/org",
            json!({ "fiveHourUsd": "lots" }),
        )
        .await;
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("is not an amount"), "{text}");
    let (status, text) = h
        .put_limit(
            &access,
            "/admin/spend/org",
            json!({ "fiveHourUsd": "2.00", "monthUsd": "50.00" }),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let member_email = format!("member-{tag}@{domain}");
    let member = seed_account(&store, &member_email, "password1", Some(org_id.as_str())).await;
    let member_access = h.access_token(&member, &member_email);
    let (status, _) = h
        .put_limit(
            &member_access,
            "/admin/spend/org",
            json!({ "fiveHourUsd": "999" }),
        )
        .await;
    assert_eq!(status, 403, "only the admin writes limits");
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["limits"]["fiveHourUsd"], json!("2.00"), "{spend}");
    assert_eq!(spend["windows"][0]["limitUsd"], json!("2.00"), "{spend}");
    assert_eq!(
        spend["windows"][1]["limitUsd"],
        Value::Null,
        "no 7-day limit: {spend}"
    );

    // One more dollar fits ($1 used of $2); the next one does not, and the sentence names the
    // window, the amounts, when it frees up, and which window still has room.
    let reads_before = h.usage_reads();
    let (state, failure) = h.turn(&coworker, 2).await;
    assert_eq!(state, "finished", "{failure:?}");
    assert!(
        h.usage_reads() > reads_before,
        "a limited coworker reads the meter"
    );
    let (state, failure) = h.turn(&coworker, 3).await;
    assert_eq!(state, "failed", "{failure:?}");
    let failure = failure.unwrap_or_default();
    assert!(
        failure.starts_with(
            "Ada has used its 5-hour allowance (2.00 of 2.00); it begins to free up at "
        ) && failure.contains(" UTC.")
            && failure.ends_with("The monthly allowance still has room."),
        "a sentence a person can act on: {failure}"
    );
    assert_eq!(
        h.stand_in.lock().unwrap().bearers.len(),
        2,
        "the refused turn never reached the gateway"
    );
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["windows"][0]["usedUsd"], json!("2.000000"), "{spend}");
    assert!(
        spend["windows"][0]["freesAt"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z')),
        "the meter says when the window frees up: {spend}"
    );

    // The coworker's own row wins over the org default: ten dollars per five hours, and the
    // next turn goes through.
    let (status, text) = h
        .put_limit(
            &access,
            &format!("/admin/spend/coworkers/{coworker}"),
            json!({ "fiveHourUsd": "10.00" }),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let (state, failure) = h.turn(&coworker, 4).await;
    assert_eq!(state, "finished", "{failure:?}");
    let listing: Value = h
        .client
        .get(format!("{}/admin/spend", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .send()
        .await
        .expect("listing")
        .json()
        .await
        .expect("json");
    assert_eq!(listing["org"]["fiveHourUsd"], json!("2.00"), "{listing}");
    assert!(
        listing["coworkers"].as_array().is_some_and(|c| c
            .iter()
            .any(|c| c["id"] == coworker && c["limits"]["fiveHourUsd"] == "10.00")),
        "{listing}"
    );

    // The meter goes down. A reading under a minute old stands in for this coworker; a coworker
    // the guard has never read is held, with the reason.
    h.stand_in.lock().unwrap().meter_down = true;
    let (state, failure) = h.turn(&coworker, 5).await;
    assert_eq!(state, "finished", "a fresh reading stands in: {failure:?}");
    let res = h
        .client
        .post(format!("{}/coworkers", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .json(&json!({ "name": "Bob" }))
        .send()
        .await
        .expect("hire");
    assert_eq!(res.status(), 201);
    let bob = res.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string();
    let (state, failure) = h.turn(&bob, 1).await;
    assert_eq!(state, "failed");
    assert!(
        failure
            .unwrap_or_default()
            .contains("spend meter could not be read"),
        "held, and says why"
    );
    h.stand_in.lock().unwrap().meter_down = false;

    // Another account cannot read it.
    let stranger_email = format!("stranger-{tag}@elsewhere.test");
    let stranger = seed_account(&store, &stranger_email, "password1", None).await;
    let stranger_access = h.access_token(&stranger, &stranger_email);
    let (status, _) = h.spend(&stranger_access, &coworker).await;
    assert_eq!(status, 404);

    // Retirement revokes the key and forgets the secret.
    let (status, deleted) = h.api("deleteAgents", json!({ "ids": [coworker] })).await;
    assert_eq!(status, 200, "{deleted}");
    assert!(
        h.stand_in.lock().unwrap().keys[0].revoked,
        "revoked on the gateway at retirement"
    );
    assert!(
        h.store
            .coworker_key(&CoworkerId::from_stored(coworker.clone()))
            .await
            .expect("row")
            .is_none(),
        "the row is gone"
    );
    let vault = h.agui.vault.clone().expect("vault");
    assert!(
        h.store
            .open_credential(&vault, &format!("coworker-gateway-key:{coworker}"))
            .await
            .expect("secret")
            .is_none(),
        "the sealed key is gone"
    );
}

#[tokio::test]
async fn a_hirer_outside_any_org_hires_on_the_deployment_key_and_the_console_says_so() {
    let database_url = database_or_skip!();
    let tag = now_ms().to_string();
    let store = store_from(&database_url).await;
    let email = format!("solo-{tag}@og.local");
    let account = seed_account(&store, &email, "password1", None).await;
    let h = harness(&database_url, &email).await;
    let access = h.access_token(&account, &email);
    let res = h
        .client
        .post(format!("{}/coworkers", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .json(&json!({ "name": "Solo" }))
        .send()
        .await
        .expect("hire");
    assert_eq!(res.status(), 201);
    let coworker = res.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string();
    assert!(
        h.stand_in.lock().unwrap().keys.is_empty(),
        "nothing minted without an org"
    );
    let (status, spend) = h.spend(&access, &coworker).await;
    assert_eq!(status, 200, "{spend}");
    assert_eq!(spend["metered"], json!(false), "{spend}");
    assert!(
        spend["note"].as_str().unwrap_or("").contains("org"),
        "{spend}"
    );
    // Not in an org: there is no admin surface to write limits on.
    let (status, _) = h
        .put_limit(&access, "/admin/spend/org", json!({ "monthUsd": "1.00" }))
        .await;
    assert_eq!(status, 403);
    // Its turn goes out on the deployment's key, and never reads the meter.
    let (state, failure) = h.turn(&coworker, 1).await;
    assert_eq!(state, "finished", "{failure:?}");
    assert_eq!(
        h.stand_in.lock().unwrap().bearers,
        vec![DEPLOYMENT_KEY.to_string()]
    );
    assert_eq!(h.usage_reads(), 0);
}
