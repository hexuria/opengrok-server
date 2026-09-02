//! The desktop path of a policy grant's "needs a human yes", over a real socket, with a stand-in
//! computer that records what ran — so the assertion is "the tool ran after the yes and not
//! before", not "a 200 came back".
//!
//! A coworker is hired with a computer, `shell` is marked needs_approval on its grant, and a turn
//! is sent through the gateway with the tool-asking mock door. The run suspends; the coworker's
//! transcript carries an `auto-review-approval` card with the grant's reason and no
//! `proposedRule`; `resolveAutoReviewApproval approved` resumes the run and the command reaches
//! the stub; a second answer says `alreadyAnswered`. A second turn is denied: the run finishes,
//! the tool never ran, and the refusal the model saw names the coworker's policy.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_core::run::RunStatus;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => url,
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

async fn harness(database_url: &str, email: &str) -> Harness {
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
        door: Arc::new(MockDoor::asking_for_a_tool()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(stub.clone()),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
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

    /// `POST /api/{method}` with the gateway bearer — how the desktop's coordinator calls.
    async fn api(&self, method: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/api/{method}", self.base))
            .header("authorization", "Bearer test-bearer")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("api call");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// Poll the coworker's transcript for a pending approval card with this request id or, when
    /// none is named, the first pending one.
    async fn wait_for_card(&self, agent: &str) -> Value {
        for _ in 0..100 {
            let (_, tail) = self
                .api(
                    "getAgentTranscriptTail",
                    json!({ "id": agent, "limit": 100 }),
                )
                .await;
            if let Some(card) = tail["entries"].as_array().and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| {
                        entry["message"]["type"] == "auto-review-approval"
                            && entry["message"]["approval"]["status"] == "pending"
                    })
                    .cloned()
            }) {
                return card;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("no pending approval card appeared in 10s");
    }
}

#[tokio::test]
async fn a_policy_ask_on_the_desktop_path_is_a_card_the_person_answers() {
    let database_url = database_or_skip!();
    let email = format!("policy-card-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);

    // A coworker with a computer (the stub), and shell marked as needing a human yes.
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
    assert!(
        hired["boxId"]
            .as_str()
            .is_some_and(|b| b.starts_with("bx_stub_")),
        "the stub computer was assigned: {hired}"
    );
    let set: Value = h
        .client
        .post(format!("{}/coworkers/{agent}/approvals", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "tools": ["shell"] }))
        .send()
        .await
        .expect("approvals")
        .json()
        .await
        .expect("approvals json");
    assert!(
        set["needsApproval"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t == "shell")),
        "{set}"
    );

    // A turn: the mock door reaches for shell; the policy says ask; the card appears.
    let (status, sent) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "run a command", "clientNonce": "n-approve" }),
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    let card = h.wait_for_card(&agent).await;
    let approval = &card["message"]["approval"];
    assert_eq!(approval["surface"], "box_shell");
    assert!(
        approval["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "the card carries the grant's reason: {approval}"
    );
    assert!(
        approval.get("proposedRule").is_none(),
        "a policy card offers no rule: {approval}"
    );
    assert!(
        approval["command"]
            .as_str()
            .is_some_and(|c| c.contains("opengrok-tool-ran")),
        "{approval}"
    );
    assert!(h.stub.ran().is_empty(), "nothing ran before the yes");
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    let request_id = approval["requestId"]
        .as_str()
        .expect("request id")
        .to_string();

    // Approved: the run resumes and the command reaches the computer.
    let (status, body) = h
        .api(
            "resolveAutoReviewApproval",
            json!({ "entryId": entry_id, "requestId": request_id, "resolution": "approved", "agentId": agent }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    let mut ran = Vec::new();
    for _ in 0..100 {
        ran = h.stub.ran();
        if !ran.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        ran.iter().any(|c| c.contains("opengrok-tool-ran")),
        "the approved command ran on the coworker's computer: {ran:?}"
    );
    let (_, again) = h
        .api(
            "resolveAutoReviewApproval",
            json!({ "entryId": entry_id, "requestId": request_id, "resolution": "approved", "agentId": agent }),
        )
        .await;
    assert_eq!(again["alreadyAnswered"], true, "{again}");

    // Denied: the run finishes, nothing more ran, and the refusal the model saw names the policy.
    let before = h.stub.ran().len();
    let (status, _) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "run it again", "clientNonce": "n-deny" }),
        )
        .await;
    assert_eq!(status, 200);
    let card = h.wait_for_card(&agent).await;
    let entry_id = card["id"].as_str().unwrap().to_string();
    let request_id = card["message"]["approval"]["requestId"]
        .as_str()
        .unwrap()
        .to_string();
    let awaiting = h
        .store
        .awaiting_approval(&h.account)
        .await
        .expect("awaiting");
    let mut run_id = None;
    for id in awaiting {
        if let Ok((run, _)) = h.store.load_run(&id).await
            && run
                .pending
                .as_ref()
                .is_some_and(|p| p.call_id == request_id)
        {
            run_id = Some(id);
            break;
        }
    }
    let run_id = run_id.expect("the suspended run");
    let (status, body) = h
        .api(
            "resolveAutoReviewApproval",
            json!({ "entryId": entry_id, "requestId": request_id, "resolution": "denied", "agentId": agent }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let mut finished = None;
    for _ in 0..100 {
        let (run, _) = h.store.load_run(&run_id).await.expect("run");
        if run.status == RunStatus::Finished || run.status == RunStatus::Failed {
            finished = Some(run);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let run = finished.expect("the denied run finished");
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(h.stub.ran().len(), before, "a denied command never ran");
    let emitted = serde_json::to_string(&run.emitted).expect("events");
    assert!(
        emitted.contains("the coworker's policy needs a person's yes"),
        "the refusal names the policy: {emitted}"
    );
    let (_, tail) = h
        .api(
            "getAgentTranscriptTail",
            json!({ "id": agent, "limit": 100 }),
        )
        .await;
    let settled = tail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == entry_id)
        .expect("card");
    assert_eq!(settled["message"]["approval"]["status"], "denied");
}
