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

pub mod broker;
mod wire;
pub use broker::LocalExecBroker;

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

/// The first word of a command pattern, after a path prefix (`/usr/bin/sudo`,
/// `C:\Windows\System32\sudo.exe`). Used only to decide whether a standing
/// allow is forbidden; matching at run time is still `matches` above.
fn first_command(pattern: &str) -> &str {
    let token = pattern.trim().split_whitespace().next().unwrap_or("");
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// Whether this standing rule may be persisted. Deny is never refused here —
/// remembering "never run sudo" is a safety net. Allow of `sudo` (and
/// `sudo.exe`, any path, any arguments) is refused, because a standing allow
/// on `sudo` would silently cover `sudo rm -rf /`.
///
/// Both writers go through this: `POST /local-exec/policy/rule` and the
/// Always/Never card in `conversation`. The store itself is not a writer of
/// policy, only of rows.
pub fn standing_rule_refusal(kind: &str, pattern: &str) -> Option<&'static str> {
    if kind != "allow" {
        return None;
    }
    let command = first_command(pattern);
    if command.eq_ignore_ascii_case("sudo") || command.eq_ignore_ascii_case("sudo.exe") {
        Some("sudo cannot be a standing allow")
    } else {
        None
    }
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
        .route("/local-exec/daemon", post(enrol_daemon).get(list_daemons))
        .route(
            "/local-exec/daemon/{machine_id}",
            axum::routing::delete(revoke_daemon),
        )
        .route("/local-exec/audit", get(audit_log))
        // User-direct enqueue (account-authed): the person runs a command on their OWN machine from
        // another device. Enqueuing IS their approval, so `Ask` is skipped — only `Never` and the
        // denylist still stop them.
        .route("/local-exec/run", post(run_direct))
        // The daemon's two endpoints (daemon-token authed): it holds `requests` open as an SSE
        // stream and POSTs results to `responses`. These are the ONLY path a command reaches the Mac.
        .route("/local-exec/requests", get(poll_requests))
        .route("/local-exec/responses", post(post_responses))
        .with_state(state)
}

/// A daemon token's claims — signed like everything else, `use: "daemon"` so a stolen access token
/// cannot pass here, `sub` the account and `machine` the enrolled machine. Long-lived on purpose:
/// its real lifecycle is the revocable `local_exec_daemon` row (checked by `jti`), not `exp`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DaemonClaims {
    #[serde(rename = "use")]
    purpose: String,
    sub: String,
    machine: String,
    jti: String,
    exp: i64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnrolBody {
    label: String,
    /// Re-enrol an existing machine (rotates its token) when present; otherwise a new machine id.
    machine_id: Option<String>,
}

/// `POST /local-exec/daemon` — enrol this account's machine and mint its daemon token (shown ONCE).
async fn enrol_daemon(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<EnrolBody>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let machine_id = body
        .machine_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| format!("mac_{}", uuid::Uuid::now_v7().simple()));
    let jti = uuid::Uuid::now_v7().to_string();
    let claims = DaemonClaims {
        purpose: "daemon".to_string(),
        sub: account_id.as_str().to_string(),
        machine: machine_id.clone(),
        jti: jti.clone(),
        // Ten years — revocation is the row, not the clock.
        exp: now_ms() / 1000 + 10 * 365 * 24 * 60 * 60,
    };
    let Ok(token) = state.minter.mint_claims(&claims) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not mint the daemon token",
        )
            .into_response();
    };
    if state
        .store
        .enrol_daemon(
            account_id.as_str(),
            &machine_id,
            body.label.trim(),
            &jti,
            now_ms(),
        )
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not enrol the machine",
        )
            .into_response();
    }
    Json(serde_json::json!({ "machineId": machine_id, "token": token })).into_response()
}

