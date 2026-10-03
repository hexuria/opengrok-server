//! A note and the change it describes are one transaction: no commit without its note, and no note
//! for a write that did not commit (`opengrok-events`, `PgStore::append_run`, `append_schedule`).
//!
//! The first is shown by making the note impossible to write and watching the change not land;
//! the second by a write that loses its race.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_core::id::{AccountId, CoworkerId, RunId, ScheduleId};
use opengrok_core::run::{Run, RunCommand, RunView, SuspendReason};
use opengrok_core::schedule::{Schedule, ScheduleCommand, Wake};
use opengrok_store::{PgStore, StoreError};

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

async fn store(url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool).await.expect("boot");
    PgStore::new(pool)
}

/// Make writing a note for this account fail, and only this account: the other tests of this
/// file share the table. Dropped by the returned name when the test is done.
async fn refuse_notes_for(store: &PgStore, account: &AccountId) -> String {
    let name = format!("refuse_{}", uuid::Uuid::now_v7().simple());
    let add = format!(
        "alter table account_event add constraint {name} check (account_id <> '{account}') not valid"
    );
    sqlx::query(sqlx::AssertSqlSafe(add))
        .execute(store.pool())
        .await
        .unwrap();
    name
}

/// Make writing a `run.waiting` for this account fail and nothing else of its notes: a park whose
/// own note is refused must not land, which a refusal of every note cannot show (the park's
/// `thread.changed` would be the one to fail first).
async fn refuse_waiting_for(store: &PgStore, account: &AccountId) -> String {
    let name = format!("refuse_{}", uuid::Uuid::now_v7().simple());
    let add = format!(
        "alter table account_event add constraint {name}
         check (not (account_id = '{account}' and kind = 'run.waiting')) not valid"
    );
    sqlx::query(sqlx::AssertSqlSafe(add))
        .execute(store.pool())
        .await
        .unwrap();
    name
}

async fn allow_notes_again(store: &PgStore, name: &str) {
    let drop = format!("alter table account_event drop constraint {name}");
    sqlx::query(sqlx::AssertSqlSafe(drop))
        .execute(store.pool())
        .await
        .unwrap();
}

async fn count(store: &PgStore, select: &'static str, key: &str) -> i64 {
    sqlx::query_scalar(select)
        .bind(key)
        .fetch_one(store.pool())
        .await
        .unwrap()
}

fn start(thread: &str, coworker: &CoworkerId) -> RunCommand {
    RunCommand::Start {
        thread_id: thread.to_string(),
        coworker_id: Some(coworker.clone()),
        model: None,
        effort: Default::default(),
        inference_source: Default::default(),
        system: None,
        skill_id: None,
        offered_skills: Vec::new(),
        prompt: None,
        limits: Default::default(),
        at_ms: 1,
    }
}

/// NO COMMIT WITHOUT ITS NOTE. With the note impossible to write, the run's first batch does not
/// land: no event in its log, and no row in its view. A note written after the commit, or beside
/// it, would leave the run behind and the app never told.
#[tokio::test]
async fn a_run_whose_note_cannot_be_written_is_not_written() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let id = RunId::new();
    let mut run = Run::default();
    let events = run.decide(start("thread-refused", &luna)).unwrap();
    events.iter().for_each(|event| run.apply(event));
    let view = RunView {
        id: id.clone(),
        thread_id: "thread-refused".into(),
        status: run.status,
        event_count: 0,
        updated_at_ms: 1,
    };

    let refusal = refuse_notes_for(&store, &ada).await;
    let appended = store.append_run(&id, 0, &events, &view, Some(&ada)).await;
    allow_notes_again(&store, &refusal).await;

    assert!(
        matches!(appended, Err(StoreError::Database(_))),
        "{appended:?}"
    );
    let log = "select count(*) from events where stream_id = $1";
    assert_eq!(count(&store, log, &format!("run/{id}")).await, 0);
    let viewed = "select count(*) from run_view where id = $1";
    assert_eq!(count(&store, viewed, id.as_str()).await, 0);
    let notes = "select count(*) from account_event where account_id = $1";
    assert_eq!(count(&store, notes, ada.as_str()).await, 0);

    // And once notes can be written the same write lands, with its notes.
    let appended = store.append_run(&id, 0, &events, &view, Some(&ada)).await;
    assert_eq!(appended.unwrap(), 1);
    assert_eq!(count(&store, notes, ada.as_str()).await, 2);
}

