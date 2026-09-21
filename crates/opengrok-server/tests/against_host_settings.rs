//! The host's settings on the AG-UI door.
//!
//! NativeChat's settings page read `getHostSettings` and `isEgressTunnelAvailable` and wrote
//! `setHostSettings` through the desktop client's JSON door (`POST /api/{method}`). That door is
//! gone, and the same record answers at `GET /ag-ui/host-settings`; a partial record is merged by
//! `PUT` — under the account token every other AG-UI route takes, not the shared host bearer.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
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

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"host-settings-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
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
        // What `opengrok_server::router` fills in from the gateway's record when it is `None`:
        // the one record both doors read, so the two answers cannot drift apart.
        host_settings: None,
    };
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui, gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        store,
        minter,
    }
}

impl Harness {
    async fn person(&self, email: &str) -> String {
        let account = seed_account(&self.store, email).await;
        self.minter
            .mint_access(
                account.as_str(),
                "sess-host-settings",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn get(&self, access: Option<&str>, query: &str) -> (u16, Value) {
        let mut req = self
            .client
            .get(format!("{}/ag-ui/host-settings{query}", self.base));
        if let Some(access) = access {
            req = req.header("Authorization", format!("Bearer {access}"));
        }
        let res = req.send().await.expect("get host settings");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn put(&self, access: &str, body: &Value) -> (u16, Value) {
        let res = self
            .client
            .put(format!("{}/ag-ui/host-settings", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .json(body)
            .send()
            .await
            .expect("put host settings");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

fn database_url() -> Option<String> {
    match std::env::var("OG_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(opengrok_store::gate_database_or_panic(url)),
        _ => {
            eprintln!("skipping: OG_DATABASE_URL is not set");
            None
        }
    }
}

#[tokio::test]
async fn the_record_reads_back_whole_and_a_patch_keeps_the_rest() {
    let Some(url) = database_url() else { return };
    // Unique per run: this file is run against a database that keeps its rows, and a second run
    // on a fixed address fails at `append_account` with `Conflict` rather than at an assertion.
    let email = format!("host-settings-{}@test.local", uuid::Uuid::now_v7().simple());
    let h = harness(&url, &email).await;
    let access = h.person(&email).await;

    let (status, record) = h.get(Some(&access), "").await;
    assert_eq!(status, 200);
    assert_eq!(
        record["egressTunnelEnabled"],
        json!(false),
        "the shipped host starts with the tunnel off"
    );
    assert_eq!(
        record["egressTunnelAvailable"],
        json!(false),
        "off, and no coworker named: not available"
    );
    assert_eq!(
        record["hasSeenOnboarding"],
        json!(true),
        "every default field is in the record, not only the ones the client asked about"
    );

    let (status, after) = h
        .put(&access, &json!({ "egressTunnelEnabled": true }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(after["egressTunnelEnabled"], json!(true));
    assert_eq!(
        after["hasSeenOnboarding"],
        json!(true),
        "a patch merges: the keys not given are still there"
    );
    assert_eq!(
        after["egressTunnelAvailable"],
        json!(false),
        "wanted is not available: no coworker was named and this host has no computer"
    );

    let (_, again) = h.get(Some(&access), "?coworker=cw_nobody").await;
    assert_eq!(again["egressTunnelEnabled"], json!(true), "the write stuck");
    assert_eq!(
        again["egressTunnelAvailable"],
        json!(false),
        "a coworker that is not this account's answers false, never an error"
    );
}

#[tokio::test]
async fn a_stranger_and_a_bad_patch_are_refused() {
    let Some(url) = database_url() else { return };
    let email = format!(
        "host-settings-refuse-{}@test.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&url, &email).await;
    let (status, _) = h.get(None, "").await;
    assert_eq!(status, 401, "no token: the record is not public");

    let access = h.person(&email).await;
    let (status, body) = h.put(&access, &json!(["not", "an", "object"])).await;
    assert_eq!(status, 400, "a patch is an object: {body}");
    let (status, record) = h.get(Some(&access), "").await;
    assert_eq!(status, 200);
    assert_eq!(
        record["egressTunnelEnabled"],
        json!(false),
        "the refused patch changed nothing"
    );
}
