//! One account's stream: what a connection is told first, what it is replayed, what it hears live,
//! and what it is never sent.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use opengrok_events::{Tuning, emit};
use opengrok_wire::events::Note;
use serde_json::json;
use sqlx::PgPool;

use support::{account, block, frame, hub, must, pool};

fn changed(thread: &str) -> Note<'_> {
    Note::ThreadChanged {
        thread_id: thread,
        coworker_id: "cw_1",
        run_id: None,
    }
}

fn by_run<'a>(thread: &'a str, run: &'a str) -> Note<'a> {
    Note::ThreadChanged {
        thread_id: thread,
        coworker_id: "cw_1",
        run_id: Some(run),
    }
}

fn started(run: &str) -> Note<'_> {
    Note::RunStarted {
        run_id: run,
        thread_id: "t",
        coworker_id: "cw_1",
        routine_id: None,
        cause: "chat",
    }
}

/// One note, in a transaction of its own, as a store write leaves it.
async fn note(pool: &PgPool, account: &str, note: Note<'_>) {
    let mut tx = pool.begin().await.unwrap();
    emit(&mut tx, account, &[note]).await.unwrap();
    tx.commit().await.unwrap();
}

/// WHAT A FRESH CONNECTION IS TOLD FIRST is that it must read everything: `reset`, carrying the
/// head as its id; and then it hears what is written after, each under the id it was given.
#[tokio::test]
async fn a_new_connection_is_told_to_reset_and_then_hears_what_follows() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    let mut frames = hub.follow(&ada, None).await.unwrap();

    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (0, "reset"));
    assert_eq!(first.data, json!({}));

    note(&pool, &ada, changed("t1")).await;
    let heard = must(&mut frames).await;
    assert_eq!((heard.id, heard.event.as_str()), (1, "thread.changed"));
    assert_eq!(
        heard.data,
        json!({ "threadId": "t1", "coworkerId": "cw_1", "runId": null })
    );
    assert_eq!(block(&mut frames, 300).await, None, "and nothing more");
}

/// A connection with notes behind it is told the head, not zero: the id it will resume from.
#[tokio::test]
async fn the_reset_carries_the_head() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    for n in 0..3 {
        note(&pool, &ada, changed(&format!("t{n}"))).await;
    }
    let mut frames = hub.follow(&ada, None).await.unwrap();
    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (3, "reset"));
}

/// REPLAY EVERYTHING AFTER N, IN ORDER, THEN FOLLOW: no gap at the join and no block twice.
#[tokio::test]
async fn a_known_id_is_replayed_in_order_and_then_followed() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    note(&pool, &ada, started("run_1")).await;
    note(&pool, &ada, changed("t1")).await;
    note(&pool, &ada, changed("t2")).await;
    note(&pool, &ada, started("run_2")).await;

    let mut frames = hub.follow(&ada, Some("1")).await.unwrap();
    let replayed: Vec<(i64, String)> = vec![
        must(&mut frames).await,
        must(&mut frames).await,
        must(&mut frames).await,
    ]
    .into_iter()
    .map(|b| (b.id, b.event))
    .collect();
    assert_eq!(
        replayed,
        [
            (2, "thread.changed".to_string()),
            (3, "thread.changed".to_string()),
            (4, "run.started".to_string())
        ]
    );

    note(&pool, &ada, changed("t3")).await;
    let live = must(&mut frames).await;
    assert_eq!(live.id, 5, "the next after the replay, none repeated");
    assert_eq!(block(&mut frames, 300).await, None);
}

/// The head itself is a valid id: there is nothing to replay and the next note is the next id.
#[tokio::test]
async fn the_head_resumes_with_nothing_to_replay() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    note(&pool, &ada, changed("t1")).await;
    let mut frames = hub.follow(&ada, Some("1")).await.unwrap();
    assert_eq!(
        block(&mut frames, 300).await,
        None,
        "no reset, nothing replayed"
    );
    note(&pool, &ada, changed("t2")).await;
    assert_eq!(must(&mut frames).await.id, 2);
}

