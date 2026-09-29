//! What is left of the USD spend limits (`docs/plan-spend-policy.md`), which `points.rs` replaced:
//! their shape, which the console's spend reply still carries empty until the desktop's usage
//! modal stops reading it, and whose coworker is whose. Money is a string of up to six decimals,
//! never a float. The `spend_limit` table stays: the operator purge still clears it.

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

impl PgStore {
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
