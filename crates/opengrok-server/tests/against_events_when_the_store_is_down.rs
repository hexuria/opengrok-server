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

// ---- the store comes back ------------------------------------------------------------------

/// A TCP relay to the real Postgres that refuses new connections AND cuts the live ones while
/// `open` says false — the database "down" and "back" under the test's control, without
/// stopping anybody's container. Cutting the live ones matters: a pooled connection that stayed
/// up would answer the read the outage is meant to fail.
async fn relay_to(target: String, open: tokio::sync::watch::Receiver<bool>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                return;
            };
            if !*open.borrow() {
                drop(inbound);
                continue;
            }
            let target = target.clone();
            let mut open = open.clone();
            tokio::spawn(async move {
                let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await else {
                    return;
                };
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                    () = async {
                        while open.changed().await.is_ok() {
                            if !*open.borrow() {
                                break;
                            }
                        }
                    } => {}
                }
            });
        }
    });
    port
}

/// The `host:port` of `postgres://u:p@host:port/db`, and the same URL pointed at the relay.
fn authority(database_url: &str) -> (String, usize, usize) {
    let at = database_url.rfind('@').expect("an authority");
    let rest = &database_url[at + 1..];
    let slash = rest.find('/').unwrap_or(rest.len());
    (rest[..slash].to_string(), at + 1, at + 1 + slash)
}

async fn read_for(res: &mut reqwest::Response, ms: u64) -> String {
    let mut body = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, res.chunk()).await {
            Ok(Ok(Some(chunk))) => body.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }
    body
}

#[tokio::test]
async fn a_stream_opened_during_the_outage_is_seeded_once_the_store_is_back() {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let (open_tx, open_rx) = tokio::sync::watch::channel(true);
    let (target, from, to) = authority(&database_url);
    let port = relay_to(target, open_rx).await;
    let relayed = format!(
        "{}127.0.0.1:{port}{}",
        &database_url[..from],
        &database_url[to..]
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect_lazy(&relayed)
        .expect("a lazy pool");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrate through the relay");
    let email = "host@og.local".to_string();
    let auth = AuthState::new(
        PgStore::new(pool),
        Arc::new(TokenMinter::new(b"events-back-test-secret")),
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
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    // Down.
    open_tx.send(false).expect("relay listening");
    let client = reqwest::Client::new();
    let mut res = client
        .get(format!("{base}/events?channels=agents"))
        .header("authorization", "Bearer test-bearer")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("open events");
    assert_eq!(res.status(), 200);
    let opener = read_for(&mut res, 1500).await;
    assert!(opener.starts_with("retry: 1000\n\n"), "{opener:?}");
    assert!(!opener.contains("complete-roster"), "{opener:?}");

    // Back: the same stream is seeded with a complete roster, stamped, within the retry ladder.
    open_tx.send(true).expect("relay listening");
    let mut seeded = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
    while tokio::time::Instant::now() < deadline && !seeded.contains("complete-roster") {
        seeded.push_str(&read_for(&mut res, 1000).await);
    }
    assert!(
        seeded.contains("\"coverage\":{\"kind\":\"complete-roster\"}"),
        "no late snapshot after the store came back: {seeded:?}"
    );
    assert!(seeded.contains("\"replicaKey\":\"roster\""), "{seeded:?}");
}
