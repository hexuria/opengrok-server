//! `POST /ag-ui` — the endpoint openbot adds as a Bot.
//!
//! One request opens one SSE stream carrying the events of a single run. openbot supplies the
//! thread and run ids (`RunAgentInput`), so we do not mint them: the client correlates its own UI
//! against those values, and inventing our own would orphan the reply.
//!
//! THE ENVELOPE IS THE CONTRACT, EVEN WHEN THE MIDDLE IS A STUB. A run that starts must finish or
//! error — `RUN_STARTED` … `RUN_FINISHED` — because a consumer holds its spinner open on the
//! promise of that closing event. This slice streams a real, correctly-shaped conversation with a
//! placeholder body; slice 3 replaces the middle with the harness, and the framing does not change.

use axum::extract::Path;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream};
use opengrok_wire::agui::{Event, RunAgentInput};

use super::provision;
use crate::auth::AuthState;
use opengrok_core::coworker::{CoworkerCommand, CoworkerView};
use opengrok_core::id::{CoworkerId, RunId};
use opengrok_core::run::{RunCommand, RunStatus, RunView};
use opengrok_harness::{ChatMessage, ModelDoor, ModelRequest, ToolRunner, run_conversation};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What the endpoint needs: a way to reach a model, and which route to ask for.
///
/// The door is a trait object so `OG_MODEL_DOOR=mock` swaps the whole model layer without the
/// endpoint, the harness or the projection knowing — which is what lets CI exercise this path.
#[derive(Clone)]
pub struct AgUiState {
    pub auth: AuthState,
    pub door: Arc<dyn ModelDoor>,
    pub model: String,
    /// The computer provider. Tools are bound to a coworker's own box **per request**, not here:
    /// a server-wide `ToolRunner` would carry one identity for everybody, which is precisely the
    /// confusion the identity rule exists to prevent.
    pub computer: Option<Arc<dyn opengrok_box::Computer>>,
    /// Seals connector credentials. `None` means no connector can be stored, which is a legitimate
    /// deployment — and must read as "connectors unavailable" rather than as a crash.
    pub vault: Option<Arc<opengrok_store::Vault>>,
    /// Provider configuration and the callback URL.
    pub connectors: crate::connections::routes::Connectors,
    /// Plugins installed on this server, by name. Installing one makes it *available*; a coworker
    /// still needs it in their ceiling before its tools run.
    pub plugins: Arc<BTreeMap<String, opengrok_plugins::Plugin>>,
}

/// Which coworker a run belongs to, and therefore whose computer its tools use.
///
/// AG-UI has no field for this, so the client passes it in `forwardedProps` — and it is a
/// *request*, not an authorisation: the id names a coworker, and the box comes from that
/// coworker's own row. A client naming a coworker it does not own is the next thing policy must
/// check (slice 5); today the row simply has to exist.
fn coworker_id_from(input: &RunAgentInput) -> Option<CoworkerId> {
    input
        .forwarded_props
        .get("coworkerId")
        .and_then(|value| value.as_str())
        .map(|id| CoworkerId::from_stored(id.to_string()))
}

/// Build the tools for this run, bound to this coworker's own computer and this principal's grant.
///
/// THE GRANT IS READ HERE, ON THIS TURN. Not cached from sign-in and not carried in the request:
/// a grant revoked a second ago must stop this turn (CLAUDE.md #6).
/// The same binding, addressed by coworker rather than by request — because the scheduler and the
/// monitor fire runs with no `RunAgentInput` anywhere in sight.
pub(crate) async fn tools_for_coworker(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
) -> Option<ToolRunner> {
    let coworker_id = coworker_id.clone();
    let (coworker, _) = state.auth.store.load_coworker(&coworker_id).await.ok()?;
    // A coworker with no computer gets no tools rather than tools that cannot run: a tool the
    // model is told about but that always refuses is a dead end it keeps trying.
    coworker.computer()?;
    // Resolve the provider for this coworker's computer by its account's effective sharing mode and
    // scope (per-org / per-account / per-bot), then that scope's recorded kind — so tools run on the
    // same provider that created the box.
    let (mode, org_id) = super::provision::resolve_mode(state, account_id).await;
    let (scope, scope_id, _) = super::provision::scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        coworker_id.as_str(),
    );
    let (box_id, kind, stopped) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
        .ok()
        .flatten()?;
    let computer = super::provision::provider_for(state, org_id.as_deref(), &kind).await?;
    // Idle-stop: a box paused by the sweep is resumed before this turn runs (disk was kept, so it
    // comes back where it was), and its last-used stamp is refreshed so the sweep leaves it running
    // while it is in use. Best-effort — a resume failure still lets the turn try.
    if stopped && let Err(error) = computer.resume(&box_id).await {
        tracing::warn!(%error, box_id, "could not resume an idle-stopped box; the turn may fail");
    }
    let _ = state
        .auth
        .store
        .mark_scoped_used(scope, &scope_id, chrono::Utc::now().timestamp_millis())
        .await;

    let policy = state
        .auth
        .store
        .policy_for(account_id, &coworker_id)
        .await
        .ok()?;

    // The plugins this coworker may use, connected with its own credentials.
    let (sessions, tools) = connect_plugins(state, account_id, &coworker_id, &policy).await;

    // Bind the SCOPE's live box, not the coworker's frozen hire-time id. They match at hire, but a
    // reset or re-provision changes the account's box while the aggregate id stays put — and this is
    // the same box we just resumed above, so exec must run on it, or a reset would leave the bot
    // executing against a destroyed box.
    let mut context =
        opengrok_tools::ToolContext::from_coworker(account_id.clone(), coworker_id.clone(), &coworker);
    context.box_id = Some(opengrok_core::id::BoxId::from_stored(box_id));

    let mut executor =
        opengrok_tools::Executor::with_policy(computer, policy).with_plugin_tools(sessions, tools);
    // The reverse-exec tool: offered ONLY when this account has an enrolled, enabled machine to
    // reach — otherwise the model is never told about a channel it cannot use. Bound to that
    // machine, and to this coworker for the audit origin.
    if let Some(machine_id) = crate::local_exec::enabled_machine(&state.auth.store, account_id.as_str()).await
    {
        executor = executor.with_user_machine(std::sync::Arc::new(crate::local_exec::ReverseExecSink {
            auth: state.auth.clone(),
            coworker_id: coworker_id.as_str().to_string(),
            machine_id,
        }));
    }

    Some(ToolRunner::new(executor, context))
}

