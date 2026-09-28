//! Minting the tokens the desktop client will hold.
//!
//! THE ACCESS TOKEN MUST BE A REAL JWT, AND THE CLIENT READS IT WITHOUT ASKING US.
//! `opengrok/source/shared/node/cursor-token.ts:9-22` base64url-decodes the payload segment
//! itself, and `cursor-auth.ts:67-73` builds the whole `logged-in` status from three claims:
//!   - `sub`  → `authId`, which keys the client's profile cache and its avatar lookup;
//!   - `email` → shown in the account menu;
//!   - `exp`  → `expiresAt`, in SECONDS; the client multiplies by 1000 itself.
//!
//! An opaque token would parse to `null` and the client would treat a successful login as
//! logged-out, with no error anywhere to explain it.
//!
//! `isTokenExpiringSoon` (`cursor-token.ts:27-30`) refreshes when `exp` is under five minutes
//! away, and `shouldRefreshAccessToken` refreshes on EVERY call against a dev backend — which we
//! are, by definition (§ the dev-client-id rule). So the refresh path is not a rare edge: it runs
//! constantly, and it is the path most worth testing.

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How long an access token lives. Short, because refresh is cheap and constant here; long enough
/// that a clock skew of a minute or two does not sign somebody out mid-request.
pub const ACCESS_TOKEN_TTL_SECONDS: i64 = 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    /// The account id. The client calls this `authId`.
    pub sub: String,
    pub email: String,
    /// Seconds since the epoch — NOT milliseconds. `cursor-auth.ts:71` multiplies by 1000.
    pub exp: i64,
    /// Which session minted it, so revoking a session can invalidate its access tokens later.
    pub sid: String,
    pub plan: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("could not mint a token: {0}")]
    Mint(String),
    #[error("the token is not valid: {0}")]
    Invalid(String),
}

/// Signs and verifies our own tokens. HS256 with a single secret: there is one issuer and one
/// verifier here, so an asymmetric key would add key distribution without adding a boundary.
#[derive(Clone)]
pub struct TokenMinter {
    encoding: EncodingKey,
    decoding: DecodingKey,
}

impl std::fmt::Debug for TokenMinter {
    /// Hand-written so the signing key cannot reach a log through a derived `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenMinter(<redacted>)")
    }
}

impl TokenMinter {
    pub fn new(secret: &[u8]) -> Self {
        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
        }
    }

    /// Mint an access token that expires `ttl_seconds` from `now_seconds`.
    pub fn mint_access(
        &self,
        account_id: &str,
        session_id: &str,
        email: &str,
        plan: &str,
        now_seconds: i64,
        ttl_seconds: i64,
    ) -> Result<String, TokenError> {
        let claims = AccessClaims {
            sub: account_id.to_string(),
            email: email.to_string(),
            exp: now_seconds + ttl_seconds,
            sid: session_id.to_string(),
            plan: plan.to_string(),
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|error| TokenError::Mint(error.to_string()))
    }

    /// Sign arbitrary claims with the same key.
    ///
    /// Used for the OAuth `state`, which needs a signature and an expiry but is not an access
    /// token. Callers MUST include a claim saying what the token is for — otherwise an access token
    /// verifies here too, and a stolen one becomes usable wherever this is checked.
    pub fn mint_claims<T: Serialize>(&self, claims: &T) -> Result<String, TokenError> {
        encode(&Header::new(Algorithm::HS256), claims, &self.encoding)
            .map_err(|error| TokenError::Mint(error.to_string()))
    }

    /// Read arbitrary claims back, checking the signature and `exp`.
    pub fn verify_claims<T: for<'de> Deserialize<'de>>(
        &self,
        token: &str,
    ) -> Result<T, TokenError> {
        let mut validation = Validation::new(Algorithm::HS256);
        // `aud` is checked by the caller that knows its resource (the MCP door, for an
        // OAuth-minted bot key). jsonwebtoken would otherwise reject ANY token carrying `aud`
        // because no expected audience is configured here — which is how an OAuth key first
        // read as "unrecognised bearer".
        validation.validate_aud = false;
        decode::<T>(token, &self.decoding, &validation)
            .map(|data| data.claims)
            .map_err(|error| TokenError::Invalid(error.to_string()))
    }

    /// Verify and read the claims back. Used by everything downstream that needs to know who is
    /// calling — the gateway commands in the next slice included.
    pub fn verify_access(&self, token: &str) -> Result<AccessClaims, TokenError> {
        let validation = Validation::new(Algorithm::HS256);
        decode::<AccessClaims>(token, &self.decoding, &validation)
            .map(|data| data.claims)
            .map_err(|error| TokenError::Invalid(error.to_string()))
    }
}

/// A refresh token: opaque, high-entropy, and never a JWT.
///
/// The client only ever hands it back to us (`cursor-auth.ts:340`), so it carries no claims worth
/// reading, and making it opaque means a leaked one tells an attacker nothing about the account.
pub fn mint_refresh_token() -> String {
    use rand::RngExt;
    let bytes: [u8; 32] = rand::rng().random();
    format!("ogr_{}", hex(&bytes))
}

/// What goes in the event log in place of the token itself.
pub fn hash_refresh_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        // `write!` to a String cannot fail; the result is discarded rather than unwrapped because
        // `unwrap` is denied workspace-wide and a panic here would be absurd.
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
#[path = "../../tests/unit/auth_token.rs"]
mod tests;
