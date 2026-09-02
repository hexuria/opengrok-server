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
//!   in OpenGrok (`resolveAutoReviewApproval`), which Finishes the synthesized run; the MCP
//!   client retries under the remembered call id, and the remembered yes releases the gate or
//!   skips the judge by the ask's reason. Reverse-exec is excluded before execute, so ExecConsent
//!   cannot arrive here.
//! - **The reverse-exec channel (`user_machine_shell`) is not carried over MCP in v1.** It reaches
//!   the account owner's real machine; a leaked bot key must not widen from "this coworker's box"
//!   to "the owner's laptop" through a new external ingress. It is excluded from the listing and
//!   refused on call.
//! - **A computerless coworker lists an EMPTY toolbox; an unreachable computer is an ERROR.** An
//!   empty success is the dangerous reply (CLAUDE.md §3): a KEK/credential/DB failure must not
//!   masquerade as "this coworker simply has no tools".
//! - **Allowed calls still have no durable per-call audit.** Box and plugin tool calls in a
//!   normal run are journaled by the harness; a successful `run_one` is still only a redacted
//!   tracing line. An Ask now has a run row (that is the card); a full audit of every door call
//!   is still later.

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
use crate::gateway::GatewayState;
use opengrok_core::CoworkerId;
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{Run, RunCommand, RunStatus, RunView, SuspendReason};
use opengrok_harness::tools::ToolRunner;
use opengrok_tools::{AwaitingReason, ToolCall, USER_MACHINE_SHELL, redact_arguments};

/// The authenticated caller, resolved once by `guard` and read by the handler. A bot key hard-binds
/// one account to one coworker, so this pair is the whole identity — the MCP client cannot name a
/// different coworker (unlike the AG-UI run path's `forwardedProps`).
#[derive(Clone)]
struct McpPrincipal {
    account: AccountId,
    coworker: CoworkerId,
}

/// The `/mcp` surface: the rmcp streamable-HTTP service behind the auth-and-origin guard.
pub fn router(state: GatewayState) -> axum::Router {
    axum::Router::new()
        .fallback_service(service(state.clone()))
        .layer(axum::middleware::from_fn_with_state(state, guard))
}

