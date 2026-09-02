//! State a second replica must see.
//!
//! Three maps lived in one process because one process was all there was: the browser logins
//! waiting for their poll, the OAuth codes waiting for their exchange, and the answered MCP
//! yeses waiting for their retry. Behind a load balancer each of those is a request that lands
//! on the wrong replica and finds nothing. Each is a row now, with a TTL and a one-shot
//! `delete … returning` take, so two replicas racing for the same row cannot both win — the
//! database decides, not a lock.
//!
//! What stays in the process, on purpose: rate-limit budgets and caches (per replica by
//! nature, `auth/budget.rs`), and the per-coworker MCP lock, which serialises a retry against
//! an approve on ONE replica; across replicas the take below is the whole race.
//!
//! What stays in the process and is NOT yet right for a second replica: the SSE broadcast
//! channel and its ordered sequence counters (`gateway/live.rs`), the host-settings mutex and
//! the reverse-exec broker. Behind a balancer a second replica still breaks the event stream
//! and lets sequences go backwards. This module makes the three one-shot handoffs correct; it
//! does not make the server multi-replica. That is the roadmap's next line, not this one.

use opengrok_core::id::CoworkerId;
use serde_json::Value;
use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

/// An authorization code's binding, as it waits between consent and exchange. Ids are plain
/// strings here: the store does not know who is behind a code, it only keeps the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCodeRow {
    pub code: String,
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub resource: String,
    pub account_id: String,
    pub coworker_id: String,
    pub at_ms: i64,
}

/// An answered MCP card's yes, as it is remembered for the retry.
#[derive(Debug, Clone, Copy)]
pub struct AllowOnce<'a> {
    pub coworker: &'a CoworkerId,
    pub tool: &'a str,
    pub arguments: &'a Value,
    pub call_id: &'a str,
    /// `true` when the yes answered a policy (gate) ask, `false` for the judge's.
    pub gate: bool,
    pub at_ms: i64,
}

