//! A match on each of auto-review's three lists does what the list says, through a real turn on a
//! real server (#354): the coworker's model asks for one shell call, the judge answers, and the
//! server acts. Allow runs the call; ask-first raises the auto-review card, quoting the
//! instruction; block is a refusal the model reads as a result, with no card and no run on the
//! box. The judge is shown the lists the store resolved for THIS coworker, global's with the Bot's
//! own ask-first list over it, not the ones the route was last given.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::{DeltaStream, JUDGE_MARKER, MockDoor, ModelDoor, ModelError, ModelRequest};
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
        Ok(format!("bx_ask_{}", uuid::Uuid::now_v7().simple()))
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

/// The repo's own mock door (`OG_MODEL_DOOR=mock-tools`: every turn asks for one `shell` call,
/// then reports it), with its judge answering the canned word, and a record of what the judge was
/// shown: the one user message that carries the three lists.
struct Door {
    inner: MockDoor,
    judged: Mutex<Vec<String>>,
}

#[async_trait]
impl ModelDoor for Door {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let judge = request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with(JUDGE_MARKER));
        if judge {
            let shown = request.messages.first().map(|m| m.content.clone());
            self.judged
                .lock()
                .expect("judged")
                .push(shown.unwrap_or_default());
        }
        self.inner.stream(request).await
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
    store: PgStore,
    account: AccountId,
    shell_box: Arc<ShellBox>,
    door: Arc<Door>,
    client: reqwest::Client,
    token: String,
}

/// A server whose judge answers `verdict`, and an account signed in to it.
async fn harness(database_url: &str, verdict: &str) -> Harness {
    let email = format!("three-lists-{}@og.local", uuid::Uuid::now_v7().simple());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, &email).await;
    let shell_box = Arc::new(ShellBox::default());
    let door = Arc::new(Door {
        inner: MockDoor::asking_for_a_tool().with_judge_verdict(verdict),
        judged: Mutex::new(Vec::new()),
    });
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"three-lists-secret")),
        email.clone(),
    );
    let agui = AgUiState {
        auth,
        door: door.clone(),
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
    let token = agui
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-three-lists",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        store,
        account,
        shell_box,
        door,
        client: reqwest::Client::new(),
        token,
    }
}

impl Harness {
    /// One call, signed in; the status and the body as JSON.
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let res = request.send().await.expect("request");
        let status = res.status().as_u16();
        let body = res.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn put(&self, body: Value) -> u16 {
        self.call(reqwest::Method::PUT, "/auto-review/policy", Some(body))
            .await
            .0
    }

    async fn hire(&self, name: &str) -> String {
        let hired = json!({ "name": name });
        let (status, hired) = self
            .call(reqwest::Method::POST, "/coworkers", Some(hired))
            .await;
        assert_eq!(status, 201, "hire {name}: {hired}");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    /// The approvals queue: one item per call waiting on a person.
    async fn queue(&self) -> Vec<Value> {
        let (status, queue) = self
            .call(reqwest::Method::GET, "/ag-ui/approvals", None)
            .await;
        assert_eq!(status, 200, "{queue}");
        queue.as_array().cloned().unwrap_or_default()
    }

    async fn turn(&self, agent: &str) -> String {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .json(&json!({
                "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": "m1", "role": "user", "content": "run it" }],
                "forwardedProps": { "coworkerId": agent },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
        res.text().await.expect("sse")
    }
}

/// Each judge word against the same policy: global writes all three lists, and the Bot writes
/// only its own ask-first list, so the judge must be shown the Bot's over global's.
#[tokio::test]
async fn each_list_acts_on_a_real_turn_and_the_judge_is_shown_the_resolved_lists() {
    let database_url = database_or_skip!();
    for verdict in ["allow", "ask", "block", "unsure"] {
        let h = harness(&database_url, verdict).await;
        let bot = h.hire("Ada").await;
        let saved = h
            .put(json!({
                "scopeKind": "global", "scopeId": "", "enabled": true,
                "allowInstructions": "echo is fine",
                "askInstructions": "check with me before writing files",
                "blockInstructions": "never delete backups",
            }))
            .await;
        assert_eq!(saved, 204);
        let own = h
            .put(json!({
                "scopeKind": "coworker", "scopeId": bot, "enabled": null,
                "allowInstructions": null,
                "askInstructions": "check with me before redirecting output",
                "blockInstructions": null,
            }))
            .await;
        assert_eq!(own, 204);

        let sse = h.turn(&bot).await;
        let ran = h.shell_box.ran.lock().expect("ran").clone();
        let queue = h.queue().await;

        let shown = h.door.judged.lock().expect("judged").clone();
        assert_eq!(
            shown.len(),
            1,
            "{verdict}: the judge is asked once: {shown:?}"
        );
        assert!(
            shown[0].contains("BLOCK INSTRUCTIONS:\nnever delete backups\n")
                && shown[0]
                    .contains("ASK-FIRST INSTRUCTIONS:\ncheck with me before redirecting output\n")
                && shown[0].ends_with("ALLOW INSTRUCTIONS:\necho is fine"),
            "{verdict}: {}",
            shown[0]
        );

        match verdict {
            "allow" => {
                assert_eq!(ran.len(), 1, "allow runs the call: {sse}");
                assert!(queue.is_empty(), "{queue:?}");
                assert!(!sse.contains("run-awaiting-approval"), "{sse}");
            }
            "ask" | "unsure" => {
                assert!(ran.is_empty(), "{verdict} must not reach the box: {ran:?}");
                assert!(sse.contains("run-awaiting-approval"), "{sse}");
                let why = queue
                    .first()
                    .map_or("", |item| item["why"].as_str().unwrap_or(""));
                assert_eq!(queue.len(), 1, "{queue:?}");
                if verdict == "ask" {
                    assert!(
                        why.contains("asked to check this first")
                            && why.contains("check with me before redirecting output")
                            && !why.contains("before writing files"),
                        "the card quotes the Bot's own list, not global's: {why}"
                    );
                } else {
                    assert!(why.contains("did not clearly allow"), "{why}");
                    assert!(!why.contains("check this first"), "{why}");
                }
            }
            _ => {
                assert!(ran.is_empty(), "block must not reach the box: {ran:?}");
                assert!(queue.is_empty(), "block raises no card: {queue:?}");
                assert!(!sse.contains("run-awaiting-approval"), "{sse}");
                assert!(
                    sse.contains("auto-review blocked this")
                        && sse.contains("your block instructions say: ")
                        && sse.contains("never delete backups"),
                    "the refusal reaches the model as a result: {sse}"
                );
                assert!(
                    sse.contains("RUN_FINISHED"),
                    "the run goes on to its end: {sse}"
                );
            }
        }
        // Nothing in this account's policy was changed by running a turn.
        let rows = h
            .store
            .auto_review_rows(h.account.as_str())
            .await
            .expect("rows");
        assert_eq!(rows.len(), 2);
    }
}
