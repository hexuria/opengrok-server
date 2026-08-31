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
