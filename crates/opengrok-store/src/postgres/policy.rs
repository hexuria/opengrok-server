//! Policy: who may make which coworker do what.

use super::*;

/// Policy: who may make which coworker do what.
impl PgStore {
    /// Everything the policy engine needs for one principal and one coworker.
    ///
    /// A row that is missing comes back as `None` inside the context, and every `None` denies —
    /// so a lookup that finds nothing is a refusal, never a default-allow.
    ///
    /// STRICT: this principal's own grant row and nothing else. It is what "may change this
    /// grant" (`set_approvals`), "may attach a connection" and "may fire a routine" ask, and
    /// widening it to shared coworkers would let a member mint a grant of their own. The question
    /// "may this person talk to it on this turn" is `policy_to_use`.
    pub async fn policy_for(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<opengrok_policy::Context> {
        let grant = sqlx::query(
            "select profile, needs_approval, revoked from grant_view
             where principal_id = $1 and coworker_id = $2",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?
        .map(|row| grant_from_row(&row, principal, coworker))
        .transpose()?;
        let ceiling = self.ceiling_of(coworker).await?;
        Ok(opengrok_policy::Context { grant, ceiling })
    }

    /// The policy a TURN runs under: this principal's own grant if they hold one, else — when the
    /// coworker is shared with the org they are in — its owner's grant, addressed to them.
    ///
    /// Derived on every read rather than written as a grant row per member when the owner
    /// shares, because a copied row goes stale three ways: an org-mate who joins later gets
    /// nothing, one who leaves keeps a live grant, and switching back to private has to find and
    /// revoke every copy. Read here, unsharing, leaving the org and the owner's own revocation
    /// all take effect on the next turn (CLAUDE.md #6), and the member's access is the owner's —
    /// never wider, `revoked` included.
    ///
    /// An own row wins even when revoked: a principal who was explicitly cut off must not walk
    /// back in through the org. Nothing found is an empty context, which denies.
    pub async fn policy_to_use(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<opengrok_policy::Context> {
        // Three statements, each over tables in the order `migrations::run` locks them, never
        // one statement over `grant_view` then `coworker_view`. The schema runs as ONE
        // transaction whose `alter table … if not exists` takes ACCESS EXCLUSIVE on
        // `coworker_view` and later `grant_view`, and holds both until it commits. A join that
        // locked them the other way round deadlocked against a concurrent boot (another replica,
        // or another test's harness). Postgres killed the read, and the turn was refused with
        // "no grant" over a grant that was fine.
        let own = self.policy_for(principal, coworker).await?;
        if own.grant.is_some() {
            return Ok(own);
        }
        let Some(owner) = self.org_mates_coworker(principal, coworker).await? else {
            return Ok(own);
        };
        let grant = sqlx::query(
            "select profile, needs_approval, revoked from grant_view
             where principal_id = $1 and coworker_id = $2",
        )
        .bind(owner.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?
        .map(|row| grant_from_row(&row, principal, coworker))
        .transpose()?;
        Ok(opengrok_policy::Context {
            grant,
            ceiling: own.ceiling,
        })
    }

    /// The owner of `coworker` when it is somebody else's, live, shared with the org, and that
    /// org is `principal`'s (non-empty) one; `None` otherwise. `roster_for`'s sharing branch,
    /// over `coworker_view` then `account_view` — the order the schema locks them.
    async fn org_mates_coworker(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<Option<AccountId>> {
        let row = sqlx::query(
            "select c.account_id
             from coworker_view c
             join account_view owner on owner.id = c.account_id
             join account_view caller on caller.id = $1
             where c.id = $2
               and c.account_id <> $1
               and c.retired = false
               and c.visibility = 'org'
               and coalesce(owner.org_id, '') <> ''
               and owner.org_id = caller.org_id",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok::<_, StoreError>(AccountId::from_stored(
                row.try_get::<String, _>("account_id")?,
            ))
        })
        .transpose()
    }

    async fn ceiling_of(
        &self,
        coworker: &CoworkerId,
    ) -> StoreResult<Option<opengrok_policy::Ceiling>> {
        sqlx::query("select tools from ceiling_view where coworker_id = $1")
            .bind(coworker.as_str())
            .fetch_optional(&self.pool)
            .await?
            .map(|row| {
                let tools: serde_json::Value = row.try_get("tools")?;
                Ok::<_, StoreError>(opengrok_policy::Ceiling {
                    coworker: coworker.clone(),
                    tools: serde_json::from_value(tools).unwrap_or(opengrok_policy::ToolSet::None),
                })
            })
            .transpose()
    }

    /// Record a grant and a ceiling together.
    ///
    /// Together because a grant without a ceiling can run nothing, and writing them separately
    /// leaves a window where a coworker exists that its own owner cannot use.
    pub async fn grant_access(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
        profile: &opengrok_policy::ToolSet,
        ceiling: &opengrok_policy::ToolSet,
        needs_approval: &opengrok_policy::ToolSet,
        at_ms: i64,
    ) -> StoreResult<()> {
        let profile_json = serde_json::to_value(profile)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let approval_json = serde_json::to_value(needs_approval)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let ceiling_json = serde_json::to_value(ceiling)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into grant_view
               (principal_id, coworker_id, profile, needs_approval, revoked, updated_at_ms)
             values ($1, $2, $3, $4, false, $5)
             on conflict (principal_id, coworker_id) do update set
               profile = excluded.profile,
               needs_approval = excluded.needs_approval,
               revoked = false,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .bind(profile_json)
        .bind(approval_json)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "insert into ceiling_view (coworker_id, tools, updated_at_ms)
             values ($1, $2, $3)
             on conflict (coworker_id) do update set
               tools = excluded.tools,
               version = ceiling_view.version + (ceiling_view.tools <> excluded.tools)::int,
               updated_at_ms = excluded.updated_at_ms",
        )
        .bind(coworker.as_str())
        .bind(ceiling_json)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// A coworker's ceiling and the `version` a write against it names, read together so the
    /// number is the tools' own. No row is `None` at version 0: it admits nothing.
    pub async fn ceiling_at(
        &self,
        coworker: &CoworkerId,
    ) -> StoreResult<(opengrok_policy::ToolSet, i64)> {
        let row = sqlx::query("select tools, version from ceiling_view where coworker_id = $1")
            .bind(coworker.as_str())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok((opengrok_policy::ToolSet::None, 0));
        };
        let tools: serde_json::Value = row.try_get("tools")?;
        let tools = serde_json::from_value(tools).unwrap_or(opengrok_policy::ToolSet::None);
        Ok((tools, row.try_get("version")?))
    }

    /// Set a coworker's ceiling as its owner chose it, and the owner's profile equal to it as hire
    /// writes them, only while the ceiling is still at `expected` (whatever it is, for `None`).
    /// `Ok(None)` when it has moved on, and nothing is written. ONE CONDITIONAL WRITE, not a read
    /// then a write: the version is compared on the locked row, so two writes made from the same
    /// read cannot both land. The grant goes first, the order `grant_access` locks them in. Only
    /// a save that changes the tools marks the ceiling `chosen`: one that changes nothing leaves
    /// it following the built-ins as before.
    pub async fn set_ceiling(
        &self,
        owner: &AccountId,
        coworker: &CoworkerId,
        tools: &opengrok_policy::ToolSet,
        expected: Option<i64>,
        at_ms: i64,
    ) -> StoreResult<Option<i64>> {
        let tools =
            serde_json::to_value(tools).map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "update grant_view set profile = $3, updated_at_ms = $4
             where principal_id = $1 and coworker_id = $2",
        )
        .bind(owner.as_str())
        .bind(coworker.as_str())
        .bind(&tools)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;
        let version: Option<i64> = sqlx::query_scalar(
            "insert into ceiling_view (coworker_id, tools, updated_at_ms, version, chosen)
             values ($1, $2, $3, 1, true)
             on conflict (coworker_id) do update set
               tools = excluded.tools, updated_at_ms = excluded.updated_at_ms,
               chosen = ceiling_view.chosen or ceiling_view.tools <> excluded.tools,
               version = ceiling_view.version + (ceiling_view.tools <> excluded.tools)::int
             where $4::bigint is null or ceiling_view.version = $4
             returning version",
        )
        .bind(coworker.as_str())
        .bind(&tools)
        .bind(at_ms)
        .bind(expected)
        .fetch_optional(&mut *tx)
        .await?;
        // Dropped without a commit, the profile write above is rolled back with it.
        if version.is_some() {
            tx.commit().await?;
        }
        Ok(version)
    }

