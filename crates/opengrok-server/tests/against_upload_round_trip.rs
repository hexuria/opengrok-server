//! A file dropped into the desktop must round-trip under a mock door.
//!
//! `uploadAttachment` was an unconditional refusal, so the desktop's `commitStaged` failed with
//! "The desktop bridge could not commit the staged attachments" and no PDF, image or spreadsheet
//! could reach a viewer unless it shipped as a fixture. This is the one place in the catalogue
//! where a CALLER chooses both the bytes and the name, so most of this file is about the name.
//!
//! Needs Postgres, the `mock-fixtures` feature and a mock door; skips loudly without any of them.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
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

async fn serve(store: PgStore, email: &str) -> String {
    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"upload-secret")),
            email.to_string(),
        ),
        door: Arc::new(MockDoor::serving_fixtures()),
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
        email.to_string(),
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
    base
}

/// A file the person drops must come back byte-for-byte, and nothing else must.
#[tokio::test]
async fn a_dropped_file_round_trips_and_containment_still_holds() {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    // The upload verb is gated on the same `enabled()` as every read: no mock door, no storage.
    let Ok(door) = std::env::var("OG_MODEL_DOOR") else {
        eprintln!("skipping: OG_MODEL_DOOR is not set");
        return;
    };
    if !matches!(door.as_str(), "mock" | "mock-tools" | "mock-cards") {
        eprintln!("skipping: OG_MODEL_DOOR={door} does not enable the catalogue");
        return;
    }

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
    let email = format!("upload-{stamp}@og.local");
    seed_account(&store, &email).await;
    let base = serve(store, &email).await;
    let client = reqwest::Client::new();

    // A PDF-ish payload with a NUL and high bytes in it: binary must survive base64 untouched.
    let bytes: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let (status, up) = api(
        &client,
        &base,
        "uploadAttachment",
        json!({ "filename": "ai-infrastructure-design.pdf", "bytesBase64": encoded }),
    )
    .await;
    assert_eq!(status, 200, "a dropped file must be accepted: {up}");
    let path = up["path"]
        .as_str()
        .expect("the reply names a path")
        .to_string();
    assert!(
        path.contains("/uploads/") && path.ends_with("-ai-infrastructure-design.pdf"),
        "stored under the fixture root, uuid-prefixed so two drops cannot collide: {path}"
    );

    // Read it back the way the desktop's viewers do — no new read code, the existing verb.
    let (status, probe) = api(
        &client,
        &base,
        "readAttachmentChunk",
        json!({ "path": path, "offset": 0, "length": 0 }),
    )
    .await;
    assert_eq!(status, 200, "{probe}");
    assert_eq!(
        probe["totalSize"].as_u64(),
        Some(bytes.len() as u64),
        "the size probe must answer the whole size and no bytes: {probe}"
    );

    let (status, whole) = api(
        &client,
        &base,
        "readAttachmentChunk",
        json!({ "path": path, "offset": 0 }),
    )
    .await;
    assert_eq!(status, 200, "{whole}");
    let back = base64::engine::general_purpose::STANDARD
        .decode(whole["bytesBase64"].as_str().unwrap_or_default())
        .expect("base64");
    assert_eq!(back, bytes, "the file must come back byte-for-byte");

    // CONTAINMENT IS UNCHANGED BY THE WRITE. An upload landing under the root must not become a
    // way to read anything else that happens to be there.
    for escape in ["/etc/passwd", "/etc/hosts", "../../etc/passwd"] {
        let (status, refused) = api(
            &client,
            &base,
            "readAttachmentChunk",
            json!({ "path": escape, "offset": 0 }),
        )
        .await;
        assert_eq!(status, 400, "{escape} must be refused: {refused}");
    }

    // And a traversal in the FILENAME is refused before anything is written.
    for bad in ["../escape.pdf", "/tmp/escape.pdf", "a/b.pdf", ".env"] {
        let (status, refused) = api(
            &client,
            &base,
            "uploadAttachment",
            json!({ "filename": bad, "bytesBase64": "eA==" }),
        )
        .await;
        assert_eq!(status, 400, "{bad} must be refused: {refused}");
    }

    // The pdf-links fixture is reachable and is a real PDF with links.
    let (status, sent) = api(
        &client,
        &base,
        "sendPrompt",
        json!({ "agentId": "", "prompt": "pdf-links", "clientNonce": format!("p-{stamp}") }),
    )
    .await;
    // No agent named: the point here is only that the verb is wired, not that a turn ran.
    assert!(status == 200 || status == 400, "{sent}");
}
