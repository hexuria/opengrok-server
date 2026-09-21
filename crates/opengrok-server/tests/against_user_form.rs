//! User-form slice A: the card, submit/dismiss, server-owned Type fill, and secret stripping.
//!
//! A coworker is hired with a computer that records `act` and `screenshot`. The mock door
//! asks for `request_user_form`. Submit types email then Tab then password, never screenshots,
//! settles `formResolution`, and the secret is absent from the entry, the transcript, and
//! history. The AG-UI REST twin authenticates with an account bearer, and a turn on
//! `POST /ag-ui` stamps `entryId` on CUSTOM `run-awaiting-approval` so NativeChat can submit
//! straight from the SSE frame.

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
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

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

const SECRET: &str = "s3cret-pass-UNIQUE";
const EMAIL: &str = "ada@example.com";

#[derive(Default)]
struct FillStub {
    acts: Mutex<Vec<CuaAction>>,
    ran: Mutex<Vec<String>>,
    shots: Mutex<u32>,
    egress: Mutex<Option<EgressTunnel>>,
    last_egress_box: Mutex<Option<String>>,
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
    fn last_egress_box(&self) -> Option<String> {
        self.last_egress_box.lock().expect("egress box").clone()
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
        command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        self.ran.lock().expect("ran").push(command.to_string());
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
    async fn offers_a_screen(&self, _box_id: &str) -> bool {
        true
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
    async fn egress_tunnel(&self, box_id: &str) -> Option<EgressTunnel> {
        *self.last_egress_box.lock().expect("egress box") = Some(box_id.to_string());
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
    gateway: HostState,
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
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
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

    /// One AG-UI POST with an account bearer — how NativeChat answers a card.
    async fn agui(&self, token: &str, path: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .expect("ag-ui call");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// One turn on the AG-UI door. The reply is the whole SSE stream; a turn that pauses for a
    /// form ends it with `run-awaiting-approval`.
    async fn turn(&self, token: &str, agent: &str, prompt: &str) -> String {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "threadId": format!("gateway-{agent}"),
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": format!("m-{}", uuid::Uuid::now_v7().simple()), "role": "user", "content": prompt }],
                "forwardedProps": { "coworkerId": agent },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
        res.text().await.expect("sse")
    }

    /// `GET /ag-ui/host-settings[?coworker=…]` — the record, plus this coworker's tunnel.
    async fn host_settings(&self, token: &str, query: &str) -> Value {
        let res = self
            .client
            .get(format!("{}/ag-ui/host-settings{query}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("get host settings");
        assert_eq!(res.status().as_u16(), 200);
        res.json().await.expect("host settings json")
    }

    /// `PUT /ag-ui/host-settings` — a partial record, merged.
    async fn patch_host_settings(&self, token: &str, patch: Value) -> Value {
        let res = self
            .client
            .put(format!("{}/ag-ui/host-settings", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&patch)
            .send()
            .await
            .expect("put host settings");
        assert_eq!(res.status().as_u16(), 200);
        res.json().await.expect("host settings json")
    }

    async fn computer_json(&self, token: &str, agent: &str) -> (u16, Value) {
        let res = self
            .client
            .get(format!("{}/coworkers/{agent}/computer", self.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("GET computer");
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
            let tail = self.tail(agent).await;
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

    async fn wait_for_forms(&self, agent: &str, n: usize) -> Vec<Value> {
        for _ in 0..100 {
            let tail = self.tail(agent).await;
            let cards: Vec<Value> = tail["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|entry| {
                    entry["message"]["type"] == "user-form"
                        && entry
                            .get("formResolution")
                            .and_then(Value::as_str)
                            .is_none()
                })
                .cloned()
                .collect();
            if cards.len() >= n {
                return cards;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("expected {n} pending user-form cards in 10s");
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

    /// The coworker's transcript, in the shape the assertions here already read. Straight off the
    /// store: the verb that used to serve it was the desktop's.
    async fn tail(&self, agent: &str) -> Value {
        let entries = self
            .store
            .gateway_transcript(
                &opengrok_core::id::CoworkerId::from_stored(agent.to_string()),
                &self.account,
            )
            .await
            .expect("transcript");
        json!({ "entries": entries })
    }

    async fn wait_user_form_idle(&self) {
        for _ in 0..50 {
            if self.pending_user_form_runs().await == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("user-form run still awaiting after Skip/hand-back");
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

    h.turn(&token, &agent, "sign in").await;
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
        .agui(
            &token,
            "/ag-ui/user-form/submit",
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
}

#[tokio::test]
async fn agui_rest_submit_round_trips_with_an_account_bearer() {
    let database_url = database_or_skip!();
    let email = format!("user-form-agui-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Bea").await;

    h.turn(&token, &agent, "sign in").await;
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
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
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

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    assert!(h.pending_user_form_runs().await >= 1);

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
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
        .agui(
            &token,
            "/ag-ui/box-handoff/resolve",
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

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
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

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    assert!(h.pending_user_form_runs().await >= 1);

    let settled = opengrok_server::agui::user_form::timeout_unresolved_form(
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

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
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
async fn skip_via_form_entry_id_settles_the_live_handoff_and_resumes() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-skip-form-id-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ivy").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let form_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
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
    let _ = h.wait_for_handoff(&agent).await;
    assert!(
        h.pending_user_form_runs().await >= 1,
        "escalate must not resume"
    );

    // NativeChat KeepAlive: Skip posts the form gateway id when handoffEntryId is missing.
    let (status, resolved) = h
        .agui(
            &token,
            "/ag-ui/box-handoff/resolve",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "resolution": "declined"
            }),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["boxResolution"], "declined", "{resolved}");

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let form = h
        .store
        .find_gateway_entry(&coworker, &h.account, &form_id)
        .await
        .expect("load form")
        .expect("form row")
        .1;
    assert_eq!(form["formResolution"], "escalated", "{form}");
    assert!(
        form.get("boxRequestId").is_none(),
        "resolve must not convert the form into a handoff: {form}"
    );
    assert!(
        form.get("boxResolution").is_none(),
        "boxResolution stays on the sibling: {form}"
    );
    let handoff = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert_eq!(handoff["boxResolution"], "declined", "{handoff}");

    h.wait_user_form_idle().await;
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn dismiss_dismissed_after_escalate_abandons_the_live_handoff() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-abandon-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Jen").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let form_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
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
    let _ = h.wait_for_handoff(&agent).await;
    assert!(h.pending_user_form_runs().await >= 1);

    let (status, dismissed) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "mode": "dismissed"
            }),
        )
        .await;
    assert_eq!(status, 200, "{dismissed}");
    assert_eq!(dismissed["formResolution"], "escalated", "{dismissed}");
    assert!(dismissed.get("boxRequestId").is_none(), "{dismissed}");

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let handoff = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert_eq!(handoff["boxResolution"], "declined", "{handoff}");
    h.wait_user_form_idle().await;
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn a_second_escalate_does_not_resume_while_the_handoff_is_live() {
    let database_url = database_or_skip!();
    let email = format!("user-form-reesc-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Kim").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let form_id = card["id"].as_str().expect("entry id").to_string();

    let (status, first) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "mode": "escalated"
            }),
        )
        .await;
    assert_eq!(status, 200, "{first}");
    let handoff_id = first["handoffEntryId"]
        .as_str()
        .expect("handoff id")
        .to_string();
    let _ = h.wait_for_handoff(&agent).await;

    let (status, again) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "mode": "escalated"
            }),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["formResolution"], "escalated", "{again}");
    assert!(
        h.pending_user_form_runs().await >= 1,
        "a second escalate must not resume"
    );

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let handoff = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert!(
        handoff.get("boxResolution").is_none(),
        "escalate retry must leave the handoff live: {handoff}"
    );

    let (status, resolved) = h
        .agui(
            &token,
            "/ag-ui/box-handoff/resolve",
            json!({
                "entryId": handoff_id,
                "agentId": agent,
                "resolution": "handed_back"
            }),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["boxResolution"], "handed_back", "{resolved}");
    h.wait_user_form_idle().await;
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
async fn egress_tunnel_needs_box_ready_when_info_is_present() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-egress-box-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "egress-box").await;
    let named = format!("?coworker={agent}");

    let record = h
        .patch_host_settings(&token, json!({ "egressTunnelEnabled": true }))
        .await;
    assert_eq!(record["egressTunnelEnabled"], true, "{record}");
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], false,
        "a box that cannot report /v1/info is not available: {record}"
    );

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: false,
    }));
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], false,
        "host wants the tunnel but no laptop client is attached: {record}"
    );

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: true,
    }));
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], true,
        "ready box + host setting: {record}"
    );

    h.patch_host_settings(&token, json!({ "egressTunnelEnabled": false }))
        .await;
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], false,
        "a ready box does not override a host that opted out: {record}"
    );
}

