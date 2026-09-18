//! User-form slice A: the card, submit/dismiss, server-owned Type fill, and secret stripping.
//!
//! A coworker is hired with a computer that records `act` and `screenshot`. The mock door
//! asks for `request_user_form`. Submit types email then Tab then password, never screenshots,
//! settles `formResolution`, and the secret is absent from the entry, the transcript, and
//! history. `submitSecret` still drops its value and does not type. The AG-UI REST twin
//! authenticates with an account bearer. A turn that starts on `POST /ag-ui` (no `sendPrompt`)
//! stamps `entryId` on CUSTOM `run-awaiting-approval` so NativeChat can submit from the SSE.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{
    BoxResult, CommandOutput, Computer, CuaAction, EgressTunnel, Screenshot, StartedCommand,
};
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
    egress: Mutex<Option<EgressTunnel>>,
}

impl FillStub {
    fn acts(&self) -> Vec<CuaAction> {
        self.acts.lock().expect("acts").clone()
    }
    fn shots(&self) -> u32 {
        *self.shots.lock().expect("shots")
    }
    fn set_egress(&self, cap: Option<EgressTunnel>) {
        *self.egress.lock().expect("egress") = cap;
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
    async fn egress_tunnel(&self, _box_id: &str) -> Option<EgressTunnel> {
        *self.egress.lock().expect("egress")
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
    gateway: GatewayState,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    harness_with_door(
        database_url,
        email,
        Arc::new(MockDoor::asking_for_user_form()),
    )
    .await
}

async fn harness_with_door(database_url: &str, email: &str, door: Arc<MockDoor>) -> Harness {
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
        door,
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(stub.clone()),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    )
    .allowing_identity_fallback();
    let app = opengrok_server::router(agui.clone(), gateway.clone());
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
        gateway,
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

    async fn wait_for_handoff(&self, agent: &str) -> Value {
        for _ in 0..100 {
            let tail = self.tail(agent).await;
            if let Some(card) = tail["entries"].as_array().and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| {
                        entry["message"]["type"] == "attachment"
                            && entry["message"]["url"] == "sand://box"
                            && entry
                                .get("boxRequestId")
                                .and_then(Value::as_str)
                                .is_some_and(|id| !id.is_empty())
                            && entry
                                .get("boxResolution")
                                .and_then(Value::as_str)
                                .is_none_or(str::is_empty)
                    })
                    .cloned()
            }) {
                return card;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("no live box handoff appeared in 10s");
    }

