//! The `fail` fixture makes `sendPrompt` actually fail.
//!
//! Asked for by the client session, which needed the desktop's failed-send path reachable from a
//! dev machine: every other mock fixture succeeds, so the bubble left behind by a refused send
//! could only be exercised by breaking the server for real. Their constraint, verbatim — "reject
//! with a real failure envelope rather than a 200 carrying an error field. My path keys on the RPC
//! rejecting, so a 200 with `{error}` inside would sail past it and look like a successful send."
//!
//! Two things this pins that are easy to get wrong later:
//!   - the status must be 4xx, not 5xx. The client reads `< 500` as a command error to show the
//!     person and never retry, `>= 500` as "unreachable" and retries with backoff
//!     (`client-grok-bot.md:165`). A 5xx would stage a transport blip, not a refusal.
//!   - the nonce must NOT be consumed. Refusing after the acceptance ledger would let the client's
//!     retry dedupe into `accepted: true`, so the fixture would refuse once and then silently
//!     succeed — worse than not existing.
//!
//! RUNNING THIS ONE TAKES AN EXTRA VARIABLE, and it is not set by `cargo test` or by
//! `scripts/gate.sh`. `enabled()` also requires a mock door, which a test cannot set for itself —
//! `set_var` is unsafe in edition 2024 and the workspace forbids `unsafe_code` — so without it
//! this skips rather than fails, exactly as `against_upload_round_trip.rs` does:
//!
//! ```sh
//! OG_MODEL_DOOR=mock-cards cargo test -p opengrok-server --features mock-fixtures \
//!   --test against_a_refused_send
//! ```
//!
//! The contract itself is pinned by the unit tests beside `refusal_for` in `mock_fixtures.rs`,
//! which have no such requirement and DO run everywhere. What only this file can prove is the
//! end-to-end half: that the status reaches the wire, that nothing is appended, and that the
//! nonce survives to be refused a second time.
//!
//! Needs Postgres too; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
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

#[tokio::test]
async fn the_fail_fixture_refuses_the_send_itself() {
    if !opengrok_server::gateway::mock_fixtures::enabled() {
        eprintln!("skipping: built without the mock-fixtures catalogue");
        return;
    }
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
    let email = format!("refused-{stamp}@og.local");
    let account = seed_account(&store, &email).await;

    let agui = AgUiState {
        auth: AuthState::new(
            store.clone(),
            Arc::new(TokenMinter::new(b"refused-send-secret")),
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
        email.clone(),
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

    let (_, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Refuser", "clientNonce": format!("hire-{stamp}") }),
    )
    .await;
    let agent = created["agent"]["id"].as_str().expect("id").to_string();
    let coworker = CoworkerId::from_stored(agent.clone());

    // ---- the refusal itself ----
    let nonce = format!("fail-{stamp}");
    let (status, body) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "fail", "clientNonce": nonce }),
    )
    .await;

    assert!(
        !(200..300).contains(&status),
        "the RPC must REJECT — a 2xx is read as a successful send however the body reads: \
         {status} {body}"
    );
    assert_eq!(
        status, 422,
        "a refusal is a command error the person is shown, not a 5xx the client retries: {body}"
    );
    assert!(
        (400..500).contains(&status),
        "`>= 500` means 'unreachable, retry' on this seam: {status}"
    );
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "`error` is the only field the client reads a message out of: {body}"
    );
    assert!(
        body.get("accepted").is_none(),
        "a refused send must not carry the acceptance flag at all: {body}"
    );

    // ---- nothing was written ----
    let transcript = store
        .gateway_transcript(&coworker, &account)
        .await
        .expect("transcript");
    assert!(
        transcript.is_empty(),
        "a refused send must append nothing — not the user's message, not a bubble: {transcript:?}"
    );

    // ---- and it stays refused ----
    //
    // The retry the client would make. If the refusal ran after the acceptance ledger this would
    // come back `200 {"accepted": true}` off the nonce, and the fixture would be a liar.
    let (again, again_body) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": agent, "prompt": "fail", "clientNonce": nonce }),
    )
    .await;
    assert_eq!(
        again, 422,
        "the same nonce must be refused again, not deduped into an acceptance: {again_body}"
    );

    // ---- an ordinary prompt is untouched ----
    //
    // Without this the whole fixture could be a `sendPrompt` that refuses everything, and every
    // assertion above would still pass.
    let (ordinary, ordinary_body) = api(
        &client,
        &base,
        "sendPrompt",
        json!({
            "agentId": agent,
            "prompt": "why did that fail",
            "clientNonce": format!("ordinary-{stamp}"),
        }),
    )
    .await;
    assert_eq!(
        ordinary, 200,
        "only the exact fixture name refuses; a prompt that merely contains it must send: \
         {ordinary_body}"
    );
}
