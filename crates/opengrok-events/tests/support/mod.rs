//! What the outbox's tests share: a database, an account of their own, and a block read back.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use opengrok_events::{Hub, Tuning};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// A pool on this binary's own database with the outbox's tables in it, or `None` (said loudly)
/// without `OG_DATABASE_URL`.
pub async fn pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return None;
    };
    let url = opengrok_testdb::gate_database_or_panic(url);
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .acquire_timeout(Duration::from_secs(20))
        .connect(&url)
        .await
        .expect("connect");
    // Tests in one binary run at once, and two `create table if not exists` can race each other
    // into a duplicate-key error in the catalog: one at a time, as the store's boot does.
    let mut lock = pool.acquire().await.expect("a connection");
    sqlx::query("select pg_advisory_lock(7402)")
        .execute(&mut *lock)
        .await
        .expect("lock");
    sqlx::raw_sql(opengrok_events::SCHEMA)
        .execute(&mut *lock)
        .await
        .expect("the outbox's tables");
    sqlx::query("select pg_advisory_unlock(7402)")
        .execute(&mut *lock)
        .await
        .expect("unlock");
    drop(lock);
    Some(pool)
}

/// An account no other test has, so tests that share a database do not share notes.
pub fn account() -> String {
    format!("acct_{}", uuid::Uuid::now_v7().simple())
}

/// A hub whose clocks are short enough for a test to wait on.
pub fn hub(pool: &PgPool) -> Hub {
    let hub = Hub::new(pool.clone());
    hub.tune(Tuning {
        ping: Duration::from_secs(60),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(10),
        room: 64,
    });
    hub
}

/// One block of the stream, taken apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub id: i64,
    pub event: String,
    pub data: serde_json::Value,
}

/// Parse a block the stream yielded: `id:`, `event:`, `data:`, and nothing else.
pub fn parse(text: &str) -> Block {
    assert!(
        text.ends_with("\n\n"),
        "a block ends in a blank line: {text:?}"
    );
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    assert_eq!(lines.len(), 3, "id, event and one line of data: {text:?}");
    let field = |at: usize, name: &str| {
        lines[at]
            .strip_prefix(&format!("{name}: "))
            .unwrap_or_else(|| panic!("line {at} of {text:?} is not {name}"))
            .to_string()
    };
    Block {
        id: field(0, "id").parse().expect("a number"),
        event: field(1, "event"),
        data: serde_json::from_str(&field(2, "data")).expect("JSON"),
    }
}

pub type Frames = BoxStream<'static, String>;

/// The next block within `ms`, or `None` if nothing came: a comment (a ping) is a panic here.
pub async fn block(frames: &mut Frames, ms: u64) -> Option<Block> {
    let next = tokio::time::timeout(Duration::from_millis(ms), frames.next()).await;
    next.ok()
        .map(|text| parse(&text.expect("the stream ended")))
}

/// The next block, which must come.
pub async fn must(frames: &mut Frames) -> Block {
    block(frames, 5_000).await.expect("a block within 5 s")
}

/// The next frame of any kind within `ms`: a block, or a comment.
pub async fn frame(frames: &mut Frames, ms: u64) -> Option<String> {
    let next = tokio::time::timeout(Duration::from_millis(ms), frames.next()).await;
    next.ok().map(|text| text.expect("the stream ended"))
}
