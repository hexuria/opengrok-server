//! OAuth 2.0, as the providers actually behave.
//!
//! Everything here is pure: build a URL, sign a state, parse a reply. The HTTP calls live in
//! `flow.rs` so the parts that are easy to get wrong can be tested without a network, which is what
//! makes the provider quirks below assertable rather than folklore.
//!
//! FOUR PROVIDER BEHAVIOURS THAT COST PEOPLE A DAY EACH, ENCODED HERE AS TESTS:
//!
//! 1. **Google issues a refresh token only once.** Without `access_type=offline` you never get one;
//!    *with* it you get one on the FIRST consent and never again — so a re-authentication silently
//!    returns none and the stored refresh token must be kept rather than overwritten with `None`.
//!    `prompt=consent` forces a fresh one when you genuinely need it.
//! 2. **GitHub replies form-encoded unless asked otherwise.** `Accept: application/json` is not
//!    optional; without it the "JSON" you parse is `access_token=gho_…&scope=&token_type=bearer`.
//! 3. **GitHub OAuth apps issue no refresh token at all.** Their tokens do not expire, so an
//!    absent `expires_in` means "forever", not "already expired".
//! 4. **`redirect_uri` must match the registration byte for byte** — a trailing slash is a
//!    different URI, and the error the provider returns says nothing useful about which.
//!
//! THE STATE IS SIGNED, AND THAT IS THE WHOLE OF THE CSRF DEFENCE. An unsigned `state` lets anybody
//! hand a person a callback URL that attaches *the attacker's* account to *the victim's* session.
//! It is a JWT here so it carries claims and an expiry rather than being an opaque nonce we would
//! have to store and reap.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How to talk to one provider. Read from configuration, never hardcoded, because the client id
/// and secret are per deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// The connector this configures: `gmail`, `github`, `gdrive`.
    pub connector: String,
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    /// Never leaves the server, and never appears in an authorize URL.
    pub client_secret: String,
    pub scopes: Vec<String>,
    /// Google needs `access_type=offline` to issue a refresh token at all. GitHub OAuth apps
    /// ignore it and issue none regardless.
    #[serde(default)]
    pub offline: bool,
    /// Anything else a provider wants on the authorize URL.
    #[serde(default)]
    pub extra_authorize_params: BTreeMap<String, String>,
}

impl ProviderConfig {
    /// Google, with the parameters that actually produce a refresh token.
    pub fn google(connector: &str, client_id: &str, client_secret: &str, scopes: &[&str]) -> Self {
        Self {
            connector: connector.to_string(),
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token_url: "https://oauth2.googleapis.com/token".to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            offline: true,
            extra_authorize_params: BTreeMap::new(),
        }
    }

    pub fn github(client_id: &str, client_secret: &str, scopes: &[&str]) -> Self {
        Self {
            connector: "github".to_string(),
            authorize_url: "https://github.com/login/oauth/authorize".to_string(),
            token_url: "https://github.com/login/oauth/access_token".to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            // GitHub OAuth apps issue no refresh token whatever you ask for.
            offline: false,
            extra_authorize_params: BTreeMap::new(),
        }
    }
}

/// What the signed `state` carries across the round trip.
///
/// Every field is something the callback must not take from the query string: an attacker controls
/// the query string, and taking the account from it is the whole vulnerability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateClaims {
    /// The account that started this. `sub`, so it reads like the access token's claims.
    pub sub: String,
    pub connector: String,
    /// `user` or `bot` — which scope the resulting connection gets.
    pub scope: String,
    /// Set when the connection is to belong to a coworker rather than the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coworker: Option<String>,
    /// Fresh per attempt, so two tabs do not collide.
    pub nonce: String,
    /// Seconds. Short — a state that outlives the walk to the provider and back is a state
    /// somebody can sit on.
    pub exp: i64,
}

/// How long a state is good for. Long enough to read a consent screen, short enough that a URL
/// copied out of a browser history is useless.
pub const STATE_TTL_SECONDS: i64 = 600;

