//! What a store write tells the account's stream: the notes a run's append or a routine's append
//! leaves, worked out from what was appended and written in the same transaction (`outbox::emit`).
//!
//! These READ THE LOG AS THE STORE DOES and write nothing but notes: they name no state the
//! caller has not already committed to, and decide nothing about the run or the routine.

use opengrok_core::id::{AccountId, ScheduleId};
use opengrok_core::run::{RunEvent, RunStatus, RunView};
use opengrok_core::schedule::{Schedule, ScheduleEvent};
use opengrok_wire::events::{Change, Note};
use serde_json::Value;
use sqlx::PgConnection;

use crate::outbox::emit;

/// A run's coworker, from the `Started` that opened its log, as `threads_owned_by` reads it.
const COWORKER: &str = "select payload->>'coworker_id' from events
    where stream_id = $1 and stream_seq = 1 and event_type = 'run-started'";

/// What fired a run: the newest firing in its thread's routine or monitor stream that names it.
/// The thread id says where to look and never what the answer is: only a `Fired` makes a run the
/// routine's, and a person replying in a routine's thread is not one.
const FIRED: &str = "select event_type, payload from events
    where stream_id = any($1) and event_type in ('schedule-fired', 'monitor-fired')
      and payload->>'run_id' = $2
    order by stream_seq desc limit 1";

/// The notes a run's append leaves, for the account that owns the run, written on `conn`: the
/// store's transaction, just after it wrote the run's events and its view, and just before it
/// commits.
///
/// `owner` is the run's owner as the view now holds it, and `None` is nobody: an unowned run is
/// not a public one, and nobody is told of it. A run with no coworker is not told either; the
/// app places a thread by its Bot, and has nowhere to put one.
///
/// A batch of only `ToolStarted` and `Spent` is the log's own bookkeeping and writes nothing.
/// Any other is a `thread.changed`, with `run.started` before it when the run began in this batch,
/// `run.waiting` after it when the batch parked the run on a card, and `run.finished` after it
/// when it ended in this one. A run waiting on a card has not ended.
/// The `thread.changed` names the run when the run's own loop wrote the batch, and says `null`
/// when a person's answer or stop, or the sweep, did.
pub async fn run_appended(
    conn: &mut PgConnection,
    events: &[RunEvent],
    view: &RunView,
    owner: Option<&str>,
) -> Result<(), sqlx::Error> {
    let Some(owner) = owner else {
        return Ok(());
    };
    let bookkeeping =
        |event: &RunEvent| matches!(event, RunEvent::ToolStarted { .. } | RunEvent::Spent { .. });
    if events.iter().all(bookkeeping) {
        return Ok(());
    }
    let started = events.iter().find_map(|event| match event {
        RunEvent::Started { coworker_id, .. } => Some(coworker_id),
        _ => None,
    });
    let ended = events.iter().any(|event| {
        matches!(
            event,
            RunEvent::Finished { .. } | RunEvent::Failed { .. } | RunEvent::Stopped { .. }
        )
    });
    let coworker = match started {
        Some(coworker) => coworker.as_ref().map(|id| id.as_str().to_string()),
        None => {
            let log = format!("run/{}", view.id);
            let opened = sqlx::query_scalar::<_, Option<String>>(COWORKER).bind(log);
            opened.fetch_optional(&mut *conn).await?.flatten()
        }
    };
    let Some(coworker) = coworker else {
        return Ok(());
    };
    let (routine, cause) = if started.is_some() || ended {
        fired(conn, &view.thread_id, view.id.as_str()).await?
    } else {
        (None, "chat")
    };
    let (run_id, thread_id) = (view.id.as_str(), view.thread_id.as_str());
    let (coworker_id, routine_id) = (coworker.as_str(), routine.as_deref());
    // WHOSE COMMIT IT IS. The run's own loop writes its start, its frames and its card; a person's
    // answer or stop, and the sweep's resume, are not the run's, and a frame that rides with one
    // (a parked run interrupted) is not either. The app skips reading a thread for the run it is
    // streaming, so a change it must read says `null` and never the run it happens to be streaming.
    let others = |event: &RunEvent| {
        let by_others = matches!(event, RunEvent::Answered { .. } | RunEvent::Stopped { .. });
        by_others || matches!(event, RunEvent::Resumed { .. })
    };
    let own = |event: &RunEvent| {
        let begun = matches!(event, RunEvent::Started { .. } | RunEvent::Emitted { .. });
        begun || matches!(event, RunEvent::Suspended { .. })
    };
    let caused_by = (events.iter().any(own) && !events.iter().any(others)).then_some(run_id);
    let mut notes = Vec::with_capacity(3);
    if started.is_some() {
        notes.push(Note::RunStarted {
            run_id,
            thread_id,
            coworker_id,
            routine_id,
            cause,
        });
    }
    notes.push(Note::ThreadChanged {
        thread_id,
        coworker_id,
        run_id: caused_by,
    });
    // A PARK IS TOLD ONCE, FROM WHAT THE BATCH LEAVES THE RUN AS: still waiting. A park that rode
    // with an ending left nothing to answer, a frame appended to a run already waiting parked
    // nothing, and a person's answer is no park. A round that stacks cards is one park, told with
    // the reason of the call the run now waits on: the aggregate keeps one pending call, the last.
    let parked = events.iter().rev().find_map(|event| match event {
        RunEvent::Suspended { reason, .. } => Some(*reason),
        _ => None,
    });
    if let Some(reason) = parked.filter(|_| view.status == RunStatus::AwaitingApproval) {
        notes.push(Note::RunWaiting {
            run_id,
            thread_id,
            coworker_id,
            reason,
        });
    }
    if ended {
        let state = view.status.history_word();
        notes.push(Note::RunFinished {
            run_id,
            thread_id,
            coworker_id,
            routine_id,
            state,
        });
    }
    emit(conn, owner, &notes).await
}

