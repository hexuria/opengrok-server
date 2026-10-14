//! Recipes: taught tasks, their versions, who they are shared with, which bots run them.

use super::*;

// ---- Recipes: taught tasks, their versions, who they are shared with, which bots run them ----

/// A recipe as a list shows it, with the caller's relation to it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeRow {
    pub id: String,
    pub owner_id: String,
    pub org_id: Option<String>,
    pub name: String,
    pub description: String,
    pub screen_w: i32,
    pub screen_h: i32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
    /// The newest version number.
    pub latest_version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeVersionRow {
    pub recipe_id: String,
    pub version: i32,
    pub kind: String,
    pub body: serde_json::Value,
    pub note: String,
    pub created_by: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeShareRow {
    pub recipe_id: String,
    pub scope: String,
    pub scope_id: String,
    pub granted_by: String,
    pub granted_at_ms: i64,
    pub accepted_at_ms: Option<i64>,
    pub declined_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeGrantRow {
    pub recipe_id: String,
    pub coworker_id: String,
    pub granted_by: String,
    pub granted_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeRunRow {
    pub id: String,
    pub recipe_id: String,
    pub version: i32,
    pub coworker_id: String,
    pub run_id: Option<String>,
    pub ok: bool,
    pub stopped_at: Option<i32>,
    pub receipt: serde_json::Value,
    /// When the run was written: when it started, for one the server played.
    pub at_ms: i64,
    /// Set while the run is playing and renewed as it plays; null once it has finished.
    #[serde(default)]
    pub lease_until_ms: Option<i64>,
}

impl RecipeRunRow {
    /// `finished`, `running`, or `interrupted` — a row whose lease ran out with nobody finishing
    /// it, because the process playing it stopped. What the box did then is not known.
    pub fn state(&self, now_ms: i64) -> &'static str {
        match self.lease_until_ms {
            None => "finished",
            Some(until) if until > now_ms => "running",
            Some(_) => "interrupted",
        }
    }
}

pub(super) fn recipe_run_row(row: &sqlx::postgres::PgRow) -> StoreResult<RecipeRunRow> {
    Ok(RecipeRunRow {
        id: row.try_get("id")?,
        recipe_id: row.try_get("recipe_id")?,
        version: row.try_get("version")?,
        coworker_id: row.try_get("coworker_id")?,
        run_id: row.try_get("run_id")?,
        ok: row.try_get("ok")?,
        stopped_at: row.try_get("stopped_at")?,
        receipt: row.try_get("receipt")?,
        at_ms: row.try_get("at_ms")?,
        lease_until_ms: row.try_get("lease_until_ms")?,
    })
}

fn recipe_row(row: &sqlx::postgres::PgRow) -> StoreResult<RecipeRow> {
    Ok(RecipeRow {
        id: row.try_get("id")?,
        owner_id: row.try_get("owner_id")?,
        org_id: row.try_get("org_id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        screen_w: row.try_get("screen_w")?,
        screen_h: row.try_get("screen_h")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        deleted_at_ms: row.try_get("deleted_at_ms")?,
        latest_version: row
            .try_get::<Option<i32>, _>("latest_version")?
            .unwrap_or(0),
    })
}

/// One `recipe_version` row, read the same way wherever it is read.
fn recipe_version_row(row: &sqlx::postgres::PgRow) -> StoreResult<RecipeVersionRow> {
    Ok(RecipeVersionRow {
        recipe_id: row.try_get("recipe_id")?,
        version: row.try_get("version")?,
        kind: row.try_get("kind")?,
        body: row.try_get("body")?,
        note: row.try_get("note")?,
        created_by: row.try_get("created_by")?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}

/// The first key of `start_recipe_run`'s per-bot lock ("RECP"); the second is the bot id's hash.
const RECIPE_LEASE_LOCK_CLASS: i32 = 0x5245_4350;

const RECIPE_SELECT: &str =
    "select r.id, r.owner_id, r.org_id, r.name, r.description, r.screen_w, r.screen_h,
        r.created_at_ms, r.updated_at_ms, r.deleted_at_ms,
        (select max(version) from recipe_version v where v.recipe_id = r.id) as latest_version
   from recipe r";

/// A grant counts only while whoever made it still holds the recipe: its owner, or a person whose
/// own share row is accepted — the question `recipe_accepted_by` answers, asked of `granted_by`.
///
/// ASKED ON EVERY READ, not only when the grant is made. Unshare and decline delete the grants they
/// end, but a turn's offers are rebuilt from this read every turn, and a grant a cleanup missed
/// (a row lost some other way, a path added later) must fail closed rather than keep a former
/// recipient's bot playing the owner's newest edits. `g` and `r` are the grant and its recipe.
const GRANT_STILL_HELD: &str = "(g.granted_by = r.owner_id or exists (
        select 1 from recipe_share s
         where s.recipe_id = r.id and s.scope = 'account' and s.scope_id = g.granted_by
           and s.accepted_at_ms is not null))";

/// The grants one person made on one recipe, never its owner's: the owner cannot lose access to
/// their own recipe, so an answer or unshare naming them has nothing of theirs to take.
const DROP_GRANTS_MADE_BY: &str = "delete from recipe_grant g using recipe r
     where g.recipe_id = $1 and r.id = g.recipe_id and g.granted_by = $2 and r.owner_id <> $2";

impl PgStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn create_recipe(
        &self,
        id: &str,
        owner_id: &str,
        org_id: Option<&str>,
        name: &str,
        description: &str,
        screen: (i32, i32),
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe (id, owner_id, org_id, name, description, screen_w, screen_h, created_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
        )
        .bind(id)
        .bind(owner_id)
        .bind(org_id)
        .bind(name)
        .bind(description)
        .bind(screen.0)
        .bind(screen.1)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn rename_recipe(
        &self,
        id: &str,
        name: &str,
        description: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update recipe set name = $2, description = $3, updated_at_ms = $4 where id = $1",
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn soft_delete_recipe(&self, id: &str, at_ms: i64) -> StoreResult<()> {
        sqlx::query("update recipe set deleted_at_ms = $2, updated_at_ms = $2 where id = $1")
            .bind(id)
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recipe(&self, id: &str) -> StoreResult<Option<RecipeRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT} where r.id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(recipe_row).transpose()
    }

    /// The recipes a person owns (not deleted).
    pub async fn recipes_owned_by(&self, owner_id: &str) -> StoreResult<Vec<RecipeRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT} where r.owner_id = $1 and r.deleted_at_ms is null order by r.updated_at_ms desc"
        )))
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(recipe_row).collect()
    }

    /// Recipes shared to this person directly or to their org, each with its share row for
    /// them (accepted, pending or declined). Excludes what they own and what is deleted.
    pub async fn recipes_shared_with(
        &self,
        account_id: &str,
        org_id: Option<&str>,
    ) -> StoreResult<Vec<(RecipeRow, RecipeShareRow)>> {
        // The recipe's columns and the share's, one row per share.
        let select = RECIPE_SELECT.replace(
            "   from recipe r",
            ", s.scope, s.scope_id, s.granted_by, s.granted_at_ms, s.accepted_at_ms, s.declined_at_ms
   from recipe r",
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{select}
              join recipe_share s on s.recipe_id = r.id
             where r.deleted_at_ms is null and r.owner_id <> $1
               and ((s.scope = 'account' and s.scope_id = $1)
                 or (s.scope = 'org' and $2 is not null and s.scope_id <> '' and s.scope_id = $2))
             order by r.updated_at_ms desc"
        )))
        .bind(account_id)
        // AN EMPTY ORG IS NO ORG, and it is checked on BOTH sides. `unwrap_or("")` used to
        // collapse "no org" into an org id of `''`, which matched any `recipe_share` row written
        // with the same empty id — every person in no org would have been shown every other
        // orgless person's shared recipes. A NULL matches nothing, and `scope_id <> ''`
        // neutralises a row an older build may already have written.
        .bind(org_id.filter(|org| !org.is_empty()))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for row in &rows {
            out.push((
                recipe_row(row)?,
                RecipeShareRow {
                    recipe_id: row.try_get("id")?,
                    scope: row.try_get("scope")?,
                    scope_id: row.try_get("scope_id")?,
                    granted_by: row.try_get("granted_by")?,
                    granted_at_ms: row.try_get("granted_at_ms")?,
                    accepted_at_ms: row.try_get("accepted_at_ms")?,
                    declined_at_ms: row.try_get("declined_at_ms")?,
                },
            ));
        }
        Ok(out)
    }

    /// What an org's admin sees: every recipe shared org-wide, with how many members accepted.
    pub async fn recipes_shared_to_org(&self, org_id: &str) -> StoreResult<Vec<(RecipeRow, i64)>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT}
              join recipe_share s on s.recipe_id = r.id and s.scope = 'org' and s.scope_id = $1
             where r.deleted_at_ms is null
             order by r.updated_at_ms desc"
        )))
        .bind(org_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for row in &rows {
            let recipe = recipe_row(row)?;
            let accepted: i64 = sqlx::query_scalar(
                "select count(*) from recipe_share
                  where recipe_id = $1 and scope = 'account' and granted_by = 'org:' || $2
                    and accepted_at_ms is not null",
            )
            .bind(&recipe.id)
            .bind(org_id)
            .fetch_one(&self.pool)
            .await?;
            out.push((recipe, accepted));
        }
        Ok(out)
    }

    pub async fn add_recipe_version(
        &self,
        recipe_id: &str,
        kind: &str,
        body: &serde_json::Value,
        note: &str,
        created_by: &str,
        at_ms: i64,
    ) -> StoreResult<i32> {
        let mut tx = self.pool.begin().await?;
        let next: i32 = sqlx::query_scalar(
            "select coalesce(max(version), 0) + 1 from recipe_version where recipe_id = $1",
        )
        .bind(recipe_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "insert into recipe_version (recipe_id, version, kind, body, note, created_by, created_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(recipe_id)
        .bind(next)
        .bind(kind)
        .bind(body)
        .bind(note)
        .bind(created_by)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update recipe set updated_at_ms = $2 where id = $1")
            .bind(recipe_id)
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(next)
    }

    /// Drop one version and the runs that played it. The runs go with it because history is
    /// read a version at a time: a run pointing at a version nobody can open is a row with no
    /// home. Returns what was deleted, so a caller can tell "gone" from "was never there".
    pub async fn delete_recipe_version(
        &self,
        recipe_id: &str,
        version: i32,
    ) -> StoreResult<Option<RecipeVersionRow>> {
        let Some(row) = self.recipe_version(recipe_id, version).await? else {
            return Ok(None);
        };
        sqlx::query("delete from recipe_run where recipe_id = $1 and version = $2")
            .bind(recipe_id)
            .bind(version)
            .execute(&self.pool)
            .await?;
        sqlx::query("delete from recipe_version where recipe_id = $1 and version = $2")
            .bind(recipe_id)
            .bind(version)
            .execute(&self.pool)
            .await?;
        Ok(Some(row))
    }

    /// One version of a recipe, by its number.
    pub async fn recipe_version(
        &self,
        recipe_id: &str,
        version: i32,
    ) -> StoreResult<Option<RecipeVersionRow>> {
        let row = sqlx::query(
            "select recipe_id, version, kind, body, note, created_by, created_at_ms
               from recipe_version where recipe_id = $1 and version = $2",
        )
        .bind(recipe_id)
        .bind(version)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(recipe_version_row).transpose()
    }

    pub async fn recipe_versions(&self, recipe_id: &str) -> StoreResult<Vec<RecipeVersionRow>> {
        let rows = sqlx::query(
            "select recipe_id, version, kind, body, note, created_by, created_at_ms
               from recipe_version where recipe_id = $1 order by version",
        )
        .bind(recipe_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(recipe_version_row).collect()
    }

    /// The version a run should use: the newest that is not the raw tape.
    pub async fn recipe_runnable_version(
        &self,
        recipe_id: &str,
    ) -> StoreResult<Option<RecipeVersionRow>> {
        Ok(self
            .recipe_versions(recipe_id)
            .await?
            .into_iter()
            .rfind(|version| version.kind != "raw"))
    }

    pub async fn share_recipe(
        &self,
        recipe_id: &str,
        scope: &str,
        scope_id: &str,
        granted_by: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe_share (recipe_id, scope, scope_id, granted_by, granted_at_ms)
             values ($1, $2, $3, $4, $5)
             on conflict (recipe_id, scope, scope_id) do update set
               granted_by = excluded.granted_by, granted_at_ms = excluded.granted_at_ms,
               accepted_at_ms = null, declined_at_ms = null",
        )
        .bind(recipe_id)
        .bind(scope)
        .bind(scope_id)
        .bind(granted_by)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Take a share back, and the grants the people losing it made to their bots.
    ///
    /// ONE TRANSACTION, GRANTS FIRST. For an org share, who accepted through it is only known from
    /// the `'org:'` account rows this deletes; dropping them first would leave nothing to say
    /// whose bots to take the recipe from.
    pub async fn unshare_recipe(
        &self,
        recipe_id: &str,
        scope: &str,
        scope_id: &str,
    ) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        if scope == "org" {
            sqlx::query(
                "delete from recipe_grant g using recipe r
                  where g.recipe_id = $1 and r.id = g.recipe_id and g.granted_by <> r.owner_id
                    and g.granted_by in (
                        select scope_id from recipe_share
                         where recipe_id = $1 and scope = 'account' and granted_by = 'org:' || $2)",
            )
            .bind(recipe_id)
            .bind(scope_id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(DROP_GRANTS_MADE_BY)
                .bind(recipe_id)
                .bind(scope_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "delete from recipe_share where recipe_id = $1 and scope = $2 and scope_id = $3",
        )
        .bind(recipe_id)
        .bind(scope)
        .bind(scope_id)
        .execute(&mut *tx)
        .await?;
        // A person who accepted through the org keeps nothing once the org share is withdrawn.
        if scope == "org" {
            sqlx::query(
                "delete from recipe_share where recipe_id = $1 and scope = 'account' and granted_by = 'org:' || $2",
            )
            .bind(recipe_id)
            .bind(scope_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The share rows of a recipe, for its owner's view.
    pub async fn recipe_shares(&self, recipe_id: &str) -> StoreResult<Vec<RecipeShareRow>> {
        let rows = sqlx::query(
            "select recipe_id, scope, scope_id, granted_by, granted_at_ms, accepted_at_ms, declined_at_ms
               from recipe_share where recipe_id = $1 order by granted_at_ms",
        )
        .bind(recipe_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RecipeShareRow {
                    recipe_id: row.try_get("recipe_id")?,
                    scope: row.try_get("scope")?,
                    scope_id: row.try_get("scope_id")?,
                    granted_by: row.try_get("granted_by")?,
                    granted_at_ms: row.try_get("granted_at_ms")?,
                    accepted_at_ms: row.try_get("accepted_at_ms")?,
                    declined_at_ms: row.try_get("declined_at_ms")?,
                })
            })
            .collect()
    }

    /// A person answers a share. Accepting through an org share writes their own account row
    /// (`granted_by = "org:<org>"`) so the answer is theirs alone.
    pub async fn answer_recipe_share(
        &self,
        recipe_id: &str,
        account_id: &str,
        org_id: Option<&str>,
        accept: bool,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let direct = sqlx::query(
            "select granted_by from recipe_share where recipe_id = $1 and scope = 'account' and scope_id = $2",
        )
        .bind(recipe_id)
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        // The org that answered, not merely whether one did: the insert below stamps it into
        // `granted_by`, and re-deriving it there is how `unwrap_or("")` got in. `scope_id <> ''`
        // neutralises a row an older build may already have written with an empty org.
        let via_org: Option<&str> = match org_id.filter(|org| !org.is_empty()) {
            Some(org) => sqlx::query(
                "select 1 from recipe_share
                  where recipe_id = $1 and scope = 'org' and scope_id <> '' and scope_id = $2",
            )
            .bind(recipe_id)
            .bind(org)
            .fetch_optional(&self.pool)
            .await?
            .map(|_| org),
            None => None,
        };
        if direct.is_none() && via_org.is_none() {
            return Ok(false);
        }
        let (accepted, declined) = if accept {
            (Some(at_ms), None::<i64>)
        } else {
            (None, Some(at_ms))
        };
        if direct.is_some() {
            sqlx::query(
                "update recipe_share set accepted_at_ms = $3, declined_at_ms = $4
                  where recipe_id = $1 and scope = 'account' and scope_id = $2",
            )
            .bind(recipe_id)
            .bind(account_id)
            .bind(accepted)
            .bind(declined)
            .execute(&self.pool)
            .await?;
        } else if let Some(org) = via_org {
            sqlx::query(
                "insert into recipe_share (recipe_id, scope, scope_id, granted_by, granted_at_ms, accepted_at_ms, declined_at_ms)
                 values ($1, 'account', $2, 'org:' || $3, $4, $5, $6)
                 on conflict (recipe_id, scope, scope_id) do update set
                   accepted_at_ms = excluded.accepted_at_ms, declined_at_ms = excluded.declined_at_ms",
            )
            .bind(recipe_id)
            .bind(account_id)
            .bind(org)
            .bind(at_ms)
            .bind(accepted)
            .bind(declined)
            .execute(&self.pool)
            .await?;
        }
        // Declining is giving it back: the bots this person granted it to stop being offered it.
        if !accept {
            sqlx::query(DROP_GRANTS_MADE_BY)
                .bind(recipe_id)
                .bind(account_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(true)
    }

    /// Whether this person has accepted a share of the recipe (directly or through their org).
    pub async fn recipe_accepted_by(&self, recipe_id: &str, account_id: &str) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 from recipe_share where recipe_id = $1 and scope = 'account' and scope_id = $2
              and accepted_at_ms is not null",
        )
        .bind(recipe_id)
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn grant_recipe(
        &self,
        recipe_id: &str,
        coworker_id: &str,
        granted_by: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe_grant (recipe_id, coworker_id, granted_by, granted_at_ms)
             values ($1, $2, $3, $4) on conflict (recipe_id, coworker_id) do nothing",
        )
        .bind(recipe_id)
        .bind(coworker_id)
        .bind(granted_by)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn revoke_recipe_grant(&self, recipe_id: &str, coworker_id: &str) -> StoreResult<()> {
        sqlx::query("delete from recipe_grant where recipe_id = $1 and coworker_id = $2")
            .bind(recipe_id)
            .bind(coworker_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recipe_grants(&self, recipe_id: &str) -> StoreResult<Vec<RecipeGrantRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select g.recipe_id, g.coworker_id, g.granted_by, g.granted_at_ms
               from recipe_grant g join recipe r on r.id = g.recipe_id
              where g.recipe_id = $1 and {GRANT_STILL_HELD} order by g.granted_at_ms"
        )))
        .bind(recipe_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RecipeGrantRow {
                    recipe_id: row.try_get("recipe_id")?,
                    coworker_id: row.try_get("coworker_id")?,
                    granted_by: row.try_get("granted_by")?,
                    granted_at_ms: row.try_get("granted_at_ms")?,
                })
            })
            .collect()
    }

    /// The recipes a bot may run: granted by somebody who still holds them, and not deleted.
    pub async fn recipes_granted_to(&self, coworker_id: &str) -> StoreResult<Vec<RecipeRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{RECIPE_SELECT} join recipe_grant g on g.recipe_id = r.id
             where g.coworker_id = $1 and r.deleted_at_ms is null and {GRANT_STILL_HELD}
             order by r.name"
        )))
        .bind(coworker_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(recipe_row).collect()
    }

    /// Write a run down BEFORE the box is asked to play it: not ok yet, and leased until
    /// `lease_until_ms`. Writes nothing and answers false when this bot already has a run whose
    /// lease is live, because two recipes clicking on one screen at once is neither's recipe.
    ///
    /// THE LOCK IS WHAT MAKES THE CHECK HOLD (#227). Under READ COMMITTED the `not exists` reads
    /// a snapshot taken before a concurrent start's row is committed, so two starts both saw no
    /// live run and both inserted. A per-bot transaction lock serialises them, and it must be its
    /// own statement before the insert: a statement's snapshot is taken when it begins, so a lock
    /// taken inside the insert would be granted after the snapshot had already missed the
    /// winner's row. A unique index cannot do it: an expired lease stays non-null. This makes the
    /// start atomic, not the lease: a run that outlives its lapsed lease is not fenced
    /// (`formal/tla/RecipeLease.tla`).
    pub async fn start_recipe_run(
        &self,
        id: &str,
        recipe_id: &str,
        version: i32,
        coworker_id: &str,
        lease_until_ms: i64,
        at_ms: i64,
    ) -> StoreResult<bool> {
        let mut tx = self.pool.begin().await?;
        // The two-int form, keyed by a class of its own, so it cannot collide with the single
        // bigint keys the purge, the migrations and the test databases lock on.
        sqlx::query("select pg_advisory_xact_lock($1, hashtext($2))")
            .bind(RECIPE_LEASE_LOCK_CLASS)
            .bind(coworker_id)
            .execute(&mut *tx)
            .await?;
        let done = sqlx::query(
            "insert into recipe_run
                 (id, recipe_id, version, coworker_id, run_id, ok, stopped_at, receipt, at_ms, lease_until_ms)
             select $1, $2, $3, $4, $1, false, null, '{\"ok\": false, \"running\": true}'::jsonb, $5, $6
              where not exists (
                  select 1 from recipe_run where coworker_id = $4 and lease_until_ms > $5)",
        )
        .bind(id)
        .bind(recipe_id)
        .bind(version)
        .bind(coworker_id)
        .bind(at_ms)
        .bind(lease_until_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(done.rows_affected() == 1)
    }

    /// Push a playing run's lease out. A finished row is left alone.
    pub async fn hold_recipe_run(&self, id: &str, lease_until_ms: i64) -> StoreResult<()> {
        sqlx::query(
            "update recipe_run set lease_until_ms = $2 where id = $1 and lease_until_ms is not null",
        )
        .bind(id)
        .bind(lease_until_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Write a finished run: a new row, or the one `start_recipe_run` wrote, finished in place.
    /// A finished row keeps the time it started, so history does not reorder as runs land.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_recipe_run(
        &self,
        id: &str,
        recipe_id: &str,
        version: i32,
        coworker_id: &str,
        run_id: Option<&str>,
        ok: bool,
        stopped_at: Option<i32>,
        receipt: &serde_json::Value,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into recipe_run (id, recipe_id, version, coworker_id, run_id, ok, stopped_at, receipt, at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             on conflict (id) do update set
               run_id = excluded.run_id, ok = excluded.ok, stopped_at = excluded.stopped_at,
               receipt = excluded.receipt, lease_until_ms = null",
        )
        .bind(id)
        .bind(recipe_id)
        .bind(version)
        .bind(coworker_id)
        .bind(run_id)
        .bind(ok)
        .bind(stopped_at)
        .bind(receipt)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drop the artifacts of runs that are no longer there.
    ///
    /// Called after pruning, because an artifact outliving its run is a megabyte nobody can
    /// reach: history is read a run at a time, and a row pointing at a run that was pruned has
    /// no page to appear on. Soft, like every other delete here.
    pub async fn orphan_artifacts_of_gone_runs(
        &self,
        recipe_id: &str,
        at_ms: i64,
    ) -> StoreResult<u64> {
        let done = sqlx::query(
            "update artifact set deleted_at_ms = $2
              where recipe_id = $1 and deleted_at_ms is null and run_id is not null
                and run_id not in (select id from recipe_run where recipe_id = $1)",
        )
        .bind(recipe_id)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// Drop all but the newest `keep` runs of one version BY ONE ACCOUNT — the account whose bot
    /// `coworker_id` is. Called after a run is written, so a long-lived recipe cannot grow an
    /// unbounded history nobody reads.
    ///
    /// PER RUNNER, because a shared recipe is run by more than its owner: kept per version alone,
    /// five runs by a recipient evicted the owner's history, and the orphan sweep then took the
    /// owner's screenshots with it. A bot with no roster row (a stand-in) is its own runner.
    ///
    /// A run still playing is never pruned: its row is what keeps the orphan sweep off the
    /// screenshots it is filing, and the receipt it is about to write needs somewhere to land.
    pub async fn prune_recipe_runs(
        &self,
        recipe_id: &str,
        version: i32,
        coworker_id: &str,
        keep: i64,
        now_ms: i64,
    ) -> StoreResult<u64> {
        let done = sqlx::query(
            "with runner as (
                 select id from coworker_view
                  where account_id in (select account_id from coworker_view where id = $3)
                 union select $3
             )
             delete from recipe_run
              where recipe_id = $1 and version = $2
                and coworker_id in (select id from runner)
                and (lease_until_ms is null or lease_until_ms <= $5)
                and id not in (
                    select id from recipe_run
                     where recipe_id = $1 and version = $2
                       and coworker_id in (select id from runner)
                     order by at_ms desc
                     limit $4
                )",
        )
        .bind(recipe_id)
        .bind(version)
        .bind(coworker_id)
        .bind(keep)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// One account's runs of a recipe: the ones its own bots played, retired bots included.
    ///
    /// A run is shown only to the account that ran it, because that is the account its
    /// screenshots belong to (`artifact.account_id`, the only thing the bytes route serves by),
    /// and a history that listed a colleague's runs listed pictures that could never open.
    pub async fn recipe_runs_for_account(
        &self,
        recipe_id: &str,
        account_id: &str,
        limit: i64,
    ) -> StoreResult<Vec<RecipeRunRow>> {
        let rows = sqlx::query(
            "select id, recipe_id, version, coworker_id, run_id, ok, stopped_at, receipt, at_ms,
                    lease_until_ms
               from recipe_run
              where recipe_id = $1
                and coworker_id in (select id from coworker_view where account_id = $2)
              order by at_ms desc limit $3",
        )
        .bind(recipe_id)
        .bind(account_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(recipe_run_row).collect()
    }
}