/// NativeChat 729bdd9 reads GET `/coworkers/{id}/computer` only — not Box
/// `/v1/info` and not the gateway verb. Live ready must be on this JSON.
#[tokio::test]
async fn computer_json_stamps_the_scoped_box_live_egress() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-computer-egress-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "computer-egress").await;

    let (status, body) = h.computer_json(&token, &agent).await;
    assert_eq!(status, 200, "{body}");
    let box_id = body["boxId"].as_str().expect("scoped boxId").to_string();
    assert_eq!(
        h.stub.last_egress_box().as_deref(),
        Some(box_id.as_str()),
        "probe the payload's live boxId, not a frozen coworker row: {body}"
    );
    assert_eq!(
        body["isEgressTunnelAvailable"], false,
        "missing /v1/info is not available: {body}"
    );
    assert!(
        body.get("egress_tunnel").is_none(),
        "do not invent nested ready: {body}"
    );
    assert_eq!(
        body["shareScope"], "user",
        "default per-account share places chrome in Settings→Computer: {body}"
    );
    assert!(
        body.get("groupId").is_none(),
        "user share has no groupId: {body}"
    );

    h.patch_host_settings(&token, json!({ "egressTunnelEnabled": true }))
        .await;

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: false,
    }));
    let (status, body) = h.computer_json(&token, &agent).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["isEgressTunnelAvailable"], false, "{body}");
    assert_eq!(body["egress_tunnel"]["enabled"], true, "{body}");
    assert_eq!(body["egress_tunnel"]["ready"], false, "{body}");

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: true,
    }));
    let (status, body) = h.computer_json(&token, &agent).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["isEgressTunnelAvailable"], true, "{body}");
    assert_eq!(body["egress_tunnel"]["enabled"], true, "{body}");
    assert_eq!(body["egress_tunnel"]["ready"], true, "{body}");
    assert_eq!(
        h.stub.last_egress_box().as_deref(),
        Some(box_id.as_str()),
        "{body}"
    );
    assert_eq!(body["shareScope"], "user", "{body}");
}

