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
use opengrok_harness::{DeltaStream, MockDoor, ModelDelta, ModelDoor, ModelError, ModelRequest};
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
    resumes: Mutex<u32>,
    egress: Mutex<Option<EgressTunnel>>,
    last_egress_box: Mutex<Option<String>>,
    /// How long the guest takes to answer `/v1/info`, and how often it was asked.
    egress_takes: Mutex<std::time::Duration>,
    egress_asked: Mutex<u32>,
    /// The page the box reports in front of its browser; `None` is a box that cannot say, and
    /// an empty page one that says no browser page is open.
    front: Mutex<Option<String>>,
    /// The screen each action went to (#376), and the screens started ahead of a turn.
    screens: Mutex<Vec<opengrok_box::Screen>>,
    opened: Mutex<Vec<opengrok_box::Screen>>,
    /// The screen each screenshot was taken of.
    shot_screens: Mutex<Vec<opengrok_box::Screen>>,
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
        *self.resumes.lock().expect("resumes") += 1;
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
    async fn screen_url(
        &self,
        _box_id: &str,
        _screen: &opengrok_box::Screen,
    ) -> BoxResult<Option<String>> {
        Ok(Some("http://vnc.invalid".to_string()))
    }
    /// Every box a fill reaches is a password box, reported anew after each action: these
    /// tests are about who may fill and where; which box has focus is `fill_into_focus`'s own
    /// (opengrok-forms `tests/unit/user_form.rs`).
    async fn focused_field(
        &self,
        _box_id: &str,
        _screen: &opengrok_box::Screen,
    ) -> BoxResult<opengrok_box::Focus> {
        let seq = self.acts.lock().expect("acts").len() as u64;
        Ok(opengrok_box::Focus {
            kind: Some("password".to_string()),
            seq,
        })
    }
    async fn open_screen(&self, _box_id: &str, screen: &opengrok_box::Screen) -> BoxResult<()> {
        self.opened.lock().expect("opened").push(screen.clone());
        Ok(())
    }
    async fn active_tab_url(
        &self,
        _box_id: &str,
        _screen: &opengrok_box::Screen,
    ) -> BoxResult<Option<String>> {
        match self.front.lock().expect("front").clone() {
            None => Err(opengrok_box::BoxError::Unreachable("cannot say".into())),
            Some(url) if url.is_empty() => Ok(None),
            Some(url) => Ok(Some(url)),
        }
    }
    async fn screenshot(
        &self,
        _box_id: &str,
        screen: &opengrok_box::Screen,
    ) -> BoxResult<Screenshot> {
        *self.shots.lock().expect("shots") += 1;
        self.shot_screens
            .lock()
            .expect("shot screens")
            .push(screen.clone());
        Ok(Screenshot {
            mime: "image/png".to_string(),
            png_base64: "iVBORw0KGgo=".to_string(),
            width: 1280,
            height: 800,
        })
    }
    async fn act(
        &self,
        _box_id: &str,
        screen: &opengrok_box::Screen,
        action: &CuaAction,
    ) -> BoxResult<()> {
        self.acts.lock().expect("acts").push(action.clone());
        self.screens.lock().expect("screens").push(screen.clone());
        Ok(())
    }
    async fn egress_tunnel(&self, box_id: &str) -> Option<EgressTunnel> {
        *self.last_egress_box.lock().expect("egress box") = Some(box_id.to_string());
        *self.egress_asked.lock().expect("asked") += 1;
        let takes = *self.egress_takes.lock().expect("takes");
        tokio::time::sleep(takes).await;
        *self.egress.lock().expect("egress")
    }
    /// A guest that took the whole patience is one whose `/v1/info` wait timed out.
    async fn egress_probe(&self, box_id: &str) -> (Option<EgressTunnel>, bool) {
        let takes = *self.egress_takes.lock().expect("takes");
        let cap = self.egress_tunnel(box_id).await;
        (
            cap,
            cap.is_none() && takes >= opengrok_box::GUEST_INFO_PATIENCE,
        )
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

/// `visibility = org` through the aggregate, as the PATCH route would have stored it.
/// A saved login row for `owner`, written straight to the store: this harness runs without a
/// vault, and a fill's values come from the card, so no sealed secret is needed. Shared with
/// `bot` when one is named.
async fn seed_login(store: &PgStore, owner: &AccountId, id: &str, kind: &str, bot: Option<&str>) {
    seed_login_at(store, owner, id, kind, bot, "example.com").await;
}

/// A saved login kept for `origin`, the site a fill of it must be on.
async fn seed_login_at(
    store: &PgStore,
    owner: &AccountId,
    id: &str,
    kind: &str,
    bot: Option<&str>,
    origin: &str,
) {
    sqlx::query(
        "insert into site_login (id, account_id, origin, username, kind, created_at_ms, updated_at_ms)
         values ($1, $2, $5, $3, $4, 1, 1)",
    )
    .bind(id)
    .bind(owner.as_str())
    .bind(format!("{id}@example.com"))
    .bind(kind)
    .bind(origin)
    .execute(store.pool())
    .await
    .expect("seed a login");
    if let Some(bot) = bot {
        assert!(
            store
                .set_site_login_shared(owner, id, bot, true, 1)
                .await
                .expect("share")
        );
    }
}

async fn mark_org_visible(store: &PgStore, owner: &AccountId, id: &str) {
    use opengrok_core::coworker::{CoworkerCommand, CoworkerView, Visibility};
    let coworker_id = opengrok_core::id::CoworkerId::from_stored(id.to_string());
    let (loaded, seq) = store.load_coworker(&coworker_id).await.expect("load");
    let at_ms = now_ms();
    let events = loaded
        .decide(CoworkerCommand::SetVisibility {
            visibility: Visibility::Org,
            at_ms,
        })
        .expect("set visibility");
    let mut after = loaded.clone();
    for event in &events {
        after.apply(event);
    }
    let view = CoworkerView {
        id: coworker_id.clone(),
        name: after.name.clone(),
        model: after.model.clone(),
        box_id: after.box_id.clone(),
        retired: after.retired,
        members: after.members.clone(),
        updated_at_ms: at_ms,
        role: after.role.clone(),
        visibility: after.visibility,
        effort: after.effort,
        source: after.source,
    };
    store
        .append_coworker(&coworker_id, owner, seq, &events, &view)
        .await
        .expect("append");
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

async fn harness_with_door(database_url: &str, email: &str, door: Arc<dyn ModelDoor>) -> Harness {
    harness_on(database_url, email, door, None).await
}

/// A server over the database. Given an account it is that account's server again, with none
/// of the first one's memory — which is what a restart is.
async fn harness_on(
    database_url: &str,
    email: &str,
    door: Arc<dyn ModelDoor>,
    account: Option<AccountId>,
) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = match account {
        Some(account) => account,
        None => seed_account(&store, email).await,
    };
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
        self.turn_on(token, agent, &format!("gateway-{agent}"), prompt)
            .await
    }

    /// One turn on a named thread, the way NativeChat sends each conversation.
    async fn turn_on(&self, token: &str, agent: &str, thread: &str, prompt: &str) -> String {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "threadId": thread,
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

/// Calls `look_at_screen` on the Bot named in the person's message ("look at bea"), then answers.
struct LookDoor;

#[async_trait]
impl ModelDoor for LookDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let answered = request
            .messages
            .iter()
            .any(|message| message.as_text().contains("[tool "));
        let asked = request
            .messages
            .iter()
            .rev()
            .map(|message| message.as_text())
            .find_map(|text| text.strip_prefix("look at ").map(str::to_string))
            .unwrap_or_default();
        let deltas = if answered {
            vec![ModelDelta::Text("Looked.".into())]
        } else {
            let id = format!("look-{}", uuid::Uuid::now_v7().simple());
            let mut bot = asked.trim().to_string();
            if let Some(first) = bot.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            vec![
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: "look_at_screen".into(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: json!({ "bot": bot }).to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ]
        };
        Ok(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))))
    }
}

