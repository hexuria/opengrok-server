//! A long conversation reaches the model trimmed, or not at all, never as a provider's 400 (#90).
//!
//! The history a client sends when the server has none of its own is taken whole, and nothing
//! counted it: a thread long enough came back from the provider as "the gateway could not take
//! this request", on every turn after. These drive a real turn with a small context and read what
//! the model was handed. Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
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

const ANSWER: &str = "Done.";

/// Keeps every request and answers every one the same way, so what differs between two runs is
/// only what the server chose to ask.
#[derive(Default)]
struct RecordingDoor {
    asked: Mutex<Vec<ModelRequest>>,
}

#[async_trait::async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push(request);
        }
        Ok(Box::pin(futures::stream::iter(vec![Ok(ModelDelta::Text(
            ANSWER.to_string(),
        ))])))
    }
}

struct Person {
    token: String,
}

struct Harness {
    base: String,
    store: PgStore,
    minter: Arc<TokenMinter>,
    client: reqwest::Client,
    door: Arc<RecordingDoor>,
}

async fn harness(database_url: &str, context_tokens: Option<u64>) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"long-thread-secret"));
    let door = Arc::new(RecordingDoor::default());
    let agui = AgUiState {
        auth: AuthState::new(store.clone(), minter.clone(), "host@og.local".to_string())
            .with_context_tokens(context_tokens),
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
        host_settings: None,
    };
    let gateway = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui, gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        store,
        minter,
        client: reqwest::Client::new(),
        door,
    }
}

impl Harness {
    async fn person(&self, first: &str, last: &str, org: Option<&str>) -> Person {
        let email = format!("long-thread-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = AccountId::new();
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: first.to_string(),
                last_name: last.to_string(),
                org_id: org.unwrap_or_default().to_string(),
                plan: Plan::Ultra,
                verified: true,
                enabled: true,
                at_ms,
            })
            .expect("register");
        let view = AccountView {
            id: account.clone(),
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: first.to_string(),
            last_name: last.to_string(),
            org_id: org.map(str::to_string),
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&account, 0, &events, &view)
            .await
            .expect("append account");
        let token = self.token_for(&account, &email, "sess-long-thread");
        Person { token }
    }

    /// Another sign-in for the same account: a new device with nothing stored locally.
    fn token_for(&self, account: &AccountId, email: &str, session: &str) -> String {
        self.minter
            .mint_access(
                account.as_str(),
                session,
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn call(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let request = match method {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PATCH" => self.client.patch(url),
            _ => panic!("no such method in this harness: {method}"),
        }
        .header("authorization", format!("Bearer {token}"));
        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };
        let response = request.send().await.expect("send");
        let status = response.status().as_u16();
        let text = response.text().await.expect("text");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn hire(&self, token: &str) -> String {
        let (status, body) = self
            .call(token, "POST", "/coworkers", Some(json!({ "name": "Ada" })))
            .await;
        assert_eq!(status, 201, "hire: {body}");
        body["id"].as_str().expect("id").to_string()
    }

    /// One AG-UI turn, read to its end. Returns the stream.
    async fn turn(&self, token: &str, coworker: &str, thread: &str, messages: Value) -> String {
        let run_id = uuid::Uuid::now_v7().to_string();
        let response = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "threadId": thread,
                "runId": run_id,
                "messages": messages,
                "forwardedProps": { "coworkerId": coworker },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(response.status().as_u16(), 200, "ag-ui turn status");
        response.text().await.expect("sse")
    }

    fn asked(&self) -> Vec<ModelRequest> {
        self.door.asked.lock().expect("door lock").clone()
    }
}

/// `turns` turns on one thread, each a person saying `bytes` of words; the server composes the
/// next turn's history from its own record of these.
async fn talk(h: &Harness, token: &str, bot: &str, thread: &str, turns: usize, bytes: usize) {
    for turn in 0..turns {
        let sse = h
            .turn(
                token,
                bot,
                thread,
                json!([{ "id": format!("m{turn}"), "role": "user",
                         "content": format!("turn {turn} {}", "w".repeat(bytes)) }]),
            )
            .await;
        assert!(sse.contains("RUN_FINISHED"), "turn {turn}: {sse}");
    }
}

fn users(request: &ModelRequest) -> usize {
    request.messages.iter().filter(|m| m.role == "user").count()
}

#[tokio::test]
async fn a_long_thread_reaches_the_model_trimmed_with_this_turn_whole() {
    let url = database_or_skip!();
    let h = harness(&url, Some(20_000)).await;
    let person = h.person("Ada", "Long", None).await;
    let bot = h.hire(&person.token).await;
    // Twelve turns of ~2k tokens each: more than the 15k of room a 20k model leaves.
    talk(&h, &person.token, &bot, "thr-long", 12, 6_000).await;
    let asked = h.asked();
    let last = asked.last().unwrap();
    assert_eq!(last.context_tokens, Some(20_000));
    assert!(
        users(last) < 12,
        "older turns were left out: {}",
        users(last)
    );
    assert!(
        last.messages
            .last()
            .unwrap()
            .content
            .starts_with("turn 11 ")
    );
    assert!(
        last.system
            .as_deref()
            .unwrap_or_default()
            .contains("oldest messages"),
        "the model is told something was left out"
    );
    // The first turns were small enough to go whole.
    assert_eq!(users(&asked[1]), 2);
}

#[tokio::test]
async fn a_turn_too_long_for_the_model_is_refused_with_a_sentence_and_never_sent() {
    let url = database_or_skip!();
    let h = harness(&url, Some(8_000)).await;
    let person = h.person("Ada", "Huge", None).await;
    let bot = h.hire(&person.token).await;
    let sse = h
        .turn(
            &person.token,
            &bot,
            "thr-huge",
            json!([{ "id": "m1", "role": "user", "content": "w".repeat(90_000) }]),
        )
        .await;
    assert!(sse.contains("RUN_ERROR"), "{sse}");
    assert!(sse.contains("too long for oag/cheap"), "{sse}");
    assert!(
        h.asked().is_empty(),
        "no request the model cannot read is sent"
    );
}

/// `OG_CONTEXT_TOKENS=0`: the operator turned the guard off, and the history goes as it came.
#[tokio::test]
async fn with_the_guard_off_nothing_is_left_out() {
    let url = database_or_skip!();
    let h = harness(&url, None).await;
    let person = h.person("Ada", "Off", None).await;
    let bot = h.hire(&person.token).await;
    talk(&h, &person.token, &bot, "thr-off", 12, 6_000).await;
    let last = h.asked().pop().unwrap();
    assert_eq!(last.context_tokens, None);
    assert_eq!(users(&last), 12);
}