fn sse_events(sse: &str) -> Vec<Value> {
    sse.split("\n\n")
        .filter_map(|chunk| {
            chunk
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .and_then(|payload| serde_json::from_str(payload).ok())
        })
        .collect()
}

/// The model's answer, glued back together: the stream is one delta per word.
fn answer_text(sse: &str) -> String {
    sse_events(sse)
        .iter()
        .filter(|event| event["type"] == "TEXT_MESSAGE_CONTENT")
        .filter_map(|event| event["delta"].as_str().map(str::to_string))
        .collect()
}

/// NativeChat paints Website login from live TOOL_CALL. Those frames must carry `e_*` —
/// a leftover `call-*` is Continue that cannot `POST /ag-ui/user-form/submit`.
fn assert_live_form_tool_calls_carry_gateway_entry_ids(events: &[Value], custom_ids: &[String]) {
    let mut form_calls = Vec::new();
    for event in events {
        if event["type"] == "TOOL_CALL_START" && event["toolCallName"] == "request_user_form" {
            let call_id = event["toolCallId"]
                .as_str()
                .expect("toolCallId")
                .to_string();
            let entry_id = event["entryId"].as_str().unwrap_or_else(|| {
                panic!("live TOOL_CALL_START {call_id} must carry e_*, not a raw call id: {event}")
            });
            assert!(
                entry_id.starts_with("e_"),
                "live TOOL_CALL_START {call_id} entryId must be a gateway id, got {entry_id}"
            );
            assert!(
                custom_ids.iter().any(|id| id == entry_id),
                "TOOL_CALL {call_id} entryId {entry_id} missing from CUSTOMs {custom_ids:?}"
            );
            form_calls.push(call_id);
        }
    }
    assert_eq!(
        form_calls.len(),
        custom_ids.len(),
        "one live TOOL_CALL_START per CUSTOM: calls={form_calls:?} customs={custom_ids:?} {events:?}"
    );
    for event in events {
        let kind = event["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "TOOL_CALL_START" | "TOOL_CALL_ARGS" | "TOOL_CALL_END" | "TOOL_CALL_RESULT"
        ) {
            continue;
        }
        let Some(call_id) = event["toolCallId"].as_str() else {
            continue;
        };
        if !form_calls.iter().any(|id| id == call_id) {
            continue;
        }
        let entry_id = event["entryId"].as_str().unwrap_or("");
        assert!(
            entry_id.starts_with("e_"),
            "live {kind} {call_id} must carry e_*: {event}"
        );
    }
}

