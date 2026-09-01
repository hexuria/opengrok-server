//! The auto-review card's answer, through the real router: a run suspended for auto-review and
//! its card are seeded in Postgres, then `resolveAutoReviewApproval` is driven over HTTP exactly
//! as the desktop client drives it. Proves the exactly-once answer, the same-entryId status flip,
//! the heal-to-expired 410 for a dead request, and that the two resolve verbs cannot settle each
//! other's cards. The judge itself and the ladder are unit-tested in `opengrok-tools` and
//! `opengrok-harness`; the full turn is the peer's CDP run. Needs Postgres; skips loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::{Run, RunCommand, RunStatus, RunView, SuspendReason};
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

async fn store_from(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
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
    let account = Account::replay(&events);
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
        verified: account.verified,
        enabled: account.enabled,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
    id
}

async fn seed_coworker(store: &PgStore, account: &AccountId) -> CoworkerId {
    let id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let events = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: "Reviewer".to_string(),
            model: "oag/cheap".to_string(),
            at_ms: 1,
        })
        .expect("hire");
    for event in &events {
        coworker.apply(event);
    }
    let view = CoworkerView {
        id: id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: None,
        retired: false,
        updated_at_ms: 2,
    };
    store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append coworker");
    id
}

/// A run that started and then suspended for `reason` on `call_id` — exactly what the journal
/// writes when the executor answers `awaiting` — plus its card entry in the transcript.
async fn seed_suspended_run(
    store: &PgStore,
    account: &AccountId,
    coworker: &CoworkerId,
    call_id: &str,
    reason: SuspendReason,
) -> (RunId, String) {
    let run_id = RunId::new();
    let thread_id = format!("gateway-{}", coworker.as_str());
    let mut run = Run::default();
    let mut events = run
        .decide(RunCommand::Start {
            thread_id: thread_id.clone(),
            coworker_id: Some(coworker.clone()),
            model: Some("xai/grok-4.6".to_string()),
            at_ms: 1,
        })
        .expect("start");
    for event in &events {
        run.apply(event);
    }
    let suspended = run
        .decide(RunCommand::Suspend {
            call_id: call_id.to_string(),
            tool: "shell".to_string(),
            arguments: json!({ "command": "brew install jq" }),
            reason,
            at_ms: 2,
        })
        .expect("suspend");
    for event in &suspended {
        run.apply(event);
    }
    events.extend(suspended);
    let view = RunView {
        id: run_id.clone(),
        thread_id,
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: now_ms(),
    };
    store
        .append_run(&run_id, 0, &events, &view, Some(account))
        .await
        .expect("append run");

    let entry_id = format!("e_{}", uuid::Uuid::now_v7());
    let card = match reason {
        SuspendReason::AutoReview => opengrok_server::gateway::cards::auto_review_card(
            &entry_id,
            call_id,
            "pending",
            "shell",
            &json!({ "command": "brew install jq" }),
            Some("why"),
            now_ms(),
        ),
        _ => json!({
            "kind": "send-message", "id": entry_id, "timestampMs": now_ms(),
            "message": { "type": "local-tool-permission",
                "ask": { "requestId": call_id, "status": "pending",
                         "action": "run-command", "target": "brew install jq" } },
        }),
    };
    store
        .append_gateway_entry(coworker, &card, now_ms())
        .await
        .expect("append card");
    (run_id, entry_id)
}

fn app_with(store: PgStore, host_email: &str) -> axum::Router {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(
            b"auto-review-gate-test-secret-auto-review",
        )),
        host_email.to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing().with_judge_verdict("ask")),
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
        host_email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    opengrok_server::router(agui, gateway)
}

