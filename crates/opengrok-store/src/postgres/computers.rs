//! Computers: enrolled daemons, relays, scoped boxes, and the org secrets behind them.

use super::*;

/// An enrolled machine (`local_exec_daemon`), as `GET /local-exec/daemon` lists it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Daemon {
    pub machine_id: String,
    pub label: String,
    pub enrolled_at_ms: i64,
    pub revoked: bool,
    /// Its own relay switch: on until switched off, or off from enrolment as its others were.
    pub relay_enabled: bool,
}

/// The first key of an account's relay lock ("RLAY", `relay_lock`); the second, its id's hash.
const RELAY_LOCK_CLASS: i32 = 0x524C_4159;

impl PgStore {
    // ---- Per-org computer credentials (box.ascii.dev key, Windows 365 creds) ----
    //
    // Reuses the generic sealed `secret_store`, keyed `org-computer:{org}:{kind}`. The org admin
    // sets these on the admin dashboard; the server opens them to provision that org's boxes. The
    // plaintext key never leaves the server and is never returned to any client — only whether a
    // kind is configured.

    pub async fn set_org_computer_secret(
        &self,
        vault: &Vault,
        org_id: &str,
        kind: &str,
        plaintext: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        let id = format!("org-computer:{org_id}:{kind}");
        let sealed = vault.seal(&id, plaintext)?;
        crate::vault_rows::write_sealed(&self.pool, &id, &sealed, at_ms).await?;
        Ok(())
    }

    /// The plaintext credential for one org+kind — the one place it is in the clear, at provision
    /// time. `None` when the org has not configured that kind.
    pub async fn org_computer_secret(
        &self,
        vault: &Vault,
        org_id: &str,
        kind: &str,
    ) -> StoreResult<Option<String>> {
        self.open_credential(vault, &format!("org-computer:{org_id}:{kind}"))
            .await
    }

