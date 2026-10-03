//! The reverse-exec policy, from the store through the gate: a stored mode + rules load into a
//! `LocalExecPolicy` and `decide` gives the right verdict — closed by default. Needs Postgres;
//! skips loudly without it.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_server::local_exec::{self, LocalExecDecision, LocalExecMode};
use opengrok_store::PgStore;

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
        LocalExecDecision::Deny { .. }
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
        LocalExecDecision::Deny { .. }
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
        Some(("jti-1".to_string(), false, true))
    );
    // Re-enrol rotates the jti and clears revocation.
    store
        .enrol_daemon(&account, &machine, "Test Mac", "jti-2", 2)
        .await
        .expect("re-enrol");
    assert_eq!(
        store.daemon_jti(&account, &machine).await.expect("jti"),
        Some(("jti-2".to_string(), false, true))
    );
    // Revoke → the row says revoked (the poll gate refuses it).
    store
        .revoke_daemon(&account, &machine)
        .await
        .expect("revoke");
    assert_eq!(
        store.daemon_jti(&account, &machine).await.expect("jti"),
        Some(("jti-2".to_string(), true, true))
    );
    assert_eq!(store.list_daemons(&account).await.expect("list").len(), 1);

    // Audit: a row at enqueue, then its result. A unique id — the persistent DB keeps rows.
    let audit_id = format!("ax_{}", uuid::Uuid::now_v7().simple());
    store
        .audit_local_exec(
            &audit_id, &account, &machine, "bot cw_x", "uptime", "allow", None, 10,
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

/// ONE RELAY SWITCH PER COMPUTER, on the account's own row: on when enrolled; another account's
/// machine under the same id is untouched and unknown to it; a revoked one is left as it was;
/// every one is switched at once, revoked or not; and one enrolled again keeps its switch.
#[tokio::test]
async fn a_computers_relay_switch_is_its_own_and_outlives_its_re_enrolment() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let [ada, eve] = ["ada", "eve"].map(|who| format!("acct_{who}_{suffix}"));
    for (account, machine) in [(&ada, "mac-a"), (&ada, "mac-b"), (&eve, "mac-a")] {
        let enrolled = store.enrol_daemon(account, machine, "Mac", "jti-1", 1);
        enrolled.await.expect("enrol");
    }
    let on = |account: &str, machine: &str| {
        let (store, account, machine) = (store.clone(), account.to_string(), machine.to_string());
        async move {
            let row = store.daemon_jti(&account, &machine).await.expect("read");
            row.expect("enrolled").2
        }
    };
    let listed = store.list_daemons(&ada).await.expect("list");
    assert!(listed.iter().all(|row| row.relay_enabled), "{listed:?}");

    let row = store.set_relay(&ada, "mac-a", false).await.expect("switch");
    let row = row.expect("Ada's");
    assert_eq!(
        (row.machine_id.as_str(), row.relay_enabled),
        ("mac-a", false)
    );
    assert!(on(&eve, "mac-a").await, "Eve's own");
    let unknown = store
        .set_relay(&ada, "mac-nobody", false)
        .await
        .expect("switch");
    assert_eq!(unknown, None);
    let again = store.enrol_daemon(&ada, "mac-a", "Mac", "jti-2", 2);
    again.await.expect("enrol again");
    assert!(!on(&ada, "mac-a").await, "kept");

    store.revoke_daemon(&ada, "mac-b").await.expect("revoke");
    let revoked = store.set_relay(&ada, "mac-b", false).await.expect("switch");
    let revoked = revoked.expect("Ada's");
    assert_eq!(
        (revoked.revoked, revoked.relay_enabled),
        (true, true),
        "as it was"
    );
    let mut every = store.set_relays(&ada, false).await.expect("every one");
    every.sort();
    assert_eq!(every, ["mac-a", "mac-b"]);
    assert!(!on(&ada, "mac-a").await && !on(&ada, "mac-b").await);
    assert!(on(&eve, "mac-a").await, "Eve's still");
    store.set_relays(&ada, true).await.expect("every one");
    assert!(on(&ada, "mac-a").await && on(&ada, "mac-b").await);
}

