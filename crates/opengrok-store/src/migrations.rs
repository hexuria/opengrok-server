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
const EVERY_BOOT: &str = r#"
-- The screen tools (open_url, computer) joined the built-ins. A grant or ceiling written as
-- EXACTLY the previous built-in set was "everything this server implements" when it was written,
-- so it follows the built-ins; a narrower or wider list was chosen on purpose and is left alone.
-- Idempotent: once widened, the row no longer matches.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["read_file", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["read_file", "shell", "write_file"]}'::jsonb and not chosen;
-- `run_recipe` joined the built-ins the same way: a row that is exactly the five-tool set
-- follows; the two statements chain, so a three-tool row widens twice in one boot.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb and not chosen;
-- `request_user_form` joined the built-ins the same way: a row that is exactly today's
-- previous set follows; a narrower list was chosen on purpose and is left alone.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb and not chosen;
-- `credential.request` joined the built-ins the same way. Site passwords are NOT stored;
-- this tool only asks the client to fill a saved login.
update grant_view
   set profile = '{"only": ["computer", "credential.request", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "credential.request", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb and not chosen;
-- `credential.request` left with the broker (Sep 2026): the saved login is offered on the
-- ordinary form card. The widening just above still matches today's default grant, so it
-- would put the dead name on every fresh bot at every boot; this takes it out again.
update grant_view
   set profile = jsonb_set(profile, '{only}', (profile->'only') - 'credential.request')
 where profile->'only' ? 'credential.request';
update ceiling_view
   set tools = jsonb_set(tools, '{only}', (tools->'only') - 'credential.request'), version = version + 1
 where tools->'only' ? 'credential.request';
-- #268: a coworker's ceiling now decides whether it may reach its person's machine, and every
-- ceiling written before that allowed the machine in effect, so each gains it ONCE. LAST, after
-- the widenings above: they look for an exact older list, which never names the machine, so a
-- ceiling on an older list that gained it first would stay narrow for good while its profile
-- widened. After the pass a ceiling without the machine is one its owner switched off, which a
-- second pass would switch back on, so its row in schema_migrations makes every later boot's
-- pass match nothing. A row an older replica writes mid-deploy misses it: the machine is off.
update ceiling_view
   set version = version + 1,
       tools = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from jsonb_array_elements_text((tools->'only') || '["user_machine_shell"]'::jsonb) tool))
 where tools ? 'only' and not (tools->'only' ? 'user_machine_shell')
   and not exists (select 1 from schema_migrations where name = 'the-machine-joins-the-ceiling');
insert into schema_migrations (name) values ('the-machine-joins-the-ceiling') on conflict do nothing;
-- #314: messaging the person's other Bots is a ceiling row, on by default, so every ceiling gains
-- it ONCE, after the machine and for the same reasons; one without it after this was switched off.
update ceiling_view
   set version = version + 1,
       tools = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from jsonb_array_elements_text((tools->'only') || '["message_bot"]'::jsonb) tool))
 where tools ? 'only' and not (tools->'only' ? 'message_bot')
   and not exists (select 1 from schema_migrations where name = 'the-bots-join-the-ceiling');
insert into schema_migrations (name) values ('the-bots-join-the-ceiling') on conflict do nothing;
-- #316: a Bot's four routine tools are one ceiling row, on by default, so every list ceiling gains
-- them ONCE, after the machine and the Bots and for the same reasons; and its owner's profile too,
-- which the run's policy intersects with it (neither of those needed a profile). One without them
-- after this was switched off, and a set that admits nothing still admits nothing.
update ceiling_view
   set version = version + 1,
       tools = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from (select distinct jsonb_array_elements_text((tools->'only') || '["create_routine",
           "delete_routine", "list_routines", "update_routine"]'::jsonb) tool) known))
 where not (tools->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}')
   and not exists (select 1 from schema_migrations where name = 'the-routines-join-the-ceiling');
update grant_view
   set profile = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from (select distinct jsonb_array_elements_text((profile->'only') || '["create_routine",
           "delete_routine", "list_routines", "update_routine"]'::jsonb) tool) known))
 where not (profile->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}')
   and not exists (select 1 from schema_migrations where name = 'the-routines-join-the-ceiling');
insert into schema_migrations (name) values ('the-routines-join-the-ceiling') on conflict do nothing;
-- #337: `run_routine` is the Routines row's fifth tool, so a list ceiling, and its owner's profile,
-- that has the row's four gains it ONCE, after them; one without them had the row switched off.
update ceiling_view
   set version = version + 1, tools = jsonb_build_object('only', (select jsonb_agg(tool order by
     tool collate "C") from jsonb_array_elements_text((tools->'only') || '["run_routine"]') tool))
 where tools->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}'
   and not (tools->'only' ? 'run_routine')
   and not exists (select 1 from schema_migrations where name = 'the-routine-runs-join-the-ceiling');
update grant_view
   set profile = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
     from jsonb_array_elements_text((profile->'only') || '["run_routine"]') tool))
 where profile->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}'
   and not (profile->'only' ? 'run_routine')
   and not exists (select 1 from schema_migrations where name = 'the-routine-runs-join-the-ceiling');
insert into schema_migrations (name) values ('the-routine-runs-join-the-ceiling') on conflict do nothing;
"#;

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