/// The access token for a connection, refreshed first if it is about to expire.
///
/// REFRESHED BEFORE USE, NOT AFTER FAILURE. Waiting for a 401 means every expiry costs a person one
/// visibly failed tool call, and a model that reads that failure may go and do something else. The
/// leeway lives in `ConnectionView::is_expiring` so a token cannot expire mid-flight.
///
/// A refusal that means the person revoked access disconnects rather than retrying: `invalid_grant`
/// is a decision somebody made, and retrying it forever turns a revoked connection into a permanent
/// error loop.
async fn live_token(
    state: &AgUiState,
    vault: &opengrok_store::Vault,
    chosen: &opengrok_core::connection::ConnectionView,
) -> Option<String> {
    let stored = state
        .auth
        .store
        .open_credential(vault, &chosen.id)
        .await
        .ok()
        .flatten();

    if !chosen.is_expiring(now_ms()) {
        return stored;
    }

    // Expiring. Without a provider or a refresh token there is nothing to do but use what we have
    // and let the provider say no — which is still better than refusing a call we might complete.
    let Some(config) = state.connectors.providers.get(&chosen.connector) else {
        return stored;
    };
    let Ok(Some(refresh_token)) = state
        .auth
        .store
        .open_credential(vault, &format!("{}_refresh", chosen.id))
        .await
    else {
        return stored;
    };

    match crate::connections::flow::refresh(&reqwest::Client::new(), config, &refresh_token).await {
        Ok(token) => {
            let at_ms = now_ms();
            let expires_at = token.expires_at_ms(at_ms);

            // Sealed before it is returned, so a crash between here and the next request does not
            // leave the old token in the database and the new one only in memory.
            if let Ok(sealed) = vault.seal(&chosen.id, &token.access_token) {
                let _ = state
                    .auth
                    .store
                    .put_secret(&chosen.id, &sealed, at_ms)
                    .await;
            }
            // Google omits the refresh token on a refresh, so the stored one is kept.
            if let Some(rotated) = token.refresh_token_to_store(Some(&refresh_token))
                && rotated != refresh_token
                && let Ok(sealed) = vault.seal(&format!("{}_refresh", chosen.id), &rotated)
            {
                let _ = state
                    .auth
                    .store
                    .put_secret(&format!("{}_refresh", chosen.id), &sealed, at_ms)
                    .await;
            }
            let _ = state
                .auth
                .store
                .touch_expiry(&chosen.id, expires_at, at_ms)
                .await;

            tracing::info!(
                connector = chosen.connector,
                "refreshed an expiring connection"
            );
            Some(token.access_token)
        }
        Err(error) => {
            if error.is_revoked() {
                tracing::warn!(
                    connector = chosen.connector,
                    "a connection was revoked at the provider; disconnecting it"
                );
                let _ = disconnect_revoked(state, &chosen.id).await;
                return None;
            }
            tracing::warn!(%error, connector = chosen.connector, "could not refresh; using what we have");
            stored
        }
    }
}

/// Record that a provider has revoked a connection.
///
/// Written down rather than merely logged: a person looking at their connections should see it is
/// gone, and the next run should not try again.
async fn disconnect_revoked(state: &AgUiState, id: &str) -> Result<(), opengrok_store::StoreError> {
    let (mut connection, seq) = state.auth.store.load_connection(id).await?;
    let at_ms = now_ms();
    let events = connection
        .decide(opengrok_core::connection::ConnectionCommand::Disconnect { at_ms })
        .unwrap_or_default();
    for event in &events {
        connection.apply(event);
    }
    state
        .auth
        .store
        .append_connection(
            id,
            seq,
            &events,
            &connection,
            &opengrok_store::CredentialUpdate::none(at_ms),
        )
        .await?;
    Ok(())
}

