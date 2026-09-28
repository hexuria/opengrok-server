//! A 502, 503 or 504 the server writes itself carries its sentence as `{"error": …}`.
//!
//! NativeChat reads any of the three as "the server is out of reach" unless the body says
//! otherwise, so a handler's plain-text refusal (a store that did not answer, a box that is
//! down) read to the person as the server being gone (NativeChat, reading the wire corpus). The
//! store here is a pool that never connects, which is what a database outage looks like from
//! a route; no database is needed.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::Value;

#[tokio::test]
async fn a_store_that_does_not_answer_is_a_sentence_not_a_bare_503() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(400))
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nothing")
        .expect("a lazily-connected pool");
    let minter = Arc::new(TokenMinter::new(b"an-outage-said-in-words"));
    let agui = AgUiState {
        auth: AuthState::new(
            PgStore::new(pool),
            minter.clone(),
            "host@og.local".to_string(),
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
        host_settings: None,
    };
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
    let token = minter
        .mint_access(
            "acct_outage",
            "sess-outage",
            "outage@og.local",
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");

    let response = reqwest::Client::new()
        .get(format!("{base}/artifacts?threadId=th-outage"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("list");
    assert_eq!(response.status().as_u16(), 503);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json")),
        "{:?}",
        response.headers()
    );
    let body: Value = response.json().await.expect("a JSON body");
    let sentence = body["error"].as_str().expect("an error sentence");
    assert!(!sentence.is_empty(), "{body}");
    assert_eq!(
        body.as_object().map(|object| object.len()),
        Some(1),
        "{body}"
    );
}
