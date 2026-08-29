//! Drives the OAuth round trip against a stand-in provider.
//!
//! The unit tests in `connections::oauth` prove we build the right URL and read the right reply.
//! These prove the two halves meet: a code from an authorize step is exchanged at a token endpoint
//! and comes back as a usable token. That gap is where an OAuth integration actually fails — a
//! `redirect_uri` that differs by a slash, a form field the provider does not read, an `Accept`
//! header somebody dropped.
//!
//! IT IS A STAND-IN, NOT GOOGLE. It behaves the way Google and GitHub are documented to behave,
//! including the two habits that cost people a day — Google issuing a refresh token only on first
//! consent, GitHub replying form-encoded — so those paths are exercised without an account. What it
//! cannot prove is that Google agrees with its own documentation; that needs the operator's app
//! registration, and is the only claim held back.

#![allow(clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Form, Router};
use opengrok_server::auth::token::TokenMinter;
use opengrok_server::connections::flow::{exchange_code, refresh, sign_state, verify_state};
use opengrok_server::connections::oauth::{ProviderConfig, StateClaims, authorize_url};
use serde::Deserialize;

/// What the stand-in has been asked, so a test can assert on the request and not only the reply.
/// One recorded request's form fields.
type Recorded = Vec<(String, String)>;

#[derive(Debug, Default, Clone)]
struct Seen {
    forms: Arc<Mutex<Vec<Recorded>>>,
    accepts: Arc<Mutex<Vec<String>>>,
    /// Set once a code has been redeemed. Every provider treats a code as single-use.
    spent: Arc<Mutex<Vec<String>>>,
}

impl Seen {
    fn last_form(&self) -> Recorded {
        self.forms
            .lock()
            .ok()
            .and_then(|forms| forms.last().cloned())
            .unwrap_or_default()
    }

    fn field(&self, key: &str) -> Option<String> {
        self.last_form()
            .into_iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    fn last_accept(&self) -> Option<String> {
        self.accepts.lock().ok().and_then(|a| a.last().cloned())
    }
}

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    state: String,
    #[serde(default)]
    redirect_uri: String,
}

/// How the stand-in should behave — one flag per real-provider habit.
#[derive(Debug, Clone, Copy)]
struct Behaviour {
    /// Google: a refresh token on the first consent and never again.
    refresh_token_first_time_only: bool,
    /// GitHub: form-encoded unless `Accept: application/json`.
    form_encoded: bool,
    /// GitHub: no expiry at all.
    omit_expiry: bool,
}

impl Behaviour {
    fn google() -> Self {
        Self {
            refresh_token_first_time_only: true,
            form_encoded: false,
            omit_expiry: false,
        }
    }
    fn github() -> Self {
        Self {
            refresh_token_first_time_only: false,
            form_encoded: true,
            omit_expiry: true,
        }
    }
}

