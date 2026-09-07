//! `/health` must not answer `ok: true` when the event store is gone.
//!
//! This is the one endpoint the desktop supervisor probes to decide whether a host is still worth
//! using (`host-supervisor.ts` `fetchHealth`, on a 1500 ms deadline). The busy lookup used to end
//! in `.unwrap_or(false)`, so a TOTAL database outage — the case where this server can serve
//! nothing at all — degraded into "not busy" and still replied `200 {"ok": true}`. The supervisor
//! believed it and kept routing work to a host that could not accept any.
//!
//! The outage half of this test needs NO Postgres: it points a lazy pool at a dead port, which is
//! the only way to exercise the failure branch deterministically. The healthy half needs a real
//! database and skips without one — and it is the half that matters most, because a "fix" that
//! answered 503 unconditionally would satisfy the outage assertion perfectly.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_store::PgStore;

fn state_over(store: PgStore, email: &str) -> AgUiState {
    AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"health-outage-secret")),
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
    }
}

/// Serve the real router over `store` and ask it `/health` exactly as the supervisor does: no
/// Authorization header, no Origin.
async fn health_through_the_router(store: PgStore, email: &str) -> (u16, serde_json::Value) {
    let agui = state_over(store, email);
    let gateway = GatewayState::new(
        agui.clone(),
        Some("health-bearer".to_string()),
        email.to_string(),
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

    let response = reqwest::Client::new()
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("probe /health");
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.expect("a json body");
    (status, body)
}

/// A pool that parses, connects lazily, and can never actually reach anything. Port 1 refuses
/// immediately, so the query fails fast rather than sitting out an acquire timeout.
fn a_store_that_cannot_answer() -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(400))
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nothing")
        .expect("a lazily-connected pool");
    PgStore::new(pool)
}

#[tokio::test]
async fn a_dead_store_is_not_a_healthy_host() {
    let (status, body) = health_through_the_router(a_store_that_cannot_answer(), "outage@og.local")
        .await;

    assert_eq!(
        status, 503,
        "an unreachable event store must refuse the probe, not answer 200: {body}"
    );
    assert_eq!(
        body["ok"],
        serde_json::json!(false),
        "`ok` is the only field the supervisor reads; it must be false here: {body}"
    );
    assert!(
        body["reason"].as_str().unwrap_or_default().contains("not answering"),
        "fail closed AND say why — the reply must name the cause: {body}"
    );
    // The failure reply is unauthenticated, so it must not carry the store's own sentence: a sqlx
    // error can quote the connection string it failed to dial.
    let whole = body.to_string();
    assert!(
        !whole.contains("nobody") && !whole.contains("127.0.0.1:1"),
        "the reply leaked the connection string it failed to reach: {body}"
    );
}

#[tokio::test]
async fn a_live_store_still_answers_ok() {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");

    let (status, body) =
        health_through_the_router(PgStore::new(pool), "healthy@og.local").await;

    assert_eq!(
        status, 200,
        "a reachable store must still answer 200 — a fix that always 503s is not a fix: {body}"
    );
    assert_eq!(body["ok"], serde_json::json!(true), "{body}");
    assert!(
        body["isBusy"].is_boolean(),
        "the busy flag must survive the refactor as a real boolean: {body}"
    );
}
