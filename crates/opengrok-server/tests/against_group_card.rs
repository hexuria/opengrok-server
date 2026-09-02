//! A card raised INSIDE a room. A member's tool that needs a person's yes used to end that
//! member's turn with nothing said and leave its run waiting where nothing could see it. Now the
//! card is the member's, in the room's transcript under its name; the room pauses where the
//! round stood; and the answer — given naming the GROUP, as the desktop does — resumes that
//! member inside the room and then the members still to speak. Needs Postgres; skips loudly
//! without OG_DATABASE_URL.

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

#[allow(dead_code)]
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
        Arc::new(TokenMinter::new(b"group-card-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::room_speaker_asking_for_a_tool("Ada")),
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

impl Harness {
    /// Hire through the account API so the coworker gets the stub computer.
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

    async fn row(&self, id: &str) -> Value {
        let (_, rows) = self.api("listAgents", json!({})).await;
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
            .unwrap_or(Value::Null)
    }

    async fn wait_quiet(&self, group: &str) {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if self.row(group).await["isRunning"] == json!(false) {
                return;
            }
        }
        panic!("the room did not go quiet in 20s");
    }

    /// The room's lines: (author name, content) for every member message, in order.
    async fn said(&self, group: &str) -> Vec<(String, String)> {
        let (_, tail) = self
            .api(
                "getAgentTranscriptTail",
                json!({ "id": group, "limit": 200 }),
            )
            .await;
        tail["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| {
                        e["kind"] == "send-message"
                            && e["message"]["type"] == "text"
                            && e.get("author").is_some()
                    })
                    .map(|e| {
                        (
                            e["author"]["name"].as_str().unwrap_or("").to_string(),
                            e["message"]["content"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn entry(&self, group: &str, id: &str) -> Value {
        let (_, tail) = self
            .api(
                "getAgentTranscriptTail",
                json!({ "id": group, "limit": 200 }),
            )
            .await;
        tail["entries"]
            .as_array()
            .and_then(|entries| entries.iter().find(|e| e["id"] == id).cloned())
            .unwrap_or(Value::Null)
    }
}

#[tokio::test]
async fn a_card_inside_a_room_is_the_members_and_the_answer_finishes_the_round() {
    let database_url = database_or_skip!();
    let email = format!("group-card-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);

    // Ada's shell needs a person's yes; Bo just talks. Both in one room.
    let ada = h.hire(&token, "Ada").await;
    let bo = h.hire(&token, "Bo").await;
    let set: Value = h
        .client
        .post(format!("{}/coworkers/{ada}/approvals", h.base))
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
    let (status, created) = h
        .api(
            "createGroup",
            json!({ "name": "Pair", "description": "two who talk", "memberAgentIds": [ada, bo] }),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let group = created["agent"]["id"].as_str().expect("id").to_string();

    // A prompt to the room: Ada reaches for shell, the policy says ask — the card is Ada's, in
    // the ROOM, and the room is paused (not running, nobody active) with nothing run yet.
    let (status, sent) = h
        .api(
            "sendPrompt",
            json!({ "agentId": group, "prompt": "hello both", "clientNonce": "room-n1" }),
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    let card = h.wait_for_card(&group).await;
    assert_eq!(card["author"]["name"], json!("Ada"), "{card}");
    assert_eq!(card["author"]["id"], json!(ada), "{card}");
    let approval = &card["message"]["approval"];
    assert_eq!(approval["surface"], "box_shell", "{approval}");
    assert!(h.stub.ran().is_empty(), "nothing ran before the yes");
    h.wait_quiet(&group).await;
    let row = h.row(&group).await;
    assert_eq!(row["isRunning"], json!(false), "{row}");
    assert!(
        row["activeRemoteMemberId"].is_null(),
        "nobody is speaking while the room waits: {row}"
    );
    assert!(
        h.said(&group).await.is_empty(),
        "nobody spoke yet: {:?}",
        h.said(&group).await
    );
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    let request_id = approval["requestId"]
        .as_str()
        .expect("request id")
        .to_string();

    // The yes, naming the GROUP as the desktop does: Ada's command runs, Ada speaks after it,
    // and Bo — still to speak in that round — follows. The card flips and keeps its author.
    let (status, body) = h
        .api(
            "resolveAutoReviewApproval",
            json!({ "entryId": entry_id, "requestId": request_id, "resolution": "approved", "agentId": group }),
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
        "the approved command ran on Ada's computer: {ran:?}"
    );
    // Give the room a moment to start again, then wait for it to go quiet. The round Ada paused
    // finishes with Bo; the NEXT round (everybody again, rotated: Bo, then Ada) has Bo speak and
    // Ada reach for shell once more — a fresh run, a second card, the room paused again. That is
    // the client's loop, not a fault: a member that always wants the tool asks every round.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    h.wait_quiet(&group).await;
    let said = h.said(&group).await;
    assert_eq!(
        said,
        vec![
            ("Ada".to_string(), "Ada here, after the tool".to_string()),
            ("Bo".to_string(), "Bo here".to_string()),
            ("Bo".to_string(), "Bo here".to_string()),
        ],
        "Ada resumed inside the room, Bo finished the round, and the next round began"
    );
    let flipped = h.entry(&group, &entry_id).await;
    assert_eq!(
        flipped["message"]["approval"]["status"],
        json!("approved"),
        "{flipped}"
    );
    assert_eq!(
        flipped["author"]["name"],
        json!("Ada"),
        "the flip kept the author: {flipped}"
    );
    // (A second press on the first card is not tried here: the mock door gives every shell call
    // the same id, so once the next round's card is up the press would name that run instead.
    // The verb's exactly-once behaviour is `against_policy_card.rs`'s to prove.)
    let second = h.wait_for_card(&group).await;
    assert_ne!(
        second["id"],
        json!(entry_id),
        "a second card, from the next round"
    );
    assert_eq!(second["author"]["name"], json!("Ada"), "{second}");
    assert!(
        h.stub.ran().len() == 1,
        "one yes, one command: {:?}",
        h.stub.ran()
    );

    // A no on the second card: nothing more runs, the refusal reaches Ada as a result she
    // speaks after, and the room goes on (the next round pauses on her card again).
    let before = h.stub.ran().len();
    let entry_id = second["id"].as_str().unwrap().to_string();
    let request_id = second["message"]["approval"]["requestId"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = h
        .api(
            "resolveAutoReviewApproval",
            json!({ "entryId": entry_id, "requestId": request_id, "resolution": "denied", "agentId": group }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    h.wait_quiet(&group).await;
    assert_eq!(h.stub.ran().len(), before, "a no runs nothing");
    let said = h.said(&group).await;
    assert_eq!(said.len(), 4, "{said:?}");
    assert_eq!(
        said[3],
        ("Ada".to_string(), "Ada here, after the tool".to_string()),
        "the refusal reached Ada as a result, inside the room: {said:?}"
    );
    let denied = h.entry(&group, &entry_id).await;
    assert_eq!(
        denied["message"]["approval"]["status"],
        json!("denied"),
        "{denied}"
    );
    assert_eq!(denied["author"]["name"], json!("Ada"), "{denied}");
    let third = h.wait_for_card(&group).await;
    assert_ne!(
        third["id"],
        json!(entry_id),
        "the next round paused on Ada again"
    );
}
