//! The /mcp door, driven by a HAND-WRITTEN JSON-RPC client over plain HTTP. Deliberately not
//! rmcp-to-rmcp: a client and server sharing one SDK only prove they share an interpretation —
//! the lesson that caught `Bearer Bearer` on the client side. Covers the auth boundary (only a
//! live bot key opens the door), the computerless coworker (empty toolbox, healthy handshake,
//! failed-closed call), and revocation. The full toolbox against a real computer is
//! `scripts/slice20-mcp-door-smoke.sh`, where a Docker daemon exists. Needs Postgres; skips
//! loudly without.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_core::run::RunStatus;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_store::PgStore;
use opengrok_tools::{AwaitingReason, ToolCall, ToolResult, USER_MACHINE_SHELL};
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

/// A coworker with NO computer: the door must still shake hands and list an empty toolbox.
async fn seed_computerless_coworker(store: &PgStore, account: &AccountId) -> CoworkerId {
    let id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let events = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: "Doorman".to_string(),
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
        members: Vec::new(),
        updated_at_ms: 2,
        role: None,
    };
    store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append coworker");
    id
}

fn app_with(store: PgStore, host_email: &str) -> (axum::Router, AgUiState, GatewayState) {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"mcp-door-test-secret-mcp-door-test!!")),
        host_email.to_string(),
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
        host_email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
    (
        opengrok_server::router(agui.clone(), gateway.clone()),
        agui,
        gateway,
    )
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

/// The hand-written client: one JSON-RPC request over plain POST, no SDK. Returns the HTTP status
/// AND the parsed body — auth is enforced at the transport edge now, so the status is the point of
/// the auth tests, not a JSON-RPC error. Accepts either a plain-JSON reply or an SSE-framed one,
/// because the transport may pick either and a client must not care.
async fn rpc(
    base: &str,
    bearer: Option<&str>,
    id: i64,
    method: &str,
    params: Value,
) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    if let Some(token) = bearer {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.expect("request");
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("body");
    // A non-2xx from the guard is plain text, not JSON — keep it as a string so a test can assert
    // on both the status and the message.
    let value = if content_type.starts_with("text/event-stream") {
        let data = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .next_back()
            .unwrap_or("null");
        serde_json::from_str(data.trim()).unwrap_or(Value::Null)
    } else {
        serde_json::from_str(&body).unwrap_or(Value::String(body))
    };
    (status, value)
}

fn mint_access_for(state: &AgUiState, account: &AccountId, email: &str) -> String {
    state
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

async fn mint_bot_key(base: &str, access: &str, coworker: &CoworkerId) -> (String, String) {
    let response = reqwest::Client::new()
        .post(format!("{base}/coworkers/{}/keys", coworker.as_str()))
        .header("Authorization", format!("Bearer {access}"))
        .send()
        .await
        .expect("mint");
    assert_eq!(response.status().as_u16(), 201, "bot key mint");
    let body: Value = response.json().await.expect("mint body");
    (
        body["key"].as_str().expect("key").to_string(),
        body["jti"].as_str().expect("jti").to_string(),
    )
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": { "name": "handwritten-test-client", "version": "0" },
    })
}

