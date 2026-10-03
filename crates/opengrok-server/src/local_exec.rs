//! The reverse-exec channel that runs commands on the USER'S OWN machine (their Mac), not a
//! disposable box: the routes a person enrols their machines and sets their consent with, the
//! one enqueue path, and the daemon's two endpoints.
//!
//! THE GATE every command passes first is `opengrok_policy::local_exec`, re-exported here: pure
//! decision logic with no transport, closed by default, beside the shell reader it judges with.

pub mod broker;
mod wire;
pub use broker::LocalExecBroker;
pub use opengrok_policy::local_exec::{
    DENIED_BY_A_RULE, LocalExecDecision, LocalExecMode, LocalExecPolicy, MAX_JUDGED_BYTES, decide,
    simple_commands, standing_rule_refusal,
};

/// Assemble a machine's policy from the store — its mode (the closed default `Never` when unset)
/// plus its allow and deny lists. The single place the persisted pieces become a `LocalExecPolicy`
/// the gate can judge; a store error reads as "no policy", which is `Never`, i.e. closed.
pub async fn load_policy(
    store: &opengrok_store::PgStore,
    account_id: &str,
    machine_id: &str,
) -> LocalExecPolicy {
    let mode = store.local_exec_mode(account_id, machine_id).await;
    let mode = mode.ok().flatten();
    let allow = store
        .local_exec_rules(account_id, machine_id, "allow")
        .await;
    let deny = store.local_exec_rules(account_id, machine_id, "deny").await;
    LocalExecPolicy {
        mode: mode
            .as_deref()
            .map_or_else(Default::default, LocalExecMode::from_stored),
        allow: allow.unwrap_or_default(),
        deny: deny.unwrap_or_default(),
        session_allow: Vec::new(),
    }
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
use opengrok_store::Daemon;

use crate::AuthState;
use crate::now_ms;

const VALID_MODES: &[&str] = &["never", "ask", "bypass"];
const VALID_KINDS: &[&str] = &["allow", "deny"];

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
            axum::routing::delete(revoke_daemon).patch(switch_relay),
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

/// A write this API could not make: a 500 that says which.
fn failed(what: &'static str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, what).into_response()
}

/// A write that landed, or the 500 saying which one did not.
fn written<E>(result: Result<(), E>, failed_to: &'static str) -> Response {
    let done = || StatusCode::NO_CONTENT.into_response();
    result.map_or_else(|_| failed(failed_to), |()| done())
}

/// `POST /local-exec/daemon` — enrol this account's machine and mint its daemon token (shown ONCE).
async fn enrol_daemon(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<EnrolBody>,
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
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
    let minted = state.minter.mint_claims(&claims);
    let token = minted.map_err(|_| failed("could not mint the daemon token"))?;
    let (account, label) = (account_id.as_str(), body.label.trim());
    let enrolled = state
        .store
        .enrol_daemon(account, &machine_id, label, &jti, now_ms());
    enrolled
        .await
        .map_err(|_| failed("could not enrol the machine"))?;
    // A RE-ENROLMENT RETIRES THE OLD TOKEN'S STREAMS TOO, as revoke does: its relay stream (review
    // of #298) and its command stream (#299), which was still sent commands. After the row, so a
    // stream opening with the old token meanwhile fails its second check.
    state.relay.disconnect(account, &machine_id);
    state.local_exec.disconnect(account, &machine_id).await;
    Ok(Json(serde_json::json!({ "machineId": machine_id, "token": token })).into_response())
}

/// `DELETE /local-exec/daemon/{machine_id}` — revoke a machine's daemon token. Sign-in is untouched.
async fn revoke_daemon(
    State(state): State<AuthState>,
    headers: HeaderMap,
    axum::extract::Path(machine_id): axum::extract::Path<String>,
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let account = account_id.as_str();
    let revoked = state.store.revoke_daemon(account, &machine_id);
    revoked.await.map_err(|_| failed("could not revoke"))?;
    // Its streams too (#292, #299): one opened before the revoke would still be sent the person's
    // turns and commands, though no answer from it would be taken.
    state.relay.disconnect(account, &machine_id);
    state.local_exec.disconnect(account, &machine_id).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /local-exec/daemon` — the account's enrolled machines.
async fn list_daemons(
    State(state): State<AuthState>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let listed = state.store.list_daemons(account_id.as_str()).await;
    let mut machines = Vec::new();
    for machine in listed.unwrap_or_default() {
        machines.push(row(&state, account_id.as_str(), machine).await);
    }
    Ok(Json(serde_json::json!({ "machines": machines })).into_response())
}

/// A machine as `GET /local-exec/daemon` lists it and its `PATCH` answers. `relayEnabled` is its
/// own switch; `connected` is its command stream and `relaying` its relay stream, each as this
/// replica holds it now.
async fn row(state: &AuthState, account: &str, machine: Daemon) -> serde_json::Value {
    let connected = !machine.revoked && state.local_exec.has_provider(&machine.machine_id).await;
    let relaying = state.relay.relaying(account, &machine.machine_id);
    serde_json::json!({ "machineId": machine.machine_id, "label": machine.label,
        "enrolledAtMs": machine.enrolled_at_ms, "revoked": machine.revoked, "connected": connected,
        "relayEnabled": machine.relay_enabled, "relaying": relaying })
}

/// `PATCH /local-exec/daemon/{machine_id}` — `{relayEnabled}`: one computer's relay switched,
/// answered with its row. ANOTHER ACCOUNT'S MACHINE IS UNKNOWN HERE, as one never enrolled is:
/// the same 404, so an id says nothing of who holds it. A revoked one is refused, unswitched.
/// Off, its open stream is told `disabled` and closed, after its row says so.
async fn switch_relay(
    State(state): State<AuthState>,
    headers: HeaderMap,
    axum::extract::Path(machine_id): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let said = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let on = said.and_then(|body| body.get("relayEnabled")?.as_bool());
    let why = "relayEnabled must be true or false";
    let on = on.ok_or_else(|| coded(StatusCode::BAD_REQUEST, why, "bad_request"))?;
    let account = account_id.as_str();
    let switched = state.store.set_relay(account, &machine_id, on).await;
    let machine = switched.map_err(|_| failed("could not switch the relay"))?;
    let unknown = "no computer of yours has that id";
    let machine = machine.ok_or_else(|| coded(StatusCode::NOT_FOUND, unknown, "not_found"))?;
    if machine.revoked {
        let why = "this computer was revoked; enrol it again to use it";
        return Err(coded(StatusCode::CONFLICT, why, "revoked"));
    }
    if !on {
        state.relay.disable(account, &machine_id);
    }
    Ok(Json(row(&state, account, machine).await).into_response())
}

/// A refusal a client reads by its `code`, beside the sentence it shows.
fn coded(status: StatusCode, error: &str, code: &str) -> Response {
    let said = serde_json::json!({ "error": error, "code": code });
    (status, Json(said)).into_response()
}

/// `GET /local-exec/audit` — the account's recent reverse-exec commands and outcomes.
async fn audit_log(
    State(state): State<AuthState>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let entries = state.store.local_exec_audit_log(account_id.as_str(), 200);
    let entries = entries.await.unwrap_or_default();
    Ok(Json(serde_json::json!({ "entries": entries })).into_response())
}

/// Resolve the (account, machine) of a presented DAEMON token, and whether its relay is switched
/// on, or `None`. Verifies the signature and `use: "daemon"`, then that the enrolment row still
/// holds this token's `jti` and is not revoked — so a revoked or superseded daemon token
/// authorises nothing. This gates ONLY the poll endpoints (a later slice); it is the daemon's
/// identity, never an account's.
pub(crate) async fn daemon_from_bearer(
    state: &AuthState,
    headers: &HeaderMap,
) -> Option<(String, String, bool)> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))?;
    let claims = state.minter.verify_claims::<DaemonClaims>(token).ok()?;
    if claims.purpose != "daemon" {
        return None;
    }
    let (jti, revoked, relay_on) = state
        .store
        .daemon_jti(&claims.sub, &claims.machine)
        .await
        .ok()??;
    if revoked || jti != claims.jti {
        return None;
    }
    Some((claims.sub, claims.machine, relay_on))
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
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let policy = load_policy(&state.store, account_id.as_str(), &query.machine).await;
    Ok(Json(policy_listing(&query.machine, &policy)).into_response())
}

/// The body of `GET /local-exec/policy`. An allow row the gate can never match (stored before
/// #203's refusal, or written straight to the store) is still listed in `allow`, and named again
/// in `inert` with the reason the rule endpoint would give. `inert` is a sibling rather than a
/// flag on each row because `allow` is an array of strings that NativeChat reads (#223).
pub fn policy_listing(machine: &str, policy: &LocalExecPolicy) -> serde_json::Value {
    let inert: Vec<serde_json::Value> = policy
        .allow
        .iter()
        .filter_map(|pattern| {
            standing_rule_refusal("allow", pattern)
                .map(|reason| serde_json::json!({"pattern": pattern, "reason": reason}))
        })
        .collect();
    serde_json::json!({
        "machineId": machine,
        "mode": policy.mode.as_stored(),
        "allow": policy.allow,
        "deny": policy.deny,
        "inert": inert,
    })
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
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    if !VALID_MODES.contains(&body.mode.as_str()) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "unknown mode").into_response());
    }
    let (account, machine) = (account_id.as_str(), &body.machine_id);
    let set = state
        .store
        .set_local_exec_mode(account, machine, &body.mode, now_ms());
    Ok(written(set.await, "could not set the mode"))
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
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let pattern = body.pattern.trim();
    let refused = |why| (StatusCode::UNPROCESSABLE_ENTITY, why).into_response();
    if !VALID_KINDS.contains(&body.kind.as_str()) || pattern.is_empty() {
        return Err(refused("kind must be allow|deny and pattern non-empty"));
    }
    if let Some(why) = standing_rule_refusal(&body.kind, pattern) {
        return Err(refused(why));
    }
    let (account, machine) = (account_id.as_str(), &body.machine_id);
    let added = state
        .store
        .add_local_exec_rule(account, machine, &body.kind, pattern, now_ms());
    Ok(written(added.await, "could not add the rule"))
}

/// `DELETE /local-exec/policy/rule` — remove an allow or deny rule.
async fn remove_rule(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<RuleBody>,
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let (account, machine, kind) = (account_id.as_str(), &body.machine_id, &body.kind);
    let removed = state
        .store
        .remove_local_exec_rule(account, machine, kind, &body.pattern);
    Ok(written(removed.await, "could not remove the rule"))
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
    let audit = |decision_word: &str, rule: Option<String>| {
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
                .audit_local_exec(
                    &id,
                    &acct,
                    &mach,
                    &org,
                    &cmd,
                    &decision_word,
                    rule.as_deref(),
                    now_ms(),
                )
                .await;
        }
    };

    match decision {
        LocalExecDecision::Deny { why, rule } => {
            audit("deny", rule).await;
            EnqueueResult::Refused(why)
        }
        // A bot's Ask: suspend for the card — UNLESS the card already approved it (pre_approved on
        // resume), in which case dispatch with the approvalId the card recorded the machine-side
        // consent under.
        LocalExecDecision::Ask if !origin.is_user() && !pre_approved => {
            audit("ask", None).await;
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
            audit(word, None).await;
            run_on_machine(state, machine_id, &request_id, approval_id, command).await
        }
    }
}

/// What the daemon evals before the model's command.
///
/// The daemon is a GUI process. Its shell is `/bin/sh`, whose PATH is
/// `/usr/bin:/bin:/usr/sbin:/sbin`, so `gpui-agent` in `~/.cargo/bin` is
/// `command not found`. A function named `gpui-agent` is illegal there
/// (`not a valid identifier`). A temp script of that name is worse: the shell
/// deletes it on exit, and the next command repeats the dead path. eBIRForms
/// refuses a client without `GPUI_AGENT_TOKEN`. That value is in the host
/// process, not in this daemon. Copy it into the environment from whichever
/// of 17423 or 17421 is listening. The assignment uses the lookup, never a
/// literal. `tr` uses `\012` because `$(printf '\n')` is empty after command
/// substitution strips the newline. The gate and the audit keep `command` as
/// the model wrote it, so an allow rule of `gpui-agent` still matches.
const USER_MACHINE_SHELL_PREAMBLE: &str = r#"export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"; if [ -z "${GPUI_AGENT_TOKEN:-}" ]; then _gpui_pid=""; for _gpui_port in 17423 17421; do _gpui_pid=$(lsof -nP -iTCP:"$_gpui_port" -sTCP:LISTEN -t 2>/dev/null | head -1); if [ -n "$_gpui_pid" ]; then break; fi; done; if [ -n "$_gpui_pid" ]; then _gpui_token=$(ps -wwE -p "$_gpui_pid" -o command= 2>/dev/null | tr ' ' '\012' | sed -n 's/^GPUI_AGENT_TOKEN=//p' | head -1); if [ -n "$_gpui_token" ]; then export GPUI_AGENT_TOKEN="$_gpui_token"; fi; fi; unset _gpui_pid _gpui_port _gpui_token; fi; "#;

fn command_with_user_bin_path(command: &str) -> String {
    format!("{USER_MACHINE_SHELL_PREAMBLE}{command}")
}

/// The shell frame `run_on_machine` sends. The PATH prefix lives here so the
/// gate's command and this frame cannot drift apart in two call sites.
fn user_machine_shell_message(
    exec_id: &str,
    command: &str,
    simple_commands: &[String],
    timeout_ms: u64,
) -> serde_json::Value {
    wire::shell_server_message(
        exec_id,
        &command_with_user_bin_path(command),
        simple_commands,
        "",
        timeout_ms,
    )
}

/// Dispatch an approved command to a machine's daemon and wait for its result, finishing the audit
/// row with the outcome case. Assumes the gate already said yes and the row already exists.
async fn run_on_machine(
    state: &AuthState,
    machine_id: &str,
    request_id: &str,
    approval_id: &str,
    command: &str,
) -> EnqueueResult {
    // The daemon's `simpleCommands` is the server's own split, never a caller's: the list the
    // machine's local approval sees must be the one the gate read (#203).
    let server_message = user_machine_shell_message(
        request_id,
        command,
        &simple_commands(command),
        EXEC_TIMEOUT.as_millis() as u64,
    );
    let rx = match state
        .local_exec
        .dispatch(machine_id, request_id, approval_id, server_message)
        .await
    {
        Ok(rx) => rx,
        Err(DispatchError::NoDaemon) => {
            let out = ExecOutcome::offline("the daemon for this machine is not connected");
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
    /// A caller's `simpleCommands` is not read: the daemon is sent the server's own split.
    command: String,
}

/// `POST /local-exec/run` — the user runs a command on their OWN machine from another device. The
/// account holder is the approver, so `Ask` is skipped; `Never` and the denylist still refuse.
async fn run_direct(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<RunBody>,
) -> Result<Response, Response> {
    let (account_id, ..) = crate::account_api::caller(&state, &headers).await?;
    let command = body.command.trim();
    if command.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "command is required").into_response());
    }
    let approval_id = uuid::Uuid::now_v7().to_string();
    let (account, machine) = (account_id.as_str(), &body.machine_id);
    let ran = enqueue_and_wait(
        &state,
        account,
        machine,
        command,
        Origin::User,
        &approval_id,
        false,
    );
    Ok(match ran.await {
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
        EnqueueResult::NeedsApproval => failed("a direct command unexpectedly needed approval"),
    })
}

/// `GET /local-exec/requests` — the daemon opens this ONCE and holds it. The server registers the
/// stream as this machine's provider and pushes newline-delimited JSON frames (`welcome`, `exec`,
/// `cancel`) down it. Daemon-token authed: the token names the machine, and only that machine's
/// commands ever come down this stream, which ends when the token is retired (#299).
async fn poll_requests(State(state): State<AuthState>, headers: HeaderMap) -> Response {
    let enrol_first = || (StatusCode::UNAUTHORIZED, "enrol this machine first").into_response();
    let Some((account_id, machine_id, _)) = daemon_from_bearer(&state, &headers).await else {
        return enrol_first();
    };
    use futures::StreamExt as _;
    let rx = state.local_exec.connect(&account_id, &machine_id).await;
    // A TOKEN RETIRED AS ITS STREAM OPENED KEEPS NO STREAM, as on the relay: revoke and
    // re-enrolment end the machine's stream after its row changes, and this connect may have come
    // after that. Dropped here, the stream is closed to the broker before its daemon is sent a
    // frame.
    if daemon_from_bearer(&state, &headers).await.is_none() {
        return enrol_first();
    }
    // The reconnect hint first (the gateway `/events` stream's, which the daemon's SSE reader
    // expects), then the frames, and every 15 s a ping, an SSE comment the reader skips, so a proxy
    // does not close an idle stream. Pings outlive the frames of a stream a reconnect replaced; the
    // broker's `null` (`disconnect`) ends the stream.
    let every = Duration::from_secs(15);
    let ping = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    let frames = futures::stream::unfold((rx, ping), |(mut rx, mut ping)| async move {
        let chunk = tokio::select! {
            Some(frame) = rx.recv() => (!frame.is_null()).then(|| format!("data: {frame}\n\n"))?,
            _ = ping.tick() => ":ping\n\n".to_string(),
        };
        Some((Ok::<_, Infallible>(chunk), (rx, ping)))
    });
    let opening = futures::stream::once(async { Ok("retry: 1000\n\n".to_string()) });
    let sse = [
        (header::CONTENT_TYPE, "text/event-stream"),
        (header::CACHE_CONTROL, "no-cache"),
    ];
    (sse, axum::body::Body::from_stream(opening.chain(frames))).into_response()
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
    let Some((_account_id, machine_id, _)) = daemon_from_bearer(&state, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "enrol this machine first").into_response();
    };
    // The daemon POSTs a BATCH (local-exec-provider.ts flushes an outbox): a top-level `providerId`
    // and a `frames` array, each frame discriminated by `kind` (hello/ping/client/control/…). One
    // POST can carry several results, and a backlog after a retry. A `client` frame carries one
    // command's `ExecClientMessage` in `message`; everything else is acknowledged. The result is
    // resolved against THIS machine (the broker rejects a mismatch), not the untrusted `providerId`.
    let (broker, machine) = (&state.local_exec, machine_id.as_str());
    if let Some(frames) = body.get("frames").and_then(|value| value.as_array()) {
        for frame in frames {
            let kind = frame.get("kind").and_then(|value| value.as_str());
            let request_id = frame.get("requestId").and_then(|value| value.as_str());
            let message = frame.get("message");
            match (kind, request_id, message) {
                // A command's result. The STREAMING shell sends `shellStream` chunks then a
                // terminal event; a non-streaming `shellResult` is handled directly.
                (Some("client"), Some(id), Some(message)) => {
                    if message.get("shellResult").is_some() {
                        let outcome = wire::outcome_from_client_message(message);
                        broker.resolve(machine, id, outcome).await;
                    } else if let Some(action) = wire::stream_action(message) {
                        use wire::StreamAction::{Exit, Ignore, Stderr, Stdout, Terminal};
                        match action {
                            Stdout(out) => broker.accumulate(machine, id, false, &out).await,
                            Stderr(err) => broker.accumulate(machine, id, true, &err).await,
                            Exit(code) => {
                                let case = if code == 0 { "success" } else { "failure" };
                                broker
                                    .finish_stream(machine, id, case, Some(code), "")
                                    .await;
                            }
                            Terminal { case, detail } => {
                                broker
                                    .finish_stream(machine, id, &case, None, &detail)
                                    .await
                            }
                            Ignore => {}
                        }
                    }
                }
                // A control frame: a `throw` means the machine refused or errored the command
                // (e.g. its local tools are on "ask" and there was no matching local approval).
                // resolve the waiter with that reason instead of letting it hang to the timeout.
                (Some("control"), Some(id), Some(message)) => {
                    if let Some(thrown) = message.get("throw") {
                        let reason = thrown.get("error").and_then(|value| value.as_str());
                        let reason = reason.unwrap_or("the machine refused or errored the command");
                        broker
                            .resolve(machine, id, ExecOutcome::malformed(reason))
                            .await;
                    }
                    // streamClose / heartbeat carry no result — nothing to resolve.
                }
                // hello / ping / file / messages-* — acknowledged, nothing to resolve. `hello`
                // is known (`hello{localRoot, terminalsFolder, computerId, label, …}`) but its
                // macOS `grants` are not built on either side yet: storing them waits on #147
                // stage 5, whose shape must be transcribed from the client, not guessed here.
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
    for machine in store.list_daemons(account_id).await.ok()? {
        if machine.revoked {
            continue;
        }
        let mode = store.local_exec_mode(account_id, &machine.machine_id).await;
        let mode = mode.ok().flatten();
        if mode
            .as_deref()
            .is_some_and(|mode| LocalExecMode::from_stored(mode) != LocalExecMode::Never)
        {
            // The label rides along so prompts can name the ACTUAL enrolled computer
            // ("Uriah's-MacBook-Pro.local") instead of guessing at an OS or hardware name.
            return Some((machine.machine_id, machine.label));
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
            LocalExecDecision::Deny { why, rule } => {
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
                        rule.as_deref(),
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
#[path = "../tests/unit/local_exec.rs"]
mod tests;