#[tokio::test]
async fn stacked_user_forms_in_one_completion_each_get_an_entry_id() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-stacked-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_stacked_user_forms()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Stack").await;
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
    let events = sse_events(&sse);
    assert_eq!(
        events.last().and_then(|event| event["type"].as_str()),
        Some("RUN_FINISHED"),
        "HITL park must close Waiting: {sse}"
    );
    let customs: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "CUSTOM"
                && event["name"] == "run-awaiting-approval"
                && event["reason"] == "user-form"
        })
        .collect();
    assert_eq!(customs.len(), 3, "one CUSTOM per stacked form: {sse}");
    let mut entry_ids = Vec::new();
    for custom in &customs {
        let id = custom["entryId"]
            .as_str()
            .expect("CUSTOM extra.entryId")
            .to_string();
        assert!(id.starts_with("e_"), "entryId is a gateway entry id: {id}");
        assert!(
            !entry_ids.contains(&id),
            "stacked forms must not share an entryId: {entry_ids:?} {id}"
        );
        entry_ids.push(id);
        let dump = custom.to_string();
        assert!(
            !dump.contains("s3cret-should-never-land"),
            "CUSTOM arguments are sanitised: {dump}"
        );
    }
    assert_live_form_tool_calls_carry_gateway_entry_ids(&events, &entry_ids);

    let replay = h
        .client
        .get(format!("{}/ag-ui/runs/{run_id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay run");
    assert_eq!(replay.status().as_u16(), 200);
    let replayed: Value = replay.json().await.expect("replay json");
    assert_eq!(
        replayed["status"].as_str(),
        Some("awaiting-approval"),
        "RUN_FINISHED on the stream must not Finish the parked run: {replayed}"
    );
    let replay_events = replayed["events"].as_array().cloned().unwrap_or_default();
    assert!(
        replay_events
            .iter()
            .any(|event| event["type"] == "RUN_FINISHED"),
        "replay must keep the SSE closer: {replayed}"
    );
    for id in &entry_ids {
        assert!(
            replay_events.iter().any(|event| {
                event["toolCallId"].as_str().is_some() && event["entryId"].as_str() == Some(id)
            }),
            "TOOL_CALL replay must carry entryId {id}: {replayed}"
        );
    }

    let cards = h.wait_for_forms(&agent, 3).await;
    assert_eq!(cards.len(), 3, "{cards:?}");
    // Tail is newest-first; SSE CUSTOMs are oldest-first. Join on id, not zip order.
    let card_ids: Vec<&str> = cards
        .iter()
        .filter_map(|card| card["id"].as_str())
        .collect();
    for id in &entry_ids {
        assert!(
            card_ids.contains(&id.as_str()),
            "gateway card missing for {id}: {cards:?}"
        );
    }
    for card in &cards {
        assert!(
            card.get("callId")
                .and_then(Value::as_str)
                .is_some_and(|call| !call.is_empty()),
            "callId joins Continue to the parked TOOL_CALL: {card}"
        );
    }

    for (index, entry_id) in entry_ids.iter().enumerate() {
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
        assert_eq!(res.status().as_u16(), 200, "submit {index} status");
        let body: Value = res.json().await.expect("agui json");
        assert_eq!(
            body["formResolution"], "submitted",
            "submit {index}: {body}"
        );
        assert!(!body.to_string().contains(SECRET), "{body}");
        if index < 2 {
            assert!(
                h.pending_user_form_runs().await >= 1,
                "extra stacked Continues must not consume the parked call"
            );
        }
    }
    h.wait_user_form_idle().await;
    assert_eq!(h.pending_user_form_runs().await, 0);
}