/// `DELETE /local-exec/daemon/{machine_id}` — revoke a machine's daemon token. Sign-in is untouched.
async fn revoke_daemon(
    State(state): State<AuthState>,
    headers: HeaderMap,
    axum::extract::Path(machine_id): axum::extract::Path<String>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    match state
        .store
        .revoke_daemon(account_id.as_str(), &machine_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not revoke").into_response(),
    }
}

/// `GET /local-exec/daemon` — the account's enrolled machines.
async fn list_daemons(State(state): State<AuthState>, headers: HeaderMap) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let machines = state
        .store
        .list_daemons(account_id.as_str())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(machine_id, label, enrolled_at_ms, revoked)| {
            serde_json::json!({
                "machineId": machine_id,
                "label": label,
                "enrolledAtMs": enrolled_at_ms,
                "revoked": revoked,
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "machines": machines })).into_response()
}

/// `GET /local-exec/audit` — the account's recent reverse-exec commands and outcomes.
async fn audit_log(State(state): State<AuthState>, headers: HeaderMap) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let entries = state
        .store
        .local_exec_audit_log(account_id.as_str(), 200)
        .await
        .unwrap_or_default();
    Json(serde_json::json!({ "entries": entries })).into_response()
}

/// Resolve the (account, machine) of a presented DAEMON token, or `None`. Verifies the signature and
/// `use: "daemon"`, then that the enrolment row still holds this token's `jti` and is not revoked —
/// so a revoked or superseded daemon token authorises nothing. This gates ONLY the poll endpoints
/// (a later slice); it is the daemon's identity, never an account's.
pub(crate) async fn daemon_from_bearer(
    state: &AuthState,
    headers: &HeaderMap,
) -> Option<(String, String)> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))?;
    let claims = state.minter.verify_claims::<DaemonClaims>(token).ok()?;
    if claims.purpose != "daemon" {
        return None;
    }
    let (jti, revoked) = state
        .store
        .daemon_jti(&claims.sub, &claims.machine)
        .await
        .ok()??;
    if revoked || jti != claims.jti {
        return None;
    }
    Some((claims.sub, claims.machine))
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
    if let Some(why) = standing_rule_refusal(&body.kind, pattern) {
        return (StatusCode::UNPROCESSABLE_ENTITY, why).into_response();
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

// ---------------------------------------------------------------------------------------------
// The enqueue path + the two daemon endpoints (slices 4–5). A command is judged by THE GATE here,
// on the server, before anything is dispatched; only an allowed command ever reaches the broker,
// and every command — allowed, denied, or awaiting a person — writes an audit row.
// ---------------------------------------------------------------------------------------------

use std::convert::Infallible;
use std::time::Duration;

use axum::http::header;
use broker::{DispatchError, ExecOutcome};

/// End-to-end budget for one reverse-exec command: dispatch, run on the Mac, result back. Past
/// this the caller is told it timed out and the daemon is asked to cancel.
const EXEC_TIMEOUT: Duration = Duration::from_secs(120);

/// Who enqueued a command — recorded on the audit row, and the reason `Ask` is or is not honored.
pub enum Origin {
    /// The account holder acting directly (their phone, the console). Enqueuing IS their approval,
    /// so `Ask` is skipped for them — only `Never` and a denylist match still refuse.
    User,
    /// A bot acting on the account's behalf (the coworker id). The full gate applies: `Ask` means a
    /// person must decide, so the run suspends rather than proceeding.
    Bot(String),
}

impl Origin {
    fn label(&self) -> String {
        match self {
            Origin::User => "user".to_string(),
            Origin::Bot(coworker) => format!("bot {coworker}"),
        }
    }
    fn is_user(&self) -> bool {
        matches!(self, Origin::User)
    }
}

/// What became of an enqueue attempt.
pub enum EnqueueResult {
    /// The command ran on the Mac; here is its outcome.
    Ran(ExecOutcome),
    /// The gate refused it (mode off, a deny rule, or no daemon connected) — never ran.
    Refused(String),
    /// A bot hit `Ask`: a person must approve. The caller (a bot run) suspends; nothing ran.
    NeedsApproval,
}

/// THE enqueue path. Judges `command` through the gate for this `machine`, and — only if allowed —
/// dispatches it to the machine's daemon and waits for the result. The one server-side choke point:
/// nothing reaches a Mac without passing `decide` and writing an audit row here.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_and_wait(
    state: &AuthState,
    account_id: &str,
    machine_id: &str,
    command: &str,
    simple_commands: &[String],
    origin: Origin,
    approval_id: &str,
    pre_approved: bool,
) -> EnqueueResult {
    let policy = load_policy(&state.store, account_id, machine_id).await;
    let decision = decide(&policy, command);
    let request_id = uuid::Uuid::now_v7().to_string();
    let origin_label = origin.label();

    // The gate's verdict, mapped to what actually happens for THIS origin: a user's own command
    // skips `Ask` (their enqueue is the approval); a bot's `Ask` suspends the run.
    let user_skipped_ask = origin.is_user() && matches!(decision, LocalExecDecision::Ask);
    let audit = |decision_word: &str| {
        let store = state.store.clone();
        let (id, acct, mach, org, cmd) = (
            request_id.clone(),
            account_id.to_string(),
            machine_id.to_string(),
            origin_label.clone(),
            command.to_string(),
        );
        let decision_word = decision_word.to_string();
        async move {
            let _ = store
                .audit_local_exec(&id, &acct, &mach, &org, &cmd, &decision_word, now_ms())
                .await;
        }
    };

    match decision {
        LocalExecDecision::Deny(reason) => {
            audit("deny").await;
            EnqueueResult::Refused(reason)
        }
        // A bot's Ask: suspend for the card — UNLESS the card already approved it (pre_approved on
        // resume), in which case dispatch with the approvalId the card recorded the machine-side
        // consent under.
        LocalExecDecision::Ask if !origin.is_user() && !pre_approved => {
            audit("ask").await;
            EnqueueResult::NeedsApproval
        }
        // Allow, Bypass, a user's own Ask-skipped command, or a bot's Ask that the card approved.
        _ => {
            let word = if user_skipped_ask {
                "allow-user"
            } else if pre_approved {
                "allow-approved"
            } else {
                "allow"
            };
            audit(word).await;
            run_on_machine(
                state,
                machine_id,
                &request_id,
                approval_id,
                command,
                simple_commands,
            )
            .await
        }
    }
}

