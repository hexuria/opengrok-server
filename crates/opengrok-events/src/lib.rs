//! The account events stream's data side (#348): the outbox its notes are written to, and the hold
//! a process keeps on it to follow it live. The wire is `opengrok_wire::events`; the route is
//! `GET /ag-ui/events`, in the server.
//!
//! A NOTE IS WRITTEN IN THE TRANSACTION OF THE CHANGE IT DESCRIBES (`PgStore::append_run`,
//! `append_schedule`), as that transaction's last statement: no commit without its note, and no
//! note for a change that rolled back. Everything here runs on the caller's connection for that
//! reason, and nothing here is called before a commit or after one.
//!
//! IDS COME FROM THE ACCOUNT'S HEAD ROW, NOT A SEQUENCE. A sequence hands out numbers in the order
//! transactions ask and they commit in another, so a reader that has seen note 11 could still be
//! missing note 10, and would skip it for good. The head row is locked until commit: notes of one
//! account are numbered in commit order, and a reader that has seen id n has seen every id below.
//! Ids are an account's own, only ever go up, and survive a restart; they are not consecutive,
//! because retention removes the oldest.
//!
//! NOTIFY IS ONLY A WAKE-UP. The outbox is the truth: a stream reads it from its cursor whenever
//! it is woken, and every 30 s whether it was or not, so a lost wake costs latency and nothing
//! else. A process holds ONE listening connection (`Hub`), not one per stream.

mod appended;
mod follow;
mod hub;
mod outbox;

pub use appended::{routine_appended, run_appended};
pub use hub::{CHANNEL, Hub, Tuning};
pub use outbox::{RETAIN_HOURS, RETAIN_MAX, emit};

/// The outbox's tables, applied with the store's schema (`opengrok_store::migrations`), so one
/// digest covers both and an unchanged schema is not replayed on every boot.
pub const SCHEMA: &str = r#"
-- The account events stream's notes (opengrok-events): ids only, never text. `id` is the account's
-- own, handed out under its head row's lock so that ids are in commit order. `head` never goes down
-- and `floor` is the highest id retention has removed: an id below it cannot be resumed from.
create table if not exists account_event_head (
    account_id text   primary key,
    head       bigint not null,
    floor      bigint not null default 0
);

create table if not exists account_event (
    account_id text        not null,
    id         bigint      not null,
    kind       text        not null,
    payload    jsonb       not null,
    created_at timestamptz not null default now(),
    primary key (account_id, id)
);

create index if not exists account_event_age_idx on account_event (account_id, created_at);
"#;
