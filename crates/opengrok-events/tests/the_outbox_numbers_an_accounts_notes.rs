//! The outbox: how an account's notes are numbered, that they live and die with the transaction
//! they are written in, and what is kept.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use opengrok_events::{CHANNEL, RETAIN_HOURS, RETAIN_MAX, emit};
use opengrok_wire::events::Note;
use sqlx::postgres::PgListener;

use support::{account, pool};

fn changed<'a>(thread: &'a str) -> Note<'a> {
    Note::ThreadChanged {
        thread_id: thread,
        coworker_id: "cw_1",
        run_id: None,
    }
}

async fn ids(pool: &sqlx::PgPool, account: &str) -> Vec<i64> {
    let select = "select id from account_event where account_id = $1 order by id";
    sqlx::query_scalar(select)
        .bind(account)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn head(pool: &sqlx::PgPool, account: &str) -> Option<(i64, i64)> {
    let select = "select head, floor from account_event_head where account_id = $1";
    sqlx::query_as(select)
        .bind(account)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// Each account counts from one, on its own, and a call that writes several numbers them in order.
#[tokio::test]
async fn an_account_numbers_its_own_notes_from_one() {
    let Some(pool) = pool().await else { return };
    let (ada, bob) = (account(), account());
    let mut conn = pool.acquire().await.unwrap();
    emit(&mut conn, &ada, &[changed("a"), changed("b")])
        .await
        .unwrap();
    emit(&mut conn, &bob, &[changed("c")]).await.unwrap();
    emit(&mut conn, &ada, &[changed("d")]).await.unwrap();
    drop(conn);

    assert_eq!(ids(&pool, &ada).await, [1, 2, 3]);
    assert_eq!(ids(&pool, &bob).await, [1]);
    assert_eq!(head(&pool, &ada).await, Some((3, 0)));
    assert_eq!(head(&pool, &bob).await, Some((1, 0)));
}

/// What the note holds is what the wire says, kind and data, and nothing else.
#[tokio::test]
async fn a_note_is_stored_as_the_wire_says_it() {
    let Some(pool) = pool().await else { return };
    let ada = account();
    let note = Note::RunStarted {
        run_id: "run_1",
        thread_id: "sched_1",
        coworker_id: "cw_1",
        routine_id: Some("sched_1"),
        cause: "clock",
    };
    let mut conn = pool.acquire().await.unwrap();
    emit(&mut conn, &ada, &[note]).await.unwrap();
    drop(conn);

    let select = "select kind, payload from account_event where account_id = $1";
    let (kind, payload): (String, serde_json::Value) = sqlx::query_as(select)
        .bind(&ada)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind, "run.started");
    assert_eq!(
        payload,
        serde_json::json!({ "runId": "run_1", "threadId": "sched_1", "coworkerId": "cw_1",
                            "routineId": "sched_1", "cause": "clock" })
    );
}

/// WRITERS OF ONE ACCOUNT COMMIT IN THE ORDER THEY WERE NUMBERED. Six writers each take an id,
/// hold their transaction a moment, and commit; a reader that follows by "id above my cursor" must
/// find every id exactly once and in order, never one that appears after a later one was seen. A
/// sequence fails this: it hands out numbers as transactions ask, and they commit in another order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_never_sees_an_id_before_the_ones_below_it() {
    let Some(pool) = pool().await else { return };
    let ada = account();
    let (writers, each) = (6_i64, 20_i64);
    let mut tasks = Vec::new();
    for writer in 0..writers {
        let (pool, ada) = (pool.clone(), ada.clone());
        tasks.push(tokio::spawn(async move {
            for n in 0..each {
                let mut tx = pool.begin().await.unwrap();
                let thread = format!("t_{writer}_{n}");
                emit(&mut tx, &ada, &[changed(&thread)]).await.unwrap();
                // The id is taken; the commit is a little later, and differs by writer.
                let pause = ((writer * 7 + n * 3) % 5) as u64;
                tokio::time::sleep(Duration::from_millis(pause)).await;
                tx.commit().await.unwrap();
            }
        }));
    }
    let reader = {
        let (pool, ada) = (pool.clone(), ada.clone());
        tokio::spawn(async move {
            let mut cursor = 0_i64;
            while cursor < writers * each {
                let select = "select id from account_event
                              where account_id = $1 and id > $2 order by id limit 50";
                let seen: Vec<i64> = sqlx::query_scalar(select)
                    .bind(&ada)
                    .bind(cursor)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                for id in seen {
                    assert_eq!(id, cursor + 1, "an id appeared after one above it was seen");
                    cursor = id;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    for task in tasks {
        task.await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(20), reader)
        .await
        .expect("the reader finished")
        .unwrap();
    assert_eq!(
        ids(&pool, &ada).await,
        (1..=writers * each).collect::<Vec<_>>()
    );
}

/// NO NOTE WITHOUT A COMMIT. A transaction that rolls back leaves no note, takes no id, and wakes
/// nobody; the same transaction committed leaves its note and wakes the account's followers.
#[tokio::test]
async fn a_note_lives_and_dies_with_its_transaction() {
    let Some(pool) = pool().await else { return };
    let ada = account();
    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen(CHANNEL).await.unwrap();
    // Of the wakes on the channel, the ones that name this account: other tests share it.
    async fn woken(listener: &mut PgListener, account: &str, ms: u64) -> Option<String> {
        let until = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            let heard = tokio::time::timeout(left, listener.recv()).await.ok()?;
            let payload = heard.unwrap().payload().to_string();
            if payload.starts_with(account) {
                return Some(payload);
            }
        }
    }

    let mut tx = pool.begin().await.unwrap();
    emit(&mut tx, &ada, &[changed("gone")]).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(ids(&pool, &ada).await, Vec::<i64>::new());
    assert_eq!(head(&pool, &ada).await, None, "the id was not kept");
    assert_eq!(woken(&mut listener, &ada, 400).await, None);

    let mut tx = pool.begin().await.unwrap();
    emit(&mut tx, &ada, &[changed("kept")]).await.unwrap();
    assert_eq!(
        woken(&mut listener, &ada, 200).await,
        None,
        "not before the commit"
    );
    tx.commit().await.unwrap();
    assert_eq!(ids(&pool, &ada).await, [1]);
    assert_eq!(
        woken(&mut listener, &ada, 2_000).await,
        Some(format!("{ada}:1"))
    );
}

/// A head row deleted by hand is not an outage. A duplicate key would read as `Conflict` to the
/// store, and a run's first batch would take it for a lost race and never run.
#[tokio::test]
async fn a_head_that_is_behind_its_notes_is_mended_and_not_a_duplicate_key() {
    let Some(pool) = pool().await else { return };
    let ada = account();
    let mut conn = pool.acquire().await.unwrap();
    emit(&mut conn, &ada, &[changed("a"), changed("b"), changed("c")])
        .await
        .unwrap();
    sqlx::query("delete from account_event_head where account_id = $1")
        .bind(&ada)
        .execute(&mut *conn)
        .await
        .unwrap();
    emit(&mut conn, &ada, &[changed("d")]).await.unwrap();
    drop(conn);
    assert_eq!(ids(&pool, &ada).await, [1, 2, 3, 4]);
    assert_eq!(head(&pool, &ada).await, Some((4, 0)));
}

/// AT MOST `RETAIN_MAX` NOTES, pruned as they are written: the oldest go, the head stays where it
/// is, and `floor` says where the gap is, so an id below it is refused rather than half-replayed.
#[tokio::test]
async fn an_account_keeps_its_newest_ten_thousand_notes() {
    let Some(pool) = pool().await else { return };
    let ada = account();
    let extra = 50;
    let threads: Vec<String> = (0..RETAIN_MAX + extra).map(|n| format!("t{n}")).collect();
    let notes: Vec<Note> = threads.iter().map(|thread| changed(thread)).collect();
    let mut conn = pool.acquire().await.unwrap();
    emit(&mut conn, &ada, &notes).await.unwrap();
    drop(conn);

    let kept = ids(&pool, &ada).await;
    assert_eq!(kept.len() as i64, RETAIN_MAX);
    assert_eq!(kept.first(), Some(&(extra + 1)));
    assert_eq!(kept.last(), Some(&(RETAIN_MAX + extra)));
    assert_eq!(head(&pool, &ada).await, Some((RETAIN_MAX + extra, extra)));
}

/// AND NONE OLDER THAN `RETAIN_HOURS`: a write prunes its account's expired notes, and the floor
/// moves to the newest it removed. Ids already handed out are never handed out again.
#[tokio::test]
async fn a_note_older_than_a_day_is_pruned_by_the_next_write() {
    let Some(pool) = pool().await else { return };
    let ada = account();
    let mut conn = pool.acquire().await.unwrap();
    emit(&mut conn, &ada, &[changed("a"), changed("b"), changed("c")])
        .await
        .unwrap();
    let age = "update account_event set created_at = now() - make_interval(hours => $2)
               where account_id = $1 and id <= 2";
    sqlx::query(age)
        .bind(&ada)
        .bind(RETAIN_HOURS + 1)
        .execute(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        ids(&pool, &ada).await,
        [1, 2, 3],
        "nothing prunes until a write"
    );

    emit(&mut conn, &ada, &[changed("d")]).await.unwrap();
    drop(conn);
    assert_eq!(ids(&pool, &ada).await, [3, 4]);
    assert_eq!(head(&pool, &ada).await, Some((4, 2)));
}