/// Dispatch an approved command to a machine's daemon and wait for its result, finishing the audit
/// row with the outcome case. Assumes the gate already said yes and the row already exists.
async fn run_on_machine(
    state: &AuthState,
    machine_id: &str,
    request_id: &str,
    approval_id: &str,
    command: &str,
    simple_commands: &[String],
) -> EnqueueResult {
    let server_message = wire::shell_server_message(
        request_id,
        command,
        simple_commands,
        "",
        EXEC_TIMEOUT.as_millis() as u64,
    );
    let rx = match state
        .local_exec
        .dispatch(machine_id, request_id, approval_id, server_message)
        .await
    {
        Ok(rx) => rx,
        Err(DispatchError::NoDaemon) => {
            let out = ExecOutcome::malformed("the daemon for this machine is not connected");
            let _ = state
                .store
                .finish_local_exec_audit(request_id, &out.case, out.exit_code, now_ms())
                .await;
            return EnqueueResult::Refused(out.detail);
        }
    };
    let outcome = match tokio::time::timeout(EXEC_TIMEOUT, rx).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => ExecOutcome::malformed("the request was dropped before a result arrived"),
        Err(_) => {
            state.local_exec.cancel(machine_id, request_id).await;
            ExecOutcome::timed_out("no result within the command timeout")
        }
    };
    let _ = state
        .store
        .finish_local_exec_audit(request_id, &outcome.case, outcome.exit_code, now_ms())
        .await;
    EnqueueResult::Ran(outcome)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunBody {
    machine_id: String,
    command: String,
    /// The app's own parse of `command`, if the caller has one. Absent ⇒ the whole command as a
    /// single simple command — the gate still matches on the readable string either way.
    #[serde(default)]
    simple_commands: Vec<String>,
}

