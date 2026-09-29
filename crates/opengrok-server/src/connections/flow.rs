//! The round trip: sign a state, send them away, take the code back, get a token.
//!
//! THE STATE IS THE CSRF DEFENCE, AND IT IS SIGNED FOR ONE REASON. Everything in a callback's query
//! string is attacker-controlled. If the account came from there, anybody could hand a person a
//! callback URL and attach *their* Google account to *the victim's* OpenGrok session — the victim's
//! coworkers would then be reading the attacker's mail, or writing to it. So the account travels
//! inside a signature we minted, and the query string supplies only the code.
//!
//! IT IS A JWT BECAUSE THE ALTERNATIVE IS A TABLE. An opaque nonce would have to be stored, looked
//! up and reaped; a signed token carries its own claims and its own expiry, and `TokenMinter`
//! already exists for exactly this.
//!
//! ON REPLAY: a state can be presented twice inside its ten minutes, and that is deliberate rather
//! than overlooked. The authorization *code* is single-use at every provider, so the second attempt
//! fails at the token endpoint. Storing nonces to close a window the provider already closes would
//! be a table, a reaper and a new failure mode for no gain.

use serde::{Deserialize, Serialize};

use crate::auth::token::TokenMinter;

use super::oauth::{ProviderConfig, STATE_TTL_SECONDS, StateClaims, TokenError, TokenResponse};

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    #[error("no provider is configured for {0}")]
    UnknownConnector(String),
    #[error("that sign-in link is not one we issued, or it has expired")]
    BadState,
    #[error("the provider refused: {0}")]
    Refused(String),
    #[error("the provider is unreachable: {0}")]
    Unreachable(String),
    #[error("the provider's reply could not be read: {0}")]
    Unreadable(String),
}

impl FlowError {
    /// Whether this means the person revoked access, rather than something transient.
    ///
    /// `invalid_grant` on a refresh is a decision somebody made, not a hiccup: retrying it forever
    /// is how a revoked connection becomes a permanent error loop.
    pub fn is_revoked(&self) -> bool {
        matches!(self, Self::Refused(reason) if reason.contains("invalid_grant"))
    }
}

/// The claims a JWT state carries, wrapped so `TokenMinter`'s access-token shape is not reused for
/// something it does not mean.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignedState {
    sub: String,
    connector: String,
    scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coworker: Option<String>,
    nonce: String,
    exp: i64,
    /// Marks this as a connection state and not an access token. Without it, an access token would
    /// verify here and a stolen one could drive a callback.
    #[serde(rename = "use")]
    purpose: String,
}

const STATE_PURPOSE: &str = "connection-state";

/// Sign the state a person carries to the provider and back. The second it expires comes back
/// with it, so what an app is told (`expiresAtMs`, #269) is the `exp` that was signed.
pub fn sign_state(
    minter: &TokenMinter,
    claims: &StateClaims,
    now_seconds: i64,
) -> Result<(String, i64), FlowError> {
    let exp = now_seconds + STATE_TTL_SECONDS;
    let signed = SignedState {
        sub: claims.sub.clone(),
        connector: claims.connector.clone(),
        scope: claims.scope.clone(),
        coworker: claims.coworker.clone(),
        nonce: claims.nonce.clone(),
        exp,
        purpose: STATE_PURPOSE.to_string(),
    };
    minter
        .mint_claims(&signed)
        .map(|token| (token, exp))
        .map_err(|error| FlowError::Unreadable(error.to_string()))
}

/// Read a state back, refusing anything we did not mint for this purpose.
pub fn verify_state(minter: &TokenMinter, state: &str) -> Result<StateClaims, FlowError> {
    let signed: SignedState = minter
        .verify_claims(state)
        .map_err(|_| FlowError::BadState)?;

    // An access token is signed with the same key. Without this check one would verify here, and a
    // stolen access token could be replayed as a callback state.
    if signed.purpose != STATE_PURPOSE {
        return Err(FlowError::BadState);
    }

    Ok(StateClaims {
        sub: signed.sub,
        connector: signed.connector,
        scope: signed.scope,
        coworker: signed.coworker,
        nonce: signed.nonce,
        exp: signed.exp,
    })
}

/// Exchange an authorization code for a token.
pub async fn exchange_code(
    http: &reqwest::Client,
    config: &ProviderConfig,
    redirect_uri: &str,
    code: &str,
    verifier: Option<&str>,
) -> Result<TokenResponse, FlowError> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        // Must match the authorize request byte for byte, or the provider refuses with an error
        // that says nothing about which character differs.
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", config.client_id.clone()),
        ("client_secret", config.client_secret.clone()),
    ];
    if let Some(verifier) = verifier {
        form.push(("code_verifier", verifier.to_string()));
    }
    post_form(http, &config.token_url, &form).await
}

/// Trade a refresh token for a new access token.
pub async fn refresh(
    http: &reqwest::Client,
    config: &ProviderConfig,
    refresh_token: &str,
) -> Result<TokenResponse, FlowError> {
    let form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", config.client_id.clone()),
        ("client_secret", config.client_secret.clone()),
    ];
    post_form(http, &config.token_url, &form).await
}

async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, String)],
) -> Result<TokenResponse, FlowError> {
    let response = http
        .post(url)
        // NOT OPTIONAL FOR GITHUB. Without it the reply is form-encoded and a JSON parse fails
        // where a token should be. `TokenResponse::parse` handles both anyway, because "always"
        // lasts until somebody's proxy strips a header.
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .await
        .map_err(|error| FlowError::Unreachable(error.to_string()))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| FlowError::Unreadable(error.to_string()))?;

    if !status.is_success() {
        // Parsed rather than dumped: `invalid_grant` means revoked, which is a disconnect.
        let reason = serde_json::from_str::<TokenError>(&body)
            .map(|error| match error.error_description {
                Some(description) => format!("{} ({description})", error.error),
                None => error.error,
            })
            .unwrap_or_else(|_| body.chars().take(300).collect());
        return Err(FlowError::Refused(reason));
    }

    // A 200 can still carry an error; GitHub does exactly this for a bad code.
    if let Ok(error) = serde_json::from_str::<TokenError>(&body) {
        return Err(FlowError::Refused(error.error));
    }

    TokenResponse::parse(&body).map_err(FlowError::Unreadable)
}

#[cfg(test)]
#[path = "../../tests/unit/connections_flow.rs"]
mod tests;
