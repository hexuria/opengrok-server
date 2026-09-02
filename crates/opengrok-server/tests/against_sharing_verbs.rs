//! The ten shared-room verbs, answered in the client's own shapes for a server that does not
//! serve shared rooms (`docs/plan-rooms.md` §0, §3). The renderer projects every state reply
//! through `projectSharingState` and throws "Sharing returned a malformed state" on anything
//! else, so the shape is the contract; the invite verbs get the host's `{status: "error",
//! message}`; typing gets nothing. Needs Postgres for the state; skips loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

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

async fn spawn(database_url: &str) -> String {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let email = "host@og.local".to_string();
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"sharing-verbs-test-secret")),
        email.clone(),
    );
    let agui = AgUiState {
        auth,
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
    );
    let app = opengrok_server::router(agui, gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

async fn call(base: &str, verb: &str, body: Value) -> (u16, Value) {
    let res = reqwest::Client::new()
        .post(format!("{base}/api/{verb}"))
        .header("Authorization", "Bearer test-bearer")
        .json(&body)
        .send()
        .await
        .expect("request");
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// `projectSharingState`, transcribed: what the renderer accepts and nothing looser.
fn is_sharing_state(value: &Value) -> bool {
    value["isEnabled"].is_boolean()
        && (value["selfAuthId"].is_null()
            || value["selfAuthId"].as_str().is_some_and(|s| !s.is_empty()))
        && value["pendingJoinRequests"].is_array()
        && value["rooms"].is_array()
        && value["typingUsers"].is_array()
}

#[tokio::test]
async fn every_shared_room_verb_answers_in_the_clients_own_shape() {
    let database_url = database_or_skip!();
    let base = spawn(&database_url).await;

    // The state verbs: the empty, disabled state — never `null`, never a looser record.
    for (verb, args) in [
        ("getSharingState", json!({})),
        (
            "respondToRoomJoinRequest",
            json!({ "requestId": "r1", "isApproved": true }),
        ),
        (
            "addOwnAgentToSharedRoom",
            json!({ "roomId": "room1", "agentId": "a1", "agentName": "Ada" }),
        ),
        (
            "removeOwnAgentFromSharedRoom",
            json!({ "roomId": "room1", "agentId": "a1" }),
        ),
        ("leaveSharedRoom", json!({ "roomId": "room1" })),
    ] {
        let (status, body) = call(&base, verb, args).await;
        assert_eq!(status, 200, "{verb}: {body}");
        assert!(
            is_sharing_state(&body),
            "{verb} is not a sharing state: {body}"
        );
        assert_eq!(body["isEnabled"], json!(false), "{verb}: {body}");
        assert_eq!(body["selfAuthId"], Value::Null, "{verb}: {body}");
    }

    // The verbs that make a room or an invite: the host's disabled error, in its words, which
    // `projectInviteResult` accepts and the renderer shows.
    for (verb, args) in [
        ("createRoomFromAgent", json!({ "agentId": "a1" })),
        ("createRoomInvite", json!({ "roomId": "room1" })),
        (
            "joinSharedRoom",
            json!({ "link": "https://example.test/join/x" }),
        ),
        ("createSharedRoom", json!({ "agents": [] })),
    ] {
        let (status, body) = call(&base, verb, args).await;
        assert_eq!(status, 200, "{verb}: {body}");
        assert_eq!(body["status"], json!("error"), "{verb}: {body}");
        assert_eq!(
            body["message"],
            json!("Sharing isn't enabled for your account."),
            "{verb}: {body}"
        );
    }

    // Typing is fire-and-forget; the host answers nothing.
    let (status, body) = call(
        &base,
        "setSharedRoomTyping",
        json!({ "roomId": "room1", "isTyping": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, Value::Null);
}