/// The routine that fired `run` (`None` for a monitor, which is not one to the app) and the word
/// the history gives its cause. A run nothing fired is `chat`. A firing the log holds but this
/// build cannot read is `chat` too: a note with a plainer cause beats a journal write that fails.
async fn fired(
    conn: &mut PgConnection,
    thread: &str,
    run: &str,
) -> Result<(Option<String>, &'static str), sqlx::Error> {
    let streams = vec![format!("schedule/{thread}"), format!("monitor/{thread}")];
    let found = sqlx::query_as::<_, (String, Value)>(FIRED).bind(streams);
    let Some((kind, payload)) = found.bind(run).fetch_optional(conn).await? else {
        return Ok((None, "chat"));
    };
    if kind == "monitor-fired" {
        let manual = payload["manual"].as_bool().unwrap_or(false);
        return Ok((None, if manual { "manual" } else { "event" }));
    }
    let mut routine = Schedule::default();
    if let Ok(event) = serde_json::from_value::<ScheduleEvent>(payload) {
        routine.apply(&event);
    }
    Ok(match routine.fired(run) {
        Some((cause, _)) => (Some(thread.to_string()), cause),
        None => (None, "chat"),
    })
}

/// The notes a routine's append leaves, for the account that owns it, written on `conn` like a
/// run's. One per event, and a change twice over in a row is told once. A firing, a skipped one
/// and a rotated key are an `updated`: the row the app shows changed. `coworker` is the Bot the
/// routine is on after the append.
pub async fn routine_appended(
    conn: &mut PgConnection,
    owner: &AccountId,
    routine: &ScheduleId,
    coworker: &str,
    events: &[ScheduleEvent],
) -> Result<(), sqlx::Error> {
    let change = |event: &ScheduleEvent| match event {
        ScheduleEvent::Created { .. } => Change::Created,
        ScheduleEvent::Paused { .. } => Change::Paused,
        ScheduleEvent::Resumed { .. } => Change::Resumed,
        ScheduleEvent::Deleted { .. } => Change::Deleted,
        ScheduleEvent::Updated { .. }
        | ScheduleEvent::SecretRotated { .. }
        | ScheduleEvent::Fired { .. }
        | ScheduleEvent::Skipped(_) => Change::Updated,
    };
    let mut notes: Vec<Note> = events
        .iter()
        .map(|event| Note::RoutineChanged {
            routine_id: routine.as_str(),
            coworker_id: coworker,
            change: change(event),
        })
        .collect();
    notes.dedup();
    emit(conn, owner.as_str(), &notes).await
}
