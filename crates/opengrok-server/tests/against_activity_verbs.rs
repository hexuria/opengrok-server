//! The activity label says what the coworker is doing.
//!
//! `live.rs` set `{"kind":"thinking"}` whenever a turn was running and never refined or cleared it,
//! so the desktop showed "Thinking" next to a bubble already typing the answer, for the whole
//! reply. The client has always parsed the richer shape (`{kind, tool, detail, ...}`) and falls
//! back to "Working" for a verb it does not know — so nothing was missing but a value that changed.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

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

/// Slow enough that a partial answer is observable, fast enough that the test is not a wait.
///
/// `echo_script` emits one delta per word and the sink flushes every 250 ms, so ~15 words at this
/// pace is about a second of turn and a handful of intermediate writes.
const PACE_MS: u64 = 60;

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

async fn api(client: &reqwest::Client, base: &str, method: &str, body: Value) -> (u16, Value) {
    let res = client
        .post(format!("{base}/api/{method}"))
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

/// The roster row for one coworker.
async fn row(client: &reqwest::Client, base: &str, agent: &str) -> Value {
    let (_, list) = api(client, base, "listAgents", json!({})).await;
    list.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)).cloned())
        .expect("the coworker must be on the roster")
}

/// The label says what the coworker is doing, and changes when that changes.
///
/// It was `{"kind":"thinking"}` for the whole of every turn, so the desktop showed "Thinking"
/// beside a bubble already typing the answer — a statement the screen contradicts, and the same
/// class of untruth as a `streaming: true` flag on a bubble nobody grew.
///
/// SAMPLED DURING THE TURN, because that is the only place the difference exists: a constant and a
/// state machine are identical once the turn is over and the field is gone. The door is paced so
/// the turn lasts long enough to sample at all.
#[tokio::test]
async fn the_activity_label_says_what_the_coworker_is_doing() {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let email = format!("activity-{}@og.local", now_ms());
    seed_account(&store, &email).await;

    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"activity-secret")),
            email.clone(),
        ),
        // `asking_for_a_tool` reaches for a tool and then answers, so one turn walks the whole
        // vocabulary: thinking, tool, thinking again, writing.
        door: Arc::new(MockDoor::asking_for_a_tool().paced_by_ms(PACE_MS)),
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
        email,
        Some("http://opengrok.lan:1447".to_string()),
    )
    .allowing_identity_fallback();
    let app = opengrok_server::router(agui, gateway);
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
    let client = reqwest::Client::new();

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Verb", "clientNonce": format!("hire-{}", now_ms()) }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();

    let (status, _) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "go", "clientNonce": format!("p-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200);

    let mut seen: Vec<String> = Vec::new();
    let mut ran = false;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let row = row(&client, &base, &agent).await;
        let running = row["isRunning"] == json!(true);
        ran |= running;
        if running {
            let kind = row["currentActivity"]["kind"]
                .as_str()
                .unwrap_or("<absent>")
                .to_string();
            if seen.last() != Some(&kind) {
                seen.push(kind);
            }
        }
        if ran && !running {
            // The verb must not outlive the turn.
            assert!(
                row["currentActivity"].is_null(),
                "a finished turn carries no activity: {row}"
            );
            break;
        }
    }

    assert!(ran, "the turn must have been observably in flight");
    assert!(
        seen.contains(&"writing".to_string()),
        "the label must say `writing` once words are arriving — otherwise it contradicts the \
         bubble filling underneath it. Saw: {seen:?}"
    );
    assert!(
        seen.len() >= 2,
        "the label must CHANGE during a turn; a constant is what was wrong. Saw: {seen:?}"
    );
    assert_eq!(
        seen.first().map(String::as_str),
        Some("thinking"),
        "a turn that has said nothing yet is thinking: {seen:?}"
    );
}
