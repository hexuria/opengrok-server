//! #316: a Bot's four routine tools are one ceiling row, on for a new hire, and every Bot hired
//! before it gains them ONCE, in its ceiling and in its owner's profile, which a turn's policy
//! intersects with it. After that pass a ceiling without them is one its owner switched off, and
//! no later boot may switch it back on. A ceiling that admits nothing is not handed them. A Bot
//! from before #314 too gains `message_bot` and the routines on the same boot, each pass under
//! its own marker, so neither pass's record stands in for the other's. `run_routine` (#337) is the
//! row's fifth tool: a Bot whose row is on gains it ONCE, on its own pass, and one whose row its
//! owner switched off does not.
//!
//! Its own test binary, so its own database: it forgets the pass and boots again, which touches
//! every ceiling in the database, and nothing else may be running beside it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_policy::ToolSet;
use opengrok_store::PgStore;
use sqlx::Row as _;

/// What a Bot was hired with before #314 and #316: every built-in and the person's machine.
const BEFORE: [&str; 8] = [
    "computer",
    "open_url",
    "read_file",
    "request_user_form",
    "run_recipe",
    "shell",
    "user_machine_shell",
    "write_file",
];

const ROUTINES: [&str; 4] = [
    "create_routine",
    "delete_routine",
    "list_routines",
    "update_routine",
];

/// The one-time passes this file boots through: #314's, #316's, then #337's.
const BOTS: &str = "the-bots-join-the-ceiling";
const PASS: &str = "the-routines-join-the-ceiling";
const RUN: &str = "the-routine-runs-join-the-ceiling";

/// Boot again, these passes forgotten first.
async fn boot(store: &PgStore, forget: &[&str]) {
    for pass in forget {
        sqlx::query("delete from schema_migrations where name = $1")
            .bind(pass)
            .execute(store.pool())
            .await
            .expect("forget the pass");
    }
    opengrok_store::migrations::run(store.pool())
        .await
        .expect("boot");
}

async fn recorded(store: &PgStore, pass: &str) -> bool {
    sqlx::query_scalar("select exists (select 1 from schema_migrations where name = $1)")
        .bind(pass)
        .fetch_one(store.pool())
        .await
        .expect("schema_migrations")
}

/// The ceiling as stored, so the order of its names is checked too, and its version.
async fn stored(store: &PgStore, coworker: &CoworkerId) -> (serde_json::Value, i64) {
    let row = sqlx::query("select tools, version from ceiling_view where coworker_id = $1")
        .bind(coworker.as_str())
        .fetch_one(store.pool())
        .await
        .expect("ceiling");
    (row.get("tools"), row.get("version"))
}

async fn profile(store: &PgStore, account: &AccountId, coworker: &CoworkerId) -> ToolSet {
    let policy = store.policy_for(account, coworker).await.expect("policy");
    policy.grant.expect("a grant").profile
}

/// A list with the Routines row on, as the passes leave it: the four, and `run_routine` (#337).
fn with_routines(names: &[&str]) -> ToolSet {
    ToolSet::only(names.iter().copied().chain(ROUTINES).chain(["run_routine"]))
}

/// A ceiling from before both passes, after them: #314's row and #316's.
fn with_both(names: &[&str]) -> ToolSet {
    with_routines(&[names, &["message_bot"]].concat())
}

fn json(tools: &ToolSet) -> serde_json::Value {
    serde_json::to_value(tools).expect("serialise")
}

#[tokio::test]
async fn every_bot_from_before_gains_the_routines_once_and_one_switched_off_stays_off() {
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
    let account = AccountId::from_stored(format!("acct_routines_{suffix}"));
    let coworker = |name: &str| CoworkerId::from_stored(format!("cw_{name}_{suffix}"));
    let (hired, chosen, everything, nothing) = (
        coworker("hired"),
        coworker("chosen"),
        coworker("all"),
        coworker("none"),
    );
    for (bot, tools) in [
        (&hired, ToolSet::only(BEFORE)),
        (&chosen, ToolSet::only(["shell"])),
        (&everything, ToolSet::All),
        (&nothing, ToolSet::None),
    ] {
        let granted = store.grant_access(&account, bot, &tools, &tools, &ToolSet::None, 1);
        granted.await.expect("grant");
    }
    let (_, version) = stored(&store, &hired).await;

    boot(&store, &[BOTS, PASS, RUN]).await;
    for pass in [BOTS, PASS, RUN] {
        assert!(recorded(&store, pass).await, "{pass}");
    }

    // Every list gains them, chosen or not, ceiling and profile alike, sorted as the store writes
    // a set; the ceiling's version moves once for each pass, so a screen that read it before is
    // refused. The Bots' row is the ceiling's alone, as #314 wrote it: it needed no profile.
    let (tools, moved) = stored(&store, &hired).await;
    assert_eq!(tools, json(&with_both(&BEFORE)));
    assert_eq!(moved, version + 3);
    assert_eq!(
        profile(&store, &account, &hired).await,
        with_routines(&BEFORE)
    );
    assert_eq!(
        stored(&store, &chosen).await.0,
        json(&with_both(&["shell"]))
    );
    assert_eq!(
        profile(&store, &account, &chosen).await,
        with_routines(&["shell"])
    );
    // `All` admits them already, and a Bot made to admit nothing is not handed them.
    assert_eq!(stored(&store, &everything).await.0, json(&ToolSet::All));
    assert_eq!(stored(&store, &nothing).await.0, json(&ToolSet::None));
    assert_eq!(profile(&store, &account, &nothing).await, ToolSet::None);

    // The next boot changes nothing; nor does the pass run again on rows that have them all.
    boot(&store, &[]).await;
    let both = (json(&with_both(&BEFORE)), moved);
    assert_eq!(stored(&store, &hired).await, both);
    for pass in [PASS, RUN] {
        boot(&store, &[pass]).await;
        assert_eq!(stored(&store, &hired).await, both, "{pass}");
    }

    // A Bot from #316's time, its row on with the four, gains `run_routine` on #337's pass alone,
    // in its ceiling and its owner's profile.
    let sixteen = coworker("sixteen");
    let four = ToolSet::only(BEFORE.into_iter().chain(ROUTINES));
    let granted = store.grant_access(&account, &sixteen, &four, &four, &ToolSet::None, 3);
    granted.await.expect("grant");
    let (_, before_run) = stored(&store, &sixteen).await;
    boot(&store, &[RUN]).await;
    let ran = (json(&with_routines(&BEFORE)), before_run + 1);
    assert_eq!(stored(&store, &sixteen).await, ran);
    let profiled = profile(&store, &account, &sixteen).await;
    assert_eq!(profiled, with_routines(&BEFORE));

    // The owner switches them off, as the ceiling route does, and every later boot leaves it.
    let off = ToolSet::only(BEFORE.into_iter().chain(["message_bot"]));
    let written = store.set_ceiling(&account, &hired, &off, None, 2).await;
    assert!(written.expect("switch them off").is_some());
    boot(&store, &[RUN]).await;
    assert_eq!(stored(&store, &hired).await.0, json(&off));
    assert_eq!(profile(&store, &account, &hired).await, off);
}
