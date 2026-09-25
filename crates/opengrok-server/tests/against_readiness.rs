//! `/ready` must say when the gateway will not take our token, and never say where it is.
//!
//! `/health` answers for the event store only, so a revoked or rotated OG_GATEWAY_TOKEN, or a
//! gateway that went away after boot, read `ok: true` until a turn failed (#185). The gateway
//! half of these tests needs no Postgres: the store is a lazy pool at a dead port, and what is
//! asserted is the gateway's field. The ready half needs a real database and skips without one.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_harness::{GatewayDoor, MockDoor, ModelDoor};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_server::spend::GuardedDoor;
use opengrok_store::PgStore;

fn state_over(store: PgStore, door: Arc<dyn ModelDoor>) -> AgUiState {
    AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"readiness-secret")),
            "ready@og.local".to_string(),
        ),
        door,
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    }
}

/// Serve the real router and ask `/ready` the way a prober does: no token, no Origin.
async fn ready_through_the_router(
    store: PgStore,
    door: Arc<dyn ModelDoor>,
) -> (u16, serde_json::Value) {
    let agui = state_over(store, door);
    let host = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui, host);
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
        .get(format!("{base}/ready"))
        .send()
        .await
        .expect("probe /ready");
    let status = response.status().as_u16();
    (status, response.json().await.expect("a json body"))
}

fn a_store_that_cannot_answer() -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(400))
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nothing")
        .expect("a lazily-connected pool");
    PgStore::new(pool)
}

/// A gateway that answers every request with `response`, whole.
async fn a_gateway_answering(response: &'static [u8]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(response).await;
        }
    });
    url
}

/// A gateway that answers every request with 401, the way it answers a revoked key.
async fn a_gateway_that_refuses_the_key() -> String {
    a_gateway_answering(
        b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await
}

/// A gateway that takes the key and lists its (empty) catalogue.
async fn a_gateway_that_takes_the_key() -> String {
    a_gateway_answering(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\
          connection: close\r\n\r\n{\"data\":[]}",
    )
    .await
}

/// The store half of the ready tests, or `None` (and a loud skip) without Postgres.
async fn a_live_store() -> Option<PgStore> {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return None;
    };
    let database_url = opengrok_store::gate_database_or_panic(database_url);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    Some(PgStore::new(pool))
}

/// Through the spend guard, as `main` wires it, so the guard's forwarding is what is tested.
fn guarded(door: GatewayDoor, store: &PgStore) -> Arc<dyn ModelDoor> {
    Arc::new(GuardedDoor::new(Arc::new(door), store.clone(), None))
}

#[tokio::test]
async fn a_refused_gateway_key_is_not_ready_and_the_address_stays_private() {
    let url = a_gateway_that_refuses_the_key().await;
    let store = a_store_that_cannot_answer();
    let door = guarded(GatewayDoor::new(url.clone(), "oag_live_revoked"), &store);
    let (status, body) = ready_through_the_router(store, door).await;

    assert_eq!(status, 503, "{body}");
    assert_eq!(body["ok"], serde_json::json!(false), "{body}");
    assert_eq!(body["gateway"], "refused", "{body}");
    assert_eq!(body["gatewayStatus"], 401, "{body}");
    let whole = body.to_string();
    let port = url.rsplit(':').next().unwrap_or_default();
    assert!(
        !whole.contains(port) && !whole.contains("oag_live"),
        "the reply named the gateway or the key: {body}"
    );
}

#[tokio::test]
async fn an_unreachable_gateway_is_not_ready() {
    let store = a_store_that_cannot_answer();
    let door = guarded(GatewayDoor::new("http://127.0.0.1:1", "oag_live_k"), &store);
    let (status, body) = ready_through_the_router(store, door).await;

    assert_eq!(status, 503, "{body}");
    assert_eq!(body["gateway"], "unreachable", "{body}");
    assert!(
        !body.to_string().contains("127.0.0.1"),
        "the reply named the gateway: {body}"
    );
}

#[tokio::test]
async fn a_live_store_and_a_door_with_no_gateway_are_ready() {
    let Some(store) = a_live_store().await else {
        return;
    };
    let (status, body) = ready_through_the_router(store, Arc::new(MockDoor::echoing())).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], serde_json::json!(true), "{body}");
    assert_eq!(body["store"], "ok", "{body}");
    assert_eq!(body["gateway"], "unused", "{body}");
}

/// The case the other tests leave out: the store answers and the gateway takes our key, through
/// the spend guard as `main` wires it. `gateway: "ok"` is the only answer a supervisor should
/// route traffic on.
#[tokio::test]
async fn a_live_store_and_a_gateway_that_takes_the_key_are_ready() {
    let Some(store) = a_live_store().await else {
        return;
    };
    let url = a_gateway_that_takes_the_key().await;
    let door = guarded(GatewayDoor::new(url, "oag_live_k"), &store);
    let (status, body) = ready_through_the_router(store, door).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], serde_json::json!(true), "{body}");
    assert_eq!(body["store"], "ok", "{body}");
    assert_eq!(body["gateway"], "ok", "{body}");
    assert_eq!(body["gatewayStatus"], serde_json::Value::Null, "{body}");
}