/// Open a session with every plugin server this coworker can both reach and be permitted to use.
///
/// TWO GATES, AND BOTH MATTER. A plugin must be in the coworker's ceiling — installing one on the
/// server is not the same as letting a coworker use it — and its credential must resolve, because
/// a connected tool without a token is a tool that fails at the moment of use rather than at the
/// moment of offer.
///
/// A server that will not connect is skipped with a warning rather than failing the run: the other
/// tools still work, and a turn that dies because one connector is down is worse than a turn that
/// proceeds without it.
async fn connect_plugins(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    policy: &opengrok_policy::Context,
) -> (
    BTreeMap<String, Arc<opengrok_tools::mcp::Session>>,
    Vec<opengrok_tools::mcp::McpTool>,
) {
    let mut sessions = BTreeMap::new();
    let mut tools = Vec::new();

    if state.plugins.is_empty() {
        return (sessions, tools);
    }

    // Every credential this coworker can use, keyed the way a plugin's placeholders name them:
    // `GMAIL_TOKEN` for the `gmail` connector.
    let candidates = state
        .auth
        .store
        .connections_for(account_id, coworker_id)
        .await
        .unwrap_or_default();

    let mut values: BTreeMap<String, String> = BTreeMap::new();
    if let Some(vault) = state.vault.as_ref() {
        for connector in candidates
            .iter()
            .map(|candidate| candidate.connector.clone())
            .collect::<std::collections::BTreeSet<_>>()
        {
            // The domain decides which of several connections wins — bot's own, then lent, then
            // global. That rule is pure and tested; this only asks it.
            let Some(chosen) =
                opengrok_core::connection::resolve(&candidates, &connector, coworker_id)
            else {
                continue;
            };
            if let Some(token) = live_token(state, vault, chosen).await {
                values.insert(format!("{}_TOKEN", connector.to_uppercase()), token);
            }
        }
    }

    for plugin in state.plugins.values() {
        let (endpoints, problems) = opengrok_tools::mcp::endpoints_for(plugin, &values);
        for problem in problems {
            tracing::debug!(%problem, plugin = plugin.manifest.name, "a plugin server is unavailable");
        }

        for endpoint in endpoints {
            let key = format!("{}.{}", endpoint.plugin, endpoint.server);

            let session = match opengrok_tools::mcp::Session::connect(endpoint).await {
                Ok(session) => Arc::new(session),
                Err(error) => {
                    tracing::warn!(%error, server = key, "could not reach a plugin server");
                    continue;
                }
            };

            match session.tools().await {
                Ok(offered) => {
                    // The ceiling gate. A tool the coworker may not run is not offered at all —
                    // being told about a tool that always refuses is a dead end a model retries.
                    for tool in offered {
                        let decision = opengrok_policy::decide(
                            account_id,
                            coworker_id,
                            opengrok_policy::Action::RunTool(&tool.qualified_name),
                            policy,
                        );
                        if decision.is_allowed() || decision.needs_approval() {
                            tools.push(tool);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, server = key, "a plugin server would not list its tools");
                    continue;
                }
            }

            sessions.insert(key, session);
        }
    }

    (sessions, tools)
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/ag-ui", post(run))
        .route("/ag-ui/runs/{run_id}", get(replay_run))
        .route("/ag-ui/runs/{run_id}/answer", post(answer_run))
        .route("/ag-ui/approvals", get(list_awaiting))
        .route("/coworkers", post(hire).get(list_coworkers))
        .route("/coworkers/{coworker_id}/approvals", post(set_approvals))
        .route(
            "/coworkers/{coworker_id}/keys",
            post(mint_bot_key).get(list_bot_keys),
        )
        .route(
            "/coworkers/{coworker_id}/keys/{jti}",
            axum::routing::delete(revoke_bot_key),
        )
        .with_state(state)
}

#[derive(Debug, Deserialize)]
pub struct HireRequest {
    pub name: String,
    /// A route through the gateway, never a key.
    #[serde(default)]
    pub model: Option<String>,
}

/// Hire a coworker, and optionally give it a computer.
///
/// The account comes from the bearer token, never from the body: a client that could name an
/// account could hire into somebody else's roster.
pub async fn hire(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<HireRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    let coworker_id = CoworkerId::new();
    let at_ms = now_ms();
    let model = request.model.unwrap_or_else(|| state.model.clone());

    let mut coworker = opengrok_core::coworker::Coworker::default();
    let mut events = match coworker.decide(CoworkerCommand::Hire {
        name: request.name.clone(),
        model: model.clone(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    for event in &events {
        coworker.apply(event);
    }

    // A computer, if asked for — via the shared helper so REST, gateway and seam-B create paths
    // behave identically. A failure leaves a boxless-but-hired coworker; the reason is in the reply.
    // 1 account = 1 computer: the account's first agent creates it, later agents share it.
    let provisioned =
        provision::ensure_computer_for(&state, &account_id, &coworker_id, &mut coworker, at_ms)
            .await;
    events.extend(provisioned.events);
    let computer_error = provisioned.error;

    let view = CoworkerView {
        id: coworker_id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: coworker.retired,
        updated_at_ms: at_ms,
    };

    if let Err(error) = state
        .auth
        .store
        .append_coworker(&coworker_id, &account_id, 0, &events, &view)
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    // Hiring grants the hirer access to what they hired. Written explicitly rather than implied by
    // ownership: "the owner may do anything" is the rule that has no seam to narrow later, and
    // coworker-to-coworker delegation will need one.
    //
    // The ceiling starts at the tools this server actually implements, not `All`: a coworker's
    // limits should be a list somebody can read, and `All` would silently include whatever is
    // added next.
    // The ceiling starts at the tools this server implements without any plugin. A plugin granted
    // to this coworker later must widen it — policy correctly refuses a tool nobody permitted, so
    // "install a plugin" and "let this coworker use it" stay two decisions rather than one.
    let tools =
        opengrok_policy::ToolSet::only(opengrok_tools::Executor::builtin_tool_names().to_vec());
    if let Err(error) = state
        .auth
        .store
        // Nothing needs approval by default. A person who wants a second pair of eyes on `shell`
        // sets it deliberately; defaulting to "approve everything" would make the prompt noise and
        // teach people to click yes.
        .grant_access(
            &account_id,
            &coworker_id,
            &tools,
            &tools,
            &opengrok_policy::ToolSet::None,
            at_ms,
        )
        .await
    {
        // A coworker nobody may use is worse than no coworker: fail the hire rather than leave one
        // that silently refuses everything.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the coworker was created but could not be granted: {error}"),
        )
            .into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": coworker_id.as_str(),
            "name": view.name,
            "model": view.model,
            "boxId": view.box_id.as_ref().map(|id| id.as_str()),
            "computerError": provision::error_json(&computer_error),
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ApprovalsRequest {
    /// Tools this coworker may only run with a human yes. An empty list means none.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Say which of a coworker's tools need a person to approve them — PLAN §4.5 layer 5.
///
/// Set by the person who holds the grant, on their own grant: approval is about what *they* may
/// have done without asking, so it is a property of the grant and not of the coworker.
pub async fn set_approvals(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    Json(request): Json<ApprovalsRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);

    // Only somebody who already holds a grant may change its approval list — otherwise this would
    // be a way to create a grant, which is a different permission entirely.
    let policy = match state.auth.store.policy_for(&account_id, &coworker_id).await {
        Ok(policy) => policy,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let decision = opengrok_policy::decide(
        &account_id,
        &coworker_id,
        opengrok_policy::Action::UseCoworker,
        &policy,
    );
    if let Some(reason) = decision.reason() {
        return (StatusCode::FORBIDDEN, reason.to_string()).into_response();
    }

    let (Some(grant), Some(ceiling)) = (policy.grant, policy.ceiling) else {
        return (StatusCode::FORBIDDEN, "no grant to change").into_response();
    };

    let needs_approval = if request.tools.is_empty() {
        opengrok_policy::ToolSet::None
    } else {
        opengrok_policy::ToolSet::only(request.tools.clone())
    };

    if let Err(error) = state
        .auth
        .store
        .grant_access(
            &account_id,
            &coworker_id,
            &grant.profile,
            &ceiling.tools,
            &needs_approval,
            now_ms(),
        )
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    Json(serde_json::json!({
        "coworkerId": coworker_id.as_str(),
        "needsApproval": request.tools,
    }))
    .into_response()
}

/// The roster, newest first — the order the client sorts by.
pub async fn list_coworkers(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    match state.auth.store.coworkers_for(&account_id).await {
        // An ARRAY, always. An empty roster is a valid answer and must not become null or an
        // object — the desktop client throws on a malformed array reply (RUNBOOK §4).
        Ok(coworkers) => Json(coworkers).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// Whose account this is, from the bearer token. Never from the body.
pub(crate) fn account_from_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Option<opengrok_core::id::AccountId> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))?;
    let claims = state.auth.minter.verify_access(token).ok()?;
    Some(opengrok_core::id::AccountId::from_stored(claims.sub))
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Mint a durable key that lets a client Bot run AS this coworker.
///
/// Access tokens live an hour; a Bot registered in a client's vault with a static header dies
/// hourly. This key is signed like everything else but LONG-lived, because its real lifecycle
/// control is the revocable row — showing the token once at mint is the only time it exists in
/// a reply, the same bargain every credential here makes.
async fn mint_bot_key(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    // Owner-and-permitted, or a 404 that does not confirm the coworker exists.
    let owns = state
        .auth
        .store
        .coworkers_for(&account_id)
        .await
        .map(|roster| {
            roster
                .iter()
                .any(|view| view.id == coworker_id && !view.retired)
        })
        .unwrap_or(false);
    if !owns {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    }

    let jti = format!("bk_{}", uuid::Uuid::now_v7());
    let at_ms = now_ms();
    let claims = BotKeyClaims {
        purpose: "bot-key".to_string(),
        sub: account_id.as_str().to_string(),
        coworker: coworker_id.as_str().to_string(),
        jti: jti.clone(),
        // Ten years: the row is the real lifecycle; the exp only bounds a leaked key whose
        // revocation row was somehow lost with it.
        exp: at_ms / 1_000 + 10 * 365 * 24 * 60 * 60,
    };
    let Ok(token) = state.auth.minter.mint_claims(&claims) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not mint").into_response();
    };
    if let Err(error) = state
        .auth
        .store
        .insert_bot_key(&jti, &account_id, &coworker_id, "bot key", at_ms)
        .await
    {
        tracing::error!(%error, "could not record a bot key");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not record the key",
        )
            .into_response();
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "jti": jti,
            "coworkerId": coworker_id.as_str(),
            // Shown exactly once. The row keeps the jti; the token is the caller's to keep.
            "key": token,
        })),
    )
        .into_response()
}

async fn list_bot_keys(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match state
        .auth
        .store
        .bot_keys_for(&account_id, &coworker_id)
        .await
    {
        Ok(keys) => Json(keys).into_response(),
        Err(error) => {
            tracing::error!(%error, "could not list bot keys");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

async fn revoke_bot_key(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((_coworker_id, jti)): axum::extract::Path<(String, String)>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    match state.auth.store.revoke_bot_key(&account_id, &jti).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such key").into_response(),
        Err(error) => {
            tracing::error!(%error, "could not revoke a bot key");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// What a bot key says about itself. The `use` claim is the discriminator the minter's own
/// documentation demands: without it, a stolen access token would verify here too.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct BotKeyClaims {
    #[serde(rename = "use")]
    pub purpose: String,
    pub sub: String,
    pub coworker: String,
    pub jti: String,
    pub exp: i64,
}

/// Who is calling, and — when the credential is a bot key — AS which coworker.
///
/// Three outcomes, and the middle one matters most: `Err(response)` is a bot key that VERIFIES
/// but is revoked or unknown. That must refuse rather than fall through to anonymous, or a
/// revoked Bot silently keeps talking on the deployment's model and nobody notices the
/// revocation did nothing.
pub(crate) async fn principal_from_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Result<Option<(opengrok_core::id::AccountId, Option<CoworkerId>)>, Response> {
    let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Ok(None);
    };
    if let Ok(claims) = state.auth.minter.verify_access(token) {
        return Ok(Some((
            opengrok_core::id::AccountId::from_stored(claims.sub),
            None,
        )));
    }
    if let Ok(claims) = state.auth.minter.verify_claims::<BotKeyClaims>(token) {
        if claims.purpose != "bot-key" {
            return Ok(None);
        }
        let live = state
            .auth
            .store
            .bot_key_live(&claims.jti)
            .await
            .unwrap_or(false);
        if !live {
            return Err((StatusCode::UNAUTHORIZED, "this bot key has been revoked").into_response());
        }
        return Ok(Some((
            opengrok_core::id::AccountId::from_stored(claims.sub),
            Some(CoworkerId::from_stored(claims.coworker)),
        )));
    }
    Ok(None)
}

/// Start a run and stream its events.
pub async fn run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(input): Json<RunAgentInput>,
) -> Response {
    // Who is asking. Established first, because the permission check, the run's ownership and the
    // model it thinks with all depend on it.
    //
    // Layer 1, every turn: may this principal talk to this coworker at all? An anonymous run gets
    // no tools rather than being refused outright — the AG-UI endpoint is also how a client with
    // no coworker just talks to a model.
    let (account_id, key_coworker) = match principal_from_bearer(&state, &headers).await {
        Ok(Some((account, coworker))) => (Some(account), coworker),
        Ok(None) => (None, None),
        // A revoked bot key refuses; downgrading to anonymous would make revocation invisible.
        Err(refusal) => return refusal,
    };
    // A BOT KEY NAMES THE COWORKER. barok-works registers a Bot with an endpoint and a header —
    // it has no forwardedProps to send — so the key itself carries which coworker the Bot IS.
    // An explicit forwardedProps still wins: a client that says what it means is believed.
    let run_coworker = coworker_id_from(&input).or(key_coworker);

    // The deployment's model is the default, not the answer: a named coworker overrides it below.
    let mut model = state.model.clone();

    if let (Some(account_id), Some(coworker_id)) = (&account_id, run_coworker.clone()) {
        let policy = state
            .auth
            .store
            .policy_for(account_id, &coworker_id)
            .await
            .unwrap_or_default();
        let decision = opengrok_policy::decide(
            account_id,
            &coworker_id,
            opengrok_policy::Action::UseCoworker,
            &policy,
        );
        if let Some(reason) = decision.reason() {
            // A refusal the client can read, not a dead socket.
            return (StatusCode::FORBIDDEN, reason.to_string()).into_response();
        }

        // WHICH MODEL A COWORKER THINKS WITH IS THE COWORKER'S, NOT THE DEPLOYMENT'S. Hiring takes
        // a model and stores it, and the roster reports it; a run that read past it left every one
        // of those answers describing a choice that never happened — a coworker hired on one model
        // silently answered on another, and the only visible symptom was the bill.
        //
        // AFTER the policy check and only for a named principal. An anonymous caller may still talk
        // to the deployment's model, but must not learn a coworker's configuration by noticing
        // which model replies.
        //
        // A coworker that cannot be loaded keeps the default rather than failing the run: the model
        // is how the turn is answered, not whether it is allowed, and that question was just asked.
        if let Ok((coworker, _)) = state.auth.store.load_coworker(&coworker_id).await {
            model = coworker.model.clone();
        }
    }

    let request = ModelRequest {
        model,
        system: None,
        messages: to_chat_messages(&input),
        tools: Vec::new(),
    };

    let tools = match &account_id {
        Some(account_id) => match &run_coworker {
            Some(coworker_id) => tools_for_coworker(&state, account_id, coworker_id).await,
            None => None,
        },
        // No bearer, no identity, and therefore no tools: tools always run as somebody.
        None => None,
    };

    // The journal writes each round to Postgres before the next model call, and stamps the run's
    // owner so only they can read it back. A run that cannot be recorded fails inside the loop
    // rather than being streamed (CLAUDE.md #5).
    let journal = StoreJournal {
        state: state.clone(),
        thread_id: input.thread_id.clone(),
        account_id: account_id.clone(),
        coworker_id: run_coworker,
    };

    // Hold the run while we serve it, so a recovery sweep does not mistake a slow model call for
    // an abandoned run. Released when this drops — including when the process dies, which is
    // exactly the case the lease exists for.
    let _lease = crate::recovery::Lease::new(crate::recovery::hold(
        state.clone(),
        RunId::from_stored(input.run_id.clone()),
    ));

    let events = run_conversation(
        state.door.as_ref(),
        tools.as_ref(),
        &journal,
        request,
        &input.thread_id,
        &input.run_id,
        now_ms(),
    )
    .await;

    sse(stream::iter(
        events.into_iter().map(Ok::<_, std::io::Error>),
    ))
}

/// The event store, as the harness's journal.
///
/// The harness owns *when* to write (before the next model call); this owns *where*. Keeping them
/// apart is what lets the ordering rule be tested without a database and still be enforced against
/// one.
pub struct StoreJournal {
    pub state: AgUiState,
    pub thread_id: String,
    /// Whose run this is, so it can be read back by them and by nobody else.
    pub account_id: Option<opengrok_core::id::AccountId>,
    /// Which coworker is doing the work. Recorded on the run because a run that is answered days
    /// later has to know whose tools to continue with, and the request that started it is long gone.
    pub coworker_id: Option<CoworkerId>,
}

#[async_trait::async_trait]
impl opengrok_harness::RunJournal for StoreJournal {
    async fn record(
        &self,
        run_id: &str,
        events: &[Event],
    ) -> Result<(), opengrok_harness::JournalError> {
        append_events(
            &self.state,
            run_id,
            &self.thread_id,
            self.account_id.as_ref(),
            self.coworker_id.as_ref(),
            events,
        )
        .await
        .map_err(|error| opengrok_harness::JournalError::Unwritable(error.to_string()))
    }
}

/// Append a batch of a run's events to the log, starting the run if this is its first batch.
async fn append_events(
    state: &AgUiState,
    run_id: &str,
    thread_id: &str,
    account_id: Option<&opengrok_core::id::AccountId>,
    coworker_id: Option<&CoworkerId>,
    events: &[Event],
) -> Result<(), opengrok_store::StoreError> {
    if events.is_empty() {
        return Ok(());
    }
    let run_id = RunId::from_stored(run_id.to_string());
    let at_ms = now_ms();

    let (mut run, seq) = state.auth.store.load_run(&run_id).await?;
    let mut to_append = Vec::new();

    if !run.started {
        let started = run
            .decide(RunCommand::Start {
                thread_id: thread_id.to_string(),
                coworker_id: coworker_id.cloned(),
                at_ms,
            })
            .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
        for event in &started {
            run.apply(event);
        }
        to_append.extend(started);
    }

    for event in events {
        let payload = serde_json::to_value(event)
            .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
        // The aggregate refuses a frame after an ending; that is a rule, not a hiccup.
        let Ok(decided) = run.decide(RunCommand::Emit { payload, at_ms }) else {
            break;
        };
        for decided_event in &decided {
            run.apply(decided_event);
        }
        to_append.extend(decided);

        // A suspension carries which call is waiting, so a person can answer *that* call later.
        // Read from the event the projection emitted, because the harness is the only thing that
        // knows the run stopped.
        if event.event_type == opengrok_wire::agui::EventType::Custom
            && event.extra.get("name").and_then(|name| name.as_str())
                == Some("run-awaiting-approval")
        {
            let call_id = event
                .extra
                .get("callId")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            if !call_id.is_empty()
                && let Ok(suspended) = run.decide(RunCommand::Suspend {
                    call_id,
                    tool: event
                        .extra
                        .get("tool")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: event
                        .extra
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    at_ms,
                })
            {
                for suspended_event in &suspended {
                    run.apply(suspended_event);
                }
                to_append.extend(suspended);
            }
        }

        // The run's own ending, recorded once, from the event that carries it.
        let closing = match event.event_type {
            opengrok_wire::agui::EventType::RunFinished => {
                Some(run.decide(RunCommand::Finish { at_ms }))
            }
            opengrok_wire::agui::EventType::RunError => Some(
                run.decide(RunCommand::Fail {
                    reason: event
                        .extra
                        .get("message")
                        .and_then(|message| message.as_str())
                        .unwrap_or("the run failed")
                        .to_string(),
                    at_ms,
                }),
            ),
            _ => None,
        };
        if let Some(Ok(closing)) = closing {
            for closing_event in &closing {
                run.apply(closing_event);
            }
            to_append.extend(closing);
        }
    }

    let view = RunView {
        id: run_id.clone(),
        thread_id: thread_id.to_string(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    state
        .auth
        .store
        .append_run(&run_id, seq, &to_append, &view, account_id)
        .await?;
    Ok(())
}

/// Replay a run from the log.
///
/// THIS IS THE PROMISE, MADE CHECKABLE. Close the tab mid-run, come back, ask here: every event
/// the run produced is returned, in order, without asking a model anything a second time.
pub async fn replay_run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    let run_id = RunId::from_stored(run_id);

    // LAYER 4 (`docs/PLAN.md` §4.5): a run holds a whole conversation, so without this check a run
    // id is a password — and run ids travel in client URLs and logs. `NOT_FOUND` rather than
    // `FORBIDDEN` for both "no such run" and "not yours", so probing ids reveals nothing about
    // which runs exist.
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    };
    match state.auth.store.run_owned_by(&run_id, &account_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }

    let (run, _) = match state.auth.store.load_run(&run_id).await {
        Ok(loaded) => loaded,
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    };

    if !run.started {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }

    Json(serde_json::json!({
        "runId": run_id.as_str(),
        "threadId": run.thread_id,
        "status": match run.status {
            RunStatus::Running => "running",
            RunStatus::AwaitingApproval => "awaiting-approval",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
        },
        "failure": run.failure,
        "pending": run.pending,
        "events": run.emitted,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct AnswerRequest {
    /// Which call is being answered. Required: "approve the run" is ambiguous the moment a turn
    /// asks for two things.
    pub call_id: String,
    pub approved: bool,
}

/// Answer a suspended run — PLAN §4.5 layer 5, the other half.
///
/// EXACTLY ONCE, AND THE AGGREGATE IS WHAT GUARANTEES IT. A retried request, a double-clicked
/// button and two devices answering together all reach here; the aggregate refuses every answer
/// after the first, and the store's sequence check makes the concurrent case safe — the loser gets
/// a conflict and re-reads to find the call already answered.
pub async fn answer_run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
    Json(request): Json<AnswerRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let run_id = RunId::from_stored(run_id);

    // Only the run's owner may answer it — the same rule as replay, for the same reason.
    match state.auth.store.run_owned_by(&run_id, &account_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }

    let (mut run, seq) = match state.auth.store.load_run(&run_id).await {
        Ok(loaded) => loaded,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    // Captured BEFORE the answer, because answering clears it — and the continuation needs to know
    // exactly which command was approved rather than asking the model to propose one again.
    let pending = run.pending.clone();
    let resumed_seq = run.emitted.len() as u32;

    let at_ms = now_ms();
    let events = match run.decide(RunCommand::Answer {
        call_id: request.call_id.clone(),
        approved: request.approved,
        by: account_id.to_string(),
        at_ms,
    }) {
        Ok(events) => events,
        // A second answer is not an error the caller needs to fix; it is the same answer arriving
        // twice. Reporting the settled state is what makes a retry safe to send.
        Err(opengrok_core::run::RunError::AlreadyAnswered) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "runId": run_id.as_str(),
                    "callId": request.call_id,
                    "alreadyAnswered": true,
                })),
            )
                .into_response();
        }
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };

    for event in &events {
        run.apply(event);
    }

    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    if let Err(error) = state
        .auth
        .store
        .append_run(&run_id, seq, &events, &view, Some(&account_id))
        .await
    {
        // A conflict here means somebody answered between our read and our write. The answer that
        // won is as good as ours, so this is not a failure to report as one.
        if matches!(error, opengrok_store::StoreError::Conflict) {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "runId": run_id.as_str(),
                    "callId": request.call_id,
                    "alreadyAnswered": true,
                })),
            )
                .into_response();
        }
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    // The answer is durable; now carry the run on. In the background, because a model call can
    // take minutes and the person clicking "approve" should not hold a socket open for it.
    if request.approved
        && let Some(pending) = pending
    {
        let state = state.clone();
        let account_id = account_id.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move {
            continue_run(state, account_id, run_id, pending, resumed_seq).await;
        });
    }

    Json(serde_json::json!({
        "runId": run_id.as_str(),
        "callId": request.call_id,
        "approved": request.approved,
        "alreadyAnswered": false,
        "continuing": request.approved,
    }))
    .into_response()
}

