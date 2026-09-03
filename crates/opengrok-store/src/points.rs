//! Points limits as WE author them (`docs/plan-spend-policy.md`). A point is one token at the
//! gateway's reference price; the gateway meters (`counterfactual_api_usd × 1e6 / R`, rounded
//! per request), this table says what may be spent. Two scopes: a member's monthly POOL, the
//! org admin's to set, drawn on by every coworker the member owns; and a coworker's own CAP
//! for the month and BRAKE for a rolling day, its owner's to set. `None` is "no limit here",
//! never zero: zero is an explicit stop.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

/// What a row says: the month's limit and, for a coworker, the day's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PointsLimit {
    #[serde(default)]
    pub month_points: Option<i64>,
    #[serde(default)]
    pub day_points: Option<i64>,
}

impl PointsLimit {
    pub fn is_empty(self) -> bool {
        self.month_points.is_none() && self.day_points.is_none()
    }
}

/// A row as stored: the limit, and who wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointsLimitRow {
    pub limit: PointsLimit,
    pub set_by: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointsScope {
    Member,
    Coworker,
}

impl PointsScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Coworker => "coworker",
        }
    }
}

fn row_of(row: &sqlx::postgres::PgRow) -> StoreResult<PointsLimitRow> {
    Ok(PointsLimitRow {
        limit: PointsLimit {
            month_points: row.try_get("month_points")?,
            day_points: row.try_get("day_points")?,
        },
        set_by: row.try_get("set_by")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

impl PgStore {
    /// Write a scope's limit; an empty limit removes the row (nothing to say).
    pub async fn put_points_limit(
        &self,
        scope: PointsScope,
        id: &str,
        limit: PointsLimit,
        set_by: &str,
        at_ms: i64,
    ) -> StoreResult<()> {
        if limit.is_empty() {
            sqlx::query("delete from points_limit where scope_kind = $1 and scope_id = $2")
                .bind(scope.as_str())
                .bind(id)
                .execute(self.pool())
                .await?;
            return Ok(());
        }
        sqlx::query(
            "insert into points_limit
                (scope_kind, scope_id, month_points, day_points, set_by, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6)
             on conflict (scope_kind, scope_id) do update
                set month_points = excluded.month_points,
                    day_points = excluded.day_points,
                    set_by = excluded.set_by,
                    updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope.as_str())
        .bind(id)
        .bind(limit.month_points)
        .bind(limit.day_points)
        .bind(set_by)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn points_limit(
        &self,
        scope: PointsScope,
        id: &str,
    ) -> StoreResult<Option<PointsLimitRow>> {
        let row = sqlx::query(
            "select month_points, day_points, set_by, updated_at_ms from points_limit
             where scope_kind = $1 and scope_id = $2",
        )
        .bind(scope.as_str())
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_of).transpose()
    }

    /// Every row at one scope, keyed by scope id — the admin page's listing.
    pub async fn points_limits_at(
        &self,
        scope: PointsScope,
    ) -> StoreResult<Vec<(String, PointsLimitRow)>> {
        let rows = sqlx::query(
            "select scope_id, month_points, day_points, set_by, updated_at_ms from points_limit
             where scope_kind = $1 order by scope_id",
        )
        .bind(scope.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("scope_id")?, row_of(row)?)))
            .collect()
    }
}
