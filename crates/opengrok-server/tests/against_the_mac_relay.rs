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
    let app = opengrok_server::router(agui, host);
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
        let (machine, token) = self.enrol(who, label).await;
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

/// THE STREAM: `ready` first on every connect, a ping every so often, and a second stream from
/// the same machine replaces the first, which is told so and closed. A token that is not a
/// daemon's opens nothing.
#[tokio::test]
async fn a_macs_stream_opens_with_ready_pings_and_is_replaced_by_its_next() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let mut first = h.mac(&ada, "Ada's MacBook").await;
    let ping = first.any().await.expect("a ping");
    assert_eq!(ping, json!({"type": "ping"}));

    let (status, mut second) = h.open(&first.token).await;
    assert_eq!(status, 200);
    let ready = second.recv().await.expect("ready");
    assert_eq!(ready, json!({"type": "ready", "machineId": first.machine}));
    // Within a deadline over the whole drain: a replaced stream left open pings for ever.
    let draining = async {
        let mut said = Vec::new();
        while let Some(frame) = first.frames.recv().await {
            said.push(frame);
        }
        said
    };
    let said = tokio::time::timeout(Duration::from_secs(20), draining).await;
    let said = said.expect("the replaced stream ends within 20 s");
    assert_eq!(said.last(), Some(&json!({"type": "replaced"})), "{said:?}");

    let (status, _) = h.open(&ada.token).await;
    assert_eq!(status, 401, "an access token is not a machine's");
    let (status, _) = h.open("not-a-token").await;
    assert_eq!(status, 401);
}

/// A TURN BY THE MAC IS THE MAC'S TO ANSWER. It is sent the very body the loopback door POSTs for
/// the same turn, with no address and no key anywhere, and its SSE is the run's text; the frame
/// says `via: "mac"`, and the gateway and the loopback proxy are never asked.
#[tokio::test]
async fn a_turn_by_the_mac_is_answered_by_the_mac_with_the_loopbacks_own_body() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let hello = || json!([user("m1", "what does my plan say?")]);

    // The loopback's body for this turn, as its door POSTs it.
    let body = json!({ "kind": "local_proxy", "via": "loopback", "baseUrl": h.proxy_url,
                       "localModel": "gpt-5.5" });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(&ada, &unique("thr"), &run_id(), hello(), props.clone());
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "from the loopback", "{frames:?}");
    let loopback = h.proxy.asked().pop().expect("the loopback was asked");

    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let run = run_id();
    let turn = h.turn(&ada, &unique("thr"), &run, hello(), props);
    let infer = mac.next().await;
    assert_eq!(infer["type"], "infer", "{infer}");
    assert_eq!(infer["runId"], run.as_str());
    assert_eq!(infer["model"], "gpt-5.5");
    assert_eq!(
        infer["request"], loopback,
        "the body the loopback door would POST"
    );
    assert_eq!(infer["request"]["stream"], true);
    let said = infer.to_string();
    for secret in [
        PROXY_KEY,
        h.proxy_url.as_str(),
        "baseUrl",
        "apiKey",
        "oag_live",
    ] {
        assert!(
            !said.contains(secret),
            "no url and no key down the stream: {said}"
        );
    }
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h
        .answer(
            &mac.token,
            id,
            "text/event-stream",
            sse(&words("from the mac")),
        )
        .await;
    assert_eq!(status, 204);
    let (status, frames) = turn.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(text_of(&frames), "from the mac", "{frames:?}");
    assert_eq!(ending(&frames)["type"], "RUN_FINISHED", "{frames:?}");
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})]
    );
    assert_eq!(h.gateway.asked().len(), 0, "never the gateway");
    assert_eq!(
        h.proxy.asked().len(),
        1,
        "the loopback only for its own turn"
    );
    let run = h.run(&run).await;
    assert_eq!(
        (run.inference_source, run.inference_via),
        (SourceKind::LocalProxy, Some(Via::Mac))
    );
}

