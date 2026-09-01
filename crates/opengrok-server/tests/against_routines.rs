//! The desktop's Routines pane, driven with its own bodies over a real socket.
//!
//! The pane sends `{id, spec}` / `{id, automationId, …}` (`routines/controller.ts:43-54`) and
//! parses every reply with `parseAutomation` (`:92-106`), which rejects the whole array unless
//! each record carries string `name`, `prompt`, `triggerDescription`, object `trigger`, boolean
//! `isEnabled` and an array `runs`. Before this slice the server answered the pane's create with
//! 400 "agentId is required", listed rows without `name`/`trigger` (the pane showed empty), and
//! routed update to create (every edit made a second schedule). Each of those is pinned here:
//! create → list in the pane's shape → update changes the prompt without adding a row → enable
//! off/on → run now → the run appears as `manual` and the coworker's chat gets the result →
//! delete. A slack trigger is refused with the 400 the pane shows; the pre-pane body still works.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
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

/// The gateway's account — the one whose coworkers the roster and the routines belong to.
async fn seed_gateway_account(store: &PgStore, email: &str) {
    if store
        .account_by_email(email)
        .await
        .expect("lookup")
        .is_some()
    {
        return;
    }
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Host".to_string(),
            last_name: String::new(),
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
        password_hash: Some("x".to_string()),
        first_name: "Host".to_string(),
        last_name: String::new(),
        org_id: None,
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
}

async fn app(database_url: &str, email: &str) -> (axum::Router, GatewayState) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    seed_gateway_account(&store, email).await;
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"routines-secret")),
        email.to_string(),
    );
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
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    (opengrok_server::router(agui, gateway.clone()), gateway)
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

/// `POST /api/{method}` with the gateway bearer — how the desktop's coordinator calls.
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
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

/// What `parseAutomation` demands of every record, asserted the same way.
fn assert_pane_shape(record: &Value) {
    for key in ["id", "name", "prompt", "triggerDescription"] {
        assert!(record[key].is_string(), "{key} must be a string: {record}");
    }
    assert!(record["trigger"].is_object(), "{record}");
    assert!(record["isEnabled"].is_boolean(), "{record}");
    let runs = record["runs"].as_array().expect("runs array");
    for run in runs {
        assert!(
            run["id"].is_string() && run["startedAt"].is_number(),
            "{run}"
        );
        assert!(
            matches!(run["status"].as_str(), Some("running" | "ok" | "error")),
            "{run}"
        );
    }
}