/// AN ID THE SERVER CANNOT RESUME FROM IS A RESET FIRST, and then the stream follows from the head:
/// not a number, from the future (greater than the head), or older than what is kept.
#[tokio::test]
async fn an_id_that_cannot_be_resumed_from_is_a_reset_first() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    for n in 0..6 {
        note(&pool, &ada, changed(&format!("t{n}"))).await;
    }
    // 1 to 3 have aged out: the oldest id kept is 4, so 3 is the last a client can resume from.
    let age = "update account_event set created_at = now() - interval '25 hours'
               where account_id = $1 and id <= 3";
    sqlx::query(age).bind(&ada).execute(&pool).await.unwrap();

    for refused in ["abc", "", "-1", "7", "99", "2", "0", "2.5", " "] {
        let mut frames = hub.follow(&ada, Some(refused)).await.unwrap();
        let first = must(&mut frames).await;
        assert_eq!(
            (first.id, first.event.as_str()),
            (6, "reset"),
            "Last-Event-ID: {refused:?}"
        );
        note(&pool, &ada, changed("live")).await;
        let live = must(&mut frames).await;
        assert_eq!(live.event, "thread.changed", "followed on after the reset");
        sqlx::query("delete from account_event where account_id = $1 and id > 6")
            .bind(&ada)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("update account_event_head set head = 6 where account_id = $1")
            .bind(&ada)
            .execute(&pool)
            .await
            .unwrap();
    }

    // 3 is the edge: everything after it is kept and within the day.
    let mut frames = hub.follow(&ada, Some("3")).await.unwrap();
    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (4, "thread.changed"));
    assert_eq!(must(&mut frames).await.id, 5);
    assert_eq!(must(&mut frames).await.id, 6);
}

/// The same refusal when retention has pruned by count: the floor remembers what the rows do not.
#[tokio::test]
async fn an_id_below_the_floor_is_a_reset_even_when_its_rows_are_gone() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    let threads: Vec<String> = (0..10_020).map(|n| format!("t{n}")).collect();
    let notes: Vec<Note> = threads.iter().map(|t| changed(t)).collect();
    let mut conn = pool.acquire().await.unwrap();
    emit(&mut conn, &ada, &notes).await.unwrap();
    drop(conn);

    let mut frames = hub.follow(&ada, Some("19")).await.unwrap();
    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (10_020, "reset"));
    let mut frames = hub.follow(&ada, Some("20")).await.unwrap();
    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (21, "thread.changed"));
}

/// WHAT RETENTION PRUNES UNDER A STREAM IS NOT SILENTLY SKIPPED. A resume at 1 is valid when it
/// opens; the notes after it are pruned before it reads them (as a replay slower than ten thousand
/// notes would find); the page it then reads begins at 4, not 2, and the stream says `reset`
/// rather than hand over the rest as if nothing was missing.
#[tokio::test]
async fn notes_pruned_before_they_were_read_are_a_reset_and_not_a_silent_gap() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    for n in 0..5 {
        note(&pool, &ada, changed(&format!("t{n}"))).await;
    }
    let mut frames = hub.follow(&ada, Some("1")).await.unwrap();
    let prune = "delete from account_event where account_id = $1 and id in (2, 3)";
    sqlx::query(prune).bind(&ada).execute(&pool).await.unwrap();
    let floor = "update account_event_head set floor = 3 where account_id = $1";
    sqlx::query(floor).bind(&ada).execute(&pool).await.unwrap();

    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (5, "reset"));
    assert_eq!(
        block(&mut frames, 300).await,
        None,
        "and not the notes after the gap"
    );
    note(&pool, &ada, changed("after")).await;
    assert_eq!(must(&mut frames).await.id, 6);
}

/// A LONG REPLAY IS PAGES: all of it, in order, each once, and the live notes after.
#[tokio::test]
async fn a_long_replay_comes_whole_and_in_order() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    let threads: Vec<String> = (0..1_300).map(|n| format!("t{n}")).collect();
    let notes: Vec<Note> = threads.iter().map(|t| changed(t)).collect();
    let mut conn = pool.acquire().await.unwrap();
    emit(&mut conn, &ada, &notes).await.unwrap();
    drop(conn);

    let mut frames = hub.follow(&ada, Some("0")).await.unwrap();
    for want in 1..=1_300 {
        assert_eq!(must(&mut frames).await.id, want);
    }
    note(&pool, &ada, changed("live")).await;
    assert_eq!(must(&mut frames).await.id, 1_301);
}

