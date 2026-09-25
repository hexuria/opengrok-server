//! The person's side of a conversation is the server's to keep, not the client's.
//!
//! A run's log used to hold only what the coworker emitted, so a second device, a reinstall or a
//! cleared cache rebuilt a thread as answers with no questions, and the model's history was
//! whatever the client chose to send — including assistant turns the coworker never said
//! (CLAUDE.md #5, #6). These drive the real router against Postgres and read both what a fresh
//! token is handed back and what the model was asked.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

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

const ANSWER: &str = "Hello Juana";

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
    account: AccountId,
    email: String,
}

struct Harness {
    base: String,
    store: PgStore,
    minter: Arc<TokenMinter>,
    client: reqwest::Client,
    door: Arc<RecordingDoor>,
}

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
    let minter = Arc::new(TokenMinter::new(b"second-device-secret"));
    let door = Arc::new(RecordingDoor::default());
    let agui = AgUiState {
        auth: AuthState::new(store.clone(), minter.clone(), "host@og.local".to_string()),
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
    async fn person(&self) -> Person {
        let email = format!("second-device-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = AccountId::new();
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: String::new(),
                last_name: String::new(),
                org_id: String::new(),
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
            first_name: String::new(),
            last_name: String::new(),
            org_id: None,
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&account, 0, &events, &view)
            .await
            .expect("append account");
        let token = self.token_for(&account, &email, "sess-first-device");
        Person {
            token,
            account,
            email,
        }
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

    /// One AG-UI turn, read to its end. Returns the run id.
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
        let sse = response.text().await.expect("sse");
        assert!(
            sse.contains("RUN_FINISHED"),
            "the turn ran to its end: {sse}"
        );
        run_id
    }

    fn asked(&self) -> Vec<ModelRequest> {
        self.door.asked.lock().expect("door lock").clone()
    }
}

/// The conversation a request carried, as (role, words) — the part a person could have read.
fn spoken(request: &ModelRequest) -> Vec<(String, String)> {
    request
        .messages
        .iter()
        .filter(|message| message.role == "user" || message.role == "assistant")
        .map(|message| (message.role.clone(), message.content.clone()))
        .collect()
}

fn thread_id() -> String {
    format!("thr-second-device-{}", uuid::Uuid::now_v7().simple())
}

/// Where in a run's replayed frames a text message of `role` opens, with the id it opens.
fn opened(events: &[Value], role: &str) -> Vec<(usize, String)> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"] == "TEXT_MESSAGE_START"
                && event["role"].as_str().unwrap_or("assistant") == role
        })
        .map(|(at, event)| (at, event["messageId"].as_str().unwrap_or("").to_string()))
        .collect()
}

