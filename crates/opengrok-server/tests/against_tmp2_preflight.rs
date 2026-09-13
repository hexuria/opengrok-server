//! Pre-LLM resolve on the real send / complete / AG-UI paths.
//!
//! Counts `ModelDoor.stream` on the door the harness actually calls. Does not reimplement bind.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_harness::model::{DeltaStream, ModelDoor, ModelError, ModelRequest};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

struct CountingDoor {
    inner: MockDoor,
    streams: AtomicUsize,
    last: Mutex<Option<ModelRequest>>,
}

impl CountingDoor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MockDoor::echoing(),
            streams: AtomicUsize::new(0),
            last: Mutex::new(None),
        })
    }

    fn streams(&self) -> usize {
        self.streams.load(Ordering::SeqCst)
    }

    fn last_user_text(&self) -> String {
        self.last
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|request| {
                request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == "user")
                    .map(|message| message.content.clone())
            })
            .unwrap_or_default()
    }

    fn last_had_lookup_tool(&self) -> bool {
        self.last
            .lock()
            .unwrap()
            .as_ref()
            .map(|request| {
                request.tools.iter().any(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .is_some_and(|name| name.contains("lookup") || name.contains("user"))
                })
            })
            .unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl ModelDoor for CountingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(request.clone());
        self.inner.stream(request).await
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn seed_person(
    store: &PgStore,
    email: &str,
    first: &str,
    last: &str,
    org: Option<&str>,
) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: first.to_string(),
            last_name: last.to_string(),
            org_id: org.unwrap_or("").to_string(),
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
        first_name: first.to_string(),
        last_name: last.to_string(),
        org_id: org.map(str::to_string),
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

struct Harness {
    base: String,
    client: reqwest::Client,
    door: Arc<CountingDoor>,
    uriah: AccountId,
    ada: AccountId,
    eve: AccountId,
}

async fn boot() -> Option<Harness> {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return None;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let stamp = now_ms();
    let org = format!("acme-{stamp}");
    let email = format!("tmp2-caller-{stamp}@acme.test");
    let uriah = seed_person(&store, &email, "Uriah", "Galang", Some(&org)).await;
    let ada = seed_person(
        &store,
        &format!("tmp2-ada-{stamp}@acme.test"),
        "Ada",
        "Lovelace",
        Some(&org),
    )
    .await;
    let eve = seed_person(
        &store,
        &format!("tmp2-eve-{stamp}@evil.test"),
        "Eve",
        "Other",
        Some("evil"),
    )
    .await;

    let door = CountingDoor::new();
    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"tmp2-preflight-secret")),
            email.clone(),
        ),
        door: door.clone(),
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
        email,
        Some("http://opengrok.lan:1447".to_string()),
    )
    .allowing_identity_fallback();
    let app = opengrok_server::router(agui, gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Some(Harness {
        base,
        client: reqwest::Client::new(),
        door,
        uriah,
        ada,
        eve,
    })
}