    pub async fn clear_org_computer_secret(&self, org_id: &str, kind: &str) -> StoreResult<()> {
        sqlx::query("delete from secret_store where id = $1")
            .bind(format!("org-computer:{org_id}:{kind}"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Computer sharing mode (org default + per-account override) ----

    /// Set a sharing mode for a scope. scope is "org" (the org default) or "account" (an override);
    /// mode is "per-org" | "per-account" | "per-bot".
    pub async fn set_sharing_mode(
        &self,
        scope: &str,
        scope_id: &str,
        mode: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into computer_sharing (scope, scope_id, mode, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (scope, scope_id) do update set
               mode = excluded.mode, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(mode)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn sharing_mode(&self, scope: &str, scope_id: &str) -> StoreResult<Option<String>> {
        let row =
            sqlx::query("select mode from computer_sharing where scope = $1 and scope_id = $2")
                .bind(scope)
                .bind(scope_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("mode"))
            .transpose()?)
    }

    pub async fn clear_sharing_mode(&self, scope: &str, scope_id: &str) -> StoreResult<()> {
        let clear = sqlx::query("delete from computer_sharing where scope = $1 and scope_id = $2");
        clear.bind(scope).bind(scope_id).execute(&self.pool).await?;
        Ok(())
    }

    // ---- Egress policy: the standing answer to the tunnel card, per computer scope ----

    /// The stored mode for a computer scope, or `None` when unset (the server reads that as
    /// `ask`, one card per run).
    pub async fn egress_policy_mode(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<String>> {
        let row = sqlx::query("select mode from egress_policy where scope = $1 and scope_id = $2")
            .bind(scope)
            .bind(scope_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("mode"))
            .transpose()?)
    }

    /// Set the mode for a computer scope; mode is "bypass" | "ask" | "never" (the server
    /// validates the word).
    pub async fn set_egress_policy_mode(
        &self,
        scope: &str,
        scope_id: &str,
        mode: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into egress_policy (scope, scope_id, mode, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (scope, scope_id) do update set
               mode = excluded.mode, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(mode)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Reverse-exec consent, per (account, machine). Raw pieces only — the server assembles the
    //      LocalExecPolicy and runs the gate; this crate stays free of that logic. ----

    /// The stored mode for a machine, or `None` when unset (the gate reads that as the default,
    /// `never` — the channel is off).
    pub async fn local_exec_mode(
        &self,
        account_id: &str,
        machine_id: &str,
    ) -> StoreResult<Option<String>> {
        let sql = "select mode from local_exec_policy where account_id = $1 and machine_id = $2";
        let mode = sqlx::query_scalar(sql).bind(account_id).bind(machine_id);
        Ok(mode.fetch_optional(&self.pool).await?)
    }

    pub async fn set_local_exec_mode(
        &self,
        account_id: &str,
        machine_id: &str,
        mode: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        let sql = "insert into local_exec_policy (account_id, machine_id, mode, updated_at_ms)
                   values ($1, $2, $3, $4)
                   on conflict (account_id, machine_id) do update set
                     mode = excluded.mode, updated_at_ms = excluded.updated_at_ms";
        let query = sqlx::query(sql).bind(account_id).bind(machine_id);
        query.bind(mode).bind(at_ms).execute(&self.pool).await?;
        Ok(())
    }

    /// The allow or deny patterns for a machine (`kind` = "allow" | "deny").
    pub async fn local_exec_rules(
        &self,
        account_id: &str,
        machine_id: &str,
        kind: &str,
    ) -> StoreResult<Vec<String>> {
        let sql = "select pattern from local_exec_rule
                   where account_id = $1 and machine_id = $2 and kind = $3 order by added_at_ms";
        let rows = sqlx::query_scalar(sql).bind(account_id).bind(machine_id);
        Ok(rows.bind(kind).fetch_all(&self.pool).await?)
    }

    pub async fn add_local_exec_rule(
        &self,
        account_id: &str,
        machine_id: &str,
        kind: &str,
        pattern: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        let sql = "insert into local_exec_rule (account_id, machine_id, kind, pattern, added_at_ms)
                   values ($1, $2, $3, $4, $5)
                   on conflict (account_id, machine_id, kind, pattern) do nothing";
        let query = sqlx::query(sql).bind(account_id).bind(machine_id);
        let query = query.bind(kind).bind(pattern).bind(at_ms);
        query.execute(&self.pool).await?;
        Ok(())
    }

    pub async fn remove_local_exec_rule(
        &self,
        account_id: &str,
        machine_id: &str,
        kind: &str,
        pattern: &str,
    ) -> StoreResult<()> {
        let sql = "delete from local_exec_rule
                   where account_id = $1 and machine_id = $2 and kind = $3 and pattern = $4";
        let query = sqlx::query(sql).bind(account_id).bind(machine_id);
        query.bind(kind).bind(pattern).execute(&self.pool).await?;
        Ok(())
    }

    // ---- Reverse-exec: enrolled machine daemons (token id only) and the audit log. ----

    /// Enrol (or re-enrol) a machine's daemon: store its token id, clear any prior revocation.
    /// RELAY OFF STICKS (review of #342): a new machine is enrolled off while every un-revoked one
    /// of its account is off, or enrolling it would turn back on the relay its person turned off;
    /// on with any one on, or none. One enrolled again keeps its own switch, unless it was revoked:
    /// then it is a new one (review of #344), or one revoked while on would turn the relay back on.
    pub async fn enrol_daemon(
        &self,
        account_id: &str,
        machine_id: &str,
        label: &str,
        jti: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        let sql = "insert into local_exec_daemon as d
               (account_id, machine_id, label, jti, enrolled_at_ms, revoked, relay_enabled)
             values ($1, $2, $3, $4, $5, false, coalesce((select bool_or(relay_enabled)
               from local_exec_daemon where account_id = $1 and not revoked), true))
             on conflict (account_id, machine_id) do update set label = excluded.label,
               jti = excluded.jti, enrolled_at_ms = excluded.enrolled_at_ms, revoked = false,
               relay_enabled = case when d.revoked then excluded.relay_enabled
                 else d.relay_enabled end";
        let mut tx = self.relay_lock(account_id).await?;
        let query = sqlx::query(sql).bind(account_id).bind(machine_id);
        let query = query.bind(label).bind(jti).bind(at_ms);
        query.execute(&mut *tx).await?;
        Ok(tx.commit().await?)
    }

    /// The daemon's token id, whether it is revoked and whether its relay is on, for its token.
    pub async fn daemon_jti(
        &self,
        account_id: &str,
        machine_id: &str,
    ) -> StoreResult<Option<(String, bool, bool)>> {
        let sql = "select jti, revoked, relay_enabled from local_exec_daemon
                   where account_id = $1 and machine_id = $2";
        let row = sqlx::query_as(sql).bind(account_id).bind(machine_id);
        Ok(row.fetch_optional(&self.pool).await?)
    }

    pub async fn revoke_daemon(&self, account_id: &str, machine_id: &str) -> StoreResult<()> {
        let sql = "update local_exec_daemon set revoked = true
                   where account_id = $1 and machine_id = $2";
        let mut tx = self.relay_lock(account_id).await?;
        let query = sqlx::query(sql).bind(account_id).bind(machine_id);
        query.execute(&mut *tx).await?;
        Ok(tx.commit().await?)
    }

    /// The account's enrolled machines, newest first.
    pub async fn list_daemons(&self, account_id: &str) -> StoreResult<Vec<Daemon>> {
        let sql = "select machine_id, label, enrolled_at_ms, revoked, relay_enabled
                   from local_exec_daemon where account_id = $1 order by enrolled_at_ms desc";
        let rows = sqlx::query_as(sql).bind(account_id);
        Ok(rows.fetch_all(&self.pool).await?)
    }

    /// One of `owner`'s machines with its relay switched, as it then is; `None` for one `owner`
    /// has not enrolled, which another account's is too. A revoked one is left as it was.
    pub async fn set_relay(&self, owner: &str, id: &str, on: bool) -> StoreResult<Option<Daemon>> {
        let sql = "update local_exec_daemon
                   set relay_enabled = case when revoked then relay_enabled else $3 end
                   where account_id = $1 and machine_id = $2
                   returning machine_id, label, enrolled_at_ms, revoked, relay_enabled";
        let row = sqlx::query_as(sql).bind(owner).bind(id).bind(on);
        Ok(row.fetch_optional(&self.pool).await?)
    }

    /// Every one of the account's machines switched at once, revoked ones too: the ids switched.
    /// A revoked one's switch is only listed; enrolled again, it is set as a new one's is.
    pub async fn set_relays(&self, account_id: &str, on: bool) -> StoreResult<Vec<String>> {
        let sql = "update local_exec_daemon set relay_enabled = $2 where account_id = $1
                   returning machine_id";
        let mut tx = self.relay_lock(account_id).await?;
        let rows = sqlx::query_scalar(sql).bind(account_id).bind(on);
        let switched = rows.fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(switched)
    }

    /// A transaction holding the account's relay lock (review of #344). Under READ COMMITTED an
    /// enrolment read the others' switches in a snapshot taken as its insert began, so a switch of
    /// every computer committing meanwhile missed the new row, and a revoke of the computer being
    /// enrolled again let it count its own switch: either left it on beside every other off. The
    /// lock is its own statement, before the read, or the snapshot would predate it. One
    /// computer's switch (`set_relay`) takes none: it reads and writes its own row only, so any
    /// order it lands in beside an enrolment is one they could have run in turn.
    async fn relay_lock(&self, account_id: &str) -> StoreResult<Transaction<'static, Postgres>> {
        let mut tx = self.pool.begin().await?;
        let lock = sqlx::query("select pg_advisory_xact_lock($1, hashtext($2))");
        let lock = lock.bind(RELAY_LOCK_CLASS).bind(account_id);
        lock.execute(&mut *tx).await?;
        Ok(tx)
    }

    /// Write an audit row at enqueue time (before the command runs).
    #[allow(clippy::too_many_arguments)]
    pub async fn audit_local_exec(
        &self,
        id: &str,
        account_id: &str,
        machine_id: &str,
        origin: &str,
        command: &str,
        decision: &str,
        rule: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<()> {
        let sql = "insert into local_exec_audit
               (id, account_id, machine_id, origin, command, decision, rule, requested_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8)";
        let query = sqlx::query(sql).bind(id).bind(account_id).bind(machine_id);
        let query = query.bind(origin).bind(command).bind(decision);
        query.bind(rule).bind(at_ms).execute(&self.pool).await?;
        Ok(())
    }

    /// Record a command's result on its audit row: the ShellResult `outcome` case (success /
    /// failure / timeout / rejected / spawnError / permissionDenied, or `offline`) and, when there
    /// is one, the process exit code. A refusal is a case with no exit code, not a non-zero exit.
    pub async fn finish_local_exec_audit(
        &self,
        id: &str,
        outcome: &str,
        exit_code: Option<i32>,
        at_ms: i64,
    ) -> StoreResult<()> {
        let sql = "update local_exec_audit
                   set outcome = $2, exit_code = $3, finished_at_ms = $4 where id = $1";
        let query = sqlx::query(sql).bind(id).bind(outcome).bind(exit_code);
        query.bind(at_ms).execute(&self.pool).await?;
        Ok(())
    }

    /// The account's recent audit rows, newest first (all machines).
    pub async fn local_exec_audit_log(
        &self,
        account_id: &str,
        limit: i64,
    ) -> StoreResult<Vec<serde_json::Value>> {
        // Built in SQL so the keys are the reply's; null where a row has no value.
        Ok(sqlx::query_scalar(
            "select json_build_object('id', id, 'machineId', machine_id, 'origin', origin,
                    'command', command, 'decision', decision, 'rule', rule,
                    'requestedAtMs', requested_at_ms, 'outcome', outcome,
                    'exitCode', exit_code, 'finishedAtMs', finished_at_ms)
             from local_exec_audit where account_id = $1
             order by requested_at_ms desc limit $2",
        )
        .bind(account_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    // ---- A computer keyed by the scope that shares it (org / account / bot) ----

    /// Record `box_id`, just made for this scope, as its computer: in place of `replacing` while
    /// the scope still names that box, else only into a scope with none. Answers the scope's box
    /// afterwards, another request's when that one got there first. NEVER AN UPSERT (#302): the
    /// later of two first creates overwrote the earlier, which ran on untracked and billed. A
    /// no-op `do update`, as `do nothing` answers a conflict with no row to say who won.
    #[allow(clippy::too_many_arguments)]
    pub async fn claim_scoped_computer(
        &self,
        scope: &str,
        scope_id: &str,
        replacing: Option<&str>,
        box_id: &str,
        kind: &str,
        org_id: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<Option<(String, String)>> {
        let sql = if replacing.is_some() {
            "update scoped_computer set box_id = $3, kind = $4, org_id = $5, last_used_at_ms = $6,
               updated_at_ms = $6, stopped = false where scope = $1 and scope_id = $2 and box_id = $7 returning box_id, kind"
        } else {
            "insert into scoped_computer (scope, scope_id, box_id, kind, org_id, last_used_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $6) on conflict (scope, scope_id)
             do update set box_id = scoped_computer.box_id returning box_id, kind"
        };
        let mut query = sqlx::query_as(sql)
            .bind(scope)
            .bind(scope_id)
            .bind(box_id)
            .bind(kind)
            .bind(org_id)
            .bind(at_ms);
        if let Some(replacing) = replacing {
            query = query.bind(replacing);
        }
        if let Some(kept) = query.fetch_optional(&self.pool).await? {
            return Ok(Some(kept));
        }
        let now = self.scoped_computer_full(scope, scope_id).await?;
        Ok(now.map(|(box_id, kind, _)| (box_id, kind)))
    }

    /// Every recorded box of one provider kind, for an admin's "update all" and its counts.
    pub async fn scoped_computers_of_kind(
        &self,
        kind: &str,
    ) -> StoreResult<Vec<(String, String, String, Option<String>)>> {
        let rows = sqlx::query(
            "select scope, scope_id, box_id, org_id from scoped_computer where kind = $1",
        )
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("scope")?,
                    row.try_get::<String, _>("scope_id")?,
                    row.try_get::<String, _>("box_id")?,
                    row.try_get::<Option<String>, _>("org_id")?,
                ))
            })
            .collect()
    }

    /// Start (or restart) the update record for a scope: phase set, clock reset, error cleared.
    pub async fn begin_box_update(
        &self,
        scope: &str,
        scope_id: &str,
        phase: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into box_update (scope, scope_id, phase, started_at_ms, updated_at_ms, error)
             values ($1, $2, $3, $4, $4, null)
             on conflict (scope, scope_id) do update set
               phase = excluded.phase, started_at_ms = excluded.started_at_ms,
               updated_at_ms = excluded.updated_at_ms, error = null",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(phase)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Move an update along, or end it in failure with the reason.
    pub async fn set_box_update_phase(
        &self,
        scope: &str,
        scope_id: &str,
        phase: &str,
        error: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update box_update set phase = $3, error = $4, updated_at_ms = $5
             where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(phase)
        .bind(error)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `(phase, started_at_ms, updated_at_ms, error)` for a scope's update, if one is recorded.
    pub async fn box_update(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<(String, i64, i64, Option<String>)>> {
        let row = sqlx::query(
            "select phase, started_at_ms, updated_at_ms, error from box_update
             where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("phase")?,
                row.try_get::<i64, _>("started_at_ms")?,
                row.try_get::<i64, _>("updated_at_ms")?,
                row.try_get::<Option<String>, _>("error")?,
            ))
        })
        .transpose()
    }

    pub async fn clear_box_update(&self, scope: &str, scope_id: &str) -> StoreResult<()> {
        let clear = sqlx::query("delete from box_update where scope = $1 and scope_id = $2");
        clear.bind(scope).bind(scope_id).execute(&self.pool).await?;
        Ok(())
    }

    /// The scope row that claims `box_id` now — (scope, scope_id, kind, org_id). A shared
    /// computer is recorded under its own scope, so this is how a document session's stored
    /// box resolves to the provider that can say whether the box still exists; a box no
    /// scope claims was replaced, and the document's own scope's kind answers instead.
    pub async fn scoped_computer_of_box(
        &self,
        box_id: &str,
    ) -> StoreResult<Option<(String, String, String, Option<String>)>> {
        let row = sqlx::query(
            "select scope, scope_id, kind, org_id from scoped_computer where box_id = $1",
        )
        .bind(box_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("scope")?,
                row.try_get::<String, _>("scope_id")?,
                row.try_get::<String, _>("kind")?,
                row.try_get::<Option<String>, _>("org_id")?,
            ))
        })
        .transpose()
    }

    /// A scoped computer with its idle state — (box_id, kind, stopped).
    pub async fn scoped_computer_full(
        &self,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<Option<(String, String, bool)>> {
        let row = sqlx::query(
            "select box_id, kind, stopped from scoped_computer where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("box_id")?,
                row.try_get::<String, _>("kind")?,
                row.try_get::<bool, _>("stopped")?,
            ))
        })
        .transpose()
    }

