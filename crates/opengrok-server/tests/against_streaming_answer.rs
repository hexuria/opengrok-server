//! The bubble grows while the model is still talking.
//!
//! THIS TEST EXISTS BECAUSE ITS ABSENCE IS WHY THE BUG SURVIVED. The server appended an entry
//! marked `streaming: true`, then awaited the whole run, glued every delta into one string and
//! updated that entry exactly ONCE — so the flag was a promise it never kept. Every assertion in
//! the suite read the transcript after the turn ended, where a buffered answer and a streamed one
//! are identical. A correct implementation and a broken one both passed.
//!
//! So this asserts on what can only be true of a live stream: that a PARTIAL answer was visible
//! while the run was still going, and that it grew. Then it checks the end state the client needs
//! — the whole answer, with `streaming` absent.
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

/// The answer entry as it stands right now: its content, and whether it still says streaming.
async fn answer_now(client: &reqwest::Client, base: &str, agent: &str) -> Option<(String, bool)> {
    let (_, tail) = api(
        client,
        base,
        "getAgentTranscriptTail",
        json!({ "id": agent, "limit": 50 }),
    )
    .await;
    tail["entries"].as_array().and_then(|entries| {
        entries.iter().rev().find_map(|entry| {
            (entry["kind"] == "send-message").then(|| {
                (
                    entry["message"]["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    entry["streaming"] == json!(true),
                )
            })
        })
    })
}

#[tokio::test]
async fn the_answer_arrives_in_pieces_and_ends_whole() {
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
    let email = format!("streaming-{}@og.local", now_ms());
    seed_account(&store, &email).await;

    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"streaming-secret")),
            email.clone(),
        ),
        // PACED ON PURPOSE. An unpaced mock door replays a script already in memory, so the whole
        // answer would be produced inside one millisecond and a streamed implementation would be
        // indistinguishable from a buffered one — which is the very confusion this test exists to
        // end.
        door: Arc::new(MockDoor::echoing().paced_by_ms(PACE_MS)),
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
    // Not an identity test: it speaks as the deployment account.
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

    let (status, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let agent = created["agent"]["id"].as_str().expect("id").to_string();

    let (status, sent) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");

    // Watch the bubble. Every distinct non-empty content seen WHILE the entry still says
    // streaming is a partial answer that a buffered implementation could never have produced.
    let mut partials: Vec<String> = Vec::new();
    let mut finished: Option<String> = None;
    for _ in 0..400 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let Some((said, streaming)) = answer_now(&client, &base, &agent).await else {
            continue;
        };
        if streaming {
            if !said.is_empty() && partials.last() != Some(&said) {
                partials.push(said);
            }
        } else if !said.is_empty() {
            finished = Some(said);
            break;
        }
    }

    let whole = finished.expect("the turn must finish and clear `streaming`");

    assert!(
        !partials.is_empty(),
        "no partial answer was ever visible: the bubble was written once at the end, which is \
         exactly the bug this test exists to catch. Final answer was {whole:?}"
    );
    assert!(
        partials.len() >= 2,
        "the bubble must GROW, not appear once complete — saw only {partials:?}"
    );

    // Monotonic: an answer that shrank would mean a later flush overwrote an earlier one with
    // less text, which the client would render as the coworker deleting its own words.
    for pair in partials.windows(2) {
        assert!(
            pair[1].starts_with(&pair[0]),
            "each write must extend the last: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }

    // The last thing the client is told is the whole answer, with the flag gone — the shape a
    // healthy turn has always ended with, and the one recovery imitates when it heals a dead run.
    let longest = partials.last().expect("checked above");
    assert!(
        whole.starts_with(longest.trim_end()) || whole.len() >= longest.len(),
        "the final answer must contain what was streamed: {longest:?} then {whole:?}"
    );
    assert!(
        whole.contains("pong?"),
        "the echoing door repeats the prompt, so this is the whole answer: {whole:?}"
    );
}

/// The roster says a coworker is working WHILE it works.
///
/// The client draws its green "working" dot straight off `isRunning` on the roster row, and reads
/// it both from an `agent-upserted` frame and from a `listAgents` re-read. Verification against the
/// packaged app found the dot never appearing during a streaming reply, so this asks the question
/// from the server side: mid-turn, does `listAgents` say the coworker is running?
#[tokio::test]
async fn the_roster_says_a_coworker_is_working_while_it_works() {
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
    let email = format!("running-{}@og.local", now_ms());
    seed_account(&store, &email).await;

    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"running-secret")),
            email.clone(),
        ),
        door: Arc::new(MockDoor::echoing().paced_by_ms(PACE_MS)),
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

    let (status, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let agent = created["agent"]["id"].as_str().expect("id").to_string();

    let (status, sent) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");

    let mut saw_running = false;
    let mut saw_activity = false;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let (_, list) = api(&client, &base, "listAgents", json!({})).await;
        let Some(row) = list
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)))
        else {
            continue;
        };
        if row["isRunning"] == json!(true) {
            saw_running = true;
            // ANY verb, not `thinking` specifically. This asserted the constant back when there
            // was one, which meant it would have gone red for the RIGHT change — and did. What it
            // means to test is that a running row carries a label at all, because that is what the
            // header reads; which label is the subject of `against_activity_verbs`.
            saw_activity |= row["currentActivity"]["kind"].is_string();
        }
        // Stop once the turn is over, so a pass cannot come from a later turn.
        if row["isRunning"] == json!(false) && saw_running {
            break;
        }
    }

    assert!(
        saw_running,
        "the roster never said the coworker was running during its own turn — the client's green \
         working dot reads `isRunning` off this row, so it can never appear"
    );
    assert!(
        saw_activity,
        "a running row must carry currentActivity, which is what the header's label reads"
    );

    // And it must come back down, or the dot would stick on forever.
    let mut settled = false;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let (_, list) = api(&client, &base, "listAgents", json!({})).await;
        if list
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)))
            .is_some_and(|row| row["isRunning"] == json!(false))
        {
            settled = true;
            break;
        }
    }
    assert!(settled, "isRunning must clear when the turn ends");
}

