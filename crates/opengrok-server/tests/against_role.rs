//! The standing role, from the field to the model's ears.
//!
//! A role that is stored but never reaches the model is decoration, so this drives the real HTTP
//! surface and then reads what the model was actually told — the mock door answers with its own
//! system prompt, which is the only way to prove the composition arrived rather than that the
//! code meant to send it. Needs Postgres; skips loudly without OG_DATABASE_URL.

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
use opengrok_server::gateway::GatewayState;
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
    agui: AgUiState,
    store: PgStore,
    account: AccountId,
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
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"role-test-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        // Says back its system prompt: what the model was TOLD is what it answers.
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
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    let app = opengrok_server::router(agui.clone(), gateway);
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
        agui,
        store,
        account,
    }
}

impl Harness {
    async fn api(&self, method: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/api/{method}", self.base))
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

    async fn hire(&self, name: &str) -> String {
        let (status, created) = self
            .api(
                "createAgent",
                json!({ "name": name, "clientNonce": format!("hire-{name}-{}", now_ms()) }),
            )
            .await;
        assert_eq!(status, 200, "{created}");
        created["agent"]["id"].as_str().expect("id").to_string()
    }
}

impl Harness {
    /// The signed-in person's token — what the account API takes, unlike /api/* which takes the
    /// gateway bearer.
    fn access_token(&self, account: &AccountId, email: &str) -> String {
        self.agui
            .auth
            .minter
            .mint_access(
                account.as_str(),
                "sess-test",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn patch(&self, access: &str, id: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .patch(format!("{}/coworkers/{}", self.base, id))
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

    /// The last thing the coworker said — which, with this door, is its system prompt.
    async fn what_the_model_was_told(&self, agent: &str) -> String {
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let (_, tail) = self
                .api(
                    "getAgentTranscriptTail",
                    json!({ "id": agent, "limit": 50 }),
                )
                .await;
            if let Some(said) = tail["entries"].as_array().and_then(|entries| {
                entries.iter().rev().find_map(|e| {
                    (e["kind"] == "send-message" && e["streaming"] != json!(true))
                        .then(|| e["message"]["content"].as_str().unwrap_or("").to_string())
                        .filter(|said| !said.is_empty())
                })
            }) {
                return said;
            }
        }
        panic!("the coworker never answered in 10s");
    }

    async fn row(&self, id: &str) -> Value {
        let (_, rows) = self.api("listAgents", json!({})).await;
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
            .unwrap_or(Value::Null)
    }
}

#[tokio::test]
async fn a_standing_role_reaches_the_model_on_every_run_and_the_roster_carries_it() {
    let database_url = database_or_skip!();
    let email = format!("role-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access_token(&h.account.clone(), &email);
    let agent = h.hire("Ada").await;

    // No role yet: the model is still told who it is, and the roster says the role is null
    // rather than omitting the field.
    let (status, sent) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "hello", "clientNonce": format!("n0-{}", now_ms()) }),
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    let told = h.what_the_model_was_told(&agent).await;
    assert!(told.starts_with("You are Ada."), "{told}");
    assert!(
        told.contains("your OWN computer") || told.contains("do NOT currently have a computer"),
        "the machine discipline survives the composition: {told}"
    );
    let row = h.row(&agent).await;
    assert!(
        row.get("role").is_some() && row["role"].is_null(),
        "present and null, not absent: {row}"
    );

    // A role is set through PATCH, comes back on the reply and on the roster.
    let (status, patched) = h
        .patch(
            &access,
            &agent,
            json!({ "role": "  Keep the changelog honest.  " }),
        )
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(
        patched["role"], "Keep the changelog honest.",
        "trimmed: {patched}"
    );
    assert_eq!(h.row(&agent).await["role"], "Keep the changelog honest.");

    // And it reaches the model, once, in the composed order, with the computer paragraph after.
    let (status, _) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "again", "clientNonce": format!("n1-{}", now_ms()) }),
        )
        .await;
    assert_eq!(status, 200);
    let told = h.what_the_model_was_told(&agent).await;
    assert!(
        told.starts_with("You are Ada.\n\nKeep the changelog honest."),
        "{told}"
    );
    assert!(
        told.contains("That role stands in every conversation"),
        "the standing sentence is what stops it reading as this turn's instruction: {told}"
    );
    let identity_at = told.find("You are Ada.").expect("identity");
    let role_at = told.find("Keep the changelog honest.").expect("role");
    let computer_at = told
        .find("computer")
        .expect("the machine paragraph is still there");
    assert!(
        identity_at < role_at && role_at < computer_at,
        "order: {told}"
    );
    assert_eq!(
        told.matches("You are Ada.").count(),
        1,
        "one identity line: {told}"
    );

    // Refusals are a 400 with a sentence, and they say the numbers.
    let (status, refused) = h
        .patch(&access, &agent, json!({ "role": "x".repeat(1001) }))
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(
        refused["error"],
        "role: 1001 characters is longer than the 1000 allowed"
    );
    let (status, refused) = h.patch(&access, &agent, json!({ "role": 7 })).await;
    assert_eq!(status, 400, "{refused}");
    let (status, refused) = h.patch(&access, &agent, json!({})).await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or("")
            .contains("name a model, a role, or both"),
        "{refused}"
    );
    // The refused writes changed nothing.
    assert_eq!(h.row(&agent).await["role"], "Keep the changelog honest.");

    // Null clears it; so does a blank string. The model then hears no role at all.
    let (status, cleared) = h.patch(&access, &agent, json!({ "role": null })).await;
    assert_eq!(status, 200, "{cleared}");
    assert!(cleared["role"].is_null(), "{cleared}");
    let (status, _) = h
        .api(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "third", "clientNonce": format!("n2-{}", now_ms()) }),
        )
        .await;
    assert_eq!(status, 200);
    let told = h.what_the_model_was_told(&agent).await;
    assert!(!told.contains("That role stands"), "{told}");
    assert!(told.starts_with("You are Ada."), "{told}");

    // A model repin still works through the same route, and leaves the role alone.
    let (status, _) = h.patch(&access, &agent, json!({ "role": "Ships." })).await;
    assert_eq!(status, 200);
    let (status, repinned) = h
        .patch(&access, &agent, json!({ "model": "oag/cheap" }))
        .await;
    assert_eq!(status, 200, "{repinned}");
    assert_eq!(repinned["model"], "oag/cheap");
    assert_eq!(repinned["role"], "Ships.", "a repin is not a role edit");

    // Another account cannot read or write it.
    let stranger = format!("stranger-{}@og.local", uuid::Uuid::now_v7().simple());
    let stranger_id = seed_account(&h.store, &stranger).await;
    let stranger_access = h.access_token(&stranger_id, &stranger);
    let (status, _) = h
        .patch(&stranger_access, &agent, json!({ "role": "mine" }))
        .await;
    assert_eq!(status, 404);
}
