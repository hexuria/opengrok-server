//! The account events stream (`GET /ag-ui/events`, #348): small notes that something the server did
//! changed, so the app re-reads it. Each is one SSE block, `id: <n>`, `event: <name>`, `data: <one
//! line of JSON>`. The id is the account's own and only goes up; the app sends the last it saw as
//! `Last-Event-ID` and is replayed what came after.
//!
//! Provenance: the contract agreed with NativeChat on hexuria/nativechat#171 (3 Oct 2026), which
//! names every note and field below; `thread.changed`'s `runId` is its client build's addition the
//! same day. `run.waiting` is the owner-approved follow-up in hexuria/opengrok-server#355 (design
//! v4, section 3): an addition, which a client that does not know the name skips
//! (`AccountEvent::read` in NativeChat's `src/opengrok/events.rs` answers an unknown name
//! `Unread::Unknown` and goes on). `cause`, `state` and `reason` are not words of its own: they
//! are the ones the server already says elsewhere, so the app reads one vocabulary. `cause` and
//! `state` are a routine's run history's (`GET /schedules/{id}/runs`): the causes are `clock`,
//! `manual`, `webhook` and `bot` for a routine, `event` and `manual` for a monitor, and a run
//! nothing fired is `chat`; its end is `ok` or `error` (a stop is an `error`, as the history has
//! it). `reason` is the approvals queue's (`GET /ag-ui/approvals`), the run's own word for what
//! its card asks.
//!
//! IDS ONLY. No variant has anywhere to put a message, a title or a name: a note says WHAT changed
//! and the app asks for it. That keeps nothing sensitive on a stream that stays open for hours, and
//! a note dropped or doubled costs one extra read, never a lost message.

use opengrok_core::run::SuspendReason;
use serde::Serialize;

pub const THREAD_CHANGED: &str = "thread.changed";
pub const RUN_STARTED: &str = "run.started";
pub const RUN_WAITING: &str = "run.waiting";
pub const RUN_FINISHED: &str = "run.finished";
pub const ROUTINE_CHANGED: &str = "routine.changed";
/// Not stored: the server's own answer to an id it cannot resume from, or to a stream that fell
/// behind. The app forgets what it holds and reads it all again.
pub const RESET: &str = "reset";

/// Every name a block's `event:` can carry: what the wire corpus lists as sent.
pub const EVENTS: [&str; 6] = [
    THREAD_CHANGED,
    RUN_STARTED,
    RUN_WAITING,
    RUN_FINISHED,
    ROUTINE_CHANGED,
    RESET,
];

/// An SSE comment, which a reader skips: it keeps a quiet stream from being taken for a dead one.
pub const PING: &str = ": ping\n\n";

/// What happened to a routine. A firing, a skipped firing and a rotated key are an `updated`: the
/// row the app shows (`lastRun`, `nextDueMs`, the webhook's key) changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Created,
    Updated,
    Deleted,
    Paused,
    Resumed,
}

/// One note, as its `data:` line says it. Untagged: the name is the block's `event:`, and a name
/// inside the object as well would be a second place for the two to disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum Note<'a> {
    /// A message, a run's frames or a card settled: a journal round was appended. `run_id` is the
    /// run whose own commit it was, so the app can skip reading what it is already streaming; it
    /// is `null`, and present, for a change no run's commit caused: a person settling a card or
    /// stopping a run, the sweep.
    ThreadChanged {
        thread_id: &'a str,
        coworker_id: &'a str,
        run_id: Option<&'a str>,
    },
    /// A run began. `routine_id` is the routine that fired it, left out for any other run.
    RunStarted {
        run_id: &'a str,
        thread_id: &'a str,
        coworker_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        routine_id: Option<&'a str>,
        cause: &'a str,
    },
    /// A run parked on a card and is waiting on the person: the app's "needs you". Sent when the
    /// park is written, once for a batch, and not when the person answers and the run goes on
    /// (that is a `ThreadChanged`, and the run's end a `RunFinished`); a run that parks again
    /// sends it again. `reason` is what the card asks, in the run's own word (`exec-consent`,
    /// `policy-approval`, `auto-review` or `user-form`) and by its type nothing else: never the
    /// card's text or its call's arguments.
    RunWaiting {
        run_id: &'a str,
        thread_id: &'a str,
        coworker_id: &'a str,
        reason: SuspendReason,
    },
    /// A run ended for good. A run waiting on a card has not: that is a `ThreadChanged` and a
    /// `RunWaiting`.
    RunFinished {
        run_id: &'a str,
        thread_id: &'a str,
        coworker_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        routine_id: Option<&'a str>,
        state: &'a str,
    },
    RoutineChanged {
        routine_id: &'a str,
        coworker_id: &'a str,
        change: Change,
    },
}

impl Note<'_> {
    /// The block's `event:`.
    pub fn event(&self) -> &'static str {
        match self {
            Self::ThreadChanged { .. } => THREAD_CHANGED,
            Self::RunStarted { .. } => RUN_STARTED,
            Self::RunWaiting { .. } => RUN_WAITING,
            Self::RunFinished { .. } => RUN_FINISHED,
            Self::RoutineChanged { .. } => ROUTINE_CHANGED,
        }
    }

    /// The block's `data:`, on one line, which an SSE field must be.
    pub fn data(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// One SSE block. `data` must be a single line; JSON from `serde_json` always is.
pub fn block(id: i64, event: &str, data: &str) -> String {
    format!("id: {id}\nevent: {event}\ndata: {data}\n\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/events.rs"]
mod tests;