impl PgStore {
    /// `/loginDeepControl` (GET): register a challenge under its uuid, unauthenticated. A
    /// re-open with the same uuid starts over (the email binding is dropped), as the map did.
    /// Sweeps what has expired while it is here, so an abandoned login never lingers.
    pub async fn register_login(
        &self,
        uuid: &str,
        challenge: &str,
        at_ms: i64,
        ttl_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query("delete from pending_login where at_ms <= $1")
            .bind(at_ms - ttl_ms)
            .execute(self.pool())
            .await?;
        sqlx::query(
            "insert into pending_login (uuid, challenge, email, at_ms) values ($1, $2, null, $3)
             on conflict (uuid) do update
                set challenge = excluded.challenge, email = null, at_ms = excluded.at_ms",
        )
        .bind(uuid)
        .bind(challenge)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// `/loginDeepControl` (POST, credentials checked by the caller): bind the uuid to the
    /// account that signed in. `false` when the challenge does not match or has expired — the
    /// page then asks the person to start over.
    pub async fn bind_login(
        &self,
        uuid: &str,
        challenge: &str,
        email: &str,
        at_ms: i64,
        ttl_ms: i64,
    ) -> StoreResult<bool> {
        let done = sqlx::query(
            "update pending_login set email = $3, at_ms = $4
             where uuid = $1 and challenge = $2 and at_ms > $5",
        )
        .bind(uuid)
        .bind(challenge)
        .bind(email)
        .bind(at_ms)
        .bind(at_ms - ttl_ms)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// `/auth/poll`: TAKE the completed login whose registered challenge is `challenge` (the
    /// hash of the verifier the client holds). `None` reads as pending — a wrong verifier, an
    /// unbound uuid and an expired one are indistinguishable to a caller, by design.
    pub async fn take_login(
        &self,
        uuid: &str,
        challenge: &str,
        at_ms: i64,
        ttl_ms: i64,
    ) -> StoreResult<Option<String>> {
        let row = sqlx::query(
            "delete from pending_login
             where uuid = $1 and challenge = $2 and email is not null and at_ms > $3
             returning email",
        )
        .bind(uuid)
        .bind(challenge)
        .bind(at_ms - ttl_ms)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| row.try_get("email"))
            .transpose()
            .map_err(Into::into)
    }

    /// Consent given: the code and everything the exchange must match it against.
    pub async fn insert_oauth_code(&self, row: &OAuthCodeRow, ttl_ms: i64) -> StoreResult<()> {
        sqlx::query("delete from oauth_code where at_ms <= $1")
            .bind(row.at_ms - ttl_ms)
            .execute(self.pool())
            .await?;
        sqlx::query(
            "insert into oauth_code
                (code, client_id, client_name, redirect_uri, code_challenge, resource,
                 account_id, coworker_id, at_ms)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&row.code)
        .bind(&row.client_id)
        .bind(&row.client_name)
        .bind(&row.redirect_uri)
        .bind(&row.code_challenge)
        .bind(&row.resource)
        .bind(&row.account_id)
        .bind(&row.coworker_id)
        .bind(row.at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// TAKE a code: one exchange, ever, on whichever replica the token request lands. The
    /// caller still checks the TTL, the client, the redirect, the verifier and the resource —
    /// a code taken and then refused is spent all the same, which is the point.
    pub async fn take_oauth_code(&self, code: &str) -> StoreResult<Option<OAuthCodeRow>> {
        let row = sqlx::query(
            "delete from oauth_code where code = $1
             returning code, client_id, client_name, redirect_uri, code_challenge, resource,
                       account_id, coworker_id, at_ms",
        )
        .bind(code)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            Ok(OAuthCodeRow {
                code: row.try_get("code")?,
                client_id: row.try_get("client_id")?,
                client_name: row.try_get("client_name")?,
                redirect_uri: row.try_get("redirect_uri")?,
                code_challenge: row.try_get("code_challenge")?,
                resource: row.try_get("resource")?,
                account_id: row.try_get("account_id")?,
                coworker_id: row.try_get("coworker_id")?,
                at_ms: row.try_get("at_ms")?,
            })
        })
        .transpose()
    }

    /// An answered MCP card: this coworker may retry this exact tool+arguments once, under the
    /// card's call id. `gate` says which ask the yes answered (a policy grant's or the judge's).
    pub async fn remember_mcp_allow_once(
        &self,
        yes: AllowOnce<'_>,
        ttl_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query("delete from mcp_allow_once where at_ms <= $1")
            .bind(yes.at_ms - ttl_ms)
            .execute(self.pool())
            .await?;
        sqlx::query(
            "insert into mcp_allow_once (coworker_id, tool, arguments, call_id, gate, at_ms)
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(yes.coworker.as_str())
        .bind(yes.tool)
        .bind(yes.arguments)
        .bind(yes.call_id)
        .bind(yes.gate)
        .bind(yes.at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// TAKE the pending yes for this coworker+tool+arguments: `(call_id, gate, at_ms)` — the
    /// `at_ms` is the yes's ORIGINAL stamp, which a give-back must carry so a retry loop against
    /// a down computer cannot keep one approval alive past its TTL. Matched as jsonb, which is
    /// equality of VALUE — key order is not part of it, which is what a yes that round-tripped
    /// through the card needs (and looser than Rust `Value` equality: `1` and `1.0` are one
    /// number to Postgres). Oldest first; `skip locked` so two replicas taking at once get two
    /// different rows or one gets none, never the same one twice.
    pub async fn take_mcp_allow_once(
        &self,
        coworker: &CoworkerId,
        tool: &str,
        arguments: &Value,
        at_ms: i64,
        ttl_ms: i64,
    ) -> StoreResult<Option<(String, bool, i64)>> {
        let row = sqlx::query(
            "delete from mcp_allow_once where id = (
                 select id from mcp_allow_once
                 where coworker_id = $1 and tool = $2 and arguments = $3 and at_ms > $4
                 order by id limit 1 for update skip locked)
             returning call_id, gate, at_ms",
        )
        .bind(coworker.as_str())
        .bind(tool)
        .bind(arguments)
        .bind(at_ms - ttl_ms)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            Ok((
                row.try_get("call_id")?,
                row.try_get("gate")?,
                row.try_get("at_ms")?,
            ))
        })
        .transpose()
    }
}
