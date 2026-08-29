//! The endpoints that turn the pieces into a connector somebody can actually use.
//!
//! THE BROWSER LANDS HERE, NEVER ON A CLIENT. `/authorize` sends a person to the provider and
//! `/callback` catches them coming back, so the `code` and the `client_secret` meet on the server
//! and nowhere else. A client is told "connected" and is never handed a token — which is the same
//! rule the desktop and web clients already live under.
//!
//! AUTHENTICATING AND LENDING STAY TWO ENDPOINTS, because they are two decisions. A person
//! authenticates Gmail once; lending it to five coworkers is five separate acts they can undo one
//! at a time. Fusing them is what makes people sign in once per coworker.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use opengrok_core::connection::{Connection, ConnectionCommand, Owner};
use opengrok_core::id::{AccountId, CoworkerId};
use serde::Deserialize;

use crate::agui::routes::{AgUiState, account_from_bearer, now_ms};

use super::flow::{FlowError, exchange_code, sign_state, verify_state};
use super::oauth::{ProviderConfig, StateClaims, authorize_url};

/// Everything the connector endpoints need beyond `AgUiState`.
#[derive(Clone, Default)]
pub struct Connectors {
    /// Provider configuration, by connector name. Empty means no connector is configured, which is
    /// a legitimate deployment and must read as "none offered" rather than as an error.
    pub providers: Arc<BTreeMap<String, ProviderConfig>>,
    /// Where a browser comes back to. Must match the provider registration byte for byte, so it is
    /// configured rather than derived from a request — a proxy rewriting `Host` would otherwise
    /// silently change it.
    pub redirect_uri: String,
}

impl std::fmt::Debug for Connectors {
    /// Provider configs carry client secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connectors")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/connections", get(list_connections))
        .route("/connections/{connector}/authorize", get(authorize))
        .route("/connections/callback", get(callback))
        .route("/connections/{id}/lend", post(lend))
        .route("/connections/{id}/revoke", post(revoke))
        .route("/connections/{id}", delete(disconnect))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    /// Give the connection to a coworker rather than to the person signing in. Used when a
    /// coworker must act as itself.
    #[serde(default)]
    pub coworker_id: Option<String>,
}

/// Send a person to the provider.
pub async fn authorize(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(connector): Path<String>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let Some(config) = state.connectors.providers.get(&connector) else {
        return (
            StatusCode::NOT_FOUND,
            FlowError::UnknownConnector(connector).to_string(),
        )
            .into_response();
    };

    // A coworker-scoped connection may only be started by somebody who may already use that
    // coworker — otherwise this endpoint would be a way to attach a provider account to a
    // stranger's bot.
    if let Some(coworker) = &query.coworker_id {
        let coworker_id = CoworkerId::from_stored(coworker.clone());
        let policy = state
            .auth
            .store
            .policy_for(&account_id, &coworker_id)
            .await
            .unwrap_or_default();
        let decision = opengrok_policy::decide(
            &account_id,
            &coworker_id,
            opengrok_policy::Action::UseCoworker,
            &policy,
        );
        if let Some(reason) = decision.reason() {
            return (StatusCode::FORBIDDEN, reason.to_string()).into_response();
        }
    }

    let claims = StateClaims {
        sub: account_id.to_string(),
        connector: connector.clone(),
        scope: if query.coworker_id.is_some() {
            "bot"
        } else {
            "user"
        }
        .to_string(),
        coworker: query.coworker_id.clone(),
        // Fresh per attempt, so two tabs do not collide on one state.
        nonce: opengrok_core::id::RunId::new().to_string(),
        exp: 0,
    };

    let state_token = match sign_state(&state.auth.minter, &claims, now_ms() / 1_000) {
        Ok(token) => token,
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    };

    Redirect::temporary(&authorize_url(
        config,
        &state.connectors.redirect_uri,
        &state_token,
        None,
    ))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// A provider says no here rather than by status code.
    #[serde(default)]
    pub error: Option<String>,
}

