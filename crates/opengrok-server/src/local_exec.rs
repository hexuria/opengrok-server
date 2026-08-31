//! The reverse-exec permission gate — the safety core of the channel that runs commands on the
//! USER'S OWN machine (their Mac), not a disposable box.
//!
//! Built GATE-FIRST and on its own: this is pure decision logic with no transport, no daemon and no
//! way to run anything, so the rules can be proven closed-by-default before a single command can
//! flow. A Claude-Code-style model (Uriah's call): a per-machine `mode`, plus an allowlist and a
//! denylist of command patterns added on demand.
//!
//! CLOSED BY DEFAULT. The default mode is `Never` (the channel is off), an unknown command in `Ask`
//! mode is `Ask` (a person decides, never a silent yes), and deny always beats allow. The only path
//! to an automatic yes is an explicit allowlist rule under `Ask`, or the deliberately-enabled
//! `Bypass`. See `docs/reverse-exec-design.md`.

use serde::{Deserialize, Serialize};

/// The consent mode for ONE machine's reverse-exec channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LocalExecMode {
    /// The channel is OFF. Every command is denied. This is the default until the user turns it on.
    #[default]
    Never,
    /// Consult the lists: deny-match denies, allow-match allows, anything else asks a person.
    Ask,
    /// Allow everything, skipping the lists — a deliberate, machine-wide choice, like Claude Code's
    /// bypass. Still audited (every command is logged, even here).
    Bypass,
}

/// A machine's reverse-exec permission policy. Absent ⇒ the default (`Never`, no rules).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalExecPolicy {
    pub mode: LocalExecMode,
    /// Command patterns that auto-ALLOW under `Ask` (added on demand: "always allow").
    #[serde(default)]
    pub allow: Vec<String>,
    /// Command patterns that auto-DENY under `Ask` (added on demand: "always deny").
    #[serde(default)]
    pub deny: Vec<String>,
}

/// The gate's verdict for one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalExecDecision {
    /// Run it automatically (an allowlist rule, or `Bypass`).
    Allow,
    /// Refuse it, with a human-readable reason. Never runs.
    Deny(String),
    /// Suspend — a person decides for THIS command. Never treated as a yes.
    Ask,
}

impl LocalExecDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Does `pattern` match `command`? Prefix match on a WORD BOUNDARY — `pattern` matches `command`
/// when they are equal or `command` begins with `pattern` followed by a space. So `git status`
/// matches `git status --short` but not `git statusx`, and a broad `git` matches `git anything`.
/// Both sides are whitespace-trimmed first. Deliberately simple and conservative: a rule can only
/// widen to whole extra arguments, never to a different command that merely shares a prefix.
fn matches(pattern: &str, command: &str) -> bool {
    let pattern = pattern.trim();
    let command = command.trim();
    if pattern.is_empty() {
        return false;
    }
    command == pattern || command.starts_with(&format!("{pattern} "))
}

/// THE GATE. The one place a command on the user's own machine is judged. Everything that would run
/// a reverse-exec command MUST pass through here first, on the server, before anything is queued.
///
/// - `Never` (default): deny, always.
/// - `Bypass`: allow (the lists are skipped by the user's deliberate choice; still audited).
/// - `Ask`: a denylist match denies (deny wins), else an allowlist match allows, else ask.
pub fn decide(policy: &LocalExecPolicy, command: &str) -> LocalExecDecision {
    match policy.mode {
        LocalExecMode::Never => LocalExecDecision::Deny(
            "this machine's reverse-exec channel is off (mode: never) — turn it on to run commands here".to_string(),
        ),
        LocalExecMode::Bypass => LocalExecDecision::Allow,
        LocalExecMode::Ask => {
            if policy.deny.iter().any(|pattern| matches(pattern, command)) {
                LocalExecDecision::Deny("a deny rule matched this command".to_string())
            } else if policy.allow.iter().any(|pattern| matches(pattern, command)) {
                LocalExecDecision::Allow
            } else {
                LocalExecDecision::Ask
            }
        }
    }
}

impl LocalExecMode {
    /// From the stored word; anything unrecognised (or absent) is the closed default, `Never`.
    pub fn from_stored(mode: &str) -> Self {
        match mode {
            "ask" => Self::Ask,
            "bypass" => Self::Bypass,
            _ => Self::Never,
        }
    }

    pub fn as_stored(&self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Ask => "ask",
            Self::Bypass => "bypass",
        }
    }
}

/// Assemble a machine's policy from the store — its mode (the closed default `Never` when unset)
/// plus its allow and deny lists. The single place the persisted pieces become a `LocalExecPolicy`
/// the gate can judge; a store error reads as "no policy", which is `Never`, i.e. closed.
pub async fn load_policy(
    store: &opengrok_store::PgStore,
    account_id: &str,
    machine_id: &str,
) -> LocalExecPolicy {
    let mode = store
        .local_exec_mode(account_id, machine_id)
        .await
        .ok()
        .flatten()
        .map(|mode| LocalExecMode::from_stored(&mode))
        .unwrap_or_default();
    let allow = store
        .local_exec_rules(account_id, machine_id, "allow")
        .await
        .unwrap_or_default();
    let deny = store
        .local_exec_rules(account_id, machine_id, "deny")
        .await
        .unwrap_or_default();
    LocalExecPolicy { mode, allow, deny }
}

