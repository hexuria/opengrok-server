//! Auto-review tiers, from the store through resolution: rows written per scope come back
//! resolved per FIELD with the right tier named as the decider, a deleted row restores
//! inheritance, and a row of a scope kind that no longer exists is ignored rather than resolved.
//! Needs Postgres; skips loudly without it.

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
    let coworker = format!("cw_{}", uuid::Uuid::now_v7().simple());
    let other_coworker = format!("cw_{}", uuid::Uuid::now_v7().simple());

    // Nothing written ⇒ off, empty, everything decided by the default, inactive.
    let effective = auto_review::load_effective(&store, &account, Some(&coworker)).await;
    assert!(!effective.enabled);
    assert!(!effective.is_active());
    assert_eq!(effective.decided_by.enabled, DecidedBy::Default);

    // Global: on, with a block rule. Coworker: switched off, adds an allow rule, inherits block.
    store
        .set_auto_review_policy(&account, "global", "", Some(true), None, Some("never touch prod"), 1)
        .await
        .expect("global");
    store
        .set_auto_review_policy(&account, "coworker", &coworker, Some(false), Some("git is fine"), None, 2)
        .await
        .expect("coworker");

    let effective = auto_review::load_effective(&store, &account, Some(&coworker)).await;
    assert!(!effective.enabled, "the coworker's off beats global's on");
    assert_eq!(effective.decided_by.enabled, DecidedBy::Coworker);
    assert_eq!(effective.allow_instructions, "git is fine");
    assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Coworker);
    assert_eq!(effective.block_instructions, "never touch prod");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Global);
    assert!(!effective.is_active(), "off ⇒ short-circuit, whatever the rules say");

    // Another coworker has no row of its own: on, from global, and active.
    let effective = auto_review::load_effective(&store, &account, Some(&other_coworker)).await;
    assert!(effective.enabled);
    assert_eq!(effective.decided_by.enabled, DecidedBy::Global);
    assert!(effective.is_active());
    assert_eq!(effective.allow_instructions, "");
    assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Default);

    // Re-PUT the coworker row with an explicit '' block: it must come back as '' from the
    // coworker tier, NOT collapse into "inherit" and let global's block leak back in.
    store
        .set_auto_review_policy(&account, "coworker", &coworker, Some(true), None, Some(""), 3)
        .await
        .expect("coworker again");
    let effective = auto_review::load_effective(&store, &account, Some(&coworker)).await;
    assert_eq!(effective.block_instructions, "");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Coworker);

    // The settings view sees both rows, keyed by scope.
    let rows = store.auto_review_rows(&account).await.expect("rows");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.scope_kind == "coworker" && row.scope_id == coworker));

    // Delete the coworker row ⇒ full inheritance again.
    store
        .delete_auto_review_policy(&account, "coworker", &coworker)
        .await
        .expect("delete");
    let effective = auto_review::load_effective(&store, &account, Some(&coworker)).await;
    assert_eq!(effective.block_instructions, "never touch prod");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Global);
    assert_eq!(store.auto_review_rows(&account).await.expect("rows").len(), 1);
}

#[tokio::test]
async fn a_legacy_machine_row_is_never_resolved() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let coworker = format!("cw_{}", uuid::Uuid::now_v7().simple());

    // The store API is generic over scope_kind on purpose (the column is just text); the SERVER
    // is what refuses "machine". A row that got in anyway — an old client, a hand edit — must be
    // invisible to resolution, not a hidden third tier.
    store
        .set_auto_review_policy(&account, "machine", "mac_ghost", Some(true), None, Some("haunt"), 1)
        .await
        .expect("legacy row");
    let effective = auto_review::load_effective(&store, &account, Some(&coworker)).await;
    assert!(!effective.enabled);
    assert_eq!(effective.block_instructions, "");
    assert_eq!(effective.decided_by.enabled, DecidedBy::Default);
    let tiers = store
        .auto_review_tiers(&account, Some(&coworker))
        .await
        .expect("tiers");
    assert!(tiers.is_empty(), "the tier query must not return foreign scope kinds");
}
