//! The `/events` opener with the database gone. It used to turn a failed roster read into an
//! EMPTY complete-roster snapshot, stamped current — and every reconnecting desktop installed
//! it as its baseline and showed "no coworkers" (timed twice on 2 Sep 2026 with the container
//! stopped). An empty success is the dangerous reply: with the store down the opener is the
//! retry line alone, and the roster read itself answers 500, never an empty array.
//!
//! No Postgres needed — that is the point. The pool is connected lazily to a port nothing
//! listens on, so every read fails the way a dead database fails.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_store::PgStore;
use serde_json::json;

async fn spawn_with_a_dead_store() -> String {
    // Port 1 answers nobody; the pool only tries when a query runs.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/nowhere")
        .expect("a lazy pool");
    let store = PgStore::new(pool);
    let email = "host@og.local".to_string();
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"events-down-test-secret")),
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

#[tokio::test]
async fn with_the_store_down_the_opener_sends_no_roster_and_the_read_says_so() {
    let base = spawn_with_a_dead_store().await;
    let client = reqwest::Client::new();

    // The stream opens (the retry line is mandatory) and carries NO complete-roster frame.
    let mut res = client
        .get(format!("{base}/events?channels=agents"))
        .header("authorization", "Bearer test-bearer")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("open events");
    assert_eq!(res.status(), 200);
    let mut body = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2500);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, res.chunk()).await {
            Ok(Ok(Some(chunk))) => body.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }
    assert!(
        body.starts_with("retry: 1000\n\n"),
        "the retry line first: {body:?}"
    );
    assert!(
        !body.contains("complete-roster"),
        "a roster the server did not read must not be sent as complete: {body:?}"
    );
    assert!(
        !body.contains("\"agents\":[]"),
        "and never an empty roster stamped current: {body:?}"
    );

    // The read itself is a 500 in words, never an empty array.
    let res = client
        .post(format!("{base}/api/listAgents"))
        .header("authorization", "Bearer test-bearer")
        .json(&json!({}))
        .send()
        .await
        .expect("listAgents");
    assert_eq!(res.status(), 500);
    let text = res.text().await.expect("body");
    assert!(text.contains("roster unavailable"), "{text}");
}
