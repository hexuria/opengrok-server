//! #359: a Bot's plugin tools are one ceiling row, on for a new hire, and every Bot hired before
//! it gains them ONCE, in its ceiling and in its owner's profile, which a turn's policy intersects
//! with it. After that pass a ceiling without them is one its owner switched off, and no later
//! boot may switch it back on. `All` admits them already, and a ceiling that admits nothing is not
//! handed them.
//!
//! Its own test binary, so its own database: it forgets the pass and boots again, which touches
//! every ceiling in the database.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_policy::ToolSet;
use opengrok_store::PgStore;

const PASS: &str = "the-plugin-tools-join-the-ceiling";

async fn boot(store: &PgStore, forget: bool) {
    if forget {
        sqlx::query("delete from schema_migrations where name = $1")
            .bind(PASS)
            .execute(store.pool())
            .await
            .expect("forget the pass");
    }
    opengrok_store::migrations::run(store.pool())
        .await
        .expect("boot");
}

async fn policy(store: &PgStore, account: &AccountId, bot: &CoworkerId) -> (ToolSet, ToolSet) {
    let policy = store.policy_for(account, bot).await.expect("policy");
    let ceiling = policy.ceiling.expect("a ceiling").tools;
    (ceiling, policy.grant.expect("a grant").profile)
}

#[tokio::test]
async fn every_bot_from_before_gains_the_plugin_tools_once_and_one_switched_off_stays_off() {
    let url = match std::env::var("OG_DATABASE_URL") {
        Ok(url) => opengrok_store::gate_database_or_panic(url),
        Err(_) => {
            eprintln!("skipping: OG_DATABASE_URL is not set");
            return;
        }
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool).await.expect("boot");
    let store = PgStore::new(pool);
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let account = AccountId::from_stored(format!("acct_plugin_tools_{suffix}"));
    let bot = |name: &str| CoworkerId::from_stored(format!("cw_{name}_{suffix}"));
    let (hired, everything, nothing) = (bot("hired"), bot("all"), bot("none"));
    let before = ToolSet::only(["list_routines", "shell"]);
    for (bot, tools) in [
        (&hired, &before),
        (&everything, &ToolSet::All),
        (&nothing, &ToolSet::None),
    ] {
        let granted = store.grant_access(&account, bot, tools, tools, &ToolSet::None, 1);
        granted.await.expect("grant");
    }

    boot(&store, true).await;
    let gained = ToolSet::only(
        ["list_routines", "shell"]
            .into_iter()
            .chain(opengrok_tools::plugin_desk::TOOLS),
    );
    assert_eq!(
        policy(&store, &account, &hired).await,
        (gained.clone(), gained.clone())
    );
    let all = (ToolSet::All, ToolSet::All);
    assert_eq!(policy(&store, &account, &everything).await, all);
    let none = (ToolSet::None, ToolSet::None);
    assert_eq!(policy(&store, &account, &nothing).await, none);

    // The owner switches them off, as the ceiling route does, and a later boot leaves it.
    let written = store.set_ceiling(&account, &hired, &before, None, 2).await;
    assert!(written.expect("switch them off").is_some());
    boot(&store, false).await;
    assert_eq!(
        policy(&store, &account, &hired).await,
        (before.clone(), before)
    );
}