fn service(state: GatewayState) -> StreamableHttpService<McpDoor, LocalSessionManager> {
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
async fn guard(State(state): State<GatewayState>, mut req: Request, next: Next) -> Response {
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
            req.extensions_mut()
                .insert(McpPrincipal { account, coworker });
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
    state: GatewayState,
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
/// Per-coworker mutex shared with `resolveAutoReviewApproval` so remember/take/execute
/// cannot interleave a leftover yes.
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

/// One allow-once from an answered MCP card, waiting for the client's retry. Process-local on
/// purpose: a restart means the retry is judged again (narrower than silently running).
/// Matched with `Value` equality (not `to_string`): arguments round-trip through jsonb, which
/// does not keep object key order, and the executor's skip is per call, never per tool.
struct McpAllowOnce {
    arguments: serde_json::Value,
    call_id: String,
    /// `true` when the yes answered a policy (gate) ask, `false` for the judge's. The retry
    /// releases the gate or skips the judge accordingly — never both.
    gate: bool,
}

static MCP_ALLOW_ONCE: LazyLock<Mutex<HashMap<String, Vec<McpAllowOnce>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn coworker_tool_key(coworker: &CoworkerId, tool: &str) -> String {
    format!("{}:{tool}", coworker.as_str())
}

/// Record that this coworker may retry this exact tool+args once under `call_id`. `gate` says
/// which ask the yes answered: a policy grant's (release the gate) or the judge's (skip it).
pub fn remember_mcp_allow_once(
    coworker: &CoworkerId,
    tool: &str,
    arguments: &serde_json::Value,
    call_id: String,
    gate: bool,
) {
    let mut map = match MCP_ALLOW_ONCE.lock() {
        Ok(map) => map,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.entry(coworker_tool_key(coworker, tool))
        .or_default()
        .push(McpAllowOnce {
            arguments: arguments.clone(),
            call_id,
            gate,
        });
}

/// Take the pending allow-once for this coworker+tool+args, if any: `(call_id, gate)`.
pub fn take_mcp_allow_once(
    coworker: &CoworkerId,
    tool: &str,
    arguments: &serde_json::Value,
) -> Option<(String, bool)> {
    let mut map = match MCP_ALLOW_ONCE.lock() {
        Ok(map) => map,
        Err(poisoned) => poisoned.into_inner(),
    };
    let key = coworker_tool_key(coworker, tool);
    let list = map.get_mut(&key)?;
    let index = list.iter().position(|yes| yes.arguments == *arguments)?;
    let yes = list.remove(index);
    if list.is_empty() {
        map.remove(&key);
    }
    Some((yes.call_id, yes.gate))
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
        // Reverse-exec is not reachable over MCP; refuse it by name rather than dispatching.
        if request.name.as_ref() == USER_MACHINE_SHELL {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "the reverse-exec channel (running on your own machine) is not available over \
                 MCP — use the Open Grok app for that",
            )])
            .into());
        }
        let mut call = ToolCall {
            id: format!("mcp_{}", uuid::Uuid::now_v7().simple()),
            name: request.name.to_string(),
            arguments: request
                .arguments
                .map(serde_json::Value::Object)
                .unwrap_or_else(|| serde_json::json!({})),
        };
        // Take, pending-card check, execute, persist, and remember all sit under this lock
        // so a retry cannot run while a card is pending, and an Approve cannot interleave
        // a leftover yes.
        let lock = coworker_lock(&principal.coworker);
        let _guard = lock.lock().await;
        let once = take_mcp_allow_once(&principal.coworker, &call.name, &call.arguments);
        if let Some((id, _)) = once.as_ref() {
            call.id = id.clone();
        } else {
            match existing_mcp_ask(&self.state, &principal.account, &principal.coworker, &call)
                .await
            {
                Ok(Some(request_id)) => {
                    return Ok(
                        CallToolResult::error(vec![ContentBlock::text(ask_waiting_text(
                            "waiting for approval",
                            &request_id,
                        ))])
                        .into(),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%error, "mcp door: could not look up a pending ask");
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "approval is needed, but the card could not be raised; grant this \
                         tool to the coworker in the Open Grok app or console, or retry.",
                    )])
                    .into());
                }
            }
        }
        // Which gate the remembered yes opens: a policy yes releases the gate, a judge yes skips
        // the judge. The same call id, never both lists.
        let (gate_yes, review_yes): (Vec<String>, Vec<String>) = match once.as_ref() {
            Some((id, true)) => (vec![id.clone()], Vec::new()),
            Some((id, false)) => (Vec::new(), vec![id.clone()]),
            None => (Vec::new(), Vec::new()),
        };
        let runner = match self.toolbox_for(&principal, &gate_yes, &review_yes).await {
            Toolbox::Ready(runner) => runner,
            Toolbox::NoComputer => {
                if let Some((id, gate)) = once {
                    remember_mcp_allow_once(
                        &principal.coworker,
                        &call.name,
                        &call.arguments,
                        id,
                        gate,
                    );
                }
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "this coworker has no computer, so it has no tools to run",
                )])
                .into());
            }
            Toolbox::Unavailable => {
                if let Some((id, gate)) = once {
                    remember_mcp_allow_once(
                        &principal.coworker,
                        &call.name,
                        &call.arguments,
                        id,
                        gate,
                    );
                }
                return Err(McpError::internal_error(
                    "this coworker's computer could not be reached right now",
                    None,
                ));
            }
        };
        let result = runner.run_one(&call).await;
        if result.awaiting_approval {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                reply_to_ask(
                    &self.state,
                    &principal.account,
                    &principal.coworker,
                    &call,
                    &result,
                )
                .await,
            )])
            .into());
        }
        if !result.ok
            && let Some((id, gate)) = once
        {
            // A failed execute must not spend the yes — the client will retry.
            remember_mcp_allow_once(&principal.coworker, &call.name, &call.arguments, id, gate);
        }
        // Allowed calls still have no durable audit (see the module note). An Ask is persisted
        // above, under the lock. Arguments are redacted the way the judge redacts them.
        tracing::info!(
            coworker = %principal.coworker.as_str(),
            tool = %call.name,
            ok = result.ok,
            awaiting = result.awaiting_approval,
            arguments = %redact_arguments(&call.arguments),
            "mcp door call"
        );
        let response = if result.ok {
            CallToolResult::success(vec![ContentBlock::text(result.content)])
        } else {
            // A refusal is content the model can reason about, exactly as in a run.
            // Ask is handled inside the lock above so persist is serialized per coworker.
            CallToolResult::error(vec![ContentBlock::text(result.content)])
        };
        Ok(response.into())
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
    state: &GatewayState,
    account: &AccountId,
    coworker: &CoworkerId,
    call: &ToolCall,
    result: &opengrok_tools::ToolResult,
) -> String {
    let reason = match result.awaiting_reason {
        Some(AwaitingReason::AutoReview) => SuspendReason::AutoReview,
        Some(AwaitingReason::PolicyApproval) => SuspendReason::PolicyApproval,
        // ExecConsent is reverse-exec, which is refused by name before execute. Do not promise
        // a card that does not exist.
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

/// Synthesize a durable run + auto-review card for an MCP Ask so the person can answer it
/// in OpenGrok. Returns the `requestId` (the tool call id). A retry of the same tool+args
/// while a card is already pending reuses that requestId — a second persist would flood cards.
async fn persist_mcp_ask(
    state: &GatewayState,
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
    // Distinct from `gateway-{coworker}`: this run is the MCP call's audit row, not a
    // conversation turn. Answering it Finishes the run; it must not resume as a model turn.
    let thread_id = format!("mcp-{}", coworker.as_str());
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
        SuspendReason::PolicyApproval => crate::gateway::cards::policy_approval_card(
            &entry_id,
            &call.id,
            "pending",
            &call.name,
            &call.arguments,
            why,
            at_ms,
        ),
        _ => crate::gateway::cards::auto_review_card(
            &entry_id,
            &call.id,
            "pending",
            &call.name,
            &call.arguments,
            Some(opengrok_tools::review::REVIEW_ASK_REASON),
            at_ms,
        ),
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(coworker, &card, at_ms)
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
    // Persist-only would be enough for a reload; emitting means an open desktop sees the
    // card without reconnecting. No subscriber is an ordinary morning (live::emit).
    crate::gateway::live::emit_transcript(state, coworker.as_str(), "appended", card);
    Ok(call.id.clone())
}

async fn existing_mcp_ask(
    state: &GatewayState,
    account: &AccountId,
    coworker: &CoworkerId,
    call: &ToolCall,
) -> Result<Option<String>, opengrok_store::StoreError> {
    let waiting = state.agui.auth.store.awaiting_approval(account).await?;
    for run_id in waiting {
        let (run, seq) = match state.agui.auth.store.load_run(&run_id).await {
            Ok(pair) => pair,
            Err(error) => return Err(error),
        };
        if run.thread_id != format!("mcp-{}", coworker.as_str()) {
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
        match card_pending_for(state, coworker, &pending.call_id).await {
            Ok(true) => return Ok(Some(pending.call_id.clone())),
            Ok(false) => fail_stuck_mcp_run(state, account, &run_id, run, seq).await?,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

async fn card_pending_for(
    state: &GatewayState,
    coworker: &CoworkerId,
    request_id: &str,
) -> Result<bool, opengrok_store::StoreError> {
    let entries = state.agui.auth.store.gateway_transcript(coworker).await?;
    Ok(entries.iter().any(|entry| {
        entry["message"]["type"] == "auto-review-approval"
            && entry["message"]["approval"]["requestId"] == request_id
            && entry["message"]["approval"]["status"] == "pending"
    }))
}

async fn fail_stuck_mcp_run(
    state: &GatewayState,
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