/// Enrol `account`'s `machine`, or enrol it again: whether its relay is then on.
async fn enrolled_on(store: &PgStore, account: &str, machine: &str) -> bool {
    let enrolled = store.enrol_daemon(account, machine, "Mac", "jti", 1);
    enrolled.await.expect("enrol");
    let row = store.daemon_jti(account, machine).await.expect("read");
    row.expect("enrolled").2
}

/// RELAY OFF STICKS ON A NEW COMPUTER, the owner's call (review of #342): enrolled while every
/// un-revoked computer of its account is off, a computer is enrolled off, or enrolling it would
/// turn back on the relay its person turned off. With any one on it is on, and with none, an
/// account's first or one whose computers are all revoked, as before. A revoked one is no
/// computer either way.
#[tokio::test]
async fn a_computer_enrolled_while_every_other_is_off_is_enrolled_off() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let [ada, bob] = ["ada", "bob"].map(|who| format!("acct_{who}_{suffix}"));
    assert!(enrolled_on(&store, &ada, "mac-a").await, "no computer: on");
    assert!(enrolled_on(&store, &ada, "mac-b").await, "one on: on");
    store.revoke_daemon(&ada, "mac-b").await.expect("revoke");
    let off = store.set_relay(&ada, "mac-a", false).await.expect("switch");
    assert!(off.is_some_and(|row| !row.relay_enabled));
    assert!(
        !enrolled_on(&store, &ada, "mac-c").await,
        "every one off: off, mac-b on but revoked"
    );

    assert!(enrolled_on(&store, &bob, "mac-a").await);
    store.set_relay(&bob, "mac-a", false).await.expect("switch");
    store.revoke_daemon(&bob, "mac-a").await.expect("revoke");
    assert!(
        enrolled_on(&store, &bob, "mac-b").await,
        "every one revoked: on, as the first"
    );
}

/// A REVOKED COMPUTER ENROLLED AGAIN IS A NEW ONE (Cursor's review of #344): its switch is set as
/// a new computer's is, never kept, or one revoked while on came back on beside others all off
/// and turned back on the relay its person had turned off. One never revoked keeps its own when
/// enrolled again, whatever the others say.
#[tokio::test]
async fn a_revoked_computer_enrolled_again_is_switched_as_a_new_one() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let [ada, bob, cyd] = ["ada", "bob", "cyd"].map(|who| format!("acct_{who}_{suffix}"));
    let machines = [
        (&ada, "mac-a"),
        (&ada, "mac-b"),
        (&bob, "mac-a"),
        (&bob, "mac-b"),
        (&cyd, "mac-a"),
    ];
    for (account, machine) in machines {
        assert!(enrolled_on(&store, account, machine).await);
    }
    // Ada's mac-b revoked on, then her mac-a switched off; Bob's mac-b switched off and revoked
    // beside his mac-a on; and Cyd's one computer switched off and revoked.
    store.revoke_daemon(&ada, "mac-b").await.expect("revoke");
    store.set_relay(&ada, "mac-a", false).await.expect("switch");
    store.set_relay(&bob, "mac-b", false).await.expect("switch");
    store.revoke_daemon(&bob, "mac-b").await.expect("revoke");
    store.set_relay(&cyd, "mac-a", false).await.expect("switch");
    store.revoke_daemon(&cyd, "mac-a").await.expect("revoke");
    let again = [
        enrolled_on(&store, &ada, "mac-b").await,
        enrolled_on(&store, &bob, "mac-b").await,
        enrolled_on(&store, &cyd, "mac-a").await,
    ];
    assert_eq!(
        again,
        [false, true, true],
        "as a new one, never its own: off beside every other off, on beside one on, on alone"
    );

    store.set_relay(&ada, "mac-b", true).await.expect("switch");
    let own = [
        enrolled_on(&store, &ada, "mac-a").await,
        enrolled_on(&store, &ada, "mac-b").await,
    ];
    let said = "never revoked, its own: off beside one on, on beside every other off";
    assert_eq!(own, [false, true], "{said}");
}

