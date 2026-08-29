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

/// Sign the state a person carries to the provider and back.
pub fn sign_state(
    minter: &TokenMinter,
    claims: &StateClaims,
    now_seconds: i64,
) -> Result<String, FlowError> {
    let signed = SignedState {
        sub: claims.sub.clone(),
        connector: claims.connector.clone(),
        scope: claims.scope.clone(),
        coworker: claims.coworker.clone(),
        nonce: claims.nonce.clone(),
        exp: now_seconds + STATE_TTL_SECONDS,
        purpose: STATE_PURPOSE.to_string(),
    };
    minter
        .mint_claims(&signed)
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn minter() -> TokenMinter {
        TokenMinter::new(b"a-test-secret-that-is-long-enough")
    }

    fn claims() -> StateClaims {
        StateClaims {
            sub: "acct_1".to_string(),
            connector: "gmail".to_string(),
            scope: "user".to_string(),
            coworker: None,
            nonce: "n1".to_string(),
            exp: 0,
        }
    }

    #[test]
    fn a_state_we_signed_reads_back() {
        let now = chrono::Utc::now().timestamp();
        let state = sign_state(&minter(), &claims(), now).unwrap();
        let read = verify_state(&minter(), &state).unwrap();
        assert_eq!(read.sub, "acct_1");
        assert_eq!(read.connector, "gmail");
        assert_eq!(read.nonce, "n1");
    }

    /// THE VULNERABILITY THIS EXISTS FOR. A state somebody else made must not attach their account
    /// to this session.
    #[test]
    fn a_state_signed_with_another_key_is_refused() {
        let now = chrono::Utc::now().timestamp();
        let theirs = TokenMinter::new(b"an-attackers-entirely-different-key");
        let state = sign_state(&theirs, &claims(), now).unwrap();
        assert!(matches!(
            verify_state(&minter(), &state),
            Err(FlowError::BadState)
        ));
    }

    #[test]
    fn a_tampered_state_is_refused() {
        let now = chrono::Utc::now().timestamp();
        let mut state = sign_state(&minter(), &claims(), now).unwrap();
        state.push('x');
        assert!(verify_state(&minter(), &state).is_err());
    }

    /// A state that outlives the walk to the provider is one somebody can sit on.
    ///
    /// Signed an hour past its TTL rather than a second past: `jsonwebtoken` allows 60 seconds of
    /// clock leeway by default, so a state that expired moments ago is still accepted — which is
    /// correct behaviour for skewed clocks and would make this test assert nothing.
    #[test]
    fn an_expired_state_is_refused() {
        let long_ago = chrono::Utc::now().timestamp() - STATE_TTL_SECONDS - 3_600;
        let state = sign_state(&minter(), &claims(), long_ago).unwrap();
        assert!(verify_state(&minter(), &state).is_err());
    }

    /// An access token is signed with the same key. Without the purpose claim, a stolen one would
    /// verify here and drive a callback.
    #[test]
    fn an_access_token_is_not_accepted_as_a_state() {
        let now = chrono::Utc::now().timestamp();
        let access = minter()
            .mint_access("acct_1", "sess_1", "a@b.c", "pro", now, 3600)
            .unwrap();
        assert!(matches!(
            verify_state(&minter(), &access),
            Err(FlowError::BadState)
        ));
    }

    #[test]
    fn nothing_at_all_is_refused() {
        assert!(verify_state(&minter(), "").is_err());
        assert!(verify_state(&minter(), "not.a.token").is_err());
    }

    /// A revoked connection must be told apart from a transient failure, or it retries forever.
    #[test]
    fn a_revoked_grant_is_recognised_as_revocation() {
        assert!(FlowError::Refused("invalid_grant (expired)".to_string()).is_revoked());
        assert!(!FlowError::Unreachable("timeout".to_string()).is_revoked());
        assert!(!FlowError::Refused("temporarily_unavailable".to_string()).is_revoked());
    }
}
