//! Coworker templates (`docs/plan-spend-policy.md` §4): a type the admin writes — route,
//! tool ceiling, what needs a human yes, spend limits — that members hire from, applied by
//! COPY at hire. Over the same stand-in gateway as the spend tests, so a coworker hired from a
//! template with limits is really metered. Needs Postgres; skips loudly without.

// The harness is the spend suite's, by copy; not every helper is used here.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used, dead_code)]

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
async fn a_member_hires_from_the_admins_template_and_gets_what_it_says() {
    let database_url = database_or_skip!();
    let tag = now_ms().to_string();
    let domain = format!("tpl-{tag}.test");
    let store = store_from(&database_url).await;
    let (org_id, admin_email) = seed_org(&store, &domain, "adminpass1").await;
    let admin_id = store
        .account_by_email(&admin_email)
        .await
        .expect("load")
        .expect("the admin exists")
        .id;
    let member_email = format!("member-{tag}@{domain}");
    let member_id = seed_account(&store, &member_email, "password1", Some(org_id.as_str())).await;
    let h = harness(&database_url, &admin_email).await;
    let admin = h.access_token(&admin_id, &admin_email);
    let member = h.access_token(&member_id, &member_email);
    let post = |access: &str, path: &str, body: Value| {
        let access = access.to_string();
        let path = path.to_string();
        let client = h.client.clone();
        let base = h.base.clone();
        async move {
            let res = client
                .post(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {access}"))
                .json(&body)
                .send()
                .await
                .expect("post");
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            (
                status,
                serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)),
            )
        }
    };

    // Only the admin writes templates; a template names only tools this server implements, and
    // asks approval only inside its own ceiling; a bad amount is refused in words.
    let researcher = json!({
        "name": "Researcher",
        "description": "Reads and reports; never writes.",
        "model": "oag/cheap",
        "tools": ["shell", "read_file"],
        "needsApproval": ["shell"],
        "limits": { "monthUsd": "5.00" },
    });
    let (status, _) = post(&member, "/admin/templates", researcher.clone()).await;
    assert_eq!(status, 403, "a member does not write templates");
    let (status, body) = post(
        &admin,
        "/admin/templates",
        json!({ "name": "Bad", "tools": ["shell", "fly"] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.as_str().unwrap_or("").contains("'fly' is not a tool"),
        "{body}"
    );
    let (status, body) = post(
        &admin,
        "/admin/templates",
        json!({ "name": "Bad", "tools": ["read_file"], "needsApproval": ["shell"] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.as_str()
            .unwrap_or("")
            .contains("not in the template's tools"),
        "{body}"
    );
    let (status, body) = post(
        &admin,
        "/admin/templates",
        json!({ "name": "Bad", "limits": { "monthUsd": "lots" } }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, created) = post(&admin, "/admin/templates", researcher).await;
    assert_eq!(status, 201, "{created}");
    let template_id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["tools"], json!(["read_file", "shell"]), "{created}");
    assert_eq!(created["needsApproval"], json!(["shell"]), "{created}");

    // The member sees it in the hire picker; somebody outside the org does not.
    let (status, listed) = {
        let res = h
            .client
            .get(format!("{}/templates", h.base))
            .header("Authorization", format!("Bearer {member}"))
            .send()
            .await
            .expect("list");
        (
            res.status().as_u16(),
            res.json::<Value>().await.expect("json"),
        )
    };
    assert_eq!(status, 200);
    assert_eq!(listed["templates"][0]["id"], json!(template_id), "{listed}");
    let stranger_email = format!("stranger-{tag}@elsewhere.test");
    let stranger = seed_account(&store, &stranger_email, "password1", None).await;
    let stranger_access = h.access_token(&stranger, &stranger_email);
    let (status, hired) = post(
        &stranger_access,
        "/coworkers",
        json!({ "name": "Nosy", "templateId": template_id }),
    )
    .await;
    assert_eq!(
        status, 404,
        "another org's template reads as no such template: {hired}"
    );

    // The member hires from it: the template's route, its ceiling and approval set as the
    // coworker's grant, its limits as the coworker's own row, its description as the profile,
    // and the coworker remembers where it came from.
    let (status, hired) = post(
        &member,
        "/coworkers",
        json!({ "name": "Ada", "templateId": template_id }),
    )
    .await;
    assert_eq!(status, 201, "{hired}");
    assert_eq!(
        hired["model"],
        json!("oag/cheap"),
        "the template's pin: {hired}"
    );
    let coworker = CoworkerId::from_stored(hired["id"].as_str().expect("id").to_string());
    let policy = store
        .policy_for(&member_id, &coworker)
        .await
        .expect("policy");
    let grant = policy.grant.as_ref().expect("a grant");
    assert_eq!(
        grant.profile,
        opengrok_policy::ToolSet::only(vec!["read_file".to_string(), "shell".to_string()]),
        "the template's ceiling is the coworker's profile"
    );
    assert_eq!(
        grant.needs_approval,
        opengrok_policy::ToolSet::only(vec!["shell".to_string()]),
        "and shell asks first"
    );
    let limits = store
        .spend_limit(opengrok_store::SpendScope::Coworker, coworker.as_str())
        .await
        .expect("limits")
        .expect("copied");
    assert_eq!(limits.month_usd.as_deref(), Some("5.00"));
    assert_eq!(
        store.template_of(&coworker).await.expect("use").as_deref(),
        Some(template_id.as_str())
    );
    let profile = store
        .seamb_profile(&coworker)
        .await
        .expect("profile")
        .expect("written");
    assert_eq!(
        profile["description"],
        json!("Reads and reports; never writes.")
    );
    // The request's own pin beats the template's.
    let (status, hired) = post(
        &member,
        "/coworkers",
        json!({ "name": "Bob", "templateId": template_id, "model": "oag/other" }),
    )
    .await;
    assert_eq!(status, 201, "{hired}");
    assert_eq!(hired["model"], json!("oag/other"), "{hired}");

    // The desktop's createAgent passes a templateId through: the same copy happens.
    let (status, created) = h
        .api(
            "createAgent",
            json!({ "name": "Cara", "templateId": template_id, "clientNonce": format!("tpl-{tag}") }),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let cara = CoworkerId::from_stored(created["agent"]["id"].as_str().expect("id").to_string());
    let policy = store.policy_for(&admin_id, &cara).await.expect("policy");
    assert_eq!(
        policy.grant.as_ref().expect("a grant").needs_approval,
        opengrok_policy::ToolSet::only(vec!["shell".to_string()])
    );
    assert_eq!(
        store.template_of(&cara).await.expect("use").as_deref(),
        Some(template_id.as_str())
    );

    // Editing the template changes no hired coworker; deleting it leaves them exactly as hired.
    let res = h
        .client
        .put(format!("{}/admin/templates/{template_id}", h.base))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&json!({ "name": "Researcher v2", "tools": ["read_file"], "limits": { "monthUsd": "1.00" } }))
        .send()
        .await
        .expect("update");
    assert_eq!(
        res.status(),
        200,
        "{}",
        res.text().await.unwrap_or_default()
    );
    let policy = store
        .policy_for(&member_id, &coworker)
        .await
        .expect("policy");
    assert_eq!(
        policy.grant.as_ref().expect("a grant").profile,
        opengrok_policy::ToolSet::only(vec!["read_file".to_string(), "shell".to_string()]),
        "hired coworkers keep what they were hired with"
    );
    let res = h
        .client
        .delete(format!("{}/admin/templates/{template_id}", h.base))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("delete");
    assert_eq!(res.status(), 204);
    let (status, _) = post(
        &member,
        "/coworkers",
        json!({ "name": "Dan", "templateId": template_id }),
    )
    .await;
    assert_eq!(status, 404, "a deleted template cannot be hired from");
    let limits = store
        .spend_limit(opengrok_store::SpendScope::Coworker, coworker.as_str())
        .await
        .expect("limits")
        .expect("still there");
    assert_eq!(
        limits.month_usd.as_deref(),
        Some("5.00"),
        "the copy outlives the template"
    );
}
