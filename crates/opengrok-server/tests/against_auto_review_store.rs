//! Auto-review tiers, from the store through resolution: rows written per scope come back
//! resolved per FIELD with the right tier named as the decider, and a deleted row restores
//! inheritance. Needs Postgres; skips loudly without it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use opengrok_server::auto_review::{self, DecidedBy};
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
async fn tiers_round_trip_and_resolve_per_field() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    // Unique ids so parallel runs don't collide.
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let machine = format!("mac_{}", uuid::Uuid::now_v7().simple());
    let coworker = format!("cw_{}", uuid::Uuid::now_v7().simple());
    let other_coworker = format!("cw_{}", uuid::Uuid::now_v7().simple());

    // Nothing written ⇒ off, empty, everything decided by the default, inactive.
    let effective =
        auto_review::load_effective(&store, &account, Some(&machine), Some(&coworker)).await;
    assert!(!effective.enabled);
    assert!(!effective.is_active());
    assert_eq!(effective.decided_by.enabled, DecidedBy::Default);

    // Global: on, with a block rule. Machine: inherits enabled, adds an allow rule.
    // Coworker: switched off, inherits everything else.
    store
        .set_auto_review_policy(
            &account,
            "global",
            "",
            Some(true),
            None,
            Some("never touch prod"),
            1,
        )
        .await
        .expect("global");
    store
        .set_auto_review_policy(
            &account,
            "machine",
            &machine,
            None,
            Some("git is fine"),
            None,
            2,
        )
        .await
        .expect("machine");
    store
        .set_auto_review_policy(&account, "coworker", &coworker, Some(false), None, None, 3)
        .await
        .expect("coworker");

    let effective =
        auto_review::load_effective(&store, &account, Some(&machine), Some(&coworker)).await;
    assert!(!effective.enabled, "the coworker's off beats global's on");
    assert_eq!(effective.decided_by.enabled, DecidedBy::Coworker);
    assert_eq!(effective.allow_instructions, "git is fine");
    assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Machine);
    assert_eq!(effective.block_instructions, "never touch prod");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Global);
    assert!(
        !effective.is_active(),
        "off ⇒ short-circuit, whatever the rules say"
    );

    // Another coworker on the same machine has no row of its own: on, from global.
    let effective =
        auto_review::load_effective(&store, &account, Some(&machine), Some(&other_coworker)).await;
    assert!(effective.enabled);
    assert_eq!(effective.decided_by.enabled, DecidedBy::Global);
    assert!(effective.is_active());
    assert_eq!(effective.allow_instructions, "git is fine");

    // Re-PUT the machine row with an explicit '' allow: it must come back as '' from the machine
    // tier, NOT collapse into "inherit".
    store
        .set_auto_review_policy(&account, "machine", &machine, None, Some(""), None, 4)
        .await
        .expect("machine again");
    let effective =
        auto_review::load_effective(&store, &account, Some(&machine), Some(&other_coworker)).await;
    assert_eq!(effective.allow_instructions, "");
    assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Machine);

    // The settings view sees all three rows, keyed by scope.
    let rows = store.auto_review_rows(&account).await.expect("rows");
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .any(|row| row.scope_kind == "machine" && row.scope_id == machine)
    );

    // Delete the machine row ⇒ its scope inherits fully again (allow falls to the default).
    store
        .delete_auto_review_policy(&account, "machine", &machine)
        .await
        .expect("delete");
    let effective =
        auto_review::load_effective(&store, &account, Some(&machine), Some(&other_coworker)).await;
    assert_eq!(effective.allow_instructions, "");
    assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Default);
    assert_eq!(
        store.auto_review_rows(&account).await.expect("rows").len(),
        2
    );

    // A machine we never wrote for matches nothing but global.
    let effective = auto_review::load_effective(&store, &account, Some("mac_nobody"), None).await;
    assert_eq!(effective.decided_by.enabled, DecidedBy::Global);
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Global);
}