/// NO MAC, NO FALL BACK: a turn by the Mac with none connected ends `relay_offline`, in words,
/// and nothing is asked anywhere, not the gateway and not the loopback.
#[tokio::test]
async fn with_no_mac_connected_a_turn_by_the_mac_ends_relay_offline_and_asks_nothing() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let props = json!({ "coworkerId": coworker });
    let (thread, run) = (unique("thr"), run_id());
    let turn = h.turn(&ada, &thread, &run, json!([user("m1", "hi")]), props);
    let (status, frames) = turn.await.unwrap();
    assert_eq!(status, 200);
    let end = ending(&frames);
    assert_eq!(end["type"], "RUN_ERROR", "{frames:?}");
    assert_eq!(end["code"], "relay_offline", "{end}");
    let said = end["message"].as_str().unwrap();
    assert!(
        said.contains("your Mac isn't connected") || said.contains("Your Mac isn't connected"),
        "{said}"
    );
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})]
    );
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));

    // THE CODE IS THE LOG'S: a replay, of the run or of its thread, ends on the same frame.
    let get = reqwest::Method::GET;
    let (_, replay) = h
        .send(
            Some(&ada.token),
            get.clone(),
            &format!("/ag-ui/runs/{run}"),
            None,
        )
        .await;
    let replayed = replay["events"].as_array().and_then(|events| events.last());
    assert_eq!(
        replayed.map(|frame| &frame["code"]),
        Some(&json!("relay_offline"))
    );
    let (_, replay) = h
        .send(
            Some(&ada.token),
            get,
            &format!("/ag-ui/threads/{thread}"),
            None,
        )
        .await;
    let events = replay["runs"][0]["events"].as_array().unwrap();
    assert_eq!(events.last().unwrap()["code"], "relay_offline", "{replay}");
}

/// A CALL IN FLIGHT OUTLIVES ITS STREAM: an answer is the machine's, by its daemon token, not the
/// connection's, so a call sent down a stream since replaced is answered all the same; its
/// cancel, when it comes, goes to the stream the machine holds now.
#[tokio::test]
async fn a_call_in_flight_when_its_stream_is_replaced_is_answered_all_the_same() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = mac.next().await;
    let (status, mut again) = h.open(&mac.token).await;
    assert_eq!(status, 200);
    assert_eq!(again.recv().await.unwrap()["type"], "ready");
    assert_eq!(mac.next().await, json!({"type": "replaced"}));
    let id = infer["requestId"].as_str().unwrap();
    let body = sse(&words("answered after the swap"));
    assert_eq!(
        h.answer(&mac.token, id, "text/event-stream", body).await.0,
        204
    );
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "answered after the swap", "{frames:?}");
}

/// A MAC THAT DOES NOT ANSWER IS TOLD TO CANCEL: no first byte, and a stream that goes quiet
/// halfway, each end the run `relay_timeout` and send the Mac `cancel` for that request.
#[tokio::test]
async fn a_mac_that_never_starts_or_goes_quiet_times_out_and_is_told_to_cancel() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });

    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props.clone(),
    );
    let infer = mac.next().await;
    let cancel = mac.next().await;
    assert_eq!(
        cancel,
        json!({"type": "cancel", "requestId": infer["requestId"]})
    );
    let (_, frames) = turn.await.unwrap();
    let end = ending(&frames);
    assert_eq!(
        (end["type"].clone(), end["code"].clone()),
        (json!("RUN_ERROR"), json!("relay_timeout"))
    );
    assert!(
        end["message"]
            .as_str()
            .unwrap()
            .contains("did not start answering"),
        "{end}"
    );
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h
        .answer(&mac.token, id, "text/event-stream", sse(&words("late")))
        .await;
    assert_eq!(status, 404, "a request given up on is expired");

    // Halfway: a first line, then nothing.
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = mac.next().await;
    let id = infer["requestId"].as_str().unwrap().to_string();
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::io::Error>>();
    let line = "data: {\"choices\":[{\"delta\":{\"content\":\"half an answer\"}}]}\n\n";
    tx.unbounded_send(Ok(line.as_bytes().to_vec())).unwrap();
    let answering = {
        let (client, base, token) = (h.client.clone(), h.base.clone(), mac.token.clone());
        let id = id.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/inference-relay/responses/{id}"))
                .bearer_auth(token)
                .header("content-type", "text/event-stream")
                .body(reqwest::Body::wrap_stream(rx))
                .send()
                .await
                .map(|res| res.status().as_u16())
        })
    };
    let cancel = mac.next().await;
    assert_eq!(cancel, json!({"type": "cancel", "requestId": id}));
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "half an answer", "{frames:?}");
    let end = ending(&frames);
    assert_eq!(end["code"], "relay_timeout", "{end}");
    assert!(
        end["message"]
            .as_str()
            .unwrap()
            .contains("stopped answering"),
        "{end}"
    );
    drop(tx);
    assert_eq!(answering.await.unwrap().unwrap(), 204);
    assert_eq!(h.gateway.asked().len(), 0);
}