/// Rebuild the conversation from what a run already emitted.
///
/// The log is the only record of a run that outlives the request that started it, so a resumed run
/// has to read its own history rather than being handed one. Text the assistant said and results
/// its tools returned are what the model needs to carry on; the framing events are not.
fn conversation_from(run: &opengrok_core::run::Run) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut assistant = String::new();

    for payload in &run.emitted {
        let Some(kind) = payload.get("type").and_then(|value| value.as_str()) else {
            continue;
        };
        match kind {
            "TEXT_MESSAGE_CONTENT" => {
                if let Some(delta) = payload.get("delta").and_then(|value| value.as_str()) {
                    assistant.push_str(delta);
                }
            }
            // An empty message is skipped rather than pushed: a provider that rejects empty
            // content would fail the whole resumed turn over nothing.
            "TEXT_MESSAGE_END" if !assistant.is_empty() => {
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: std::mem::take(&mut assistant),
                });
            }
            "TOOL_CALL_RESULT" => {
                if let Some(content) = payload.get("content").and_then(|value| value.as_str()) {
                    messages.push(ChatMessage {
                        role: "user".to_string(),
                        content: format!("[tool result] {content}"),
                    });
                }
            }
            _ => {}
        }
    }

    messages
}

/// Carry an answered run on, without waiting for anybody to ask again.
///
/// THE SERVER PICKS IT BACK UP. A run that only continues when the next request happens to arrive
/// is a run that depends on a client being there — which is the thing this project exists to stop
/// (CLAUDE.md #5). The answer is already durable when this starts, so a crash here leaves a run
/// that is answered and unfinished, which `interrupted_runs` can find and this can be told to do
/// again.
async fn continue_run(
    state: AgUiState,
    account_id: opengrok_core::id::AccountId,
    run_id: RunId,
    approved: opengrok_core::run::PendingApproval,
    resumed_seq: u32,
) {
    let Ok((run, _)) = state.auth.store.load_run(&run_id).await else {
        tracing::warn!(run = %run_id, "could not load an answered run to continue it");
        return;
    };

    // The coworker whose tools these are. Without it there is nothing to continue *as*.
    let Some(coworker_id) = run.coworker_id.clone() else {
        tracing::warn!(run = %run_id, "an answered run has no coworker, so it cannot continue");
        return;
    };
    let Some(computer) = state.computer.clone() else {
        return;
    };
    let Ok((coworker, _)) = state.auth.store.load_coworker(&coworker_id).await else {
        return;
    };
    let Ok(policy) = state.auth.store.policy_for(&account_id, &coworker_id).await else {
        return;
    };

    // The approved call, and only it: the executor carries the id the person actually answered.
    let runner = ToolRunner::new(
        opengrok_tools::Executor::with_policy(computer, policy)
            .with_approved([approved.call_id.clone()]),
        opengrok_tools::ToolContext::from_coworker(account_id.clone(), coworker_id, &coworker),
    );

    let journal = StoreJournal {
        state: state.clone(),
        thread_id: run.thread_id.clone(),
        account_id: Some(account_id),
        coworker_id: run.coworker_id.clone(),
    };

    let request = ModelRequest {
        // The coworker's model, not the deployment's — the same rule `run()` enforces, and the
        // same silent substitution if it slips.
        model: coworker.model.clone(),
        system: None,
        messages: conversation_from(&run),
        tools: Vec::new(),
    };

    // The run keeps its id, so everything the resumption emits lands in the same log and a client
    // replaying later sees one continuous run rather than two halves.
    let events = opengrok_harness::resume_conversation(
        state.door.as_ref(),
        &runner,
        &journal,
        request,
        opengrok_harness::RunContext::new(&run.thread_id, run_id.as_str(), now_ms()),
        opengrok_harness::Resumption {
            approved: opengrok_tools::ToolCall {
                id: approved.call_id,
                name: approved.tool,
                arguments: approved.arguments,
            },
            message_seq: resumed_seq,
        },
    )
    .await;

    tracing::info!(run = %run_id, events = events.len(), "continued an answered run");
}