/// Read chunks until a `data:` frame appears; return its JSON payload.
async fn next_frame(
    res: &mut reqwest::Response,
    buffer: &mut String,
    within: std::time::Duration,
) -> Option<Value> {
    loop {
        if let Some(start) = buffer.find("data: ")
            && let Some(end) = buffer[start..].find("\n\n")
        {
            let line = buffer[start + 6..start + end].to_string();
            buffer.replace_range(..start + end + 2, "");
            return serde_json::from_str(&line).ok();
        }
        let chunk = tokio::time::timeout(within, res.chunk())
            .await
            .ok()?
            .ok()??;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// The working state reaches an OPEN STREAM, not only a re-read.
///
/// `listAgents` reporting `isRunning` mid-turn is necessary and not sufficient: the desktop draws
/// its dot from an `agent-upserted` frame when the coworker is already in its roster. #63 made
/// every live frame addressed per account, so this asserts the roster channel's audience actually
/// includes the person whose coworker is running — the transcript channel taking the account
/// directly while the roster channel derives it is exactly where the two could diverge.
#[tokio::test]
async fn the_working_state_reaches_an_open_stream() {
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
    let email = format!("upserted-{}@og.local", now_ms());
    seed_account(&store, &email).await;

    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"upserted-secret")),
            email.clone(),
        ),
        door: Arc::new(MockDoor::echoing().paced_by_ms(PACE_MS)),
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

    let (status, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let agent = created["agent"]["id"].as_str().expect("id").to_string();

    let mut events = client
        // BOTH CHANNELS. `agents` carries the full-roster snapshot; the single-row delta that
        // actually drives the working dot rides `agent-upserted` (`client-grok-bot.md` §4, row 4:
        // "show thinking" is an `agent-upserted` with `isRunning:true`). Subscribing to `agents`
        // alone reads as a total delivery failure and is really a wrong subscription — which is
        // how this test first failed.
        .get(format!("{base}/events?channels=agents,agent-upserted"))
        .header("authorization", "Bearer test-bearer")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("events");
    assert_eq!(events.status(), 200);
    let mut buffer = String::new();
    // The opener's own snapshot, so what follows is genuinely live.
    next_frame(&mut events, &mut buffer, std::time::Duration::from_secs(5))
        .await
        .expect("the opening snapshot");

    let (status, sent) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");

    let mut running_frame: Option<Value> = None;
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..60 {
        let Some(frame) =
            next_frame(&mut events, &mut buffer, std::time::Duration::from_secs(3)).await
        else {
            break;
        };
        seen.push(frame["channel"].as_str().unwrap_or("?").to_string());
        let row = if frame["payload"]["agent"]["id"] == json!(agent) {
            frame["payload"]["agent"].clone()
        } else {
            frame["payload"]["agents"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)))
                .cloned()
                .unwrap_or(Value::Null)
        };
        if row["isRunning"] == json!(true) {
            running_frame = Some(row);
            break;
        }
    }

    let row = running_frame.unwrap_or_else(|| {
        panic!(
            "no live frame ever said the coworker was running; the client draws its working dot \
             from exactly this. Channels seen: {seen:?}"
        )
    });
    assert_eq!(
        row["currentActivity"]["kind"],
        json!("thinking"),
        "a running row must carry the activity the header labels: {row}"
    );
}