/// `POST /local-exec/run` — the user runs a command on their OWN machine from another device. The
/// account holder is the approver, so `Ask` is skipped; `Never` and the denylist still refuse.
async fn run_direct(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<RunBody>,
) -> Response {
    let (account_id, ..) = match crate::account_api::caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let command = body.command.trim();
    if command.is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "command is required").into_response();
    }
    let simple = if body.simple_commands.is_empty() {
        vec![command.to_string()]
    } else {
        body.simple_commands
    };
    let approval_id = uuid::Uuid::now_v7().to_string();
    match enqueue_and_wait(
        &state,
        account_id.as_str(),
        &body.machine_id,
        command,
        &simple,
        Origin::User,
        &approval_id,
        false,
    )
    .await
    {
        EnqueueResult::Ran(outcome) => Json(serde_json::json!({
            "outcome": outcome.case,
            "exitCode": outcome.exit_code,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "detail": outcome.detail,
            "text": outcome.render(),
        }))
        .into_response(),
        EnqueueResult::Refused(reason) => (StatusCode::FORBIDDEN, reason).into_response(),
        // A user's own command never hits `Ask`; treat an unexpected one as a server fault.
        EnqueueResult::NeedsApproval => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "a direct command unexpectedly needed approval",
        )
            .into_response(),
    }
}

/// `GET /local-exec/requests` — the daemon opens this ONCE and holds it. The server registers the
/// stream as this machine's provider and pushes newline-delimited JSON frames (`welcome`, `exec`,
/// `cancel`) down it. Daemon-token authed: the token names the machine, and only that machine's
/// commands ever come down this stream.
async fn poll_requests(State(state): State<AuthState>, headers: HeaderMap) -> Response {
    let Some((_account_id, machine_id)) = daemon_from_bearer(&state, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "enrol this machine first").into_response();
    };
    use futures::StreamExt as _;
    let rx = state.local_exec.connect(&machine_id).await;
    // The reconnect hint goes first, before any frame (mirrors the gateway `/events` stream and what
    // the daemon's SSE reader expects).
    let opening =
        futures::stream::once(async { Ok::<_, Infallible>("retry: 1000\n\n".to_string()) });
    let frames = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|frame| (Ok::<_, Infallible>(format!("data: {frame}\n\n")), rx))
    });
    // Keepalives so a proxy does not close an idle stream; they are SSE comments the reader skips.
    let pings = futures::stream::unfold((), |()| async {
        tokio::time::sleep(Duration::from_secs(15)).await;
        Some((Ok::<_, Infallible>(":ping\n\n".to_string()), ()))
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::body::Body::from_stream(opening.chain(futures::stream::select(frames, pings))),
    )
        .into_response()
}