    /// Mark a scoped computer used now (and not stopped) — called on the run path.
    pub async fn mark_scoped_used(
        &self,
        scope: &str,
        scope_id: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update scoped_computer set last_used_at_ms = $3, stopped = false where scope = $1 and scope_id = $2",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_scoped_stopped(&self, scope: &str, scope_id: &str) -> StoreResult<()> {
        sqlx::query("update scoped_computer set stopped = true where scope = $1 and scope_id = $2")
            .bind(scope)
            .bind(scope_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Running boxes idle since before `before_ms` — the sweep stops these. Returns
    /// (scope, scope_id, box_id, kind). A box never used yet (null last_used) is left alone.
    #[allow(clippy::type_complexity)]
    pub async fn idle_scoped_computers(
        &self,
        before_ms: i64,
    ) -> StoreResult<Vec<(String, String, String, String, Option<String>)>> {
        let rows = sqlx::query(
            "select scope, scope_id, box_id, kind, org_id from scoped_computer
             where stopped = false and last_used_at_ms is not null and last_used_at_ms < $1",
        )
        .bind(before_ms)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("scope")?,
                    row.try_get::<String, _>("scope_id")?,
                    row.try_get::<String, _>("box_id")?,
                    row.try_get::<String, _>("kind")?,
                    row.try_get::<Option<String>, _>("org_id")?,
                ))
            })
            .collect()
    }

    /// Forget the scope's box only while it is `box_id`: a box recorded since the caller read the
    /// row is the live one, and forgetting it left that box running untracked (#302).
    pub async fn clear_scoped_computer(
        &self,
        scope: &str,
        scope_id: &str,
        box_id: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "delete from scoped_computer where scope = $1 and scope_id = $2 and box_id = $3",
        )
        .bind(scope)
        .bind(scope_id)
        .bind(box_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every coworker row (box, coworker, account) naming a box no computer row records, by box.
    /// One snapshot: a box is recorded before a coworker is given it and a replaced record is
    /// never written again, so none of these is about to be recorded.
    pub async fn unrecorded_coworker_boxes(&self) -> StoreResult<Vec<(String, String, String)>> {
        Ok(sqlx::query_as(
            "select c.box_id, c.id, c.account_id from coworker_view c
              where c.box_id is not null
                and not exists (select 1 from scoped_computer s where s.box_id = c.box_id)
                and not exists (select 1 from account_computer a where a.box_id = c.box_id)
              order by c.box_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    // ---- The account's last computer-provisioning error ----

    pub async fn set_account_computer_error(
        &self,
        account_id: &str,
        code: &str,
        message: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into account_computer_error (account_id, code, message, updated_at_ms)
             values ($1, $2, $3, $4)
             on conflict (account_id) do update set
               code = excluded.code, message = excluded.message, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(account_id)
        .bind(code)
        .bind(message)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The account's last provisioning error as (code, message, updated_at_ms), or None when it
    /// has none. The STAMP is part of the answer: without it an hours-old refusal is
    /// indistinguishable from one a second old, which is exactly how a stale `quota_exceeded` was
    /// read as the live state of a box on 5 Sep 2026.
    pub async fn account_computer_error(
        &self,
        account_id: &str,
    ) -> StoreResult<Option<(String, String, i64)>> {
        let row = sqlx::query(
            "select code, message, updated_at_ms from account_computer_error where account_id = $1",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get::<String, _>("code")?,
                row.try_get::<String, _>("message")?,
                row.try_get::<i64, _>("updated_at_ms")?,
            ))
        })
        .transpose()
    }

    pub async fn clear_account_computer_error(&self, account_id: &str) -> StoreResult<()> {
        sqlx::query("delete from account_computer_error where account_id = $1")
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Which computer kinds this org has a secret row for — the names only, never the secrets.
    /// A row can exist and still be unreadable (KEK rotated); callers that advertise "configured"
    /// to a person must use `org_computer_kinds_openable` so a dead blob is not a live computer.
    pub async fn org_computer_kinds(&self, org_id: &str) -> StoreResult<Vec<String>> {
        let prefix = format!("org-computer:{org_id}:");
        let rows = sqlx::query("select id from secret_store where id like $1")
            .bind(format!("{prefix}%"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.try_get::<String, _>("id").ok())
            .filter_map(|id| id.strip_prefix(&prefix).map(str::to_string))
            .collect())
    }

    /// Kinds whose secrets actually open with this vault. A ciphertext sealed under a lost KEK
    /// is not configured — listing it as ready is how a dead key looked like a live computer.
    pub async fn org_computer_kinds_openable(
        &self,
        vault: &Vault,
        org_id: &str,
    ) -> StoreResult<Vec<String>> {
        let mut openable = Vec::new();
        for kind in self.org_computer_kinds(org_id).await? {
            if matches!(
                self.org_computer_secret(vault, org_id, &kind).await,
                Ok(Some(_))
            ) {
                openable.push(kind);
            }
        }
        Ok(openable)
    }
}
