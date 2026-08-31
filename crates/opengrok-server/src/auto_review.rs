//! Auto-review: two tiers, one gate. Design and rationale: `docs/AUTO-REVIEW.md`.
//!
//! This is the ADDITIVE half — the tier rows, their resolution, and the account-facing endpoints
//! the settings surfaces write against. Enforcement (the judge in the tool executor and a real
//! `resolveAutoReviewApproval`) is the second half and lands separately.
//!
//! TWO tiers, not three: global, overridden per coworker. "What may bots do on THIS machine" is
//! already that machine's standing rules in the local-exec policy; a device tier here would be a
//! second answer to the same question, and the user asked for one answer per question.
//!
//! Precedence is per FIELD, not per row: a coworker row that sets only `enabled` still inherits
//! its instructions from the global tier. That is what "override" means for a settings UI with
//! independent controls — and it is the only reading under which "clear this override" (store
//! null) is expressible.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use opengrok_store::auto_review::AutoReviewRow;

use crate::AuthState;

/// `machine` is deliberately absent — see the module note. A client that still sends it gets a
/// 422 naming the two scopes that exist, never a silently ignored row.
const SCOPE_KINDS: &[&str] = &["global", "coworker"];

/// Instruction text is user-written prose, not code — but it is also what the judge reads on
/// every reviewed action, so an unbounded blob is a cost and a prompt-stuffing surface. The
/// client already caps at 20 instructions × 1000 chars; this is the server's own ceiling.
const MAX_INSTRUCTIONS_CHARS: usize = 20_000;

/// Which tier decided a field of the effective policy — so the settings UI can say
/// "inherited from …" truthfully instead of re-deriving precedence on its side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecidedBy {
    Coworker,
    Global,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decided {
    pub enabled: DecidedBy,
    pub allow_instructions: DecidedBy,
    pub block_instructions: DecidedBy,
}

/// The resolved policy for one (account, coworker) — what the gate would judge with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePolicy {
    pub enabled: bool,
    pub allow_instructions: String,
    pub block_instructions: String,
    pub decided_by: Decided,
}

impl EffectivePolicy {
    /// THE SHORT-CIRCUIT (`docs/AUTO-REVIEW.md` §3, a user requirement): off, or on with nothing
    /// written, means no judge call and no DB read per tool call. Resolved once per run; this is
    /// the one in-memory test each call pays.
    pub fn is_active(&self) -> bool {
        self.enabled && !(self.allow_instructions.is_empty() && self.block_instructions.is_empty())
    }
}

fn pick<T>(
    tiers: &[(DecidedBy, Option<&AutoReviewRow>)],
    field: impl Fn(&AutoReviewRow) -> Option<T>,
    default: T,
) -> (T, DecidedBy) {
    for (by, row) in tiers {
        if let Some(row) = row
            && let Some(value) = field(row)
        {
            return (value, *by);
        }
    }
    (default, DecidedBy::Default)
}

/// Per field: coworker ?? global ?? default. Defaults are OFF and empty — auto-review is an
/// opt-in the user switches on (unlike the exec channel, whose default is the closed `Never`:
/// that gate guards reaching a machine at all; this one refines what a reachable coworker may do).
pub fn resolve(global: Option<&AutoReviewRow>, coworker: Option<&AutoReviewRow>) -> EffectivePolicy {
    let tiers = [(DecidedBy::Coworker, coworker), (DecidedBy::Global, global)];
    let (enabled, enabled_by) = pick(&tiers, |row| row.enabled, false);
    let (allow_instructions, allow_by) =
        pick(&tiers, |row| row.allow_instructions.clone(), String::new());
    let (block_instructions, block_by) =
        pick(&tiers, |row| row.block_instructions.clone(), String::new());
    EffectivePolicy {
        enabled,
        allow_instructions,
        block_instructions,
        decided_by: Decided {
            enabled: enabled_by,
            allow_instructions: allow_by,
            block_instructions: block_by,
        },
    }
}

