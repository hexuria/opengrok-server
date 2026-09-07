//! A message arriving in the chat you are watching is not unread.
//!
//! Verified against the packaged app: with unread state live, the sidebar dot was BLUE for 58 of
//! 60 samples during a streaming reply. Not a regression — a precedence rule. The client's status
//! projection ranks the unread marker ABOVE "working", so every streamed `send-message` bumped the
//! count on the very chat being read and masked the green working dot for the whole run.
//!
//! Official's rule is `markActiveSessionArrival` (`session-runtime.ts:149`): an utterance landing
//! in the session the person currently has open and focused marks it viewed instead of raising the
//! count. Views recorded this way are INCIDENTAL — they must never clear a deliberate Mark as
//! Unread.
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

/// Wait until the arrival stamp has landed, rather than sleeping a guess.
///
/// THE TURN'S LAST ACT IS NOT THE ANSWER. `note_arrival` runs AFTER the final entry is written, so
/// `wait_for_an_answer` returning says nothing about whether the stamp has been made — and a fixed
/// sleep after it is a bet on how fast the machine is. That bet lost on CI: 400 ms was enough on
/// this desk and not on a loaded runner, and it took a green PR red on `main` after the merge.
/// Polling the condition costs the same on a fast box and does not fail on a slow one.
async fn wait_until_read(
    store: &PgStore,
    coworker: &opengrok_core::id::CoworkerId,
    account: &AccountId,
) -> i64 {
    let mut unread = -1;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if let Ok(state) = store.unread_state(coworker, account).await {
            unread = state.unread;
            if unread == 0 {
                return 0;
            }
        }
    }
    unread
}

/// The same, for the roster row the client actually reads.
async fn wait_until_row_read(client: &reqwest::Client, base: &str, agent: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        last = row(client, base, agent).await;
        if last["unreadCount"] == json!(0) {
            return last;
        }
    }
    last
}

/// The roster row for one coworker, as this caller sees it.
async fn row(client: &reqwest::Client, base: &str, agent: &str) -> Value {
    let (_, list) = api(client, base, "listAgents", json!({})).await;
    list.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)).cloned())
        .expect("the coworker must be on the roster")
}

async fn wait_for_an_answer(client: &reqwest::Client, base: &str, agent: &str) {
    for _ in 0..300 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, tail) = api(
            client,
            base,
            "getAgentTranscriptTail",
            json!({ "id": agent, "limit": 50 }),
        )
        .await;
        let done = tail["entries"].as_array().is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry["kind"] == json!("send-message")
                    && entry["streaming"] != json!(true)
                    && entry["message"]["content"]
                        .as_str()
                        .is_some_and(|said| !said.is_empty())
            })
        });
        if done {
            return;
        }
    }
    panic!("the turn never produced an answer");
}

async fn serve(store: PgStore, email: &str) -> String {
    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"arrival-secret")),
            email.to_string(),
        ),
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
    base
}