#[tokio::test]
async fn two_same_completion_website_logins_live_tool_calls_carry_e_ids() {
    let database_url = database_or_skip!();
    let email = format!("user-form-twin-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_two_website_logins()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Twin").await;
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
    let events = sse_events(&sse);
    let customs: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "CUSTOM"
                && event["name"] == "run-awaiting-approval"
                && event["reason"] == "user-form"
        })
        .collect();
    assert_eq!(customs.len(), 2, "two Website login CUSTOMs: {sse}");
    let mut entry_ids = Vec::new();
    let mut call_ids = Vec::new();
    for custom in &customs {
        let id = custom["entryId"]
            .as_str()
            .expect("CUSTOM extra.entryId")
            .to_string();
        assert!(id.starts_with("e_"), "gateway entryId, not call-*: {id}");
        assert!(
            !entry_ids.contains(&id),
            "twins must not share an entryId: {entry_ids:?}"
        );
        entry_ids.push(id);
        call_ids.push(
            custom["callId"]
                .as_str()
                .expect("CUSTOM callId")
                .to_string(),
        );
    }
    assert_eq!(
        call_ids,
        vec!["call-42628be6".to_string(), "call-42628be6-1".to_string()],
        "{call_ids:?}"
    );
    assert_live_form_tool_calls_carry_gateway_entry_ids(&events, &entry_ids);

    for (index, entry_id) in entry_ids.iter().enumerate() {
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
        assert_eq!(res.status().as_u16(), 200, "submit {index} status");
        let body: Value = res.json().await.expect("agui json");
        assert_eq!(
            body["formResolution"], "submitted",
            "submit {index} of {entry_id}: {body}"
        );
        if index == 0 {
            assert!(
                h.pending_user_form_runs().await >= 1,
                "first Continue must not consume the parked twin"
            );
        }
    }
    h.wait_user_form_idle().await;
    assert_eq!(h.pending_user_form_runs().await, 0);
}

#[tokio::test]
async fn a_new_prompt_while_waiting_interrupts_the_parked_run_as_steer() {
    let database_url = database_or_skip!();
    let email = format!("user-form-steer-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_user_form_until_steered()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Steer").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    assert!(h.pending_user_form_runs().await >= 1);

    let steered = h
        .turn(
            &token,
            &agent,
            "forget the form and list the open tabs instead",
        )
        .await;

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        h.pending_user_form_runs().await,
        0,
        "parked HITL must not remain beside the new turn"
    );

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let form = h
        .store
        .find_gateway_entry(&coworker, &h.account, &entry_id)
        .await
        .expect("load form")
        .expect("form row")
        .1;
    assert_eq!(
        form["formResolution"], "dismissed",
        "interrupt settles leftover cards without filling: {form}"
    );
    assert!(
        h.stub.acts().is_empty(),
        "steer must not Type into the form"
    );

    // The steer reached the MODEL, not just the run log: the mock answers by quoting what it
    // was told, so its answer naming the new text is the proof the parked card did not swallow it.
    let answer = answer_text(&steered);
    assert!(
        answer.contains("list the open tabs")
            && answer.contains("You said:")
            && answer.contains("mock door"),
        "steer must reach the model as a new turn: {answer}"
    );
}

#[tokio::test]
async fn escalate_still_holds_until_hand_back_and_is_not_an_interrupt() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-escalate-hold-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Hold").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let form_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "mode": "escalated"
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let _ = h.wait_for_handoff(&agent).await;
    assert!(
        h.pending_user_form_runs().await >= 1,
        "escalate must not resume or interrupt"
    );
}

