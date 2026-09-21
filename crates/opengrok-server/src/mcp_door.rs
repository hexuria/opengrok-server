//! The MCP door — an MCP client (Claude Code first) borrows a coworker's toolbox.
//!
//! This is a FRONT DOOR onto the same executor every run uses, not a second tool path. The
//! bearer names the coworker (a slice-10 bot key), `tools_for_coworker` builds the same
//! policy-and-auto-review-wired runner a run would get, and every call goes through
//! `Executor::execute` — identity overwrite, the primary gate, the judge. The door adds
//! transport, never authority.
//!
//! What is enforced where, and what is deliberately deferred:
//! - **Auth and browser-origin refusal are a transport-edge layer** (`guard`), so a missing,
//!   personal, or revoked credential is a real `401`/`403` before rmcp is reached — and even
//!   `initialize` requires a live bot key. The layer stashes the resolved principal so the
//!   handler never re-derives it.
//! - **An `ask` raises a real card — the judge's and the policy grant's alike.** `run_one` has no
//!   in-flight run to suspend, so the door synthesizes one (Start + Suspend) and emits the same
//!   `auto-review-approval` card a shell Ask would; a policy ask carries the grant's reason and
//!   no proposed rule. The MCP reply names `requestId` and does **not** wait; the person answers
//!   in OpenGrok (`POST /ag-ui/runs/{id}/answer`), which flips the card, remembers the yes and
//!   FINISHES the synthesized run rather than resuming it as a turn; the MCP client retries under
//!   the remembered call id, and the remembered yes releases the gate or skips the judge by the
//!   ask's reason. Reverse-exec is excluded before execute, so ExecConsent cannot arrive here.
//! - **The reverse-exec channel (`user_machine_shell`) is not carried over MCP in v1.** It reaches
//!   the account owner's real machine; a leaked bot key must not widen from "this coworker's box"
//!   to "the owner's laptop" through a new external ingress. It is excluded from the listing and
//!   refused on call.
//! - **A computerless coworker lists an EMPTY toolbox; an unreachable computer is an ERROR.** An
//!   empty success is the dangerous reply (CLAUDE.md §3): a KEK/credential/DB failure must not
//!   masquerade as "this coworker simply has no tools".
//! - **Every door call leaves a durable row** (`mcp_call_audit`): tool, redacted arguments,
//!   outcome, the request id, written after the call by `McpDoor::audit`. A run journals its
//!   own tool calls; a door call has no run (an Ask makes one — that is the card), so without
//!   this row a bot key's use was a tracing line and nothing else. The row is written AFTER the
//!   call because the outcome is part of it; a failed write is logged at error and does not
//!   turn a finished call into a refusal — the tool has already run.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{ErrorData as McpError, ServerHandler};

use crate::agui::routes::{principal_from_bearer, tools_for_coworker};

/// How long an MCP call waits for a sleeping box before trying its command anyway. The MCP client
/// (Claude Code) has its own request timeout, so this stays well under it; a box still starting
/// answers the command with 409 `box_starting`, which reaches the caller as a truthful tool
/// result it can retry, rather than a request that times out with nothing to show.
const MCP_WAKE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);
use crate::host_state::HostState;
use opengrok_core::CoworkerId;
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{PendingApproval, Run, RunCommand, RunStatus, RunView, SuspendReason};
use opengrok_harness::tools::ToolRunner;
use opengrok_tools::{AwaitingReason, ToolCall, USER_MACHINE_SHELL, redact_arguments};

/// The authenticated caller, resolved once by `guard` and read by the handler. A bot key hard-binds
/// one account to one coworker, so this pair is the whole identity — the MCP client cannot name a
/// different coworker (unlike the AG-UI run path's `forwardedProps`).
#[derive(Clone)]
struct McpPrincipal {
    account: AccountId,
    coworker: CoworkerId,
    /// The request id the tracing layer stamped, so an audit row and the request log line
    /// that produced it can be found by the same handle.
    request_id: String,
}

/// The `/mcp` surface: the rmcp streamable-HTTP service behind the auth-and-origin guard.
pub fn router(state: HostState) -> axum::Router {
    axum::Router::new()
        .fallback_service(service(state.clone()))
        .layer(axum::middleware::from_fn_with_state(state, guard))
}