/// Runs waiting on this person.
///
/// A suspended run nobody can find is a run nobody will answer, which is the same as a lost one.
pub async fn list_awaiting(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    match state.auth.store.awaiting_approval(&account_id).await {
        Ok(runs) => {
            let mut waiting = Vec::new();
            for run_id in runs {
                if let Ok((run, _)) = state.auth.store.load_run(&run_id).await
                    && let Some(pending) = run.pending
                {
                    waiting.push(serde_json::json!({
                        "runId": run_id.as_str(),
                        "threadId": run.thread_id,
                        "callId": pending.call_id,
                        "tool": pending.tool,
                        // What is actually being approved. A person asked to approve "shell"
                        // without seeing the command is being asked to approve nothing.
                        "arguments": pending.arguments,
                    }));
                }
            }
            // An ARRAY, always: an empty queue is a valid answer.
            Json(waiting).into_response()
        }
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// AG-UI messages to the model's vocabulary.
///
/// Roles the model door does not understand are dropped rather than passed through: a provider
/// that rejects an unknown role fails the whole turn, and AG-UI carries roles (`developer`) that
/// have no place in a chat completion.
pub fn to_chat_messages(input: &RunAgentInput) -> Vec<ChatMessage> {
    input
        .messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "user" | "assistant" | "system"))
        .filter_map(|message| {
            message.content.as_ref().map(|content| ChatMessage {
                role: message.role.clone(),
                content: content.clone(),
            })
        })
        .collect()
}