/// ONE ACCOUNT'S NOTES NEVER REACH ANOTHER'S STREAM, live or replayed, and an account whose id
/// begins with another's is a different account.
#[tokio::test]
async fn one_accounts_notes_never_reach_anothers_stream() {
    let Some(pool) = pool().await else { return };
    let hub = hub(&pool);
    let ada = account();
    let (bob, ada_too) = (account(), format!("{ada}_too"));
    note(&pool, &ada, changed("ada-before")).await;
    note(&pool, &bob, changed("bob-before")).await;
    note(&pool, &ada_too, changed("too-before")).await;

    let mut for_ada = hub.follow(&ada, Some("0")).await.unwrap();
    let mut for_bob = hub.follow(&bob, Some("0")).await.unwrap();
    note(&pool, &ada, changed("ada-live")).await;
    note(&pool, &bob, changed("bob-live")).await;
    note(&pool, &ada_too, changed("too-live")).await;

    let mut heard = Vec::new();
    for _ in 0..2 {
        heard.push(
            must(&mut for_ada).await.data["threadId"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(heard, ["ada-before", "ada-live"]);
    assert_eq!(block(&mut for_ada, 400).await, None);
    let mut heard = Vec::new();
    for _ in 0..2 {
        heard.push(
            must(&mut for_bob).await.data["threadId"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(heard, ["bob-before", "bob-live"]);
    assert_eq!(block(&mut for_bob, 400).await, None);
}

/// AN ACCOUNT THAT CONNECTED ONCE LEAVES NOTHING BEHIND: its room is dropped with the last stream
/// in it, and kept while one is left, so a long-lived process does not keep an entry for everyone
/// who ever opened the app.
#[tokio::test]
async fn a_room_goes_with_the_last_stream_in_it() {
    let Some(pool) = pool().await else { return };
    let (hub, ada, bob) = (hub(&pool), account(), account());
    let first = hub.follow(&ada, None).await.unwrap();
    let second = hub.follow(&ada, None).await.unwrap();
    let others = hub.follow(&bob, None).await.unwrap();
    assert_eq!(hub.open_rooms(), 2);
    drop(first);
    assert_eq!(hub.open_rooms(), 2, "one is left in ada's");
    drop(second);
    assert_eq!(hub.open_rooms(), 1);
    drop(others);
    assert_eq!(hub.open_rooms(), 0);
}

/// A QUIET STREAM SAYS SO: a `: ping` comment, on the clock the hub is tuned to, again and again.
#[tokio::test]
async fn a_quiet_stream_pings() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    hub.tune(Tuning {
        ping: Duration::from_millis(80),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(10),
        room: 64,
    });
    let mut frames = hub.follow(&ada, None).await.unwrap();
    must(&mut frames).await;
    for _ in 0..3 {
        let said = frame(&mut frames, 2_000).await.expect("a ping");
        assert_eq!(said, ": ping\n\n");
    }
}

/// A BURST OF `thread.changed` FOR ONE THREAD LEAVES AS ONE, the last, so the app reads where the
/// thread has got to and not where each round left it. Threads stay apart, nothing else merges, and
/// what is told is in id order. The first is told at once; the burst waits out the window.
#[tokio::test]
async fn a_burst_for_one_thread_is_told_once_and_ends_on_the_last() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    hub.tune(Tuning {
        ping: Duration::from_secs(60),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(600),
        room: 64,
    });
    let mut frames = hub.follow(&ada, None).await.unwrap();
    must(&mut frames).await;

    note(&pool, &ada, changed("busy")).await;
    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (1, "thread.changed"));
    let told_at = std::time::Instant::now();

    // Ids 2 to 12, in one commit so they are one burst: busy at 2, 4, 6, 8, 10 and 12; the other
    // thread at 3 and 7; a run starting at 5, 9 and 11.
    let notes = [
        changed("busy"),
        changed("other"),
        changed("busy"),
        started("run_1"),
        changed("busy"),
        changed("other"),
        changed("busy"),
        started("run_2"),
        changed("busy"),
        started("run_3"),
        changed("busy"),
    ];
    let mut tx = pool.begin().await.unwrap();
    emit(&mut tx, &ada, &notes).await.unwrap();
    tx.commit().await.unwrap();

    let mut told = Vec::new();
    while let Some(next) = block(&mut frames, 1_500).await {
        told.push(next.id);
    }
    assert_eq!(
        told,
        [5, 7, 9, 11, 12],
        "the last of each thread, and every run start"
    );
    assert!(
        told_at.elapsed() >= Duration::from_millis(400),
        "the burst waited out the window"
    );
}

/// WHAT A BURST SAYS ABOUT WHOSE COMMIT IT WAS is on the wire as the merge decided: one run's
/// rounds name that run, and rounds of two runs, or of a run and a person, name none. The app that
/// streams a run skips the read for its own, and must not skip one it was not the cause of.
#[tokio::test]
async fn a_burst_names_its_run_only_when_every_note_in_it_does() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    hub.tune(Tuning {
        ping: Duration::from_secs(60),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(600),
        room: 64,
    });
    let mut frames = hub.follow(&ada, None).await.unwrap();
    must(&mut frames).await;
    note(&pool, &ada, changed("lead")).await;
    must(&mut frames).await;

    // Within the window, in one commit: "mine" is one run's alone, "split" is two runs', and
    // "settled" is a run's and then a person's.
    let notes = [
        by_run("mine", "run_a"),
        by_run("split", "run_a"),
        by_run("settled", "run_a"),
        by_run("mine", "run_a"),
        by_run("split", "run_b"),
        changed("settled"),
    ];
    let mut tx = pool.begin().await.unwrap();
    emit(&mut tx, &ada, &notes).await.unwrap();
    tx.commit().await.unwrap();

    let mut told = Vec::new();
    while let Some(next) = block(&mut frames, 1_500).await {
        told.push((
            next.data["threadId"].as_str().unwrap().to_string(),
            next.data["runId"].clone(),
        ));
    }
    assert_eq!(
        told,
        [
            ("mine".to_string(), json!("run_a")),
            ("split".to_string(), json!(null)),
            ("settled".to_string(), json!(null)),
        ]
    );
}

/// A REPLAY IS NOT MERGED: every note after the id, even a thread's twenty in a row.
#[tokio::test]
async fn a_replay_is_every_note_even_a_threads_twenty() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    for _ in 0..20 {
        note(&pool, &ada, changed("busy")).await;
    }
    let mut frames = hub.follow(&ada, Some("0")).await.unwrap();
    for want in 1..=20 {
        assert_eq!(must(&mut frames).await.id, want);
    }
}

/// A STREAM THAT FALLS BEHIND IS TOLD TO RESET, and the stream goes on from there. The room holds
/// two wakes; five land while the stream is not read.
#[tokio::test]
async fn a_stream_that_cannot_keep_up_is_told_to_reset() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    hub.tune(Tuning {
        ping: Duration::from_secs(60),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(10),
        room: 2,
    });
    let mut frames = hub.follow(&ada, None).await.unwrap();
    must(&mut frames).await;
    for n in 0..5 {
        note(&pool, &ada, changed(&format!("t{n}"))).await;
    }
    // The listener hands each wake to the room as it hears it; give it the time.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let first = must(&mut frames).await;
    assert_eq!((first.id, first.event.as_str()), (5, "reset"));
    note(&pool, &ada, changed("after")).await;
    let next = must(&mut frames).await;
    assert_eq!((next.id, next.event.as_str()), (6, "thread.changed"));
}

