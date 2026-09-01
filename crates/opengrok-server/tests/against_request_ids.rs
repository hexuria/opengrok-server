//! Every response carries an `X-Request-Id`: the caller's when it sent one, a fresh UUID when it
//! did not. This is what lets a desktop-client log line and a server log line for the same call
//! be joined by one key — the observability plan's first brick. Also opens and drops an `/events`
//! stream so the open/close logging path runs under test (the lines themselves go to tracing).
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
use opengrok_server::gateway::GatewayState;
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

async fn app(database_url: &str) -> axum::Router {
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
        Arc::new(TokenMinter::new(b"request-id-secret")),
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
    opengrok_server::router(agui, gateway)
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

#[tokio::test]
async fn every_response_carries_a_request_id_the_callers_or_a_fresh_one() {
    let database_url = database_or_skip!();
    let base = spawn(app(&database_url).await).await;
    let client = reqwest::Client::new();

    // No id sent: a UUID is minted and echoed.
    let res = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health");
    assert_eq!(res.status(), 200);
    let minted = res
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .expect("x-request-id on the response")
        .to_string();
    uuid::Uuid::parse_str(&minted).expect("a minted id is a UUID");

    // The caller's id wins, verbatim — the desktop client's log and ours must share it.
    let res = client
        .get(format!("{base}/health"))
        .header("x-request-id", "desk-0x1f")
        .send()
        .await
        .expect("health");
    assert_eq!(
        res.headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("desk-0x1f")
    );

    // A refused request still answers with the id: the refusal is what you want to find.
    let res = client
        .get(format!("{base}/events"))
        .header("x-request-id", "desk-refused")
        .send()
        .await
        .expect("events without bearer");
    assert_eq!(res.status(), 401);
    assert_eq!(
        res.headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("desk-refused")
    );

    // An SSE connect carries the id too, and dropping the body runs the close path.
    let mut res = client
        .get(format!("{base}/events?channels=agents"))
        .header("authorization", "Bearer test-bearer")
        .header("x-request-id", "desk-sse")
        .send()
        .await
        .expect("events");
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("desk-sse")
    );
    let first = res.chunk().await.expect("first chunk").expect("some bytes");
    assert!(String::from_utf8_lossy(&first).starts_with("retry: 1000"));
    drop(res);
    // Give the server a beat to drop the body and log the close; no panic is the assertion.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}
