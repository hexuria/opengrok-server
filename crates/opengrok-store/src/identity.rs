//! Store operations for orgs, invites, and credential-account lookup.
//!
//! Same contract as the rest of the store: the events stream is the truth, the view is a
//! projection written in the same transaction. Orgs and invites are new aggregates; credential
//! accounts reuse the account aggregate (its new events persist through `append_account`), and
//! this module adds only the reads signup and login need — the org that owns an invite code, and
//! the account behind an email.

use opengrok_core::id::{AccountId, OrgId};
use opengrok_core::org::{InviteState, Org, OrgEvent, OrgView};
use sqlx::Row;

use crate::postgres::PgStore;
use crate::{StoreError, StoreResult, org_stream};

impl PgStore {
    pub async fn load_org(&self, id: &OrgId) -> StoreResult<(Org, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(org_stream(id))
        .fetch_all(self.pool())
        .await?;

        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: OrgEvent = serde_json::from_value(payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            events.push(event);
        }
        Ok((Org::replay(&events), seq))
    }

    pub async fn append_org(
        &self,
        id: &OrgId,
        expected_seq: i64,
        events: &[OrgEvent],
        state: &Org,
        at_ms: i64,
    ) -> StoreResult<i64> {
        let mut tx = self.pool().begin().await?;
        let stream = org_stream(id);
        let mut seq = expected_seq;

        for event in events {
            seq += 1;
            let payload = serde_json::to_value(event)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            sqlx::query(
                "insert into events (stream_id, stream_seq, event_type, payload)
                 values ($1, $2, $3, $4)",
            )
            .bind(&stream)
            .bind(seq)
            .bind(event.event_type())
            .bind(payload)
            .execute(&mut *tx)
            .await?;

            // The invite index tracks state per code, so signup can find an org by code and know
            // whether the code is spendable without replaying the org.
            match event {
                OrgEvent::InviteIssued { code, .. } => {
                    sqlx::query(
                        "insert into org_invite (code, org_id, state, updated_at_ms)
                         values ($1, $2, 'open', $3)
                         on conflict (code) do update set
                           state = excluded.state, updated_at_ms = excluded.updated_at_ms",
                    )
                    .bind(code)
                    .bind(id.as_str())
                    .bind(at_ms)
                    .execute(&mut *tx)
                    .await?;
                }
                OrgEvent::InviteRedeemed { code, .. } => {
                    sqlx::query(
                        "update org_invite set state = 'redeemed', updated_at_ms = $2 where code = $1",
                    )
                    .bind(code)
                    .bind(at_ms)
                    .execute(&mut *tx)
                    .await?;
                }
                OrgEvent::InviteRevoked { code, .. } => {
                    sqlx::query(
                        "update org_invite set state = 'revoked', updated_at_ms = $2 where code = $1",
                    )
                    .bind(code)
                    .bind(at_ms)
                    .execute(&mut *tx)
                    .await?;
                }
                _ => {}
            }
        }

        if let (Some(admin), false) = (state.admin.as_ref(), state.domains.is_empty()) {
            let domains = serde_json::to_value(&state.domains)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            let pending = serde_json::to_value(&state.pending_domains)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            sqlx::query(
                "insert into org_view (id, name, admin_id, domains, pending_domains, updated_at_ms)
                 values ($1, $2, $3, $4, $5, $6)
                 on conflict (id) do update set
                   name = excluded.name,
                   admin_id = excluded.admin_id,
                   domains = excluded.domains,
                   pending_domains = excluded.pending_domains,
                   updated_at_ms = excluded.updated_at_ms",
            )
            .bind(id.as_str())
            .bind(&state.name)
            .bind(admin.as_str())
            .bind(domains)
            .bind(pending)
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(seq)
    }

    /// The org that issued an invite code, if any — signup's first lookup.
    pub async fn org_by_invite(&self, code: &str) -> StoreResult<Option<OrgId>> {
        let row = sqlx::query("select org_id from org_invite where code = $1")
            .bind(code)
            .fetch_optional(self.pool())
            .await?;
        Ok(row
            .map(|row| row.try_get::<String, _>("org_id"))
            .transpose()?
            .map(OrgId::from_stored))
    }

    /// An org's projection — for the admin surfaces and tests.
    pub async fn org_view(&self, id: &OrgId) -> StoreResult<Option<OrgView>> {
        let row = sqlx::query(
            "select id, name, admin_id, domains, pending_domains, updated_at_ms
             from org_view where id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            Ok(OrgView {
                id: OrgId::from_stored(row.try_get::<String, _>("id")?),
                name: row.try_get("name")?,
                admin: AccountId::from_stored(row.try_get::<String, _>("admin_id")?),
                domains: serde_json::from_value(row.try_get("domains")?)
                    .map_err(|error| StoreError::Corrupt(error.to_string()))?,
                pending_domains: serde_json::from_value(row.try_get("pending_domains")?)
                    .map_err(|error| StoreError::Corrupt(error.to_string()))?,
                updated_at_ms: row.try_get("updated_at_ms")?,
            })
        })
        .transpose()
    }

    /// A convenience the smoke and the admin CLI both want: the state of one invite code.
    pub async fn invite_state(&self, code: &str) -> StoreResult<Option<InviteState>> {
        let row = sqlx::query("select state from org_invite where code = $1")
            .bind(code)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.and_then(|row| {
            row.try_get::<String, _>("state")
                .ok()
                .map(|state| match state.as_str() {
                    "redeemed" => InviteState::Redeemed(AccountId::from_stored("")),
                    "revoked" => InviteState::Revoked,
                    _ => InviteState::Open,
                })
        }))
    }
}