/// A saved login is the person's own. On a box the account shares (the default sharing
/// mode), the submit that carries it is refused and the card stays open; once the bot has
/// its own box, the same submit fills. A hand-typed submit was never subject to the rule.
#[tokio::test]
async fn a_saved_login_fills_only_a_dedicated_box() {
    let database_url = database_or_skip!();
    let email = format!("user-form-saved-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    let submit = json!({
        "entryId": entry_id,
        "agentId": agent,
        "savedLogin": true,
        "values": { "email": EMAIL, "password": SECRET }
    });

    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", submit.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "shared-computer", "{body}");
    assert!(
        h.stub.acts.lock().expect("acts").is_empty(),
        "nothing was typed into the shared box"
    );
    let still_open = h.wait_for_form(&agent).await;
    assert!(
        still_open.get("formResolution").is_none()
            && still_open["message"].get("formResolution").is_none(),
        "the card is still open: {still_open}"
    );

    // A bot hired once the account gives each bot its own computer fills from the same card.
    h.store
        .set_sharing_mode("account", h.account.as_str(), "per-bot", 1)
        .await
        .expect("per-bot");
    let own = h.hire(&token, "Bea").await;
    h.turn(&token, &own, "sign in").await;
    let card = h.wait_for_form(&own).await;
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"].as_str().expect("entry id"),
                "agentId": own,
                "savedLogin": true,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(
        !h.stub.acts.lock().expect("acts").is_empty(),
        "the dedicated box was typed into"
    );
    // A login that came from the vault is not offered to the vault again, and nothing of it
    // is shared back to the model.
    assert!(
        body.get("sharedValues")
            .and_then(Value::as_object)
            .is_none_or(|shared| shared.is_empty()),
        "a saved login shares nothing: {body}"
    );
    let replay = h
        .client
        .get(format!("{}/ag-ui/threads/gateway-{own}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay thread");
    let replayed: Value = replay.json().await.expect("replay json");
    let offered = replayed["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["events"].as_array())
        .flatten()
        .any(|event| event["name"] == "credential.offer_save");
    assert!(!offered, "no save offer after a vault fill: {replayed}");

    // A bot the person shares with their org is driven by everyone in it: its box never gets
    // a saved login, even though the box is the bot's own.
    let shown = h.hire(&token, "Dee").await;
    let patched = h
        .client
        .patch(format!("{}/coworkers/{shown}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "visibility": "org" }))
        .send()
        .await
        .expect("patch visibility");
    assert!(patched.status().is_success(), "{}", patched.status());
    h.turn(&token, &shown, "sign in").await;
    let card = h.wait_for_form(&shown).await;
    let acts_before = h.stub.acts.lock().expect("acts").len();
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"].as_str().expect("entry id"),
                "agentId": shown,
                "savedLogin": true,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "shared-computer", "{body}");
    assert_eq!(
        h.stub.acts.lock().expect("acts").len(),
        acts_before,
        "nothing typed into an org-visible bot's box"
    );
}

/// A passkey card typed nothing and asked the box for its DevTools pipe. The stub box has
/// none, so the card settles as Not filled with the reason, and the bot is told not to type
/// a password; on a shared box the card is refused before anything is asked.
#[tokio::test]
async fn a_passkey_card_asks_the_box_for_its_pipe_and_settles_honestly_without_one() {
    let database_url = database_or_skip!();
    let email = format!("passkey-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::with_script(vec![
            opengrok_harness::ModelDelta::Text("passkey time".to_string()),
            opengrok_harness::ModelDelta::ToolCallStart {
                id: "pk-1".to_string(),
                name: "request_user_form".to_string(),
            },
            opengrok_harness::ModelDelta::ToolCallArgs {
                id: "pk-1".to_string(),
                delta: r#"{"title":"Sign in with your passkey","challengeKind":"passkey","passkeyMode":"use","liveHost":"webauthn.io","fields":[]}"#.to_string(),
            },
            opengrok_harness::ModelDelta::ToolCallEnd {
                id: "pk-1".to_string(),
            },
        ])),
    )
    .await;
    let token = h.access_token(&email);

    // Shared box (the default sharing mode): refused, the card stays open.
    let shared = h.hire(&token, "Ada").await;
    h.turn(&token, &shared, "sign in").await;
    let card = h.wait_for_form(&shared).await;
    assert_eq!(
        card["message"]["formRequest"]["challengeKind"], "passkey",
        "{card}"
    );
    assert_eq!(
        card["message"]["formRequest"]["passkeyMode"], "use",
        "{card}"
    );
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": card["id"].as_str().expect("id"), "agentId": shared, "savedLoginId": "sl_x", "values": {} }),
        )
        .await;
    assert_eq!(status, 403, "{body}");

    // Own box: the pipe is asked for; the stub has none; the card says so.
    h.store
        .set_sharing_mode("account", h.account.as_str(), "per-bot", 1)
        .await
        .expect("per-bot");
    let own = h.hire(&token, "Bea").await;
    h.turn(&token, &own, "sign in").await;
    let card = h.wait_for_form(&own).await;
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": card["id"].as_str().expect("id"), "agentId": own, "savedLoginId": "sl_missing", "values": {} }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "fill_failed", "{body}");
    assert!(
        h.stub.acts.lock().expect("acts").is_empty(),
        "a passkey card types nothing"
    );
    // A computer without a pipe has nothing in it touched: no browser is killed.
    let ran = h.stub.ran.lock().expect("ran").clone();
    assert!(
        ran.iter().all(|c| !c.contains("pkill")),
        "nothing is killed on a computer without a pipe: {ran:?}"
    );
}