    async fn pending_user_form_runs(&self) -> usize {
        self.store
            .awaiting_approval(&self.account)
            .await
            .expect("awaiting")
            .len()
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
    assert_eq!(body["formFieldOutcomes"][0]["id"], "email", "{body}");
    assert_eq!(body["formFieldOutcomes"][0]["filled"], true, "{body}");
    assert_eq!(body["formFieldOutcomes"][0]["fillFailed"], false, "{body}");
    assert_eq!(body["formFieldOutcomes"][1]["id"], "password", "{body}");
    assert_eq!(body["formFieldOutcomes"][1]["filled"], false, "{body}");
    assert_eq!(body["formFieldOutcomes"][1]["fillFailed"], false, "{body}");
    assert!(
        body.get("boxRequestId").is_none(),
        "user-form must not carry boxRequestId: {body}"
    );
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
        if !acts.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        acts,
        vec![CuaAction::Type {
            text: EMAIL.to_string()
        }],
        "default multi-field types only the first field"
    );
    assert!(
        !acts
            .iter()
            .any(|act| matches!(act, CuaAction::Type { text } if text == SECRET)),
        "must not type the password into the email box: {acts:?}"
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
        if !h.stub.acts().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        h.stub
            .acts()
            .iter()
            .any(|act| matches!(act, CuaAction::Type { text } if text == EMAIL)),
        "AG-UI submit still types the focused field: {:?}",
        h.stub.acts()
    );
    assert!(
        !h.stub
            .acts()
            .iter()
            .any(|act| matches!(act, CuaAction::Type { text } if text == SECRET)),
        "must not type the password into the email box: {:?}",
        h.stub.acts()
    );
    assert!(
        !h.stub
            .acts()
            .iter()
            .any(|act| matches!(act, CuaAction::Key { key } if key == "Return")),
        "default multi-field must not Return: {:?}",
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
    assert!(h.pending_user_form_runs().await >= 1);

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
    assert!(
        body.get("boxRequestId").is_none(),
        "escalate must not put boxRequestId on the user-form: {body}"
    );
    let handoff_id = body["handoffEntryId"]
        .as_str()
        .expect("handoffEntryId on escalate response")
        .to_string();
    assert!(h.stub.acts().is_empty(), "dismiss must not Type");
    assert_eq!(h.stub.shots(), 0);
    assert!(!body.to_string().contains(SECRET), "{body}");

    let handoff = h.wait_for_handoff(&agent).await;
    assert_eq!(handoff["id"], handoff_id);
    assert_eq!(handoff["message"]["type"], "attachment");
    assert_eq!(handoff["message"]["url"], "sand://box");
    assert!(
        handoff["boxRequestId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{handoff}"
    );
    assert!(
        handoff.get("boxResolution").is_none(),
        "boxResolution absent while live: {handoff}"
    );
    let dumped = handoff.to_string().to_lowercase();
    assert!(!dumped.contains("take over"), "{dumped}");
    assert!(!dumped.contains("i'm done"), "{dumped}");
    assert!(!dumped.contains("skip"), "{dumped}");

    // Escalated must KEEP hold: the user-form run is still awaiting hand-back.
    assert!(
        h.pending_user_form_runs().await >= 1,
        "escalate must not resume the run"
    );

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let (status, resolved) = h
        .api(
            "resolveBoxHandoff",
            json!({
                "entryId": handoff_id,
                "agentId": agent,
                "resolution": "handed_back"
            }),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["boxResolution"], "handed_back", "{resolved}");
    assert!(
        resolved.get("formResolution").is_none(),
        "handoff is not a user-form: {resolved}"
    );

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        h.pending_user_form_runs().await,
        0,
        "hand-back resumes the waiting run"
    );
    assert!(h.stub.acts().is_empty(), "hand-back must not Type");
    assert_eq!(h.stub.shots(), 0, "hand-back must not screenshot");
    // handBackForeverBox would stop the box; this path only stamps boxResolution.
    let still = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert_eq!(still["boxResolution"], "handed_back");
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
    let thread_id = format!("thr-{}", uuid::Uuid::now_v7());
    let run_id = uuid::Uuid::now_v7().to_string();

    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": thread_id,
            "runId": run_id,
            "messages": [{ "id": "m1", "role": "user", "content": "sign in" }],
            "forwardedProps": { "coworkerId": agent },
        }))
        .send()
        .await
        .expect("post ag-ui");
    assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
    let sse = res.text().await.expect("sse");
    // NativeChat mounts this CUSTOM frame. `entryId` must be on the SSE, not only on the
    // gateway transcript: they never watch `transcript:{agentId}`. TOOL_CALL_ARGS is the
    // model's raw delta (the mock smuggles `values` there on purpose).
    let mut custom_entry_id = None;
    for chunk in sse.split("\n\n") {
        let Some(payload) = chunk.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if event["type"] == "CUSTOM" && event["name"] == "run-awaiting-approval" {
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
            assert_eq!(
                event.get("formRequest"),
                event.get("arguments"),
                "formRequest is the same sanitised schema: {event}"
            );
            let id = event["entryId"]
                .as_str()
                .expect("CUSTOM extra.entryId")
                .to_string();
            assert!(id.starts_with("e_"), "entryId is a gateway entry id: {id}");
            custom_entry_id = Some(id);
        }
    }
    let entry_id = custom_entry_id.expect("CUSTOM must still stream with entryId");

    let card = h.wait_for_form(&agent).await;
    assert_eq!(card["kind"], "send-message");
    assert_eq!(card["message"]["type"], "user-form");
    assert_eq!(
        card["id"].as_str().expect("card id"),
        entry_id.as_str(),
        "SSE entryId is the gateway card id"
    );
    let pending_dump = card.to_string();
    assert!(
        !pending_dump.contains("s3cret-should-never-land"),
        "the model's smuggled values must not sit on the entry: {pending_dump}"
    );

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
    assert!(
        body.is_object(),
        "NativeChat collapses on Null submit body: {body}"
    );
    assert!(
        matches!(
            body["formResolution"].as_str(),
            Some("submitted") | Some("fill_failed")
        ),
        "submit of a stamped entryId must carry formResolution: {body}"
    );
    assert!(
        body.get("formFieldOutcomes")
            .and_then(Value::as_array)
            .is_some(),
        "formFieldOutcomes required on the happy path: {body}"
    );
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");

    let tail = h.tail(&agent).await;
    let tail_dump = dumped(&tail);
    assert!(
        !tail_dump.contains(SECRET),
        "secret in gateway tail: {tail_dump}"
    );
    let settled_tail = tail["entries"].as_array().and_then(|entries| {
        entries.iter().find(|entry| {
            entry["message"]["type"] == "user-form" && entry.get("formResolution").is_some()
        })
    });
    let settled_tail = settled_tail.expect("getAgentTranscriptTail must emit the settled card");
    assert_eq!(
        settled_tail["formResolution"], "submitted",
        "{settled_tail}"
    );
    assert_eq!(
        settled_tail["message"]["type"], "user-form",
        "{settled_tail}"
    );

    let replay = h
        .client
        .get(format!("{}/ag-ui/threads/{thread_id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay thread");
    assert_eq!(replay.status().as_u16(), 200, "thread replay");
    let replayed: Value = replay.json().await.expect("replay json");
    let replay_dump = replayed.to_string();
    assert!(
        !replay_dump.contains(SECRET),
        "secret in AG-UI replay: {replay_dump}"
    );
    let settled_replay = replayed["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["events"].as_array())
        .flatten()
        .find(|event| {
            event.get("formResolution").and_then(Value::as_str) == Some("submitted")
                && (event["message"]["type"] == "user-form"
                    || event["name"] == "user-form"
                    || event["reason"] == "user-form")
        });
    let settled_replay = settled_replay.unwrap_or_else(|| {
        panic!("GET /ag-ui/threads/{{id}} must emit settled user-form for NativeChat hydrate: {replay_dump}")
    });
    assert_eq!(settled_replay["formResolution"], "submitted");
    assert!(
        settled_replay["message"]["type"] == "user-form"
            || settled_replay.pointer("/value/message/type") == Some(&json!("user-form")),
        "message.type user-form: {settled_replay}"
    );
    let offer = replayed["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["events"].as_array())
        .flatten()
        .find(|event| event["name"] == "credential.offer_save");
    let offer = offer.expect("submitted fill must offer_save on AG-UI replay");
    assert_eq!(offer["origin"], "accounts.google.com", "{offer}");
    assert_eq!(offer["username"], EMAIL, "{offer}");
    assert_eq!(offer["formEntryId"], entry_id, "{offer}");
    let offer_dump = offer.to_string();
    assert!(
        !offer_dump.contains(SECRET),
        "offer_save must never include the password: {offer_dump}"
    );
    assert!(offer.get("password").is_none(), "{offer}");

    for _ in 0..50 {
        if !h.stub.acts().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        h.stub.acts(),
        vec![CuaAction::Type {
            text: EMAIL.to_string()
        }],
        "default multi-field types only the first field"
    );
    assert_eq!(h.stub.shots(), 0, "fill must not screenshot");
}

#[tokio::test]
async fn dismissed_resumes_without_a_handoff() {
    let database_url = database_or_skip!();
    let email = format!("user-form-skip-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Eve").await;

    let (status, _) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "sign in", "clientNonce": "n-dismissed" }),
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
                "mode": "dismissed"
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "dismissed", "{body}");
    assert!(body.get("handoffEntryId").is_none(), "{body}");
    assert!(h.stub.acts().is_empty());

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(h.pending_user_form_runs().await, 0);
    let tail = h.tail(&agent).await;
    let has_handoff = tail["entries"].as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry
                .get("boxRequestId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        })
    });
    assert!(
        !has_handoff,
        "dismissed must not start a box handoff: {tail}"
    );
}

