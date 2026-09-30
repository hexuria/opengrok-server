//! Where a person's turns are answered (`/account/inference-source`): the gateway, or their own
//! subscription through an OpenAI-compatible proxy on this server's loopback (opencodex).
//!
//! Stand-ins on 127.0.0.1 play both, and keep every request each was sent: a thread moves from one
//! to the other and keeps its history, a turn's own word beats the account's, a run keeps the
//! source it started on across a card, `/models` lists both, and the proxy never sees a gateway
//! key. NEVER THE OWNER'S REAL PROXY: every address here is a stand-in's own ephemeral port, since
//! a call to 127.0.0.1:8080 would spend a real subscription. Needs Postgres; skips loudly without
//! OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::inference::SourceKind;
use opengrok_core::run::RunStatus;
use opengrok_harness::GatewayDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::{PgStore, Vault};
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

const KEK: &str = "q7Q8b3yEc2w9Y1n4b5HkX0p6sT9eVbWzR2uJmLcDfAg=";
const DEPLOYMENT_KEY: &str = "oag_live_deployment_key_for_tests";
const PROXY_KEY: &str = "proxy-key-never-shown";
const ROUTE: &str = "/account/inference-source";

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One streamed answer of words.
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

/// An OpenAI-compatible stand-in: what it answers, and every chat request it was sent.
#[derive(Clone, Default)]
struct StandIn {
    asked: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    /// The streams it answers with, in order; the last one again once the rest are spent.
    replies: Arc<Mutex<VecDeque<String>>>,
    models: Arc<Value>,
}

impl StandIn {
    fn asked(&self) -> Vec<(HeaderMap, Value)> {
        self.asked.lock().unwrap().clone()
    }
}

async fn chat(
    State(seen): State<StandIn>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    seen.asked.lock().unwrap().push((headers, body));
    let mut replies = seen.replies.lock().unwrap();
    let reply = if replies.len() > 1 {
        replies.pop_front().unwrap()
    } else {
        replies.front().cloned().unwrap()
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        reply,
    )
}