// ---------------------------------------------------------------------------------------------
// The account-facing management API: a person sets their own machines' mode and allow/deny rules.
// (The daemon poll endpoints and the enqueue path are separate, later slices.) Account-authed via
// the same Bearer-or-cookie check the rest of the account API uses.
// ---------------------------------------------------------------------------------------------

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::AuthState;

const VALID_MODES: &[&str] = &["never", "ask", "bypass"];
const VALID_KINDS: &[&str] = &["allow", "deny"];

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route("/local-exec/policy", get(get_policy).put(set_mode))
        .route(
            "/local-exec/policy/rule",
            post(add_rule).delete(remove_rule),
        )
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct MachineQuery {
    machine: String,
}

/// `GET /local-exec/policy?machine=<id>` — this machine's mode and rule lists, for the caller.
async fn get_policy(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Query(query): Query<MachineQuery>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let policy = load_policy(&state.store, account_id.as_str(), &query.machine).await;
    Json(serde_json::json!({
        "machineId": query.machine,
        "mode": policy.mode.as_stored(),
        "allow": policy.allow,
        "deny": policy.deny,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMode {
    machine_id: String,
    mode: String,
}

/// `PUT /local-exec/policy` — set a machine's consent mode (never | ask | bypass).
async fn set_mode(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<SetMode>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    if !VALID_MODES.contains(&body.mode.as_str()) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown mode").into_response();
    }
    match state
        .store
        .set_local_exec_mode(account_id.as_str(), &body.machine_id, &body.mode, now_ms())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not set the mode").into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleBody {
    machine_id: String,
    kind: String,
    pattern: String,
}

/// `POST /local-exec/policy/rule` — add an allow or deny rule ("always allow/deny this").
async fn add_rule(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<RuleBody>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let pattern = body.pattern.trim();
    if !VALID_KINDS.contains(&body.kind.as_str()) || pattern.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "kind must be allow|deny and pattern non-empty",
        )
            .into_response();
    }
    match state
        .store
        .add_local_exec_rule(
            account_id.as_str(),
            &body.machine_id,
            &body.kind,
            pattern,
            now_ms(),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not add the rule").into_response(),
    }
}

/// `DELETE /local-exec/policy/rule` — remove an allow or deny rule.
async fn remove_rule(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<RuleBody>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    match state
        .store
        .remove_local_exec_rule(
            account_id.as_str(),
            &body.machine_id,
            &body.kind,
            &body.pattern,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not remove the rule",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: LocalExecMode, allow: &[&str], deny: &[&str]) -> LocalExecPolicy {
        LocalExecPolicy {
            mode,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn default_is_closed() {
        // The default policy (no config at all) denies everything — the channel is off.
        assert!(matches!(
            decide(&LocalExecPolicy::default(), "echo hi"),
            LocalExecDecision::Deny(_)
        ));
    }

    #[test]
    fn never_denies_even_an_allowlisted_command() {
        // Mode is the baseline: Never denies regardless of the lists.
        let p = policy(LocalExecMode::Never, &["echo"], &[]);
        assert!(matches!(decide(&p, "echo hi"), LocalExecDecision::Deny(_)));
    }

    #[test]
    fn ask_with_no_rules_asks() {
        let p = policy(LocalExecMode::Ask, &[], &[]);
        assert_eq!(decide(&p, "echo hi"), LocalExecDecision::Ask);
    }

    #[test]
    fn ask_allowlist_allows_on_word_boundary_only() {
        let p = policy(LocalExecMode::Ask, &["git status"], &[]);
        assert_eq!(decide(&p, "git status"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "git status --short"), LocalExecDecision::Allow);
        // A shared prefix that is NOT a word boundary must not match — closed by default.
        assert_eq!(decide(&p, "git statusx"), LocalExecDecision::Ask);
        assert_eq!(decide(&p, "git log"), LocalExecDecision::Ask);
    }

    #[test]
    fn ask_denylist_denies() {
        let p = policy(LocalExecMode::Ask, &[], &["rm"]);
        assert!(matches!(decide(&p, "rm -rf /"), LocalExecDecision::Deny(_)));
    }

    #[test]
    fn deny_wins_over_allow() {
        // The same command both allowed and denied: deny wins.
        let p = policy(LocalExecMode::Ask, &["sudo rm"], &["sudo"]);
        assert!(matches!(
            decide(&p, "sudo rm -rf /"),
            LocalExecDecision::Deny(_)
        ));
    }

    #[test]
    fn bypass_allows_everything() {
        // Bypass is the user's deliberate "allow all" — even a command that would be denylisted.
        let p = policy(LocalExecMode::Bypass, &[], &["rm"]);
        assert_eq!(decide(&p, "rm -rf /"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "anything at all"), LocalExecDecision::Allow);
    }

    #[test]
    fn an_empty_pattern_never_matches() {
        // A blank rule must not become a wildcard that allows or denies everything.
        assert!(!matches("", "echo hi"));
        assert!(!matches("   ", "echo hi"));
        let p = policy(LocalExecMode::Ask, &[""], &[]);
        assert_eq!(decide(&p, "echo hi"), LocalExecDecision::Ask);
    }
}