/// THE MAC'S OWN WORDS REACH THE RUN: an `{error}` answer is `relay_failed`, its sentence as sent.
#[tokio::test]
async fn a_macs_error_reaches_the_run_in_its_own_words() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = mac.next().await;
    let why = "You have reached your plan's usage limit; it resets at 14:00.";
    let body = reqwest::Body::from(json!({ "error": why }).to_string());
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h.answer(&mac.token, id, "application/json", body).await;
    assert_eq!(status, 204);
    let (_, frames) = turn.await.unwrap();
    let end = ending(&frames);
    assert_eq!(end["type"], "RUN_ERROR", "{frames:?}");
    assert_eq!(end["code"], "relay_failed");
    assert_eq!(end["message"], why);
}

/// STOP REACHES THE MAC: a person stops a run while their Mac is answering it, the Mac is sent
/// `cancel` for that request, and the run ends stopped with what it had said.
#[tokio::test]
async fn stopping_a_run_mid_relay_cancels_the_call_on_the_mac() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let run = run_id();
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run,
        json!([user("m1", "write me a novel")]),
        props,
    );
    let infer = mac.next().await;
    let id = infer["requestId"].as_str().unwrap().to_string();
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::io::Error>>();
    let line = "data: {\"choices\":[{\"delta\":{\"content\":\"Chapter one.\"}}]}\n\n";
    tx.unbounded_send(Ok(line.as_bytes().to_vec())).unwrap();
    let answering = {
        let (client, base, token) = (h.client.clone(), h.base.clone(), mac.token.clone());
        let id = id.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/inference-relay/responses/{id}"))
                .bearer_auth(token)
                .header("content-type", "text/event-stream")
                .body(reqwest::Body::wrap_stream(rx))
                .send()
                .await
                .map(|res| res.status().as_u16())
        })
    };
    // The run has its first words before the person stops it; well inside the Mac's idle clock.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let path = format!("/ag-ui/runs/{run}/stop");
    let (status, stopped) = h
        .send(Some(&ada.token), reqwest::Method::POST, &path, None)
        .await;
    assert_eq!(status, 202, "{stopped}");
    let cancel = mac.next().await;
    assert_eq!(cancel, json!({"type": "cancel", "requestId": id}));
    let (_, frames) = turn.await.unwrap();
    assert!(
        frames.iter().any(|frame| frame["name"] == "run-stopped"),
        "{frames:?}"
    );
    assert_eq!(ending(&frames)["type"], "RUN_FINISHED", "{frames:?}");
    assert_eq!(h.run(&run).await.status, RunStatus::Stopped);
    drop(tx);
    assert_eq!(answering.await.unwrap().unwrap(), 204);
}

