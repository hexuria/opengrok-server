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

/// One minted key as the stand-in keeps it: its ledger is a list of (when, cost, what the same
/// tokens would have cost at the model's list API price).
#[derive(Debug, Clone)]
struct StandInKey {
    id: String,
    prefix: String,
    key: String,
    name: String,
    principal: String,
    events: Vec<(i64, f64, f64)>,
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
    /// When true the mint endpoint answers 403, as the gateway does when the admin token it is
    /// asked with is not an admin key (the dev server's state on 2 Sep 2026).
    refuse_mints: bool,
    /// Every mint asked for, refused or not.
    mint_attempts: usize,
    /// Principals the gateway knows, by email. A mint on one it does not know is refused with
    /// the real gateway's sentence: the org must be bound first (`ensure_org_principal`).
    principals: Vec<String>,
    /// When true every completion runs on a subscription seat: cost 0, list price 0.001153
    /// (the figure the operator's Grok seat booked on 2 Sep 2026).
    seat: bool,
    /// When true the usage endpoint answers as a gateway older than open-ai-gateway #51 would:
    /// no per-window request counts, no counterfactuals, no points, no points routes.
    older_gateway: bool,
    /// The points reference price (USD per million tokens), as the admin set it; `None` until
    /// set. Points are each request's list price × 1e6 / this, rounded, summed.
    reference: Option<f64>,
    /// Every batch pool read, for the count a test asserts on.
    pool_reads: usize,
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
const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

/// A window's length in ms; `None` for the month (everything the stand-in holds).
fn window_len(window: &str) -> Option<Option<i64>> {
    match window {
        "5h" => Some(Some(FIVE_HOURS_MS)),
        "24h" => Some(Some(DAY_MS)),
        "7d" => Some(Some(SEVEN_DAYS_MS)),
        "month" => Some(None),
        _ => None,
    }
}

/// The gateway's arithmetic: a request's list price over the reference, rounded half up.
fn points_of(api: f64, reference: f64) -> i64 {
    (api * 1_000_000.0 / reference).round() as i64
}

fn windows(
    events: &[(i64, f64, f64)],
    now: i64,
    older_gateway: bool,
    reference: Option<f64>,
) -> serde_json::Value {
    let window = |len: i64| {
        let inside: Vec<&(i64, f64, f64)> = events
            .iter()
            .filter(|(at, _, _)| *at >= now - len)
            .collect();
        let used: f64 = inside.iter().map(|(_, cost, _)| cost).sum();
        let displaced: f64 = inside.iter().map(|(_, _, api)| api).sum();
        let frees = inside.iter().map(|(at, _, _)| at + len).min().map(rfc3339);
        let points = reference.map(|r| {
            inside
                .iter()
                .map(|(_, _, api)| points_of(*api, r))
                .sum::<i64>()
        });
        (money(used), frees, inside.len(), money(displaced), points)
    };
    let (five, five_frees, five_n, five_api, five_points) = window(FIVE_HOURS_MS);
    let (day, day_frees, day_n, day_api, day_points) = window(DAY_MS);
    let (seven, seven_frees, seven_n, seven_api, seven_points) = window(SEVEN_DAYS_MS);
    let month: f64 = events.iter().map(|(_, cost, _)| cost).sum();
    let month_api: f64 = events.iter().map(|(_, _, api)| api).sum();
    let month_points = reference.map(|r| {
        events
            .iter()
            .map(|(_, _, api)| points_of(*api, r))
            .sum::<i64>()
    });
    let mut body = json!({
        "five_hour_usd": five, "five_hour_frees_at": five_frees,
        "seven_day_usd": seven, "seven_day_frees_at": seven_frees,
        "month_to_date_usd": money(month), "month_resets_at": "2026-10-01T00:00:00Z",
        "spent_usd": money(month), "requests": events.len(),
    });
    if !older_gateway {
        body["five_hour_requests"] = json!(five_n);
        body["seven_day_requests"] = json!(seven_n);
        body["five_hour_counterfactual_usd"] = json!(five_api);
        body["seven_day_counterfactual_usd"] = json!(seven_api);
        body["month_counterfactual_usd"] = json!(money(month_api));
        body["day_usd"] = json!(day);
        body["day_frees_at"] = json!(day_frees);
        body["day_requests"] = json!(day_n);
        body["day_counterfactual_usd"] = json!(day_api);
        body["month_points"] = json!(month_points);
        body["five_hour_points"] = json!(five_points);
        body["day_points"] = json!(day_points);
        body["seven_day_points"] = json!(seven_points);
    }
    body
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
                    stand_in.mint_attempts += 1;
                    if stand_in.refuse_mints {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({"error": "not minted as an admin key"})),
                        );
                    }
                    let principal = body["principal_email"].as_str().unwrap_or_default().to_string();
                    if !stand_in.principals.contains(&principal) {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({"error": "no principal with that email, or no route with that name"})),
                        );
                    }
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
            "/admin/api/principals",
            post(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let email = body["email"].as_str().unwrap_or_default().to_string();
                    let mut stand_in = shared.lock().unwrap();
                    if !stand_in.principals.contains(&email) {
                        stand_in.principals.push(email.clone());
                    }
                    (StatusCode::OK, Json(json!({ "email": email })))
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
            "/admin/api/points/reference",
            get(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let stand_in = shared.lock().unwrap();
                    if stand_in.older_gateway {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})));
                    }
                    (StatusCode::OK, Json(json!({ "usd_per_mtok": stand_in.reference.map(money) })))
                },
            )
            .put(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let Some(value) = body["usd_per_mtok"].as_str().and_then(|v| v.parse::<f64>().ok()) else {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": "that is not a price, e.g. \"0.20\""})));
                    };
                    if value <= 0.0 {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": "the reference price must be positive: a point is one token at it"})));
                    }
                    shared.lock().unwrap().reference = Some(value);
                    (StatusCode::OK, Json(json!({ "usd_per_mtok": money(value) })))
                },
            ),
        )
        .route(
            "/admin/api/points/models",
            get(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let stand_in = shared.lock().unwrap();
                    let Some(r) = stand_in.reference.filter(|_| !stand_in.older_gateway) else {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "no reference price set; PUT /admin/api/points/reference first"})));
                    };
                    // One catalog model at $0.20 in / $1.20 out / $0.02 cache read per million.
                    let x = |price: f64| format!("{}", (price / r * 1e6).round() / 1e6);
                    (StatusCode::OK, Json(json!([{
                        "id": "oag/cheap", "input_x": x(0.20), "output_x": x(1.20),
                        "cache_read_x": x(0.02), "cache_write_x": Value::Null, "shown_x": x(0.20),
                    }])))
                },
            ),
        )
        .route(
            "/admin/api/keys/{id}/usage/models",
            get(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Path(id): Path<String>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let stand_in = shared.lock().unwrap();
                    if stand_in.older_gateway {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})));
                    }
                    let Some(len) = window_len(q.get("window").map(String::as_str).unwrap_or("month")) else {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": "not a window; one of 5h, 24h, 7d, month"})));
                    };
                    let Some(key) = stand_in.keys.iter().find(|k| k.id == id) else {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "no key with that id"})));
                    };
                    let now = now_ms();
                    let inside: Vec<&(i64, f64, f64)> = key.events.iter().filter(|(at, _, _)| len.is_none_or(|len| *at >= now - len)).collect();
                    if inside.is_empty() {
                        return (StatusCode::OK, Json(json!([])));
                    }
                    let cost: f64 = inside.iter().map(|(_, c, _)| c).sum();
                    let api: f64 = inside.iter().map(|(_, _, a)| a).sum();
                    let points = stand_in.reference.map(|r| inside.iter().map(|(_, _, a)| points_of(*a, r)).sum::<i64>());
                    // Every stand-in completion is ten tokens in, five out, on the one model.
                    (StatusCode::OK, Json(json!([{
                        "model_id": "oag/cheap", "requests": inside.len(),
                        "input_tokens": 10 * inside.len(), "output_tokens": 5 * inside.len(),
                        "cache_read_tokens": 0, "cache_write_tokens": 0,
                        "cost_usd": money(cost), "list_usd": money(api), "points": points,
                    }])))
                },
            ),
        )
        .route(
            "/admin/api/usage/points",
            post(
                |State(shared): State<Shared>, headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    if bearer_of(&headers) != ADMIN_TOKEN {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "admin only"})));
                    }
                    let mut stand_in = shared.lock().unwrap();
                    stand_in.pool_reads += 1;
                    if stand_in.meter_down {
                        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "the meter is down"})));
                    }
                    if stand_in.older_gateway {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})));
                    }
                    let Some(r) = stand_in.reference else {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "no reference price set; PUT /admin/api/points/reference first"})));
                    };
                    let window = body["window"].as_str().unwrap_or("month");
                    let Some(len) = window_len(window) else {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": "not a window; one of 5h, 24h, 7d, month"})));
                    };
                    let now = now_ms();
                    let mut keys = serde_json::Map::new();
                    let mut total = 0i64;
                    for id in body["keys"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                        let points = stand_in.keys.iter().find(|k| k.id == id).map_or(0, |k| {
                            k.events.iter().filter(|(at, _, _)| len.is_none_or(|len| *at >= now - len)).map(|(_, _, a)| points_of(*a, r)).sum()
                        });
                        total += points;
                        keys.insert(id.to_string(), json!(points));
                    }
                    (StatusCode::OK, Json(json!({ "window": window, "keys": keys, "total": total })))
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
                    let older_gateway = stand_in.older_gateway;
                    let reference = stand_in.reference;
                    let mut body = windows(&key.events, now_ms(), older_gateway, reference);
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
                    let seat = stand_in.seat;
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
                        // Every completion costs a dollar, on the key's ledger, now — or, on a
                        // seat, nothing, against the list price it displaced.
                        key.events.push(if seat {
                            (now_ms(), 0.0, 0.001153)
                        } else {
                            (now_ms(), 1.0, 1.0)
                        });
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
    let stand_in: Shared = Arc::new(Mutex::new(StandIn {
        reference: Some(0.20),
        ..StandIn::default()
    }));
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

    async fn get_json(&self, access: &str, path: &str) -> (u16, Value) {
        let res = self
            .client
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("get");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn put_json(&self, access: &str, path: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .put(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .json(&body)
            .send()
            .await
            .expect("put");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

#[tokio::test]
async fn a_capped_coworker_thinks_on_its_own_key_until_its_points_run_out_in_plain_words() {
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
        assert_eq!(
            stand_in.principals,
            vec![GatewayAdmin::org_principal_email(org_id.as_str())],
            "the org was bound to its principal before the mint"
        );
    }
    let (status, spend) = h.spend(&access, &coworker).await;
    assert_eq!(status, 200, "{spend}");
    assert_eq!(spend["metered"], json!(true), "{spend}");
    assert!(
        spend["limits"]["fiveHourUsd"].is_null() && spend["limits"]["monthUsd"].is_null(),
        "the USD limits are retired; the field stays empty until the modal lands: {spend}"
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

    // Every stand-in completion lists at a dollar: 5,000,000 points at the reference of $0.20.
    // The org admin gives the member (here, the admin's own account) a pool of twelve million;
    // a bad value is refused in words, and a member who is not the admin is refused outright.
    let (status, body) = h
        .put_json(
            &access,
            &format!("/admin/points/members/{account_id}"),
            json!({ "pool": -5 }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.as_str().unwrap_or("").contains("whole number"),
        "{body}"
    );
    let (status, body) = h
        .put_json(
            &access,
            &format!("/admin/points/members/{account_id}"),
            json!({ "pool": 12_000_000 }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let member_email = format!("member-{tag}@{domain}");
    let member = seed_account(&store, &member_email, "password1", Some(org_id.as_str())).await;
    let member_access = h.access_token(&member, &member_email);
    let (status, _) = h
        .put_json(
            &member_access,
            &format!("/admin/points/members/{account_id}"),
            json!({ "pool": 1 }),
        )
        .await;
    assert_eq!(status, 403, "only the admin sets pools");
    let (status, overview) = h.get_json(&access, "/admin/points").await;
    assert_eq!(status, 200, "{overview}");
    assert_eq!(
        overview["reference"]["usdPerMtok"],
        json!("0.200000"),
        "{overview}"
    );
    let me = overview["members"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["id"] == account_id.as_str()))
        .cloned()
        .expect("the admin is a member");
    assert_eq!(me["pool"], json!(12_000_000), "{me}");
    assert_eq!(
        me["usedPoints"],
        json!(5_000_000),
        "one turn at list price: {me}"
    );
    assert!(
        overview["coworkers"].as_array().is_some_and(|c| c
            .iter()
            .any(|c| c["id"] == coworker && c["usedPoints"] == 5_000_000 && c["cap"].is_null())),
        "{overview}"
    );

    // The owner caps Ada: above the pool is refused with the numbers; seven million is taken,
    // and the limit read says what she has used and what is effectively hers.
    let (status, body) = h
        .put_json(
            &access,
            &format!("/coworkers/{coworker}/limit"),
            json!({ "cap": 20_000_000 }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["error"],
        json!("a cap of 20,000,000 points is above your pool of 12,000,000"),
        "{body}"
    );
    let (status, body) = h
        .put_json(
            &access,
            &format!("/coworkers/{coworker}/limit"),
            json!({ "cap": "seven" }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, limit) = h
        .put_json(
            &access,
            &format!("/coworkers/{coworker}/limit"),
            json!({ "cap": 7_000_000, "dayCap": null }),
        )
        .await;
    assert_eq!(status, 200, "{limit}");
    assert_eq!(limit["metered"], json!(true), "{limit}");
    assert_eq!(limit["cap"], json!(7_000_000), "{limit}");
    assert_eq!(limit["effectiveCap"], json!(7_000_000), "{limit}");
    assert_eq!(limit["usedPoints"], json!(5_000_000), "{limit}");
    assert!(limit["dayCap"].is_null(), "{limit}");
    assert_eq!(limit["usedToday"], json!(5_000_000), "{limit}");
    assert_eq!(limit["pool"]["max"], json!(12_000_000), "{limit}");
    assert_eq!(limit["pool"]["used"], json!(5_000_000), "{limit}");
    assert_eq!(
        limit["pool"]["setBy"],
        json!(account_id.as_str()),
        "{limit}"
    );
    assert_eq!(
        limit["pool"]["resetsAt"],
        json!("2026-10-01T00:00:00Z"),
        "{limit}"
    );
    assert_eq!(
        limit["reference"]["usdPerMtok"],
        json!("0.200000"),
        "{limit}"
    );
    let (status, limit) = h
        .get_json(&member_access, &format!("/coworkers/{coworker}/limit"))
        .await;
    assert_eq!(status, 404, "not the owner: {limit}");

    // Five million used of seven: the next turn goes through (and reads the meter); at ten
    // million the one after is refused, in the plan's sentence — the cap, the month, what is
    // used, when it resets, and what the pool leaves for other agents.
    let reads_before = h.usage_reads();
    let (state, failure) = h.turn(&coworker, 2).await;
    assert_eq!(state, "finished", "{failure:?}");
    assert!(
        h.usage_reads() > reads_before,
        "a capped coworker reads the meter"
    );
    let (state, failure) = h.turn(&coworker, 3).await;
    assert_eq!(state, "failed", "{failure:?}");
    let failure = failure.unwrap_or_default();
    assert!(
        failure.starts_with("Ada has used its 7,000,000 points for ")
            && failure.contains(" (10,000,000 used); it resets on 1 ")
            && failure.ends_with(" 2,000,000 of your 12,000,000 remain for other agents."),
        "a sentence a person can act on: {failure}"
    );
    assert_eq!(
        h.stand_in.lock().unwrap().bearers.len(),
        2,
        "the refused turn never reached the gateway"
    );
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["windows"][0]["usedUsd"], json!("2.000000"), "{spend}");
    assert_eq!(spend["windows"][0]["requests"], json!(2), "{spend}");
    assert_eq!(
        spend["windows"][0]["counterfactualUsd"],
        json!("2.000000"),
        "a metered key's list price is its cost: {spend}"
    );
    assert_eq!(spend["seat"], json!("api"), "{spend}");

    // The usage report, per model: a window that is not one is refused in words.
    let (status, usage) = h
        .get_json(
            &access,
            &format!("/coworkers/{coworker}/usage?window=month"),
        )
        .await;
    assert_eq!(status, 200, "{usage}");
    assert_eq!(usage["metered"], json!(true), "{usage}");
    assert_eq!(usage["window"], json!("month"), "{usage}");
    assert_eq!(usage["seat"], json!("api"), "{usage}");
    assert_eq!(usage["models"][0]["modelId"], json!("oag/cheap"), "{usage}");
    assert_eq!(usage["models"][0]["requests"], json!(2), "{usage}");
    assert_eq!(usage["models"][0]["inputTokens"], json!(20), "{usage}");
    assert_eq!(usage["models"][0]["points"], json!(10_000_000), "{usage}");
    assert_eq!(usage["totals"]["points"], json!(10_000_000), "{usage}");
    assert_eq!(usage["totals"]["listUsd"], json!("2.000000"), "{usage}");
    let (status, usage) = h
        .get_json(&access, &format!("/coworkers/{coworker}/usage?window=1d"))
        .await;
    assert_eq!(status, 400, "{usage}");
    assert!(
        usage["error"]
            .as_str()
            .unwrap_or("")
            .contains("5h, 24h, 7d, month"),
        "{usage}"
    );

    // The cap cleared, the pool binds: one more turn fits (ten of twelve million), the next is
    // refused by the pool.
    let (status, limit) = h
        .put_json(
            &access,
            &format!("/coworkers/{coworker}/limit"),
            json!({ "cap": null }),
        )
        .await;
    assert_eq!(status, 200, "{limit}");
    assert!(limit["cap"].is_null(), "{limit}");
    // A ceiling on usedPoints, like the cap: the pool less what the owner's OTHER coworkers used
    // (none yet), not the room left — that is effectiveCap minus usedPoints.
    assert_eq!(
        limit["effectiveCap"],
        json!(12_000_000),
        "what the pool leaves her: {limit}"
    );
    let (state, failure) = h.turn(&coworker, 4).await;
    assert_eq!(state, "finished", "{failure:?}");
    let (state, failure) = h.turn(&coworker, 5).await;
    assert_eq!(state, "failed", "{failure:?}");
    let failure = failure.unwrap_or_default();
    assert!(
        failure.starts_with("Your pool of 12,000,000 points for ")
            && failure.contains(" is used up (15,000,000 used); it resets on 1 "),
        "{failure}"
    );

    // The daily brake: a wide pool again, sixteen million for today — one more fits, the next
    // is refused with when it frees up.
    let (status, _) = h
        .put_json(
            &access,
            &format!("/admin/points/members/{account_id}"),
            json!({ "pool": 100_000_000 }),
        )
        .await;
    assert_eq!(status, 200);
    let (status, limit) = h
        .put_json(
            &access,
            &format!("/coworkers/{coworker}/limit"),
            json!({ "dayCap": 16_000_000 }),
        )
        .await;
    assert_eq!(status, 200, "{limit}");
    assert_eq!(limit["dayCap"], json!(16_000_000), "{limit}");
    assert!(
        limit["dayFreesAt"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z')),
        "{limit}"
    );
    let (state, failure) = h.turn(&coworker, 6).await;
    assert_eq!(state, "finished", "{failure:?}");
    let (state, failure) = h.turn(&coworker, 7).await;
    assert_eq!(state, "failed", "{failure:?}");
    let failure = failure.unwrap_or_default();
    assert!(
        failure.starts_with(
            "Ada has used its 16,000,000 points for today (20,000,000 used); it frees up at "
        ) && failure.ends_with(" UTC."),
        "{failure}"
    );
    let (status, _) = h
        .put_json(
            &access,
            &format!("/coworkers/{coworker}/limit"),
            json!({ "dayCap": null }),
        )
        .await;
    assert_eq!(status, 200);

    // The meter goes down. A reading under a minute old stands in for this coworker and for
    // the owner's pool; a coworker the guard has never read is held, with the reason.
    h.stand_in.lock().unwrap().meter_down = true;
    let (state, failure) = h.turn(&coworker, 8).await;
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
    let (status, _) = h
        .get_json(&stranger_access, &format!("/coworkers/{coworker}/usage"))
        .await;
    assert_eq!(status, 404);

    // Retirement revokes the key and forgets the secret, but KEEPS the row, marked: Ada's
    // month still counts toward her owner's pool.
    let (_, before) = h.get_json(&access, "/admin/points").await;
    let owner_used_before = before["members"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["id"] == account_id.as_str()))
        .and_then(|m| m["usedPoints"].as_i64())
        .expect("used");
    let (status, deleted) = h.api("deleteAgents", json!({ "ids": [coworker] })).await;
    assert_eq!(status, 200, "{deleted}");
    assert!(
        h.stand_in.lock().unwrap().keys[0].revoked,
        "revoked on the gateway at retirement"
    );
    let row = h
        .store
        .coworker_key(&CoworkerId::from_stored(coworker.clone()))
        .await
        .expect("row")
        .expect("the row stays");
    assert!(row.revoked_at_ms.is_some(), "marked revoked, not dropped");
    let (_, after) = h.get_json(&access, "/admin/points").await;
    let owner_used_after = after["members"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["id"] == account_id.as_str()))
        .and_then(|m| m["usedPoints"].as_i64())
        .expect("used");
    assert_eq!(
        owner_used_after, owner_used_before,
        "a retired coworker's month still counts"
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
    // Not in an org: there is no admin surface to write pools on.
    let (status, _) = h
        .put_json(
            &access,
            &format!("/admin/points/members/{account}"),
            json!({ "pool": 1 }),
        )
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

/// An org admin, signed in, with the stand-in refusing every mint from the start.
async fn org_admin_with_mints_refused(database_url: &str) -> (Harness, String) {
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let domain = format!("late-{tag}.test");
    let store = store_from(database_url).await;
    let (_org_id, admin_email) = seed_org(&store, &domain, "adminpass1").await;
    let account_id = store
        .account_by_email(&admin_email)
        .await
        .expect("load")
        .expect("the admin exists")
        .id;
    let h = harness(database_url, &admin_email).await;
    let access = h.access_token(&account_id, &admin_email);
    h.stand_in.lock().unwrap().refuse_mints = true;
    (h, access)
}

async fn hire(h: &Harness, access: &str, name: &str) -> String {
    let res = h
        .client
        .post(format!("{}/coworkers", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .json(&json!({ "name": name }))
        .send()
        .await
        .expect("hire");
    assert_eq!(res.status(), 201);
    res.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string()
}

#[tokio::test]
async fn a_coworker_hired_while_the_gateway_would_not_mint_gets_its_key_on_its_next_turn() {
    let database_url = database_or_skip!();
    let (h, access) = org_admin_with_mints_refused(&database_url).await;

    // The hire goes through — never a 4xx over a cap — with no key of its own.
    let coworker = hire(&h, &access, "Ada").await;
    {
        let stand_in = h.stand_in.lock().unwrap();
        assert_eq!(stand_in.mint_attempts, 1, "the hire asked once");
        assert!(stand_in.keys.is_empty(), "and was refused");
    }
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["metered"], json!(false), "{spend}");

    // The admin key is fixed: the next turn mints the key before it thinks, and thinks on it.
    h.stand_in.lock().unwrap().refuse_mints = false;
    let (status, failure) = h.turn(&coworker, 1).await;
    assert!(status.contains("finished"), "{status} {failure:?}");
    {
        let stand_in = h.stand_in.lock().unwrap();
        assert_eq!(stand_in.keys.len(), 1, "minted late, on the turn");
        assert_eq!(stand_in.keys[0].name, "coworker: Ada");
        assert_eq!(
            stand_in.bearers.last(),
            Some(&stand_in.keys[0].key),
            "the turn went out on the coworker's own key: {:?}",
            stand_in.bearers
        );
    }
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["metered"], json!(true), "{spend}");
}

#[tokio::test]
async fn a_gateway_that_keeps_refusing_is_asked_once_an_interval_and_the_deployment_key_carries_the_turn()
 {
    let database_url = database_or_skip!();
    let (h, access) = org_admin_with_mints_refused(&database_url).await;
    let coworker = hire(&h, &access, "Bo").await;

    // Two turns while the gateway still refuses: both think on the deployment's key (no limits
    // means no hold), and the gateway is asked once more, not once per turn.
    let (status, failure) = h.turn(&coworker, 1).await;
    assert!(status.contains("finished"), "{status} {failure:?}");
    let (status, failure) = h.turn(&coworker, 2).await;
    assert!(status.contains("finished"), "{status} {failure:?}");
    let stand_in = h.stand_in.lock().unwrap();
    assert!(
        stand_in.keys.is_empty(),
        "still no key: {:?}",
        stand_in.keys
    );
    assert_eq!(
        stand_in.bearers,
        vec![DEPLOYMENT_KEY.to_string(), DEPLOYMENT_KEY.to_string()],
        "the deployment's key carried both turns"
    );
    assert_eq!(
        stand_in.mint_attempts, 2,
        "the hire asked, the first turn asked again, the second turn waited out the interval"
    );
}

#[tokio::test]
async fn a_seats_usage_shows_its_requests_and_the_bill_it_displaced() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let domain = format!("seat-{tag}.test");
    let store = store_from(&database_url).await;
    let (_org_id, admin_email) = seed_org(&store, &domain, "adminpass1").await;
    let account_id = store
        .account_by_email(&admin_email)
        .await
        .expect("load")
        .expect("the admin exists")
        .id;
    let h = harness(&database_url, &admin_email).await;
    let access = h.access_token(&account_id, &admin_email);
    let coworker = hire(&h, &access, "Ada").await;

    // Nothing run yet: metered, but the month carries neither cost nor a displaced bill, so the
    // reply does not say how it is paid for.
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["metered"], json!(true), "{spend}");
    assert!(spend["seat"].is_null(), "{spend}");
    assert_eq!(spend["windows"][2]["requests"], json!(0), "{spend}");

    // Two turns on a subscription seat: zero cost, a request count, and the list-price bill the
    // seat displaced — per window and for the month.
    h.stand_in.lock().unwrap().seat = true;
    let (status, failure) = h.turn(&coworker, 1).await;
    assert!(status.contains("finished"), "{status} {failure:?}");
    let (status, failure) = h.turn(&coworker, 2).await;
    assert!(status.contains("finished"), "{status} {failure:?}");
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["seat"], json!("subscription"), "{spend}");
    for (i, window) in ["5h", "7d", "month"].iter().enumerate() {
        assert_eq!(spend["windows"][i]["window"], json!(window), "{spend}");
        assert_eq!(spend["windows"][i]["usedUsd"], json!("0.000000"), "{spend}");
        assert_eq!(spend["windows"][i]["requests"], json!(2), "{spend}");
        assert_eq!(
            spend["windows"][i]["counterfactualUsd"],
            json!("0.002306"),
            "{spend}"
        );
    }

    // A gateway older than open-ai-gateway #51 says nothing about requests or the bill: the
    // fields are absent rather than zero, and so is the seat.
    h.stand_in.lock().unwrap().older_gateway = true;
    let (_, spend) = h.spend(&access, &coworker).await;
    assert_eq!(spend["metered"], json!(true), "{spend}");
    assert_eq!(spend["windows"][0]["usedUsd"], json!("0.000000"), "{spend}");
    assert!(spend["windows"][0]["requests"].is_null(), "{spend}");
    assert!(
        spend["windows"][0]["counterfactualUsd"].is_null(),
        "{spend}"
    );
    assert_eq!(
        spend["windows"][2]["requests"],
        json!(2),
        "the month's count always was there: {spend}"
    );
    assert!(spend["seat"].is_null(), "{spend}");
}
