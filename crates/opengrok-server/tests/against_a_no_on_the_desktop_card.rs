//! A no on the desktop's approval card reaches the model, and the run ends.
//!
//! THE BUG THIS EXISTS FOR, seen on 18 Sep 2026 in NativeChat: a person denied a `shell` card and
//! the composer went on offering to stop a turn that nothing was doing. The working line had gone
//! — no coworker was thinking — but the stop button stayed, because as far as the app could tell
//! the turn was still in flight. It was: `POST /ag-ui/runs/{id}/answer` put the run back to
//! `Running`, the way answering always does, and then resumed it only when the answer was yes. A
//! denied run was left running with nobody advancing it.
//!
//! THREE THINGS WENT WRONG AT ONCE, and only the button was visible. The model was never told it
//! had been refused, so it could not say what it would do instead. The tool call was left with no
//! result, and a call with no result is what the NEXT turn in that thread replays to the model.
//! And the recovery sweep, which claims runs that are `running` and nobody is holding, eventually
//! reached this one and ended it with "we do not know whether it ran" — a sentence about a crash,
//! written over a refusal somebody made on purpose.
//!
//! The gateway's door has resumed refusals from the start (see `against_policy_card`, which is
//! this test's twin on that side). Two doors on to the same run disagreed; this is the one that
//! was wrong. So this asserts what only a resumed refusal can be: the run REACHES AN ENDING, the
//! tool never ran, and the refusal the model was handed names the tool it may not use.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

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
    )
    // Not an identity test: it speaks as the deployment account, which since 5 Sep 2026
    // must be asked for rather than assumed.
    .allowing_identity_fallback();
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
async fn a_no_on_the_desktop_card_is_told_to_the_model_and_ends_the_run() {
    let database_url = database_or_skip!();
    let email = format!("desktop-no-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);

    // A coworker with a computer (the stub), and shell marked as needing a human yes — the same
    // setup the gateway's twin uses, so the only difference between the two tests is the door the
    // answer arrives at.
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

    // A turn the mock door answers by reaching for shell, which the policy says to ask about.
    let (status, sent) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "run a command", "clientNonce": "n-desktop-no" }),
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    let card = h.wait_for_card(&agent).await;
    let call_id = card["message"]["approval"]["requestId"]
        .as_str()
        .expect("request id")
        .to_string();
    assert!(h.stub.ran().is_empty(), "nothing ran before the answer");

    // The run the card belongs to, found the way the desktop finds it: off the queue of runs
    // waiting on this person.
    let mut run_id = None;
    for id in h
        .store
        .awaiting_approval(&h.account)
        .await
        .expect("awaiting")
    {
        if let Ok((run, _)) = h.store.load_run(&id).await
            && run.pending.as_ref().is_some_and(|p| p.call_id == call_id)
        {
            run_id = Some(id);
            break;
        }
    }
    let run_id = run_id.expect("the suspended run");

    // THE DESKTOP'S DOOR, and a no.
    let answered: Value = h
        .client
        .post(format!("{}/ag-ui/runs/{}/answer", h.base, run_id.as_str()))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "call_id": call_id, "approved": false }))
        .send()
        .await
        .expect("answer")
        .json()
        .await
        .expect("answer json");
    assert_eq!(answered["approved"], false, "{answered}");
    assert_eq!(answered["alreadyAnswered"], false, "{answered}");
    // THE ASSERTION THE BUG FAILS: the run reaches an ending on its own. Before this it stayed
    // `Running` until the recovery sweep mistook it for a run a restart had dropped.
    let mut ended = None;
    for _ in 0..100 {
        let (run, _) = h.store.load_run(&run_id).await.expect("run");
        if run.status.is_terminal() {
            ended = Some(run);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let run = ended.expect(
        "the denied run never ended: it was left running with nobody advancing it, which is the \
         stop button the person could not get rid of",
    );
    assert_eq!(
        run.status,
        RunStatus::Finished,
        "a refusal is an ordinary ending, not a failure"
    );
    assert!(
        run.pending.is_none(),
        "the card is answered, so nothing is pending: {:?}",
        run.pending
    );
    assert!(
        h.stub.ran().is_empty(),
        "a denied command never ran: {:?}",
        h.stub.ran()
    );

    // A no is still something happening, and the answer says so. The client decides whether to
    // keep watching on the strength of this word; while it said false, the app had no way to know
    // a refusal was on its way to the model. Asserted after the ending above, so that a rebuilt
    // bug is caught by the run that never ends rather than by the word that describes it.
    assert_eq!(
        answered["continuing"], true,
        "a refusal is carried to the model, so the run continues: {answered}"
    );

    // And the model was told, in words that name what it may not use — so it can offer something
    // else rather than proposing the same call again.
    let emitted = serde_json::to_string(&run.emitted).expect("events");
    assert!(
        emitted.contains("declined"),
        "the refusal reached the model: {emitted}"
    );
    assert!(
        emitted.contains("shell"),
        "the refusal names the tool: {emitted}"
    );
}
