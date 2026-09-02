//! A reply keeps its link. The page sends `replyToId` on `sendPrompt` and draws its "quoted"
//! header from `entry.replyTo`; the server used to read the id only for the acceptance digest
//! and drop it, so the header vanished the moment the stream echo replaced the optimistic
//! bubble. Now the user's entry carries `replyTo`, the model reads what was replied to ahead of
//! the prompt, and the answer carries the same link (Cursor parity — the answer bubble shows the
//! same header). Needs Postgres; skips loudly without OG_DATABASE_URL.

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
        Arc::new(TokenMinter::new(b"reply-test-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        // Echoes the last user message it was given: what the model was shown is what it says.
        door: Arc::new(MockDoor::echoing()),
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

    async fn is_running(&self, id: &str) -> bool {
        let (_, rows) = self.api("listAgents", json!({})).await;
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id))
            .is_some_and(|row| row["isRunning"] == json!(true))
    }

    /// `sendPrompt` with the given extra arguments, then wait for the turn to end.
    async fn turn(&self, agent: &str, prompt: &str, extra: Value) -> String {
        let nonce = format!("p-{}-{}", prompt.len(), now_ms());
        let mut args = json!({ "agentId": agent, "prompt": prompt, "clientNonce": nonce });
        if let (Some(target), Some(overlay)) = (args.as_object_mut(), extra.as_object()) {
            for (key, value) in overlay {
                target.insert(key.clone(), value.clone());
            }
        }
        let (status, sent) = self.api("sendPrompt", args).await;
        assert_eq!(status, 200, "{sent}");
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if !self.is_running(agent).await {
                return nonce;
            }
        }
        panic!("the turn did not end in 20s");
    }

    async fn tail(&self, agent: &str) -> Vec<Value> {
        let (_, tail) = self
            .api(
                "getAgentTranscriptTail",
                json!({ "id": agent, "limit": 200 }),
            )
            .await;
        tail["entries"].as_array().cloned().unwrap_or_default()
    }
}

fn answers(entries: &[Value]) -> Vec<&Value> {
    entries
        .iter()
        .filter(|e| {
            e["kind"] == "send-message"
                && e["message"]["type"] == "text"
                && !e["message"]["content"].as_str().unwrap_or("").is_empty()
        })
        .collect()
}

#[tokio::test]
async fn a_reply_keeps_its_link_and_the_model_reads_what_was_replied_to() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, &format!("reply-{}@og.local", now_ms())).await;
    let agent = h.hire("Quill").await;

    // A first exchange to reply to.
    h.turn(&agent, "the first thing", json!({})).await;
    let entries = h.tail(&agent).await;
    let first_user = entries
        .iter()
        .find(|e| e["kind"] == "message" && e["content"] == "the first thing")
        .expect("the first user entry")
        .clone();
    let first_answer = answers(&entries)
        .last()
        .expect("the first answer")
        .to_owned()
        .clone();
    assert!(first_user.get("replyTo").is_none(), "{first_user}");
    let answer_id = first_answer["id"].as_str().expect("id").to_string();

    // Watch the stream: the echo that settles the optimistic bubble must carry the link.
    let mut stream = h
        .client
        .get(format!("{}/events?channels=transcript", h.base))
        .header("authorization", "Bearer test-bearer")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("open events");

    // A reply to the answer.
    let nonce = h
        .turn(&agent, "and the second", json!({ "replyToId": answer_id }))
        .await;
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !seen.contains(&nonce) {
        match tokio::time::timeout_at(deadline, stream.chunk()).await {
            Ok(Ok(Some(chunk))) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }
    let echo = seen
        .split("\n\n")
        .find(|frame| frame.contains(&nonce))
        .expect("the echo carrying the clientNonce");
    assert!(
        echo.contains(&format!("\"replyTo\":\"{answer_id}\"")),
        "the echo dropped the link: {echo}"
    );

    // Durable: the tail carries the link on the user's entry and on the answer's.
    let entries = h.tail(&agent).await;
    let reply = entries
        .iter()
        .find(|e| e["kind"] == "message" && e["content"] == "and the second")
        .expect("the reply entry");
    assert_eq!(reply["replyTo"], json!(answer_id), "{reply}");
    let second_answer = answers(&entries)
        .last()
        .expect("the second answer")
        .to_owned();
    assert_eq!(
        second_answer["replyTo"],
        json!(answer_id),
        "{second_answer}"
    );
    // The mock echoes the last user message it was shown: the quoted answer came first.
    let said = second_answer["message"]["content"].as_str().unwrap_or("");
    assert!(
        said.contains("[Replying to your earlier message: \"You said: the first thing"),
        "the model was not shown what was replied to: {said}"
    );
    assert!(said.contains("and the second"), "{said}");

    // A reply to one's own message names it as such; a link to nothing adds nothing.
    let own_id = first_user["id"].as_str().expect("id").to_string();
    h.turn(&agent, "and a third", json!({ "replyToId": own_id }))
        .await;
    let entries = h.tail(&agent).await;
    let third = answers(&entries)
        .last()
        .expect("the third answer")
        .to_owned();
    assert_eq!(third["replyTo"], json!(own_id));
    let said = third["message"]["content"].as_str().unwrap_or("");
    assert!(
        said.contains("[Replying to their own earlier message: \"the first thing\"]"),
        "{said}"
    );
    h.turn(
        &agent,
        "and a fourth",
        json!({ "replyToId": "no-such-entry" }),
    )
    .await;
    let entries = h.tail(&agent).await;
    let fourth = answers(&entries)
        .last()
        .expect("the fourth answer")
        .to_owned();
    assert_eq!(
        fourth["replyTo"],
        json!("no-such-entry"),
        "the link is kept as sent"
    );
    let said = fourth["message"]["content"].as_str().unwrap_or("");
    assert!(said.starts_with("You said: and a fourth"), "{said}");
}