async fn stand_in(replies: Vec<String>, models: &[&str]) -> (String, StandIn) {
    let data: Vec<Value> = models.iter().map(|id| json!({ "id": id })).collect();
    let seen = StandIn {
        replies: Arc::new(Mutex::new(replies.into())),
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
        Ok(format!("bx_source_{}", uuid::Uuid::now_v7().simple()))
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

/// The server, with a gateway stand-in behind its door and its catalogue, and a proxy stand-in
/// answering `proxy_replies`. `vault` is whether it can keep a proxy's key.
async fn harness(database_url: &str, proxy_replies: Vec<String>, vault: bool) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let gateway_models = ["xai/grok-4.6", "openai/gpt-5.5"];
    let (gateway_url, gateway) = stand_in(vec![words("from the gateway")], &gateway_models).await;
    let served = [
        "gpt-5.5",
        "gpt-6-sol--fast",
        "claude-sonnet-4.5",
        "gemini-2.5-pro",
        "xai/grok-4.7",
        "llama-3.3-70b",
    ];
    let (proxy_url, proxy) = stand_in(proxy_replies, &served).await;
    let minter = Arc::new(TokenMinter::new(b"inference-source-test-secret"));
    let catalogue = opengrok_server::models::ModelCatalogue::new(&gateway_url, DEPLOYMENT_KEY);
    let auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string())
        .with_model_catalogue(Some(Arc::new(catalogue)))
        .with_gateway_admin(None);
    let door = GatewayDoor::new(&gateway_url, DEPLOYMENT_KEY);
    let door = opengrok_server::spend::GuardedDoor::new(Arc::new(door), store.clone(), None);
    let agui = AgUiState {
        auth,
        door: Arc::new(door),
        model: "xai/grok-4.6".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(Arc::new(StubComputer)),
        vault: vault.then(|| Arc::new(Vault::from_base64_key(KEK).expect("vault"))),
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

impl Harness {
    async fn person(&self) -> Person {
        let id = AccountId::new();
        let email = format!("{}@og.local", unique("source"));
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
            .mint_access(id.as_str(), "sess-source", &email, "ultra", now, 3600)
            .expect("mint access");
        Person { id, token }
    }

    async fn send(
        &self,
        who: Option<&Person>,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(who) = who {
            request = request.header("Authorization", format!("Bearer {}", who.token));
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

    async fn read(&self, who: &Person) -> (u16, Value) {
        self.send(Some(who), reqwest::Method::GET, ROUTE, None)
            .await
    }

    async fn set(&self, who: &Person, body: Value) -> (u16, Value) {
        self.send(Some(who), reqwest::Method::PUT, ROUTE, Some(body))
            .await
    }

    async fn models(&self, who: &Person, query: &str) -> (u16, Value) {
        let path = format!("/models{query}");
        self.send(Some(who), reqwest::Method::GET, &path, None)
            .await
    }

    /// The person's own proxy, saved as their setting under `kind`.
    async fn on_the_proxy(&self, who: &Person, kind: &str) {
        let body = json!({ "kind": kind, "baseUrl": self.proxy_url, "localModel": "gpt-5.5" });
        let (status, saved) = self.set(who, body).await;
        assert_eq!(status, 200, "{saved}");
    }

    async fn hire(&self, who: &Person, name: &str) -> String {
        let body = Some(json!({ "name": name }));
        let (status, hired) = self
            .send(Some(who), reqwest::Method::POST, "/coworkers", body)
            .await;
        assert_eq!(status, 201, "hire {name}: {hired}");
        hired["id"].as_str().expect("id").to_string()
    }

    /// One turn on the AG-UI door, as the app sends it: its HTTP status and every frame streamed.
    async fn turn(
        &self,
        who: &Person,
        thread: &str,
        messages: Value,
        props: Value,
    ) -> (u16, Vec<Value>) {
        let run_id = uuid::Uuid::now_v7().to_string();
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("Authorization", format!("Bearer {}", who.token))
            .json(&json!({
                "threadId": thread,
                "runId": run_id,
                "messages": messages,
                "forwardedProps": props,
            }))
            .send()
            .await
            .expect("ag-ui turn");
        let status = res.status().as_u16();
        let text = res.text().await.expect("stream");
        let frames = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|frame| serde_json::from_str(frame).ok())
            .collect();
        (status, frames)
    }

    async fn replay(&self, who: &Person, thread: &str) -> Value {
        let path = format!("/ag-ui/threads/{thread}");
        let (status, body) = self
            .send(Some(who), reqwest::Method::GET, &path, None)
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }
}

fn user(id: &str, text: &str) -> Value {
    json!({ "id": id, "role": "user", "content": text })
}

/// The `opengrok.inferenceSource` frames among `frames`, by value.
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

/// Every message's content a stand-in was asked with, joined.
fn conversation(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|message| message["content"].to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ids of one source's entries in a `/models` reply, in order.
fn ids(listing: &Value, source: &str) -> Vec<String> {
    listing["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["source"] == source)
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn a_person_is_on_the_gateway_until_they_choose_their_own_subscription() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let ada = h.person().await;
    let (status, read) = h.read(&ada).await;
    assert_eq!(status, 200, "{read}");
    assert_eq!(
        read,
        json!({"kind": "gateway", "baseUrl": null, "localModel": null, "healthy": false,
               "hasApiKey": false}),
        "the default, whole: never a 404 and never an empty body"
    );
}

#[tokio::test]
async fn a_person_saves_their_own_proxy_and_reads_it_back_healthy() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "baseUrl": format!("{}/", h.proxy_url),
                       "localModel": "gpt-5.5" });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(status, 200, "{saved}");
    let expected = json!({"kind": "local_proxy", "baseUrl": h.proxy_url, "localModel": "gpt-5.5",
                          "healthy": true, "hasApiKey": false});
    assert_eq!(
        saved, expected,
        "the PUT answers as GET does, its trailing slash gone"
    );
    assert_eq!(h.read(&ada).await.1, expected);

    // Merge: an absent field is kept, and `null` clears one.
    let (status, kept) = h.set(&ada, json!({ "kind": "gateway" })).await;
    assert_eq!(status, 200, "{kept}");
    assert_eq!(kept["baseUrl"], json!(h.proxy_url), "{kept}");
    assert_eq!(kept["localModel"], "gpt-5.5");
    assert_eq!(kept["healthy"], true, "probed whatever the kind: {kept}");
    let body = json!({ "kind": "local_proxy", "localModel": null });
    let (_, cleared) = h.set(&ada, body).await;
    assert_eq!(cleared["localModel"], Value::Null, "{cleared}");
    assert_eq!(
        cleared["kind"], "local_proxy",
        "a proxy with no model is still saved"
    );
    let body = json!({ "kind": "local_proxy", "baseUrl": null });
    let (_, gone) = h.set(&ada, body).await;
    assert_eq!(gone["baseUrl"], Value::Null, "{gone}");
    assert_eq!(
        gone["healthy"], false,
        "no address is never healthy: {gone}"
    );

    // Every loopback spelling this machine answers to.
    for base in [
        "http://127.5.5.5:9",
        "http://[::1]:9",
        "http://localhost",
        "http://localhost:9",
    ] {
        let body = json!({ "kind": "gateway", "baseUrl": base });
        let (status, saved) = h.set(&ada, body).await;
        assert_eq!(status, 200, "{base}: {saved}");
        assert_eq!(saved["baseUrl"], base, "{saved}");
    }
}

#[tokio::test]
async fn an_address_off_this_machine_is_refused_whatever_the_kind() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "baseUrl": "http://10.0.0.5:8080" });
    let (status, refused) = h.set(&ada, body).await;
    assert_eq!(status, 400, "{refused}");
    let why = refused["error"].as_str().unwrap_or_default();
    assert!(
        why.starts_with("baseUrl: ") && why.contains("this server's own machine"),
        "{why}"
    );
    for base in [
        "http://169.254.169.254/latest",
        "http://example.com",
        "http://localhost.evil.com:8080",
        "http://a@127.0.0.1",
        "file:///etc/passwd",
        "ftp://127.0.0.1",
    ] {
        let body = json!({ "kind": "gateway", "baseUrl": base });
        let (status, refused) = h.set(&ada, body).await;
        assert_eq!(status, 400, "{base}: {refused}");
        assert!(refused["error"].is_string(), "{refused}");
    }
    for kind in [json!("local-proxy"), json!(null), json!(7)] {
        let (status, refused) = h.set(&ada, json!({ "kind": kind })).await;
        assert_eq!(status, 400, "{kind}: {refused}");
        assert_eq!(
            refused,
            json!({ "error": "kind must be \"gateway\" or \"local_proxy\"" })
        );
    }
    let (status, refused) = h.set(&ada, json!({ "baseUrl": h.proxy_url })).await;
    assert_eq!(status, 400, "no kind is no setting: {refused}");
    let read = h.read(&ada).await.1;
    assert_eq!(read["kind"], "gateway", "nothing refused was saved: {read}");
    assert_eq!(read["baseUrl"], Value::Null, "{read}");
}