#[tokio::test]
async fn an_unanswered_form_times_out_and_resumes() {
    let database_url = database_or_skip!();
    let email = format!("user-form-to-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Fay").await;

    let (status, _) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "sign in", "clientNonce": "n-timeout" }),
        )
        .await;
    assert_eq!(status, 200);
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    assert!(h.pending_user_form_runs().await >= 1);

    let settled = opengrok_server::gateway::user_form::timeout_unresolved_form(
        &h.gateway, &h.account, &coworker, &agent,
    )
    .await;
    assert!(settled, "timeout should settle the open form");

    let stored = h
        .store
        .find_gateway_entry(&coworker, &h.account, &entry_id)
        .await
        .expect("load")
        .expect("row")
        .1;
    assert_eq!(stored["formResolution"], "dismissed", "{stored}");
    assert_eq!(stored["timedOut"], true, "{stored}");
    assert_eq!(stored["widgetDismissed"], true, "{stored}");

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(h.pending_user_form_runs().await, 0);
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn agui_handoff_resolve_declines_without_stopping_the_box() {
    let database_url = database_or_skip!();
    let email = format!("user-form-hb-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Gia").await;

    let (status, _) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "sign in", "clientNonce": "n-decline" }),
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
    let handoff_id = body["handoffEntryId"]
        .as_str()
        .expect("handoff id")
        .to_string();
    assert!(h.pending_user_form_runs().await >= 1);

    let res = h
        .client
        .post(format!("{}/ag-ui/box-handoff/resolve", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "entryId": handoff_id,
            "agentId": agent,
            "resolution": "declined"
        }))
        .send()
        .await
        .expect("agui resolve");
    assert_eq!(res.status().as_u16(), 200, "agui resolve status");
    let resolved: Value = res.json().await.expect("agui json");
    assert_eq!(resolved["boxResolution"], "declined", "{resolved}");

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(h.pending_user_form_runs().await, 0);
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn a_missing_form_entry_is_an_error_not_null() {
    let database_url = database_or_skip!();
    let email = format!("user-form-miss-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Hal").await;

    let res = h
        .client
        .post(format!("{}/ag-ui/user-form/submit", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "entryId": "e_ghost_never_appended",
            "agentId": agent,
            "values": { "email": EMAIL }
        }))
        .send()
        .await
        .expect("agui submit ghost");
    assert_eq!(res.status().as_u16(), 404, "ghost entryId must not be Null");
    let body: Value = res.json().await.expect("agui json");
    assert_ne!(body, Value::Null, "{body}");
    assert_eq!(body["error"], "form entry missing", "{body}");
}