/// `POST /local-exec/responses` — the daemon posts back on this. A `client` frame carries one
/// command's `ExecClientMessage` result, which resolves the waiting caller; `hello`/`ping` and any
/// other frame are acknowledged. Daemon-token authed, and a result is only accepted for a command
/// dispatched to THIS machine (the broker enforces that).
async fn post_responses(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some((_account_id, machine_id)) = daemon_from_bearer(&state, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "enrol this machine first").into_response();
    };
    // The daemon POSTs a BATCH (local-exec-provider.ts flushes an outbox): a top-level `providerId`
    // and a `frames` array, each frame discriminated by `kind` (hello/ping/client/control/…). One
    // POST can carry several results, and a backlog after a retry. A `client` frame carries one
    // command's `ExecClientMessage` in `message`; everything else is acknowledged. The result is
    // resolved against THIS machine (the broker rejects a mismatch), not the untrusted `providerId`.
    if let Some(frames) = body.get("frames").and_then(|value| value.as_array()) {
        for frame in frames {
            let kind = frame.get("kind").and_then(|value| value.as_str());
            let request_id = frame.get("requestId").and_then(|value| value.as_str());
            let message = frame.get("message");
            match (kind, request_id, message) {
                // A command's result. The STREAMING shell sends `shellStream` chunks then a
                // terminal event; a non-streaming `shellResult` is handled directly.
                (Some("client"), Some(request_id), Some(message)) => {
                    if message.get("shellResult").is_some() {
                        let outcome = wire::outcome_from_client_message(message);
                        state
                            .local_exec
                            .resolve(&machine_id, request_id, outcome)
                            .await;
                    } else if let Some(action) = wire::stream_action(message) {
                        use wire::StreamAction;
                        match action {
                            StreamAction::Stdout(chunk) => {
                                state
                                    .local_exec
                                    .accumulate(&machine_id, request_id, false, &chunk)
                                    .await;
                            }
                            StreamAction::Stderr(chunk) => {
                                state
                                    .local_exec
                                    .accumulate(&machine_id, request_id, true, &chunk)
                                    .await;
                            }
                            StreamAction::Exit(code) => {
                                let case = if code == 0 { "success" } else { "failure" };
                                state
                                    .local_exec
                                    .finish_stream(&machine_id, request_id, case, Some(code), "")
                                    .await;
                            }
                            StreamAction::Terminal { case, detail } => {
                                state
                                    .local_exec
                                    .finish_stream(&machine_id, request_id, &case, None, &detail)
                                    .await;
                            }
                            StreamAction::Ignore => {}
                        }
                    }
                }
                // A control frame: a `throw` means the machine refused or errored the command
                // (e.g. its local tools are on "ask" and there was no matching local approval).
                // resolve the waiter with that reason instead of letting it hang to the timeout.
                (Some("control"), Some(request_id), Some(message)) => {
                    if let Some(thrown) = message.get("throw") {
                        let reason = thrown
                            .get("error")
                            .and_then(|value| value.as_str())
                            .unwrap_or("the machine refused or errored the command");
                        state
                            .local_exec
                            .resolve(&machine_id, request_id, ExecOutcome::malformed(reason))
                            .await;
                    }
                    // streamClose / heartbeat carry no result — nothing to resolve.
                }
                // hello / ping / file / messages-* — acknowledged, nothing to resolve.
                _ => {}
            }
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------------------------
// The bot-facing side (slice 6): the `user_machine_shell` tool reaches this. The tool lives in
// `opengrok-tools`, which cannot depend on the server, so the server implements the sink trait here
// over the enqueue path and attaches it per request — only when the account has an enabled machine.
// ---------------------------------------------------------------------------------------------

/// The account's first enrolled, enabled machine, if any — the target `user_machine_shell` binds to.
/// `Never` (or revoked) machines are skipped, so the tool is offered ONLY when there is a live,
/// consenting machine to reach. (v1 targets the first such machine; per-machine choice is later.)
pub async fn enabled_machine(
    store: &opengrok_store::PgStore,
    account_id: &str,
) -> Option<(String, String)> {
    let machines = store.list_daemons(account_id).await.ok()?;
    for (machine_id, label, _enrolled_at_ms, revoked) in machines {
        if revoked {
            continue;
        }
        let mode = store
            .local_exec_mode(account_id, &machine_id)
            .await
            .ok()
            .flatten()
            .map(|mode| LocalExecMode::from_stored(&mode))
            .unwrap_or_default();
        if mode != LocalExecMode::Never {
            // The label rides along so prompts can name the ACTUAL enrolled computer
            // ("Uriah's-MacBook-Pro.local") instead of guessing at an OS or hardware name.
            return Some((machine_id, label));
        }
    }
    None
}

/// The server's implementation of the reverse-exec tool seam. A bot's `user_machine_shell` call
/// lands in `run`, which forwards the command through the SAME gated enqueue path as everything
/// else — as a `Bot`, so `Ask` suspends the run for the user to approve.
pub struct ReverseExecSink {
    pub auth: AuthState,
    /// The coworker asking — recorded as the audit origin.
    pub coworker_id: String,
    /// The machine this tool is bound to for this request.
    pub machine_id: String,
}

#[async_trait::async_trait]
impl opengrok_tools::UserMachineSink for ReverseExecSink {
    /// The gate's verdict with nothing queued — the executor consults auto-review between this
    /// and `run`. A DENY is audited here, because a denied command never reaches `run`; an ask or
    /// an allow is audited by `run` (`enqueue_and_wait`), which the executor still calls for both,
    /// so every command that touched the channel has exactly one row.
    async fn decide(
        &self,
        account_id: &opengrok_core::id::AccountId,
        command: &str,
    ) -> opengrok_tools::UserMachineVerdict {
        let policy = load_policy(&self.auth.store, account_id.as_str(), &self.machine_id).await;
        match decide(&policy, command) {
            LocalExecDecision::Allow => opengrok_tools::UserMachineVerdict::Allow,
            LocalExecDecision::Ask => opengrok_tools::UserMachineVerdict::Ask,
            LocalExecDecision::Deny(why) => {
                let _ = self
                    .auth
                    .store
                    .audit_local_exec(
                        &uuid::Uuid::now_v7().to_string(),
                        account_id.as_str(),
                        &self.machine_id,
                        &Origin::Bot(self.coworker_id.clone()).label(),
                        command,
                        "deny",
                        now_ms(),
                    )
                    .await;
                opengrok_tools::UserMachineVerdict::Deny(why)
            }
        }
    }

    async fn run(
        &self,
        account_id: &opengrok_core::id::AccountId,
        command: &str,
        call_id: &str,
        approved: bool,
    ) -> opengrok_tools::UserMachineReply {
        // The approvalId IS the tool call id — stable across suspend/resume, and the id the inline
        // card records the machine-side approval under. `approved` is true on resume (the card said
        // yes), so the Ask gate dispatches instead of suspending again.
        match enqueue_and_wait(
            &self.auth,
            account_id.as_str(),
            &self.machine_id,
            command,
            &[command.to_string()],
            Origin::Bot(self.coworker_id.clone()),
            call_id,
            approved,
        )
        .await
        {
            EnqueueResult::Ran(outcome) => opengrok_tools::UserMachineReply::Ran(outcome.render()),
            EnqueueResult::Refused(why) => opengrok_tools::UserMachineReply::Refused(why),
            EnqueueResult::NeedsApproval => opengrok_tools::UserMachineReply::NeedsApproval,
        }
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
    fn sudo_cannot_stand_as_allow() {
        let refused = Some("sudo cannot be a standing allow");
        assert_eq!(standing_rule_refusal("allow", "sudo"), refused);
        assert_eq!(standing_rule_refusal("allow", "sudo rm -rf /"), refused);
        assert_eq!(standing_rule_refusal("allow", "/usr/bin/sudo"), refused);
        assert_eq!(
            standing_rule_refusal("allow", r"C:\Windows\System32\sudo.exe"),
            refused
        );
        assert_eq!(standing_rule_refusal("allow", "  SUDO -u root id"), refused);
        // Deny of sudo is the safety net; it is not refused.
        assert_eq!(standing_rule_refusal("deny", "sudo"), None);
        assert_eq!(standing_rule_refusal("deny", "sudo rm"), None);
        // A different command that merely starts with the letters, or sudo later.
        assert_eq!(standing_rule_refusal("allow", "sudoedit"), None);
        assert_eq!(standing_rule_refusal("allow", "echo sudo"), None);
        assert_eq!(standing_rule_refusal("allow", "uname"), None);
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