#[tokio::test]
async fn a_model_whose_terms_forbid_it_is_refused_by_name() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "baseUrl": h.proxy_url,
                       "localModel": "claude-sonnet-4.5" });
    let (status, refused) = h.set(&ada, body).await;
    assert_eq!(status, 400, "{refused}");
    let why = refused["error"].as_str().unwrap_or_default();
    assert!(
        why.starts_with("localModel: ") && why.contains("Anthropic's terms forbid"),
        "{why}"
    );
    for model in ["google/gemini-2.5-pro", "llama-3.3-70b", "openai/grok-4.7"] {
        let body = json!({ "kind": "gateway", "localModel": model });
        let (status, refused) = h.set(&ada, body).await;
        assert_eq!(status, 400, "{model}: {refused}");
    }
    let read = h.read(&ada).await.1;
    assert_eq!(
        read["kind"], "gateway",
        "a refused body saves none of itself: {read}"
    );
    assert_eq!(read["baseUrl"], Value::Null, "{read}");
}

#[tokio::test]
async fn a_signed_out_caller_is_refused_the_inference_source() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let (status, refused) = h.send(None, reqwest::Method::GET, ROUTE, None).await;
    assert_eq!(status, 401, "{refused}");
    assert!(
        refused["error"].is_string(),
        "a sentence, as JSON: {refused}"
    );
    let body = Some(json!({ "kind": "gateway" }));
    let (status, refused) = h.send(None, reqwest::Method::PUT, ROUTE, body).await;
    assert_eq!(status, 401, "{refused}");
    assert!(refused["error"].is_string(), "{refused}");
}