/// ONLY THE MACHINE IT WENT TO MAY ANSWER, AND ONCE: another machine of the same person's is
/// 401, as is a token that is not a daemon's; a second answer is 409; an id nothing waits on 404.
#[tokio::test]
async fn an_answer_is_refused_from_another_machine_twice_or_for_no_request() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut older = h.mac(&ada, "Ada's old Mac").await;
    let mut newer = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = newer.next().await;
    assert_eq!(infer["type"], "infer", "the newest stream is the one asked");
    let id = infer["requestId"].as_str().unwrap();
    let answer = |token: &str| {
        let token = token.to_string();
        let id = id.to_string();
        let h = &h;
        async move {
            h.answer(&token, &id, "text/event-stream", sse(&words("mine")))
                .await
        }
    };
    let (status, refused) = answer(&older.token).await;
    assert_eq!(status, 401, "{refused}");
    assert!(refused["error"].is_string(), "{refused}");
    assert_eq!(
        answer(&ada.token).await.0,
        401,
        "an access token is not a machine's"
    );
    assert_eq!(answer(&newer.token).await.0, 204);
    let (status, refused) = answer(&newer.token).await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        answer(&older.token).await.0,
        401,
        "and still not the other machine's"
    );
    let (status, refused) = h
        .answer(
            &newer.token,
            &uuid::Uuid::new_v4().to_string(),
            "text/event-stream",
            sse(""),
        )
        .await;
    assert_eq!(status, 404, "{refused}");
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "mine", "{frames:?}");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), older.next())
            .await
            .is_err(),
        "the older stream was sent nothing"
    );
}

/// A QUEUED SEND FOR AN ABSENT MAC IS HELD, NOT DROPPED OR RE-ROUTED: its snapshot says
/// `heldFor: "relay_offline"`, firing it answers 202 and leaves it queued, and when the Mac
/// connects the server sends it, as the person's own turn, to the Mac.
#[tokio::test]
async fn a_queued_send_held_for_an_absent_mac_drains_when_the_mac_reconnects() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let thread = unique("thr");
    let first = json!([user("m1", "first")]);
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let (status, frames) = h
        .turn(&ada, &thread, &run_id(), first, props)
        .await
        .unwrap();
    assert_eq!(
        (status, text_of(&frames).as_str()),
        (200, "from the gateway")
    );

    let path = format!("/ag-ui/threads/{thread}/pending");
    let body = json!({ "v": 1, "content": "then this, by my Mac", "clientMessageId": "m2",
                       "inferenceSource": { "kind": "local_proxy", "via": "mac" } });
    let post = reqwest::Method::POST;
    let (status, created) = h.send(Some(&ada.token), post, &path, Some(body)).await;
    assert_eq!(status, 201, "{created}");
    let queued = created["pendingUserMessage"].clone();
    assert_eq!(
        queued["inferenceSource"],
        json!({"kind": "local_proxy", "via": "mac"})
    );
    let pending_id = queued["id"].as_str().unwrap().to_string();

    let (_, listed) = h
        .send(Some(&ada.token), reqwest::Method::GET, &path, None)
        .await;
    assert_eq!(
        listed["pendingUserMessages"][0]["heldFor"], "relay_offline",
        "{listed}"
    );
    assert_eq!(
        listed["pendingEvents"][0]["value"]["message"]["heldFor"],
        "relay_offline"
    );

    let second = json!([user("m1", "first"), user("m2", "then this, by my Mac")]);
    let props = json!({ "coworkerId": coworker, "pendingId": pending_id });
    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .bearer_auth(&ada.token)
        .json(
            &json!({ "threadId": thread, "runId": run_id(), "messages": second,
                       "forwardedProps": props }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 202);
    let held: Value = res.json().await.unwrap();
    assert_eq!(held["heldFor"], "relay_offline", "{held}");
    assert_eq!(held["id"], pending_id.as_str());
    assert_eq!(
        held["event"]["value"]["message"]["heldFor"],
        "relay_offline"
    );
    let (_, listed) = h
        .send(Some(&ada.token), reqwest::Method::GET, &path, None)
        .await;
    assert_eq!(
        listed["pendingUserMessages"][0]["id"],
        pending_id.as_str(),
        "still queued"
    );

    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let infer = mac.next().await;
    assert_eq!(
        infer["type"], "infer",
        "the held send, sent by the server: {infer}"
    );
    let said = infer["request"]["messages"].to_string();
    assert!(
        said.contains("then this, by my Mac") && said.contains("first"),
        "{said}"
    );
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h
        .answer(
            &mac.token,
            id,
            "text/event-stream",
            sse(&words("done by the mac")),
        )
        .await;
    assert_eq!(status, 204);

    let mut replay = Value::Null;
    for _ in 0..100 {
        let thread_path = format!("/ag-ui/threads/{thread}");
        replay = h
            .send(Some(&ada.token), reqwest::Method::GET, &thread_path, None)
            .await
            .1;
        let runs = replay["runs"].as_array().map(Vec::len).unwrap_or_default();
        if runs == 2 && replay["runs"][1]["status"] == "finished" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(replay["runs"][1]["status"], "finished", "{replay}");
    assert_eq!(
        replay["pendingUserMessages"],
        json!([]),
        "drained: {replay}"
    );
    let events = replay["runs"][1]["events"].as_array().unwrap();
    // The replay draws the person's words too: theirs, then the Mac's answer.
    assert_eq!(text_of(events), "then this, by my Macdone by the mac");
    assert_eq!(
        sources(events),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})]
    );
    assert_eq!(h.gateway.asked().len(), 1, "only the first turn");
}

