//! Booting against a schema that is already there.
//!
//! Every server and every test harness runs `migrations::run` on boot, and the schema is one
//! transaction. Replayed in full, its bare `alter table … add column if not exists` lines took
//! ACCESS EXCLUSIVE on `coworker_view` and friends and held them to the end, so any read that
//! touched those tables in the other order deadlocked against a boot: a recipe's history prune
//! (`recipe_run` then `coworker_view`) was killed and left six runs where five belong.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn a_boot_over_the_current_schema_takes_no_table_locks() {
    let url = match std::env::var("OG_DATABASE_URL") {
        Ok(url) => opengrok_store::gate_database_or_panic(url),
        Err(_) => {
            eprintln!("skipping: OG_DATABASE_URL is not set");
            return;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("first boot");

    // A reader in the middle of a transaction over `coworker_view`, as a prune or a policy read
    // is. ACCESS SHARE conflicts with nothing but ACCESS EXCLUSIVE, so a boot that waits on it
    // is a boot that tried to alter the table.
    let mut reader = pool.begin().await.expect("begin");
    sqlx::query("lock table coworker_view, recipe_run in access share mode")
        .execute(&mut *reader)
        .await
        .expect("hold the reader's locks");

    let second = tokio::time::timeout(
        Duration::from_secs(5),
        opengrok_store::migrations::run(&pool),
    )
    .await;
    reader.rollback().await.expect("rollback");
    match second {
        Ok(done) => done.expect("second boot"),
        Err(_) => panic!("a boot over the current schema waited on a reader's table lock"),
    }
}
