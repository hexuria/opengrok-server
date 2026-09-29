//! #268: a coworker's ceiling now decides whether it may reach its person's machine, and every
//! ceiling written before that allowed the machine in effect — so the schema adds it, ONCE. After
//! that pass a ceiling without it is one its owner switched off, and a later replay of the schema
//! (any change to it replays it whole) must not switch it back on.
//!
//! Its own test binary, so its own database: it makes the schema replay, which takes the table
//! locks a replay takes, and nothing else may be running beside it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_policy::ToolSet;
use opengrok_store::PgStore;
use sqlx::Row as _;

/// The built-ins a coworker was hired with before the machine had a switch.
const BEFORE: [&str; 7] = [
    "computer",
    "open_url",
    "read_file",
    "request_user_form",
    "run_recipe",
    "shell",
    "write_file",
];

/// Replay the schema as the next change to it will, with or without the pass already recorded.
async fn replay(store: &PgStore, forget_the_pass: bool) {
    if forget_the_pass {
        sqlx::query("delete from schema_migrations where name = 'the-machine-joins-the-ceiling'")
            .execute(store.pool())
            .await
            .expect("forget the pass");
    }
    sqlx::query("delete from schema_applied")
        .execute(store.pool())
        .await
        .expect("forget the schema");
    opengrok_store::migrations::run(store.pool())
        .await
        .expect("replay");
}

async fn grant(store: &PgStore, account: &AccountId, coworker: &CoworkerId, tools: &ToolSet) {
    store
        .grant_access(account, coworker, tools, tools, &ToolSet::None, 1)
        .await
        .expect("grant");
}

/// The row as stored, so the order of its names is checked too.
async fn stored(store: &PgStore, coworker: &CoworkerId) -> serde_json::Value {
    sqlx::query("select tools from ceiling_view where coworker_id = $1")
        .bind(coworker.as_str())
        .fetch_one(store.pool())
        .await
        .expect("ceiling")
        .get("tools")
}

fn with_the_machine(names: &[&str]) -> serde_json::Value {
    let tools = ToolSet::only(names.iter().copied().chain(["user_machine_shell"]));
    serde_json::to_value(tools).expect("serialise")
}

#[tokio::test]
async fn every_ceiling_from_before_gains_the_machine_once_and_one_switched_off_stays_off() {
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
    let account = AccountId::from_stored(format!("acct_machine_{suffix}"));
    let coworker = |name: &str| CoworkerId::from_stored(format!("cw_{name}_{suffix}"));
    let (hired, chosen, everything, nothing) = (
        coworker("hired"),
        coworker("chosen"),
        coworker("all"),
        coworker("none"),
    );
    grant(&store, &account, &hired, &ToolSet::only(BEFORE)).await;
    grant(&store, &account, &chosen, &ToolSet::only(["shell"])).await;
    grant(&store, &account, &everything, &ToolSet::All).await;
    grant(&store, &account, &nothing, &ToolSet::None).await;

    replay(&store, true).await;

    // Every list gains it, chosen or not: none of them could say no to the machine before. Stored
    // in order, as the store writes a set, so a later statement matching an exact list still can.
    assert_eq!(stored(&store, &hired).await, with_the_machine(&BEFORE));
    assert_eq!(stored(&store, &chosen).await, with_the_machine(&["shell"]));
    // `All` admits it already, and a coworker made to admit nothing is not handed a machine.
    let all = serde_json::to_value(ToolSet::All).expect("all");
    assert_eq!(stored(&store, &everything).await, all);
    let none = serde_json::to_value(ToolSet::None).expect("none");
    assert_eq!(stored(&store, &nothing).await, none);

    // The owner switches it off, as the ceiling route does. The next change to the schema replays
    // it, and every boot runs the built-in widenings, and it stays off.
    let off = ToolSet::only(BEFORE);
    let written = store.set_ceiling(&account, &hired, &off, None, 2).await;
    assert!(written.expect("switch it off").is_some());
    replay(&store, false).await;
    assert_eq!(
        stored(&store, &hired).await,
        serde_json::to_value(&off).expect("off")
    );
    let policy = store.policy_for(&account, &hired).await.expect("policy");
    assert_eq!(policy.ceiling.map(|ceiling| ceiling.tools), Some(off));
}
