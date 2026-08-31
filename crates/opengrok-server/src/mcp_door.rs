//! The MCP door — an MCP client (Claude Code first) borrows a coworker's toolbox.
//!
//! This is a FRONT DOOR onto the same executor every run uses, not a second tool path. The
//! bearer names the coworker (a slice-10 bot key), `tools_for_coworker` builds the same
//! policy-and-auto-review-wired runner a run would get, and every call goes through
//! `Executor::execute` — identity overwrite, the primary gate, the judge, the audit. The door
//! adds transport, never authority.
//!
//! Two shapes are deliberate:
//! - An `ask` FAILS CLOSED here. There is no run to suspend and the MCP client cannot render
//!   our consent cards, so a call that needs a person comes back as an error naming what to do
//!   (approve it in the app, call again). Raising a durable card from a runless call is a
//!   follow-up slice, not a silent allow today.
//! - A coworker with no computer gets an EMPTY tool list, not a failed handshake. An empty
//!   toolbox is a valid state the client renders; a broken `initialize` is a support ticket.

use std::sync::Arc;

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{ErrorData as McpError, ServerHandler};

use crate::AgUiState;
use crate::agui::routes::{principal_from_bearer, tools_for_coworker};
use opengrok_core::CoworkerId;
use opengrok_core::id::AccountId;
use opengrok_harness::tools::ToolRunner;
use opengrok_tools::ToolCall;

/// The tower service the router nests at `/mcp`.
pub fn service(state: AgUiState) -> StreamableHttpService<McpDoor, LocalSessionManager> {
    StreamableHttpService::new(
        move || {
            Ok(McpDoor {
                state: state.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        // The default host allowlist admits only loopback, and this door is used from the LAN
        // (the desktop and Claude Code reach the server on a non-loopback address by design).
        // The DNS-rebinding attack the allowlist guards against needs a browser that cannot
        // set headers — and every request here must carry a bearer, so the key is the guard.
        //
        // Stateless on purpose: every request re-derives its principal from the bearer, so a
        // session would only cache what must not be cached (CLAUDE.md #6 — policy is enforced
        // every action, not once at the start). `json_response` keeps a plain call a plain
        // reply; the transport still falls back to SSE when a stream is genuinely needed.
        {
            let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
            config.legacy_session_mode = false;
            config.json_response = true;
            config
        },
    )
}

pub struct McpDoor {
    state: AgUiState,
}

impl McpDoor {
    /// Who is calling, from the Authorization header the transport preserved in the context.
    ///
    /// Only a bot key opens the door: it NAMES the coworker whose toolbox this is. An account
    /// access token is a person, not a coworker, and is told how to mint the right credential
    /// rather than being guessed a coworker for.
    async fn coworker_for(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(AccountId, CoworkerId), McpError> {
        let Some(parts) = context.extensions.get::<axum::http::request::Parts>() else {
            return Err(McpError::internal_error(
                "the transport did not preserve the request headers",
                None,
            ));
        };
        let principal = principal_from_bearer(&self.state, &parts.headers)
            .await
            .map_err(|_| McpError::invalid_request("this bot key has been revoked", None))?;
        match principal {
            Some((account, Some(coworker))) => Ok((account, coworker)),
            Some((_, None)) => Err(McpError::invalid_request(
                "this token names a person, not a coworker — mint a bot key \
                 (POST /coworkers/{id}/keys) and use that as the bearer",
                None,
            )),
            None => Err(McpError::invalid_request(
                "missing or unrecognised bearer — use a coworker's bot key",
                None,
            )),
        }
    }

    async fn runner_for(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<Option<ToolRunner>, McpError> {
        let (account, coworker) = self.coworker_for(context).await?;
        Ok(tools_for_coworker(&self.state, &account, &coworker, &[], &[]).await)
    }
}

impl ServerHandler for McpDoor {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Tools run on the coworker this key names, on that coworker's own computer, \
                 under the account's policy. A call that needs a person's approval returns an \
                 error saying so — grant it in the OpenGrok app or console, then call again.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // ttlMs 0 + private, required by protocol 2026-07-28 (SEP-2549) and chosen, not
        // defaulted: this list is policy-filtered per bot key, and policy is enforced on every
        // action — a cached listing must not outlive a policy change or leak across keys.
        let uncacheable = |tools| {
            ListToolsResult::with_all_items(tools)
                .with_ttl_ms(0)
                .with_cache_scope(CacheScope::Private)
        };
        let Some(runner) = self.runner_for(&context).await? else {
            return Ok(uncacheable(Vec::new()));
        };
        let tools = runner
            .tool_schemas()
            .into_iter()
            .filter_map(|schema| {
                // Our schemas are the OpenAI function shape the harness advertises; the door
                // re-expresses the same offering, never a different one.
                let function = schema.get("function")?;
                let name = function.get("name")?.as_str()?.to_string();
                let description = function
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string();
                let parameters: JsonObject = match function.get("parameters") {
                    Some(serde_json::Value::Object(map)) => map.clone(),
                    // A tool with no schema is still callable; an invented schema would be a
                    // lie the client validates against.
                    _ => JsonObject::new(),
                };
                Some(Tool::new(name, description, parameters))
            })
            .collect();
        Ok(uncacheable(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(runner) = self.runner_for(&context).await? else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "this coworker has no computer, so it has no tools to run",
            )])
            .into());
        };
        let call = ToolCall {
            id: format!("mcp_{}", uuid::Uuid::now_v7().simple()),
            name: request.name.to_string(),
            arguments: request
                .arguments
                .map(serde_json::Value::Object)
                .unwrap_or_else(|| serde_json::json!({})),
        };
        let result = runner.run_one(&call).await;
        tracing::info!(
            tool = %call.name,
            ok = result.ok,
            awaiting = result.awaiting_approval,
            "mcp door call"
        );
        let response = if result.ok {
            CallToolResult::success(vec![ContentBlock::text(result.content)])
        } else if result.awaiting_approval {
            CallToolResult::error(vec![ContentBlock::text(format!(
                "{} — approve it in the OpenGrok app or console, then call the tool again.",
                result.content
            ))])
        } else {
            // A refusal is content the model can reason about, exactly as in a run.
            CallToolResult::error(vec![ContentBlock::text(result.content)])
        };
        Ok(response.into())
    }
}