// -------------------------------------------------------------------------------------------------
// The enqueue path end to end (slices 4–5): the gate judges a command, an allowed one is dispatched
// to a (fake) daemon over the broker, the result comes back, and every path writes the right audit
// row. No HTTP — the broker is exercised directly, standing in for a connected daemon.
// -------------------------------------------------------------------------------------------------

use opengrok_server::auth::routes::AuthState;
use opengrok_server::auth::token::TokenMinter;
use opengrok_server::local_exec::broker::ExecOutcome;
use opengrok_server::local_exec::{EnqueueResult, Origin, enqueue_and_wait};
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
async fn fake_daemon(state: &AuthState, account: &str, machine: &str, reply: ExecOutcome) {
    let broker = state.local_exec.clone();
    let machine = machine.to_string();
    // Connect synchronously — the provider must exist before enqueue dispatches, or it refuses.
    let mut stream = broker.connect(account, &machine).await;
    tokio::spawn(async move {
        while let Some(frame) = stream.recv().await {
            if frame["kind"] == "exec" {
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

    state
        .store
        .set_local_exec_mode(&account, &machine, "ask", 1)
        .await
        .expect("mode");
    state
        .store
        .add_local_exec_rule(&account, &machine, "allow", "echo", 2)
        .await
        .expect("allow");

    fake_daemon(&state, &account, &machine, success()).await;
    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "echo hi",
        Origin::Bot("cw_1".to_string()),
        "appr",
        false,
    )
    .await;
    assert!(matches!(&result, EnqueueResult::Ran(o) if o.succeeded()));

    let log = state
        .store
        .local_exec_audit_log(&account, 10)
        .await
        .expect("log");
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
    state
        .store
        .set_local_exec_mode(&account, &machine, "ask", 1)
        .await
        .expect("mode");

    // No daemon connected — if this dispatched it would refuse; instead it must suspend for a person.
    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "curl example.com",
        Origin::Bot("cw_1".to_string()),
        "appr",
        false,
    )
    .await;
    assert!(matches!(result, EnqueueResult::NeedsApproval));
    let log = state
        .store
        .local_exec_audit_log(&account, 10)
        .await
        .expect("log");
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
    state
        .store
        .set_local_exec_mode(&account, &machine, "ask", 1)
        .await
        .expect("mode");

    fake_daemon(&state, &account, &machine, success()).await;
    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "whoami",
        Origin::User,
        "appr",
        false,
    )
    .await;
    assert!(matches!(result, EnqueueResult::Ran(_)));
    let log = state
        .store
        .local_exec_audit_log(&account, 10)
        .await
        .expect("log");
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
    state
        .store
        .set_local_exec_mode(&account, &machine, "ask", 1)
        .await
        .expect("mode");
    state
        .store
        .add_local_exec_rule(&account, &machine, "deny", "rm", 2)
        .await
        .expect("deny");

    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "rm -rf /",
        Origin::User,
        "appr",
        false,
    )
    .await;
    // The model is told a rule refused it, not which (#224); the person's audit row says which.
    let EnqueueResult::Refused(why) = result else {
        panic!("a denied command is refused");
    };
    assert_eq!(why, opengrok_server::local_exec::DENIED_BY_A_RULE);
    assert!(!why.contains("rm"), "{why}");
    let log = state
        .store
        .local_exec_audit_log(&account, 10)
        .await
        .expect("log");
    assert_eq!(log[0]["decision"], "deny");
    assert_eq!(log[0]["rule"], "rm");
}

#[tokio::test]
async fn an_allowed_command_with_no_daemon_is_refused_not_hung() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    state
        .store
        .set_local_exec_mode(&account, &machine, "bypass", 1)
        .await
        .expect("mode");

    // Bypass allows it, but nothing is connected: refuse crisply rather than wait forever.
    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "echo hi",
        Origin::User,
        "appr",
        false,
    )
    .await;
    assert!(matches!(&result, EnqueueResult::Refused(reason) if reason.contains("not connected")));
    let log = state
        .store
        .local_exec_audit_log(&account, 10)
        .await
        .expect("log");
    assert_eq!(log[0]["decision"], "allow");
    // A Mac that is asleep or signed out is not a command that failed to spawn. The audit keeps
    // the two apart so "what happened on my laptop" does not read as a broken daemon.
    assert_eq!(log[0]["outcome"], "offline");
}

