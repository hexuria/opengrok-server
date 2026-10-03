//! The one connection a process listens on, killed: it is replaced, and what is written after is
//! heard through a wake. The poll is set far too long to be what finds it.
//!
//! Its own test binary, so its own database: it kills every listener on the database it runs in,
//! and another test's would be among them.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use opengrok_events::emit;
use opengrok_wire::events::Note;
use sqlx::PgPool;

use support::{account, hub, must, pool};

async fn changed(pool: &PgPool, account: &str, thread: &str) {
    let note = Note::ThreadChanged {
        thread_id: thread,
        coworker_id: "cw_1",
        run_id: None,
    };
    let mut tx = pool.begin().await.unwrap();
    emit(&mut tx, account, &[note]).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn the_listener_comes_back_after_its_connection_is_killed() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    let mut frames = hub.follow(&ada, None).await.unwrap();
    must(&mut frames).await;
    changed(&pool, &ada, "before").await;
    assert_eq!(must(&mut frames).await.id, 1, "listening");

    let kill = "select pg_terminate_backend(pid) from pg_stat_activity
                where datname = current_database() and query like 'LISTEN %opengrok_events%'";
    let killed = sqlx::query(kill).fetch_all(&pool).await.unwrap().len();
    assert_eq!(killed, 1, "the one listener");

    // It connects again after a pause, and wakes every stream once it has.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    changed(&pool, &ada, "after").await;
    assert_eq!(must(&mut frames).await.id, 2);
}