fn service(state: HostState) -> StreamableHttpService<McpDoor, LocalSessionManager> {
    StreamableHttpService::new(
        move || {
            Ok(McpDoor {
                state: state.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        // Stateless on purpose: every request re-derives its principal from the bearer (the guard
        // above), so a session would only cache what must not be cached — policy is enforced on
        // every action, not once at the start (CLAUDE.md #6). `json_response` keeps a plain call a
        // plain reply; the transport still falls back to SSE when a stream is genuinely needed.
        {
            let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
            config.legacy_session_mode = false;
            config.json_response = true;
            config
        },
    )
}

/// The transport edge: refuse a browser origin, require a live bot key, stash the principal.
///
/// This is where the door is made to fail closed the way every other route family does. A missing,
/// personal, or revoked credential never reaches rmcp (which would answer 200 + a JSON-RPC error);
/// it gets a real `401`/`403`, so an OAuth-capable client can discover it must authenticate, and
/// `initialize` itself is gated.
async fn guard(State(state): State<HostState>, mut req: Request, next: Next) -> Response {
    // A browser page must never be able to drive this, with or without a token — the same refusal
    // the gateway makes, before anything else.
    if req.headers().contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "browser origins are not served").into_response();
    }
    // RFC 8707: a key the OAuth door minted names the resource it is for. One minted for
    // another server's `/mcp` must not open this one, however valid its signature. Hand-minted
    // keys carry no audience and stay accepted — they are ours.
    if let Some(token) = bearer_of(req.headers())
        && let Ok(claims) = state
            .agui
            .auth
            .minter
            .verify_claims::<crate::auth::bot_keys::BotKeyClaims>(token)
        && let Some(aud) = claims.aud.as_deref()
        && aud != crate::auth::oauth_mcp::resource_uri(&state.agui.auth.public_url)
    {
        return unauthorized(
            &state.agui.auth.public_url,
            "this token was issued for another server",
        );
    }
    let public_url = state.agui.auth.public_url.clone();
    match principal_from_bearer(&state.agui, req.headers()).await {
        Ok(Some((account, Some(coworker)))) => {
            let request_id = crate::request_id(req.headers());
            req.extensions_mut().insert(McpPrincipal {
                account,
                coworker,
                request_id,
            });
            next.run(req).await
        }
        Ok(Some((_, None))) => unauthorized(
            &public_url,
            "this token names a person, not a coworker — mint a bot key \
             (POST /coworkers/{id}/keys) or sign in through OAuth and use that as the bearer",
        ),
        Ok(None) => unauthorized(
            &public_url,
            "missing or unrecognised bearer — use a coworker's bot key, or sign in through OAuth",
        ),
        // `principal_from_bearer`'s only Err today is a revoked key; a revoked key must be named
        // revoked, never silently downgraded to anonymous.
        Err(_) => unauthorized(&public_url, "this bot key has been revoked"),
    }
}

fn bearer_of(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Every unauthenticated answer — the initial `initialize` included — carries the challenge an
/// OAuth-capable client discovers the authorization server from (RFC 9728 via MCP authorization,
/// 2026-07-28): where the protected-resource metadata is, and the scope to ask for.
fn unauthorized(public_url: &str, message: &str) -> Response {
    let challenge = format!(
        "Bearer resource_metadata=\"{}\", scope=\"{}\"",
        crate::auth::oauth_mcp::protected_resource_metadata_url(public_url),
        crate::auth::oauth_mcp::SCOPE,
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        message.to_string(),
    )
        .into_response()
}

pub struct McpDoor {
    state: HostState,
}

/// What a coworker's toolbox resolved to. The three cases are kept apart because collapsing them is
/// the "empty success is the dangerous reply" hazard: an unreachable computer must not read as one
/// that has no tools.
enum Toolbox {
    Ready(Box<ToolRunner>),
    /// The coworker genuinely has no computer assigned — an empty toolbox is the honest answer.
    NoComputer,
    /// The coworker HAS a computer, but it could not be resolved right now (credential sealed under
    /// a lost KEK, provider down, database hiccup). An error, never a silent empty list.
    Unavailable,
}

impl McpDoor {
    fn principal(context: &RequestContext<RoleServer>) -> Result<McpPrincipal, McpError> {
        context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<McpPrincipal>())
            .cloned()
            .ok_or_else(|| {
                // The guard inserts it on every path that reaches rmcp, so absence is our bug.
                McpError::internal_error("the request lost its authenticated principal", None)
            })
    }

    async fn toolbox_for(
        &self,
        principal: &McpPrincipal,
        gate_yes: &[String],
        review_yes: &[String],
    ) -> Toolbox {
        let Ok((coworker, _)) = self
            .state
            .agui
            .auth
            .store
            .load_coworker(&principal.coworker)
            .await
        else {
            return Toolbox::Unavailable;
        };
        if coworker.computer().is_none() {
            return Toolbox::NoComputer;
        }
        match tools_for_coworker(
            &self.state.agui,
            &principal.account,
            &principal.coworker,
            gate_yes,
            review_yes,
            MCP_WAKE_PATIENCE,
        )
        .await
        {
            Some(runner) => Toolbox::Ready(Box::new(runner)),
            // A computer is assigned but the runner could not be built — the box could not be
            // resolved. Truthful error, not an empty toolbox.
            None => Toolbox::Unavailable,
        }
    }
}

/// Serialize tool calls per coworker: the executor runs one call, but concurrent MCP requests would
/// race on the same box (a `write_file` and the `shell` that reads it arriving together). `run_all`
/// serializes a run's calls for exactly this reason; the door has no run, so it holds the line here.
/// Per-coworker mutex shared with `settle_mcp_answer` so remember/take/execute cannot
/// interleave a leftover yes.
pub fn coworker_lock(coworker: &CoworkerId) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut map = match LOCKS.lock() {
        Ok(map) => map,
        // A poisoned lock means a prior holder panicked; the map itself is still usable.
        Err(poisoned) => poisoned.into_inner(),
    };
    map.entry(coworker.as_str().to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// How long an answered yes waits for its retry. The process-local map this replaced forgot a
/// yes on restart (narrower than silently running); a row forgets it by the clock instead, and
/// ten minutes is longer than any client's retry and shorter than anybody's memory of why
/// they said yes.
pub const ALLOW_ONCE_TTL_MS: i64 = 10 * 60 * 1_000;

/// Record that this coworker may retry this exact tool+args once under `call_id`. `gate` says
/// which ask the yes answered: a policy grant's (release the gate) or the judge's (skip it).
/// A row, not a map: the retry lands on whichever replica the client reaches.
pub async fn remember_mcp_allow_once(
    store: &opengrok_store::PgStore,
    coworker: &CoworkerId,
    account: Option<&str>,
    tool: &str,
    arguments: &serde_json::Value,
    call_id: &str,
    gate: bool,
) -> Result<(), opengrok_store::StoreError> {
    remember_mcp_allow_once_at(
        store,
        opengrok_store::AllowOnce {
            coworker,
            account,
            tool,
            arguments,
            call_id,
            gate,
            at_ms: chrono::Utc::now().timestamp_millis(),
        },
    )
    .await
}

/// The same, stamped by the caller: a give-back keeps the yes's ORIGINAL stamp, so a retry loop
/// against a down computer does not renew an approval every time it fails to run.
async fn remember_mcp_allow_once_at(
    store: &opengrok_store::PgStore,
    once: opengrok_store::AllowOnce<'_>,
) -> Result<(), opengrok_store::StoreError> {
    store.remember_mcp_allow_once(once, ALLOW_ONCE_TTL_MS).await
}

/// Take the pending allow-once for this coworker+tool+args, if any: `(call_id, gate)`. Matched
/// by jsonb value (key order is not part of it — the arguments round-tripped through the card).
/// A store error reads as "no yes": the call is judged again, which is the narrow side.
pub async fn take_mcp_allow_once(
    store: &opengrok_store::PgStore,
    coworker: &CoworkerId,
    account: Option<&str>,
    tool: &str,
    arguments: &serde_json::Value,
) -> Option<(String, bool)> {
    take_mcp_allow_once_stamped(store, coworker, account, tool, arguments)
        .await
        .map(|(call_id, gate, _)| (call_id, gate))
}

/// The take with the yes's original stamp, for the door: what it gives back must carry it.
async fn take_mcp_allow_once_stamped(
    store: &opengrok_store::PgStore,
    coworker: &CoworkerId,
    account: Option<&str>,
    tool: &str,
    arguments: &serde_json::Value,
) -> Option<(String, bool, i64)> {
    match store
        .take_mcp_allow_once(
            coworker,
            account,
            tool,
            arguments,
            chrono::Utc::now().timestamp_millis(),
            ALLOW_ONCE_TTL_MS,
        )
        .await
    {
        Ok(yes) => yes,
        Err(error) => {
            tracing::error!(%error, coworker = %coworker.as_str(), tool, "mcp door: could not read the remembered yes; judging the call again");
            None
        }
    }
}

/// One OpenAI-function-shape schema → an rmcp `Tool`, or `None` (logged) if it is malformed. A tool
/// the executor would run but that we cannot advertise is a silently lost capability, so it is
/// logged rather than dropped in silence.
fn to_mcp_tool(schema: &serde_json::Value) -> Option<Tool> {
    let function = schema.get("function")?;
    let Some(name) = function.get("name").and_then(|n| n.as_str()) else {
        tracing::warn!(
            ?schema,
            "mcp door: a tool schema had no name and was dropped"
        );
        return None;
    };
    let description = function
        .get("description")
        .and_then(|d| d.as_str())
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    // A tool with no real schema still needs `{"type":"object"}` — an empty object is not a valid
    // MCP inputSchema and strict hosts reject it.
    let parameters: JsonObject = match function.get("parameters") {
        Some(serde_json::Value::Object(map)) if !map.is_empty() => map.clone(),
        _ => {
            let mut map = JsonObject::new();
            map.insert(
                "type".to_string(),
                serde_json::Value::String("object".to_string()),
            );
            map
        }
    };
    let mut tool = Tool::new(name.to_string(), String::new(), parameters);
    // Omit description entirely when empty, rather than shipping `"description": ""`.
    tool.description = description.map(Into::into);
    Some(tool)
}

impl ServerHandler for McpDoor {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            // Identify as OpenGrok, not the rmcp SDK: the default `from_build_env` reports rmcp's
            // own crate name and version to every client.
            .with_server_info(Implementation::new("opengrok", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Tools run on the coworker this key names, on that coworker's own computer, \
                 under the account's policy. A call that needs a person's approval (the \
                 account's auto-review, or the coworker's policy) is refused with a requestId; \
                 a card is waiting in the Open Grok app — answer it there, then retry. \
                 Reverse-exec (your own machine) is not available over MCP.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let principal = Self::principal(&context)?;
        // ttlMs 0 + private, required by protocol 2026-07-28 (SEP-2549) and, for Claude Code today,
        // required in practice — a listing without them is rejected. Chosen, not defaulted: the
        // list is policy-filtered per bot key, so a cached listing must not outlive a policy change
        // or leak across authorization contexts.
        let uncacheable = |tools| {
            ListToolsResult::with_all_items(tools)
                .with_ttl_ms(0)
                .with_cache_scope(CacheScope::Private)
        };
        let runner = match self.toolbox_for(&principal, &[], &[]).await {
            Toolbox::Ready(runner) => runner,
            Toolbox::NoComputer => return Ok(uncacheable(Vec::new())),
            Toolbox::Unavailable => {
                return Err(McpError::internal_error(
                    "this coworker's computer could not be reached right now — try again, or \
                     check the deployment's box credentials",
                    None,
                ));
            }
        };
        let mut seen = std::collections::HashSet::new();
        let tools = runner
            .tool_schemas()
            .into_iter()
            // The reverse-exec channel is not carried over MCP in v1 (see the module note).
            .filter(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    != Some(USER_MACHINE_SHELL)
            })
            .filter_map(|schema| to_mcp_tool(&schema))
            // Names must be unique within a server; a duplicate is undefined behaviour for a client.
            .filter(|tool| seen.insert(tool.name.to_string()))
            .collect();
        Ok(uncacheable(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let principal = Self::principal(&context)?;
        let call = ToolCall {
            id: format!("mcp_{}", uuid::Uuid::now_v7().simple()),
            name: request.name.to_string(),
            arguments: request
                .arguments
                .map(serde_json::Value::Object)
                .unwrap_or_else(|| serde_json::json!({})),
        };
        let done = self.dispatch(&principal, call).await;
        self.audit(&principal, &done).await;
        done.reply
    }
}

/// What one door call came to — the audit row's word for it. See `McpCallView` for the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok,
    Failed,
    Refused,
    Awaiting,
    Error,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Refused => "refused",
            Self::Awaiting => "awaiting",
            Self::Error => "error",
        }
    }
}

/// A dispatched call: the reply for the client, and what the audit needs to say about it. The
/// call travels with it because a retry that spends a remembered yes takes over that yes's id.
struct Dispatch {
    reply: Result<CallToolResponse, McpError>,
    outcome: Outcome,
    call: ToolCall,
}

impl Dispatch {
    fn said(call: ToolCall, outcome: Outcome, result: CallToolResult) -> Self {
        Self {
            reply: Ok(result.into()),
            outcome,
            call,
        }
    }

    fn failed(call: ToolCall, error: McpError) -> Self {
        Self {
            reply: Err(error),
            outcome: Outcome::Error,
            call,
        }
    }
}

impl McpDoor {
    /// The call itself: take, pending-card check, execute, persist, remember — every path a
    /// `tools/call` can take, each ending in a `Dispatch` so the audit sees all of them.
    async fn dispatch(&self, principal: &McpPrincipal, mut call: ToolCall) -> Dispatch {
        // Reverse-exec is not reachable over MCP; refuse it by name rather than dispatching.
        if call.name == USER_MACHINE_SHELL {
            return Dispatch::said(
                call,
                Outcome::Refused,
                CallToolResult::error(vec![ContentBlock::text(
                    "the reverse-exec channel (running on your own machine) is not available \
                     over MCP — use the Open Grok app for that",
                )]),
            );
        }
        // Take, pending-card check, execute, persist, and remember all sit under this lock
        // so a retry cannot run while a card is pending, and an Approve cannot interleave
        // a leftover yes.
        let lock = coworker_lock(&principal.coworker);
        let _guard = lock.lock().await;
        let store = &self.state.agui.auth.store;
        // PAIRED WITH the remember in `settle_mcp_answer`, which writes the account of whoever
        // pressed approve. A yes is spendable only by the account that gave it, so if these two
        // ever resolve to different people the yes is silently lost and the retry asks again.
        // See the note at that call site before sharing goes live.
        let once = take_mcp_allow_once_stamped(
            store,
            &principal.coworker,
            Some(principal.account.as_str()),
            &call.name,
            &call.arguments,
        )
        .await;
        if let Some((id, _, _)) = once.as_ref() {
            call.id = id.clone();
        } else {
            match existing_mcp_ask(&self.state, &principal.account, &principal.coworker, &call)
                .await
            {
                Ok(Some(request_id)) => {
                    return Dispatch::said(
                        call,
                        Outcome::Awaiting,
                        CallToolResult::error(vec![ContentBlock::text(ask_waiting_text(
                            "waiting for approval",
                            &request_id,
                        ))]),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%error, "mcp door: could not look up a pending ask");
                    return Dispatch::said(
                        call,
                        Outcome::Error,
                        CallToolResult::error(vec![ContentBlock::text(
                            "approval is needed, but the card could not be raised; grant this \
                             tool to the coworker in the Open Grok app or console, or retry.",
                        )]),
                    );
                }
            }
        }
        // Which gate the remembered yes opens: a policy yes releases the gate, a judge yes skips
        // the judge. The same call id, never both lists.
        let (gate_yes, review_yes): (Vec<String>, Vec<String>) = match once.as_ref() {
            Some((id, true, _)) => (vec![id.clone()], Vec::new()),
            Some((id, false, _)) => (Vec::new(), vec![id.clone()]),
            None => (Vec::new(), Vec::new()),
        };
        let runner = match self.toolbox_for(principal, &gate_yes, &review_yes).await {
            Toolbox::Ready(runner) => runner,
            Toolbox::NoComputer => {
                if let Some((id, gate, at_ms)) = once {
                    Self::give_back(store, principal, &call, &id, gate, at_ms).await;
                }
                return Dispatch::said(
                    call,
                    Outcome::Refused,
                    CallToolResult::error(vec![ContentBlock::text(
                        "this coworker has no computer, so it has no tools to run",
                    )]),
                );
            }
            Toolbox::Unavailable => {
                if let Some((id, gate, at_ms)) = once {
                    Self::give_back(store, principal, &call, &id, gate, at_ms).await;
                }
                return Dispatch::failed(
                    call,
                    McpError::internal_error(
                        "this coworker's computer could not be reached right now",
                        None,
                    ),
                );
            }
        };
        let result = runner.run_one(&call).await;
        if result.awaiting_approval {
            let text = reply_to_ask(
                &self.state,
                &principal.account,
                &principal.coworker,
                &call,
                &result,
            )
            .await;
            return Dispatch::said(
                call,
                Outcome::Awaiting,
                CallToolResult::error(vec![ContentBlock::text(text)]),
            );
        }
        if !result.ok
            && let Some((id, gate, at_ms)) = once
        {
            // A failed execute must not spend the yes — the client will retry.
            Self::give_back(store, principal, &call, &id, gate, at_ms).await;
        }
        // The durable row is written by `audit` once this returns; this line is the same fact
        // in the request log, greppable by the request id. Arguments redacted the way the judge
        // redacts them.
        tracing::info!(
            coworker = %principal.coworker.as_str(),
            tool = %call.name,
            ok = result.ok,
            awaiting = result.awaiting_approval,
            arguments = %redact_arguments(&call.arguments),
            "mcp door call"
        );
        if result.ok {
            Dispatch::said(
                call,
                Outcome::Ok,
                CallToolResult::success(vec![ContentBlock::text(result.content)]),
            )
        } else {
            // A refusal is content the model can reason about, exactly as in a run.
            // Ask is handled inside the lock above so persist is serialized per coworker.
            Dispatch::said(
                call,
                Outcome::Failed,
                CallToolResult::error(vec![ContentBlock::text(result.content)]),
            )
        }
    }

    /// Put a taken yes back: the call did not run, so the retry may spend it — under the yes's
    /// ORIGINAL stamp, so a retry loop against a down computer does not renew the approval each
    /// time it fails to run. A failed write loses the yes — the retry asks again, which is the
    /// narrow side — and says so.
    async fn give_back(
        store: &opengrok_store::PgStore,
        principal: &McpPrincipal,
        call: &ToolCall,
        call_id: &str,
        gate: bool,
        at_ms: i64,
    ) {
        if let Err(error) = remember_mcp_allow_once_at(
            store,
            opengrok_store::AllowOnce {
                coworker: &principal.coworker,
                account: Some(principal.account.as_str()),
                tool: &call.name,
                arguments: &call.arguments,
                call_id,
                gate,
                at_ms,
            },
        )
        .await
        {
            tracing::error!(%error, call_id, "mcp door: could not give a taken yes back; the retry will ask again");
        }
    }

    /// The durable row. Written after the call because the outcome is part of it; a failed
    /// write is an error-level line, not a refusal — the tool has already run, and turning a
    /// finished call into "no" would make the client retry something that happened.
    async fn audit(&self, principal: &McpPrincipal, done: &Dispatch) {
        // The judge's redaction is JSON text, clipped with a stated cut when long; a clipped
        // one no longer parses, and is kept as the string it is rather than dropped.
        let redacted = redact_arguments(&done.call.arguments);
        let arguments =
            serde_json::from_str(&redacted).unwrap_or_else(|_| serde_json::Value::String(redacted));
        let row = opengrok_store::NewMcpCall {
            call_id: &done.call.id,
            tool: &done.call.name,
            arguments,
            outcome: done.outcome.as_str(),
            request_id: &principal.request_id,
            at_ms: chrono::Utc::now().timestamp_millis(),
        };
        if let Err(error) = self
            .state
            .agui
            .auth
            .store
            .insert_mcp_call(&principal.account, &principal.coworker, &row)
            .await
        {
            tracing::error!(
                %error,
                call_id = %done.call.id,
                tool = %done.call.name,
                "mcp door: the call finished but its audit row was not written"
            );
        }
    }
}

fn ask_waiting_text(content: &str, request_id: &str) -> String {
    format!(
        "{content} — a card is waiting in the Open Grok app (requestId: {request_id}). \
         Answer it there, then retry this call."
    )
}

/// An MCP Ask: raise a card when we have one, fail closed when we do not, never wait.
/// Returns the error text the door sends the MCP client. Public so the door test can drive
/// this path without a Ready toolbox (a computer) — `run_one` only Asks after that.
pub async fn reply_to_ask(
    state: &HostState,
    account: &AccountId,
    coworker: &CoworkerId,
    call: &ToolCall,
    result: &opengrok_tools::ToolResult,
) -> String {
    let reason = match result.awaiting_reason {
        Some(AwaitingReason::AutoReview) => SuspendReason::AutoReview,
        Some(AwaitingReason::PolicyApproval) => SuspendReason::PolicyApproval,
        // ExecConsent is reverse-exec, which is refused by name before execute. UserForm
        // and Credential session-broker are not available over MCP: there is no in-chat card, and a
        // site password must never ride this door.
        _ => {
            return format!(
                "{} — approval is not available over MCP; grant this tool to the coworker in the \
                 Open Grok app or console, or run it from the app.",
                result.content
            );
        }
    };
    let why = result
        .content
        .strip_prefix("waiting for approval: ")
        .map(str::trim)
        .filter(|why| !why.is_empty());
    match reason {
        SuspendReason::AutoReview | SuspendReason::PolicyApproval => {
            match persist_mcp_ask(state, account, coworker, call, reason, why).await {
                Ok(request_id) => ask_waiting_text(&result.content, &request_id),
                Err(error) => {
                    tracing::error!(%error, "mcp door: could not raise an approval card");
                    format!(
                        "{} — approval is needed, but the card could not be raised; grant this \
                         tool to the coworker in the Open Grok app or console, or retry.",
                        result.content
                    )
                }
            }
        }
        _ => format!(
            "{} — approval is not available over MCP; grant this tool to the coworker in the \
             Open Grok app or console, or run it from the app.",
            result.content
        ),
    }
}

/// The thread an MCP Ask's run lives on. DISTINCT from `gateway-{coworker}` on purpose: this
/// run is one MCP call's audit row, not a conversation turn. The literal is written here once —
/// every reader of it goes through `is_mcp_audit_thread`, so the door that mints these runs and
/// the door that answers their cards cannot come to disagree about which runs are which.
const MCP_AUDIT_THREAD_PREFIX: &str = "mcp-";

fn mcp_audit_thread(coworker: &CoworkerId) -> String {
    format!("{MCP_AUDIT_THREAD_PREFIX}{}", coworker.as_str())
}

/// Is this run an MCP call's audit row? THE ONE TEST, because answering one as a conversation
/// would resume it: the tool would run on this side while the MCP client is told to retry, and
/// the retry would run it again under a call id that cannot spend the yes.
///
/// EXACT, AND AGAINST THE RUN'S OWN COWORKER — never a prefix on the thread alone. `POST /ag-ui`
/// takes `threadId` from the client verbatim, so a prefix test would let anybody open a turn on
/// `mcp-anything`, answer their own card, and mint an allow-once the MCP door would spend. Only
/// a run whose thread is the one `mcp_audit_thread` would have minted for its own coworker is
/// one of ours; the door mints both halves together and nothing else can.
pub fn is_mcp_audit_run(run: &Run) -> bool {
    run.coworker_id
        .as_ref()
        .is_some_and(|coworker| run.thread_id == mcp_audit_thread(coworker))
}

/// Synthesize a durable run + auto-review card for an MCP Ask so the person can answer it
/// in OpenGrok. Returns the `requestId` (the tool call id). A retry of the same tool+args
/// while a card is already pending reuses that requestId — a second persist would flood cards.
async fn persist_mcp_ask(
    state: &HostState,
    account: &AccountId,
    coworker: &CoworkerId,
    call: &ToolCall,
    reason: SuspendReason,
    why: Option<&str>,
) -> Result<String, opengrok_store::StoreError> {
    if let Some(existing) = existing_mcp_ask(state, account, coworker, call).await? {
        return Ok(existing);
    }

    let at_ms = chrono::Utc::now().timestamp_millis();
    let run_id = RunId::new();
    let thread_id = mcp_audit_thread(coworker);
    let model = state
        .agui
        .auth
        .store
        .load_coworker(coworker)
        .await
        .ok()
        .map(|(coworker, _)| coworker.model)
        .filter(|pin| !pin.trim().is_empty());

    let mut run = Run::default();
    let mut events = run
        .decide(RunCommand::Start {
            thread_id: thread_id.clone(),
            coworker_id: Some(coworker.clone()),
            model,
            // The MCP door composes no system message of its own; a resume of this run has
            // none to restore, which is the pre-existing behaviour.
            system: None,
            at_ms,
        })
        .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
    for event in &events {
        run.apply(event);
    }
    let suspended = run
        .decide(RunCommand::Suspend {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            arguments: call.arguments.clone(),
            reason,
            at_ms,
        })
        .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
    for event in &suspended {
        run.apply(event);
    }
    events.extend(suspended);
    let view = RunView {
        id: run_id.clone(),
        thread_id: thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    let seq = state
        .agui
        .auth
        .store
        .append_run(&run_id, 0, &events, &view, Some(account))
        .await?;

    let entry_id = format!("e_{}", uuid::Uuid::now_v7());
    let card = match reason {
        SuspendReason::PolicyApproval => crate::cards::policy_approval_card(
            &entry_id,
            &call.id,
            "pending",
            &call.name,
            &call.arguments,
            why,
            at_ms,
        ),
        // The ask's OWN sentence, the way `resume::card_for` does it. Hardcoding the
        // judge's default reason here overwrote the real one: an egress-tunnel ask says "this
        // would use your network through the egress tunnel", and the person was shown "your
        // auto-review instructions did not clearly allow this" instead.
        _ => crate::cards::auto_review_card(
            &entry_id,
            &call.id,
            "pending",
            &call.name,
            &call.arguments,
            Some(
                why.filter(|why| !why.is_empty())
                    .unwrap_or(opengrok_tools::review::REVIEW_ASK_REASON),
            ),
            at_ms,
        ),
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(coworker, account, &card, at_ms)
        .await
    {
        // A suspended run with no card is stuck forever (recovery skips awaiting). Fail it so
        // a retry cannot accumulate more of them. Best-effort: if this append also fails the
        // caller still hears "card could not be raised".
        if let Ok(failed) = run.decide(RunCommand::Fail {
            reason: "the approval card could not be raised".to_string(),
            at_ms,
        }) {
            for event in &failed {
                run.apply(event);
            }
            let failed_view = RunView {
                id: run_id.clone(),
                thread_id,
                status: RunStatus::Failed,
                event_count: run.emitted.len() as i64,
                updated_at_ms: at_ms,
            };
            let _ = state
                .agui
                .auth
                .store
                .append_run(&run_id, seq, &failed, &failed_view, Some(account))
                .await;
        }
        return Err(error);
    }
    Ok(call.id.clone())
}

/// An answered MCP card, on its way to the background task that settles it. Owned, because the
/// settle is spawned: it has to hold the door's per-coworker lock, and the door holds that lock
/// across a real tool call — minutes, on a slow box. Nobody's HTTP request waits for that.
///
/// `run` and `seq` are the run AS LOADED — not yet answered. The Answer is journalled inside the
/// lock, which also closes the window where a `tools/call` arriving between the Answer's append
/// and the lock would find no pending ask, run the call, and raise a second card.
pub struct McpCardAnswer {
    pub run_id: RunId,
    pub run: Run,
    pub seq: i64,
    pub pending: PendingApproval,
    pub approved: bool,
    pub at_ms: i64,
}

/// Settle an answered MCP card: journal the Answer, flip the card in the transcript, remember a
/// yes for the retry, and FINISH the run instead of resuming it.
///
/// AN MCP-SYNTHESIZED RUN IS NOT A CONVERSATION. Resuming it would execute the tool on this side
/// while the MCP client is being told to retry — the same call twice, the second time under a new
/// call id that cannot spend this yes. So the run ends here, and what the client's retry needs
/// instead is the remembered allow-once this writes.
///
/// A NO REMEMBERS NOTHING. The card reads `denied`, the run finishes, and the client's next
/// `tools/call` raises a fresh card. A remembered no would be a standing refusal with nowhere to
/// lift it; a person who said "not this time" said exactly that.
///
/// Nothing is returned: this runs detached, so every step that can fail says so in the log and
/// the run's own status is the record. A caller that wants to know reads the run back.
pub async fn settle_mcp_answer(
    store: opengrok_store::PgStore,
    account: AccountId,
    answer: McpCardAnswer,
) {
    let run_id = answer.run_id;
    let pending = answer.pending;
    let Some(coworker) = answer.run.coworker_id.clone() else {
        tracing::error!(run = %run_id.as_str(), "mcp ask: a run with no coworker cannot be settled");
        return;
    };
    let lock = coworker_lock(&coworker);
    let _guard = lock.lock().await;

    // The Answer, inside the lock. Decided again rather than trusting the validation the route
    // already did: a second press that got past it lands here too, and the aggregate is what
    // makes an approval exactly-once.
    let mut run = answer.run;
    let events = match run.decide(RunCommand::Answer {
        call_id: pending.call_id.clone(),
        approved: answer.approved,
        by: account.to_string(),
        at_ms: answer.at_ms,
    }) {
        Ok(events) => events,
        Err(opengrok_core::run::RunError::AlreadyAnswered) => {
            tracing::info!(run = %run_id.as_str(), call_id = %pending.call_id, "mcp ask: already answered; the first answer settles it");
            return;
        }
        Err(error) => {
            tracing::error!(%error, run = %run_id.as_str(), call_id = %pending.call_id, "mcp ask: the answer could not be decided");
            return;
        }
    };
    for event in &events {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: answer.at_ms,
    };
    let seq = match store
        .append_run(&run_id, answer.seq, &events, &view, Some(&account))
        .await
    {
        Ok(seq) => seq,
        // Two presses raced to here. The one that won the append owns the rest of this — the card,
        // the yes and the ending — so this one stops rather than writing a second yes.
        Err(opengrok_store::StoreError::Conflict) => {
            tracing::warn!(run = %run_id.as_str(), call_id = %pending.call_id, "mcp ask: another answer won the append; it settles the card");
            return;
        }
        Err(error) => {
            tracing::error!(%error, run = %run_id.as_str(), call_id = %pending.call_id, "mcp ask: the answer could not be journalled");
            return;
        }
    };

    // THE FLIP IS NOT A GATE. If the transcript's pill cannot be moved, the yes and the finished
    // run still matter more: NativeChat paints its card from the approvals queue and settles it
    // locally, so a stuck pill is a stale pixel, while a lost yes is a call somebody approved
    // that never runs. Every way it can fail is an error line, and the settle carries on.
    let status = if answer.approved {
        "approved"
    } else {
        "denied"
    };
    match pending_card_entry_for(&store, &coworker, &account, &pending.call_id).await {
        Ok(Some(entry_id)) => match store
            .set_gateway_approval_status(&coworker, &account, &entry_id, status)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::error!(entry_id, call_id = %pending.call_id, "mcp ask: the answered card matched no entry to flip")
            }
            Err(error) => {
                tracing::error!(%error, entry_id, call_id = %pending.call_id, "mcp ask: the answered card could not be flipped")
            }
        },
        Ok(None) => {
            tracing::error!(call_id = %pending.call_id, "mcp ask: no pending card for this answer to flip")
        }
        Err(error) => {
            tracing::error!(%error, call_id = %pending.call_id, "mcp ask: the transcript could not be read to flip the card")
        }
    }

    if answer.approved
        && let Err(error) = remember_mcp_allow_once(
            &store,
            &coworker,
            // The person who answered owns the yes, so only their retry can spend it. PAIRED WITH
            // the take in `dispatch`, which binds the account off the MCP session: if these two
            // ever resolve to different people the yes is written and never spendable — narrow,
            // not unsafe, but it would present as "approvals do not work on shared coworkers".
            // Check the pairing when the roster widens past the owner.
            Some(account.as_str()),
            &pending.tool,
            &pending.arguments,
            &pending.call_id,
            // A policy yes releases the GATE on the retry; a judge yes skips the judge.
            pending.reason != SuspendReason::AutoReview,
        )
        .await
    {
        // The card is answered either way; a lost yes only means the retry asks again.
        tracing::error!(%error, call_id = %pending.call_id, "mcp ask: the yes could not be remembered for the retry");
    }

    let finished = match run.decide(RunCommand::Finish {
        at_ms: answer.at_ms,
    }) {
        Ok(finished) => finished,
        Err(error) => {
            tracing::error!(%error, run = %run_id.as_str(), "mcp ask: the answered run could not be finished");
            return;
        }
    };
    for event in &finished {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: answer.at_ms,
    };
    match store
        .append_run(&run_id, seq, &finished, &view, Some(&account))
        .await
    {
        Ok(_) => {}
        Err(opengrok_store::StoreError::Conflict) => {
            // Somebody ended it between our write and theirs. Which ending won is worth knowing,
            // so read it back and say — assuming "finished" here is how a stopped or failed run
            // comes to be logged as one that ended cleanly.
            match store.load_run(&run_id).await {
                Ok((settled, _)) => {
                    tracing::warn!(run = %run_id.as_str(), status = settled.status.as_str(), "mcp ask: the ending raced; the run reads as this")
                }
                Err(error) => {
                    tracing::error!(%error, run = %run_id.as_str(), "mcp ask: the ending raced and the run could not be read back")
                }
            }
            return;
        }
        Err(error) => {
            tracing::error!(%error, run = %run_id.as_str(), "mcp ask: the answered run could not be finished");
            return;
        }
    }
    if answer.approved {
        tracing::info!(
            run = %run_id.as_str(),
            coworker = %coworker.as_str(),
            call_id = %pending.call_id,
            tool = %pending.tool,
            reason = pending.reason.as_str(),
            "mcp ask: approved — the run is finished and the retry may spend the yes"
        );
    } else {
        tracing::info!(
            run = %run_id.as_str(),
            coworker = %coworker.as_str(),
            call_id = %pending.call_id,
            tool = %pending.tool,
            reason = pending.reason.as_str(),
            "mcp ask: denied — the run is finished and nothing is remembered"
        );
    }
}

async fn existing_mcp_ask(
    state: &HostState,
    account: &AccountId,
    coworker: &CoworkerId,
    call: &ToolCall,
) -> Result<Option<String>, opengrok_store::StoreError> {
    let waiting = state.agui.auth.store.awaiting_approval(account).await?;
    let thread_id = mcp_audit_thread(coworker);
    for run_id in waiting {
        let (run, seq) = match state.agui.auth.store.load_run(&run_id).await {
            Ok(pair) => pair,
            Err(error) => return Err(error),
        };
        // This coworker's audit thread, and a run the door itself minted — the same exactness
        // the answer door applies, so a conversation opened on a look-alike `threadId` cannot
        // lend its requestId to an MCP retry.
        if run.thread_id != thread_id || !is_mcp_audit_run(&run) {
            continue;
        }
        let Some(pending) = run.pending.as_ref() else {
            continue;
        };
        if !matches!(
            pending.reason,
            SuspendReason::AutoReview | SuspendReason::PolicyApproval
        ) || pending.tool != call.name
            || pending.arguments != call.arguments
        {
            continue;
        }
        // Reuse only when the card is actually in the transcript. A crash between append_run
        // and append_gateway_entry would otherwise promise a requestId nobody can answer.
        // A transcript READ error must not look like "no card": the card may already be
        // there, and Fail would leave the original press hitting 410.
        match pending_card_entry_for(&state.agui.auth.store, coworker, account, &pending.call_id)
            .await
        {
            Ok(Some(_)) => return Ok(Some(pending.call_id.clone())),
            Ok(None) => fail_stuck_mcp_run(state, account, &run_id, run, seq).await?,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// The entry id of the PENDING approval card for this request id, if the transcript holds one.
/// Two callers and one scan: the door reuses a requestId only when its card is really there, and
/// settling an answer needs that same card's id to flip it.
async fn pending_card_entry_for(
    store: &opengrok_store::PgStore,
    coworker: &CoworkerId,
    account: &AccountId,
    request_id: &str,
) -> Result<Option<String>, opengrok_store::StoreError> {
    let entries = store.gateway_transcript(coworker, account).await?;
    Ok(entries
        .iter()
        .find(|entry| {
            entry["message"]["type"] == "auto-review-approval"
                && entry["message"]["approval"]["requestId"] == request_id
                && entry["message"]["approval"]["status"] == "pending"
        })
        .and_then(|entry| entry["id"].as_str())
        .map(str::to_string))
}

async fn fail_stuck_mcp_run(
    state: &HostState,
    account: &AccountId,
    run_id: &RunId,
    mut run: Run,
    seq: i64,
) -> Result<(), opengrok_store::StoreError> {
    let at_ms = chrono::Utc::now().timestamp_millis();
    let failed = run
        .decide(RunCommand::Fail {
            reason: "the approval card was never written".to_string(),
            at_ms,
        })
        .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
    for event in &failed {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: RunStatus::Failed,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    state
        .agui
        .auth
        .store
        .append_run(run_id, seq, &failed, &view, Some(account))
        .await?;
    Ok(())
}
