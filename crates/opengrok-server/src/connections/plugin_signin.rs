//! Signing in to an installed plugin's own MCP server (#364): the routes an app calls, and the
//! callback's half. The protocol is `opengrok_integrations::mcp_oauth`; this is where it meets the
//! person's session, their install, the signed state and the browser.
//!
//! A sign-in is an unfinished sign-in until the provider sends the person back (`attempts`), so an
//! app shows it as Needs Auth meanwhile and Reopen resumes the same one.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use opengrok_core::id::AccountId;
use opengrok_integrations::mcp_oauth::{self, McpAuthError};
use opengrok_integrations::{attempts, installed};
use serde::Deserialize;

use super::flow::sign_state;
use super::oauth::StateClaims;
use super::routes::refused;
use crate::agui::routes::{AgUiState, account_from_bearer, now_ms};

#[derive(Debug, Deserialize)]
pub struct PluginAuthorizeQuery {
    /// `json` answers `{url, expiresAtMs}` in place of the redirect, as `/connections/.../authorize`
    /// does for an app.
    #[serde(default)]
    pub format: Option<String>,
    /// The MCP account a reconnect refreshes; absent, the sign-in adds one.
    #[serde(default)]
    pub connection_id: Option<String>,
}

/// The install, and the MCP server its `connector` names. 404 for an install or a service that is
/// not this person's, 422 for a service with no hosted server to sign in to.
async fn server_of(
    state: &AgUiState,
    account: &AccountId,
    name: &str,
    connector: &str,
) -> Result<String, Response> {
    let installs = installed::list(&state.auth.store, account)
        .await
        .map_err(|error| refused(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()))?;
    let Some(install) = installs.into_iter().find(|install| install.name == name) else {
        return Err(refused(StatusCode::NOT_FOUND, "no such installed plugin"));
    };
    if !install.bundle.connectors().iter().any(|c| c == connector) {
        return Err(refused(
            StatusCode::NOT_FOUND,
            "no such installed connector",
        ));
    }
    mcp_oauth::server_url(&install.bundle, connector).ok_or_else(|| {
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "this service has no server of its own to sign in to; paste a token instead",
        )
    })
}

