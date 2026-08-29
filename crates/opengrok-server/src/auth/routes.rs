//! The two endpoints that replace Cursor's OAuth.
//!
//! Setting `SAND_BACKEND_URL` repoints the client's ENTIRE auth backend at us
//! (`cursor-token.ts:39`), and any host that is not `api2.cursor.sh` resolves to the dev client id,
//! which makes `isDevAuthBackend` true (`:42-49`). That unlocks the dev-login path — so the first
//! slice needs no browser, no PKCE and no redirect: two endpoints and a JWT.
//!
//! THE TWO REPLIES USE DIFFERENT CASING AND IT IS NOT A MISTAKE.
//!   - `/auth/cursor_dev_session_token` → `{ accessToken, refreshToken }`  (camelCase)
//!     `cursor-auth.ts:315-316` reads `body.accessToken` and `body.refreshToken`.
//!   - `POST /oauth/token`             → `{ access_token, refresh_token }` (snake_case)
//!     `parseOAuthTokenBody` (`:160-166`) rejects the body outright unless those keys are strings.
//! Normalising either one to match the other is the tidying that breaks a client we do not compile.
//!
//! On `refreshToken` being optional in the dev reply (`:316` falls back to the access token): we
//! always send one. Falling back would make the client hold an access token as its refresh token,
//! and `cursorSessionPresent` would report a session that cannot survive its own first refresh.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use opengrok_core::account::{AccountCommand, AccountError, Plan};
use opengrok_core::id::{AccountId, SessionId};
use opengrok_store::{PgStore, StoreError};
use serde::{Deserialize, Serialize};

use super::token::{ACCESS_TOKEN_TTL_SECONDS, TokenMinter, hash_refresh_token, mint_refresh_token};

#[derive(Clone)]
pub struct AuthState {
    pub store: PgStore,
    pub minter: Arc<TokenMinter>,
}

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route("/auth/cursor_dev_session_token", get(dev_session_token))
        .route("/oauth/token", post(oauth_token))
        .with_state(state)
}

/// `GET /auth/cursor_dev_session_token?plan=&trial=&email=`
///
/// Query parameters as the client builds them at `cursor-auth.ts:313`: `plan` always, `trial=true`
/// only when the tier was a trial tier, `email` only when non-empty.
#[derive(Debug, Deserialize)]
pub struct DevSessionQuery {
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub trial: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// camelCase — see the module note.
#[derive(Debug, Serialize)]
pub struct DevSessionReply {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
}

/// snake_case — see the module note.
#[derive(Debug, Serialize)]
pub struct OAuthTokenReply {
    pub access_token: String,
    pub refresh_token: String,
}

/// What the client POSTs to `/oauth/token` (`cursor-auth.ts:340`).
#[derive(Debug, Deserialize)]
pub struct OAuthTokenRequest {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub grant_type: Option<String>,
    pub refresh_token: String,
}

/// A failure the client can act on.
///
/// `shouldLogout: true` is the client's instruction to drop the session and show the sign-in wall
/// (`cursor-auth.ts:344-345`). We send it only when the refresh token is genuinely not ours to
/// honour — never on a transient database error, which would sign people out during an outage.
#[derive(Debug, Serialize)]
pub struct AuthErrorBody {
    pub error: String,
    #[serde(rename = "shouldLogout", skip_serializing_if = "Option::is_none")]
    pub should_logout: Option<bool>,
}

pub enum AuthFailure {
    /// The session is gone or the token was never ours. Sign out.
    SessionRejected(String),
    /// Something on our side broke. The client keeps its session and retries.
    Unavailable(String),
}

impl IntoResponse for AuthFailure {
    fn into_response(self) -> Response {
        match self {
            Self::SessionRejected(error) => (
                StatusCode::UNAUTHORIZED,
                Json(AuthErrorBody {
                    error,
                    should_logout: Some(true),
                }),
            )
                .into_response(),
            Self::Unavailable(error) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(AuthErrorBody {
                    error,
                    should_logout: None,
                }),
            )
                .into_response(),
        }
    }
}