#[tokio::test]
async fn egress_tunnel_follows_host_setting() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-egress-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;

    let (status, body) = h.api("isEgressTunnelAvailable", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!(false), "default is off: {body}");

    let (status, settings) = h.api("getHostSettings", json!({})).await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings["egressTunnelEnabled"], false, "{settings}");

    let (status, settings) = h
        .api("setHostSettings", json!({ "egressTunnelEnabled": true }))
        .await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings["egressTunnelEnabled"], true, "{settings}");

    let (status, body) = h.api("isEgressTunnelAvailable", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!(false),
        "host setting alone is not available until /v1/info ready: {body}"
    );

    let (status, settings) = h
        .api("setHostSettings", json!({ "egressTunnelEnabled": false }))
        .await;
    assert_eq!(status, 200, "{settings}");
    let (status, body) = h.api("isEgressTunnelAvailable", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!(false), "toggle off: {body}");
}

#[tokio::test]
async fn egress_tunnel_needs_box_ready_when_info_is_present() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-egress-box-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "egress-box").await;
    let (status, _) = h.api("openAgent", json!({ "id": agent })).await;
    assert_eq!(status, 200);

    let (status, _) = h
        .api("setHostSettings", json!({ "egressTunnelEnabled": true }))
        .await;
    assert_eq!(status, 200);
    let (status, body) = h.api("isEgressTunnelAvailable", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!(false),
        "a box that cannot report /v1/info is not available: {body}"
    );

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: false,
    }));
    let (status, body) = h.api("isEgressTunnelAvailable", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!(false),
        "host wants the tunnel but no laptop client is attached: {body}"
    );

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: true,
    }));
    let (status, body) = h.api("isEgressTunnelAvailable", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!(true), "ready box + host setting: {body}");

    let (status, _) = h
        .api("setHostSettings", json!({ "egressTunnelEnabled": false }))
        .await;
    assert_eq!(status, 200);
    let (status, body) = h.api("isEgressTunnelAvailable", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!(false),
        "a ready box does not override a host that opted out: {body}"
    );
}

