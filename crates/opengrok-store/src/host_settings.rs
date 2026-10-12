//! The host settings record, kept across restarts (`host_settings`, one row).
//!
//! The store keeps the record whole and opaque — its shape belongs to the server
//! (`opengrok_server::host_state::default_settings`), the same way `meta` belongs to its writers.

use sqlx::Row as _;

use crate::StoreResult;
use crate::postgres::PgStore;

impl PgStore {
    /// The record the last write left, or `None` on a host that never saved one.
    pub async fn host_settings(&self) -> StoreResult<Option<serde_json::Value>> {
        let row = sqlx::query("select record from host_settings where id = 'host'")
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|row| row.try_get("record")).transpose()?)
    }

    /// Replace the record. Whole, not merged: the caller merged the patch into what it holds.
    pub async fn save_host_settings(
        &self,
        record: &serde_json::Value,
        updated_at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into host_settings (id, record, updated_at_ms) values ('host', $1, $2)
             on conflict (id) do update set record = excluded.record,
                                            updated_at_ms = excluded.updated_at_ms",
        )
        .bind(record)
        .bind(updated_at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
