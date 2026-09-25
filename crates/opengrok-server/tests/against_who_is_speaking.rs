//! The coworker is told who is speaking to it and what day it is (#193).
//!
//! The system message used to be identity, standing role and tail — nothing named the person or
//! the date, so an org-shared coworker could not tell its people apart and "remind me tomorrow"
//! had no today. The line is composed from the bearer's account, never from the body
//! (CLAUDE.md #7). These read the system message the model was handed.
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
    let minter = Arc::new(TokenMinter::new(b"who-is-speaking-secret"));
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
    async fn person(&self, first: &str, last: &str, org: Option<&str>) -> Person {
        let email = format!("speaking-{}@og.local", uuid::Uuid::now_v7().simple());
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
        let token = self.token_for(&account, &email, "sess-speaking");
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

/// Today as the line spells it, taken either side of a turn so one that crosses midnight
/// cannot flake.
fn days() -> String {
    chrono::Utc::now().format("%Y-%m-%d, %A").to_string()
}

fn system_of(request: &ModelRequest) -> String {
    request.system.clone().expect("a composed system message")
}

#[tokio::test]
async fn the_system_message_names_who_is_speaking_and_today() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let juana = h.person("Juana", "Cruz", None).await;
    let bot = h.hire(&juana.token).await;
    let before = days();
    h.turn(
        &juana.token,
        &bot,
        "thr-speaking",
        json!([{ "id": "m1", "role": "user", "content": "what's my name and what day is it?" }]),
    )
    .await;
    let after = days();
    let system = system_of(&h.asked()[0]);
    assert!(
        system.contains("You are talking with Juana Cruz."),
        "the bearer's own name: {system}"
    );
    assert!(
        system.contains(&format!("Today is {before} (UTC)."))
            || system.contains(&format!("Today is {after} (UTC).")),
        "today's date, and the zone it is in: {system}"
    );
    assert!(
        !system.contains(&juana.email),
        "a name wins over the address: {system}"
    );
}

/// Nobody gave a name: the address the account signed up with is what the person is called.
#[tokio::test]
async fn a_person_with_no_name_is_named_by_their_email() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let nameless = h.person("", " ", None).await;
    let bot = h.hire(&nameless.token).await;
    h.turn(
        &nameless.token,
        &bot,
        "thr-nameless",
        json!([{ "id": "m1", "role": "user", "content": "hi" }]),
    )
    .await;
    let system = system_of(&h.asked()[0]);
    assert!(
        system.contains(&format!("You are talking with {}.", nameless.email)),
        "{system}"
    );
}

/// A coworker two people share tells them apart, each by their own sign-in and never by what
/// the other one said (CLAUDE.md #7).
#[tokio::test]
async fn each_member_of_an_org_shared_coworker_is_named_as_themselves() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let org = format!("org-{}", uuid::Uuid::now_v7().simple());
    let owner = h.person("Ada", "Owner", Some(&org)).await;
    let colleague = h.person("Bea", "Colleague", Some(&org)).await;
    let bot = h.hire(&owner.token).await;
    let (status, body) = h
        .call(
            &owner.token,
            "PATCH",
            &format!("/coworkers/{bot}"),
            Some(json!({ "visibility": "org" })),
        )
        .await;
    assert!(
        (200..300).contains(&status),
        "share with the org: {status} {body}"
    );
    // Sharing makes the coworker visible; using it takes a grant of the colleague's own, which
    // no AG-UI route issues yet, so it is written the way hiring writes the owner's.
    h.store
        .grant_access(
            &colleague.account,
            &opengrok_core::id::CoworkerId::from_stored(bot.clone()),
            &opengrok_policy::ToolSet::All,
            &opengrok_policy::ToolSet::All,
            &opengrok_policy::ToolSet::None,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("grant the colleague");

    let hello = json!([{ "id": "m1", "role": "user", "content": "who am I?" }]);
    h.turn(
        &owner.token,
        &bot,
        &format!("thr-{bot}-owner"),
        hello.clone(),
    )
    .await;
    h.turn(
        &colleague.token,
        &bot,
        &format!("thr-{bot}-colleague"),
        hello,
    )
    .await;
    let asked = h.asked();
    assert_eq!(asked.len(), 2);
    let (first, second) = (system_of(&asked[0]), system_of(&asked[1]));
    assert!(
        first.contains("Ada Owner") && !first.contains("Bea Colleague"),
        "{first}"
    );
    assert!(
        second.contains("Bea Colleague") && !second.contains("Ada Owner"),
        "{second}"
    );
}