/// WAKES THAT COME LATE FOR NOTES A STREAM ALREADY HAS ARE NOT A FALL BEHIND. Postgres may hand
/// the listener a burst of wakes after the stream has read what they announce (a reset covered
/// them, or a poll did). They overflowed the room, and the stream was told `reset` again for
/// nothing: every note it had not read was still in the outbox, one note away. Seen in CI as the
/// stream above resetting twice (10 Oct 2026).
#[tokio::test]
async fn late_wakes_for_notes_already_read_do_not_reset_the_stream() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    hub.tune(Tuning {
        ping: Duration::from_secs(60),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(10),
        room: 2,
    });
    let mut frames = hub.follow(&ada, None).await.unwrap();
    must(&mut frames).await;
    note(&pool, &ada, changed("t0")).await;
    assert_eq!(must(&mut frames).await.id, 1);

    // Five wakes for the note already read, more than the room holds, heard while the stream is
    // not read.
    for _ in 0..5 {
        let late = "select pg_notify($1, $2)";
        let wake = format!("{ada}:1");
        let send = sqlx::query(late).bind(opengrok_events::CHANNEL).bind(wake);
        send.execute(&pool).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    note(&pool, &ada, changed("after")).await;
    let next = must(&mut frames).await;
    assert_eq!((next.id, next.event.as_str()), (2, "thread.changed"));
}

/// A LOST WAKE COSTS LATENCY, NOT A NOTE. This one is written with no NOTIFY at all; the stream's
/// own poll finds it.
#[tokio::test]
async fn a_note_whose_wake_was_lost_is_found_by_the_poll() {
    let Some(pool) = pool().await else { return };
    let (hub, ada) = (hub(&pool), account());
    hub.tune(Tuning {
        ping: Duration::from_secs(60),
        poll: Duration::from_millis(300),
        window: Duration::from_millis(10),
        room: 64,
    });
    let mut frames = hub.follow(&ada, None).await.unwrap();
    must(&mut frames).await;

    let head = "insert into account_event_head (account_id, head) values ($1, 1)";
    sqlx::query(head).bind(&ada).execute(&pool).await.unwrap();
    let write = "insert into account_event (account_id, id, kind, payload)
                 values ($1, 1, 'thread.changed', '{\"threadId\":\"quiet\",\"coworkerId\":\"cw_1\"}')";
    sqlx::query(write).bind(&ada).execute(&pool).await.unwrap();

    let heard = block(&mut frames, 3_000).await.expect("found by the poll");
    assert_eq!(
        (heard.id, heard.data["threadId"].as_str()),
        (1, Some("quiet"))
    );
}