/// Asks once for a login whose card names `.0` as its site, then answers.
struct SiteLoginDoor(&'static str);

#[async_trait]
impl ModelDoor for SiteLoginDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let answered = request
            .messages
            .iter()
            .any(|message| message.as_text().contains("[tool "));
        let deltas = if answered {
            vec![ModelDelta::Text("Signed in.".into())]
        } else {
            let id = format!("form-{}", uuid::Uuid::now_v7().simple());
            vec![
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: "request_user_form".into(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: json!({
                        "title": "Log in to Facebook",
                        "fields": [
                            {"id": "email", "label": "Email", "type": "email", "required": true},
                            {"id": "password", "label": "Password", "type": "password", "required": true}
                        ],
                        "liveHost": self.0
                    })
                    .to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ]
        };
        Ok(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))))
    }
}

/// Records the actual request after Continue, before any history reconstruction can hide a leak.
struct CollectDoor {
    asked: Mutex<Vec<ModelRequest>>,
    resumed: tokio::sync::Notify,
}

impl CollectDoor {
    fn new() -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
            resumed: tokio::sync::Notify::new(),
        }
    }

    async fn first_resume(&self) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let notified = self.resumed.notified();
                if let Some(request) = self.asked.lock().expect("requests").get(1) {
                    return serde_json::to_string(&request.messages).expect("messages");
                }
                notified.await;
            }
        })
        .await
        .expect("model resumed within 10s")
    }
}