/// THE KEY IS SEALED, AND NEVER COMES BACK: not in a reply, not in the account's log. Absent keeps
/// it; `""` and `null` clear it.
#[tokio::test]
async fn a_proxy_key_is_sealed_and_never_read_back() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "baseUrl": h.proxy_url, "localModel": "gpt-5.5",
                       "apiKey": PROXY_KEY });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["hasApiKey"], true, "{saved}");
    assert!(!saved.to_string().contains(PROXY_KEY), "{saved}");
    let read = h.read(&ada).await.1;
    assert_eq!(read["hasApiKey"], true);
    assert!(!read.to_string().contains(PROXY_KEY), "{read}");
    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    let logged: Vec<Value> = sqlx::query_scalar("select payload from events where stream_id = $1")
        .bind(format!("account/{}", ada.id.as_str()))
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(
        logged
            .iter()
            .all(|event| !event.to_string().contains(PROXY_KEY)),
        "the log says a key exists, never what it is"
    );

    let (_, kept) = h.set(&ada, json!({ "kind": "local_proxy" })).await;
    assert_eq!(kept["hasApiKey"], true, "absent keeps the key: {kept}");
    for clear in [json!(""), json!(null)] {
        let body = json!({ "kind": "local_proxy", "apiKey": PROXY_KEY });
        assert_eq!(h.set(&ada, body).await.1["hasApiKey"], true);
        let body = json!({ "kind": "local_proxy", "apiKey": clear });
        let (status, cleared) = h.set(&ada, body).await;
        assert_eq!(status, 200, "{cleared}");
        assert_eq!(cleared["hasApiKey"], false, "{clear} clears it: {cleared}");
    }
}

#[tokio::test]
async fn a_proxy_key_with_no_vault_to_keep_it_is_refused() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], false).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "baseUrl": h.proxy_url, "apiKey": PROXY_KEY });
    let (status, refused) = h.set(&ada, body).await;
    assert_eq!(status, 503, "{refused}");
    let why = refused["error"].as_str().unwrap_or_default();
    assert!(
        why.contains("no vault") && why.contains("nothing was saved"),
        "{why}"
    );
    assert_eq!(h.read(&ada).await.1["kind"], "gateway", "nothing was saved");
    // Without a key there is nothing to keep, and the setting saves.
    let body = json!({ "kind": "local_proxy", "baseUrl": h.proxy_url });
    assert_eq!(h.set(&ada, body).await.0, 200);
}

/// BOTH KINDS, WHENEVER AN ADDRESS IS STORED, whatever the setting's kind: the gateway's entries,
/// tagged, and the proxy's allowed ones beside them; `?source=` narrows to one kind, and
/// `localProxy.healthy` says whether the proxy answered.
#[tokio::test]
async fn the_model_list_carries_both_sources_when_a_proxy_is_stored() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let ada = h.person().await;
    let (status, plain) = h.models(&ada, "").await;
    assert_eq!(status, 200, "{plain}");
    assert!(
        plain.get("localProxy").is_none(),
        "no address, no proxy: {plain}"
    );
    assert_eq!(ids(&plain, "gateway"), ["xai/grok-4.6", "openai/gpt-5.5"]);

    h.on_the_proxy(&ada, "gateway").await;
    let (status, both) = h.models(&ada, "").await;
    assert_eq!(status, 200, "{both}");
    assert_eq!(ids(&both, "gateway"), ["xai/grok-4.6", "openai/gpt-5.5"]);
    assert_eq!(
        ids(&both, "local_proxy"),
        ["gpt-5.5", "gpt-6-sol--fast", "xai/grok-4.7"],
        "Anthropic, Google and the unrecognised are never offered: {both}"
    );
    assert_eq!(both["localProxy"], json!({ "healthy": true }));
    let entry = both["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "local_proxy")
        .cloned();
    assert_eq!(
        entry,
        Some(json!({"id": "gpt-5.5", "points": null, "source": "local_proxy"})),
        "the gateway's entry shape, and its source"
    );

    let (_, gateway_only) = h.models(&ada, "?source=gateway").await;
    assert_eq!(ids(&gateway_only, "gateway").len(), 2, "{gateway_only}");
    assert!(
        ids(&gateway_only, "local_proxy").is_empty(),
        "{gateway_only}"
    );
    let (_, local_only) = h.models(&ada, "?source=local_proxy").await;
    assert!(ids(&local_only, "gateway").is_empty(), "{local_only}");
    assert_eq!(ids(&local_only, "local_proxy").len(), 3, "{local_only}");
    let (status, refused) = h.models(&ada, "?source=proxy").await;
    assert_eq!(status, 400, "{refused}");
}