/// The PKCE pair. Optional for a confidential client, included because it costs almost nothing and
/// removes a whole class of interception attack from the callback.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// S256, which is the only method worth using; `plain` exists in the spec and defeats the point.
    pub fn new(verifier: impl Into<String>) -> Self {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let verifier = verifier.into();
        let digest = Sha256::digest(verifier.as_bytes());
        Self {
            challenge: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest),
            verifier,
        }
    }
}

/// Build the URL a person is sent to.
///
/// The client secret is not here and must never be: this URL is handed to a browser.
pub fn authorize_url(
    config: &ProviderConfig,
    redirect_uri: &str,
    state: &str,
    pkce: Option<&Pkce>,
) -> String {
    let mut params: Vec<(String, String)> = vec![
        ("client_id".to_string(), config.client_id.clone()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("response_type".to_string(), "code".to_string()),
        // Space-separated, then percent-encoded. Comma-separated is GitHub's older habit and
        // Google rejects it.
        ("scope".to_string(), config.scopes.join(" ")),
        ("state".to_string(), state.to_string()),
    ];

    if config.offline {
        // Without this Google issues no refresh token at all, and the connection dies in an hour.
        params.push(("access_type".to_string(), "offline".to_string()));
    }
    if let Some(pkce) = pkce {
        params.push(("code_challenge".to_string(), pkce.challenge.clone()));
        params.push(("code_challenge_method".to_string(), "S256".to_string()));
    }
    for (key, value) in &config.extra_authorize_params {
        params.push((key.clone(), value.clone()));
    }

    let query = params
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&");

    let separator = if config.authorize_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{separator}{query}", config.authorize_url)
}

/// What a provider sends back when a code is exchanged.
///
/// `expires_in` and `refresh_token` are both optional, and their absence means different things per
/// provider — see the module note.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

/// A provider's error reply. Worth parsing rather than dumping: `invalid_grant` on a refresh means
/// "the person revoked this", which is a disconnect, not a retry.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenError {
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
}

impl TokenResponse {
    /// Parse a token reply from either dialect.
    ///
    /// GitHub answers `application/x-www-form-urlencoded` unless asked for JSON, and a client that
    /// assumes JSON gets a parse error instead of a token. We always send `Accept: application/json`
    /// — this exists because "always" lasts until somebody's proxy strips the header.
    pub fn parse(body: &str) -> Result<Self, String> {
        let trimmed = body.trim_start();
        if trimmed.starts_with('{') {
            return serde_json::from_str(trimmed).map_err(|error| error.to_string());
        }

        // Form-encoded fallback.
        let mut response = Self::default();
        for pair in trimmed.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let value = decode(value);
            match key {
                "access_token" => response.access_token = value,
                "refresh_token" => response.refresh_token = Some(value),
                "expires_in" => response.expires_in = value.parse().ok(),
                "scope" => response.scope = Some(value),
                "token_type" => response.token_type = Some(value),
                _ => {}
            }
        }
        if response.access_token.is_empty() {
            return Err(format!("no access_token in the reply: {trimmed}"));
        }
        Ok(response)
    }

    /// When this token stops working, in epoch milliseconds.
    ///
    /// NO `expires_in` MEANS FOREVER, NOT EXPIRED. GitHub OAuth-app tokens carry no expiry, and
    /// treating absence as zero would refresh them constantly against an endpoint that issues no
    /// refresh token.
    pub fn expires_at_ms(&self, now_ms: i64) -> Option<i64> {
        self.expires_in.map(|seconds| now_ms + seconds * 1_000)
    }

    /// The refresh token to store, given what we already had.
    ///
    /// GOOGLE SENDS ONE ONLY ON THE FIRST CONSENT. Overwriting a stored refresh token with the
    /// `None` of a later exchange is how a working connection quietly becomes unrefreshable an hour
    /// later.
    pub fn refresh_token_to_store(&self, existing: Option<&str>) -> Option<String> {
        self.refresh_token
            .clone()
            .or_else(|| existing.map(str::to_string))
    }
}

/// Percent-encode a query value. Small and explicit rather than a dependency for one function.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
            serde_json::from_str(r#"{"error":"invalid_grant","error_description":"expired"}"#)
                .unwrap();
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
}