    /// Change only what needs a human yes, on this principal's own live grant as it is NOW.
    /// Nothing else of the grant and nothing of the ceiling is written: a write built from what a
    /// request read puts back whatever changed since it read, and that undid an owner's ceiling
    /// save (#268). `false` when there is no live grant to change.
    pub async fn set_needs_approval(
        &self,
        principal: &AccountId,
        coworker: &CoworkerId,
        needs_approval: &opengrok_policy::ToolSet,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let needs_approval = serde_json::to_value(needs_approval)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let written = sqlx::query(
            "update grant_view set needs_approval = $3, updated_at_ms = $4
             where principal_id = $1 and coworker_id = $2 and not revoked",
        )
        .bind(principal.as_str())
        .bind(coworker.as_str())
        .bind(needs_approval)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(written.rows_affected() == 1)
    }
}

/// A grant row, addressed to `principal`. For a shared coworker the row is the owner's and the
/// principal the member: `decide` checks the grant names who is asking, and the member is.
fn grant_from_row(
    row: &sqlx::postgres::PgRow,
    principal: &AccountId,
    coworker: &CoworkerId,
) -> StoreResult<opengrok_policy::Grant> {
    let profile: serde_json::Value = row.try_get("profile")?;
    let needs_approval: serde_json::Value = row.try_get("needs_approval")?;
    Ok(opengrok_policy::Grant {
        principal: principal.clone(),
        coworker: coworker.clone(),
        // An unreadable profile becomes `None` — the narrowest reading, per the rule that a typo
        // may only ever narrow access.
        profile: serde_json::from_value(profile).unwrap_or(opengrok_policy::ToolSet::None),
        // An unreadable approval list becomes `All`, which is the NARROW reading here: every tool
        // then needs a human yes. The direction flips because this field restricts rather than
        // grants, and a typo must still only ever narrow.
        needs_approval: serde_json::from_value(needs_approval)
            .unwrap_or(opengrok_policy::ToolSet::All),
        revoked: row.try_get("revoked")?,
    })
}
