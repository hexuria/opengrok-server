//! A run that reached a cap says so: `reason: "budget"` on its `RUN_FINISHED`, and
//! `finishReason: "budget"` on the run a replay reads back (#244).
//!
//! A capped run FINISHES rather than failing — its last call, with no tools, is the model's own
//! account of what it did — so its status alone reads as "done". A person looking at a routine
//! that stopped halfway needs to see it stopped at its limit, and the word has to survive the
//! journal: the frame is what streams, the aggregate is what every later reader asks.
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

/// A computer that exists only to say what was run on it.
#[derive(Default)]
struct StubComputer {
    commands: Mutex<Vec<String>>,
}

impl StubComputer {
    fn ran(&self) -> Vec<String> {
        self.commands.lock().expect("commands").clone()
    }
}

#[async_trait]
impl Computer for StubComputer {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_stub_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(
        &self,
        _box_id: &str,
        command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        self.commands
            .lock()
            .expect("commands")
            .push(command.to_string());
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, command: &str) -> BoxResult<StartedCommand> {
        self.commands
            .lock()
            .expect("commands")
            .push(command.to_string());
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
    stub: Arc<StubComputer>,
    client: reqwest::Client,
}

async fn harness_with_door(
    database_url: &str,
    email: &str,
    door: Arc<dyn opengrok_harness::ModelDoor>,
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
    let account = seed_account(&store, email).await;
    let stub = Arc::new(StubComputer::default());
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"policy-card-secret")),
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
    let app = opengrok_server::router(agui.clone(), gateway);
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

    /// One turn on the AG-UI door, the way NativeChat drives one. The reply is the whole SSE
    /// stream; a turn that pauses for a card ends it with `run-awaiting-approval`.
    async fn turn(&self, token: &str, agent: &str, prompt: &str) -> String {
        self.turn_on(
            token,
            agent,
            &format!("thr-{}", uuid::Uuid::now_v7()),
            prompt,
        )
        .await
    }

    /// The same, on a thread the caller names — the client picks `threadId` and the server takes
    /// it verbatim, which is the whole reason the MCP test below exists.
    async fn turn_on(&self, token: &str, agent: &str, thread: &str, prompt: &str) -> String {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "threadId": thread,
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": "m1", "role": "user", "content": prompt }],
                "forwardedProps": { "coworkerId": agent },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
        res.text().await.expect("sse")
    }
}

/// A model that asks for `shell` every time it is offered tools, and answers in words when it is
/// not: the cap is reached, and the wrap-up has something to say.
struct NeverStops;

#[async_trait]
impl opengrok_harness::ModelDoor for NeverStops {
    async fn stream(
        &self,
        request: opengrok_harness::ModelRequest,
    ) -> Result<opengrok_harness::DeltaStream, opengrok_harness::ModelError> {
        use opengrok_harness::ModelDelta;
        let script = if request.tools.is_empty() {
            vec![ModelDelta::Text(
                "I ran it eight times and it never settled.".to_string(),
            )]
        } else {
            vec![
                ModelDelta::ToolCallStart {
                    id: format!("c{}", request.messages.len()),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: format!("c{}", request.messages.len()),
                    delta: r#"{"command":"again"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: format!("c{}", request.messages.len()),
                },
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

#[tokio::test]
async fn a_run_that_reached_its_limit_finishes_and_says_so() {
    let database_url = database_or_skip!();
    let email = format!("at-limit-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(NeverStops)).await;
    let token = h.access_token(&email);
    let hired: Value = h
        .client
        .post(format!("{}/coworkers", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "name": "Ada" }))
        .send()
        .await
        .expect("hire")
        .json()
        .await
        .expect("hire json");
    let agent = hired["id"].as_str().expect("coworker id").to_string();

    let sse = h.turn(&token, &agent, "keep going").await;
    let frames: Vec<Value> = sse
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect();
    let finished = frames
        .iter()
        .find(|frame| frame["type"] == "RUN_FINISHED")
        .unwrap_or_else(|| panic!("no RUN_FINISHED: {sse}"));
    assert_eq!(finished["reason"], "budget", "live: {finished}");
    let run_id = finished["runId"].as_str().expect("run id").to_string();
    assert!(
        h.stub.ran().len() >= opengrok_harness::MAX_ROUNDS,
        "the cap was reached by working: {:?}",
        h.stub.ran()
    );

    let replay: Value = h
        .client
        .get(format!("{}/ag-ui/runs/{run_id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay")
        .json()
        .await
        .expect("replay json");
    assert_eq!(replay["status"], "finished", "{replay}");
    assert_eq!(
        replay["finishReason"], "budget",
        "the log kept it: {replay}"
    );
    let (run, _) = h
        .store
        .load_run(&opengrok_core::id::RunId::from_stored(run_id.clone()))
        .await
        .expect("run");
    assert_eq!(
        run.finish_reason,
        Some(opengrok_core::run::FinishReason::Budget)
    );
}

/// And a run that was simply done says nothing: the word is for the capped run alone.
#[tokio::test]
async fn a_run_that_was_simply_done_names_no_reason() {
    let database_url = database_or_skip!();
    let email = format!("done-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(MockDoor::echoing())).await;
    let token = h.access_token(&email);
    let hired: Value = h
        .client
        .post(format!("{}/coworkers", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "name": "Ada" }))
        .send()
        .await
        .expect("hire")
        .json()
        .await
        .expect("hire json");
    let agent = hired["id"].as_str().expect("coworker id").to_string();
    let sse = h.turn(&token, &agent, "hello").await;
    let finished: Value = sse
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|frame| frame["type"] == "RUN_FINISHED")
        .unwrap_or_else(|| panic!("no RUN_FINISHED: {sse}"));
    assert!(finished.get("reason").is_none(), "{finished}");
    let run_id = finished["runId"].as_str().expect("run id");
    let replay: Value = h
        .client
        .get(format!("{}/ag-ui/runs/{run_id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay")
        .json()
        .await
        .expect("replay json");
    assert!(replay["finishReason"].is_null(), "{replay}");
}
