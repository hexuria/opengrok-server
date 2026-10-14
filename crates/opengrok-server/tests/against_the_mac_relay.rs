//! The Mac relay (#292): a person's own subscription reached through their own Mac, which holds
//! `GET /inference-relay/requests` open with its daemon token and answers each frame on
//! `POST /inference-relay/responses/{requestId}`. A test Mac here plays NativeChat's part: it
//! enrols as local-exec does, reads the SSE, and answers with canned opencodex bodies.
//!
//! Stand-ins on 127.0.0.1 play the gateway and a loopback proxy, and keep every request, so a
//! test can say nothing reached them. The broker's clocks are the contract's, shortened. Needs
//! Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::inference::{SourceKind, Via};
use opengrok_core::run::RunStatus;
use opengrok_harness::GatewayDoor;
use opengrok_harness::relay::{Clocks, RelayBroker};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};
use tokio::sync::mpsc;

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

const DEPLOYMENT_KEY: &str = "oag_live_deployment_key_for_tests";
const PROXY_KEY: &str = "proxy-key-never-sent-to-a-mac";
const ROUTE: &str = "/account/inference-source";

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One streamed answer of words, as opencodex sends it.
fn words(text: &str) -> String {
    let chunk = json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

/// One streamed call of the box's shell.
fn a_shell_call(id: &str, command: &str) -> String {
    let arguments = json!({ "command": command }).to_string();
    let call = json!({"id": id, "index": 0, "type": "function",
                      "function": {"name": "shell", "arguments": arguments}});
    let chunk =
        json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

/// An OpenAI-compatible stand-in: what it answers, and every chat request it was sent. `gate`,
/// while a test holds it, keeps every answer back, so a turn stays in flight until it lets go.
#[derive(Clone, Default)]
struct StandIn {
    asked: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<VecDeque<String>>>,
    models: Arc<Value>,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl StandIn {
    fn asked(&self) -> Vec<Value> {
        self.asked.lock().unwrap().clone()
    }
}

async fn chat(
    State(seen): State<StandIn>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    seen.asked.lock().unwrap().push(body);
    let _open = seen.gate.lock().await;
    let reply = seen.replies.lock().unwrap().front().cloned().unwrap();
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        reply,
    )
}

async fn stand_in(reply: &str, models: &[&str]) -> (String, StandIn) {
    let data: Vec<Value> = models.iter().map(|id| json!({ "id": id })).collect();
    let seen = StandIn {
        replies: Arc::new(Mutex::new(VecDeque::from([words(reply)]))),
        models: Arc::new(json!({ "object": "list", "data": data })),
        ..StandIn::default()
    };
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/v1/models",
            get(|State(seen): State<StandIn>| async move { Json((*seen.models).clone()) }),
        )
        .route("/v1/chat/completions", post(chat))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// A computer that runs whatever it is given, so a shell card can be answered and carried on.
struct StubComputer;

#[async_trait]
impl Computer for StubComputer {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_relay_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _b: &str, _command: &str, _t: u32) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _b: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, b: &str, _p: &str) -> BoxResult<StartedCommand> {
        self.start(b, "").await
    }
    async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".to_string())
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
}

struct Person {
    id: AccountId,
    token: String,
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
    gateway: StandIn,
    proxy: StandIn,
    proxy_url: String,
    /// For driving the schedule sweep by hand.
    host: HostState,
}

/// The server, a gateway stand-in behind its door, a proxy stand-in on loopback, and the relay's
/// clocks short enough for a test to wait them out.
async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let (gateway_url, gateway) = stand_in("from the gateway", &["xai/grok-4.6"]).await;
    let (proxy_url, proxy) = stand_in("from the loopback", &["gpt-5.5"]).await;
    let minter = Arc::new(TokenMinter::new(b"mac-relay-test-secret"));
    let catalogue = opengrok_server::models::ModelCatalogue::new(&gateway_url, DEPLOYMENT_KEY);
    let mut auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string())
        .with_model_catalogue(Some(Arc::new(catalogue)))
        .with_gateway_admin(None);
    auth.relay = Arc::new(RelayBroker::new(Clocks {
        first_byte: Duration::from_secs(4),
        idle: Duration::from_secs(4),
        listing: Duration::from_secs(4),
        ping: Duration::from_millis(400),
    }));
    let door = GatewayDoor::new(&gateway_url, DEPLOYMENT_KEY);
    let door = opengrok_server::spend::GuardedDoor::new(Arc::new(door), store.clone(), None);
    let agui = AgUiState {
        auth,
        door: Arc::new(door),
        model: "xai/grok-4.6".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(Arc::new(StubComputer)),
        vault: Some(Arc::new(
            opengrok_store::Vault::from_base64_key("q7Q8b3yEc2w9Y1n4b5HkX0p6sT9eVbWzR2uJmLcDfAg=")
                .expect("vault"),
        )),
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui, host.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        store,
        minter,
        gateway,
        proxy,
        proxy_url,
        host,
    }
}

