//! Unread state on the roster row, and the verb that sets it.
//!
//! `summaries.rs` hard-coded `hasUnread: false` and `unreadCount: 0` and set both `lastViewedAt`
//! and `lastActivityAt` to the row's `updatedAt` — which the official renderer reads as "this
//! person has seen everything, always". Its "New" separator anchors at `lastViewedAt` whenever
//! `lastActivityAt` is greater, so it could never appear, and no row could carry a badge.
//!
//! `setAgentUnread` existed as a no-op that answered `Null`. Its argument names here are
//! transcribed from the shipped bundle by the client session — `{id, isUnread, atMs?}`, reply
//! void — not chosen by us.
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

/// The roster row for one coworker, as this caller sees it.
async fn row(client: &reqwest::Client, base: &str, agent: &str) -> Value {
    let (_, list) = api(client, base, "listAgents", json!({})).await;
    list.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == json!(agent)).cloned())
        .expect("the coworker must be on the roster")
}

/// Wait for the turn to land, so the coworker has actually said something to be unread about.
async fn wait_for_an_answer(client: &reqwest::Client, base: &str, agent: &str) {
    for _ in 0..200 {
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

#[tokio::test]
async fn the_roster_carries_real_unread_state() {
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
    let email = format!("unread-{}@og.local", now_ms());
    let account_id = seed_account(&store, &email).await;
    let agui_store = store.clone();

    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"unread-secret")),
            email.clone(),
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

    // A coworker that has never spoken has nothing unread, and has never been viewed.
    let fresh = row(&client, &base, &agent).await;
    assert_eq!(fresh["unreadCount"], json!(0), "{fresh}");
    assert_eq!(fresh["hasUnread"], json!(false), "{fresh}");
    assert_eq!(
        fresh["lastViewedAt"],
        json!(0),
        "never viewed must read as never — the renderer treats a non-positive stamp that way, and \
         the old hard-coded `updatedAt` claimed the opposite: {fresh}"
    );

    // It answers a prompt the person just sent. THAT IS NOT UNREAD: typing to a coworker means
    // looking at it, and its reply lands in the chat in front of them. The first version of this
    // test asserted the opposite — "the coworker spoke and nobody has read it" — which is the
    // exact badge the follow-up removed: a blue marker on the very conversation being read,
    // masking the green working dot for the whole run.
    let (status, sent) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "pong?", "clientNonce": format!("p-{}", now_ms()) }),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    wait_for_an_answer(&client, &base, &agent).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let answered = row(&client, &base, &agent).await;
    assert_eq!(
        answered["unreadCount"],
        json!(0),
        "an answer to the person's own prompt is read, not unread: {answered}"
    );
    assert!(
        answered["lastActivityAt"].as_i64().unwrap_or(0) > 0,
        "lastActivityAt must move with the newest entry: {answered}"
    );

    // Something lands while they are NOT looking — appended fresh, outside any turn, so no arrival
    // rule can see it. This is what a badge is for.
    {
        let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
        agui_store
            .append_gateway_entry(
                &coworker,
                &account_id,
                &json!({
                    "kind": "send-message",
                    "id": format!("later-{}", now_ms()),
                    "message": { "type": "text", "content": "while you were away" },
                    "timestampMs": now_ms(),
                }),
                // Stamped NOW, not in the future: Mark as Read clamps its stamp to now, so a row
                // dated ahead of the clock could never be marked read — which is a test defect,
                // not a badge. The sleep above already puts this after the turn's arrival stamp.
                now_ms(),
            )
            .await
            .expect("append");
    }
    let spoken = row(&client, &base, &agent).await;
    assert!(
        spoken["unreadCount"].as_i64().unwrap_or(0) >= 1,
        "an utterance that landed while nobody was looking must be unread: {spoken}"
    );
    assert_eq!(spoken["hasUnread"], json!(true), "{spoken}");

    // Mark as Read — official's `isUnread: false`.
    let (status, reply) = api(
        &client,
        &base,
        "setAgentUnread",
        json!({ "id": agent, "isUnread": false }),
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        reply,
        Value::Null,
        "the reply is declared void; the row travels on the roster frame: {reply}"
    );

    let read = row(&client, &base, &agent).await;
    assert_eq!(read["unreadCount"], json!(0), "{read}");
    assert_eq!(read["hasUnread"], json!(false), "{read}");
    assert!(
        read["lastViewedAt"].as_i64().unwrap_or(0) > 0,
        "reading must stamp a moment: {read}"
    );

    // Mark as Unread — `isUnread: true` — puts exactly the newest utterance back.
    let (status, reply) = api(
        &client,
        &base,
        "setAgentUnread",
        json!({ "id": agent, "isUnread": true }),
    )
    .await;
    assert_eq!(status, 200, "{reply}");

    let unread = row(&client, &base, &agent).await;
    assert_eq!(
        unread["unreadCount"],
        json!(1),
        "marking unread must restore exactly the newest utterance, not the whole history: {unread}"
    );
    assert_eq!(unread["hasUnread"], json!(true), "{unread}");

    // A DELIBERATE MARK-UNREAD SURVIVES THE APP LOOKING AT THE ROW. The desktop records a view
    // when a coworker is opened or focused; without the manual flag that view lands a moment later
    // and wipes the badge the person just asked for, before they can look away.
    {
        let store = &agui_store;
        let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
        store
            .record_incidental_view(&coworker, &account_id, now_ms())
            .await
            .expect("incidental view");
    }
    let after_open = row(&client, &base, &agent).await;
    assert_eq!(
        after_open["unreadCount"],
        json!(1),
        "opening the coworker must NOT clear a deliberate Mark as Unread: {after_open}"
    );

    // An explicit Mark as Read does clear it — that is the person overriding themselves.
    let (status, _) = api(
        &client,
        &base,
        "setAgentUnread",
        json!({ "id": agent, "isUnread": false }),
    )
    .await;
    assert_eq!(status, 200);
    let after_read = row(&client, &base, &agent).await;
    assert_eq!(after_read["unreadCount"], json!(0), "{after_read}");

    // and a later incidental view stays harmless once the manual flag is gone.
    api(
        &client,
        &base,
        "setAgentUnread",
        json!({ "id": agent, "isUnread": false }),
    )
    .await;

    // `atMs` is honoured, and cannot be used to mark the future read.
    let (status, _) = api(
        &client,
        &base,
        "setAgentUnread",
        json!({ "id": agent, "isUnread": false, "atMs": now_ms() + 86_400_000i64 }),
    )
    .await;
    assert_eq!(status, 200);
    let clamped = row(&client, &base, &agent).await;
    assert!(
        clamped["lastViewedAt"].as_i64().unwrap_or(i64::MAX) <= now_ms() + 1_000,
        "a client clock ahead of ours must not mark unseen entries read: {clamped}"
    );

    // A verb that names a coworker must be refused for a stranger rather than answered.
    let (status, refused) = api(
        &client,
        &base,
        "setAgentUnread",
        json!({ "id": "cw_does_not_exist", "isUnread": false }),
    )
    .await;
    assert!(
        status == 200 && refused == Value::Null,
        "an unknown id answers this verb's own void shape, never a 404 that would tell a stranger \
         a real id from an invented one: {status} {refused}"
    );
}
