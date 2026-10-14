//! Connections: an authentication that happened, and who may borrow it.

use super::*;

/// What a connection write says about the credential itself.
///
/// Grouped because the three always travel together and always mean one thing: "here is the token
/// this write learned, and when it dies". `None` for the secret means the write is about the
/// connection rather than its credential — a lend, a revoke — and must leave the stored one alone.
#[derive(Debug, Clone, Copy)]
pub struct CredentialUpdate<'a> {
    pub secret: Option<&'a Sealed>,
    pub expires_at_ms: Option<i64>,
    pub at_ms: i64,
}

impl<'a> CredentialUpdate<'a> {
    /// A write that changes nothing about the credential.
    pub fn none(at_ms: i64) -> Self {
        Self {
            secret: None,
            expires_at_ms: None,
            at_ms,
        }
    }

    pub fn sealed(secret: &'a Sealed, expires_at_ms: Option<i64>, at_ms: i64) -> Self {
        Self {
            secret: Some(secret),
            expires_at_ms,
            at_ms,
        }
    }
}

/// Connections: an authentication that happened, and who may borrow it.
impl PgStore {
    pub async fn load_connection(&self, id: &str) -> StoreResult<(Connection, i64)> {
        let (events, seq) = self
            .stream::<ConnectionEvent>(&format!("connection/{id}"))
            .await?;
        Ok((Connection::replay(&events), seq))
    }

