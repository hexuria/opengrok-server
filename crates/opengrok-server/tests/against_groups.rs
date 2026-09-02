//! Groups (`docs/plan-rooms.md` §2): a coworker with members, whose turn is the client's own
//! orchestrator run on the server. Through the real router with a mock door that behaves as a
//! member — it says "{name} here" through the room's `SendMessage` tool and then stops — so the
//! rounds, the rotation, the mentions and the caps are what a person would actually see in the
//! group's transcript. Needs Postgres; skips loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
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

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
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
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
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
    seed_account(&store, email).await;
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"groups-test-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::room_speaker()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
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
        client: reqwest::Client::new(),
    }
}

impl Harness {
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

    async fn hire(&self, name: &str) -> String {
        let (status, created) = self
            .api(
                "createAgent",
                json!({ "name": name, "clientNonce": format!("hire-{name}-{}", now_ms()) }),
            )
            .await;
        assert_eq!(status, 200, "{created}");
        created["agent"]["id"].as_str().expect("id").to_string()
    }

    async fn row(&self, id: &str) -> Value {
        let (_, rows) = self.api("listAgents", json!({})).await;
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
            .unwrap_or(Value::Null)
    }

    /// Send a prompt to the group and wait for the room to go quiet.
    async fn room_turn(&self, group: &str, prompt: &str) {
        let (status, sent) = self
            .api(
                "sendPrompt",
                json!({ "agentId": group, "prompt": prompt, "clientNonce": format!("p-{}-{}", group, now_ms()) }),
            )
            .await;
        assert_eq!(status, 200, "{sent}");
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let row = self.row(group).await;
            if row["isRunning"] == json!(false) {
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
                    .filter(|e| e["kind"] == "send-message" && e.get("author").is_some())
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
}

#[tokio::test]
async fn a_group_is_a_coworker_with_members_that_answer_in_turns() {
    let database_url = database_or_skip!();
    let email = format!("groups-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let ada = h.hire("Ada").await;
    let bob = h.hire("Bob").await;

    // createGroup answers the createAgent shape, with the roster's group fields set.
    let (status, created) = h
        .api(
            "createGroup",
            json!({ "name": "Pair", "description": "two who talk", "memberAgentIds": [ada, bob] }),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let group = created["agent"]["id"].as_str().expect("id").to_string();
    assert_eq!(created["agent"]["isGroup"], json!(true), "{created}");
    assert_eq!(
        created["agent"]["memberIds"],
        json!([ada, bob]),
        "{created}"
    );
    assert_eq!(
        created["agent"]["description"],
        json!("two who talk"),
        "{created}"
    );
    assert!(created["transcript"].is_array(), "{created}");
    // The same members, in another order, are the same group — not a twin.
    let (status, again) = h
        .api(
            "createGroup",
            json!({ "name": "Twin", "memberAgentIds": [bob, ada, bob] }),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(
        again["agent"]["id"],
        json!(group),
        "same member set ⇒ the existing group"
    );
    // A group inside a group is refused in the client's own words; no members is refused.
    let (status, nested) = h
        .api(
            "createGroup",
            json!({ "name": "Nest", "memberAgentIds": [group, ada] }),
        )
        .await;
    assert_eq!(status, 400, "{nested}");
    assert!(
        nested["error"]
            .as_str()
            .unwrap_or("")
            .contains("only contain individual agents"),
        "{nested}"
    );
    let (status, _) = h
        .api(
            "createGroup",
            json!({ "name": "Empty", "memberAgentIds": ["cw_nobody"] }),
        )
        .await;
    assert_eq!(status, 400);
    // setGroupMembers: a stranger's id and the group itself are dropped; a non-group is null.
    let (status, summary) = h
        .api(
            "setGroupMembers",
            json!({ "id": group, "memberAgentIds": [bob, group, "cw_nobody"] }),
        )
        .await;
    assert_eq!(status, 200, "{summary}");
    assert_eq!(summary["memberIds"], json!([bob]), "{summary}");
    let (status, summary) = h
        .api(
            "setGroupMembers",
            json!({ "id": group, "memberAgentIds": [ada, bob] }),
        )
        .await;
    assert_eq!(status, 200, "{summary}");
    assert_eq!(summary["memberIds"], json!([ada, bob]), "{summary}");
    let (_, not_a_group) = h
        .api(
            "setGroupMembers",
            json!({ "id": ada, "memberAgentIds": [bob] }),
        )
        .await;
    assert_eq!(not_a_group, Value::Null);

    // A prompt to the room: three rounds, everybody speaks each round (the mock member always
    // has something to say), in an order that rotates by round — six lines, under six names.
    h.room_turn(&group, "hello room").await;
    let lines = h.said(&group).await;
    let names: Vec<&str> = lines.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        ["Ada", "Bob", "Bob", "Ada", "Ada", "Bob"],
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|(name, content)| *content == format!("{name} here")),
        "{lines:?}"
    );
    let row = h.row(&group).await;
    assert_eq!(row["isRunning"], json!(false), "{row}");
    assert!(
        row["activeRemoteMemberId"].is_null(),
        "cleared when the room is quiet: {row}"
    );
    assert!(
        row["computerError"].is_null(),
        "a group needs no computer, so the account's provisioning error is not its problem: {row}"
    );

    // A mention narrows the round to the one named, until the next user message.
    h.room_turn(&group, "@bob only you please").await;
    let lines = h.said(&group).await;
    let after: Vec<&str> = lines[6..].iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(after, ["Bob", "Bob", "Bob"], "{lines:?}");

    // A retired member drops out of the room; the rest carry on.
    let (status, deleted) = h.api("deleteAgents", json!({ "ids": [ada] })).await;
    assert_eq!(status, 200, "{deleted}");
    h.room_turn(&group, "anyone there?").await;
    let lines = h.said(&group).await;
    let after: Vec<&str> = lines[9..].iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(after, ["Bob", "Bob", "Bob"], "{lines:?}");
}