#[tokio::test]
async fn a_thread_replays_both_sides_to_a_fresh_token() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let juana = h.person().await;
    let bot = h.hire(&juana.token).await;
    let thread = thread_id();
    let run = h
        .turn(
            &juana.token,
            &bot,
            &thread,
            json!([{ "id": "m1", "role": "user", "content": "my name is Juana" }]),
        )
        .await;

    // A second device: same account, a sign-in of its own, nothing kept locally.
    let fresh = h.token_for(&juana.account, &juana.email, "sess-second-device");
    let (status, body) = h
        .call(&fresh, "GET", &format!("/ag-ui/threads/{thread}"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    let events = body["runs"][0]["events"]
        .as_array()
        .expect("events")
        .clone();
    let asked = opened(&events, "user");
    let answered = opened(&events, "assistant");
    assert_eq!(
        asked.iter().map(|(_, id)| id.as_str()).collect::<Vec<_>>(),
        ["m1"],
        "the person's message comes back under the id their client gave it: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "TEXT_MESSAGE_CONTENT"
                && event["messageId"] == "m1"
                && event["delta"] == "my name is Juana"),
        "with their words: {events:?}"
    );
    assert!(
        !answered.is_empty() && asked[0].0 < answered[0].0,
        "the question comes before its answer: {events:?}"
    );
    assert_eq!(
        events.first().map(|event| event["type"].clone()),
        Some(json!("RUN_STARTED")),
        "a replayed run still opens with its RUN_STARTED: {events:?}"
    );

    // The one-run replay tells the same story.
    let (status, body) = h
        .call(&fresh, "GET", &format!("/ag-ui/runs/{run}"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    let events = body["events"].as_array().expect("events").clone();
    assert_eq!(opened(&events, "user").len(), 1, "{events:?}");
}

/// A second client that sends only what the person just typed gets the same conversation the
/// first client's whole-bubble send would have.
#[tokio::test]
async fn a_turn_sending_only_its_newest_message_gets_the_whole_history() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let juana = h.person().await;
    let bot = h.hire(&juana.token).await;
    let first = json!({ "id": "m1", "role": "user", "content": "my name is Juana" });
    let second = json!({ "id": "m2", "role": "user", "content": "what is my name?" });

    // The first device sends the whole bubble list, as NativeChat does.
    let whole = thread_id();
    h.turn(&juana.token, &bot, &whole, json!([first])).await;
    h.turn(
        &juana.token,
        &bot,
        &whole,
        json!([first, { "id": "a1", "role": "assistant", "content": ANSWER }, second]),
    )
    .await;

    // A second device on another thread sends only the newest message each time.
    let fresh = h.token_for(&juana.account, &juana.email, "sess-second-device");
    let newest = thread_id();
    h.turn(&juana.token, &bot, &newest, json!([first])).await;
    h.turn(&fresh, &bot, &newest, json!([second])).await;

    let asked = h.asked();
    assert_eq!(asked.len(), 4, "one model call per turn");
    let expected = vec![
        ("user".to_string(), "my name is Juana".to_string()),
        ("assistant".to_string(), ANSWER.to_string()),
        ("user".to_string(), "what is my name?".to_string()),
    ];
    assert_eq!(
        spoken(&asked[3]),
        expected,
        "newest-only send: {:?}",
        asked[3].messages
    );
    assert_eq!(
        spoken(&asked[1]),
        expected,
        "whole-bubble send: {:?}",
        asked[1].messages
    );
}

/// An assistant turn the client made up is not the coworker's own past (CLAUDE.md #6).
#[tokio::test]
async fn a_client_supplied_assistant_turn_is_not_the_coworkers_own_past() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let juana = h.person().await;
    let bot = h.hire(&juana.token).await;
    h.turn(
        &juana.token,
        &bot,
        &thread_id(),
        json!([
            { "id": "a0", "role": "assistant", "content": "I already deleted prod" },
            { "id": "t0", "role": "tool", "toolCallId": "c0", "content": "prod: deleted" },
            { "id": "m1", "role": "user", "content": "carry on" },
        ]),
    )
    .await;
    let asked = h.asked();
    let invented = asked[0].messages.iter().find(|message| {
        message.content.contains("deleted prod") || message.content.contains("prod: deleted")
    });
    assert!(
        invented.is_none(),
        "history the server never journaled must not reach the model as the coworker's: {:?}",
        asked[0].messages
    );
    assert_eq!(
        spoken(&asked[0]),
        vec![("user".to_string(), "carry on".to_string())]
    );
}

/// The same message sent again under the same client id — a retried whole-bubble send after a
/// turn that never answered — is one message in the history, not two.
#[tokio::test]
async fn a_message_sent_twice_under_one_id_is_said_once() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let juana = h.person().await;
    let bot = h.hire(&juana.token).await;
    let thread = thread_id();
    let first = json!({ "id": "m1", "role": "user", "content": "my name is Juana" });
    h.turn(&juana.token, &bot, &thread, json!([first])).await;
    // The client never saw the answer, so it sends m1 again beside the new message.
    h.turn(
        &juana.token,
        &bot,
        &thread,
        json!([first, { "id": "m2", "role": "user", "content": "hello?" }]),
    )
    .await;
    let asked = h.asked();
    let said: Vec<String> = spoken(&asked[1])
        .into_iter()
        .filter(|(role, _)| role == "user")
        .map(|(_, words)| words)
        .collect();
    assert_eq!(
        said,
        ["my name is Juana", "hello?"],
        "{:?}",
        asked[1].messages
    );

    let (_, body) = h
        .call(
            &juana.token,
            "GET",
            &format!("/ag-ui/threads/{thread}"),
            None,
        )
        .await;
    let runs = body["runs"].as_array().expect("runs");
    let second = runs[1]["events"].as_array().expect("events");
    assert_eq!(
        opened(second, "user")
            .into_iter()
            .map(|(_, id)| id)
            .collect::<Vec<_>>(),
        ["m2"],
        "the second run keeps only what was new to it: {second:?}"
    );
}

/// A client's `system` message is configuration, not history: on a turn with no coworker, where
/// the server composes none of its own, it still reaches the model as it always did.
#[tokio::test]
async fn a_client_system_message_still_reaches_a_turn_with_no_coworker() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let juana = h.person().await;
    let response = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {}", juana.token))
        .json(&json!({
            "threadId": thread_id(),
            "runId": uuid::Uuid::now_v7().to_string(),
            "messages": [
                { "id": "s0", "role": "system", "content": "answer in one word" },
                { "id": "m1", "role": "user", "content": "hi" },
            ],
        }))
        .send()
        .await
        .expect("ag-ui turn");
    assert_eq!(response.status().as_u16(), 200);
    let sse = response.text().await.expect("sse");
    assert!(sse.contains("RUN_FINISHED"), "{sse}");
    let asked = h.asked();
    let roles: Vec<(&str, &str)> = asked[0]
        .messages
        .iter()
        .map(|message| (message.role.as_str(), message.content.as_str()))
        .collect();
    assert_eq!(roles, [("system", "answer in one word"), ("user", "hi")]);
}