#[tokio::test]
async fn credential_request_round_trips_a_status_and_never_stores_a_password() {
    let database_url = database_or_skip!();
    let email = format!(
        "credential-result-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_credential()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Dot").await;
    let thread_id = format!("thr-{}", uuid::Uuid::now_v7());
    let run_id = uuid::Uuid::now_v7().to_string();

    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": thread_id,
            "runId": run_id,
            "messages": [{ "id": "m1", "role": "user", "content": "sign in" }],
            "forwardedProps": { "coworkerId": agent },
        }))
        .send()
        .await
        .expect("post ag-ui");
    assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
    let sse = res.text().await.expect("sse");
    let mut request_id = None;
    for chunk in sse.split("\n\n") {
        let Some(payload) = chunk.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if event["type"] == "CUSTOM" && event["name"] == "credential.request" {
            let dump = event.to_string();
            assert!(
                !dump.contains("s3cret-should-never-land"),
                "CUSTOM must not carry a password: {dump}"
            );
            assert_eq!(event["origin"], "accounts.google.com", "{event}");
            assert_eq!(event["reason"], "credential", "{event}");
            request_id = event["requestId"]
                .as_str()
                .or_else(|| event["callId"].as_str())
                .map(str::to_string);
        }
    }
    let request_id = request_id.expect("credential.request CUSTOM with requestId");

    let res = h
        .client
        .post(format!("{}/ag-ui/credential/result", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "agentId": agent,
            "requestId": request_id,
            "status": "session_established",
            "credentialId": "cred_nativechat_1",
            "password": SECRET
        }))
        .send()
        .await
        .expect("credential result");
    assert_eq!(res.status().as_u16(), 200, "credential result status");
    let body: Value = res.json().await.expect("result json");
    assert_eq!(body["status"], "session_established", "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");

    let replay = h
        .client
        .get(format!("{}/ag-ui/threads/{thread_id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay thread");
    assert_eq!(replay.status().as_u16(), 200, "thread replay");
    let replayed: Value = replay.json().await.expect("replay json");
    let replay_dump = replayed.to_string();
    assert!(
        !replay_dump.contains(SECRET),
        "secret in AG-UI replay: {replay_dump}"
    );
    assert!(
        replay_dump.contains("credential.request"),
        "replay must keep the request CUSTOM: {replay_dump}"
    );

    let hints = h
        .store
        .credential_hints(
            &h.account,
            &opengrok_core::id::CoworkerId::from_stored(agent),
        )
        .await
        .expect("hints");
    assert_eq!(hints.len(), 1, "{hints:?}");
    assert_eq!(hints[0].origin, "accounts.google.com");
    assert_eq!(hints[0].credential_id, "cred_nativechat_1");
    assert!(!format!("{hints:?}").contains(SECRET), "{hints:?}");
}
