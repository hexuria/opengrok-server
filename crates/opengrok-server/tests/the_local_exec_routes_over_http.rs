//! The local-exec routes NativeChat calls, over HTTP (#255). Their behaviour is tested beside
//! the handlers (`against_a_chained_command.rs`, `against_local_exec_store.rs`), which is why the
//! wire corpus had none of them: it is recorded from what crosses the router. This walks them as
//! the app does, so their bodies are in `tests/fixtures/wire/`.
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

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
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

async fn seed_account(store: &PgStore, email: &str, org: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: org.to_string(),
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
        org_id: (!org.is_empty()).then(|| org.to_string()),
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

#[tokio::test]
async fn the_desktop_enrols_sets_its_policy_and_reads_it_back() {
    let database_url = database_or_skip!();
    let email = format!("local-exec-{}@og.local", uuid::Uuid::now_v7().simple());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, &email, "").await;
    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"the-desktop-over-http")),
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
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui.clone(), host);
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
    let token = agui
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-local-exec",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    let client = reqwest::Client::new();
    let send = |method: reqwest::Method, path: &str, body: Option<Value>| {
        let mut request = client
            .request(method, format!("{base}{path}"))
            .header("authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send()
    };

    let enrolled = send(
        reqwest::Method::POST,
        "/local-exec/daemon",
        Some(json!({ "label": "Ada's Mac" })),
    )
    .await
    .expect("enrol");
    assert!(enrolled.status().is_success(), "{}", enrolled.status());
    let enrolled: Value = enrolled.json().await.expect("enrolled json");
    let machine = enrolled["machineId"]
        .as_str()
        .expect("a machine id")
        .to_string();

    let listed = send(reqwest::Method::GET, "/local-exec/daemon", None)
        .await
        .expect("list");
    assert!(listed.status().is_success(), "{}", listed.status());

    let mode = send(
        reqwest::Method::PUT,
        "/local-exec/policy",
        Some(json!({ "machineId": machine, "mode": "ask" })),
    )
    .await
    .expect("mode");
    assert!(mode.status().is_success(), "{}", mode.status());

    let rule = json!({ "machineId": machine, "kind": "allow", "pattern": "git status" });
    let allowed = send(
        reqwest::Method::POST,
        "/local-exec/policy/rule",
        Some(rule.clone()),
    )
    .await
    .expect("rule");
    assert!(allowed.status().is_success(), "{}", allowed.status());

    let chained = send(
        reqwest::Method::POST,
        "/local-exec/policy/rule",
        Some(json!({ "machineId": machine, "kind": "allow", "pattern": "git status && rm -rf ~" })),
    )
    .await
    .expect("chained rule");
    assert_eq!(
        chained.status().as_u16(),
        422,
        "an allow must be one plain command"
    );

    let policy = send(
        reqwest::Method::GET,
        &format!("/local-exec/policy?machine={machine}"),
        None,
    )
    .await
    .expect("policy");
    assert!(policy.status().is_success(), "{}", policy.status());
    let policy: Value = policy.json().await.expect("policy json");
    assert_eq!(policy["machineId"], machine.as_str(), "{policy}");

    let removed = send(
        reqwest::Method::DELETE,
        "/local-exec/policy/rule",
        Some(rule),
    )
    .await
    .expect("remove rule");
    assert!(removed.status().is_success(), "{}", removed.status());

    let audit = send(reqwest::Method::GET, "/local-exec/audit", None)
        .await
        .expect("audit");
    assert!(audit.status().is_success(), "{}", audit.status());
}