/// NO RUN IN SIGHT IS AN IDLE THREAD (review of #298): a send held on a thread whose one turn
/// was hidden, and one on a thread with no turn at all, each go when the Mac connects. The drain
/// read "no turn in sight" as "a turn in flight", and they waited for good.
#[tokio::test]
async fn held_sends_on_threads_with_no_turn_in_sight_go_when_the_mac_connects() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let (hidden, first) = (unique("thr"), run_id());
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let turn = h.turn(&ada, &hidden, &first, json!([user("m1", "first")]), props);
    assert_eq!(turn.await.unwrap().0, 200);
    let (path, post) = (
        format!("/ag-ui/threads/{hidden}/pending"),
        reqwest::Method::POST,
    );
    let body = json!({ "v": 1, "content": "behind a hidden turn", "clientMessageId": "m2" });
    let (status, queued) = h
        .send(Some(&ada.token), post.clone(), &path, Some(body))
        .await;
    assert_eq!(status, 201, "{queued}");
    let hide = format!("/ag-ui/runs/{first}/hide");
    assert_eq!(h.send(Some(&ada.token), post, &hide, None).await.0, 204);
    // No turn at all: the route queues only behind one, and the store is told directly.
    let (fresh, id) = (
        unique("thr"),
        opengrok_core::id::PendingUserMessageId::new(),
    );
    let first_words = opengrok_store::NewPendingUserMessage {
        id: id.as_str(),
        thread_id: &fresh,
        account_id: ada.id.as_str(),
        content: "the first words on a new thread",
        reply_to: None,
        recipe_id: None,
        recipe_values: None,
        skill_id: None,
        client_message_id: Some("m1"),
        inference_source: None,
    };
    let queued = h.store.enqueue_pending_user_message(first_words, now_ms());
    queued.await.expect("queued");

    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let mut asked = Vec::new();
    for _ in 0..2 {
        let infer = mac.next().await;
        assert_eq!(infer["type"], "infer", "{infer}");
        asked.push(infer["request"]["messages"].to_string());
        let id = infer["requestId"].as_str().unwrap();
        let body = sse(&words("sent by the mac"));
        assert_eq!(
            h.answer(&mac.token, id, "text/event-stream", body).await.0,
            204
        );
    }
    let said = |words: &str| asked.iter().any(|asked| asked.contains(words));
    assert!(said("behind a hidden turn") && said("the first words on a new thread"));
    for thread in [&hidden, &fresh] {
        let replay = h.settled(&ada, thread, 1).await;
        assert_eq!(replay["runs"][0]["status"], "finished", "{replay}");
        assert_eq!(replay["pendingUserMessages"], json!([]), "{replay}");
    }
}

