#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    let (state, _) = sign_state(&minter(), &claims(), now).unwrap();
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
    let (state, _) = sign_state(&theirs, &claims(), now).unwrap();
    assert!(matches!(
        verify_state(&minter(), &state),
        Err(FlowError::BadState)
    ));
}

#[test]
fn a_tampered_state_is_refused() {
    let now = chrono::Utc::now().timestamp();
    let (mut state, _) = sign_state(&minter(), &claims(), now).unwrap();
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
    let (state, _) = sign_state(&minter(), &claims(), long_ago).unwrap();
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
