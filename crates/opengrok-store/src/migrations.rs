//! Schema, applied in-process under an advisory lock.
//!
//! The lock is why several replicas can boot at once without racing each other into a half-applied
//! schema: whoever gets it migrates, the rest wait and then find the work already done. Matches
//! open-ai-gateway's pattern deliberately (`RUNBOOK.md` §2) — one database server for a developer,
//! and one habit to learn across the two services.

use sqlx::PgPool;

use crate::StoreResult;

/// Chosen once, arbitrary, and must never change: a different number is a different lock, which
/// would defeat the point on the one deploy where two versions overlap.
const MIGRATION_LOCK_KEY: i64 = 0x0_6E_67_72_6F_6B; // "ngrok" in hex, the tail of opengrok

const SCHEMA: &str = r#"
-- The log. Append-only: no UPDATE or DELETE is ever issued against this table.
create table if not exists events (
    id          bigserial primary key,
    stream_id   text        not null,
    stream_seq  bigint      not null,
    event_type  text        not null,
    payload     jsonb       not null,
    occurred_at timestamptz not null default now(),
    -- The optimistic-concurrency check, enforced by the database rather than by a read-then-write
    -- in application code, which would race.
    constraint events_stream_seq_unique unique (stream_id, stream_seq)
);

create index if not exists events_stream_idx on events (stream_id, stream_seq);

-- The lookup index for "whose refresh token is this". A projection like any other: derivable by
-- replaying `events`, and maintained in the same transaction as the append. A row exists only
-- while the token it names is live, so a rotated or revoked token simply has no row.
create table if not exists session_view (
    refresh_token_hash text   primary key,
    account_id         text   not null,
    session_id         text   not null
);

create index if not exists session_view_session_idx on session_view (session_id);

-- What a client asking "what happened in this run" is answered from. `status = running` with no
-- process behind it is the shape a restart leaves behind, and the reason this column exists: a
-- lost run must be findable, not merely absent.
create table if not exists run_view (
    id            text        primary key,
    thread_id     text        not null,
    status        text        not null,
    event_count   bigint      not null,
    updated_at_ms bigint      not null
);

create index if not exists run_view_thread_idx on run_view (thread_id);
create index if not exists run_view_status_idx on run_view (status);

-- A projection, not a source of truth: every column here is derivable by replaying `events`.
-- Written in the same transaction as the append that causes it.
create table if not exists account_view (
    id            text        primary key,
    email         text        not null unique,
    plan          text        not null,
    trial         boolean     not null,
    updated_at_ms bigint      not null
);
"#;

/// Apply the schema. Safe to call on every boot and from every replica.
pub async fn run(pool: &PgPool) -> StoreResult<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await?;

    let applied = sqlx::raw_sql(SCHEMA).execute(&mut *conn).await;

    // Release even when the migration failed, or the next boot deadlocks against our own lock.
    let released = sqlx::query("select pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await;

    applied?;
    released?;
    Ok(())
}