/// A TURN IN FLIGHT IS WAITED OUT, NOT GIVEN UP ON (review of #298): the Mac connects while a
/// turn still runs on the thread, and its held send goes when that turn ends. The drain gave up
/// at a busy thread, and with the app closed nothing sent it after. The Mac's `Content-Type` is
/// read case-insensitively.
#[tokio::test]
async fn a_held_send_waits_out_the_turn_in_flight_when_the_mac_connects() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let thread = unique("thr");
    let holding = h.gateway.gate.clone().lock_owned().await;
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let running = h.turn(
        &ada,
        &thread,
        &run_id(),
        json!([user("m1", "first")]),
        props,
    );
    let path = format!("/ag-ui/threads/{thread}/pending");
    let body = json!({ "v": 1, "content": "after the turn in flight", "clientMessageId": "m2" });
    let mut status = 0;
    for _ in 0..100 {
        let post = reqwest::Method::POST;
        status = h
            .send(Some(&ada.token), post, &path, Some(body.clone()))
            .await
            .0;
        if status == 201 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, 201, "queued behind the turn in flight");

    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let early = tokio::time::timeout(Duration::from_millis(700), mac.next()).await;
    assert!(
        early.is_err(),
        "nothing goes while the turn runs: {early:?}"
    );
    drop(holding);
    let (_, frames) = running.await.unwrap();
    assert_eq!(text_of(&frames), "from the gateway", "{frames:?}");
    let infer = mac.next().await;
    assert_eq!(infer["type"], "infer", "{infer}");
    let said = infer["request"]["messages"].to_string();
    assert!(said.contains("after the turn in flight"), "{said}");
    let id = infer["requestId"].as_str().unwrap();
    let kind = "Text/Event-Stream; charset=utf-8";
    assert_eq!(
        h.answer(&mac.token, id, kind, sse(&words("then mine")))
            .await
            .0,
        204
    );
    let replay = h.settled(&ada, &thread, 2).await;
    assert_eq!(replay["runs"][1]["status"], "finished", "{replay}");
    let events = replay["runs"][1]["events"].as_array().unwrap();
    assert_eq!(text_of(events), "after the turn in flightthen mine");
    assert_eq!(replay["pendingUserMessages"], json!([]), "{replay}");
}

/// A RE-ENROLLED MAC'S OLD STREAM IS CLOSED (review of #298): re-enrolment rotates the daemon
/// token, and the stream the old one opened was still sent the person's turns, though no answer
/// from it was taken. It ends at once, sent nothing, and the old token opens nothing again; the
/// new token's stream carries the next turn.
#[tokio::test]
async fn re_enrolling_a_mac_closes_the_stream_its_old_token_opened() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut old = h.mac(&ada, "Ada's MacBook").await;
    let (post, path) = (reqwest::Method::POST, "/local-exec/daemon");
    let body = json!({ "label": "Ada's MacBook", "machineId": old.machine });
    let (status, again) = h.send(Some(&ada.token), post, path, Some(body)).await;
    assert_eq!(
        (status, &again["machineId"]),
        (200, &json!(old.machine)),
        "{again}"
    );

    let ending = async {
        let mut said = Vec::new();
        while let Some(frame) = old.frames.recv().await {
            said.push(frame);
        }
        said
    };
    let said = tokio::time::timeout(Duration::from_secs(5), ending).await;
    let said = said.expect("the old stream ends");
    assert!(said.iter().all(|frame| frame["type"] == "ping"), "{said:?}");
    assert_eq!(
        h.open(&old.token).await.0,
        401,
        "the old token opens nothing"
    );

    let (thread, props) = (unique("thr"), json!({ "coworkerId": coworker }));
    let turn = h.turn(
        &ada,
        &thread,
        &run_id(),
        json!([user("m1", "hi")]),
        props.clone(),
    );
    let (_, frames) = turn.await.unwrap();
    assert_eq!(
        frames.last().unwrap()["code"],
        "relay_offline",
        "{frames:?}"
    );

    let token = again["token"].as_str().unwrap().to_string();
    let (status, frames) = h.open(&token).await;
    assert_eq!(status, 200);
    let machine = old.machine.clone();
    let mut new = Mac {
        machine,
        token,
        frames,
    };
    assert_eq!(new.next().await["type"], "ready");
    let turn = h.turn(
        &ada,
        &thread,
        &run_id(),
        json!([user("m2", "again")]),
        props,
    );
    let infer = new.next().await;
    assert_eq!(infer["type"], "infer", "{infer}");
    let id = infer["requestId"].as_str().unwrap();
    let body = sse(&words("on the new token"));
    assert_eq!(
        h.answer(&new.token, id, "text/event-stream", body).await.0,
        204
    );
    assert_eq!(text_of(&turn.await.unwrap().1), "on the new token");
}