#[tokio::test]
async fn the_routines_pane_creates_edits_runs_and_deletes_one_schedule() {
    let database_url = database_or_skip!();
    let email = format!("routines-{}@og.local", uuid::Uuid::now_v7().simple());
    let (router, _gateway) = app(&database_url, &email).await;
    let base = spawn(router).await;
    let client = reqwest::Client::new();

    let (status, created) = api(
        &client,
        &base,
        "createAgent",
        json!({ "name": "Reporter", "description": "writes reports" }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let agent = created["agent"]["id"]
        .as_str()
        .expect("agent id")
        .to_string();

    // A trigger the server cannot serve is refused with the sentence the pane shows.
    let (status, refused) = api(
        &client,
        &base,
        "createAgentAutomation",
        json!({ "id": agent, "spec": {
            "name": "On slack", "prompt": "reply", "isEnabled": true,
            "trigger": { "type": "slack", "channel": "#ops" } } }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(
        refused["error"],
        "only schedules are supported on this server"
    );

    // Create with the pane's body: the reply is the pane's array, in the pane's shape.
    let (status, list) = api(
        &client,
        &base,
        "createAgentAutomation",
        json!({ "id": agent, "spec": {
            "name": "Monday report", "prompt": "write the weekly report", "isEnabled": true,
            "trigger": { "type": "cron", "schedule": "0 9 * * 1" } } }),
    )
    .await;
    assert_eq!(status, 200, "{list}");
    let records = list.as_array().expect("array");
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_pane_shape(record);
    assert_eq!(record["name"], "Monday report");
    assert_eq!(
        record["trigger"],
        json!({ "type": "cron", "schedule": "0 9 * * 1" })
    );
    assert_eq!(
        record["schedule"], "0 9 * * 1",
        "5-field form on the way out"
    );
    assert_eq!(record["triggerDescription"], "Every Monday at 09:00");
    assert_eq!(record["isEnabled"], true);
    assert!(record["nextRunAt"].is_number());
    assert!(record["createdAt"].is_number());
    assert_eq!(record["lastRunAt"], Value::Null);
    assert_eq!(record["runs"], json!([]));
    // The pre-pane keys ride along for the smokes.
    assert_eq!(record["cron"], "0 0 9 * * 1");
    assert_eq!(record["instruction"], "write the weekly report");
    let automation = record["id"].as_str().expect("id").to_string();

    // Update edits the row: same id, new prompt and schedule, still one record.
    let (status, list) = api(
        &client,
        &base,
        "updateAgentAutomation",
        json!({ "id": agent, "automationId": automation, "spec": {
            "name": "Friday report", "prompt": "write the week's report", "isEnabled": true,
            "trigger": { "type": "cron", "schedule": "30 17 * * 5" } } }),
    )
    .await;
    assert_eq!(status, 200, "{list}");
    let records = list.as_array().expect("array");
    assert_eq!(records.len(), 1, "an edit must not add a row: {list}");
    assert_eq!(records[0]["id"], automation);
    assert_eq!(records[0]["name"], "Friday report");
    assert_eq!(records[0]["prompt"], "write the week's report");
    assert_eq!(records[0]["triggerDescription"], "Every Friday at 17:30");

    // The editor's toggle on update, and the dedicated verb, both flip isEnabled.
    let (_, list) = api(
        &client,
        &base,
        "setAgentAutomationEnabled",
        json!({ "id": agent, "automationId": automation, "isEnabled": false }),
    )
    .await;
    assert_eq!(list[0]["isEnabled"], false, "{list}");
    assert_eq!(list[0]["nextRunAt"], Value::Null, "paused: nothing is due");
    let (_, list) = api(
        &client,
        &base,
        "setAgentAutomationEnabled",
        json!({ "id": agent, "automationId": automation, "isEnabled": true }),
    )
    .await;
    assert_eq!(list[0]["isEnabled"], true, "{list}");

    // Run now: a manual run appears in the history, and when it finishes the coworker's chat
    // carries the result as a message from the coworker.
    let (status, _) = api(
        &client,
        &base,
        "runAgentAutomationNow",
        json!({ "id": agent, "automationId": automation }),
    )
    .await;
    assert_eq!(status, 200);
    let mut finished = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (_, list) = api(
            &client,
            &base,
            "getAgentAutomations",
            json!({ "id": agent }),
        )
        .await;
        let runs = list[0]["runs"].as_array().cloned().unwrap_or_default();
        if let Some(run) = runs.iter().find(|run| run["status"] == "ok") {
            finished = Some((run.clone(), list[0].clone()));
            break;
        }
    }
    let (run, record) = finished.expect("the manual run finished within 10s");
    assert_pane_shape(&record);
    assert_eq!(run["trigger"], "manual");
    assert!(run["finishedAt"].is_number());
    assert!(record["lastRunAt"].is_number());
    let (_, tail) = api(
        &client,
        &base,
        "getAgentTranscriptTail",
        json!({ "id": agent, "limit": 50 }),
    )
    .await;
    let entries = tail["entries"].as_array().expect("entries");
    let posted = entries.iter().any(|entry| {
        entry["message"]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("Routine Friday report ran"))
    });
    assert!(
        posted,
        "the routine's result must land in the coworker's chat: {tail}"
    );

    // The pre-pane body still creates, named after itself when it has no name.
    let (status, list) = api(
        &client,
        &base,
        "createAgentAutomation",
        json!({ "agentId": agent, "cron": "*/15 * * * *", "instruction": "check the queue" }),
    )
    .await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list.as_array().expect("array").len(), 2);
    let old_style = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["instruction"] == "check the queue")
        .expect("the old body's row");
    assert_pane_shape(old_style);
    assert_eq!(old_style["name"], "Routine");
    assert_eq!(old_style["triggerDescription"], "Every 15 minutes");

    // Delete: gone from the pane and from the all-agents listing.
    let (status, list) = api(
        &client,
        &base,
        "deleteAgentAutomation",
        json!({ "id": agent, "automationId": automation }),
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|r| r["id"] != automation),
        "{list}"
    );
    let (_, all) = api(&client, &base, "listAllAutomations", json!({})).await;
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .all(|r| r["id"] != automation)
    );
}