    /// Append connection events, refresh the projections, and seal the credential — all in one
    /// transaction.
    ///
    /// The secret rides along rather than being written separately: a connection whose row exists
    /// without its credential is one that resolves, is chosen, and then fails at the moment of use.
    pub async fn append_connection(
        &self,
        id: &str,
        expected_seq: i64,
        events: &[ConnectionEvent],
        state: &Connection,
        update: &CredentialUpdate<'_>,
    ) -> StoreResult<i64> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::append_connection_in(&mut tx, id, expected_seq, events, state, update);
        let seq = seq.await?;
        tx.commit().await?;
        Ok(seq)
    }

    /// `append_connection` in a transaction the caller holds: a pasted token is saved under its
    /// installation's row lock, so an uninstall cannot land between the two (#359).
    pub async fn append_connection_in(
        tx: &mut sqlx::PgConnection,
        id: &str,
        expected_seq: i64,
        events: &[ConnectionEvent],
        state: &Connection,
        update: &CredentialUpdate<'_>,
    ) -> StoreResult<i64> {
        let CredentialUpdate {
            secret,
            expires_at_ms,
            at_ms,
        } = *update;
        let stream = format!("connection/{id}");
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

        let (scope, owner_id) = match &state.owner {
            Some(Owner::Global) => ("global", None),
            Some(Owner::User(account)) => ("user", Some(account.as_str().to_string())),
            Some(Owner::Bot(coworker)) => ("bot", Some(coworker.as_str().to_string())),
            None => ("global", None),
        };

        sqlx::query(
            "insert into connection_view (id, connector, scope, owner_id, label, disconnected,
               updated_at_ms, expires_at_ms, kind)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             on conflict (id) do update set
               connector = excluded.connector,
               scope = excluded.scope,
               owner_id = excluded.owner_id,
               label = excluded.label,
               disconnected = excluded.disconnected,
               updated_at_ms = excluded.updated_at_ms,
               -- Kept when a caller has nothing newer to say, so a lend does not erase the expiry
               -- the last token exchange recorded.
               expires_at_ms = coalesce(excluded.expires_at_ms, connection_view.expires_at_ms),
               kind = excluded.kind",
        )
        .bind(id)
        .bind(&state.connector)
        .bind(scope)
        .bind(owner_id)
        .bind(&state.label)
        .bind(state.disconnected)
        .bind(at_ms)
        .bind(expires_at_ms)
        .bind(state.kind.word())
        .execute(&mut *tx)
        .await?;

        // Loans are rewritten wholesale from the aggregate rather than patched per event: the
        // aggregate is the truth, and reconstructing beats trying to keep two views in step.
        let unlend = sqlx::query("delete from connection_loan where connection_id = $1");
        unlend.bind(id).execute(&mut *tx).await?;
        for coworker in &state.loans {
            sqlx::query(
                "insert into connection_loan (connection_id, coworker_id, updated_at_ms)
                 values ($1, $2, $3)",
            )
            .bind(id)
            .bind(coworker.as_str())
            .bind(at_ms)
            .execute(&mut *tx)
            .await?;
        }

        if let Some(sealed) = secret {
            crate::vault_rows::write_sealed(&mut *tx, id, sealed, at_ms).await?;
        }

        // A disconnected connection keeps its record and loses its credential. The row saying it
        // once existed is worth keeping; the token is not. A pasted token's is kept under the id
        // its plugin binding names, which sealed it; and the pins naming it go too (#359), so no
        // Bot is left set to an account that is gone.
        if state.disconnected {
            sqlx::query(
                "delete from secret_store where id = $1
                    or id in (select secret_id from plugin_credential where connection_id = $1)",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            let unpin = sqlx::query("delete from bot_connection_pin where connection_id = $1");
            unpin.bind(id).execute(&mut *tx).await?;
        }
        Ok(seq)
    }

    /// Every connection this coworker could possibly use, for `connection::resolve` to choose from.
    ///
    /// Deliberately returns candidates rather than picking: the choosing rule is pure, tested, and
    /// belongs in the domain — not in a SQL `order by` nobody can unit-test.
    pub async fn connections_for(
        &self,
        account: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<Vec<ConnectionView>> {
        let rows = sqlx::query(
            "select v.id, v.connector, v.scope, v.owner_id, v.label, v.updated_at_ms,
                    v.expires_at_ms, v.kind
               from connection_view v
              where v.disconnected = false
                and ( v.scope = 'global'
                   or (v.scope = 'user' and v.owner_id = $1)
                   or (v.scope = 'bot'  and v.owner_id = $2) )
              order by v.connector, v.label, v.id",
        )
        .bind(account.as_str())
        .bind(coworker.as_str())
        .fetch_all(&self.pool)
        .await?;
        self.connection_views(rows).await
    }

    /// Every connection this account owns, for showing a person what they have connected.
    pub async fn connections_owned_by(
        &self,
        account: &AccountId,
    ) -> StoreResult<Vec<ConnectionView>> {
        let rows = sqlx::query(
            "select id, connector, scope, owner_id, label, updated_at_ms, expires_at_ms, kind
               from connection_view
              where scope = 'user' and owner_id = $1 and disconnected = false
              order by connector, label, id",
        )
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await?;
        self.connection_views(rows).await
    }

    /// `connection_view` rows as views, each with its loans.
    async fn connection_views(
        &self,
        rows: Vec<sqlx::postgres::PgRow>,
    ) -> StoreResult<Vec<ConnectionView>> {
        let mut views = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let owner = match (
                row.try_get::<String, _>("scope")?.as_str(),
                row.try_get::<Option<String>, _>("owner_id")?,
            ) {
                ("user", Some(id)) => Owner::User(AccountId::from_stored(id)),
                ("bot", Some(id)) => Owner::Bot(CoworkerId::from_stored(id)),
                _ => Owner::Global,
            };
            let kind: String = row.try_get("kind")?;
            let kind = opengrok_core::connection::ConnectionKind::from_word(&kind)
                .ok_or_else(|| StoreError::Corrupt(format!("connection kind {kind}")))?;
            let loans = "select coworker_id from connection_loan where connection_id = $1";
            let loans: Vec<String> = sqlx::query_scalar(loans)
                .bind(&id)
                .fetch_all(&self.pool)
                .await?;
            views.push(ConnectionView {
                id,
                connector: row.try_get("connector")?,
                owner,
                label: row.try_get("label")?,
                loans: loans.into_iter().map(CoworkerId::from_stored).collect(),
                updated_at_ms: row.try_get("updated_at_ms")?,
                expires_at_ms: row.try_get("expires_at_ms")?,
                kind,
            });
        }
        Ok(views)
    }

    /// Drop a sealed secret by id. Idempotent: a secret already gone is the outcome asked for.
    pub async fn delete_secret(&self, id: &str) -> StoreResult<()> {
        let delete = sqlx::query("delete from secret_store where id = $1");
        delete.bind(id).execute(self.pool()).await?;
        Ok(())
    }

    /// Store a sealed secret on its own, outside a connection's transaction.
    ///
    /// Used for the refresh token, which lives in its own row: it outlives the access token, and
    /// keeping them apart means rotating one does not disturb the other.
    pub async fn put_secret(&self, id: &str, sealed: &Sealed, at_ms: i64) -> StoreResult<()> {
        crate::vault_rows::write_sealed(&self.pool, id, sealed, at_ms).await
    }

    /// Record a new expiry after a refresh, without touching the event log.
    ///
    /// A refresh is not a domain event: nothing about who owns the connection or who may borrow it
    /// changed, and writing one per hour would bury the events that matter.
    pub async fn touch_expiry(
        &self,
        id: &str,
        expires_at_ms: Option<i64>,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "update connection_view set expires_at_ms = $2, updated_at_ms = $3 where id = $1",
        )
        .bind(id)
        .bind(expires_at_ms)
        .bind(at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A saved login may land only on a bot that is this person's own and shown to nobody
    /// else: an org-visible bot is driven by every member of the org, so a session left in its
    /// box would be theirs too.
    pub async fn coworker_is_private_and_owned_by(
        &self,
        account_id: &AccountId,
        coworker: &CoworkerId,
    ) -> StoreResult<bool> {
        let row = sqlx::query(
            "select 1 as ok from coworker_view
             where id = $2 and account_id = $1 and visibility = 'private'",
        )
        .bind(account_id.as_str())
        .bind(coworker.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }
}
