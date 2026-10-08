#![allow(clippy::unwrap_used)]
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