impl From<StoreError> for AuthFailure {
    fn from(error: StoreError) -> Self {
        Self::Unavailable(error.to_string())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Sign in. Idempotent per email: a second call adds a session, never a second account.
pub async fn dev_session_token(
    State(state): State<AuthState>,
    Query(query): Query<DevSessionQuery>,
) -> Result<Json<DevSessionReply>, AuthFailure> {
    let plan = Plan::from_wire(query.plan.as_deref().unwrap_or_default());
    // The client only ever sends `trial=true`, and only when it means it (`cursor-auth.ts:313`).
    let trial = query.trial.as_deref() == Some("true");
    let email = query
        .email
        .filter(|email| !email.is_empty())
        // No email arrives when the tier had none; a stable placeholder keeps the account
        // identifiable across launches instead of minting a new one each time.
        .unwrap_or_else(|| "dev@opengrok.local".to_string());

    // Read side answers "does this person already exist"; the write side replays their log.
    let account_id = match state.store.account_by_email(&email).await? {
        Some(view) => view.id,
        None => AccountId::new(),
    };

    let (account, seq) = state.store.load_account(&account_id).await?;

    let session_id = SessionId::new();
    let refresh_token = mint_refresh_token();
    let at_ms = now_ms();

    let events = account
        .decide(AccountCommand::SignIn {
            email: email.clone(),
            plan,
            trial,
            session_id: session_id.clone(),
            refresh_token_hash: hash_refresh_token(&refresh_token),
            at_ms,
        })
        .map_err(|error: AccountError| AuthFailure::SessionRejected(error.to_string()))?;

    let view = opengrok_core::account::AccountView {
        id: account_id.clone(),
        email: email.clone(),
        plan,
        trial,
        updated_at_ms: at_ms,
    };
    state
        .store
        .append_account(&account_id, seq, &events, &view)
        .await?;

    let access_token = state
        .minter
        .mint_access(
            account_id.as_str(),
            session_id.as_str(),
            &email,
            plan.as_wire(),
            at_ms / 1_000,
            ACCESS_TOKEN_TTL_SECONDS,
        )
        .map_err(|error| AuthFailure::Unavailable(error.to_string()))?;

    Ok(Json(DevSessionReply {
        access_token,
        refresh_token,
    }))
}

/// Rotate. The client calls this constantly — `shouldRefreshAccessToken` is unconditionally true
/// against a dev backend (`cursor-token.ts:50`), so this is the hot path, not the rare one.
pub async fn oauth_token(
    State(state): State<AuthState>,
    Json(request): Json<OAuthTokenRequest>,
) -> Result<Json<OAuthTokenReply>, AuthFailure> {
    if let Some(grant) = request.grant_type.as_deref()
        && grant != "refresh_token"
    {
        return Err(AuthFailure::SessionRejected(format!(
            "unsupported grant_type {grant}"
        )));
    }

    let presented_hash = hash_refresh_token(&request.refresh_token);
    let account_id = state
        .store
        .account_by_refresh_hash(&presented_hash)
        .await?
        .ok_or_else(|| AuthFailure::SessionRejected("unknown refresh token".to_string()))?;

    let (account, seq) = state.store.load_account(&account_id).await?;
    let new_refresh = mint_refresh_token();
    let at_ms = now_ms();

    let events = account
        .decide(AccountCommand::Refresh {
            presented_hash,
            new_hash: hash_refresh_token(&new_refresh),
            at_ms,
        })
        .map_err(|error| AuthFailure::SessionRejected(error.to_string()))?;

    // The session the rotation belongs to — needed for the new access token's `sid`.
    let session_id = events
        .iter()
        .find_map(|event| match event {
            opengrok_core::account::AccountEvent::SessionRefreshed { session_id, .. } => {
                Some(session_id.clone())
            }
            _ => None,
        })
        .ok_or_else(|| AuthFailure::Unavailable("refresh produced no session".to_string()))?;

    let plan = account.plan.unwrap_or(Plan::Ultra);
    let view = opengrok_core::account::AccountView {
        id: account_id.clone(),
        email: account.email.clone(),
        plan,
        trial: account.trial,
        updated_at_ms: at_ms,
    };
    state
        .store
        .append_account(&account_id, seq, &events, &view)
        .await?;

    let access_token = state
        .minter
        .mint_access(
            account_id.as_str(),
            session_id.as_str(),
            &account.email,
            plan.as_wire(),
            at_ms / 1_000,
            ACCESS_TOKEN_TTL_SECONDS,
        )
        .map_err(|error| AuthFailure::Unavailable(error.to_string()))?;

    Ok(Json(OAuthTokenReply {
        access_token,
        refresh_token: new_refresh,
    }))
}
