//! User-form slice A: the card, submit/dismiss, server-owned Type fill, and secret stripping.
//!
//! A coworker is hired with a computer that records `act` and `screenshot`. The mock door
//! asks for `request_user_form`. Submit types email then Tab then password, never screenshots,
//! settles `formResolution`, and the secret is absent from the entry, the transcript, and
//! history. `submitSecret` still drops its value and does not type. The AG-UI REST twin
//! authenticates with an account bearer. A turn that starts on `POST /ag-ui` (no `sendPrompt`)
//! still appends a gateway `user-form` entry so submit has an `entryId`.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, CuaAction, Screenshot, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
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

const SECRET: &str = "s3cret-pass-UNIQUE";
const EMAIL: &str = "ada@example.com";

#[derive(Default)]
struct FillStub {
    acts: Mutex<Vec<CuaAction>>,
    shots: Mutex<u32>,
}

impl FillStub {
    fn acts(&self) -> Vec<CuaAction> {
        self.acts.lock().expect("acts").clone()
    }
    fn shots(&self) -> u32 {
        *self.shots.lock().expect("shots")
    }
}

#[async_trait]
impl Computer for FillStub {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_stub_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(
        &self,
        _box_id: &str,
        _command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".to_string())
    }
    async fn stop(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _box_id: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
    async fn screen_url(&self, _box_id: &str) -> BoxResult<Option<String>> {
        Ok(Some("http://vnc.invalid".to_string()))
    }
    async fn screenshot(&self, _box_id: &str) -> BoxResult<Screenshot> {
        *self.shots.lock().expect("shots") += 1;
        Ok(Screenshot {
            mime: "image/png".to_string(),
            png_base64: "iVBORw0KGgo=".to_string(),
            width: 1280,
            height: 800,
        })
    }
    async fn act(&self, _box_id: &str, action: &CuaAction) -> BoxResult<()> {
        self.acts.lock().expect("acts").push(action.clone());
        Ok(())
    }
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Host".to_string(),
            last_name: String::new(),
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
        password_hash: Some("x".to_string()),
        first_name: "Host".to_string(),
        last_name: String::new(),
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

struct Harness {
    base: String,
    agui: AgUiState,
    store: PgStore,
    account: AccountId,
    stub: Arc<FillStub>,
    client: reqwest::Client,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, email).await;
    let stub = Arc::new(FillStub::default());
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"user-form-secret-user-form-ok")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::asking_for_user_form()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(stub.clone()),
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
        Some("http://opengrok.lan:1447".to_string()),
    )
    .allowing_identity_fallback();
    let app = opengrok_server::router(agui.clone(), gateway);
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
        account,
        stub,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    fn access_token(&self, email: &str) -> String {
        self.agui
            .auth
            .minter
            .mint_access(
                self.account.as_str(),
                "sess-test",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

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

    async fn hire(&self, token: &str, name: &str) -> String {
        let hired: Value = self
            .client
            .post(format!("{}/coworkers", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("hire")
            .json()
            .await
            .expect("hire json");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    async fn wait_for_form(&self, agent: &str) -> Value {
        for _ in 0..100 {
            let (_, tail) = self
                .api(
                    "getAgentTranscriptTail",
                    json!({ "id": agent, "limit": 100 }),
                )
                .await;
            if let Some(card) = tail["entries"].as_array().and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| {
                        entry["message"]["type"] == "user-form"
                            && entry
                                .get("formResolution")
                                .and_then(Value::as_str)
                                .is_none()
                    })
                    .cloned()
            }) {
                return card;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("no pending user-form card appeared in 10s");
    }

    async fn tail(&self, agent: &str) -> Value {
        let (_, tail) = self
            .api(
                "getAgentTranscriptTail",
                json!({ "id": agent, "limit": 100 }),
            )
            .await;
        tail
    }
}

fn dumped(tail: &Value) -> String {
    tail.to_string()
}

#[tokio::test]
async fn submit_types_into_the_box_settles_the_card_and_strips_secrets() {
    let database_url = database_or_skip!();
    let email = format!("user-form-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    let (status, sent) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "sign in", "clientNonce": "n-form" }),
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    let card = h.wait_for_form(&agent).await;
    assert_eq!(card["kind"], "send-message");
    assert_eq!(card["message"]["type"], "user-form");
    assert_eq!(card["message"]["formRequest"]["title"], "Google account");
    let pending_dump = card.to_string();
    assert!(
        !pending_dump.contains("s3cret-should-never-land"),
        "the model's smuggled values must not sit on the entry: {pending_dump}"
    );
    assert!(
        !pending_dump.contains(SECRET),
        "no submit value yet: {pending_dump}"
    );
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .api(
            "submitUserForm",
            json!({
                "entryId": entry_id,
                "agentId": agent,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert_eq!(body["sharedValues"]["email"], EMAIL, "{body}");
    assert!(
        body.get("sharedValues")
            .and_then(|v| v.get("password"))
            .is_none(),
        "password is not a shared value: {body}"
    );
    let settled_dump = body.to_string();
    assert!(!settled_dump.contains(SECRET), "{settled_dump}");

    let mut acts = Vec::new();
    for _ in 0..50 {
        acts = h.stub.acts();
        if acts.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        acts,
        vec![
            CuaAction::Type {
                text: EMAIL.to_string()
            },
            CuaAction::Key {
                key: "Tab".to_string()
            },
            CuaAction::Type {
                text: SECRET.to_string()
            },
        ],
        "fill is Type, Tab, Type"
    );
    assert_eq!(h.stub.shots(), 0, "fill must not screenshot");

    let tail = h.tail(&agent).await;
    let dump = dumped(&tail);
    assert!(!dump.contains(SECRET), "secret in transcript: {dump}");
    assert!(
        dump.contains("submitted") || dump.contains(EMAIL),
        "settled card still visible: {dump}"
    );

    // `submitSecret` still drops the value and does not type into the box.
    let before_acts = h.stub.acts().len();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let secret_entry = json!({
        "kind": "send-message",
        "id": format!("e_secret_{}", uuid::Uuid::now_v7().simple()),
        "timestampMs": now_ms(),
        "message": {
            "type": "secret-request",
            "secretRequest": { "label": "GITHUB_TOKEN", "description": "vault" }
        }
    });
    let secret_id = secret_entry["id"].as_str().unwrap().to_string();
    h.store
        .append_gateway_entry(&coworker, &h.account, &secret_entry, now_ms())
        .await
        .expect("append secret-request");
    let (status, saved) = h
        .api(
            "submitSecret",
            json!({
                "entryId": secret_id,
                "agentId": agent,
                "value": SECRET
            }),
        )
        .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["secretProvided"], true, "{saved}");
    let saved_dump = saved.to_string();
    assert!(!saved_dump.contains(SECRET), "{saved_dump}");
    assert_eq!(
        h.stub.acts().len(),
        before_acts,
        "submitSecret must not Type"
    );
    assert_eq!(h.stub.shots(), 0);
}

#[tokio::test]
async fn agui_rest_submit_round_trips_with_an_account_bearer() {
    let database_url = database_or_skip!();
    let email = format!("user-form-agui-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Bea").await;

    let (status, sent) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "sign in", "clientNonce": "n-agui" }),
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let res = h
        .client
        .post(format!("{}/ag-ui/user-form/submit", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "entryId": entry_id,
            "agentId": agent,
            "values": { "email": EMAIL, "password": SECRET }
        }))
        .send()
        .await
        .expect("agui submit");
    assert_eq!(res.status().as_u16(), 200, "agui submit status");
    let body: Value = res.json().await.expect("agui json");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");

    for _ in 0..50 {
        if h.stub.acts().len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        h.stub
            .acts()
            .iter()
            .any(|act| matches!(act, CuaAction::Type { text } if text == SECRET)),
        "AG-UI submit still types: {:?}",
        h.stub.acts()
    );
    assert_eq!(h.stub.shots(), 0);

    let (status, dismissed) = h
        .api(
            "dismissUserForm",
            json!({
                "entryId": entry_id,
                "agentId": agent,
                "mode": "dismissed"
            }),
        )
        .await;
    assert_eq!(status, 200, "{dismissed}");
    assert_eq!(
        dismissed["alreadyAnswered"], true,
        "a second answer is a no-op: {dismissed}"
    );
}

#[tokio::test]
async fn dismiss_settles_without_typing() {
    let database_url = database_or_skip!();
    let email = format!("user-form-d-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Cam").await;

    let (status, _) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "sign in", "clientNonce": "n-dismiss" }),
        )
        .await;
    assert_eq!(status, 200);
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .api(
            "dismissUserForm",
            json!({
                "entryId": entry_id,
                "agentId": agent,
                "mode": "escalated"
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "escalated", "{body}");
    assert_eq!(body["widgetDismissed"], true, "{body}");
    assert!(h.stub.acts().is_empty(), "dismiss must not Type");
    assert_eq!(h.stub.shots(), 0);
    assert!(!body.to_string().contains(SECRET), "{body}");
}

#[tokio::test]
async fn an_agui_only_turn_still_mints_a_user_form_entry_id() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-agui-only-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Dot").await;

    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
            "runId": uuid::Uuid::now_v7().to_string(),
            "messages": [{ "id": "m1", "role": "user", "content": "sign in" }],
            "forwardedProps": { "coworkerId": agent },
        }))
        .send()
        .await
        .expect("post ag-ui");
    assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
    let sse = res.text().await.expect("sse");
    // CUSTOM stays on the AG-UI SSE; the gateway card is a separate transcript entry.
    // TOOL_CALL_ARGS is the model's raw delta (the mock smuggles `values` there on purpose);
    // sanitise is on collect / CUSTOM arguments / the card, not the token stream.
    let mut saw_custom = false;
    for chunk in sse.split("\n\n") {
        let Some(payload) = chunk.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if event["type"] == "CUSTOM" && event["name"] == "run-awaiting-approval" {
            saw_custom = true;
            assert_eq!(event["reason"], "user-form", "{event}");
            let dump = event.to_string();
            assert!(
                !dump.contains("s3cret-should-never-land"),
                "CUSTOM arguments are sanitised: {dump}"
            );
            assert!(
                event
                    .get("arguments")
                    .and_then(|args| args.get("values"))
                    .is_none(),
                "values must not sit on CUSTOM: {event}"
            );
        }
    }
    assert!(saw_custom, "CUSTOM must still stream: {sse}");

    let card = h.wait_for_form(&agent).await;
    assert_eq!(card["kind"], "send-message");
    assert_eq!(card["message"]["type"], "user-form");
    let pending_dump = card.to_string();
    assert!(
        !pending_dump.contains("s3cret-should-never-land"),
        "the model's smuggled values must not sit on the entry: {pending_dump}"
    );
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let res = h
        .client
        .post(format!("{}/ag-ui/user-form/submit", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "entryId": entry_id,
            "agentId": agent,
            "values": { "email": EMAIL, "password": SECRET }
        }))
        .send()
        .await
        .expect("agui submit");
    assert_eq!(res.status().as_u16(), 200, "agui submit status");
    let body: Value = res.json().await.expect("agui json");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");

    for _ in 0..50 {
        if h.stub.acts().len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        h.stub.acts(),
        vec![
            CuaAction::Type {
                text: EMAIL.to_string()
            },
            CuaAction::Key {
                key: "Tab".to_string()
            },
            CuaAction::Type {
                text: SECRET.to_string()
            },
        ],
        "fill is Type, Tab, Type"
    );
    assert_eq!(h.stub.shots(), 0, "fill must not screenshot");
}