#[tokio::test]
async fn a_stored_proxy_that_is_unreachable_leaves_the_gateway_list_whole() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("ok")], true).await;
    let ada = h.person().await;
    // Nothing listens on port 1.
    let body = json!({ "kind": "local_proxy", "baseUrl": "http://127.0.0.1:1",
                       "localModel": "gpt-5.5" });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let (status, listing) = h.models(&ada, "").await;
    assert_eq!(
        status, 200,
        "never a 5xx over the gateway's list: {listing}"
    );
    assert_eq!(ids(&listing, "gateway"), ["xai/grok-4.6", "openai/gpt-5.5"]);
    assert_eq!(
        listing["models"].as_array().map(Vec::len),
        Some(2),
        "{listing}"
    );
    assert_eq!(listing["localProxy"], json!({ "healthy": false }));
    assert_eq!(h.read(&ada).await.1["healthy"], false);
}

/// THE WHOLE FEATURE IN ONE THREAD. Turn 1 on the gateway; the person chooses their own proxy;
/// turn 2 on the same thread goes there, asked with turn 1's words and answer (rebuilt from the
/// log), with the person's key in the one header the proxy reads and no gateway key anywhere. Each
/// turn says where it asked, right after `RUN_STARTED`, live and on replay.
#[tokio::test]
async fn a_thread_moves_from_the_gateway_to_the_persons_own_proxy_and_keeps_its_history() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("from the proxy")], true).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let thread = unique("thr");
    let props = json!({ "coworkerId": coworker });

    let first = json!([user("m1", "what is the first answer?")]);
    let (status, frames) = h.turn(&ada, &thread, first, props.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(frames[0]["type"], "RUN_STARTED", "{frames:?}");
    assert_eq!(
        frames[1]["name"], "opengrok.inferenceSource",
        "right after it"
    );
    assert_eq!(
        sources(&frames),
        [json!({"kind": "gateway", "model": "xai/grok-4.6"})]
    );
    assert_eq!(text_of(&frames), "from the gateway");

    let body = json!({ "kind": "local_proxy", "baseUrl": h.proxy_url, "localModel": "gpt-5.5",
                       "apiKey": PROXY_KEY });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let second = json!([
        user("m1", "what is the first answer?"),
        { "id": "a1", "role": "assistant", "content": "from the gateway" },
        user("m2", "and the second?"),
    ]);
    let (status, frames) = h.turn(&ada, &thread, second, props).await;
    assert_eq!(status, 200);
    assert_eq!(frames[1]["name"], "opengrok.inferenceSource", "{frames:?}");
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "model": "gpt-5.5"})]
    );
    assert_eq!(text_of(&frames), "from the proxy");
    assert_eq!(ending(&frames)["type"], "RUN_FINISHED", "{frames:?}");

    assert_eq!(
        h.gateway.asked().len(),
        1,
        "the gateway answered turn 1 only"
    );
    let asked = h.proxy.asked();
    assert_eq!(asked.len(), 1, "the proxy answered turn 2");
    let (headers, body) = &asked[0];
    assert_eq!(body["model"], "gpt-5.5");
    let said = conversation(body);
    for line in [
        "what is the first answer?",
        "from the gateway",
        "and the second?",
    ] {
        assert!(
            said.contains(line),
            "turn 2 was asked with {line:?}: {said}"
        );
    }
    let key = headers.get("x-opencodex-api-key");
    assert_eq!(key.and_then(|v| v.to_str().ok()), Some(PROXY_KEY));
    assert!(headers.get("authorization").is_none(), "no bearer at all");
    assert!(
        !format!("{headers:?}{body}").contains("oag_live"),
        "no gateway key"
    );

    let replay = h.replay(&ada, &thread).await;
    let runs = replay["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 2, "{replay}");
    let journaled: Vec<Vec<Value>> = runs
        .iter()
        .map(|run| sources(run["events"].as_array().unwrap()))
        .collect();
    assert_eq!(
        journaled,
        [
            vec![json!({"kind": "gateway", "model": "xai/grok-4.6"})],
            vec![json!({"kind": "local_proxy", "model": "gpt-5.5"})],
        ]
    );
    let second_run = RunId::from_stored(runs[1]["runId"].as_str().unwrap().to_string());
    let (run, _) = h.store.load_run(&second_run).await.unwrap();
    assert_eq!(
        run.inference_source,
        SourceKind::LocalProxy,
        "captured on its start"
    );
}