async fn api(h: &Harness, method: &str, body: Value) -> (u16, Value) {
    let res = h
        .client
        .post(format!("{}/api/{method}", h.base))
        .header("authorization", "Bearer test-bearer")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("api");
    let status = res.status().as_u16();
    let text = res.text().await.expect("body");
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn hire(h: &Harness) -> String {
    let (status, created) = api(
        h,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    created["agent"]["id"].as_str().expect("id").to_string()
}

async fn wait_streams(door: &CountingDoor, want: usize) -> usize {
    for _ in 0..80 {
        if door.streams() >= want {
            return door.streams();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    door.streams()
}

#[tokio::test]
async fn complete_endpoint_does_not_open_door() {
    let Some(h) = boot().await else {
        return;
    };
    let res = h
        .client
        .get(format!("{}/tmp/complete?token=user&q=Uri", h.base))
        .header("authorization", "Bearer test-bearer")
        .send()
        .await
        .expect("complete");
    assert_eq!(res.status().as_u16(), 200, "complete must succeed");
    let body: Value = res.json().await.expect("json");
    let rows = body["candidates"].as_array().expect("candidates");
    assert!(
        rows.iter().any(|row| row["label"]
            .as_str()
            .is_some_and(|label| label.contains("Uriah"))),
        "expected Uriah in {body}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row["value"].as_str() == Some(h.eve.as_str())),
        "other-org Eve leaked: {body}"
    );
    assert_eq!(h.door.streams(), 0, "complete must not open the door");
    let catalog = body["catalog"].as_array().expect("catalog");
    assert!(
        catalog
            .iter()
            .any(|row| row["name"].as_str() == Some("user")
                && row["resolver"].as_str() == Some("org-accounts")),
        "org-users plugin must declare the user resolver in {body}"
    );
    assert!(
        catalog
            .iter()
            .any(|row| row["mention"].as_str() == Some("person")
                && row["plugin"].as_str() == Some("org-users")),
        "catalog must name the @person plugin: {body}"
    );
}

#[tokio::test]
async fn identity_cannot_escape_org() {
    let Some(h) = boot().await else {
        return;
    };
    let res = h
        .client
        .get(format!("{}/tmp/complete?token=user&q=Eve", h.base))
        .header("authorization", "Bearer test-bearer")
        .send()
        .await
        .expect("complete");
    let body: Value = res.json().await.expect("json");
    let rows = body["candidates"].as_array().cloned().unwrap_or_default();
    assert!(
        rows.is_empty()
            || rows
                .iter()
                .all(|row| row["value"].as_str() != Some(h.eve.as_str())),
        "Eve from org evil must not appear for acme: {body}"
    );
    assert_eq!(h.door.streams(), 0);
}

#[tokio::test]
async fn unique_user_grounds_then_opens_door_once() {
    let Some(h) = boot().await else {
        return;
    };
    let agent = hire(&h).await;
    let (status, sent) = api(
        &h,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "ping @user:Uriah",
            "clientNonce": format!("p-{}", now_ms()),
        }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    let (_, tail) = api(
        &h,
        "getAgentTranscriptTail",
        json!({ "id": agent, "limit": 20 }),
    )
    .await;
    assert_ne!(
        sent.get("tmp"),
        Some(&json!("pick")),
        "unique @user:Uriah must not pick; sent={sent} tail={tail}"
    );
    assert_eq!(
        wait_streams(&h.door, 1).await,
        1,
        "unique bind must open the door; sent={sent} tail={tail}"
    );
    let text = h.door.last_user_text();
    assert!(
        text.contains(h.uriah.as_str()),
        "model must see canonical id, got {text:?}"
    );
    assert!(!h.door.last_had_lookup_tool());
}

#[tokio::test]
async fn transcript_keeps_display_name_model_sees_id() {
    let Some(h) = boot().await else {
        return;
    };
    let agent = hire(&h).await;
    let wire = format!("@user:{} say hi", h.uriah);
    let (status, sent) = api(
        &h,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "Uriah Galang say hi",
            "tmpWire": wire,
            "tmpMode": true,
            "clientNonce": format!("p-{}", now_ms()),
        }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(
        wait_streams(&h.door, 1).await,
        1,
        "tmpWire unique bind must open the door; sent={sent}"
    );
    let model = h.door.last_user_text();
    assert!(
        model.contains(h.uriah.as_str()),
        "model must see canonical id, got {model:?}"
    );
    let (_, tail) = api(
        &h,
        "getAgentTranscriptTail",
        json!({ "id": agent, "limit": 20 }),
    )
    .await;
    let stored = user_message_texts(&tail);
    assert!(
        stored
            .iter()
            .any(|text| text.contains("Uriah Galang") && text.contains("say hi")),
        "transcript must store the display name, got {stored:?} tail={tail}"
    );
    assert!(
        stored.iter().all(|text| !text.contains("acct_")),
        "transcript user bubble must not show acct_ ids, got {stored:?}"
    );
}

fn user_message_texts(tail: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                let is_user = map.get("role").and_then(Value::as_str) == Some("user")
                    || map.get("author").and_then(Value::as_str) == Some("You");
                if is_user {
                    for key in ["content", "text"] {
                        if let Some(text) = map.get(key).and_then(Value::as_str) {
                            out.push(text.to_string());
                        }
                    }
                }
                for child in map.values() {
                    walk(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    walk(tail, &mut out);
    out
}

#[tokio::test]
async fn implicit_uriah_unique_grounds_before_door() {
    let Some(h) = boot().await else {
        return;
    };
    let agent = hire(&h).await;
    let (status, sent) = api(
        &h,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "email Uriah the invoice",
            "tmpMode": true,
            "clientNonce": format!("p-{}", now_ms()),
        }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(
        sent.get("tmp"),
        Some(&json!("tool")),
        "email leftover must be a host tool-call, not a chat; sent={sent}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        h.door.streams(),
        0,
        "email leftover must not open ModelDoor; the coworker has no mail tool"
    );
    let (_, tail) = api(
        &h,
        "getAgentTranscriptTail",
        json!({ "id": agent, "limit": 20 }),
    )
    .await;
    let names = tool_call_names(&tail);
    assert!(
        names.iter().any(|name| name == "find_user"),
        "expected find_user tool-call, got {names:?} tail={tail}"
    );
    assert!(
        names.iter().any(|name| name == "send_email"),
        "expected send_email tool-call, got {names:?} tail={tail}"
    );
}

fn tool_call_names(tail: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if map.get("kind").and_then(Value::as_str) == Some("tool-call") {
                    if let Some(name) = map.get("name").and_then(Value::as_str) {
                        out.push(name.to_string());
                    }
                }
                for child in map.values() {
                    walk(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    walk(tail, &mut out);
    out
}

#[tokio::test]
async fn ambiguous_user_does_not_open_door() {
    let Some(h) = boot().await else {
        return;
    };
    let agent = hire(&h).await;
    let before = h.door.streams();
    let (status, sent) = api(
        &h,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "ping @user",
            "clientNonce": format!("p-{}", now_ms()),
        }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(sent["tmp"], json!("pick"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        h.door.streams(),
        before,
        "ambiguous @user must not call ModelDoor.stream"
    );
    let (_, tail) = api(
        &h,
        "getAgentTranscriptTail",
        json!({ "id": agent, "limit": 20 }),
    )
    .await;
    let entries = tail["entries"].as_array().cloned().unwrap_or_default();
    assert!(
        entries
            .iter()
            .any(|entry| entry["kind"] == json!("tmp-pick")),
        "expected tmp-pick entry, got {tail}"
    );
}

#[tokio::test]
async fn unknown_explicit_at_user_does_not_open_door() {
    let Some(h) = boot().await else {
        return;
    };
    let agent = hire(&h).await;
    let before = h.door.streams();
    let (status, sent) = api(
        &h,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "ping @user:nobody",
            "clientNonce": format!("p-{}", now_ms()),
        }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(h.door.streams(), before);
    let _ = (h.ada, h.eve);
}

#[tokio::test]
async fn ag_ui_ambiguous_emits_tmp_pick_without_stream() {
    let Some(h) = boot().await else {
        return;
    };
    let minter = TokenMinter::new(b"tmp2-preflight-secret");
    let token = minter
        .mint_access(
            h.uriah.as_str(),
            "sess",
            "unused@acme.test",
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("jwt");
    let before = h.door.streams();
    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": "t1",
            "runId": "r1",
            "messages": [{ "id": "m1", "role": "user", "content": "ping @user" }],
            "forwardedProps": {}
        }))
        .send()
        .await
        .expect("ag-ui");
    let body = res.text().await.expect("sse");
    assert!(
        body.contains("tmp-pick"),
        "AG-UI pick must use Custom tmp-pick, got {body}"
    );
    assert_eq!(h.door.streams(), before);
}

#[tokio::test]
async fn tmp_mode_missing_required_user_does_not_open_door() {
    let Some(h) = boot().await else {
        return;
    };
    let agent = hire(&h).await;
    let before = h.door.streams();
    let (status, sent) = api(
        &h,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "say hi",
            "tmpMode": true,
            "clientNonce": format!("p-{}", now_ms()),
        }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(sent["tmp"], json!("pick"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        h.door.streams(),
        before,
        "activated plugin with no #user must not call ModelDoor.stream"
    );
}

#[tokio::test]
async fn implicit_uriah_without_tmp_mode_does_not_ground() {
    let Some(h) = boot().await else {
        return;
    };
    let agent = hire(&h).await;
    let (status, sent) = api(
        &h,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "email Uriah the invoice",
            "tmpMode": false,
            "clientNonce": format!("p-{}", now_ms()),
        }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    assert_ne!(sent.get("tmp"), Some(&json!("pick")));
    assert_eq!(wait_streams(&h.door, 1).await, 1);
    let text = h.door.last_user_text();
    assert!(
        !text.contains(h.uriah.as_str()),
        "without tmpMode, Uriah must stay a word, got {text:?}"
    );
}