/// Resolve from the store for one decision. A store error reads as "nothing written" — the OFF
/// default. That is the honest reading for an opt-in feature (no row was ever proven to exist),
/// and a store that cannot serve this read cannot serve the run's journal either, so the run does
/// not proceed unreviewed on the strength of it. Any row of another scope kind (a legacy
/// `machine` row) is ignored here, never resolved.
pub async fn load_effective(
    store: &opengrok_store::PgStore,
    account_id: &str,
    coworker_id: Option<&str>,
) -> EffectivePolicy {
    let rows = store
        .auto_review_tiers(account_id, coworker_id)
        .await
        .unwrap_or_default();
    let find = |kind: &str| rows.iter().find(|row| row.scope_kind == kind);
    resolve(find("global"), find("coworker"))
}

// ---------------------------------------------------------------------------------------------
// The account-facing management API — `docs/AUTO-REVIEW.md` §6. Account-authed like `/local-exec/*`.
// ---------------------------------------------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(
            "/auto-review/policy",
            get(get_policy).put(set_policy).delete(delete_policy),
        )
        .route("/auto-review/effective", get(get_effective))
        .with_state(state)
}

fn row_json(row: &AutoReviewRow) -> serde_json::Value {
    serde_json::json!({
        "enabled": row.enabled,
        "allowInstructions": row.allow_instructions,
        "blockInstructions": row.block_instructions,
        "updatedAtMs": row.updated_at_ms,
    })
}

