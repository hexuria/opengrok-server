use super::*;

fn google() -> ProviderConfig {
    ProviderConfig::google(
        "gmail",
        "client-123.apps.googleusercontent.com",
        "secret-abc",
        &["https://www.googleapis.com/auth/gmail.send"],
    )
}

// ---- the authorize URL ------------------------------------------------

/// The URL goes to a browser, so the secret must not be in it. This is the test that catches a
/// copy-paste of `client_secret` next to `client_id`.
#[test]
fn the_authorize_url_never_carries_the_client_secret() {
    let url = authorize_url(
        &google(),
        "https://og.example/connections/callback",
        "st",
        None,
    );
    assert!(!url.contains("secret-abc"), "{url}");
    assert!(url.contains("client_id=client-123"), "{url}");
}

/// Without `access_type=offline` Google issues no refresh token and the connection dies in an
/// hour.
#[test]
fn google_is_asked_for_offline_access() {
    let url = authorize_url(&google(), "https://og.example/cb", "st", None);
    assert!(url.contains("access_type=offline"), "{url}");
}

/// GitHub issues no refresh token regardless, so asking would be noise.
#[test]
fn github_is_not_asked_for_offline_access() {
    let config = ProviderConfig::github("id", "secret", &["repo"]);
    let url = authorize_url(&config, "https://og.example/cb", "st", None);
    assert!(!url.contains("access_type"), "{url}");
}

/// Space-separated and percent-encoded. Comma-separated is GitHub's old habit and Google
/// rejects it outright.
#[test]
fn scopes_are_space_separated_and_encoded() {
    let config = ProviderConfig::google("gdrive", "id", "s", &["a/scope.one", "b/scope.two"]);
    let url = authorize_url(&config, "https://og.example/cb", "st", None);
    assert!(url.contains("scope=a%2Fscope.one%20b%2Fscope.two"), "{url}");
}

/// A redirect URI must arrive byte-identical to the registration, which means encoded.
#[test]
fn the_redirect_uri_is_encoded_whole() {
    let url = authorize_url(
        &google(),
        "https://og.example/connections/callback",
        "st",
        None,
    );
    assert!(
        url.contains("redirect_uri=https%3A%2F%2Fog.example%2Fconnections%2Fcallback"),
        "{url}"
    );
}

#[test]
fn pkce_is_sent_as_s256_when_used() {
    let pkce = Pkce::new("a-verifier-of-reasonable-length-0123456789");
    let url = authorize_url(&google(), "https://og.example/cb", "st", Some(&pkce));
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    assert!(url.contains("code_challenge="), "{url}");
    // The verifier stays here; only the challenge crosses the wire.
    assert!(!url.contains(&pkce.verifier), "{url}");
}

/// A provider whose authorize URL already has a query must not get a second `?`.
#[test]
fn an_authorize_url_with_an_existing_query_still_parses() {
    let mut config = google();
    config.authorize_url = "https://provider.example/auth?tenant=acme".to_string();
    let url = authorize_url(&config, "https://og.example/cb", "st", None);
    assert!(url.contains("auth?tenant=acme&client_id="), "{url}");
    assert_eq!(url.matches('?').count(), 1, "{url}");
}

// ---- token replies ----------------------------------------------------

#[test]
fn a_google_json_reply_parses() {
    let response = TokenResponse::parse(
        r#"{"access_token":"ya29.a0","expires_in":3599,"refresh_token":"1//refresh",
            "scope":"gmail.send","token_type":"Bearer"}"#,
    )
    .unwrap();
    assert_eq!(response.access_token, "ya29.a0");
    assert_eq!(response.refresh_token.as_deref(), Some("1//refresh"));
    assert_eq!(response.expires_in, Some(3599));
}

/// GitHub answers form-encoded unless asked otherwise. A client assuming JSON gets a parse
/// error where a token should be.
#[test]
fn a_github_form_encoded_reply_parses_too() {
    let response =
        TokenResponse::parse("access_token=gho_16C7e42F&scope=repo%2Cgist&token_type=bearer")
            .unwrap();
    assert_eq!(response.access_token, "gho_16C7e42F");
    assert_eq!(response.scope.as_deref(), Some("repo,gist"));
    assert_eq!(response.expires_in, None);
}

#[test]
fn a_reply_with_no_token_is_an_error_not_an_empty_token() {
    assert!(TokenResponse::parse("error=bad_verification_code").is_err());
    assert!(TokenResponse::parse("").is_err());
}

/// GitHub OAuth-app tokens do not expire. Treating that as "expired" would refresh them
/// forever against an endpoint that issues no refresh token.
#[test]
fn no_expiry_means_forever_not_already_expired() {
    let response = TokenResponse::parse("access_token=gho_x").unwrap();
    assert_eq!(response.expires_at_ms(1_000), None);
}

#[test]
fn an_expiry_is_computed_from_now() {
    let response = TokenResponse::parse(r#"{"access_token":"a","expires_in":3600}"#).unwrap();
    assert_eq!(response.expires_at_ms(1_000), Some(3_601_000));
}

/// THE GOOGLE TRAP. A re-authentication returns no refresh token, and overwriting the stored
/// one with `None` makes a working connection unrefreshable an hour later.
#[test]
fn a_reauthentication_keeps_the_refresh_token_we_already_had() {
    let second = TokenResponse::parse(r#"{"access_token":"new","expires_in":3599}"#).unwrap();
    assert_eq!(
        second
            .refresh_token_to_store(Some("1//original"))
            .as_deref(),
        Some("1//original"),
        "the original refresh token must survive a re-consent that omits one"
    );
}

#[test]
fn a_fresh_refresh_token_replaces_the_old_one() {
    let response =
        TokenResponse::parse(r#"{"access_token":"new","refresh_token":"1//newer"}"#).unwrap();
    assert_eq!(
        response
            .refresh_token_to_store(Some("1//original"))
            .as_deref(),
        Some("1//newer")
    );
}

#[test]
fn an_error_reply_names_the_reason() {
    let error: TokenError =
        serde_json::from_str(r#"{"error":"invalid_grant","error_description":"expired"}"#).unwrap();
    // `invalid_grant` on a refresh means the person revoked access — a disconnect, not a retry.
    assert_eq!(error.error, "invalid_grant");
    assert_eq!(error.error_description.as_deref(), Some("expired"));
}

// ---- encoding ---------------------------------------------------------

#[test]
fn encoding_round_trips_the_awkward_characters() {
    for value in ["a b", "a/b", "a+b", "a&b=c", "https://x/y?z=1"] {
        assert_eq!(decode(&encode(value)), value, "{value}");
    }
}
