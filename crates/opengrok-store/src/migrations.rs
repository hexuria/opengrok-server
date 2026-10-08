//! Schema, applied in-process under an advisory lock.
//!
//! The lock is why several replicas can boot at once without racing each other into a half-applied
//! schema: whoever gets it migrates, the rest wait and find the work done. Matches open-ai-gateway
//! (`RUNBOOK.md` §2). `SCHEMA` replays in full whenever it changes, so every statement in it runs
//! again on a database that already went through it; `EVERY_BOOT` runs on every boot. Read
//! docs/setup/postgres.md, "Data-transforming migrations", before writing either.

use sqlx::PgPool;

use crate::StoreResult;

/// Chosen once, arbitrary, and must never change: a different number is a different lock, which
/// would defeat the point on the one deploy where two versions overlap.
const MIGRATION_LOCK_KEY: i64 = 0x0_6E_67_72_6F_6B; // "ngrok" in hex, the tail of opengrok

const SCHEMA: &str = include_str!("schema.sql");

/// Run on EVERY boot, after `SCHEMA`, whether or not the schema itself was replayed.
///
/// A grant or ceiling written as exactly an older built-in set follows the built-ins. During a
/// deploy an older replica can still write the older set after a newer one has migrated, and the
/// next boot is what brings that row along (`old_grants_follow_the_builtins.rs`). These are
/// UPDATEs over rows that match, so they take row locks only, never the table locks that made
/// replaying `SCHEMA` on every boot deadlock against live reads. A CEILING ITS OWNER CHOSE
/// (`chosen`, #268) IS NEVER WIDENED, nor the grants on that coworker: a choice that equals an
/// older set is a choice, and widening it at the next boot would switch back on what its owner
/// switched off, or leave a profile wider than the ceiling it was set equal to. Every change
/// bumps `version`, so a write made against the old one is refused rather than undoing this.
const EVERY_BOOT: &str = include_str!("every_boot.sql");

/// Apply the schema. Safe to call on every boot and from every replica.
pub async fn run(pool: &PgPool) -> StoreResult<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await?;

    let applied = apply_unless_current(&mut conn).await;

    // Release even when the migration failed, or the next boot deadlocks against our own lock.
    let released = sqlx::query("select pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await;

    applied?;
    released?;
    Ok(())
}

/// Replay the schema only when THIS schema has not been applied here before.
///
/// A REPLAY IS NOT FREE, even when every statement is `if not exists`. The schema is one
/// transaction, and a bare `alter table … add column if not exists` takes ACCESS EXCLUSIVE before
/// it decides there is nothing to do, then holds it to the end. Every server and every test
/// harness boots through here, so any read that locked those tables in the other order
/// deadlocked against a boot: `policy_to_use` refused a turn over a grant that was fine, the
/// site-login saves died (#239), and a recipe's history prune was killed, leaving six runs where
/// five belong. Guarding each statement one at a time left the next one to be found in CI.
///
/// Keyed by the digest of `SCHEMA` and the events crate's, so an edit to either replays both once,
/// as before; only an unchanged schema is skipped. The check runs under the advisory lock, so two
/// replicas booting a new schema still apply it once and the second finds the digest. What must
/// run on every boot regardless lives in `EVERY_BOOT`, which never takes a table lock.
async fn apply_unless_current(conn: &mut sqlx::PgConnection) -> StoreResult<()> {
    use sha2::Digest as _;
    let digest: String = sha2::Sha256::digest([SCHEMA, opengrok_events::SCHEMA].concat())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // Created on its own, before the schema: it is the one table the skip decision reads.
    sqlx::query(
        "create table if not exists schema_applied (
             digest     text primary key,
             applied_at bigint not null
         )",
    )
    .execute(&mut *conn)
    .await?;
    let current: Option<(i32,)> = sqlx::query_as("select 1 from schema_applied where digest = $1")
        .bind(&digest)
        .fetch_optional(&mut *conn)
        .await?;
    if current.is_none() {
        sqlx::raw_sql(SCHEMA).execute(&mut *conn).await?;
        opengrok_events::apply(conn).await?;
        record_applied(conn, &digest).await?;
    }
    sqlx::raw_sql(EVERY_BOOT).execute(&mut *conn).await?;
    Ok(())
}

async fn record_applied(conn: &mut sqlx::PgConnection, digest: &str) -> StoreResult<()> {
    sqlx::query(
        "insert into schema_applied (digest, applied_at)
         values ($1, (extract(epoch from now()) * 1000)::bigint)
         on conflict (digest) do nothing",
    )
    .bind(digest)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
