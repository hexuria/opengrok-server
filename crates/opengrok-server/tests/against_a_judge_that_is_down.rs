//! A run whose auto-review judge cannot answer (#201).
//!
//! The judge is billed and capped on the coworker's own key, so a coworker at its spend cap has
//! its judge refused on every call. Each refusal is `Unavailable`, which fails closed as a card
//! — and every card used to say the same "the reviewer did not answer", once per tool call, for
//! the whole run. The card now says why, and after a few failures in a row the run stops asking:
//! the next reviewed call is refused in words the model passes on, without calling the judge.
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
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{Run, RunStatus};
use opengrok_harness::{
    DeltaStream, JUDGE_MARKER, ModelDelta, ModelDoor, ModelError, ModelRequest,
};
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

/// A box that records the commands that reached it.
#[derive(Default)]
struct ShellBox {
    ran: Mutex<Vec<String>>,
}

#[async_trait]
impl Computer for ShellBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_judge_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _box_id: &str, command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
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
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
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

/// The coworker's model runs `ls` with a fresh call id each round for four rounds, then passes
/// on what it was told. The judge's route answers as a capped coworker's does: refused, with the
/// cap's sentence. (A model that retries a refusal instead is stopped by the loop's own
/// same-refusal guard, whose error carries the same sentence.)
#[derive(Default)]
struct CappedModel {
    rounds: AtomicUsize,
    judged: AtomicUsize,
}

#[async_trait]
impl ModelDoor for CappedModel {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with(JUDGE_MARKER))
        {
            self.judged.fetch_add(1, Ordering::SeqCst);
            return Err(ModelError::SpendCap(
                "Ada has reached her monthly spend limit".to_string(),
            ));
        }
        let round = self.rounds.fetch_add(1, Ordering::SeqCst) + 1;
        let script = if round > 4 {
            vec![ModelDelta::Text(
                "the reviewer is down, so I stopped".to_string(),
            )]
        } else {
            let id = format!("ls-{round}");
            vec![
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: "shell".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: r#"{"command":"ls"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ]
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
    shell_box: Arc<ShellBox>,
    model: Arc<CappedModel>,
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
    let shell_box = Arc::new(ShellBox::default());
    let model = Arc::new(CappedModel::default());
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"judge-down-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        door: model.clone(),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/judge".to_string(),
        computer: Some(shell_box.clone()),
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
        shell_box,
        model,
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

    async fn hire(&self, token: &str, name: &str) -> String {
        let hired: Value = self
            .client
            .post(format!("{}/coworkers", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("hire")
            .json()
            .await
            .expect("hire json");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    async fn queue(&self, token: &str) -> Value {
        self.client
            .get(format!("{}/ag-ui/approvals", self.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("approvals")
            .json()
            .await
            .expect("approvals json")
    }

    async fn turn(&self, token: &str, agent: &str, prompt: &str) -> String {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
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

    /// The run waiting on a card whose call id is not `after`, or the run itself once it ended:
    /// a run that stops asking must be seen to end, not waited on for a card that never comes.
    async fn next_card_or_end(&self, run: Option<&RunId>, after: Option<&str>) -> (Run, RunId) {
        for _ in 0..100 {
            if let Some(run_id) = run {
                let (loaded, _) = self.store.load_run(run_id).await.expect("run");
                if loaded.status.is_terminal()
                    || loaded
                        .pending
                        .as_ref()
                        .is_some_and(|p| Some(p.call_id.as_str()) != after)
                {
                    return (loaded, run_id.clone());
                }
            } else {
                for id in self
                    .store
                    .awaiting_approval(&self.account)
                    .await
                    .expect("awaiting")
                {
                    let (loaded, _) = self.store.load_run(&id).await.expect("run");
                    if loaded.pending.is_some() {
                        return (loaded, id);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the run neither asked again nor ended in 10s");
    }

    async fn answer_yes(&self, token: &str, run_id: &RunId, call_id: &str) {
        let answered: Value = self
            .client
            .post(format!(
                "{}/ag-ui/runs/{}/answer",
                self.base,
                run_id.as_str()
            ))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({ "call_id": call_id, "approved": true }))
            .send()
            .await
            .expect("answer")
            .json()
            .await
            .expect("answer json");
        assert_eq!(answered["approved"], true, "{answered}");
    }
}

/// Three cards that each say why, then no more: the fourth reviewed call is refused in words,
/// the judge is not called for it, and the run ends instead of parking again.
#[tokio::test]
async fn a_judge_that_is_down_is_named_then_stops_being_asked() {
    let database_url = database_or_skip!();
    let email = format!("judge-down-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.token(&email);
    let agent = h.hire(&token, "Ada").await;
    h.store
        .set_auto_review_policy(
            h.account.as_str(),
            "global",
            "",
            Some(true),
            None,
            Some("never touch prod"),
            now_ms(),
        )
        .await
        .expect("auto-review on");

    let sse = h.turn(&token, &agent, "list the files").await;
    assert!(sse.contains("run-awaiting-approval"), "{sse}");
    let (run, run_id) = h.next_card_or_end(None, None).await;
    let first = run.pending.expect("the first card").call_id;
    let queue = h.queue(&token).await;
    let item = queue
        .as_array()
        .and_then(|items| items.iter().find(|item| item["callId"] == json!(first)))
        .cloned()
        .unwrap_or_else(|| panic!("the call is on the queue: {queue}"));
    let why = item["why"].as_str().unwrap_or_default();
    assert!(why.contains("did not answer"), "{item}");
    assert!(
        why.contains("spend limit"),
        "the card says WHY the reviewer did not answer: {item}"
    );

    let mut answered = first;
    let mut cards = 1;
    loop {
        h.answer_yes(&token, &run_id, &answered).await;
        let (run, _) = h.next_card_or_end(Some(&run_id), Some(&answered)).await;
        match run.pending {
            Some(pending) if !run.status.is_terminal() => {
                cards += 1;
                assert!(cards <= 3, "a fourth card for a judge that is down");
                answered = pending.call_id;
            }
            _ => {
                assert_eq!(run.status, RunStatus::Finished, "{:?}", run.status);
                let refused: Vec<&Value> = run
                    .emitted
                    .iter()
                    .filter(|event| {
                        event["type"] == json!("TOOL_CALL_RESULT")
                            && event["content"]
                                .as_str()
                                .is_some_and(|content| content.starts_with("refused:"))
                    })
                    .collect();
                assert_eq!(refused.len(), 1, "round four: {refused:?}");
                for result in refused {
                    let content = result["content"].as_str().unwrap_or_default();
                    assert!(content.contains("reviewer is down"), "{content}");
                }
                break;
            }
        }
    }
    assert_eq!(cards, 3);
    assert_eq!(
        h.model.judged.load(Ordering::SeqCst),
        3,
        "the judge is not called again once it is known to be down"
    );
    assert_eq!(
        h.shell_box.ran.lock().expect("ran").len(),
        3,
        "only the three calls a person approved ran"
    );
}