/// The turn's own word beats the account's, both ways.
#[tokio::test]
async fn a_turns_own_source_beats_the_accounts() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("from the proxy")], true).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let hi = || json!([user("m1", "hi")]);

    h.on_the_proxy(&ada, "local_proxy").await;
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let (_, frames) = h.turn(&ada, &unique("thr"), hi(), props).await;
    assert_eq!(text_of(&frames), "from the gateway", "{frames:?}");
    assert_eq!(sources(&frames)[0]["kind"], "gateway");

    h.on_the_proxy(&ada, "gateway").await;
    let props = json!({ "coworkerId": coworker, "inferenceSource": "local_proxy" });
    let (_, frames) = h.turn(&ada, &unique("thr"), hi(), props).await;
    assert_eq!(text_of(&frames), "from the proxy", "{frames:?}");
    assert_eq!(sources(&frames)[0]["kind"], "local_proxy");
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (1, 1));

    let props = json!({ "coworkerId": coworker, "inferenceSource": "local-proxy" });
    let (status, _) = h.turn(&ada, &unique("thr"), hi(), props).await;
    assert_eq!(
        status, 400,
        "a word we do not know is refused, never read as either"
    );
}

/// NEVER A SILENT FALL BACK TO THE GATEWAY: a turn that asks for the proxy with no address, or no
/// model, ends in a sentence that says which, and nothing is asked anywhere.
#[tokio::test]
async fn a_turn_that_names_the_proxy_with_nothing_set_ends_in_words() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("from the proxy")], true).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let props = json!({ "coworkerId": coworker, "inferenceSource": "local_proxy" });
    let hi = || json!([user("m1", "hi")]);

    let (status, frames) = h.turn(&ada, &unique("thr"), hi(), props.clone()).await;
    assert_eq!(status, 200);
    let end = ending(&frames);
    assert_eq!(end["type"], "RUN_ERROR", "{frames:?}");
    let said = end["message"].as_str().unwrap_or_default();
    assert!(said.contains("no proxy address is set"), "{said}");
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "model": ""})]
    );

    let body = json!({ "kind": "local_proxy", "baseUrl": h.proxy_url });
    assert_eq!(
        h.set(&ada, body).await.0,
        200,
        "a proxy with no model is accepted"
    );
    let (_, frames) = h.turn(&ada, &unique("thr"), hi(), props).await;
    let said = ending(&frames)["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        said.starts_with("Choose a model for your own subscription first"),
        "{said}"
    );
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}

