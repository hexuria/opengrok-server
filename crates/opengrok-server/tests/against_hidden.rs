//! Hide-from-sidebar is the viewer's preference, not a fact about the coworker.
//!
//! PATCH `hiddenFromSidebar` writes a row keyed by the caller; GET /coworkers stamps the flag
//! on that caller's roster. DELETE retires the coworker. Needs Postgres; skips without
//! OG_DATABASE_URL.

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
use opengrok_store::PgStore;
use serde_json::{Value, json};

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
    account: AccountId,
    minter: Arc<TokenMinter>,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, email).await;
    let minter = Arc::new(TokenMinter::new(b"hide-test-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing_the_system_prompt()),
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
    let app = opengrok_server::router(agui.clone());
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
        account,
        minter,
    }
}

impl Harness {
    fn access(&self, email: &str) -> String {
        self.minter
            .mint_access(
                self.account.as_str(),
                "sess-hide",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn hire(&self, access: &str, name: &str) -> String {
        let res = self
            .client
            .post(format!("{}/coworkers", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("hire");
        assert_eq!(res.status().as_u16(), 201, "hire {}", name);
        let body: Value = res.json().await.expect("hire body");
        body["id"].as_str().expect("id").to_string()
    }

    async fn list(&self, access: &str) -> Value {
        let res = self
            .client
            .get(format!("{}/coworkers", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("list");
        assert_eq!(res.status().as_u16(), 200);
        res.json().await.expect("list body")
    }

    async fn patch(&self, access: &str, id: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .patch(format!("{}/coworkers/{id}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .json(&body)
            .send()
            .await
            .expect("patch");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn delete(&self, access: &str, id: &str) -> u16 {
        self.client
            .delete(format!("{}/coworkers/{id}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("delete")
            .status()
            .as_u16()
    }
}

fn row<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id))
        .unwrap_or(&Value::Null)
}

#[tokio::test]
async fn hidden_from_sidebar_is_the_caller_s_preference_and_delete_retires() {
    let database_url = database_or_skip!();
    let email = format!("hide-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access(&email);
    let agent = h.hire(&access, "Ada").await;

    let listed = h.list(&access).await;
    assert_eq!(row(&listed, &agent)["hiddenFromSidebar"], json!(false));

    let (status, patched) = h
        .patch(&access, &agent, json!({ "hiddenFromSidebar": true }))
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["hiddenFromSidebar"], json!(true), "{patched}");
    let listed = h.list(&access).await;
    assert_eq!(row(&listed, &agent)["hiddenFromSidebar"], json!(true));

    let (status, patched) = h
        .patch(&access, &agent, json!({ "hiddenFromSidebar": false }))
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["hiddenFromSidebar"], json!(false));
    let listed = h.list(&access).await;
    assert_eq!(row(&listed, &agent)["hiddenFromSidebar"], json!(false));

    let (status, refused) = h
        .patch(&access, &agent, json!({ "hiddenFromSidebar": "yes" }))
        .await;
    assert_eq!(status, 400, "{refused}");

    assert_eq!(h.delete(&access, &agent).await, 204);
    let listed = h.list(&access).await;
    assert!(
        listed
            .as_array()
            .is_some_and(|rows| !rows.iter().any(|row| row["id"] == agent)),
        "retired coworker must leave the roster: {listed}"
    );
    assert_eq!(h.delete(&access, &agent).await, 404);
}
