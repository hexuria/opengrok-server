//! A form raised by a run that was continued from a card has a card of its own.
//!
//! THE BUG THIS EXISTS FOR, seen 21 Sep 2026: a person allowed `open_url`, the continued run
//! then raised `request_user_form` for the login page, and NativeChat drew the card with its
//! Log in button grey forever. The answer route continued the run but never minted the form's
//! transcript entry, so the frame carried no `entryId` and there was nothing to submit to. A
//! fresh turn's form gets its entry from the run handler; the continuation now does the same.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
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

/// Round 1: a shell command the policy asks about. Round 2 (after the yes): a login form.
/// Round 3: done. Fresh call ids each round.
struct FormAfterShell {
    rounds: AtomicUsize,
}

#[async_trait]
impl ModelDoor for FormAfterShell {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst) + 1;
        let script = match round {
            1 => vec![
                ModelDelta::Text("checking the box".to_string()),
                ModelDelta::ToolCallStart {
                    id: "shell-1".to_string(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: "shell-1".to_string(),
                    delta: r#"{"command":"echo hi"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "shell-1".to_string(),
                },
            ],
            2 => vec![
                ModelDelta::Text("now the login".to_string()),
                ModelDelta::ToolCallStart {
                    id: "form-1".to_string(),
                    name: "request_user_form".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: "form-1".to_string(),
                    delta: r#"{"title":"Log in to The Internet","samePage":true,"submit":true,"fields":[{"id":"username","label":"Username","type":"text","required":true,"at":{"x":285,"y":348}},{"id":"password","label":"Password","type":"password","required":true,"at":{"x":285,"y":410}}]}"#.to_string(),
                },
                ModelDelta::ToolCallEnd {
                    id: "form-1".to_string(),
                },
            ],
            _ => vec![ModelDelta::Text("done".to_string())],
        };
        Ok(Box::pin(stream::iter(script.into_iter().map(Ok))))
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
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"form-after-answer-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(FormAfterShell {
            rounds: AtomicUsize::new(0),
        }),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(Arc::new(StubBox::default())),
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
        client: reqwest::Client::new(),
    }
}

impl Harness {
    fn token(&self, email: &str) -> String {
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

    async fn post(&self, token: &str, path: &str, body: &Value) -> Value {
        self.client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(body)
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("json")
    }

    /// The run waiting on this person whose card is NOT `after`.
    async fn wait_for_pending(&self, after: Option<&str>) -> (opengrok_core::id::RunId, String) {
        for _ in 0..100 {
            for id in self
                .store
                .awaiting_approval(&self.account)
                .await
                .expect("awaiting")
            {
                if let Ok((run, _)) = self.store.load_run(&id).await
                    && let Some(pending) = run.pending.as_ref()
                    && after != Some(pending.call_id.as_str())
                {
                    return (id, pending.call_id.clone());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("no run suspended for an approval in 10s");
    }
}

#[tokio::test]
async fn a_form_raised_after_an_answer_has_its_card_and_entry_id() {
    let database_url = database_or_skip!();
    let email = format!(
        "form-after-answer-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.token(&email);
    let hired = h
        .post(&token, "/coworkers", &json!({ "name": "Hex" }))
        .await;
    let agent = hired["id"].as_str().expect("coworker id").to_string();
    let coworker_id = CoworkerId::from_stored(agent.clone());
    let set = h
        .post(
            &token,
            &format!("/coworkers/{agent}/approvals"),
            &json!({ "tools": ["shell"] }),
        )
        .await;
    assert!(set["needsApproval"].as_array().is_some(), "{set}");

    // The turn parks on the shell card.
    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": agent,
            "runId": uuid::Uuid::now_v7().to_string(),
            "messages": [{ "id": "m1", "role": "user", "content": "log me in" }],
            "forwardedProps": { "coworkerId": agent },
        }))
        .send()
        .await
        .expect("turn");
    assert_eq!(res.status().as_u16(), 200);
    let _ = res.text().await;
    let (run_id, shell_call) = h.wait_for_pending(None).await;
    assert_eq!(shell_call, "shell-1");

    // The yes continues the run, which raises the form.
    let answered = h
        .post(
            &token,
            &format!("/ag-ui/runs/{}/answer", run_id.as_str()),
            &json!({ "call_id": shell_call, "approved": true }),
        )
        .await;
    assert_eq!(answered["continuing"], true, "{answered}");
    let (again, form_call) = h.wait_for_pending(Some(&shell_call)).await;
    assert_eq!(again, run_id, "the same run pauses again, on the form");
    assert_eq!(form_call, "form-1");

    // THE ASSERTION THE BUG FAILS: the form has a transcript card, with an id to submit to.
    let mut card = None;
    for _ in 0..50 {
        let entries = h
            .store
            .gateway_transcript(&coworker_id, &h.account)
            .await
            .expect("transcript");
        card = entries.into_iter().find(|entry| {
            entry.pointer("/message/type").and_then(Value::as_str) == Some("user-form")
                && entry.to_string().contains("form-1")
        });
        if card.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let card = card.expect("the continued run's form got no transcript card, so NativeChat has no entryId to submit to");
    let entry_id = card["id"].as_str().expect("entry id");
    assert!(entry_id.starts_with("e_"), "{card}");
    let fields = card
        .pointer("/message/formRequest/fields")
        .and_then(Value::as_array)
        .expect("fields on the card");
    assert_eq!(fields.len(), 2, "{card}");
    assert_eq!(
        fields[1]["at"]["y"], 410,
        "positions ride on the card: {card}"
    );
}