/// Catch a person coming back from the provider.
///
/// NOTHING HERE TRUSTS THE QUERY STRING except the code. The account, the connector and the scope
/// all come out of the signed state, because everything in a callback URL is attacker-supplied.
pub async fn callback(
    State(state): State<AgUiState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        // A person who declined is not a failure to report as one.
        return (
            StatusCode::OK,
            format!("the provider did not connect: {error}. You can close this window."),
        )
            .into_response();
    }

    let (Some(code), Some(state_token)) = (query.code, query.state) else {
        return (StatusCode::BAD_REQUEST, "that callback is missing its code").into_response();
    };

    let claims = match verify_state(&state.auth.minter, &state_token) {
        Ok(claims) => claims,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };

    let Some(config) = state.connectors.providers.get(&claims.connector) else {
        return (
            StatusCode::NOT_FOUND,
            FlowError::UnknownConnector(claims.connector).to_string(),
        )
            .into_response();
    };

    let token = match exchange_code(
        &reqwest::Client::new(),
        config,
        &state.connectors.redirect_uri,
        &code,
        None,
    )
    .await
    {
        Ok(token) => token,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, error.to_string()).into_response();
        }
    };

    let account_id = AccountId::from_stored(claims.sub.clone());
    let owner = match (claims.scope.as_str(), &claims.coworker) {
        ("bot", Some(coworker)) => Owner::Bot(CoworkerId::from_stored(coworker.clone())),
        _ => Owner::User(account_id.clone()),
    };

    // One connection per (connector, owner): re-authenticating replaces the credential rather than
    // adding a rival nobody chooses between.
    let connection_id = match &owner {
        Owner::Bot(coworker) => format!("conn_{}_{}", claims.connector, coworker),
        Owner::User(account) => format!("conn_{}_{}", claims.connector, account),
        Owner::Global => format!("conn_{}_global", claims.connector),
    };

    let (existing, seq) = match state.auth.store.load_connection(&connection_id).await {
        Ok(loaded) => loaded,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    // Google issues a refresh token only on the first consent, so the stored one is kept when the
    // provider omits it — see `TokenResponse::refresh_token_to_store`.
    let previous_refresh = match state.vault.as_ref() {
        Some(vault) => state
            .auth
            .store
            .open_credential(vault, &format!("{connection_id}_refresh"))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let refresh_to_store = token.refresh_token_to_store(previous_refresh.as_deref());

    let at_ms = now_ms();
    let mut connection = existing;
    let events = if connection.connected {
        connection
            .decide(ConnectionCommand::Refresh { at_ms })
            .unwrap_or_default()
    } else {
        connection
            .decide(ConnectionCommand::Connect {
                connector: claims.connector.clone(),
                owner: owner.clone(),
                // The label is what a person sees; a real deployment fetches the provider's own
                // profile here. The connector name is honest until that exists.
                label: claims.connector.clone(),
                at_ms,
            })
            .unwrap_or_default()
    };
    for event in &events {
        connection.apply(event);
    }

    let Some(vault) = state.vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no credential vault configured, so it cannot store a connection",
        )
            .into_response();
    };

    let sealed = match vault.seal(&connection_id, &token.access_token) {
        Ok(sealed) => sealed,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    if let Err(error) = state
        .auth
        .store
        .append_connection(
            &connection_id,
            seq,
            &events,
            &connection,
            &opengrok_store::CredentialUpdate::sealed(&sealed, token.expires_at_ms(at_ms), at_ms),
        )
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    // The refresh token is its own row: it outlives the access token, and keeping them apart means
    // rotating one does not disturb the other.
    if let Some(refresh) = refresh_to_store
        && let Ok(sealed) = vault.seal(&format!("{connection_id}_refresh"), &refresh)
    {
        let _ = state
            .auth
            .store
            .put_secret(&format!("{connection_id}_refresh"), &sealed, at_ms)
            .await;
    }

    (
        StatusCode::OK,
        format!(
            "{} is connected. You can close this window and go back to the app.",
            claims.connector
        ),
    )
        .into_response()
}

/// What a person has connected, and who they have lent it to.
pub async fn list_connections(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    match state.auth.store.connections_owned_by(&account_id).await {
        // An ARRAY, always: nothing connected is a valid answer.
        Ok(connections) => Json(connections).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct LendRequest {
    pub coworker_id: String,
}

/// Lend a connection to a coworker — the "no need to authenticate again" move.
pub async fn lend(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<LendRequest>,
) -> Response {
    mutate(state, headers, id, |connection, at_ms| {
        connection.decide(ConnectionCommand::Lend {
            coworker: CoworkerId::from_stored(request.coworker_id.clone()),
            at_ms,
        })
    })
    .await
}

pub async fn revoke(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<LendRequest>,
) -> Response {
    mutate(state, headers, id, |connection, at_ms| {
        connection.decide(ConnectionCommand::Revoke {
            coworker: CoworkerId::from_stored(request.coworker_id.clone()),
            at_ms,
        })
    })
    .await
}

pub async fn disconnect(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    mutate(state, headers, id, |connection, at_ms| {
        connection.decide(ConnectionCommand::Disconnect { at_ms })
    })
    .await
}

/// Load, decide, append — with the ownership check every one of these needs.
///
/// Shared because forgetting the ownership check on one endpoint is exactly the bug this shape
/// prevents: a connection id would otherwise be enough to lend somebody else's Gmail to your bot.
async fn mutate<F>(state: AgUiState, headers: HeaderMap, id: String, decide: F) -> Response
where
    F: FnOnce(
        &Connection,
        i64,
    ) -> Result<
        Vec<opengrok_core::connection::ConnectionEvent>,
        opengrok_core::connection::ConnectionError,
    >,
{
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    let (mut connection, seq) = match state.auth.store.load_connection(&id).await {
        Ok(loaded) => loaded,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    // 404 for both "no such connection" and "not yours", so an id reveals nothing.
    let owned = matches!(&connection.owner, Some(Owner::User(owner)) if owner == &account_id);
    if !connection.connected || !owned {
        return (StatusCode::NOT_FOUND, "no such connection").into_response();
    }

    let at_ms = now_ms();
    let events = match decide(&connection, at_ms) {
        Ok(events) => events,
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };
    for event in &events {
        connection.apply(event);
    }

    if let Err(error) = state
        .auth
        .store
        // A lend says nothing about the credential, so the stored token and its expiry are left
        // exactly as the last exchange recorded them.
        .append_connection(
            &id,
            seq,
            &events,
            &connection,
            &opengrok_store::CredentialUpdate::none(at_ms),
        )
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    Json(serde_json::json!({
        "id": id,
        "connector": connection.connector,
        "lentTo": connection.loans.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        "disconnected": connection.disconnected,
    }))
    .into_response()
}
