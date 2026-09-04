//! A routine belongs to the person who set it, and a duplicate to the person who made it.
//!
//! Both used to belong to the DEPLOYMENT account: five seam-A handlers resolved
//! `account(state, &state.email)` and never looked at the caller. That pooled every member's
//! routines into one identity — invisible to their owners, and listable, editable and deletable
//! by anyone signed in. Needs Postgres; skips loudly without OG_DATABASE_URL.

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
        Arc::new(TokenMinter::new(b"caller-identity-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        // Says back its system prompt: what the model was TOLD is what it answers.
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
}

impl Harness {
    async fn api_as(&self, method: &str, body: Value, access: &str) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/api/{method}", self.base))
            .header("authorization", "Bearer test-bearer")
            .header("x-opengrok-account", access)
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
}

/// A routine is its author's. Before this, `createAgentAutomation` wrote it to the deployment
/// account, so the person who made it could not see it and everybody else could.
#[tokio::test]
async fn a_routine_belongs_to_whoever_set_it_and_to_nobody_else() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let ada_email = format!("ada-auto-{tag}@og.local");
    let h = harness(&database_url, &ada_email).await;
    let ada = h.access_token(&h.account.clone(), &ada_email);
    let agent = h.hire("Ada").await;

    let bo_email = format!("bo-auto-{tag}@og.local");
    let bo_id = seed_account(&h.store, &bo_email).await;
    let bo = h.access_token(&bo_id, &bo_email);

    let (status, made) = h
        .api_as(
            "createAgentAutomation",
            json!({ "agentId": agent, "cron": "0 9 * * 1", "instruction": "weekly report" }),
            &ada,
        )
        .await;
    assert_eq!(status, 200, "{made}");
    let routine = made
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row["id"].as_str())
        .expect("the routine's id")
        .to_string();

    // Hers, and she can see it. This is the half that was simply broken: her own routine was
    // written to somebody else's account and vanished from her list.
    let (_, hers) = h
        .api_as("getAgentAutomations", json!({ "id": agent }), &ada)
        .await;
    assert!(
        hers.as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == routine.as_str())),
        "the author cannot see her own routine: {hers}"
    );

    // Not Bo's, in any of the four ways he could reach it.
    let (_, his) = h.api_as("listAllAutomations", json!({}), &bo).await;
    assert_eq!(
        his,
        json!([]),
        "another account's routines were listed to a stranger: {his}"
    );

    let (_, renamed) = h
        .api_as(
            "updateAgentAutomation",
            json!({
                "id": agent, "automationId": routine,
                "cron": "0 9 * * 1", "instruction": "changed by a stranger"
            }),
            &bo,
        )
        .await;
    assert!(
        !renamed.to_string().contains("changed by a stranger"),
        "a stranger edited a routine that is not theirs: {renamed}"
    );

    let (_, _) = h
        .api_as(
            "deleteAgentAutomation",
            json!({ "id": agent, "automationId": routine }),
            &bo,
        )
        .await;
    let (_, still) = h
        .api_as("getAgentAutomations", json!({ "id": agent }), &ada)
        .await;
    assert!(
        still
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == routine.as_str())),
        "a stranger deleted a routine that is not theirs: {still}"
    );

    // And its author still governs it.
    let (_, mine) = h
        .api_as(
            "setAgentAutomationEnabled",
            json!({ "id": routine, "enabled": false }),
            &ada,
        )
        .await;
    assert!(
        mine.as_array().is_some_and(|rows| rows
            .iter()
            .any(|r| r["id"] == routine.as_str() && r["enabled"] == json!(false))),
        "the author cannot disable her own routine: {mine}"
    );
}

/// A duplicate belongs to the person who asked for it. It used to be written to the DEPLOYMENT
/// account, so a member's copy landed on somebody else's roster and never appeared on their own.
///
/// The caller here is Bo, deliberately. The harness's own email IS the deployment email, so a
/// test where the owner duplicates their own coworker passes either way and demonstrates
/// nothing — the two identities coincide. Only a caller who is not the deployment can show it.
#[tokio::test]
async fn a_duplicate_belongs_to_the_caller_not_the_deployment() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let host_email = format!("host-dup-{tag}@og.local");
    let h = harness(&database_url, &host_email).await;

    let bo_email = format!("bo-dup-{tag}@og.local");
    let bo_id = seed_account(&h.store, &bo_email).await;
    let bo = h.access_token(&bo_id, &bo_email);

    let (status, made) = h
        .api_as(
            "createAgent",
            json!({ "name": "Bo's own", "clientNonce": format!("n-{tag}") }),
            &bo,
        )
        .await;
    assert_eq!(status, 200, "{made}");
    let agent = made["agent"]["id"]
        .as_str()
        .or_else(|| made["id"].as_str())
        .expect("the new coworker's id")
        .to_string();

    let (status, copy) = h
        .api_as("duplicateAgent", json!({ "id": agent }), &bo)
        .await;
    assert_eq!(status, 200, "{copy}");
    let copy_id = copy["agent"]["id"]
        .as_str()
        .or_else(|| copy["id"].as_str())
        .expect("the copy's id")
        .to_string();
    assert_ne!(copy_id, agent, "a duplicate is a new coworker");

    let (_, his) = h.api_as("listAgents", json!({}), &bo).await;
    assert!(
        his.as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == copy_id.as_str())),
        "the copy is not on the roster of the person who made it: {his}"
    );

    // And it did not land on the deployment's roster instead.
    let host = h.access_token(&h.account.clone(), &host_email);
    let (_, theirs) = h.api_as("listAgents", json!({}), &host).await;
    assert!(
        !theirs
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == copy_id.as_str())),
        "somebody else's copy appeared on the deployment account's roster: {theirs}"
    );
}