/// `GET /auto-review/policy` — every tier row as stored. Null fields mean "inherits"; an absent
/// scope is null. Nothing is pre-resolved: the UI renders inheritance from the raw rows and asks
/// `/auto-review/effective` when it wants the resolved answer.
async fn get_policy(State(state): State<AuthState>, headers: HeaderMap) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let rows = state
        .store
        .auto_review_rows(account_id.as_str())
        .await
        .unwrap_or_default();
    let mut global = serde_json::Value::Null;
    let mut coworkers = serde_json::Map::new();
    for row in &rows {
        match row.scope_kind.as_str() {
            "global" => global = row_json(row),
            "coworker" => {
                coworkers.insert(row.scope_id.clone(), row_json(row));
            }
            _ => {}
        }
    }
    Json(serde_json::json!({
        "global": global,
        "coworkers": coworkers,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PolicyBody {
    scope_kind: String,
    #[serde(default)]
    scope_id: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    allow_instructions: Option<String>,
    #[serde(default)]
    block_instructions: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeBody {
    scope_kind: String,
    #[serde(default)]
    scope_id: String,
}

/// A scope is refused unless it is the caller's own: `global` carries no id; a `coworker` must be
/// one of the account's coworkers. Otherwise a client could park policy rows on ids it does not
/// own — harmless today, but a row that appears the day that id exists here is exactly the kind
/// of surprise a policy store must not hold.
async fn refuse_scope(
    state: &AuthState,
    account_id: &opengrok_core::id::AccountId,
    scope_kind: &str,
    scope_id: &str,
) -> Option<Response> {
    let refuse = |why: &'static str| Some((StatusCode::UNPROCESSABLE_ENTITY, why).into_response());
    if !SCOPE_KINDS.contains(&scope_kind) {
        return refuse("scopeKind must be global|coworker");
    }
    match scope_kind {
        "global" if !scope_id.is_empty() => refuse("a global scope carries no scopeId"),
        "global" => None,
        _ if scope_id.is_empty() => refuse("scopeId is required for this scopeKind"),
        _ => {
            let owned = state
                .store
                .coworkers_for(account_id)
                .await
                .unwrap_or_default()
                .iter()
                .any(|coworker| coworker.id.as_str() == scope_id);
            if owned {
                None
            } else {
                refuse("scopeId is not one of this account's coworkers")
            }
        }
    }
}

/// `PUT /auto-review/policy` — upsert one tier's whole row (null = inherit, not "keep").
async fn set_policy(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<PolicyBody>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let scope_id = body.scope_id.trim();
    if let Some(refusal) = refuse_scope(&state, &account_id, &body.scope_kind, scope_id).await {
        return refusal;
    }
    let allow = body.allow_instructions.as_deref().map(str::trim);
    let block = body.block_instructions.as_deref().map(str::trim);
    let chars = allow.map_or(0, |text| text.chars().count())
        + block.map_or(0, |text| text.chars().count());
    if chars > MAX_INSTRUCTIONS_CHARS {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "instructions are too long for one scope",
        )
            .into_response();
    }
    match state
        .store
        .set_auto_review_policy(
            account_id.as_str(),
            &body.scope_kind,
            scope_id,
            body.enabled,
            allow,
            block,
            now_ms(),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save the policy",
        )
            .into_response(),
    }
}

/// `DELETE /auto-review/policy` — remove a tier's row; that scope inherits fully again.
async fn delete_policy(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<ScopeBody>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    if !SCOPE_KINDS.contains(&body.scope_kind.as_str()) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "scopeKind must be global|coworker",
        )
            .into_response();
    }
    match state
        .store
        .delete_auto_review_policy(account_id.as_str(), &body.scope_kind, body.scope_id.trim())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not delete the policy",
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveQuery {
    #[serde(default)]
    coworker_id: Option<String>,
}

/// `GET /auto-review/effective?coworkerId=…` — the resolved policy plus which tier decided each
/// field. A foreign id simply matches no row (only the caller's rows are read).
async fn get_effective(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Query(query): Query<EffectiveQuery>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let effective = load_effective(
        &state.store,
        account_id.as_str(),
        query.coworker_id.as_deref().filter(|id| !id.is_empty()),
    )
    .await;
    Json(effective).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        kind: &str,
        enabled: Option<bool>,
        allow: Option<&str>,
        block: Option<&str>,
    ) -> AutoReviewRow {
        AutoReviewRow {
            scope_kind: kind.to_string(),
            scope_id: String::new(),
            enabled,
            allow_instructions: allow.map(str::to_string),
            block_instructions: block.map(str::to_string),
            updated_at_ms: 0,
        }
    }

    #[test]
    fn nothing_written_is_off_and_inactive() {
        let effective = resolve(None, None);
        assert!(!effective.enabled);
        assert!(!effective.is_active());
        assert_eq!(effective.decided_by.enabled, DecidedBy::Default);
    }

    #[test]
    fn precedence_is_per_field_coworker_over_global() {
        let global = row("global", Some(true), Some("g-allow"), Some("g-block"));
        let coworker = row("coworker", Some(false), Some("c-allow"), None);
        let effective = resolve(Some(&global), Some(&coworker));
        // enabled: coworker said false — it wins even though global said true.
        assert!(!effective.enabled);
        assert_eq!(effective.decided_by.enabled, DecidedBy::Coworker);
        // allow: coworker overrides global.
        assert_eq!(effective.allow_instructions, "c-allow");
        assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Coworker);
        // block: coworker inherits; only global wrote one.
        assert_eq!(effective.block_instructions, "g-block");
        assert_eq!(effective.decided_by.block_instructions, DecidedBy::Global);
    }

    #[test]
    fn an_explicit_empty_string_stops_inheritance() {
        // The user cleared this coworker's block rules on purpose; global's must not leak back in.
        let global = row("global", Some(true), None, Some("g-block"));
        let coworker = row("coworker", None, None, Some(""));
        let effective = resolve(Some(&global), Some(&coworker));
        assert_eq!(effective.block_instructions, "");
        assert_eq!(effective.decided_by.block_instructions, DecidedBy::Coworker);
    }

    #[test]
    fn short_circuit_needs_enabled_and_at_least_one_instruction() {
        let on_but_empty = row("global", Some(true), Some(""), None);
        assert!(!resolve(Some(&on_but_empty), None).is_active());
        let off_with_rules = row("global", Some(false), Some("x"), Some("y"));
        assert!(!resolve(Some(&off_with_rules), None).is_active());
        let on_with_block = row("global", Some(true), None, Some("never touch prod"));
        assert!(resolve(Some(&on_with_block), None).is_active());
    }

    #[test]
    fn the_only_scopes_are_global_and_coworker() {
        // A device tier would be a second answer to "what on this machine" — the standing rules
        // already answer it. If someone re-adds it, this is the test that asks them why.
        assert_eq!(SCOPE_KINDS, &["global", "coworker"]);
    }
}
