//! Skills: a named, versioned bundle of instructions a person invokes for one turn by typing
//! `/name`. A `SKILL.md` body, plus whatever small files sit beside it, owned by an account.
//!
//! The rows only. WHO may touch one is decided in the server's `skills::permitted`, in one place,
//! so this module never has to be told whose request it is serving — every function here takes an
//! id the caller has already been permitted, and none of them takes a "whose" argument that a
//! request body could have supplied.
//!
//! Deletion is soft, like a recipe's: a run that cited a skill has to stay able to say what it
//! cited, and a hard delete would turn that citation into a dangling id.

use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

/// One `skill` row, with how many versions it has and which is newest. Both come off the same
/// read: a listing that fetched the count separately would be a round trip per skill on every
/// page open, which is the shape that made the recipe listing slow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRow {
    pub id: String,
    pub owner_id: String,
    pub org_id: Option<String>,
    pub name: String,
    pub description: String,
    /// `authored` | `uploaded` | `taught`. Written by the server from how the row was made.
    pub source: String,
    pub enabled: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
    /// 0 on a skill with no body yet — which is what makes it a draft.
    pub version_count: i64,
    /// 0 when there is no version. Versions start at 1.
    pub latest_version: i32,
}

/// One body, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillVersionRow {
    pub skill_id: String,
    pub version: i32,
    pub kind: String,
    pub body: String,
    pub note: String,
    pub created_by: String,
    pub created_at_ms: i64,
}

/// One supporting file, at one version. `bytes` is the file itself — kept out of listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFileRow {
    pub path: String,
    pub bytes: Vec<u8>,
}

const SKILL_SELECT: &str = "select s.id, s.owner_id, s.org_id, s.name, s.description, s.source,
        s.enabled, s.created_at_ms, s.updated_at_ms, s.deleted_at_ms,
        (select count(*) from skill_version v where v.skill_id = s.id) as version_count,
        (select max(version) from skill_version v where v.skill_id = s.id) as latest_version
   from skill s";