/// THE PICKER: with a Mac connected its models are listed beside the gateway's, `via: "mac"`,
/// kept to what a subscription may run, and `localProxy` says the relay is connected.
#[tokio::test]
async fn the_model_list_carries_the_macs_models_and_says_it_is_connected() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "via": "mac" });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let (status, away) = h
        .send(Some(&ada.token), reqwest::Method::GET, "/models", None)
        .await;
    assert_eq!(status, 200, "{away}");
    assert_eq!(
        away["localProxy"],
        json!({"healthy": false, "relayConnected": false})
    );

    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let listing = {
        let (client, base, token) = (h.client.clone(), h.base.clone(), ada.token.clone());
        tokio::spawn(async move {
            let res = client
                .get(format!("{base}/models"))
                .bearer_auth(token)
                .send()
                .await;
            res.unwrap().json::<Value>().await.unwrap()
        })
    };
    let asked = mac.next().await;
    assert_eq!(asked["type"], "models", "{asked}");
    let models = json!({"object": "list", "data": [
        {"id": "gpt-6-sol"}, {"id": "claude-sonnet-4.5"}, {"id": "xai/grok-4.7"}]});
    let id = asked["requestId"].as_str().unwrap();
    let body = reqwest::Body::from(models.to_string());
    assert_eq!(
        h.answer(&mac.token, id, "application/json", body).await.0,
        204
    );
    let listing = listing.await.unwrap();
    let mac_entries: Vec<Value> = listing["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["via"] == "mac")
        .cloned()
        .collect();
    assert_eq!(
        mac_entries,
        [
            json!({"id": "gpt-6-sol", "points": null, "source": "local_proxy", "via": "mac"}),
            json!({"id": "xai/grok-4.7", "points": null, "source": "local_proxy", "via": "mac"}),
        ]
    );
    assert_eq!(
        listing["localProxy"],
        json!({"healthy": false, "relayConnected": true})
    );
    assert!(
        listing["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["source"] == "gateway"),
        "{listing}"
    );
}

/// THE SETTING: `via` and the Mac's own model are saved and merged as the other fields are, the
/// model held to the same allowlist; `relay` reads back whether a Mac is connected and which;
/// `helper` is refused until it is built.
#[tokio::test]
async fn a_person_chooses_their_mac_and_reads_the_relay_back() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let body =
        json!({ "kind": "local_proxy", "via": "mac", "relay": { "localModel": "gpt-6-sol" } });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["via"], "mac");
    assert_eq!(
        saved["relay"],
        json!({"connected": false, "machineId": null, "machineLabel": null,
               "localModel": "gpt-6-sol"})
    );

    let mac = h.mac(&ada, "Ada's MacBook").await;
    let read = h.read(&ada).await;
    assert_eq!(
        read["relay"],
        json!({"connected": true, "machineId": mac.machine, "machineLabel": "Ada's MacBook",
               "localModel": "gpt-6-sol"})
    );

    // Absent keeps; the kind alone changes nothing else.
    let (_, kept) = h.set(&ada, json!({ "kind": "gateway" })).await;
    assert_eq!(
        (kept["via"].clone(), kept["relay"]["localModel"].clone()),
        (json!("mac"), json!("gpt-6-sol"))
    );
    for (body, field) in [
        (json!({ "kind": "local_proxy", "via": "helper" }), "via"),
        (
            json!({ "kind": "local_proxy", "via": "carrier-pigeon" }),
            "via",
        ),
        (
            json!({ "kind": "local_proxy", "relay": { "localModel": "claude-sonnet-4.5" } }),
            "relay.localModel",
        ),
        (
            json!({ "kind": "local_proxy", "relay": "gpt-6-sol" }),
            "relay",
        ),
    ] {
        let (status, refused) = h.set(&ada, body.clone()).await;
        assert_eq!(status, 400, "{body}: {refused}");
        let why = refused["error"].as_str().unwrap();
        assert!(why.starts_with(field), "{body}: {why}");
    }
    let (_, refused) = h
        .set(&ada, json!({ "kind": "local_proxy", "via": "helper" }))
        .await;
    assert!(
        refused["error"].as_str().unwrap().contains("not built yet"),
        "{refused}"
    );
    let read = h.read(&ada).await;
    assert_eq!(
        (read["via"].clone(), read["relay"]["localModel"].clone()),
        (json!("mac"), json!("gpt-6-sol")),
        "nothing refused was saved"
    );

    // `null` and `""` clear, back to the loopback and no model for the Mac.
    let body = json!({ "kind": "local_proxy", "via": null, "relay": { "localModel": "" } });
    let (_, cleared) = h.set(&ada, body).await;
    assert_eq!(cleared["via"], "loopback", "{cleared}");
    assert_eq!(cleared["relay"]["localModel"], Value::Null, "{cleared}");
}

