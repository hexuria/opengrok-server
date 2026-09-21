//! A coworker hired before `open_url` and `computer` existed kept a grant of exactly the old
//! built-in set, and told the person it could not see its own screen. The schema widens that
//! set on boot; a list somebody chose on purpose is left alone.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_policy::ToolSet;
use opengrok_store::PgStore;

async fn connect() -> Option<PgStore> {
    let database_url =
        opengrok_store::gate_database_or_panic(std::env::var("OG_DATABASE_URL").ok()?);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    Some(PgStore::new(pool))
}

fn only(tools: &[&str]) -> ToolSet {
    ToolSet::only(
        tools
            .iter()
            .map(|tool| tool.to_string())
            .collect::<Vec<_>>(),
    )
}

#[tokio::test]
async fn a_grant_of_the_old_builtin_set_follows_the_builtins_and_a_chosen_list_does_not() {
    let Some(store) = connect().await else {
        eprintln!("OG_DATABASE_URL is not set; skipping");
        return;
    };
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let account = AccountId::from_stored(format!("acct_grants_{suffix}"));
    let old = CoworkerId::from_stored(format!("cw_old_{suffix}"));
    let chosen = CoworkerId::from_stored(format!("cw_chosen_{suffix}"));
    let previous_builtins = only(&["read_file", "shell", "write_file"]);
    let chosen_list = only(&["shell"]);
    store
        .grant_access(
            &account,
            &old,
            &previous_builtins,
            &previous_builtins,
            &ToolSet::None,
            1,
        )
        .await
        .expect("grant the old set");
    store
        .grant_access(
            &account,
            &chosen,
            &chosen_list,
            &chosen_list,
            &ToolSet::None,
            1,
        )
        .await
        .expect("grant a chosen list");

    // The boot-time schema pass is what widens; run it again as a restart would.
    opengrok_store::migrations::run(store.pool())
        .await
        .expect("migrations");

    let widened = store.policy_for(&account, &old).await.expect("policy");
    // The widening statements chain, so a row written as the three-tool set arrives at the
    // last widened set in one boot — which is what "follows the built-ins" has to mean.
    // `credential.request` came and went: the backfill that added it stays as written, and the
    // statement after it takes the name out again, so a widened row ends on today's built-ins.
    let expected = only(&[
        "computer",
        "open_url",
        "read_file",
        "request_user_form",
        "run_recipe",
        "shell",
        "write_file",
    ]);
    assert_eq!(
        widened.grant.map(|grant| grant.profile),
        Some(expected.clone())
    );
    assert_eq!(widened.ceiling.map(|ceiling| ceiling.tools), Some(expected));

    let kept = store.policy_for(&account, &chosen).await.expect("policy");
    assert_eq!(
        kept.grant.map(|grant| grant.profile),
        Some(chosen_list.clone())
    );
    assert_eq!(kept.ceiling.map(|ceiling| ceiling.tools), Some(chosen_list));
}