#[async_trait]
impl ModelDoor for CollectDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let first = {
            let mut asked = self.asked.lock().expect("requests");
            asked.push(request);
            asked.len() == 1
        };
        let deltas = if first {
            vec![
                ModelDelta::ToolCallStart {
                    id: "collect-1".into(),
                    name: "request_user_form".into(),
                },
                ModelDelta::ToolCallArgs {
                    id: "collect-1".into(),
                    delta: json!({
                        "collect": true, "title": "New tax profile", "fields": [
                            {"id": "email", "label": "Email", "type": "text"},
                            {"id": "password", "label": "Password", "type": "password"}
                        ]
                    })
                    .to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "collect-1".into(),
                },
            ]
        } else {
            self.resumed.notify_one();
            vec![ModelDelta::Text("Collection complete.".into())]
        };
        Ok(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))))
    }
}

/// `GET /ag-ui/approvals`, polled until a row for this run is waiting.
async fn wait_for_approval(h: &Harness, token: &str, run_id: &str) -> Value {
    for _ in 0..100 {
        let rows: Value = h
            .client
            .get(format!("{}/ag-ui/approvals", h.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("approvals")
            .json()
            .await
            .expect("approvals json");
        if let Some(row) = rows
            .as_array()
            .expect("the queue is an array")
            .iter()
            .find(|row| row["runId"] == json!(run_id))
        {
            return row.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("run {run_id} never reached the approvals queue");
}

/// The routine's row on `GET /schedules`, polled until its `lastRun` satisfies `done`.
async fn wait_for_routine(
    h: &Harness,
    token: &str,
    id: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let mut row = Value::Null;
    for _ in 0..100 {
        let rows: Value = h
            .client
            .get(format!("{}/schedules", h.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("schedules")
            .json()
            .await
            .expect("schedules json");
        row = rows
            .as_array()
            .expect("an array")
            .iter()
            .find(|row| row["id"] == json!(id))
            .cloned()
            .unwrap_or(Value::Null);
        if done(&row["lastRun"]) {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the routine's lastRun never settled: {row}");
}

/// Asks for a login on every fresh conversation, with a call id of its own each time — as a real
/// model does — and answers once its tool result is in. The stock mock reuses one call id, which
/// would make two parked forms indistinguishable by the one thing that tells them apart.
///
/// Two fresh conversations meet at a barrier before either asks, so both have been handed their
/// tools before either form exists. That is the race a real 9am routine and a person's chat run:
/// the screen hold a pending form raises (`tools_for_coworker`) only reaches runs that start after
/// it, so two runs already under way can both park on a form.
struct EachAskDoor {
    both_asking: tokio::sync::Barrier,
}

#[async_trait]
impl ModelDoor for EachAskDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let answered = request
            .messages
            .iter()
            .any(|message| message.as_text().contains("[tool "));
        let deltas = if answered {
            vec![ModelDelta::Text("Signed in; carrying on.".into())]
        } else {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(10), self.both_asking.wait())
                    .await;
            let id = format!("form-{}", uuid::Uuid::now_v7().simple());
            vec![
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: "request_user_form".into(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: json!({
                        "title": "Google account",
                        "fields": [
                            {"id": "email", "label": "Email", "type": "email", "required": true},
                            {"id": "password", "label": "Password", "type": "password", "required": true}
                        ],
                        "liveHost": "accounts.google.com"
                    })
                    .to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ]
        };
        Ok(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))))
    }
}

/// A model that does what the prompt names, with a FRESH call id every time: "sign in" raises a
/// form, "look" takes a screenshot, anything else is answered in words, and a round that can see
/// its tool result answers. The fixed-id mock stamps `mock-form-1` on every run's card, so two
/// runs' cards could not be told apart — and telling them apart is what these tests are about.
struct HoldDoor;

#[async_trait]
impl ModelDoor for HoldDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let last = request
            .messages
            .last()
            .map(|message| message.as_text().to_ascii_lowercase())
            .unwrap_or_default();
        let fresh = uuid::Uuid::now_v7().simple().to_string();
        let call = |id: String, name: &str, args: Value| {
            vec![
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: name.to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: args.to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ]
        };
        let deltas = if last.contains("[tool ") {
            vec![ModelDelta::Text("done".into())]
        } else if last.contains("sign in") {
            call(
                format!("form-{fresh}"),
                "request_user_form",
                json!({
                    "title": "Google account",
                    "fields": [
                        {"id": "email", "label": "Email", "type": "email", "required": true},
                        {"id": "password", "label": "Password", "type": "password", "required": true}
                    ],
                    "liveHost": "accounts.google.com"
                }),
            )
        } else if last.contains("look") {
            call(
                format!("look-{fresh}"),
                "computer",
                json!({"action": "screenshot"}),
            )
        } else {
            vec![ModelDelta::Text("noted".into())]
        };
        Ok(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))))
    }
}

