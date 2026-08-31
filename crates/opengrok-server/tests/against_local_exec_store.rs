//! The reverse-exec policy, from the store through the gate: a stored mode + rules load into a
//! `LocalExecPolicy` and `decide` gives the right verdict — closed by default. Needs Postgres;
//! skips loudly without it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use opengrok_server::local_exec::{self, LocalExecDecision, LocalExecMode};
use opengrok_store::PgStore;

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

async fn store(database_url: &str) -> PgStore {
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

#[tokio::test]
async fn a_stored_policy_loads_and_the_gate_judges_it() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    // Unique account + machine so parallel runs don't collide.
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());

    // Nothing stored ⇒ the closed default: never, deny everything.
    let policy = local_exec::load_policy(&store, &account, &machine).await;
    assert_eq!(policy.mode, LocalExecMode::Never);
    assert!(matches!(
        local_exec::decide(&policy, "echo hi"),
        LocalExecDecision::Deny(_)
    ));

    // Turn it to ask, allow `git status`, deny `rm`.
    store
        .set_local_exec_mode(&account, &machine, "ask", 1)
        .await
        .expect("set mode");
    store
        .add_local_exec_rule(&account, &machine, "allow", "git status", 2)
        .await
        .expect("allow");
    store
        .add_local_exec_rule(&account, &machine, "deny", "rm", 3)
        .await
        .expect("deny");

    let policy = local_exec::load_policy(&store, &account, &machine).await;
    assert_eq!(policy.mode, LocalExecMode::Ask);
    assert_eq!(policy.allow, vec!["git status".to_string()]);
    assert_eq!(policy.deny, vec!["rm".to_string()]);
    assert_eq!(
        local_exec::decide(&policy, "git status --short"),
        LocalExecDecision::Allow
    );
    assert!(matches!(
        local_exec::decide(&policy, "rm -rf /"),
        LocalExecDecision::Deny(_)
    ));
    assert_eq!(
        local_exec::decide(&policy, "curl example.com"),
        LocalExecDecision::Ask
    );

    // Remove the allow rule ⇒ that command goes back to ask.
    store
        .remove_local_exec_rule(&account, &machine, "allow", "git status")
        .await
        .expect("remove");
    let policy = local_exec::load_policy(&store, &account, &machine).await;
    assert!(policy.allow.is_empty());
    assert_eq!(
        local_exec::decide(&policy, "git status"),
        LocalExecDecision::Ask
    );
}

#[tokio::test]
async fn daemon_enrolment_and_audit_round_trip() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());

    // Enrol → the jti is current and not revoked.
    store
        .enrol_daemon(&account, &machine, "Test Mac", "jti-1", 1)
        .await
        .expect("enrol");
    assert_eq!(
        store.daemon_jti(&account, &machine).await.expect("jti"),
        Some(("jti-1".to_string(), false))
    );
    // Re-enrol rotates the jti and clears revocation.
    store
        .enrol_daemon(&account, &machine, "Test Mac", "jti-2", 2)
        .await
        .expect("re-enrol");
    assert_eq!(
        store.daemon_jti(&account, &machine).await.expect("jti"),
        Some(("jti-2".to_string(), false))
    );
    // Revoke → the row says revoked (the poll gate refuses it).
    store
        .revoke_daemon(&account, &machine)
        .await
        .expect("revoke");
    assert_eq!(
        store.daemon_jti(&account, &machine).await.expect("jti"),
        Some(("jti-2".to_string(), true))
    );
    assert_eq!(store.list_daemons(&account).await.expect("list").len(), 1);

    // Audit: a row at enqueue, then its result. A unique id — the persistent DB keeps rows.
    let audit_id = format!("ax_{}", uuid::Uuid::now_v7().simple());
    store
        .audit_local_exec(
            &audit_id, &account, &machine, "bot cw_x", "uptime", "allow", 10,
        )
        .await
        .expect("audit");
    store
        .finish_local_exec_audit(&audit_id, "success", Some(0), 20)
        .await
        .expect("finish");
    let log = store.local_exec_audit_log(&account, 50).await.expect("log");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["command"], "uptime");
    assert_eq!(log[0]["decision"], "allow");
    assert_eq!(log[0]["outcome"], "success");
    assert_eq!(log[0]["exitCode"], 0);
}

// -------------------------------------------------------------------------------------------------
// The enqueue path end to end (slices 4–5): the gate judges a command, an allowed one is dispatched
// to a (fake) daemon over the broker, the result comes back, and every path writes the right audit
// row. No HTTP — the broker is exercised directly, standing in for a connected daemon.
// -------------------------------------------------------------------------------------------------

use opengrok_server::auth::token::TokenMinter;
use opengrok_server::auth::routes::AuthState;
use opengrok_server::local_exec::{enqueue_and_wait, EnqueueResult, Origin};
use opengrok_server::local_exec::broker::ExecOutcome;
use std::sync::Arc;