/// NO PARK WITHOUT ITS NOTE. The batch that parks a run is written with the `run.waiting` that
/// says so, or not at all: with that one note impossible to write, the park does not land, and the
/// run is as it was. Nothing of the park is in its log, its view still says it is running, and the
/// stream has not heard of it. A `run.waiting` written after the commit would leave a run waiting
/// on a person who is never told.
#[tokio::test]
async fn a_park_whose_note_cannot_be_written_is_not_written() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let id = RunId::new();
    let mut run = Run::default();
    let started = run.decide(start("thread-parked", &luna)).unwrap();
    started.iter().for_each(|event| run.apply(event));
    let view = RunView {
        id: id.clone(),
        thread_id: "thread-parked".into(),
        status: run.status,
        event_count: 0,
        updated_at_ms: 1,
    };
    let seq = store
        .append_run(&id, 0, &started, &view, Some(&ada))
        .await
        .unwrap();

    let park = RunCommand::Suspend {
        call_id: "call_1".into(),
        tool: "shell".into(),
        arguments: serde_json::json!({}),
        reason: SuspendReason::PolicyApproval,
        at_ms: 2,
    };
    let parked = run.decide(park).unwrap();
    parked.iter().for_each(|event| run.apply(event));
    let waiting = RunView {
        status: run.status,
        updated_at_ms: 2,
        ..view
    };

    let refusal = refuse_waiting_for(&store, &ada).await;
    let appended = store
        .append_run(&id, seq, &parked, &waiting, Some(&ada))
        .await;
    allow_notes_again(&store, &refusal).await;

    assert!(
        matches!(appended, Err(StoreError::Database(_))),
        "{appended:?}"
    );
    let log = "select count(*) from events where stream_id = $1";
    assert_eq!(count(&store, log, &format!("run/{id}")).await, 1);
    let running = "select count(*) from run_view where id = $1 and status = 'running'";
    assert_eq!(count(&store, running, id.as_str()).await, 1);
    let notes = "select count(*) from account_event where account_id = $1";
    assert_eq!(count(&store, notes, ada.as_str()).await, 2, "the start's");

    // And once the note can be written the same write lands, with its two.
    let appended = store
        .append_run(&id, seq, &parked, &waiting, Some(&ada))
        .await;
    assert_eq!(appended.unwrap(), seq + 1);
    assert_eq!(count(&store, notes, ada.as_str()).await, 4);
    let waits = "select count(*) from account_event where account_id = $1 and kind = 'run.waiting'";
    assert_eq!(count(&store, waits, ada.as_str()).await, 1);
}

/// The same for a routine: a write whose note cannot be written does not land, the row included.
#[tokio::test]
async fn a_routine_whose_note_cannot_be_written_is_not_written() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let id = ScheduleId::new();
    let create = ScheduleCommand::Create {
        coworker_id: luna,
        prompt: "write the report".into(),
        name: "Weekly".into(),
        wake: Wake::Cron {
            cron: "0 9 * * 1".into(),
        },
        run_limits: Default::default(),
        tz: "UTC".into(),
        at_ms: 1,
    };
    let state = Schedule::default();
    let events = state.decide(create).unwrap();
    let state = Schedule::replay(&events);

    let refusal = refuse_notes_for(&store, &ada).await;
    let appended = store
        .append_schedule(&id, &ada, 0, &events, &state, 1)
        .await;
    allow_notes_again(&store, &refusal).await;

    assert!(
        matches!(appended, Err(StoreError::Database(_))),
        "{appended:?}"
    );
    let log = "select count(*) from events where stream_id = $1";
    assert_eq!(count(&store, log, &format!("schedule/{id}")).await, 0);
    let viewed = "select count(*) from schedule_view where id = $1";
    assert_eq!(count(&store, viewed, id.as_str()).await, 0);
}

/// NO NOTE FOR A WRITE THAT DID NOT COMMIT. A batch that loses the race for its sequence is a
/// `Conflict` and writes nothing, its note and the id it would have taken included: the next write
/// takes the id it would have.
#[tokio::test]
async fn a_write_that_loses_its_race_leaves_no_note_and_takes_no_id() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let id = RunId::new();
    let mut run = Run::default();
    let events = run.decide(start("thread-raced", &luna)).unwrap();
    events.iter().for_each(|event| run.apply(event));
    let view = RunView {
        id: id.clone(),
        thread_id: "thread-raced".into(),
        status: run.status,
        event_count: 0,
        updated_at_ms: 1,
    };
    store
        .append_run(&id, 0, &events, &view, Some(&ada))
        .await
        .unwrap();
    let notes = "select count(*) from account_event where account_id = $1";
    let told = count(&store, notes, ada.as_str()).await;

    // A second writer that read the run before the first one wrote.
    let lost = store.append_run(&id, 0, &events, &view, Some(&ada)).await;
    assert!(matches!(lost, Err(StoreError::Conflict)), "{lost:?}");
    assert_eq!(count(&store, notes, ada.as_str()).await, told);
    let head = "select head from account_event_head where account_id = $1";
    assert_eq!(count(&store, head, ada.as_str()).await, told);
}