#[test]
fn offline_outcome_is_its_own_case() {
    let offline = ExecOutcome::offline("the daemon for this machine is not connected");
    assert_eq!(offline.case, "offline");
    assert_eq!(offline.exit_code, None);
    assert!(!offline.succeeded());
    assert_eq!(
        offline.render(),
        "offline: the daemon for this machine is not connected"
    );
    // A garbled reply is still spawnError: offline is only for a machine nobody could reach.
    assert_eq!(ExecOutcome::malformed("garbled").case, "spawnError");
}

// -------------------------------------------------------------------------------------------------
// #203/#204 end to end: the gate reads the shell line, not its first words, on the enqueue path.
// -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_chained_command_after_a_denied_word_is_refused_for_the_user_too() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    state
        .store
        .set_local_exec_mode(&account, &machine, "ask", 1)
        .await
        .expect("mode");
    state
        .store
        .add_local_exec_rule(&account, &machine, "deny", "rm", 2)
        .await
        .expect("deny");

    // The user skips Ask, so the deny rule is their only guard: `true; rm` must still meet it.
    // No daemon — a deny returns before dispatch, so "not connected" would mean the gate missed.
    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "true; rm -rf x",
        Origin::User,
        "appr",
        false,
    )
    .await;
    assert!(
        matches!(&result, EnqueueResult::Refused(reason) if reason.contains("deny rule") && !reason.contains("`rm`")),
        "a chained rm must meet the deny rule"
    );
    let log = state
        .store
        .local_exec_audit_log(&account, 10)
        .await
        .expect("log");
    assert_eq!(log[0]["decision"], "deny");
    assert_eq!(log[0]["rule"], "rm");
    assert_eq!(log[0]["command"], "true; rm -rf x");
}

#[tokio::test]
async fn a_bot_chained_command_after_an_allow_needs_approval() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    state
        .store
        .set_local_exec_mode(&account, &machine, "ask", 1)
        .await
        .expect("mode");
    state
        .store
        .add_local_exec_rule(&account, &machine, "allow", "echo", 2)
        .await
        .expect("allow");

    // A connected daemon that would run it: the only thing between `; rm -rf ~` and the Mac is
    // the gate asking a person.
    fake_daemon(&state, &account, &machine, success()).await;
    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "echo hi; rm -rf ~",
        Origin::Bot("cw_1".to_string()),
        "appr",
        false,
    )
    .await;
    assert!(matches!(result, EnqueueResult::NeedsApproval));
    let log = state
        .store
        .local_exec_audit_log(&account, 10)
        .await
        .expect("log");
    assert_eq!(log[0]["decision"], "ask");
}

#[tokio::test]
async fn the_daemon_is_sent_the_servers_own_split() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let state = auth_state(store);
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    state
        .store
        .set_local_exec_mode(&account, &machine, "bypass", 1)
        .await
        .expect("mode");

    // Capture the exec frame the daemon would receive, then answer it.
    let broker = state.local_exec.clone();
    let mut stream = broker.connect(&account, &machine).await;
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let daemon_machine = machine.clone();
    tokio::spawn(async move {
        while let Some(frame) = stream.recv().await {
            if frame["kind"] == "exec" {
                let request_id = frame["requestId"].as_str().unwrap_or_default().to_string();
                let _ = seen_tx.send(frame["serverMessage"]["shellStreamArgs"].clone());
                broker
                    .resolve(&daemon_machine, &request_id, success())
                    .await;
                return;
            }
        }
    });
    let result = enqueue_and_wait(
        &state,
        &account,
        &machine,
        "cd src && cargo test",
        Origin::User,
        "appr",
        false,
    )
    .await;
    assert!(matches!(result, EnqueueResult::Ran(_)));
    let args = seen_rx.await.expect("an exec frame");
    assert_eq!(
        args["simpleCommands"],
        serde_json::json!(["cd src", "cargo test"])
    );
    // The line the shell runs is still the whole command, after the PATH preamble.
    assert!(
        args["command"]
            .as_str()
            .is_some_and(|line| line.ends_with("; cd src && cargo test"))
    );
}