fn skill_row(row: &sqlx::postgres::PgRow) -> StoreResult<SkillRow> {
    Ok(SkillRow {
        id: row.try_get("id")?,
        owner_id: row.try_get("owner_id")?,
        org_id: row.try_get("org_id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        source: row.try_get("source")?,
        enabled: row.try_get("enabled")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        deleted_at_ms: row.try_get("deleted_at_ms")?,
        version_count: row.try_get::<Option<i64>, _>("version_count")?.unwrap_or(0),
        latest_version: row
            .try_get::<Option<i32>, _>("latest_version")?
            .unwrap_or(0),
    })
}

fn skill_version_row(row: &sqlx::postgres::PgRow) -> StoreResult<SkillVersionRow> {
    Ok(SkillVersionRow {
        skill_id: row.try_get("skill_id")?,
        version: row.try_get("version")?,
        kind: row.try_get("kind")?,
        body: row.try_get("body")?,
        note: row.try_get("note")?,
        created_by: row.try_get("created_by")?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}

const SKILL_VERSION_SELECT: &str =
    "select skill_id, version, kind, body, note, created_by, created_at_ms from skill_version";

/// What a new `skill` row is, minus the bits the store decides. Gathered into a struct because
/// the alternative is a seven-argument call in which `owner_id` and `org_id` are both `&str` and
/// swapping them compiles.
#[derive(Debug, Clone)]
pub struct NewSkill<'a> {
    pub id: &'a str,
    pub owner_id: &'a str,
    pub org_id: Option<&'a str>,
    pub name: &'a str,
    pub description: &'a str,
    pub source: &'a str,
    /// Whether it may be invoked the moment it exists. `true` for a skill a person wrote or
    /// uploaded: they have read it, because they wrote it.
    ///
    /// ON THE INSERT RATHER THAN AN UPDATE AFTERWARDS, which is the whole reason the field
    /// exists. A body a MODEL wrote (`server/skills.rs`, `from_tape`) is born switched off so a
    /// person reads it before any turn does; written as a second call, the insert could succeed
    /// and the switch-off fail, and what that leaves behind is a live skill nobody has read —
    /// the one state the review gate exists to prevent. The column defaults to `true`, so the
    /// safe value is the one a caller has to ask for.
    pub enabled: bool,
}

/// One body to write, with what came beside it. A struct for the same reason as `NewSkill`:
/// `kind`, `body`, `note` and `created_by` are four `&str` in a row, and any permutation of them
/// compiles into a version that is wrong in a way no test would notice until a person read one.
#[derive(Debug, Clone)]
pub struct NewSkillVersion<'a> {
    pub kind: &'a str,
    pub body: &'a str,
    pub note: &'a str,
    pub files: &'a [SkillFileRow],
    pub created_by: &'a str,
}

/// One version row, its files, and the parent's timestamp — the three writes that are always
/// made together. Shared by `create_skill` and `add_skill_version` so a body written at creation
/// and one written later cannot come to mean different things.
async fn write_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    skill_id: &str,
    version: i32,
    new: &NewSkillVersion<'_>,
    at_ms: i64,
) -> StoreResult<()> {
    sqlx::query(
        "insert into skill_version (skill_id, version, kind, body, note, created_by, created_at_ms)
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(skill_id)
    .bind(version)
    .bind(new.kind)
    .bind(new.body)
    .bind(new.note)
    .bind(new.created_by)
    .bind(at_ms)
    .execute(&mut **tx)
    .await?;
    for file in new.files {
        sqlx::query(
            "insert into skill_file (skill_id, version, path, bytes) values ($1, $2, $3, $4)",
        )
        .bind(skill_id)
        .bind(version)
        .bind(&file.path)
        .bind(&file.bytes)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("update skill set updated_at_ms = $2 where id = $1")
        .bind(skill_id)
        .bind(at_ms)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl PgStore {
    /// Write the row and, when there is one, its first body — in ONE transaction.
    ///
    /// The body is not a second call for a reason that cost a person their skill's name: a
    /// failure between the two left a named, bodiless row holding the unique `(owner_id, name)`,
    /// and the retry was then refused as a duplicate of a skill that had never been made and
    /// whose id nobody had been told.
    pub async fn create_skill(
        &self,
        new: NewSkill<'_>,
        first: Option<NewSkillVersion<'_>>,
        at_ms: i64,
    ) -> StoreResult<()> {
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "insert into skill (id, owner_id, org_id, name, description, source, enabled,
                                created_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
        )
        .bind(new.id)
        .bind(new.owner_id)
        .bind(new.org_id)
        .bind(new.name)
        .bind(new.description)
        .bind(new.source)
        .bind(new.enabled)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?;
        if let Some(first) = first {
            write_version(&mut tx, new.id, 1, &first, at_ms).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Rename, re-describe, and turn a skill on or off. There is no owner argument on purpose:
    /// an update may never move a row to another account.
    pub async fn update_skill(
        &self,
        id: &str,
        name: &str,
        description: &str,
        enabled: bool,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update skill set name = $2, description = $3, enabled = $4, updated_at_ms = $5
              where id = $1",
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(enabled)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn soft_delete_skill(&self, id: &str, at_ms: i64) -> StoreResult<()> {
        sqlx::query("update skill set deleted_at_ms = $2, updated_at_ms = $2 where id = $1")
            .bind(id)
            .bind(at_ms)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// One skill by id, deleted or not. The caller decides what a deleted row means to it — the
    /// owner is shown one, everybody else is told it does not exist.
    pub async fn skill(&self, id: &str) -> StoreResult<Option<SkillRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SKILL_SELECT} where s.id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(skill_row).transpose()
    }

    /// The live skill one account has under a name, which is what `/name` will resolve against
    /// and what a rename has to refuse colliding with.
    pub async fn skill_named(&self, owner_id: &str, name: &str) -> StoreResult<Option<SkillRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SKILL_SELECT} where s.owner_id = $1 and s.name = $2 and s.deleted_at_ms is null"
        )))
        .bind(owner_id)
        .bind(name)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(skill_row).transpose()
    }

    /// The skills a person owns (not deleted).
    pub async fn skills_owned_by(&self, owner_id: &str) -> StoreResult<Vec<SkillRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SKILL_SELECT} where s.owner_id = $1 and s.deleted_at_ms is null
              order by s.updated_at_ms desc"
        )))
        .bind(owner_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(skill_row).collect()
    }

    /// The org's skills that somebody else owns. Disabled ones are left out: the owner switched
    /// them off, and a colleague seeing one in a list would be seeing something they cannot use.
    pub async fn skills_in_org(&self, org_id: &str, but_not: &str) -> StoreResult<Vec<SkillRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SKILL_SELECT} where s.org_id = $1 and s.owner_id <> $2
               and s.deleted_at_ms is null and s.enabled
              order by s.updated_at_ms desc"
        )))
        .bind(org_id)
        .bind(but_not)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(skill_row).collect()
    }

    /// Write a body as the next version, with the files that came with it, and touch the skill.
    ///
    /// THE SKILL ROW IS LOCKED FIRST, and that is what makes the number safe. A transaction alone
    /// does not: at READ COMMITTED, `max(version) + 1` takes no lock on rows that do not exist
    /// yet, so two writers both read 3 and both try to write 4. The primary key means only one
    /// lands — nothing wrong is ever stored — but the loser gets a constraint violation for a
    /// request that was perfectly valid. `for update` on the parent serialises them instead, and
    /// the second one reads 4 and writes 5.
    pub async fn add_skill_version(
        &self,
        skill_id: &str,
        new: NewSkillVersion<'_>,
        at_ms: i64,
    ) -> StoreResult<i32> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("select 1 from skill where id = $1 for update")
            .bind(skill_id)
            .fetch_optional(&mut *tx)
            .await?;
        let next: i32 = sqlx::query_scalar(
            "select coalesce(max(version), 0) + 1 from skill_version where skill_id = $1",
        )
        .bind(skill_id)
        .fetch_one(&mut *tx)
        .await?;
        write_version(&mut tx, skill_id, next, &new, at_ms).await?;
        tx.commit().await?;
        Ok(next)
    }

    /// The newest body, which is the one a turn would be given.
    pub async fn latest_skill_version(
        &self,
        skill_id: &str,
    ) -> StoreResult<Option<SkillVersionRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SKILL_VERSION_SELECT} where skill_id = $1 order by version desc limit 1"
        )))
        .bind(skill_id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(skill_version_row).transpose()
    }

    /// Every version, newest first, for a history list.
    pub async fn skill_versions(&self, skill_id: &str) -> StoreResult<Vec<SkillVersionRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SKILL_VERSION_SELECT} where skill_id = $1 order by version desc"
        )))
        .bind(skill_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(skill_version_row).collect()
    }

    /// The files that came with one version, in path order so a listing is stable.
    pub async fn skill_files(
        &self,
        skill_id: &str,
        version: i32,
    ) -> StoreResult<Vec<SkillFileRow>> {
        let rows = sqlx::query(
            "select path, bytes from skill_file where skill_id = $1 and version = $2
              order by path",
        )
        .bind(skill_id)
        .bind(version)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SkillFileRow {
                    path: row.try_get("path")?,
                    bytes: row.try_get("bytes")?,
                })
            })
            .collect()
    }
}