#[tokio::test]
async fn a_bot_key_shakes_hands_and_a_computerless_coworker_lists_an_empty_toolbox() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_computerless_coworker(&store, &account).await;
    let (app, state, _) = app_with(store.clone(), &host_email);
    let base = spawn(app).await;
    let access = mint_access_for(&state, &account, &host_email);
    let (bot_key, _) = mint_bot_key(&base, &access, &coworker).await;

    let (status, init) = rpc(&base, Some(&bot_key), 1, "initialize", initialize_params()).await;
    assert_eq!(status, 200, "a live bot key initializes: {init}");
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "tools capability advertised: {init}"
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"], "opengrok",
        "the handshake identifies OpenGrok, not the SDK: {init}"
    );
    let instructions = init["result"]["instructions"].as_str().unwrap_or("");
    assert!(
        instructions.contains("requestId"),
        "handshake says an Ask names a requestId, not that no card exists: {instructions}"
    );

    let (_, list) = rpc(&base, Some(&bot_key), 2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.is_empty(),
        "a coworker with no computer offers no tools, not broken ones: {list}"
    );

    // A call still fails CLOSED, with a reason, not a hang or a success.
    let (_, call) = rpc(
        &base,
        Some(&bot_key),
        3,
        "tools/call",
        json!({ "name": "shell", "arguments": { "command": "echo hi" } }),
    )
    .await;
    assert_eq!(call["result"]["isError"], json!(true), "{call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("no computer"),
        "the refusal names the reason: {text}"
    );

    // Reverse-exec is refused by name before the toolbox — a computerless coworker must not
    // see "no computer" here, or a leaked bot key could look like the channel simply isn't
    // provisioned rather than being excluded.
    let (_, reverse) = rpc(
        &base,
        Some(&bot_key),
        4,
        "tools/call",
        json!({ "name": USER_MACHINE_SHELL, "arguments": { "command": "echo hi" } }),
    )
    .await;
    assert_eq!(reverse["result"]["isError"], json!(true), "{reverse}");
    let reverse_text = reverse["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        reverse_text.contains("reverse-exec") && reverse_text.contains("not available"),
        "reverse-exec is excluded by name: {reverse_text}"
    );
    assert!(
        !reverse_text.contains("no computer"),
        "must not masquerade as a missing box: {reverse_text}"
    );

    // Both refusals left a durable row the owner can read, newest first, with the door's own
    // word for what happened and the request id the log line carries.
    let calls = mcp_calls(&base, &access, &coworker).await;
    assert_eq!(calls.status().as_u16(), 200);
    let calls: Value = calls.json().await.expect("calls json");
    let rows = calls.as_array().expect("an array");
    assert_eq!(rows.len(), 2, "one row per call: {calls}");
    assert_eq!(rows[0]["tool"], json!(USER_MACHINE_SHELL));
    assert_eq!(rows[0]["outcome"], json!("refused"));
    assert_eq!(rows[1]["tool"], json!("shell"));
    assert_eq!(rows[1]["outcome"], json!("refused"));
    assert_eq!(rows[1]["arguments"]["command"], json!("echo hi"));
    assert!(
        rows[1]["callId"].as_str().unwrap_or("").starts_with("mcp_"),
        "{calls}"
    );
    assert!(
        !rows[1]["requestId"].as_str().unwrap_or("").is_empty(),
        "{calls}"
    );
    assert!(rows[1]["atMs"].as_i64().unwrap_or(0) > 0, "{calls}");

    // Another account's view of this coworker is a 404, not an empty list.
    let stranger_email = format!("mcp-stranger-{}@og.local", uuid::Uuid::now_v7().simple());
    let stranger = seed_account(&store, &stranger_email).await;
    let stranger_access = mint_access_for(&state, &stranger, &stranger_email);
    let refused = mcp_calls(&base, &stranger_access, &coworker).await;
    assert_eq!(refused.status().as_u16(), 404);
    // And no bearer at all is a 401.
    let anonymous = reqwest::Client::new()
        .get(format!("{base}/coworkers/{}/mcp-calls", coworker.as_str()))
        .send()
        .await
        .expect("anonymous");
    assert_eq!(anonymous.status().as_u16(), 401);
}

/// `GET /coworkers/{id}/mcp-calls` as the owner's console would.
async fn mcp_calls(base: &str, access: &str, coworker: &CoworkerId) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}/coworkers/{}/mcp-calls", coworker.as_str()))
        .header("Authorization", format!("Bearer {access}"))
        .send()
        .await
        .expect("mcp-calls")
}