async fn spawn(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

async fn call(base: &str, verb: &str, body: Value) -> (u16, Value) {
    let res = reqwest::Client::new()
        .post(format!("{base}/api/{verb}"))
        .header("Authorization", "Bearer test-bearer")
        .json(&body)
        .send()
        .await
        .expect("request");
    let status = res.status().as_u16();
    let body = res.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn card_status(store: &PgStore, coworker: &CoworkerId, entry_id: &str) -> Option<String> {
    store
        .gateway_transcript(coworker)
        .await
        .expect("transcript")
        .into_iter()
        .find(|entry| entry["id"] == entry_id)
        .and_then(|entry| {
            entry["message"]["approval"]["status"]
                .as_str()
                .or_else(|| entry["message"]["ask"]["status"].as_str())
                .map(str::to_string)
        })
}

#[tokio::test]
async fn an_auto_review_card_is_answered_exactly_once_and_flips_in_place() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_coworker(&store, &account).await;
    let call_id = format!("call_{}", uuid::Uuid::now_v7().simple());
    let (run_id, entry_id) = seed_suspended_run(
        &store,
        &account,
        &coworker,
        &call_id,
        SuspendReason::AutoReview,
    )
    .await;
    let base = spawn(app_with(store.clone(), &host_email)).await;

    // The exec verb must NOT settle an auto-review suspension: it finds no run and heals nothing
    // (the entry has no `ask`), answering 410.
    let (status, _) = call(
        &base,
        "resolveLocalToolPermission",
        json!({ "entryId": entry_id, "requestId": call_id, "resolution": "allow-once",
                "agentId": coworker.as_str() }),
    )
    .await;
    assert_eq!(status, 410, "the wrong verb settles nothing");
    assert_eq!(
        card_status(&store, &coworker, &entry_id).await.as_deref(),
        Some("pending")
    );

    // The right verb: approved, once.
    let (status, body) = call(
        &base,
        "resolveAutoReviewApproval",
        json!({ "entryId": entry_id, "requestId": call_id, "resolution": "approved",
                "agentId": coworker.as_str() }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(
        card_status(&store, &coworker, &entry_id).await.as_deref(),
        Some("approved")
    );
    let (run, _) = store.load_run(&run_id).await.expect("run");
    assert_eq!(run.status, RunStatus::Running, "answered ⇒ running again");
    assert!(run.answered.contains(&call_id));

    // A second press is not a second answer.
    let (status, body) = call(
        &base,
        "resolveAutoReviewApproval",
        json!({ "entryId": entry_id, "requestId": call_id, "resolution": "denied",
                "agentId": coworker.as_str() }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["alreadyAnswered"], true);
    assert_eq!(
        card_status(&store, &coworker, &entry_id).await.as_deref(),
        Some("approved")
    );
}

#[tokio::test]
async fn a_denied_auto_review_card_settles_as_denied() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_coworker(&store, &account).await;
    let call_id = format!("call_{}", uuid::Uuid::now_v7().simple());
    let (run_id, entry_id) = seed_suspended_run(
        &store,
        &account,
        &coworker,
        &call_id,
        SuspendReason::AutoReview,
    )
    .await;
    let base = spawn(app_with(store.clone(), &host_email)).await;

    // `status` is accepted where `resolution` would be — the transcription records both.
    let (status, _) = call(
        &base,
        "resolveAutoReviewApproval",
        json!({ "entryId": entry_id, "requestId": call_id, "status": "denied",
                "agentId": coworker.as_str() }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        card_status(&store, &coworker, &entry_id).await.as_deref(),
        Some("denied")
    );
    let (run, _) = store.load_run(&run_id).await.expect("run");
    // A refusal is an answer: the run is no longer waiting.
    assert_ne!(run.status, RunStatus::AwaitingApproval);
}

#[tokio::test]
async fn a_press_on_a_dead_request_heals_the_card_to_expired_with_410() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_coworker(&store, &account).await;
    // A card whose run never existed (or died): nothing is awaiting this requestId.
    let entry_id = format!("e_{}", uuid::Uuid::now_v7());
    let card = opengrok_server::gateway::cards::auto_review_card(
        &entry_id,
        "call_ghost",
        "pending",
        "shell",
        &json!({ "command": "ls" }),
        None,
        now_ms(),
    );
    store
        .append_gateway_entry(&coworker, &card, now_ms())
        .await
        .expect("append card");
    let base = spawn(app_with(store.clone(), &host_email)).await;

    let (status, body) = call(
        &base,
        "resolveAutoReviewApproval",
        json!({ "entryId": entry_id, "requestId": "call_ghost", "resolution": "approved",
                "agentId": coworker.as_str() }),
    )
    .await;
    assert_eq!(status, 410, "{body}");
    assert_eq!(
        card_status(&store, &coworker, &entry_id).await.as_deref(),
        Some("expired")
    );
    // The command the user was shown survives the flip (status-only jsonb_set).
    let entry = store
        .gateway_transcript(&coworker)
        .await
        .expect("transcript")
        .into_iter()
        .find(|entry| entry["id"] == entry_id)
        .expect("card");
    assert_eq!(entry["message"]["approval"]["command"], "ls");
}

#[tokio::test]
async fn the_exec_card_verb_ignores_an_auto_review_suspension_and_vice_versa() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_coworker(&store, &account).await;
    let call_id = format!("call_{}", uuid::Uuid::now_v7().simple());
    let (run_id, entry_id) = seed_suspended_run(
        &store,
        &account,
        &coworker,
        &call_id,
        SuspendReason::ExecConsent,
    )
    .await;
    let base = spawn(app_with(store.clone(), &host_email)).await;

    // The auto-review verb finds no auto-review suspension ⇒ 410, and the exec card (which has an
    // `ask`, not an `approval`) is untouched.
    let (status, _) = call(
        &base,
        "resolveAutoReviewApproval",
        json!({ "entryId": entry_id, "requestId": call_id, "resolution": "approved",
                "agentId": coworker.as_str() }),
    )
    .await;
    assert_eq!(status, 410);
    assert_eq!(
        card_status(&store, &coworker, &entry_id).await.as_deref(),
        Some("pending")
    );
    let (run, _) = store.load_run(&run_id).await.expect("run");
    assert_eq!(
        run.status,
        RunStatus::AwaitingApproval,
        "still waiting for the owner"
    );
}
