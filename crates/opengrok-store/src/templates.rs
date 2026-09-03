//! Coworker templates: a coworker TYPE an org admin writes once and members hire from. What a
//! template says — the model pin, the tool ceiling, what needs a human yes, the spend limits —
//! is COPIED to the coworker at hire; the coworker remembers which template it came from and
//! nothing else links them. Editing a template changes no running coworker unless the admin
//! applies it deliberately; deleting one leaves its coworkers exactly as hired.

use opengrok_policy::ToolSet;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::StoreResult;
use crate::points::PointsLimit;
use crate::postgres::PgStore;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoworkerTemplate {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub description: String,
    /// A route through the gateway; `None` ⇒ the deployment's default at hire.
    pub model: Option<String>,
    /// The most a coworker hired from this may ever do (its profile starts equal to it).
    pub tool_ceiling: ToolSet,
    /// Inside the ceiling: what runs only with a human yes.
    pub needs_approval: ToolSet,
    /// The month's cap and the day's brake a coworker hired from this starts with.
    pub points: PointsLimit,
    /// The standing role a coworker hired from this starts with.
    pub role: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

fn template_row(row: sqlx::postgres::PgRow) -> StoreResult<CoworkerTemplate> {
    let ceiling: serde_json::Value = row.try_get("tool_ceiling")?;
    let approval: serde_json::Value = row.try_get("needs_approval")?;
    Ok(CoworkerTemplate {
        id: row.try_get("id")?,
        org_id: row.try_get("org_id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        model: row.try_get("model")?,
        tool_ceiling: serde_json::from_value(ceiling).map_err(|error| {
            crate::StoreError::Corrupt(format!("template tool ceiling: {error}"))
        })?,
        needs_approval: serde_json::from_value(approval).map_err(|error| {
            crate::StoreError::Corrupt(format!("template approval set: {error}"))
        })?,
        points: PointsLimit {
            month_points: row.try_get("month_points")?,
            day_points: row.try_get("day_points")?,
        },
        role: row.try_get("role")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

impl PgStore {
    /// Create or replace a template; the id is the caller's, the org is the row's owner.
    pub async fn put_template(&self, template: &CoworkerTemplate) -> StoreResult<()> {
        let ceiling = serde_json::to_value(&template.tool_ceiling).map_err(|error| {
            crate::StoreError::Corrupt(format!("template tool ceiling: {error}"))
        })?;
        let approval = serde_json::to_value(&template.needs_approval).map_err(|error| {
            crate::StoreError::Corrupt(format!("template approval set: {error}"))
        })?;
        sqlx::query(
            "insert into coworker_template
                (id, org_id, name, description, model, tool_ceiling, needs_approval,
                 month_points, day_points, role, created_at_ms, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             on conflict (id) do update set
                name = excluded.name, description = excluded.description,
                model = excluded.model, tool_ceiling = excluded.tool_ceiling,
                needs_approval = excluded.needs_approval,
                month_points = excluded.month_points, day_points = excluded.day_points,
                role = excluded.role, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(&template.id)
        .bind(&template.org_id)
        .bind(&template.name)
        .bind(&template.description)
        .bind(&template.model)
        .bind(&ceiling)
        .bind(&approval)
        .bind(template.points.month_points)
        .bind(template.points.day_points)
        .bind(&template.role)
        .bind(template.created_at_ms)
        .bind(template.updated_at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// One org's templates, by name. Scoped by org so a listing can never show another org's.
    pub async fn templates_for_org(&self, org_id: &str) -> StoreResult<Vec<CoworkerTemplate>> {
        let rows = sqlx::query(
            "select id, org_id, name, description, model, tool_ceiling, needs_approval,
                    month_points, day_points, role, created_at_ms, updated_at_ms
             from coworker_template where org_id = $1 order by name, id",
        )
        .bind(org_id)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(template_row).collect()
    }

    /// A template, only if THIS org owns it — `None` for anybody else's, so the caller 404s
    /// rather than confirm it exists.
    pub async fn template_in_org(
        &self,
        org_id: &str,
        id: &str,
    ) -> StoreResult<Option<CoworkerTemplate>> {
        let row = sqlx::query(
            "select id, org_id, name, description, model, tool_ceiling, needs_approval,
                    month_points, day_points, role, created_at_ms, updated_at_ms
             from coworker_template where org_id = $1 and id = $2",
        )
        .bind(org_id)
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.map(template_row).transpose()
    }

    /// `true` when a row was removed. Coworkers hired from it are untouched, on purpose.
    pub async fn delete_template(&self, org_id: &str, id: &str) -> StoreResult<bool> {
        let done = sqlx::query("delete from coworker_template where org_id = $1 and id = $2")
            .bind(org_id)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Remember which template a coworker was hired from.
    pub async fn record_template_use(
        &self,
        coworker: &opengrok_core::id::CoworkerId,
        template_id: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into coworker_template_use (coworker_id, template_id, at_ms)
             values ($1, $2, $3)
             on conflict (coworker_id) do update
                set template_id = excluded.template_id, at_ms = excluded.at_ms",
        )
        .bind(coworker.as_str())
        .bind(template_id)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The template a coworker was hired from, if any.
    pub async fn template_of(
        &self,
        coworker: &opengrok_core::id::CoworkerId,
    ) -> StoreResult<Option<String>> {
        let row =
            sqlx::query("select template_id from coworker_template_use where coworker_id = $1")
                .bind(coworker.as_str())
                .fetch_optional(self.pool())
                .await?;
        row.map(|row| row.try_get("template_id").map_err(Into::into))
            .transpose()
    }
}
