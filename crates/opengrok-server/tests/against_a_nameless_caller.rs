//! A turn that names a coworker must say who is asking.
//!
//! `forwardedProps.coworkerId` is sent by the client, so anyone can send it — including a caller
//! with no credential at all. That pair could only ever fail, and before this it failed late and
//! in the wrong words: every gate downstream is keyed on having BOTH a coworker and a principal,
//! so the policy check, the coworker's model and role, its tools and its persona were all skipped,
//! and the turn reached the spend guard carrying a scope with no payer.
//!
//! The guard held it, correctly, with "This turn does not say whose spend it is … This is a server
//! bug, not a limit you have hit." It was our bug, and it reached a person as red text in their
//! transcript that read like a quota they had hit. Observed on 17 Sep 2026 against a live server,
//! twice, with `auth_len=0` in the request log both times.
//!
//! Since 25 Sep 2026 NO turn is anonymous. An unsigned turn ran on the deployment's gateway key
//! with no payer and no meter, and an expired or foreign bearer silently became one — so anyone
//! who could reach the host spent its model credit, one run row per request. A caller is named
//! or refused; a named caller's turn with no coworker (which has no key of its own to meter) is
//! bounded by a per-account budget instead.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::budget::AGUI_UNSCOPED;
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

/// The AG-UI body, with `coworkerId` optional so one helper serves both halves of the claim.
fn turn(coworker: Option<&str>) -> Value {
    let mut props = json!({});
    if let Some(coworker) = coworker {
        props["coworkerId"] = json!(coworker);
    }
    json!({
        "threadId": format!("thr-{}", now_ms()),
        "runId": uuid::Uuid::now_v7().to_string(),
        "messages": [{ "id": "m1", "role": "user", "content": "hello" }],
        "forwardedProps": props,
    })
}

#[tokio::test]
async fn a_turn_that_names_a_coworker_needs_a_caller_we_can_name_back() {
    let Ok(database_url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
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
    let store = PgStore::new(pool);
    let email = format!("nameless-{}@og.local", now_ms());
    let account = seed_account(&store, &email).await;
    let access = TokenMinter::new(b"nameless-secret")
        .mint_access(
            account.as_str(),
            "s1",
            &email,
            "ultra",
            now_ms() / 1_000,
            3_600,
        )
        .expect("mint access");
    let agui = AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"nameless-secret")),
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
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
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

    // A coworker named by nobody: refused at the door, not carried to the spend guard.
    let res = client
        .post(format!("{base}/ag-ui"))
        .header("content-type", "application/json")
        .body(turn(Some("cw_someone_elses")).to_string())
        .send()
        .await
        .expect("post");
    assert_eq!(
        res.status().as_u16(),
        401,
        "a named coworker needs a caller"
    );
    let said = res.text().await.expect("body");
    assert!(
        said.contains("names a coworker"),
        "the refusal must say what to do about it, got: {said}"
    );
    // AND IT MUST NOT READ AS A LIMIT. The whole point of refusing here is that the person no
    // longer meets the spend guard's sentence, which describes a quota they never hit.
    assert!(
        !said.to_ascii_lowercase().contains("limit"),
        "a missing credential must not be dressed as a spend limit, got: {said}"
    );

    // An unsigned turn naming nobody is refused too: it had no payer, so it ran on the
    // deployment's key with no meter and no limit.
    let res = client
        .post(format!("{base}/ag-ui"))
        .header("content-type", "application/json")
        .body(turn(None).to_string())
        .send()
        .await
        .expect("post");
    assert_eq!(res.status().as_u16(), 401, "an unsigned turn is refused");
    let said = res.text().await.expect("body");
    assert!(said.contains("sign in"), "say what to do, got: {said}");
    assert!(
        !said.to_ascii_lowercase().contains("limit"),
        "a missing credential is not a spend limit, got: {said}"
    );

    // A bearer we did not issue, or one that expired, is refused — never downgraded to
    // anonymous. That downgrade is how a Bot holding a stale key "worked" on 1 Sep while
    // owning nothing (ROADMAP 10.3): the empty success, not an error anybody could see.
    let expired = TokenMinter::new(b"nameless-secret")
        .mint_access(
            account.as_str(),
            "s1",
            &email,
            "ultra",
            now_ms() / 1_000 - 7_200,
            3_600,
        )
        .expect("mint expired");
    for bearer in ["not-a-jwt", expired.as_str()] {
        let res = client
            .post(format!("{base}/ag-ui"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(turn(None).to_string())
            .send()
            .await
            .expect("post");
        assert_eq!(res.status().as_u16(), 401, "a bad bearer is not anonymous");
        let said = res.text().await.expect("body");
        assert!(said.contains("sign in again"), "got: {said}");
    }

    // A signed-in turn with no coworker has a payer but no key of its own to meter it, so it
    // is bounded per account instead: served up to the budget, then a readable 429.
    for n in 0..AGUI_UNSCOPED.per_window {
        let res = client
            .post(format!("{base}/ag-ui"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {access}"))
            .body(turn(None).to_string())
            .send()
            .await
            .expect("post");
        assert_eq!(res.status().as_u16(), 200, "turn {n} is inside the budget");
        let _ = res.bytes().await;
    }
    let res = client
        .post(format!("{base}/ag-ui"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {access}"))
        .body(turn(None).to_string())
        .send()
        .await
        .expect("post");
    assert_eq!(res.status().as_u16(), 429, "past the budget");
    let retry_after: u64 = res
        .headers()
        .get("retry-after")
        .expect("Retry-After")
        .to_str()
        .expect("ascii")
        .parse()
        .expect("seconds");
    assert!(retry_after >= 1, "a spent budget never says retry now");
    let said = res.text().await.expect("body");
    assert!(
        said.contains("coworker"),
        "the refusal names the way round it: {said}"
    );
}