fn thread(name: &str) -> String {
    format!("thr-{name}-{}", uuid::Uuid::now_v7())
}

async fn stored_card(h: &Harness, agent: &str, entry_id: &str) -> Value {
    h.store
        .find_gateway_entry(
            &opengrok_core::id::CoworkerId::from_stored(agent.to_string()),
            &h.account,
            entry_id,
        )
        .await
        .expect("load card")
        .expect("card row")
        .1
}

async fn parked(h: &Harness) -> Vec<opengrok_core::id::RunId> {
    h.store
        .awaiting_approval(&h.account)
        .await
        .expect("awaiting")
}

async fn status_of(h: &Harness, run: &opengrok_core::id::RunId) -> opengrok_core::run::RunStatus {
    h.store.load_run(run).await.expect("load run").0.status
}

/// Poll until the run reaches `want`, and say what it was when the wait ran out.
async fn wait_for_status(
    h: &Harness,
    run: &opengrok_core::id::RunId,
    want: opengrok_core::run::RunStatus,
) -> opengrok_core::run::RunStatus {
    for _ in 0..100 {
        if status_of(h, run).await == want {
            return want;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    status_of(h, run).await
}

/// Stop a run straight in the log, the way a stop written somewhere else lands: no route, so
/// nothing settles its card. That is how a stop left a card open before a stop closed it, and
/// how a card is left open when the process that stopped the run died before closing it.
async fn stop_in_store(h: &Harness, run_id: &opengrok_core::id::RunId) {
    use opengrok_core::run::{RunCommand, RunView};
    let (mut run, seq) = h.store.load_run(run_id).await.expect("load run");
    let at_ms = now_ms();
    let events = run
        .decide(RunCommand::Stop {
            by: h.account.to_string(),
            at_ms,
        })
        .expect("stop");
    for event in &events {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    h.store
        .append_run(run_id, seq, &events, &view, Some(&h.account))
        .await
        .expect("append stop");
}

fn holds_any(tail: &Value) -> bool {
    tail["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .any(opengrok_tools::user_form::holds_the_screen)
}

fn hold_ms() -> i64 {
    i64::try_from(opengrok_server::agui::user_form::FORM_HOLD_TIMEOUT.as_millis()).expect("ms")
}

/// One authenticated GET, as JSON.
async fn get_json(h: &Harness, token: &str, path: &str) -> Value {
    let res = h
        .client
        .get(format!("{}{path}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("get");
    assert_eq!(res.status().as_u16(), 200, "GET {path}");
    res.json().await.expect("json")
}

/// "Open the screen" on a form; the handoff's entry id.
async fn escalate(h: &Harness, token: &str, agent: &str, form_id: &str) -> String {
    let (status, body) = h
        .agui(
            token,
            "/ag-ui/user-form/dismiss",
            json!({ "entryId": form_id, "agentId": agent, "mode": "escalated" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    body["handoffEntryId"]
        .as_str()
        .expect("handoff id")
        .to_string()
}

/// The handoff's end, as NativeChat posts it from the handoff card.
async fn end_handoff(h: &Harness, token: &str, agent: &str, handoff_id: &str, ended: &str) {
    let (status, resolved) = h
        .agui(
            token,
            "/ag-ui/box-handoff/resolve",
            json!({ "entryId": handoff_id, "agentId": agent, "resolution": ended }),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(
        resolved["boxResolution"], ended,
        "the box's own reply: {resolved}"
    );
}

/// Every replayed frame that paints this form — its park, and its `user-form` CUSTOM when
/// `custom` — says how its handoff ended, beside `formResolution` and in `value`; `None` is a
/// handoff still live, which says nothing.
fn assert_frames_say(events: &Value, form_id: &str, ended: Option<&str>, custom: bool) {
    let frames: Vec<&Value> = events
        .as_array()
        .into_iter()
        .flatten()
        .filter(|frame| frame["type"] == "CUSTOM" && frame["entryId"] == form_id)
        .filter(|frame| {
            frame["name"] == "user-form"
                || (frame["name"] == "run-awaiting-approval" && frame["reason"] == "user-form")
        })
        .collect();
    assert!(
        frames.iter().any(|f| f["name"] == "run-awaiting-approval"),
        "the park replays: {events}"
    );
    assert_eq!(
        frames.iter().any(|f| f["name"] == "user-form"),
        custom,
        "{events}"
    );
    let yes = json!(true);
    let timed_out = (ended == Some("timed_out")).then_some(&yes);
    for frame in frames {
        assert_eq!(frame["formResolution"], "escalated", "{frame}");
        assert_eq!(
            frame.get("boxResolution"),
            ended.map(|word| json!(word)).as_ref(),
            "{frame}"
        );
        assert_eq!(
            frame["value"].get("boxResolution"),
            frame.get("boxResolution"),
            "{frame}"
        );
        assert_eq!(frame.get("timedOut"), timed_out, "{frame}");
        assert_eq!(frame["value"].get("timedOut"), timed_out, "{frame}");
    }
}

/// `GET /ag-ui/approvals` has no row for this run.
async fn assert_no_approval_row(h: &Harness, token: &str, run: &opengrok_core::id::RunId) {
    let rows = get_json(h, token, "/ag-ui/approvals").await;
    let rows = rows.as_array().expect("the queue is an array");
    assert!(
        !rows.iter().any(|row| row["runId"] == json!(run.as_str())),
        "a settled handoff's run is not waiting on anyone: {rows:?}"
    );
}

/// How a handoff is ended in `end_with_an_unreadable_log`.
#[derive(Clone, Copy)]
enum Ending {
    HandBackByHandoff,
    HandBackByForm,
    Skip,
}

async fn end_with_an_unreadable_log(database_url: &str, tag: &str, ending: Ending) {
    let email = format!("{tag}-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let form_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    let handoff_id = escalate(&h, &token, &agent, &form_id).await;
    let stream = opengrok_store::run_stream(&run);
    let original: Value =
        sqlx::query_scalar("select payload from events where stream_id = $1 and stream_seq = 1")
            .bind(&stream)
            .fetch_one(h.store.pool())
            .await
            .expect("first event");
    let rewrite = |payload: Value| {
        sqlx::query("update events set payload = $2 where stream_id = $1 and stream_seq = 1")
            .bind(stream.clone())
            .bind(payload)
            .execute(h.store.pool())
    };
    rewrite(json!({"unreadable": true}))
        .await
        .expect("break the log");

    let (route, post) = match ending {
        Ending::HandBackByHandoff => (
            "/ag-ui/box-handoff/resolve",
            json!({ "entryId": handoff_id, "agentId": agent, "resolution": "handed_back" }),
        ),
        Ending::HandBackByForm => (
            "/ag-ui/box-handoff/resolve",
            json!({ "entryId": form_id, "agentId": agent, "resolution": "handed_back" }),
        ),
        Ending::Skip => (
            "/ag-ui/user-form/dismiss",
            json!({ "entryId": form_id, "agentId": agent, "mode": "dismissed" }),
        ),
    };
    let (status, body) = h.agui(&token, route, post.clone()).await;
    assert_eq!(status, 503, "{body}");
    let handoff = stored_card(&h, &agent, &handoff_id).await;
    assert!(handoff.get("boxResolution").is_none(), "{handoff}");
    let stored = stored_card(&h, &agent, &form_id).await;
    assert!(stored.get("boxResolution").is_none(), "{stored}");

    rewrite(original).await.expect("mend the log");
    assert_eq!(
        status_of(&h, &run).await,
        opengrok_core::run::RunStatus::AwaitingApproval
    );
    let (status, body) = h.agui(&token, route, post).await;
    assert_eq!(
        status, 200,
        "once the log reads, the same post lands: {body}"
    );
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
    let word = match ending {
        Ending::Skip => "declined",
        Ending::HandBackByHandoff | Ending::HandBackByForm => "handed_back",
    };
    let stored = stored_card(&h, &agent, &form_id).await;
    assert_eq!(stored["boxResolution"], word, "{stored}");
}

/// A form handed back and its run finished, then unstamped: the form as a server before #143
/// left it. Returns the harness, a token, the coworker, the form's id and the run's replay path.
async fn a_form_handed_back_before_the_stamp(
    database_url: &str,
    tag: &str,
) -> (Harness, String, String, String, String) {
    let email = format!("{tag}-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let form_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    let handoff_id = escalate(&h, &token, &agent, &form_id).await;
    end_handoff(&h, &token, &agent, &handoff_id, "handed_back").await;
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
    // The form as a server before #143 left it.
    let (seq, mut old) = h
        .store
        .find_gateway_entry(&coworker, &h.account, &form_id)
        .await
        .expect("load")
        .expect("row");
    old.as_object_mut()
        .expect("an object")
        .remove("boxResolution");
    h.store
        .update_gateway_entry(&coworker, &h.account, seq, &old)
        .await
        .expect("unstamp the form");

    let path = format!("/ag-ui/runs/{}", run.as_str());
    (h, token, agent, form_id, path)
}

#[path = "against_user_form/doors.rs"]
mod doors;
#[path = "against_user_form/egress.rs"]
mod egress;
#[path = "against_user_form/handoffs.rs"]
mod handoffs;
#[path = "against_user_form/parks.rs"]
mod parks;
#[path = "against_user_form/replay.rs"]
mod replay;
#[path = "against_user_form/saved_logins.rs"]
mod saved_logins;
#[path = "against_user_form/stacked.rs"]
mod stacked;
#[path = "against_user_form/submit.rs"]
mod submit;