/// A RESUMED RUN GOES ON AT THE MAC. It parks on a card while its Mac answers it; its person
/// moves their setting to the loopback while the card waits; the answer carries it on at the
/// Mac all the same, on the model it started on, and it says where it asks only the once.
#[tokio::test]
async fn a_run_carried_on_after_a_card_keeps_asking_the_mac() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let path = format!("/coworkers/{coworker}/approvals");
    let body = Some(json!({ "tools": ["shell"] }));
    let (status, set) = h
        .send(Some(&ada.token), reqwest::Method::POST, &path, body)
        .await;
    assert_eq!(status, 200, "shell needs a person's yes: {set}");
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;

    let run = run_id();
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run,
        json!([user("m1", "list it")]),
        props,
    );
    let infer = mac.next().await;
    let id = infer["requestId"].as_str().unwrap();
    let call = a_shell_call("call_ls", "ls");
    assert_eq!(
        h.answer(&mac.token, id, "text/event-stream", sse(&call))
            .await
            .0,
        204
    );
    let (_, frames) = turn.await.unwrap();
    let card = frames
        .iter()
        .find(|frame| frame["name"] == "run-awaiting-approval")
        .cloned()
        .unwrap_or_else(|| panic!("the run parked on a card: {frames:?}"));

    let body = json!({ "kind": "local_proxy", "via": "loopback" });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let answer = format!("/ag-ui/runs/{run}/answer");
    let body = Some(json!({ "call_id": card["callId"], "approved": true }));
    let (status, answered) = h
        .send(Some(&ada.token), reqwest::Method::POST, &answer, body)
        .await;
    assert_eq!(status, 200, "{answered}");
    let carried = mac.next().await;
    assert_eq!(
        carried["type"], "infer",
        "the carry-on asks the Mac: {carried}"
    );
    assert_eq!(carried["runId"], run.as_str());
    assert_eq!(carried["model"], "gpt-5.5");
    let id = carried["requestId"].as_str().unwrap();
    let done = words("listed on the mac");
    assert_eq!(
        h.answer(&mac.token, id, "text/event-stream", sse(&done))
            .await
            .0,
        204
    );
    let mut ended = None;
    for _ in 0..100 {
        let loaded = h.run(&run).await;
        if loaded.status.is_terminal() {
            ended = Some(loaded);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ended = ended.expect("the run ended in 10s");
    assert_eq!(ended.status, RunStatus::Finished, "{:?}", ended.failure);
    assert_eq!(ended.inference_via, Some(Via::Mac));
    assert_eq!(
        sources(&ended.emitted),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})],
        "said once, at the start"
    );
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}