async fn start_provider(behaviour: Behaviour) -> (String, Seen) {
    let seen = Seen::default();
    let issued = Arc::new(Mutex::new(0_usize));

    let app = Router::new()
        .route(
            "/authorize",
            axum::routing::get(|Query(query): Query<AuthorizeQuery>| async move {
                // A real provider bounces the browser back with the state untouched and a code.
                axum::response::Redirect::temporary(&format!(
                    "{}?code=the-code&state={}",
                    query.redirect_uri, query.state
                ))
            }),
        )
        .route(
            "/token",
            post(
                move |State((seen, issued, behaviour)): State<(Seen, Arc<Mutex<usize>>, Behaviour)>,
                      headers: axum::http::HeaderMap,
                      Form(form): Form<Vec<(String, String)>>| async move {
                    if let Ok(mut forms) = seen.forms.lock() {
                        forms.push(form.clone());
                    }
                    if let Ok(mut accepts) = seen.accepts.lock() {
                        accepts.push(
                            headers
                                .get(axum::http::header::ACCEPT)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }

                    let field = |key: &str| {
                        form.iter()
                            .find(|(name, _)| name == key)
                            .map(|(_, value)| value.clone())
                    };

                    // Codes are single-use everywhere. A replayed callback must fail here.
                    if field("grant_type").as_deref() == Some("authorization_code") {
                        let code = field("code").unwrap_or_default();
                        let mut spent = seen.spent.lock().expect("lock");
                        if spent.contains(&code) {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                r#"{"error":"invalid_grant","error_description":"code already used"}"#,
                            )
                                .into_response();
                        }
                        spent.push(code);
                    }

                    let count = {
                        let mut issued = issued.lock().expect("lock");
                        *issued += 1;
                        *issued
                    };

                    let include_refresh =
                        !behaviour.refresh_token_first_time_only || count == 1;
                    let refresh_part = if include_refresh { Some("1//refresh-token") } else { None };

                    if behaviour.form_encoded {
                        let mut body = format!("access_token=token-{count}&token_type=bearer");
                        if let Some(refresh) = refresh_part {
                            body.push_str(&format!("&refresh_token={refresh}"));
                        }
                        if !behaviour.omit_expiry {
                            body.push_str("&expires_in=3599");
                        }
                        return (
                            [(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")],
                            body,
                        )
                            .into_response();
                    }

                    let mut json = serde_json::json!({
                        "access_token": format!("token-{count}"),
                        "token_type": "Bearer",
                    });
                    if let Some(refresh) = refresh_part {
                        json["refresh_token"] = serde_json::json!(refresh);
                    }
                    if !behaviour.omit_expiry {
                        json["expires_in"] = serde_json::json!(3599);
                    }
                    axum::Json(json).into_response()
                },
            ),
        )
        .with_state((seen.clone(), issued, behaviour));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("read the address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), seen)
}

fn config_for(base: &str, behaviour_is_github: bool) -> ProviderConfig {
    let mut config = if behaviour_is_github {
        ProviderConfig::github("client-id", "client-secret", &["repo"])
    } else {
        ProviderConfig::google("gmail", "client-id", "client-secret", &["gmail.send"])
    };
    config.authorize_url = format!("{base}/authorize");
    config.token_url = format!("{base}/token");
    config
}

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

/// The whole round trip: authorize, follow the redirect, verify the state, exchange the code.
#[tokio::test]
async fn a_person_authorises_and_we_get_a_usable_token() {
    let (base, seen) = start_provider(Behaviour::google()).await;
    let config = config_for(&base, false);
    let redirect_uri = format!("{base}/landing");

    let now = chrono::Utc::now().timestamp();
    let state = sign_state(&minter(), &claims(), now).expect("sign the state");
    let url = authorize_url(&config, &redirect_uri, &state, None);

    // Follow it the way a browser would, without following the final redirect.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let response = http.get(&url).send().await.expect("authorize");
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("a redirect back to us")
        .to_string();

    // The provider hands the state back untouched; we verify it rather than trusting the query.
    let returned = url_param(&location, "state").expect("state comes back");
    let verified = verify_state(&minter(), &returned).expect("our own state verifies");
    assert_eq!(verified.sub, "acct_1");
    assert_eq!(verified.connector, "gmail");

    let code = url_param(&location, "code").expect("a code comes back");
    let token = exchange_code(&reqwest::Client::new(), &config, &redirect_uri, &code, None)
        .await
        .expect("exchange the code");

    assert_eq!(token.access_token, "token-1");
    assert_eq!(token.refresh_token.as_deref(), Some("1//refresh-token"));
    assert!(token.expires_at_ms(0).is_some());

    // The secret went in the body, never the URL.
    assert_eq!(
        seen.field("client_secret").as_deref(),
        Some("client-secret")
    );
    assert!(!url.contains("client-secret"), "{url}");
    // And the redirect_uri was echoed exactly, which is what providers check.
    assert_eq!(
        seen.field("redirect_uri").as_deref(),
        Some(redirect_uri.as_str())
    );
}

/// THE GOOGLE TRAP, END TO END. The second consent returns no refresh token, and the stored one
/// must survive — otherwise a working connection becomes unrefreshable an hour later.
#[tokio::test]
async fn a_second_consent_returns_no_refresh_token_and_the_first_survives() {
    let (base, _) = start_provider(Behaviour::google()).await;
    let config = config_for(&base, false);
    let http = reqwest::Client::new();
    let redirect_uri = format!("{base}/landing");

    let first = exchange_code(&http, &config, &redirect_uri, "code-one", None)
        .await
        .expect("first exchange");
    let stored = first
        .refresh_token
        .clone()
        .expect("a refresh token first time");

    let second = exchange_code(&http, &config, &redirect_uri, "code-two", None)
        .await
        .expect("second exchange");
    assert_eq!(
        second.refresh_token, None,
        "Google omits it the second time"
    );

    assert_eq!(
        second.refresh_token_to_store(Some(&stored)).as_deref(),
        Some("1//refresh-token"),
        "the original must be kept rather than overwritten with nothing"
    );
}

/// GitHub answers form-encoded, and its tokens do not expire.
#[tokio::test]
async fn a_form_encoded_provider_is_read_correctly() {
    let (base, seen) = start_provider(Behaviour::github()).await;
    let config = config_for(&base, true);

    let token = exchange_code(
        &reqwest::Client::new(),
        &config,
        &format!("{base}/landing"),
        "the-code",
        None,
    )
    .await
    .expect("exchange");

    assert_eq!(token.access_token, "token-1");
    // No expiry means forever, not already expired.
    assert_eq!(token.expires_at_ms(1_000), None);
    // And we did ask for JSON, even though this provider ignored us.
    assert_eq!(seen.last_accept().as_deref(), Some("application/json"));
}

/// A code is single-use at every provider, which is what bounds state replay.
#[tokio::test]
async fn a_replayed_code_is_refused_by_the_provider() {
    let (base, _) = start_provider(Behaviour::google()).await;
    let config = config_for(&base, false);
    let http = reqwest::Client::new();
    let redirect_uri = format!("{base}/landing");

    exchange_code(&http, &config, &redirect_uri, "only-once", None)
        .await
        .expect("first use works");

    let error = exchange_code(&http, &config, &redirect_uri, "only-once", None)
        .await
        .expect_err("a second use must fail");
    // And it is recognised as revocation rather than something to retry.
    assert!(error.is_revoked(), "{error}");
}

#[tokio::test]
async fn a_refresh_returns_a_new_access_token() {
    let (base, seen) = start_provider(Behaviour::google()).await;
    let config = config_for(&base, false);

    let token = refresh(&reqwest::Client::new(), &config, "1//refresh-token")
        .await
        .expect("refresh");

    assert!(token.access_token.starts_with("token-"));
    assert_eq!(seen.field("grant_type").as_deref(), Some("refresh_token"));
    assert_eq!(
        seen.field("refresh_token").as_deref(),
        Some("1//refresh-token")
    );
}

/// A state we did not sign must not be honoured, however it arrives.
#[tokio::test]
async fn a_state_from_somebody_else_is_refused_at_the_callback() {
    let theirs = TokenMinter::new(b"an-attackers-entirely-different-key");
    let forged = sign_state(&theirs, &claims(), chrono::Utc::now().timestamp()).expect("sign");
    assert!(verify_state(&minter(), &forged).is_err());
}

fn url_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            )
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
