//! Delete every account but the ones named, with everything those accounts own.
//!
//! The dev database once held 2,249 fixture accounts from an integration suite that had been
//! pointed at it, and their 104 schedules fired all day on the deployment's key. Nothing could
//! delete them: the schema has four foreign keys and no cascade, and the store had no notion of
//! removing an account. This is that notion, written as an allowlist rather than a pattern,
//! because "everything that is not yours" is the only rule that cannot miss a fixture shape.
//!
//! One transaction, children first. A dry run does the same work and rolls it back, so its
//! report is what the real run would do on the same rows. Run it against a stopped server: the
//! id lists are read at the start, so a coworker hired or a run started while the purge runs
//! would be left behind naming an account that is gone.

use std::collections::BTreeMap;

use sqlx::Row;

use crate::postgres::PgStore;
use crate::{StoreError, StoreResult};

/// What a purge did, or would do: the accounts it keeps, and rows removed per table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// `(email, account id)` of every account that survives.
    pub kept: Vec<(String, String)>,
    pub accounts_deleted: u64,
    pub coworkers_deleted: u64,
    pub orgs_deleted: u64,
    /// Rows deleted, by table, for every table the purge touched.
    pub rows: BTreeMap<&'static str, u64>,
}

impl PgStore {
    /// Delete every account whose email is not in `keep`, and everything it owns. Errors before
    /// touching anything when `keep` is empty or names an email that does not exist: a typo must
    /// not turn into an empty database.
    pub async fn purge_accounts_except(
        &self,
        keep: &[String],
        dry_run: bool,
    ) -> StoreResult<PurgeReport> {
        let keep: Vec<String> = keep
            .iter()
            .map(|email| email.trim().to_string())
            .filter(|email| !email.is_empty())
            .collect();
        if keep.is_empty() {
            return Err(StoreError::Database(
                "refusing to purge with an empty allowlist".to_string(),
            ));
        }

        let mut tx = self.pool().begin().await?;
        // One purge at a time. The id lists are read once and used as literals below, so a
        // second purge interleaving with this one would delete around a moving target.
        sqlx::query("select pg_advisory_xact_lock(7_324_001)")
            .execute(&mut *tx)
            .await?;

        let kept_rows = sqlx::query(
            "select id, email, org_id from account_view \
             where lower(email) = any(select lower(k) from unnest($1::text[]) as k)",
        )
        .bind(&keep)
        .fetch_all(&mut *tx)
        .await?;
        let mut report = PurgeReport::default();
        let mut kept_ids: Vec<String> = Vec::new();
        let mut kept_orgs: Vec<String> = Vec::new();
        for row in &kept_rows {
            let id: String = row.try_get("id")?;
            let email: String = row.try_get("email")?;
            if let Some(org) = row
                .try_get::<Option<String>, _>("org_id")?
                .filter(|org| !org.trim().is_empty())
            {
                kept_orgs.push(org);
            }
            kept_ids.push(id.clone());
            report.kept.push((email, id));
        }
        for email in &keep {
            if !report
                .kept
                .iter()
                .any(|(kept, _)| kept.eq_ignore_ascii_case(email))
            {
                return Err(StoreError::Database(format!(
                    "refusing to purge: allowlisted account {email} does not exist"
                )));
            }
        }

        let accounts: Vec<String> =
            sqlx::query_scalar("select id from account_view where not (id = any($1))")
                .bind(&kept_ids)
                .fetch_all(&mut *tx)
                .await?;
        let emails: Vec<String> =
            sqlx::query_scalar("select email from account_view where not (id = any($1))")
                .bind(&kept_ids)
                .fetch_all(&mut *tx)
                .await?;
        // An org survives when a kept account belongs to it or administers it. Everything else
        // goes — and "everything else" is decided per org, never by an empty list turning into
        // "all of them".
        let orgs: Vec<String> = sqlx::query_scalar(
            "select id from org_view where not (id = any($1)) and not (admin_id = any($2))",
        )
        .bind(&kept_orgs)
        .bind(&kept_ids)
        .fetch_all(&mut *tx)
        .await?;
        let coworkers: Vec<String> =
            sqlx::query_scalar("select id from coworker_view where account_id = any($1)")
                .bind(&accounts)
                .fetch_all(&mut *tx)
                .await?;
        let schedules: Vec<String> = sqlx::query_scalar(
            "select id from schedule_view where account_id = any($1) or coworker_id = any($2)",
        )
        .bind(&accounts)
        .bind(&coworkers)
        .fetch_all(&mut *tx)
        .await?;
        let monitors: Vec<String> = sqlx::query_scalar(
            "select id from monitor_view where account_id = any($1) or coworker_id = any($2)",
        )
        .bind(&accounts)
        .bind(&coworkers)
        .fetch_all(&mut *tx)
        .await?;
        let connections: Vec<String> = sqlx::query_scalar(
            "select id from connection_view where owner_id = any($1) or owner_id = any($2) \
             or owner_id = any($3)",
        )
        .bind(&accounts)
        .bind(&coworkers)
        .bind(&orgs)
        .fetch_all(&mut *tx)
        .await?;
        let templates: Vec<String> =
            sqlx::query_scalar("select id from coworker_template where org_id = any($1)")
                .bind(&orgs)
                .fetch_all(&mut *tx)
                .await?;
        let recipes: Vec<String> = sqlx::query_scalar(
            "select id from recipe where owner_id = any($1) or org_id = any($2)",
        )
        .bind(&accounts)
        .bind(&orgs)
        .fetch_all(&mut *tx)
        .await?;
        // A run belongs to a doomed account, or sits on a doomed thread: a coworker's chat, a
        // schedule's firings, or the MCP door's audit thread for that coworker.
        let mcp_threads: Vec<String> = coworkers.iter().map(|id| format!("mcp-{id}")).collect();
        let runs: Vec<String> = sqlx::query_scalar(
            "select id from run_view where account_id = any($1) or thread_id = any($2) \
             or thread_id = any($3) or thread_id = any($4)",
        )
        .bind(&accounts)
        .bind(&coworkers)
        .bind(&schedules)
        .bind(&mcp_threads)
        .fetch_all(&mut *tx)
        .await?;

        // The journal: every aggregate above is a stream in `events`, keyed `<type>/<id>` as the
        // `*_stream` helpers in `lib.rs` spell it.
        let mut streams: Vec<String> = Vec::new();
        streams.extend(accounts.iter().map(|id| format!("account/{id}")));
        streams.extend(coworkers.iter().map(|id| format!("coworker/{id}")));
        streams.extend(schedules.iter().map(|id| format!("schedule/{id}")));
        streams.extend(orgs.iter().map(|id| format!("org/{id}")));
        streams.extend(monitors.iter().map(|id| format!("monitor/{id}")));
        streams.extend(runs.iter().map(|id| format!("run/{id}")));
        streams.extend(connections.iter().map(|id| format!("connection/{id}")));

        // Vault keys embed an owner id (`org-computer:<org>:<kind>`, `conn_<connector>_<acct>`),
        // so the id as a LIKE infix is the handle — with `_` and `%` backslash-escaped (LIKE's
        // default escape; `LIKE ANY (subquery)` takes no ESCAPE clause), since every id has an
        // underscore in it.
        let like_escape = |id: &String| {
            id.replace('\\', "\\\\")
                .replace('_', "\\_")
                .replace('%', "\\%")
        };
        let mut owners: Vec<String> = Vec::new();
        owners.extend(accounts.iter().map(like_escape));
        owners.extend(coworkers.iter().map(like_escape));
        owners.extend(orgs.iter().map(like_escape));

        macro_rules! delete {
            ($table:literal, $sql:literal $(, $bind:expr)*) => {{
                let done = sqlx::query($sql)$(.bind($bind))*.execute(&mut *tx).await?;
                *report.rows.entry($table).or_insert(0) += done.rows_affected();
            }};
        }

        delete!(
            "events",
            "delete from events where stream_id = any($1)",
            &streams
        );
        delete!(
            "monitor_firing",
            "delete from monitor_firing where monitor_id = any($1) or run_id = any($2)",
            &monitors,
            &runs
        );
        delete!(
            "monitor_view",
            "delete from monitor_view where id = any($1)",
            &monitors
        );
        delete!(
            "room_pause",
            "delete from room_pause where run_id = any($1) or group_id = any($2) or member_id = any($2)",
            &runs,
            &coworkers
        );
        delete!(
            "artifact",
            "delete from artifact where account_id = any($1) or run_id = any($2)",
            &accounts,
            &runs
        );
        delete!(
            "recipe_run",
            "delete from recipe_run where recipe_id = any($1) or coworker_id = any($2) or run_id = any($3)",
            &recipes,
            &coworkers,
            &runs
        );
        delete!(
            "recipe_grant",
            "delete from recipe_grant where recipe_id = any($1) or coworker_id = any($2)",
            &recipes,
            &coworkers
        );
        delete!(
            "recipe_share",
            "delete from recipe_share where recipe_id = any($1) or scope_id = any($2) or scope_id = any($3)",
            &recipes,
            &accounts,
            &orgs
        );
        delete!(
            "recipe_version",
            "delete from recipe_version where recipe_id = any($1)",
            &recipes
        );
        delete!("recipe", "delete from recipe where id = any($1)", &recipes);
        delete!("run_view", "delete from run_view where id = any($1)", &runs);
        delete!(
            "schedule_view",
            "delete from schedule_view where id = any($1)",
            &schedules
        );
        delete!(
            "gateway_entry",
            "delete from gateway_entry where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "gateway_nonce",
            "delete from gateway_nonce where split_part(account_slot, ':', 1) = any($1)",
            &accounts
        );
        delete!(
            "mcp_allow_once",
            "delete from mcp_allow_once where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "mcp_call_audit",
            "delete from mcp_call_audit where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "bot_key_view",
            "delete from bot_key_view where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "coworker_gateway_key",
            "delete from coworker_gateway_key where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "coworker_hidden",
            "delete from coworker_hidden where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "coworker_last_viewed",
            "delete from coworker_last_viewed where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "credential_hint",
            "delete from credential_hint where coworker_id = any($1) or account_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "coworker_template_use",
            "delete from coworker_template_use where coworker_id = any($1) or template_id = any($2)",
            &coworkers,
            &templates
        );
        delete!(
            "ceiling_view",
            "delete from ceiling_view where coworker_id = any($1)",
            &coworkers
        );
        delete!(
            "grant_view",
            "delete from grant_view where coworker_id = any($1) or principal_id = any($2)",
            &coworkers,
            &accounts
        );
        delete!(
            "connection_loan",
            "delete from connection_loan where coworker_id = any($1) or connection_id = any($2)",
            &coworkers,
            &connections
        );
        delete!(
            "connection_view",
            "delete from connection_view where id = any($1)",
            &connections
        );
        delete!(
            "seamb_profile",
            "delete from seamb_profile where coworker_id = any($1)",
            &coworkers
        );
        delete!(
            "oauth_code",
            "delete from oauth_code where account_id = any($1) or coworker_id = any($2)",
            &accounts,
            &coworkers
        );
        delete!(
            "oauth_refresh_token",
            "delete from oauth_refresh_token where account_id = any($1) or coworker_id = any($2)",
            &accounts,
            &coworkers
        );
        delete!(
            "local_exec_audit",
            "delete from local_exec_audit where account_id = any($1)",
            &accounts
        );
        delete!(
            "local_exec_daemon",
            "delete from local_exec_daemon where account_id = any($1)",
            &accounts
        );
        delete!(
            "local_exec_policy",
            "delete from local_exec_policy where account_id = any($1)",
            &accounts
        );
        delete!(
            "local_exec_rule",
            "delete from local_exec_rule where account_id = any($1)",
            &accounts
        );
        delete!(
            "auto_review_policy",
            "delete from auto_review_policy where account_id = any($1) or scope_id = any($2)",
            &accounts,
            &coworkers
        );
        delete!(
            "webauthn_credential",
            "delete from webauthn_credential where account_id = any($1)",
            &accounts
        );
        delete!(
            "session_view",
            "delete from session_view where account_id = any($1)",
            &accounts
        );
        delete!(
            "pending_login",
            "delete from pending_login where lower(email) = any($1)",
            &emails
                .iter()
                .map(|e| e.to_ascii_lowercase())
                .collect::<Vec<_>>()
        );
        delete!(
            "account_computer",
            "delete from account_computer where account_id = any($1)",
            &accounts
        );
        delete!(
            "account_computer_error",
            "delete from account_computer_error where account_id = any($1)",
            &accounts
        );
        // By scope only: `org_id` on a box row is the idle sweep's bookkeeping, not ownership,
        // and a kept account's box can carry a doomed org's id.
        delete!(
            "scoped_computer",
            "delete from scoped_computer where scope_id = any($1) or scope_id = any($2) or scope_id = any($3)",
            &accounts,
            &coworkers,
            &orgs
        );
        delete!(
            "computer_sharing",
            "delete from computer_sharing where scope_id = any($1) or scope_id = any($2) or scope_id = any($3)",
            &accounts,
            &coworkers,
            &orgs
        );
        delete!(
            "box_update",
            "delete from box_update where scope_id = any($1) or scope_id = any($2) or scope_id = any($3)",
            &accounts,
            &coworkers,
            &orgs
        );
        delete!(
            "spend_limit",
            "delete from spend_limit where scope_id = any($1) or scope_id = any($2) or scope_id = any($3)",
            &accounts,
            &coworkers,
            &orgs
        );
        delete!(
            "points_limit",
            "delete from points_limit where scope_id = any($1) or scope_id = any($2) or scope_id = any($3)",
            &accounts,
            &coworkers,
            &orgs
        );
        // Vault rows are keyed by the thing they belong to (`org-computer:<org>:<kind>` and the
        // like), so an owner's id somewhere in the key is the only handle there is.
        delete!(
            "secret_store",
            "delete from secret_store where id like any (select '%' || u || '%' from unnest($1::text[]) as u)",
            &owners
        );
        delete!(
            "coworker_template",
            "delete from coworker_template where id = any($1)",
            &templates
        );
        delete!(
            "gateway_key_view",
            "delete from gateway_key_view where org_id = any($1) or member_account_id = any($2)",
            &orgs,
            &accounts
        );
        delete!(
            "org_invite",
            "delete from org_invite where org_id = any($1)",
            &orgs
        );
        delete!(
            "coworker_view",
            "delete from coworker_view where id = any($1)",
            &coworkers
        );
        delete!("org_view", "delete from org_view where id = any($1)", &orgs);
        delete!(
            "account_view",
            "delete from account_view where id = any($1)",
            &accounts
        );

        report.accounts_deleted = report.rows.get("account_view").copied().unwrap_or(0);
        report.coworkers_deleted = report.rows.get("coworker_view").copied().unwrap_or(0);
        report.orgs_deleted = report.rows.get("org_view").copied().unwrap_or(0);

        if dry_run {
            tx.rollback().await?;
        } else {
            tx.commit().await?;
        }
        Ok(report)
    }
}