/// A RUN KEEPS THE SOURCE IT STARTED ON. It parks on a card at the proxy; its person moves their
/// setting back to the gateway while the card waits; the answer carries it on at the proxy all the
/// same, on the model it started on, and it says where it asks only the once.
#[tokio::test]
async fn a_run_answered_after_the_source_changed_keeps_the_source_it_started_with() {
    let database_url = database_or_skip!();
    let replies = vec![a_shell_call("call_ls", "ls"), words("done on the proxy")];
    let h = harness(&database_url, replies, true).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let path = format!("/coworkers/{coworker}/approvals");
    let body = Some(json!({ "tools": ["shell"] }));
    let (status, set) = h.send(Some(&ada), reqwest::Method::POST, &path, body).await;
    assert_eq!(status, 200, "shell needs a person's yes: {set}");
    h.on_the_proxy(&ada, "local_proxy").await;

    let props = json!({ "coworkerId": coworker });
    let messages = json!([user("m1", "list it")]);
    let (_, frames) = h.turn(&ada, &unique("thr"), messages, props).await;
    let card = frames
        .iter()
        .find(|frame| frame["name"] == "run-awaiting-approval")
        .cloned()
        .unwrap_or_else(|| panic!("the run parked on a card: {frames:?}"));
    let run_id = RunId::from_stored(card["runId"].as_str().unwrap().to_string());

    h.on_the_proxy(&ada, "gateway").await;
    let path = format!("/ag-ui/runs/{}/answer", run_id.as_str());
    let body = Some(json!({ "call_id": card["callId"], "approved": true }));
    let (status, answered) = h.send(Some(&ada), reqwest::Method::POST, &path, body).await;
    assert_eq!(status, 200, "{answered}");
    let mut run = None;
    for _ in 0..100 {
        let (loaded, _) = h.store.load_run(&run_id).await.unwrap();
        if loaded.status.is_terminal() {
            run = Some(loaded);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let run = run.expect("the run ended in 10s");
    assert_eq!(run.status, RunStatus::Finished, "{:?}", run.failure);
    assert_eq!(run.inference_source, SourceKind::LocalProxy);
    assert_eq!(h.gateway.asked().len(), 0, "never the gateway mid-run");
    let asked = h.proxy.asked();
    assert_eq!(
        asked.len(),
        2,
        "the turn, then its carry-on, both at the proxy"
    );
    assert_eq!(asked[1].1["model"], "gpt-5.5");
    assert_eq!(
        sources(&run.emitted),
        [json!({"kind": "local_proxy", "model": "gpt-5.5"})],
        "said once, at the start; a carry-on does not say it again"
    );
}

/// A QUEUED SEND KEEPS THE SOURCE IT WAS QUEUED WITH. The account is on the gateway; a send queued
/// behind turn 1 names the proxy, the queue shows it, and the turn that drains it asks the proxy.
/// A word we do not know is refused as on a live turn, and a send that names none shows none: the
/// account's setting is not the row's to say.
#[tokio::test]
async fn a_queued_send_keeps_the_source_it_was_queued_with() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("from the proxy")], true).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.on_the_proxy(&ada, "gateway").await;
    let thread = unique("thr");
    let first = json!([user("m1", "first")]);
    let props = json!({ "coworkerId": coworker });
    assert_eq!(h.turn(&ada, &thread, first, props).await.0, 200);

    let path = format!("/ag-ui/threads/{thread}/pending");
    let post = reqwest::Method::POST;
    let body = json!({ "v": 1, "content": "queued", "clientMessageId": "m2",
                       "inferenceSource": "local-proxy" });
    let (status, refused) = h.send(Some(&ada), post.clone(), &path, Some(body)).await;
    assert_eq!(status, 400, "{refused}");
    let why = refused.as_str().unwrap_or_default();
    assert!(why.starts_with("inferenceSource must be"), "{why}");

    let body = json!({ "v": 1, "content": "queued", "clientMessageId": "m2",
                       "inferenceSource": "local_proxy" });
    let (status, created) = h.send(Some(&ada), post.clone(), &path, Some(body)).await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(
        created["pendingUserMessage"]["inferenceSource"],
        "local_proxy"
    );
    let id = created["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let body = json!({ "v": 1, "content": "later", "clientMessageId": "m3" });
    let (status, plain) = h.send(Some(&ada), post, &path, Some(body)).await;
    assert_eq!(status, 201, "{plain}");
    let plain_id = plain["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, listed) = h.send(Some(&ada), reqwest::Method::GET, &path, None).await;
    assert_eq!(status, 200, "{listed}");
    let queued = |id: &str| {
        let entries = listed["pendingUserMessages"].as_array().unwrap();
        entries
            .iter()
            .find(|entry| entry["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(queued(&id)["inferenceSource"], "local_proxy", "{listed}");
    assert!(
        queued(&plain_id).get("inferenceSource").is_none(),
        "{listed}"
    );
    let snapshot = h.replay(&ada, &thread).await;
    let entries = snapshot["pendingUserMessages"].as_array().unwrap();
    let on_replay = entries.iter().find(|entry| entry["id"] == id.as_str());
    let on_replay = on_replay.map(|entry| entry["inferenceSource"].clone());
    assert_eq!(on_replay, Some(json!("local_proxy")), "{snapshot}");

    let second = json!([
        user("m1", "first"),
        { "id": "a1", "role": "assistant", "content": "from the gateway" },
        user("m2", "queued"),
    ]);
    let props = json!({ "coworkerId": coworker, "pendingId": id });
    let (status, frames) = h.turn(&ada, &thread, second, props).await;
    assert_eq!(status, 200);
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "model": "gpt-5.5"})],
        "{frames:?}"
    );
    assert_eq!(text_of(&frames), "from the proxy");
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (1, 1));
    let said = conversation(&h.proxy.asked()[0].1);
    assert!(
        said.contains("queued"),
        "the drained words were asked: {said}"
    );
}

/// A queued send whose words start with this breaks its person's account log when it is drained.
const BREAKS_THE_READ: &str = "og-test-breaks-the-read";