/// `GET /plugins/installations/{name}/connectors/{connector}/sign-in`: how an account of this
/// service is added, `{"method": "oauth"}` when its server offers a sign-in of its own and
/// `{"method": "token"}` when a token is pasted. Remembered a while (`offers_sign_in`).
pub async fn method(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((name, connector)): Path<(String, String)>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return refused(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let method = match server_of(&state, &account, &name, &connector).await {
        Ok(url) if mcp_oauth::offers_sign_in(&mcp_oauth::http(), &url).await => "oauth",
        Ok(_) => "token",
        Err(response) if response.status() == StatusCode::UNPROCESSABLE_ENTITY => "token",
        Err(response) => return response,
    };
    Json(serde_json::json!({ "method": method })).into_response()
}

/// `GET /plugins/installations/{name}/connectors/{connector}/authorize`: the provider's consent
/// page for a new account of this service, or with `connection_id` for that MCP account again.
/// A server with no sign-in of its own is 422 `{error, code: "no_sign_in"}`; the app offers the
/// token field then.
pub async fn authorize(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((name, connector)): Path<(String, String)>,
    Query(query): Query<PluginAuthorizeQuery>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return refused(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let mcp_url = match server_of(&state, &account, &name, &connector).await {
        Ok(url) => url,
        Err(response) => return response,
    };
    let target = match &query.connection_id {
        None => None,
        Some(id) => {
            match mcp_oauth::account_label(&state.auth.store, &account, &name, &connector, id).await
            {
                Ok(Some(label)) => Some((id.clone(), label)),
                Ok(None) => return refused(StatusCode::NOT_FOUND, "no such account"),
                Err(error) => return refused(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
            }
        }
    };
    link(
        &state,
        account,
        &name,
        &connector,
        &mcp_url,
        Begin::New(target),
        query.format.as_deref(),
    )
    .await
}

/// What a link begins: a new sign-in (with the account a reconnect refreshes), or a waiting one
/// reopened.
pub enum Begin {
    New(Option<(String, String)>),
    Reopen(String),
}

/// Discover, register, start (or renew) the sign-in, sign its state, and hand out the page.
pub async fn link(
    state: &AgUiState,
    account: AccountId,
    plugin: &str,
    connector: &str,
    mcp_url: &str,
    begin: Begin,
    format: Option<&str>,
) -> Response {
    let Some(vault) = state.vault.as_deref() else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential vault is unavailable",
        );
    };
    let http = mcp_oauth::http();
    let metadata = match mcp_oauth::discover(&http, mcp_oauth::PUBLIC, mcp_url).await {
        Ok(metadata) => metadata,
        Err(McpAuthError::NoSignIn(why)) => {
            let body = serde_json::json!({"error": why, "code": "no_sign_in"});
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
        }
        Err(error) => return refused(StatusCode::BAD_GATEWAY, &error.to_string()),
    };
    let redirect_uri = state.connectors.redirect_uri.clone();
    let at_ms = now_ms();
    let client = match mcp_oauth::client(
        &http,
        mcp_oauth::PUBLIC,
        &state.auth.store,
        vault,
        &metadata,
        &redirect_uri,
        at_ms,
    )
    .await
    {
        Ok(client) => client,
        Err(McpAuthError::NoSignIn(why)) => {
            let body = serde_json::json!({"error": why, "code": "no_sign_in"});
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
        }
        Err(error) => return refused(StatusCode::BAD_GATEWAY, &error.to_string()),
    };
    let pkce = mcp_oauth::pkce();
    let store = &state.auth.store;
    let attempt = match begin {
        Begin::New(target) => {
            let target = target
                .as_ref()
                .map(|(id, label)| (id.as_str(), label.as_str()));
            let started = attempts::start_mcp(
                store,
                &account,
                plugin,
                connector,
                target,
                &pkce.verifier,
                &metadata,
                &client.client_id,
                at_ms,
            );
            match started.await {
                Ok(attempt) => attempt.id,
                Err(error) => return refused(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
            }
        }
        Begin::Reopen(id) => {
            let renewed = attempts::renew_mcp(
                store,
                &account,
                &id,
                &pkce.verifier,
                &metadata,
                &client.client_id,
                at_ms,
            );
            match renewed.await {
                Ok(true) => id,
                Ok(false) => return refused(StatusCode::NOT_FOUND, "no such sign-in"),
                Err(error) => return refused(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
            }
        }
    };
    let claims = StateClaims {
        sub: account.to_string(),
        connector: connector.to_string(),
        scope: "user".into(),
        coworker: None,
        connection: None,
        attempt: Some(attempt),
        plugin: Some(plugin.to_string()),
        nonce: opengrok_core::id::RunId::new().to_string(),
        exp: 0,
    };
    let (token, expires_at) = match sign_state(&state.auth.minter, &claims, at_ms / 1_000) {
        Ok(signed) => signed,
        Err(error) => return refused(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    };
    let url = mcp_oauth::authorize_url(&metadata, &client, &redirect_uri, &token, &pkce.challenge);
    if format == Some("json") {
        return Json(serde_json::json!({ "url": url, "expiresAtMs": expires_at * 1_000 }))
            .into_response();
    }
    Redirect::temporary(&url).into_response()
}

/// Reopen a waiting MCP sign-in: the same one, a fresh trip to its provider.
pub async fn reopen(
    state: &AgUiState,
    account: AccountId,
    attempt: &attempts::Attempt,
    format: Option<&str>,
) -> Response {
    let Some(plugin) = attempt.plugin.as_deref() else {
        return refused(StatusCode::NOT_FOUND, "no such sign-in");
    };
    let mcp_url = match server_of(state, &account, plugin, &attempt.connector).await {
        Ok(url) => url,
        Err(response) => return response,
    };
    let begin = Begin::Reopen(attempt.id.clone());
    link(
        state,
        account,
        plugin,
        &attempt.connector,
        &mcp_url,
        begin,
        format,
    )
    .await
}

/// The callback's half for an MCP sign-in: trade the code at the plugin's provider and keep the
/// account. What the browser is shown is plain words, as a configured sign-in's callback shows.
pub async fn finish(state: &AgUiState, claims: &StateClaims, code: &str) -> Response {
    let account = AccountId::from_stored(claims.sub.clone());
    let Some(id) = claims.attempt.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            "that sign-in is not one this server started",
        )
            .into_response();
    };
    let Some(vault) = state.vault.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no credential vault configured",
        )
            .into_response();
    };
    let store = &state.auth.store;
    let redirect_uri = &state.connectors.redirect_uri;
    let pending = match attempts::mcp_pending(store, &account, id, redirect_uri).await {
        Ok(Some(pending)) => pending,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                "that sign-in is no longer waiting; start it again from the app",
            )
                .into_response();
        }
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let http = mcp_oauth::http();
    let token =
        match mcp_oauth::exchange(&http, mcp_oauth::PUBLIC, store, vault, &pending, code).await {
            Ok(token) => token,
            Err(error) => {
                give_up(
                    state,
                    &account,
                    id,
                    pending.target.is_some(),
                    &error.to_string(),
                )
                .await;
                return (StatusCode::BAD_GATEWAY, error.to_string()).into_response();
            }
        };
    match mcp_oauth::connect(store, vault, &account, &pending, &token, now_ms()).await {
        Ok(Some(_)) => {
            let _ = attempts::remove(store, &account, id).await;
            (
                StatusCode::OK,
                format!(
                    "{} is connected. You can close this window and go back to the app.",
                    pending.label
                ),
            )
                .into_response()
        }
        Ok(None) => {
            let _ = attempts::remove(store, &account, id).await;
            (
                StatusCode::CONFLICT,
                format!(
                    "{} was uninstalled while you were signing in, so nothing was kept.",
                    pending.plugin
                ),
            )
                .into_response()
        }
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// A refused MCP sign-in needs auth, with why; a refused reconnect is dropped instead, since the
/// account it would have refreshed still works.
pub async fn give_up(state: &AgUiState, account: &AccountId, id: &str, reconnect: bool, why: &str) {
    let store = &state.auth.store;
    let done = if reconnect {
        attempts::remove(store, account, id).await.map(|_| ())
    } else {
        attempts::fail(store, account, id, why, now_ms()).await
    };
    if let Err(error) = done {
        tracing::warn!(%error, "a refused plugin sign-in could not be recorded");
    }
}
