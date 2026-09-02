//! Spend limits as WE author them (`docs/plan-spend-policy.md`). Three windows per scope —
//! a rolling five hours, a rolling seven days, the calendar month — at three scopes: the org's
//! default, a member's override, a coworker's own. Money is a string of up to six decimals,
//! never a float; the resolver compares in micro-dollars. `None` at a layer means "this layer
//! says nothing", never zero.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

/// The three limits, each optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendLimit {
    #[serde(default)]
    pub five_hour_usd: Option<String>,
    #[serde(default)]
    pub seven_day_usd: Option<String>,
    #[serde(default)]
    pub month_usd: Option<String>,
}

impl SpendLimit {
    pub fn is_empty(&self) -> bool {
        self.five_hour_usd.is_none() && self.seven_day_usd.is_none() && self.month_usd.is_none()
    }

    /// The most specific value per window: `self` where it says something, `below` otherwise.
    #[must_use]
    pub fn over(self, below: &SpendLimit) -> SpendLimit {
        SpendLimit {
            five_hour_usd: self.five_hour_usd.or_else(|| below.five_hour_usd.clone()),
            seven_day_usd: self.seven_day_usd.or_else(|| below.seven_day_usd.clone()),
            month_usd: self.month_usd.or_else(|| below.month_usd.clone()),
        }
    }
}

/// Where a limit row applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendScope {
    Org,
    Member,
    Coworker,
}

impl SpendScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Org => "org",
            Self::Member => "member",
            Self::Coworker => "coworker",
        }
    }
}

impl PgStore {
    /// Write the limits at a scope; an empty limit removes the row (nothing to say).
    pub async fn put_spend_limit(
        &self,
        scope: SpendScope,
        id: &str,
        limit: &SpendLimit,
        at_ms: i64,
    ) -> StoreResult<()> {
        if limit.is_empty() {
            sqlx::query("delete from spend_limit where scope_kind = $1 and scope_id = $2")
                .bind(scope.as_str())
                .bind(id)
                .execute(self.pool())
                .await?;
            return Ok(());
        }
        sqlx::query(
            "insert into spend_limit
                (scope_kind, scope_id, five_hour_usd, seven_day_usd, month_usd, updated_at_ms)
             values ($1, $2, $3, $4, $5, $6)
             on conflict (scope_kind, scope_id) do update
                set five_hour_usd = excluded.five_hour_usd,
                    seven_day_usd = excluded.seven_day_usd,
                    month_usd = excluded.month_usd,
                    updated_at_ms = excluded.updated_at_ms",
        )
        .bind(scope.as_str())
        .bind(id)
        .bind(&limit.five_hour_usd)
        .bind(&limit.seven_day_usd)
        .bind(&limit.month_usd)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn spend_limit(
        &self,
        scope: SpendScope,
        id: &str,
    ) -> StoreResult<Option<SpendLimit>> {
        let row = sqlx::query(
            "select five_hour_usd, seven_day_usd, month_usd from spend_limit
             where scope_kind = $1 and scope_id = $2",
        )
        .bind(scope.as_str())
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            Ok(SpendLimit {
                five_hour_usd: row.try_get("five_hour_usd")?,
                seven_day_usd: row.try_get("seven_day_usd")?,
                month_usd: row.try_get("month_usd")?,
            })
        })
        .transpose()
    }

    /// Every row at one scope, keyed by scope id — the admin card's listing.
    pub async fn spend_limits_at(
        &self,
        scope: SpendScope,
    ) -> StoreResult<Vec<(String, SpendLimit)>> {
        let rows = sqlx::query(
            "select scope_id, five_hour_usd, seven_day_usd, month_usd from spend_limit
             where scope_kind = $1 order by scope_id",
        )
        .bind(scope.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("scope_id")?,
                    SpendLimit {
                        five_hour_usd: row.try_get("five_hour_usd")?,
                        seven_day_usd: row.try_get("seven_day_usd")?,
                        month_usd: row.try_get("month_usd")?,
                    },
                ))
            })
            .collect()
    }

    /// Whose coworker this is — the account that hired it, retired or not.
    pub async fn coworker_owner(
        &self,
        coworker: &opengrok_core::id::CoworkerId,
    ) -> StoreResult<Option<opengrok_core::id::AccountId>> {
        let row = sqlx::query("select account_id from coworker_view where id = $1")
            .bind(coworker.as_str())
            .fetch_optional(self.pool())
            .await?;
        row.map(|row| {
            row.try_get::<String, _>("account_id")
                .map(opengrok_core::id::AccountId::from_stored)
                .map_err(Into::into)
        })
        .transpose()
    }
}