/// The event a newer binary might write to an account's log, which this one cannot read, added
/// the moment a `BREAKS_THE_READ` send is drained. AT THE DRAIN, NOT BEFORE: a turn reads its run
/// limits from the same log first, and a log that cannot be read at all is refused there with a
/// 503 before any source is resolved. Broken between the two reads, the source's own read is the
/// one that fails, as a store fault between them would make it.
async fn break_the_account_read_at_the_drain(h: &Harness) {
    for sql in [
        "create or replace function og_test_break_account_on_drain() returns trigger as $$
         begin
           if new.status = 'drained' and old.status <> 'drained'
              and new.content like 'og-test-breaks-the-read%' then
             insert into events (stream_id, stream_seq, event_type, payload)
             select 'account/' || new.account_id, coalesce(max(stream_seq), 0) + 1,
                    'account-from-the-future', '{\"type\":\"from-the-future\"}'::jsonb
               from events where stream_id = 'account/' || new.account_id;
           end if;
           return new;
         end $$ language plpgsql",
        "drop trigger if exists og_test_break_account_on_drain on pending_user_message",
        "create trigger og_test_break_account_on_drain after update on pending_user_message
         for each row execute function og_test_break_account_on_drain()",
    ] {
        sqlx::query(sql).execute(h.store.pool()).await.expect(sql);
    }
}

/// `who`'s account log, readable again once the drain has broken it.
async fn heal_the_account_read(h: &Harness, who: &Person) {
    assert!(
        h.store.load_account(&who.id).await.is_err(),
        "the drain broke the read"
    );
    sqlx::query(
        "delete from events where stream_id = $1 and event_type = 'account-from-the-future'",
    )
    .bind(opengrok_store::account_stream(&who.id))
    .execute(h.store.pool())
    .await
    .expect("the account readable again");
    assert!(h.store.load_account(&who.id).await.is_ok());
}

/// A SETTING THAT CANNOT BE READ IS NEVER GUESSED AS THE GATEWAY: for a person who chose their own
/// subscription, that would be a silent fall back onto a key they chose not to use. A queued send
/// that names no source is refused in words when it is drained, and nothing is asked anywhere;
/// one that names the gateway itself goes there. A live turn's source is resolved by the same
/// `local_proxy::route`, whose own test covers the live pick.
#[tokio::test]
async fn a_drained_send_whose_setting_cannot_be_read_is_refused_unless_it_named_the_gateway() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, vec![words("from the proxy")], true).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.on_the_proxy(&ada, "local_proxy").await;
    let thread = unique("thr");
    let gateway = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let first = json!([user("m1", "first")]);
    assert_eq!(h.turn(&ada, &thread, first, gateway).await.0, 200);
    let path = format!("/ag-ui/threads/{thread}/pending");
    let mut queued = Vec::new();
    for (id, source) in [("m2", Value::Null), ("m3", json!("gateway"))] {
        let said = format!("{BREAKS_THE_READ} {id}");
        let body =
            json!({ "v": 1, "content": said, "clientMessageId": id, "inferenceSource": source });
        let post = reqwest::Method::POST;
        let (status, created) = h.send(Some(&ada), post, &path, Some(body)).await;
        assert_eq!(status, 201, "{created}");
        let row = created["pendingUserMessage"]["id"].as_str().unwrap();
        let props = json!({ "coworkerId": coworker, "pendingId": row });
        queued.push((json!([user(id, &said)]), props));
    }
    break_the_account_read_at_the_drain(&h).await;

    let (messages, props) = queued[0].clone();
    let (status, frames) = h.turn(&ada, &thread, messages, props).await;
    assert_eq!(status, 200);
    let end = ending(&frames);
    assert_eq!(end["type"], "RUN_ERROR", "{frames:?}");
    let said = end["message"].as_str().unwrap_or_default();
    assert!(
        said.starts_with("Your reply source could not be read"),
        "{said}"
    );
    // Refused as a proxy turn with a gap is: no model asked, no gateway key minted.
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "model": ""})]
    );
    heal_the_account_read(&h, &ada).await;

    let (messages, props) = queued[1].clone();
    let (_, frames) = h.turn(&ada, &thread, messages, props).await;
    assert_eq!(text_of(&frames), "from the gateway", "{frames:?}");
    heal_the_account_read(&h, &ada).await;
    assert_eq!(
        (h.gateway.asked().len(), h.proxy.asked().len()),
        (2, 0),
        "the refused turn asked neither"
    );
    for sql in [
        "drop trigger if exists og_test_break_account_on_drain on pending_user_message",
        "drop function if exists og_test_break_account_on_drain()",
    ] {
        sqlx::query(sql).execute(h.store.pool()).await.expect(sql);
    }
}
