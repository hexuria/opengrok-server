//! Minting the tokens the desktop client will hold.
//!
//! THE ACCESS TOKEN MUST BE A REAL JWT, AND THE CLIENT READS IT WITHOUT ASKING US.
//! `grok-bot/source/shared/node/cursor-token.ts:9-22` base64url-decodes the payload segment
//! itself, and `cursor-auth.ts:67-73` builds the whole `logged-in` status from three claims:
//!   - `sub`  → `authId`, which keys the client's profile cache and its avatar lookup;
//!   - `email` → shown in the account menu;
//!   - `exp`  → `expiresAt`, in SECONDS; the client multiplies by 1000 itself.
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use base64::Engine;

    fn minter() -> TokenMinter {
        TokenMinter::new(b"a-test-secret-that-is-long-enough")
    }

    /// Minted against the real clock, because `verify_access` checks `exp` against the real clock —
    /// a fixed epoch here would test a token that expired in 1970.
    #[test]
    fn a_minted_token_verifies_and_carries_its_claims() {
        let now = chrono::Utc::now().timestamp();
        let token = minter()
            .mint_access("acct_1", "sess_1", "a@b.c", "pro", now, 3_600)
            .unwrap();
        let claims = minter().verify_access(&token).unwrap();
        assert_eq!(claims.sub, "acct_1");
        assert_eq!(claims.email, "a@b.c");
        assert_eq!(claims.exp, now + 3_600);
        assert_eq!(claims.sid, "sess_1");
    }

    /// The client decodes the payload segment itself, without our help and without verifying.
    /// This test is that client, so a change to the claim names fails here rather than in the app.
    #[test]
    fn the_client_can_read_sub_email_and_exp_by_itself() {
        let token = minter()
            .mint_access("acct_9", "sess_9", "who@example.com", "ultra", 2_000, 3_600)
            .unwrap();
        let payload = token.split('.').nth(1).unwrap();
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["sub"], "acct_9");
        assert_eq!(value["email"], "who@example.com");
        // Seconds, not milliseconds — the client multiplies by 1000 (`cursor-auth.ts:71`).
        assert_eq!(value["exp"], 5_600);
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let token = minter()
            .mint_access(
                "acct_1",
                "sess_1",
                "a@b.c",
                "pro",
                chrono::Utc::now().timestamp(),
                3_600,
            )
            .unwrap();
        let other = TokenMinter::new(b"a-different-secret-entirely-here");
        assert!(other.verify_access(&token).is_err());
    }

    #[test]
    fn an_expired_token_is_refused() {
        // Minted so that it expired an hour ago.
        let now = chrono::Utc::now().timestamp();
        let token = minter()
            .mint_access("acct_1", "sess_1", "a@b.c", "pro", now - 7_200, 3_600)
            .unwrap();
        assert!(minter().verify_access(&token).is_err());
    }

    #[test]
    fn refresh_tokens_are_opaque_unique_and_hashed_stably() {
        let first = mint_refresh_token();
        let second = mint_refresh_token();
        assert_ne!(first, second);
        assert!(first.starts_with("ogr_"));
        // Not a JWT: nothing to decode, nothing to learn.
        assert!(!first.contains('.'));
        assert_eq!(hash_refresh_token(&first), hash_refresh_token(&first));
        assert_ne!(hash_refresh_token(&first), hash_refresh_token(&second));
        // A hash, not the token.
        assert!(!hash_refresh_token(&first).contains(&first));
    }

    /// The signing key must not be printable, however it is logged.
    #[test]
    fn the_minter_does_not_print_its_secret() {
        let printed = format!("{:?}", minter());
        assert!(!printed.contains("secret"), "{printed}");
        assert_eq!(printed, "TokenMinter(<redacted>)");
    }
}
