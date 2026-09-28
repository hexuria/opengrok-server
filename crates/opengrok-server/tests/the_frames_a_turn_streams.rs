//! A turn that thinks and uses a tool streams every frame NativeChat reads in its feed:
//! reasoning, the tool call, and the tool's result (#255, nativechat#3). The wire corpus NativeChat
//! vendors is recorded from what these tests stream (`scripts/record-wire.sh`), so a frame no
//! test streams is a frame the corpus cannot show it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
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

/// A box that runs anything and says so.
#[derive(Default)]
struct StubBox {
    ran: Mutex<Vec<String>>,
}

#[async_trait]
impl Computer for StubBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_form_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _b: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
        self.ran.lock().expect("ran").push(command.to_string());
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
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
    async fn offers_a_screen(&self, _b: &str) -> bool {
        true
    }
}

async fn seed_account(store: &PgStore, email: &str, org: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: org.to_string(),
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
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
        org_id: (!org.is_empty()).then(|| org.to_string()),
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

fn frames(sse: &str) -> Vec<Value> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect()
}

#[tokio::test]
async fn a_turn_that_thinks_and_uses_a_tool_streams_every_step() {
    let database_url = database_or_skip!();
    let email = format!("frames-{}@og.local", uuid::Uuid::now_v7().simple());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, &email, "").await;
    let stub = Arc::new(StubBox::default());
    let minter = Arc::new(TokenMinter::new(b"a-turn-that-thinks-and-acts"));
    let agui = AgUiState {
        auth: AuthState::new(store, minter, email.clone()),
        door: Arc::new(MockDoor::reasoning_then_a_tool()),
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
    let host = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui.clone(), host);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let token = agui
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-frames",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    let client = reqwest::Client::new();
    let hired: Value = client
        .post(format!("{base}/coworkers"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "name": "Thinker" }))
        .send()
        .await
        .expect("hire")
        .json()
        .await
        .expect("hired");
    let coworker = hired["id"].as_str().expect("coworker id");

    let sse = client
        .post(format!("{base}/ag-ui"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": format!("th-{}", uuid::Uuid::now_v7().simple()),
            "runId": uuid::Uuid::now_v7().to_string(),
            "messages": [{ "id": "m-1", "role": "user", "content": "what is on the box?" }],
            "forwardedProps": { "coworkerId": coworker },
        }))
        .send()
        .await
        .expect("turn")
        .text()
        .await
        .expect("sse");
    let frames = frames(&sse);
    let kinds: Vec<&str> = frames
        .iter()
        .filter_map(|frame| frame["type"].as_str())
        .collect();
    for kind in [
        "RUN_STARTED",
        "REASONING_MESSAGE_START",
        "REASONING_MESSAGE_CONTENT",
        "REASONING_MESSAGE_END",
        "TOOL_CALL_START",
        "TOOL_CALL_ARGS",
        "TOOL_CALL_END",
        "TOOL_CALL_RESULT",
        "RUN_FINISHED",
    ] {
        assert!(kinds.contains(&kind), "{kind} was not streamed: {kinds:?}");
    }
    let position = |kind: &str| kinds.iter().position(|seen| *seen == kind).unwrap();
    assert!(
        position("REASONING_MESSAGE_END") < position("TOOL_CALL_START"),
        "the thinking closes before the tool call opens: {kinds:?}"
    );
    let result = frames
        .iter()
        .find(|frame| frame["type"] == "TOOL_CALL_RESULT")
        .unwrap();
    assert_eq!(result["toolCallId"], "mock-call-1");
    assert_eq!(result["ok"], true, "{result}");
    assert!(
        stub.ran
            .lock()
            .expect("ran")
            .iter()
            .any(|command| command.contains("opengrok-tool-ran")),
        "and the tool ran on the box"
    );
}