#[tokio::test]
async fn an_answer_in_the_chat_you_are_watching_is_not_unread() {
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
    let email = format!("arrival-{}@og.local", now_ms());
    seed_account(&store, &email).await;
    let base = serve(store, &email).await;
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

    // The person opens the chat, which is what the desktop does, and is now watching it.
    let (status, _) = api(&client, &base, "openAgent", json!({ "id": agent })).await;
    assert_eq!(status, 200);

    // The coworker answers, into the chat they are looking at.
    let (status, sent) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    wait_for_an_answer(&client, &base, &agent).await;
    // The arrival stamp lands just AFTER the final entry, so wait for the condition rather than
    // for a clock: a fixed sleep is a bet on the machine, and that bet took `main` red once.
    let watched = wait_until_row_read(&client, &base, &agent).await;

    // The answer itself cannot out-date the view: `update_gateway_entry` leaves `at_ms` alone, so
    // the row keeps the placeholder's stamp from the moment the prompt was sent. What USED to
    // raise the badge here is the placeholder — an empty `send-message` row appended a
    // millisecond after a Mark as Read — which is why sending now records a view.
    assert_eq!(
        watched["unreadCount"],
        json!(0),
        "sending must not raise a badge on your own chat: the streaming placeholder is itself a \
         `send-message` row, appended just after any Mark as Read: {watched}"
    );
    assert_eq!(watched["hasUnread"], json!(false), "{watched}");
    assert_eq!(
        watched["isActive"],
        json!(true),
        "the chat being watched is the active one for THIS viewer: {watched}"
    );

    // A DELIBERATE Mark as Unread still survives an arrival. The person said unread; an incidental
    // view must not overrule them, however many messages land afterwards.
    let (status, _) = api(
        &client,
        &base,
        "setAgentUnread",
        json!({ "id": agent, "isUnread": true }),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "again?", "clientNonce": format!("p2-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200);
    wait_for_an_answer(&client, &base, &agent).await;
    // A deliberate unread must SURVIVE. Give the stamp that would clear it every chance to land —
    // `wait_until_row_read` polls until the count reaches zero and gives up after its bound — and
    // then assert it did not. Waiting for the wrong outcome is the honest way to test a negative:
    // a fixed sleep would pass here by being too short.
    let still = wait_until_row_read(&client, &base, &agent).await;
    assert!(
        still["unreadCount"].as_i64().unwrap_or(0) >= 1,
        "a deliberate Mark as Unread must survive an arrival in the same chat: {still}"
    );
}

/// One person watching a coworker must not mark another person's copy read.
///
/// A shared coworker has one transcript per person, so "is anyone looking at this" is not a
/// question that has one answer. With a single global slot it did.
#[tokio::test]
async fn watching_your_own_chat_does_not_read_somebody_elses() {
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
    let stamp = now_ms();
    let mine = format!("arrival-mine-{stamp}@og.local");
    let mine_id = seed_account(&store, &mine).await;
    let theirs_id = seed_account(&store, &format!("arrival-theirs-{stamp}@og.local")).await;
    let base = serve(store.clone(), &mine).await;
    let client = reqwest::Client::new();

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{stamp}") }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    // Somebody else has an unread message on their own copy of this coworker.
    store
        .append_gateway_entry(
            &coworker,
            &theirs_id,
            &json!({
                "kind": "send-message",
                "id": format!("theirs-{stamp}"),
                "message": { "type": "text", "content": "for them" },
                "timestampMs": now_ms(),
            }),
            now_ms(),
        )
        .await
        .expect("append");

    // I open and read mine.
    api(&client, &base, "openAgent", json!({ "id": agent })).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let theirs = store
        .unread_state(&coworker, &theirs_id)
        .await
        .expect("their unread");
    assert_eq!(
        theirs.unread, 1,
        "my opening a chat must not mark another reader's copy read"
    );
    let ours = store
        .unread_state(&coworker, &mine_id)
        .await
        .expect("my unread");
    assert_eq!(
        ours.unread, 0,
        "and mine is read, because I am looking at it"
    );
}

/// An utterance appended DURING a turn, while the person watches, is marked read by the arrival
/// rule.
///
/// This is the case the arrival rule is actually for, and it is not the coworker's own answer:
/// that answer updates the placeholder in place and `update_gateway_entry` does not touch `at_ms`,
/// so it can never be newer than the view recorded when the prompt was sent. Entries appended
/// FRESH during a turn can be — the fixture drain does exactly that, and so does any card — and
/// those are what light a badge on a chat being read.
#[tokio::test]
async fn an_entry_appended_while_watching_is_marked_read() {
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
    let stamp = now_ms();
    let email = format!("arrival-during-{stamp}@og.local");
    let account = seed_account(&store, &email).await;
    let base = serve(store.clone(), &email).await;
    let client = reqwest::Client::new();

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{stamp}") }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    // The person is watching this chat.
    api(&client, &base, "openAgent", json!({ "id": agent })).await;

    // Something the coworker says lands as a NEW row while the turn runs — a fixture drain or a
    // card, stamped now, after the view.
    store
        .append_gateway_entry(
            &coworker,
            &account,
            &json!({
                "kind": "send-message",
                "id": format!("arrived-{stamp}"),
                "message": { "type": "text", "content": "landed mid-turn" },
                "timestampMs": now_ms() + 50,
            }),
            now_ms() + 50,
        )
        .await
        .expect("append");

    // Unread until the turn ends, because nothing has told the roster it was seen.
    let before = store
        .unread_state(&coworker, &account)
        .await
        .expect("before");
    assert_eq!(
        before.unread, 1,
        "the fresh entry must start unread, or this test proves nothing"
    );

    // A turn runs and finishes; its arrival rule marks what landed while the person watched.
    let (status, _) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{stamp}") }),
    )
    .await;
    assert_eq!(status, 200);
    wait_for_an_answer(&client, &base, &agent).await;
    let after_unread = wait_until_read(&store, &coworker, &account).await;
    assert_eq!(
        after_unread, 0,
        "an entry that landed while the person was watching must be marked read when the turn \
         ends — otherwise the badge paints blue over the working dot for the whole run"
    );
}

/// Sending without opening still counts as looking.
///
/// The `viewing` map lives in memory, so a restart forgets who had what open — and the desktop
/// does not necessarily re-open a chat before its next prompt. Without recording a view on send,
/// the arrival rule has nothing to match against and the person's own answer raises a badge on the
/// chat they are typing into. A prompt is the least ambiguous evidence there is: the client cannot
/// send one from a chat it does not have open.
#[tokio::test]
async fn sending_without_opening_still_counts_as_looking() {
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
    let stamp = now_ms();
    let email = format!("arrival-nosend-{stamp}@og.local");
    let account = seed_account(&store, &email).await;
    let base = serve(store.clone(), &email).await;
    let client = reqwest::Client::new();

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Quill", "clientNonce": format!("hire-{stamp}") }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    // DELIBERATELY NO `openAgent`. This is the post-restart shape: the server has forgotten who
    // was looking at what, and the next thing it hears is a prompt.
    let (status, _) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{stamp}") }),
    )
    .await;
    assert_eq!(status, 200);
    wait_for_an_answer(&client, &base, &agent).await;
    let after_unread = wait_until_read(&store, &coworker, &account).await;
    assert_eq!(
        after_unread, 0,
        "typing to a coworker means looking at it: the answer must not arrive as unread on the \
         chat the person is typing into"
    );
}
