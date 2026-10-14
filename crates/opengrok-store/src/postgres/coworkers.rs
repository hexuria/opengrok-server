//! Coworkers: who works here, and which computer is theirs.

use super::*;

/// Coworkers: who works here, and which computer is theirs.
impl PgStore {
    pub async fn load_coworker(&self, id: &CoworkerId) -> StoreResult<(Coworker, i64)> {
        let (events, seq) = self
            .stream::<CoworkerEvent>(&crate::coworker_stream(id))
            .await?;
        Ok((Coworker::replay(&events), seq))
    }

    pub async fn append_coworker(
        &self,
        id: &CoworkerId,
        account_id: &AccountId,
        expected_seq: i64,
        events: &[CoworkerEvent],
        view: &CoworkerView,
    ) -> StoreResult<i64> {
        let mut tx = self.pool.begin().await?;
        let stream = crate::coworker_stream(id);
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
        }

        let members = serde_json::to_value(&view.members)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        sqlx::query(
            "insert into coworker_view (id, account_id, name, model, box_id, retired,
                updated_at_ms, members, role, visibility, effort, source)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             on conflict (id) do update set
               name = excluded.name,
               model = excluded.model,
               box_id = excluded.box_id,
               retired = excluded.retired,
               updated_at_ms = excluded.updated_at_ms,
               members = excluded.members,
               role = excluded.role,
               visibility = excluded.visibility,
               effort = excluded.effort,
               source = excluded.source",
        )
        .bind(view.id.as_str())
        .bind(account_id.as_str())
        .bind(&view.name)
        .bind(&view.model)
        .bind(view.box_id.as_ref().map(|id| id.as_str()))
        .bind(view.retired)
        .bind(view.updated_at_ms)
        .bind(&members)
        .bind(&view.role)
        .bind(view.visibility.as_str())
        .bind(view.effort.as_str())
        .bind(view.source.map(SourceKind::as_str))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(seq)
    }

    /// Whether this person may TALK to this coworker: they own it, or it is shared with their
    /// org and they are in that org. Not whether they may manage it — management stays with the
    /// owner, and `coworkers_for` is what that is gated on.
    ///
    /// An empty `org_id` never matches, so two accounts with no org do not silently share.
    /// `roster_for` and `policy_to_use` repeat this predicate in SQL; the three must agree, or a
    /// member would be listed a coworker they cannot talk to, or talk to one they cannot see.
    /// So this is exactly `roster_for`'s WHERE — retired excluded, LEFT joins so an owner whose
    /// account row is missing still has their own coworker — because the run and approvals
    /// doors answer 404 on a `false` here, and must not say "no such coworker" about one the
    /// caller's roster lists.
    pub async fn may_use_coworker(
        &self,
        account_id: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 as ok
             from coworker_view c
             left join account_view owner on owner.id = c.account_id
             left join account_view caller on caller.id = $1
             where c.id = $2
               and c.retired = false
               and (c.account_id = $1
                    or (c.visibility = 'org'
                        and coalesce(owner.org_id, '') <> ''
                        and owner.org_id = caller.org_id))
             limit 1",
        )
        .bind(account_id.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// The roster, newest first — the order the client sorts by: this person's own coworkers,
    /// plus the ones an org-mate has shared with the org. Each row carries its owner, because a
    /// shared row has to be able to say whose it is.
    ///
    /// A superset of `coworkers_for`, never narrower: the account joins are LEFT joins, so a
    /// person's own coworker is listed (and PATCHable) even if an account row is missing.
    ///
    /// SEPARATE from `coworkers_for` on purpose. That one is the authorisation primitive
    /// management is gated on (PATCH, retire, keys, limits, the computer — a dozen routes), and
    /// widening it would hand every org member every other member's write surface. Sharing a
    /// coworker lets somebody talk to it; it is not a write grant.
    pub async fn roster_for(
        &self,
        account_id: &AccountId,
    ) -> StoreResult<Vec<(CoworkerView, RosterOwner)>> {
        let rows = sqlx::query(
            "select c.id, c.name, c.model, c.box_id, c.retired, c.updated_at_ms, c.members,
                    c.role, c.visibility, c.effort, c.source,
                    c.account_id as owner_id, coalesce(owner.first_name, '') as owner_first,
                    coalesce(owner.last_name, '') as owner_last, owner.org_id as owner_org
             from coworker_view c
             left join account_view owner on owner.id = c.account_id
             left join account_view caller on caller.id = $1
             where c.retired = false
               and (c.account_id = $1
                    or (c.visibility = 'org'
                        and coalesce(owner.org_id, '') <> ''
                        and owner.org_id = caller.org_id))
             order by c.updated_at_ms desc",
        )
        .bind(account_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| Ok((coworker_view_row(row)?, roster_owner_row(row)?)))
            .collect()
    }

    /// One account as `roster_for` names an owner, for a reply about a coworker that is not on
    /// a roster yet (the hire). The same LEFT join and the same columns, so the hire reply's
    /// `owner` is the one the next roster read lists; a missing account row is blank names,
    /// not an error, exactly as the roster reads it.
    pub async fn roster_owner(&self, account_id: &AccountId) -> StoreResult<RosterOwner> {
        let row = sqlx::query(
            "select $1 as owner_id, coalesce(owner.first_name, '') as owner_first,
                    coalesce(owner.last_name, '') as owner_last, owner.org_id as owner_org
             from (select 1) as one
             left join account_view owner on owner.id = $1",
        )
        .bind(account_id.as_str())
        .fetch_one(&self.pool)
        .await?;
        roster_owner_row(&row)
    }

    /// This account's OWN coworkers, newest first — what management is gated on. Not the
    /// roster: a coworker an org-mate shared is `roster_for`'s, never this.
    pub async fn coworkers_for(&self, account_id: &AccountId) -> StoreResult<Vec<CoworkerView>> {
        let rows = sqlx::query(
            "select id, name, model, box_id, retired, updated_at_ms, members, role, visibility,
                    effort, source
             from coworker_view
             where account_id = $1 and retired = false
             order by updated_at_ms desc",
        )
        .bind(account_id.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(coworker_view_row).collect()
    }

    /// Which of this account's coworkers they have hidden from their own sidebar.
    pub async fn hidden_coworker_ids(
        &self,
        account_id: &AccountId,
    ) -> StoreResult<HashSet<String>> {
        let rows = sqlx::query("select coworker_id from coworker_hidden where account_id = $1")
            .bind(account_id.as_str())
            .fetch_all(&self.pool)
            .await?;
        let mut ids = HashSet::new();
        for row in rows {
            ids.insert(row.try_get::<String, _>("coworker_id")?);
        }
        Ok(ids)
    }

    /// Hide or unhide a coworker on this account's sidebar. Idempotent.
    pub async fn set_coworker_hidden(
        &self,
        account_id: &AccountId,
        coworker_id: &CoworkerId,
        hidden: bool,
        at_ms: i64,
    ) -> StoreResult<()> {
        if hidden {
            sqlx::query(
                "insert into coworker_hidden (account_id, coworker_id, hidden_at_ms)
                 values ($1, $2, $3)
                 on conflict (account_id, coworker_id) do nothing",
            )
            .bind(account_id.as_str())
            .bind(coworker_id.as_str())
            .bind(at_ms)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query("delete from coworker_hidden where account_id = $1 and coworker_id = $2")
                .bind(account_id.as_str())
                .bind(coworker_id.as_str())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
}

/// Who hired a coworker, for a roster row that may not be the reader's own.
#[derive(Debug, Clone)]
pub struct RosterOwner {
    pub id: AccountId,
    pub first_name: String,
    pub last_name: String,
    pub org_id: Option<String>,
}

fn roster_owner_row(row: &sqlx::postgres::PgRow) -> StoreResult<RosterOwner> {
    Ok(RosterOwner {
        id: AccountId::from_stored(row.try_get::<String, _>("owner_id")?),
        first_name: row.try_get("owner_first")?,
        last_name: row.try_get("owner_last")?,
        // Blank is no org, as `Account` reads the same column.
        org_id: row
            .try_get::<Option<String>, _>("owner_org")?
            .filter(|org| !org.is_empty()),
    })
}

fn coworker_view_row(row: &sqlx::postgres::PgRow) -> StoreResult<CoworkerView> {
    Ok(CoworkerView {
        id: CoworkerId::from_stored(row.try_get::<String, _>("id")?),
        name: row.try_get("name")?,
        model: row.try_get("model")?,
        box_id: row
            .try_get::<Option<String>, _>("box_id")?
            .map(BoxId::from_stored),
        retired: row.try_get("retired")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        members: serde_json::from_value(row.try_get::<serde_json::Value, _>("members")?)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        role: row.try_get("role")?,
        // An unrecognised string reads as private: the safe answer, and the only one that cannot
        // accidentally share a coworker nobody chose to share.
        visibility: row
            .try_get::<Option<String>, _>("visibility")?
            .and_then(|text| opengrok_core::coworker::Visibility::parse(&text))
            .unwrap_or_default(),
        // An unrecognised word reads as inherit: no `reasoning_effort` sent, the route's default.
        effort: opengrok_core::coworker::Effort::parse(&row.try_get::<String, _>("effort")?)
            .unwrap_or_default(),
        // An unrecognised word reads as none, the driving person's own setting, never a guess.
        source: row
            .try_get::<Option<&str>, _>("source")?
            .and_then(SourceKind::parse),
    })
}