/// Wrap an event stream in the SSE response openbot expects.
fn sse<S, E>(events: S) -> Response
where
    S: Stream<Item = Result<Event, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    use futures::StreamExt;

    let body = events.map(|event| {
        event.map(|event| {
            // An event that will not serialise is dropped rather than allowed to panic mid-run;
            // `to_sse_frame` returns None and the stream continues.
            event.to_sse_frame().unwrap_or_default()
        })
    });

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            // Without this a proxy may buffer the whole run and deliver it at the end, which looks
            // exactly like a server that never streamed.
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use opengrok_harness::MockDoor;
    use opengrok_wire::agui::{EventType, Message};
    use serde_json::json;

    fn input(messages: Vec<Message>) -> RunAgentInput {
        RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            parent_run_id: None,
            state: json!(null),
            messages,
            tools: json!(null),
            context: json!(null),
            forwarded_props: json!(null),
            extra: Default::default(),
        }
    }

    fn message(role: &str, content: Option<&str>) -> Message {
        Message {
            id: "m1".to_string(),
            role: role.to_string(),
            content: content.map(str::to_string),
            name: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn chat_roles_the_model_understands_are_kept() {
        let messages = to_chat_messages(&input(vec![
            message("system", Some("be brief")),
            message("user", Some("hello")),
            message("assistant", Some("hi")),
        ]));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].content, "hello");
    }

    /// AG-UI carries roles a chat completion has no place for. Passing one through fails the whole
    /// turn on providers that reject unknown roles.
    #[test]
    fn roles_the_model_does_not_understand_are_dropped() {
        let messages = to_chat_messages(&input(vec![
            message("developer", Some("internal")),
            message("tool", Some("result")),
            message("user", Some("hello")),
        ]));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    /// A message with no content is a placeholder the client is still filling in.
    #[test]
    fn a_message_without_content_is_skipped() {
        let messages = to_chat_messages(&input(vec![message("user", None)]));
        assert!(messages.is_empty());
    }

    /// End to end through the mock door: no provider, no key, and still a complete run.
    #[tokio::test]
    async fn a_run_through_the_mock_door_is_well_formed() {
        let door = MockDoor::echoing();
        let events = opengrok_harness::run_conversation(
            &door,
            None,
            &opengrok_harness::MemoryJournal::new(),
            ModelRequest {
                model: "mock".to_string(),
                system: None,
                messages: to_chat_messages(&input(vec![message("user", Some("ping"))])),
                tools: Vec::new(),
            },
            "t1",
            "r1",
            1,
        )
        .await;

        assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
        assert_eq!(events.first().unwrap().extra.get("threadId").unwrap(), "t1");

        for event in &events {
            let frame = event.to_sse_frame().unwrap();
            assert!(frame.starts_with("data: "));
            assert_eq!(frame.matches("\n\n").count(), 1, "{frame:?}");
        }
    }
}