/// A Mac as NativeChat runs one: an enrolled machine's daemon token, and the frames its relay
/// stream is sent, one JSON per `data:` line.
struct Mac {
    machine: String,
    token: String,
    frames: mpsc::UnboundedReceiver<Value>,
}

impl Mac {
    /// The next frame, pings included; `None` once the stream has ended.
    async fn any(&mut self) -> Option<Value> {
        let next = tokio::time::timeout(Duration::from_secs(20), self.frames.recv());
        next.await.expect("a frame within 20 s")
    }

    /// The next frame that is not a ping, within 20 s IN ALL: pings come faster than any limit
    /// per frame, so a frame that never comes hung the test instead of failing it.
    async fn next(&mut self) -> Value {
        let skipping = async {
            loop {
                let frame = self.frames.recv().await.expect("the stream is still open");
                if frame["type"] != "ping" {
                    return frame;
                }
            }
        };
        let within = tokio::time::timeout(Duration::from_secs(20), skipping);
        within
            .await
            .expect("a frame that is not a ping within 20 s")
    }
}

impl Harness {
    async fn person(&self) -> Person {
        let id = AccountId::new();
        let email = format!("{}@og.local", unique("relay"));
        let at_ms = now_ms();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
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
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: None,
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&id, 0, &events, &view)
            .await
            .expect("append account");
        let now = chrono::Utc::now().timestamp();
        let token = self
            .minter
            .mint_access(id.as_str(), "sess-relay", &email, "ultra", now, 3600)
            .expect("mint access");
        Person { id, token }
    }

    async fn send(
        &self,
        bearer: Option<&str>,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let res = request.send().await.expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn set(&self, who: &Person, body: Value) -> (u16, Value) {
        self.send(Some(&who.token), reqwest::Method::PUT, ROUTE, Some(body))
            .await
    }

    async fn read(&self, who: &Person) -> Value {
        let (status, read) = self
            .send(Some(&who.token), reqwest::Method::GET, ROUTE, None)
            .await;
        assert_eq!(status, 200, "{read}");
        read
    }

    /// Their turns by their Mac, on `gpt-5.5`; the loopback proxy stored too, with a key, so a
    /// test can say neither ever reaches the Mac.
    async fn by_the_mac(&self, who: &Person) {
        let body = json!({ "kind": "local_proxy", "via": "mac", "baseUrl": self.proxy_url,
                           "localModel": "gpt-5.5", "apiKey": PROXY_KEY,
                           "relay": { "localModel": "gpt-5.5" } });
        let (status, saved) = self.set(who, body).await;
        assert_eq!(status, 200, "{saved}");
    }

    async fn hire(&self, who: &Person, name: &str) -> String {
        let body = Some(json!({ "name": name }));
        let (status, hired) = self
            .send(Some(&who.token), reqwest::Method::POST, "/coworkers", body)
            .await;
        assert_eq!(status, 201, "hire {name}: {hired}");
        hired["id"].as_str().expect("id").to_string()
    }

    /// Enrol a machine of `who`'s, as local-exec does, under `label`.
    async fn enrol(&self, who: &Person, label: &str) -> (String, String) {
        let body = Some(json!({ "label": label }));
        let path = "/local-exec/daemon";
        let (status, enrolled) = self
            .send(Some(&who.token), reqwest::Method::POST, path, body)
            .await;
        assert_eq!(status, 200, "{enrolled}");
        let machine = enrolled["machineId"].as_str().unwrap().to_string();
        (machine, enrolled["token"].as_str().unwrap().to_string())
    }

    /// Open a machine's relay stream: its HTTP status, and its frames as they arrive.
    async fn open(&self, token: &str) -> (u16, mpsc::UnboundedReceiver<Value>) {
        let res = self
            .client
            .get(format!("{}/inference-relay/requests", self.base))
            .bearer_auth(token)
            .send()
            .await
            .expect("the relay stream");
        let status = res.status().as_u16();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut body = res.bytes_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = body.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find("\n\n") {
                    let event: String = buffer.drain(..end + 2).collect();
                    for data in event.lines().filter_map(|line| line.strip_prefix("data: ")) {
                        let frame = serde_json::from_str(data).expect("one JSON per data line");
                        if tx.send(frame).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        (status, rx)
    }

    /// Enrol a Mac and hold its stream open, past its `ready`.
    async fn mac(&self, who: &Person, label: &str) -> Mac {
        self.hold(self.enrol(who, label).await).await
    }

    /// Hold an enrolled machine's stream open, past its `ready`: a Mac that was asleep wakes.
    async fn hold(&self, (machine, token): (String, String)) -> Mac {
        let (status, frames) = self.open(&token).await;
        assert_eq!(status, 200);
        let mut mac = Mac {
            machine,
            token,
            frames,
        };
        let ready = mac.next().await;
        assert_eq!(ready, json!({"type": "ready", "machineId": mac.machine}));
        mac
    }

    /// Answer a frame as a Mac: its status, and the body when it has one.
    async fn answer(&self, token: &str, id: &str, kind: &str, body: reqwest::Body) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/inference-relay/responses/{id}", self.base))
            .bearer_auth(token)
            .header("content-type", kind)
            .body(body)
            .send()
            .await
            .expect("an answer");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Start one turn on the AG-UI door; its frames arrive on the handle as the run streams them.
    fn turn(
        &self,
        who: &Person,
        thread: &str,
        run_id: &str,
        messages: Value,
        props: Value,
    ) -> tokio::task::JoinHandle<(u16, Vec<Value>)> {
        let request = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .bearer_auth(&who.token)
            .json(
                &json!({ "threadId": thread, "runId": run_id, "messages": messages,
                           "forwardedProps": props }),
            );
        tokio::spawn(async move {
            let res = request.send().await.expect("ag-ui turn");
            let status = res.status().as_u16();
            let text = res.text().await.expect("stream");
            let frames = text
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|frame| serde_json::from_str(frame).ok())
                .collect();
            (status, frames)
        })
    }

    async fn run(&self, run_id: &str) -> opengrok_core::run::Run {
        let id = RunId::from_stored(run_id.to_string());
        self.store.load_run(&id).await.expect("the run").0
    }

    /// A thread's replay once it shows `runs` turns, the last of them finished; within 10 s.
    async fn settled(&self, who: &Person, thread: &str, runs: usize) -> Value {
        let path = format!("/ag-ui/threads/{thread}");
        let mut replay = Value::Null;
        for _ in 0..100 {
            let get = reqwest::Method::GET;
            replay = self.send(Some(&who.token), get, &path, None).await.1;
            let seen = replay["runs"].as_array().map(Vec::len).unwrap_or_default();
            if seen == runs && replay["runs"][runs - 1]["status"] == "finished" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        replay
    }
}

fn user(id: &str, text: &str) -> Value {
    json!({ "id": id, "role": "user", "content": text })
}

fn sources(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter(|frame| frame["type"] == "CUSTOM" && frame["name"] == "opengrok.inferenceSource")
        .map(|frame| frame["value"].clone())
        .collect()
}

fn text_of(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|frame| frame["type"] == "TEXT_MESSAGE_CONTENT")
        .filter_map(|frame| frame["delta"].as_str())
        .collect()
}

fn ending(frames: &[Value]) -> Value {
    frames.last().cloned().unwrap_or(Value::Null)
}

fn sse(body: &str) -> reqwest::Body {
    reqwest::Body::from(body.to_string())
}

fn run_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Fire a queued send as its app does: its status and its JSON body (a 202 for a held one).
async fn fire(
    h: &Harness,
    who: &Person,
    thread: &str,
    messages: Value,
    props: Value,
) -> (u16, Value) {
    let body = json!({ "threadId": thread, "runId": run_id(), "messages": messages,
                       "forwardedProps": props });
    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .bearer_auth(&who.token)
        .json(&body);
    let res = res.send().await.expect("a fire");
    let status = res.status().as_u16();
    (status, res.json().await.unwrap_or(Value::Null))
}

/// ONE SWEEP AT A TIME in this binary's database: a tick claims every due routine in it, so one
/// test's tick could fire or skip another's through its own Mac. Held for the test.
async fn one_sweeper(database_url: &str) -> sqlx::PgConnection {
    use sqlx::Connection;
    let mut connection = sqlx::PgConnection::connect(database_url)
        .await
        .expect("connect for the sweep lock");
    sqlx::query("select pg_advisory_lock($1)")
        .bind(0x5eed_0316_i64)
        .execute(&mut connection)
        .await
        .expect("take the sweep lock");
    connection
}

const SKIPPED: &str = "Skipped: your computer was off, so your plan couldn't answer";

impl Harness {
    /// A routine for a Bot of `who`'s on their own plan (#316), weekly, so it is never due by
    /// itself: its id.
    async fn plan_routine(&self, who: &Person, kind: &str) -> String {
        let luna = self.hire(who, "Luna").await;
        let body = Some(json!({ "source": "local_proxy", "model": "gpt-6-luna" }));
        let path = format!("/coworkers/{luna}");
        let (status, row) = self
            .send(Some(&who.token), reqwest::Method::PATCH, &path, body)
            .await;
        assert_eq!(status, 200, "{row}");
        let body = json!({ "coworkerId": luna, "name": "Weekly", "kind": kind,
                           "prompt": "write the weekly report", "cron": "0 9 * * 1" });
        let post = reqwest::Method::POST;
        let (status, made) = self
            .send(Some(&who.token), post, "/schedules", Some(body))
            .await;
        assert_eq!(status, 201, "{made}");
        made["id"].as_str().unwrap().to_string()
    }

    /// Make the routine due a second ago, as the clock would, and run one tick of the sweep.
    async fn tick_when_due(&self, routine: &str) -> usize {
        sqlx::query("update schedule_view set next_due_ms = $2 where id = $1")
            .bind(routine)
            .bind(now_ms() - 1_000)
            .execute(self.store.pool())
            .await
            .expect("due now");
        opengrok_server::autonomy::sweep::schedule_tick(&self.host)
            .await
            .expect("a tick")
    }

    async fn routine_row(&self, who: &Person, routine: &str) -> Value {
        let get = reqwest::Method::GET;
        let (_, rows) = self.send(Some(&who.token), get, "/schedules", None).await;
        let rows = rows.as_array().cloned().unwrap_or_default();
        let row = rows.into_iter().find(|row| row["id"] == routine);
        row.expect("the routine")
    }

    async fn history(&self, who: &Person, routine: &str) -> Value {
        let path = format!("/schedules/{routine}/runs");
        let get = reqwest::Method::GET;
        let (status, rows) = self.send(Some(&who.token), get, &path, None).await;
        assert_eq!(status, 200, "{rows}");
        rows
    }

    async fn runs_of(&self, who: &Person, routine: &str) -> usize {
        let runs = self.store.runs_for_thread_owned_by(routine, &who.id, 10);
        runs.await.expect("runs").len()
    }
}

/// What a turn by the Mac is refused with while the relay is off and no fallback is set (#332).
const RELAY_OFF: &str = "Relay is off for your plan, so the turn was not sent. Turn Relay on, or \
     choose a Server model to answer while it is off, or switch this turn to the gateway.";

impl Harness {
    /// `who` switches their relay off (#332), with `fallback` as what answers in its place, or
    /// null; the way stays the Mac's.
    async fn relay_off(&self, who: &Person, fallback: Value) {
        let body =
            json!({ "kind": "local_proxy", "relayEnabled": false, "planFallback": fallback });
        let (status, saved) = self.set(who, body).await;
        assert_eq!(status, 200, "{saved}");
        let switched = (
            &saved["relayEnabled"],
            &saved["via"],
            &saved["planFallback"],
        );
        assert_eq!(
            switched,
            (&json!(false), &json!("mac"), &fallback),
            "{saved}"
        );
    }

    /// The Bot a routine wakes, as the store holds it.
    async fn routine_bot(&self, who: &Person, routine: &str) -> opengrok_core::coworker::Coworker {
        let row = self.routine_row(who, routine).await;
        let id = opengrok_core::id::CoworkerId::from_stored(row["coworkerId"].as_str().unwrap());
        self.store.load_coworker(&id).await.expect("the Bot").0
    }

    /// Whether `mac` was sent anything but pings within a moment: a turn it carries is an `infer`.
    async fn quiet(mac: &mut Mac) -> bool {
        let next = tokio::time::timeout(Duration::from_millis(600), mac.next());
        next.await.is_err()
    }

    /// What `mac`'s stream said, pings aside, until it ended: within 20 s.
    async fn said_until_closed(mac: &mut Mac) -> Vec<Value> {
        let draining = async {
            let mut said = Vec::new();
            while let Some(frame) = mac.frames.recv().await {
                if frame["type"] != "ping" {
                    said.push(frame);
                }
            }
            said
        };
        let said = tokio::time::timeout(Duration::from_secs(20), draining).await;
        said.expect("the stream ends within 20 s")
    }
}

/// What a computer's relay stream is refused with while its relay is off: the contract's words.
const RELAY_IS_OFF: &str = "Relay is off for this computer. Turn it on in Settings → Computer.";

impl Harness {
    /// `who`'s enrolled computers, as `GET /local-exec/daemon` lists them.
    async fn computers(&self, who: &Person) -> Vec<Value> {
        let get = reqwest::Method::GET;
        let (status, listed) = self
            .send(Some(&who.token), get, "/local-exec/daemon", None)
            .await;
        assert_eq!(status, 200, "{listed}");
        listed["machines"].as_array().cloned().unwrap_or_default()
    }

    /// `PATCH /local-exec/daemon/{machine}` with `body`, as `who`.
    async fn switch(&self, who: &Person, machine: &str, body: Value) -> (u16, Value) {
        let path = format!("/local-exec/daemon/{machine}");
        let patch = reqwest::Method::PATCH;
        self.send(Some(&who.token), patch, &path, Some(body)).await
    }

    /// A computer's switch as another replica's PATCH leaves it here: its row written, and the
    /// stream this replica holds for it still open.
    async fn switched_elsewhere(&self, who: &Person, machine: &str, on: bool) {
        let sql = "update local_exec_daemon set relay_enabled = $3
                   where account_id = $1 and machine_id = $2";
        let query = sqlx::query(sql)
            .bind(who.id.as_str())
            .bind(machine)
            .bind(on);
        query
            .execute(self.store.pool())
            .await
            .expect("switched elsewhere");
    }

    /// The answer a machine's relay stream is opened with when it is refused, within 10 s: a
    /// stream wrongly opened never ends, and would hang the test instead of failing it.
    async fn refused_a_stream(&self, token: &str) -> (u16, Value) {
        let get = reqwest::Method::GET;
        let opening = self.send(Some(token), get, "/inference-relay/requests", None);
        let opening = tokio::time::timeout(Duration::from_secs(10), opening).await;
        opening.expect("refused, not opened")
    }

    /// One turn of `coworker`'s by its person's Mac, answered by `mac` with `said`: its words.
    async fn carried(&self, who: &Person, coworker: &str, mac: &mut Mac, said: &str) -> String {
        let props = json!({ "coworkerId": coworker });
        let turn = self.turn(
            who,
            &unique("thr"),
            &run_id(),
            json!([user("m1", "hi")]),
            props,
        );
        let infer = mac.next().await;
        assert_eq!(infer["type"], "infer", "{infer}");
        let id = infer["requestId"].as_str().unwrap();
        let answered = self.answer(&mac.token, id, "text/event-stream", sse(&words(said)));
        assert_eq!(answered.await.0, 204);
        text_of(&turn.await.unwrap().1)
    }
}

#[path = "against_the_mac_relay/doors.rs"]
mod doors;
#[path = "against_the_mac_relay/fallback.rs"]
mod fallback;
#[path = "against_the_mac_relay/models.rs"]
mod models;
#[path = "against_the_mac_relay/queued.rs"]
mod queued;
#[path = "against_the_mac_relay/routines.rs"]
mod routines;
#[path = "against_the_mac_relay/stream.rs"]
mod stream;
#[path = "against_the_mac_relay/switches.rs"]
mod switches;