fn auth_state(store: PgStore) -> AuthState {
    AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"a-test-secret-that-is-long-enough")),
        "host@og.local".to_string(),
    )
}

/// A fake daemon: connect to the broker for `machine` NOW (so the provider is registered before we
/// enqueue), then in the background reply to the first `exec` frame with the given outcome.
async fn fake_daemon(state: &AuthState, machine: &str, reply: ExecOutcome) {
    let broker = state.local_exec.clone();
    let machine = machine.to_string();
    // Connect synchronously — the provider must exist before enqueue dispatches, or it refuses.
    let mut stream = broker.connect(&machine).await;
    tokio::spawn(async move {
        while let Some(frame) = stream.recv().await {
            if frame["type"] == "exec" {
                let request_id = frame["requestId"].as_str().unwrap_or_default().to_string();
                broker.resolve(&machine, &request_id, reply.clone()).await;
                return;
            }
        }
    });
}

fn success() -> ExecOutcome {
    ExecOutcome {
        case: "success".to_string(),
        exit_code: Some(0),
        stdout: "hi\n".to_string(),
        stderr: String::new(),
        detail: String::new(),
    }
}

#[tokio::test]
async fn a_bot_allowlisted_command_runs_and_audits_success() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());

    state.store.set_local_exec_mode(&account, &machine, "ask", 1).await.expect("mode");
    state.store.add_local_exec_rule(&account, &machine, "allow", "echo", 2).await.expect("allow");

    fake_daemon(&state, &machine, success()).await;
    let result = enqueue_and_wait(
        &state, &account, &machine, "echo hi", &["echo hi".to_string()],
        Origin::Bot("cw_1".to_string()),
    ).await;
    assert!(matches!(&result, EnqueueResult::Ran(o) if o.succeeded()));

    let log = state.store.local_exec_audit_log(&account, 10).await.expect("log");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["decision"], "allow");
    assert_eq!(log[0]["outcome"], "success");
    assert_eq!(log[0]["origin"], "bot cw_1");
}

#[tokio::test]
async fn a_bot_unlisted_command_needs_approval_and_never_dispatches() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    state.store.set_local_exec_mode(&account, &machine, "ask", 1).await.expect("mode");

    // No daemon connected — if this dispatched it would refuse; instead it must suspend for a person.
    let result = enqueue_and_wait(
        &state, &account, &machine, "curl example.com", &["curl example.com".to_string()],
        Origin::Bot("cw_1".to_string()),
    ).await;
    assert!(matches!(result, EnqueueResult::NeedsApproval));
    let log = state.store.local_exec_audit_log(&account, 10).await.expect("log");
    assert_eq!(log[0]["decision"], "ask");
    assert_eq!(log[0]["outcome"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_user_direct_command_skips_ask_and_runs() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    // Ask mode, no allow rule — a bot would suspend here, but the user is the approver.
    state.store.set_local_exec_mode(&account, &machine, "ask", 1).await.expect("mode");

    fake_daemon(&state, &machine, success()).await;
    let result = enqueue_and_wait(
        &state, &account, &machine, "whoami", &["whoami".to_string()], Origin::User,
    ).await;
    assert!(matches!(result, EnqueueResult::Ran(_)));
    let log = state.store.local_exec_audit_log(&account, 10).await.expect("log");
    assert_eq!(log[0]["decision"], "allow-user");
    assert_eq!(log[0]["outcome"], "success");
    assert_eq!(log[0]["origin"], "user");
}

#[tokio::test]
async fn a_denylisted_command_is_refused_for_the_user_too() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    state.store.set_local_exec_mode(&account, &machine, "ask", 1).await.expect("mode");
    state.store.add_local_exec_rule(&account, &machine, "deny", "rm", 2).await.expect("deny");

    let result = enqueue_and_wait(
        &state, &account, &machine, "rm -rf /", &["rm -rf /".to_string()], Origin::User,
    ).await;
    assert!(matches!(result, EnqueueResult::Refused(_)));
    let log = state.store.local_exec_audit_log(&account, 10).await.expect("log");
    assert_eq!(log[0]["decision"], "deny");
}

#[tokio::test]
async fn an_allowed_command_with_no_daemon_is_refused_not_hung() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    state.store.set_local_exec_mode(&account, &machine, "bypass", 1).await.expect("mode");

    // Bypass allows it, but nothing is connected: refuse crisply rather than wait forever.
    let result = enqueue_and_wait(
        &state, &account, &machine, "echo hi", &["echo hi".to_string()], Origin::User,
    ).await;
    assert!(matches!(&result, EnqueueResult::Refused(reason) if reason.contains("not connected")));
    let log = state.store.local_exec_audit_log(&account, 10).await.expect("log");
    assert_eq!(log[0]["decision"], "allow");
    assert_eq!(log[0]["outcome"], "spawnError");
}