#[tokio::test]
async fn only_a_live_bot_key_opens_the_door() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("host-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_computerless_coworker(&store, &account).await;
    let (app, state, _) = app_with(store.clone(), &host_email);
    let base = spawn(app).await;
    let access = mint_access_for(&state, &account, &host_email);

    // The refusals are at the transport edge now: a real 401, so an OAuth-capable client can
    // discover it must authenticate, and the body names the fix. The message rides through as a
    // plain string (the guard does not answer JSON).
    let message_of = |value: &Value| value.as_str().unwrap_or_default().to_string();

    // No bearer at all: 401, refused with guidance, not a panic and not a listing.
    let (status, anonymous) = rpc(&base, None, 1, "tools/list", json!({})).await;
    assert_eq!(status, 401, "no bearer is unauthorized: {anonymous}");
    assert!(
        message_of(&anonymous).contains("bot key"),
        "the refusal says what credential to use: {anonymous}"
    );

    // An account access token is a person, not a coworker: 401, told how to mint the right thing.
    let (status, person) = rpc(&base, Some(&access), 2, "tools/list", json!({})).await;
    assert_eq!(status, 401, "a person's token is unauthorized: {person}");
    let person = message_of(&person);
    assert!(
        person.contains("bot key") && person.contains("/coworkers/"),
        "a person is pointed at the mint, not guessed a coworker for: {person}"
    );

    // Even initialize is gated — an anonymous scanner cannot confirm the door or read its SDK.
    let (status, _) = rpc(&base, None, 3, "initialize", initialize_params()).await;
    assert_eq!(status, 401, "initialize itself requires a bot key");

    // A browser origin is refused outright, with or without a token.
    let origin_refused = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Origin", "https://evil.example")
        .header("Content-Type", "application/json")
        .json(&json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": {} }))
        .send()
        .await
        .expect("request");
    assert_eq!(
        origin_refused.status().as_u16(),
        403,
        "a browser origin is refused"
    );

    // A revoked key answers revoked (401) — never a silent downgrade to anonymous.
    let (bot_key, jti) = mint_bot_key(&base, &access, &coworker).await;
    store.revoke_bot_key(&account, &jti).await.expect("revoke");
    let (status, revoked) = rpc(&base, Some(&bot_key), 4, "tools/list", json!({})).await;
    assert_eq!(status, 401, "a revoked key is unauthorized: {revoked}");
    assert!(
        message_of(&revoked).contains("revoked"),
        "a revoked key is named revoked: {revoked}"
    );
}

/// An MCP Ask has no in-flight run. The door synthesizes one and a real auto-review card;
/// the desktop verb that already answers conversation Asks settles it. Driven through
/// `reply_to_ask` (the shipped Ask path), not `tools/call`: that only Asks after the
/// toolbox is Ready (a computer).
#[tokio::test]
async fn an_mcp_ask_raises_a_real_card_the_desktop_can_answer() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("mcp-ask-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_computerless_coworker(&store, &account).await;
    let (app, state, gateway) = app_with(store.clone(), &host_email);
    let base = spawn(app).await;

    let call = ToolCall {
        id: format!("mcp_{}", uuid::Uuid::now_v7().simple()),
        name: "write_file".to_string(),
        arguments: json!({ "path": "/tmp/from-mcp", "content": "hi" }),
    };
    let result = ToolResult::awaiting(&call.id, AwaitingReason::AutoReview, "why");
    let error =
        opengrok_server::mcp_door::reply_to_ask(&gateway, &account, &coworker, &call, &result)
            .await;
    assert!(
        error.contains(&format!("requestId: {}", call.id)),
        "the MCP error names the card's requestId: {error}"
    );
    assert!(
        !error.contains("approval is not available over MCP"),
        "must not claim no card exists once one was raised: {error}"
    );

    let error_again =
        opengrok_server::mcp_door::reply_to_ask(&gateway, &account, &coworker, &call, &result)
            .await;
    assert!(
        error_again.contains(&format!("requestId: {}", call.id)),
        "a retry before answer reuses the same requestId: {error_again}"
    );

    // The same call through the door itself, while the card is up: the door finds the pending
    // Ask before it reaches the toolbox, answers "waiting", and the audit says so.
    let access = mint_access_for(&state, &account, &host_email);
    let (bot_key, _) = mint_bot_key(&base, &access, &coworker).await;
    let (_, waiting) = rpc(
        &base,
        Some(&bot_key),
        7,
        "tools/call",
        json!({ "name": call.name, "arguments": call.arguments }),
    )
    .await;
    assert_eq!(waiting["result"]["isError"], json!(true), "{waiting}");
    assert!(
        waiting["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains(&format!("requestId: {}", call.id)),
        "the door names the same card: {waiting}"
    );
    let rows: Value = mcp_calls(&base, &access, &coworker)
        .await
        .json()
        .await
        .expect("calls json");
    assert_eq!(rows[0]["tool"], json!("write_file"), "{rows}");
    assert_eq!(rows[0]["outcome"], json!("awaiting"), "{rows}");
    assert_eq!(
        rows[0]["arguments"]["path"],
        json!("/tmp/from-mcp"),
        "{rows}"
    );

    let awaiting = store.awaiting_approval(&account).await.expect("awaiting");
    assert_eq!(
        awaiting.len(),
        1,
        "the MCP Ask left exactly one suspended run"
    );
    let (run, _) = store.load_run(&awaiting[0]).await.expect("run");
    assert_eq!(run.status, RunStatus::AwaitingApproval);
    assert_eq!(
        run.pending.as_ref().map(|p| p.call_id.as_str()),
        Some(call.id.as_str())
    );
    assert_eq!(
        run.pending.as_ref().map(|p| p.reason),
        Some(opengrok_core::run::SuspendReason::AutoReview)
    );
    assert!(
        run.thread_id.starts_with("mcp-"),
        "an MCP Ask is its own thread, not a conversation turn: {}",
        run.thread_id
    );
    assert_eq!(run.model.as_deref(), Some("oag/cheap"));

    let transcript = store
        .gateway_transcript(&coworker)
        .await
        .expect("transcript");
    let card = transcript
        .iter()
        .find(|entry| entry["message"]["type"] == "auto-review-approval")
        .cloned()
        .expect("an auto-review card was appended");
    assert_eq!(card["message"]["approval"]["requestId"], call.id);
    assert_eq!(card["message"]["approval"]["status"], "pending");
    assert_eq!(card["message"]["approval"]["surface"], "box_shell");
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let res = reqwest::Client::new()
        .post(format!("{base}/api/resolveAutoReviewApproval"))
        .header("Authorization", "Bearer test-bearer")
        .json(&json!({
            "entryId": entry_id,
            "requestId": call.id,
            "resolution": "approved",
            "agentId": coworker.as_str(),
        }))
        .send()
        .await
        .expect("resolve");
    assert_eq!(
        res.status().as_u16(),
        200,
        "the desktop verb answers the MCP card"
    );
    let body: Value = res.json().await.expect("body");
    assert_eq!(body["ok"], true, "{body}");

    let flipped = store
        .gateway_transcript(&coworker)
        .await
        .expect("transcript")
        .into_iter()
        .find(|entry| entry["id"] == entry_id)
        .expect("card still present");
    assert_eq!(flipped["message"]["approval"]["status"], "approved");

    let (run, _) = store.load_run(&awaiting[0]).await.expect("run");
    assert_eq!(
        run.status,
        RunStatus::Finished,
        "an MCP run is finished on the card, not resumed as a conversation"
    );
    assert!(run.answered.contains(&call.id));
    assert_eq!(
        opengrok_server::mcp_door::take_mcp_allow_once(
            &store,
            &coworker,
            "write_file",
            &json!({ "path": "/tmp/other", "content": "nope" }),
        )
        .await,
        None,
        "a different command of the same tool cannot spend this yes"
    );
    // Arguments round-tripped through jsonb (key order not preserved). Take with the
    // other key order so a string-hash of to_string would miss and Value equality hits.
    let reordered = json!({ "content": "hi", "path": "/tmp/from-mcp" });
    assert_eq!(
        opengrok_server::mcp_door::take_mcp_allow_once(&store, &coworker, "write_file", &reordered)
            .await,
        Some((call.id.clone(), false)),
        "allow-once matches by Value equality, not key insertion order; a judge yes is not a gate yes"
    );
    assert_eq!(
        opengrok_server::mcp_door::take_mcp_allow_once(
            &store,
            &coworker,
            "write_file",
            &call.arguments
        )
        .await,
        None,
        "the yes is one-shot"
    );
}

/// A policy grant's "needs a human yes" over MCP raises the SAME card as the judge's ask, with
/// the grant's reason and no proposed rule; the desktop's verb answers it, and the remembered
/// yes is a GATE yes for the retry — not a judge skip.
#[tokio::test]
async fn a_policy_approval_ask_raises_the_card_and_its_yes_releases_the_gate() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let host_email = format!("mcp-policy-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &host_email).await;
    let coworker = seed_computerless_coworker(&store, &account).await;
    let (app, _state, gateway) = app_with(store.clone(), &host_email);
    let base = spawn(app).await;

    let call = ToolCall {
        id: format!("mcp_{}", uuid::Uuid::now_v7().simple()),
        name: "shell".to_string(),
        arguments: json!({ "command": "echo policy" }),
    };
    let result = ToolResult::awaiting(
        &call.id,
        AwaitingReason::PolicyApproval,
        "only the on-call may run shell here",
    );
    let error =
        opengrok_server::mcp_door::reply_to_ask(&gateway, &account, &coworker, &call, &result)
            .await;
    assert!(
        error.contains(&format!("requestId: {}", call.id)),
        "the MCP error names the card's requestId: {error}"
    );
    assert!(
        !error.contains("approval is not available over MCP"),
        "a policy ask has a card now: {error}"
    );

    let awaiting = store.awaiting_approval(&account).await.expect("awaiting");
    assert_eq!(awaiting.len(), 1);
    let (run, _) = store.load_run(&awaiting[0]).await.expect("run");
    assert_eq!(
        run.pending.as_ref().map(|p| p.reason),
        Some(opengrok_core::run::SuspendReason::PolicyApproval)
    );

    let transcript = store
        .gateway_transcript(&coworker)
        .await
        .expect("transcript");
    let card = transcript
        .iter()
        .find(|entry| entry["message"]["type"] == "auto-review-approval")
        .cloned()
        .expect("the policy ask rides the auto-review card");
    let approval = &card["message"]["approval"];
    assert_eq!(approval["requestId"], call.id);
    assert_eq!(approval["status"], "pending");
    assert_eq!(approval["reason"], "only the on-call may run shell here");
    assert_eq!(approval["command"], "echo policy");
    assert!(
        approval.get("proposedRule").is_none(),
        "a policy card offers no rule to write: {approval}"
    );
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    // A retry before the answer reuses the pending card rather than raising a second one.
    let again =
        opengrok_server::mcp_door::reply_to_ask(&gateway, &account, &coworker, &call, &result)
            .await;
    assert!(
        again.contains(&format!("requestId: {}", call.id)),
        "{again}"
    );

    let res = reqwest::Client::new()
        .post(format!("{base}/api/resolveAutoReviewApproval"))
        .header("Authorization", "Bearer test-bearer")
        .json(&json!({
            "entryId": entry_id,
            "requestId": call.id,
            "resolution": "approved",
            "agentId": coworker.as_str(),
        }))
        .send()
        .await
        .expect("resolve");
    assert_eq!(
        res.status().as_u16(),
        200,
        "the desktop verb answers a policy card too"
    );

    let flipped = store
        .gateway_transcript(&coworker)
        .await
        .expect("transcript")
        .into_iter()
        .find(|entry| entry["id"] == entry_id)
        .expect("card still present");
    assert_eq!(flipped["message"]["approval"]["status"], "approved");
    let (run, _) = store.load_run(&awaiting[0]).await.expect("run");
    assert_eq!(run.status, RunStatus::Finished);

    assert_eq!(
        opengrok_server::mcp_door::take_mcp_allow_once(&store, &coworker, "shell", &call.arguments)
            .await,
        Some((call.id.clone(), true)),
        "a policy yes is remembered as a GATE yes for the retry"
    );
}
