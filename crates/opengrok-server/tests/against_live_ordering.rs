//! The two rules the desktop's replica imposes on `/events` stamps, proven over a real socket.
//!
//! 1. A frame written to one subscriber must not consume a sequence. Opening a second stream
//!    used to mint a roster sequence for its private snapshot, so the first stream saw N, N+2 and
//!    resynced the roster after every send. Here: open A, open B, emit one roster frame, and A's
//!    frame is exactly A's opening sequence + 1.
//! 2. Sequences reach the broadcast in the order they were minted, even when many tasks emit on
//!    one agent at once. Here: fifty tasks race `emit_transcript` on one agent and one subscriber
//!    receives 1..=50 in order.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::{GatewayState, live};
use opengrok_store::PgStore;

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

async fn state(database_url: &str) -> (axum::Router, GatewayState) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let auth = AuthState::new(
        PgStore::new(pool),
        Arc::new(TokenMinter::new(b"live-ordering-secret")),
        "host@og.local".to_string(),
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
        "host@og.local".to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    (opengrok_server::router(agui, gateway.clone()), gateway)
}

async fn spawn(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// Open `/events` on the `agents` channel and hand back the response to read frames from.
async fn open_events(client: &reqwest::Client, base: &str) -> reqwest::Response {
    let res = client
        .get(format!("{base}/events?channels=agents"))
        .header("authorization", "Bearer test-bearer")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("events");
    assert_eq!(res.status(), 200);
    res
}

/// Read chunks until a `data:` frame appears; return its JSON payload.
async fn next_frame(res: &mut reqwest::Response, buffer: &mut String) -> serde_json::Value {
    loop {
        if let Some(start) = buffer.find("data: ")
            && let Some(end) = buffer[start..].find("\n\n")
        {
            let line = buffer[start + 6..start + end].to_string();
            buffer.replace_range(..start + end + 2, "");
            return serde_json::from_str(&line).expect("frame json");
        }
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), res.chunk())
            .await
            .expect("a frame within 5s")
            .expect("chunk")
            .expect("bytes");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

fn roster_sequence(frame: &serde_json::Value) -> i64 {
    assert_eq!(
        frame["payload"]["ordered"]["replicaKey"], "roster",
        "{frame}"
    );
    frame["payload"]["ordered"]["sequence"]
        .as_i64()
        .expect("sequence")
}

#[tokio::test]
async fn opening_a_second_stream_leaves_no_gap_on_the_first() {
    let database_url = database_or_skip!();
    let (app, gateway) = state(&database_url).await;
    let base = spawn(app).await;
    let client = reqwest::Client::new();

    // Something has been emitted before anyone connects, so the opener has a real number to
    // report rather than zero.
    live::emit_roster(&gateway).await;

    let mut a = open_events(&client, &base).await;
    let mut a_buf = String::new();
    let a_open = next_frame(&mut a, &mut a_buf).await;
    assert_eq!(a_open["payload"]["coverage"]["kind"], "complete-roster");
    let a_seq = roster_sequence(&a_open);

    // A second subscriber opens: its private snapshot must not consume a sequence.
    let mut b = open_events(&client, &base).await;
    let mut b_buf = String::new();
    let b_open = next_frame(&mut b, &mut b_buf).await;
    assert_eq!(
        roster_sequence(&b_open),
        a_seq,
        "the opener reports the current sequence, not a fresh one"
    );

    // One real roster emit: A sees exactly the next number, B too.
    live::emit_roster(&gateway).await;
    let a_next = next_frame(&mut a, &mut a_buf).await;
    assert_eq!(roster_sequence(&a_next), a_seq + 1, "A saw a gap: {a_next}");
    let b_next = next_frame(&mut b, &mut b_buf).await;
    assert_eq!(roster_sequence(&b_next), a_seq + 1);
}

#[tokio::test]
async fn concurrent_emits_on_one_agent_arrive_in_sequence_order() {
    let database_url = database_or_skip!();
    let (_, gateway) = state(&database_url).await;
    let mut subscriber = gateway.events_tx.subscribe();

    let mut tasks = Vec::new();
    for i in 0..50 {
        let gateway = gateway.clone();
        tasks.push(tokio::spawn(async move {
            live::emit_transcript(
                &gateway,
                "agent-race",
                "appended",
                serde_json::json!({ "id": format!("e{i}") }),
            );
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }

    let mut seen = Vec::new();
    for _ in 0..50 {
        let (channel, payload) = subscriber.recv().await.expect("frame");
        assert_eq!(channel, "transcript");
        assert_eq!(payload["ordered"]["replicaKey"], "transcript:agent-race");
        seen.push(payload["ordered"]["sequence"].as_i64().expect("sequence"));
    }
    let expected: Vec<i64> = (1..=50).collect();
    assert_eq!(
        seen, expected,
        "frames reached the broadcast out of mint order"
    );
}
