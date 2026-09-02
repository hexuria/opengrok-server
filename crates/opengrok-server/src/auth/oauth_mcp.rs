//! The MCP door's OAuth 2.1 authorization server — "mint a bot key from the browser".
//!
//! WHY WE ARE AN AUTHORIZATION SERVER AT ALL. Claude Code's MCP client runs an OAuth flow when a
//! `/mcp` 401 carries `resource_metadata` (MCP authorization, protocol revision 2026-07-28); with
//! no server answering, that client fails where the static header used to work. Metadata alone
//! is therefore worse than nothing. The pieces an authorization server needs already exist here —
//! accounts, a browser credential page, a signing key, and bot keys with a revocation row — so
//! the token this server issues **is a bot key**: the same credential `POST /coworkers/{id}/keys`
//! mints, bound to one account and one coworker, revocable from the coworker's key list. The
//! flow adds no authority; it replaces `curl` + paste with a browser tab.
//!
//! WHAT IS IMPLEMENTED, AND WHAT THE SPECS SAY (`docs/plan-slice16-later.md` §2.1, cited):
//! - RFC 9728 protected-resource metadata, served at both the path-suffixed
//!   (`/.well-known/oauth-protected-resource/mcp`) and root forms — clients probe both.
//! - RFC 8414 authorization-server metadata at `/.well-known/oauth-authorization-server`.
//! - RFC 7591 dynamic client registration for PUBLIC clients (no secret): Claude Code speaks it
//!   by default. Client ID Metadata Documents (the spec's SHOULD) are the follow-up.
//! - PKCE S256 only; `resource` (RFC 8707) required on both legs and must be OUR `/mcp`; the
//!   token carries it as `aud`, and the door refuses a key minted for another server.
//! - `iss` on the authorization response (RFC 9207), advertised in the metadata.
//! - No refresh tokens in v1: the key lives 90 days (`OAUTH_KEY_TTL_SECS`), and on its 401
//!   Claude Code re-runs the browser flow — the right behaviour for a key that leaked or a
//!   machine that changed hands.
//!
//! THE ENDPOINTS LIVE UNDER `/oauth/mcp/*`, NOT `/oauth/token`. That path is the desktop client's
//! refresh (`cursor-auth.ts:450`, JSON `{client_id, grant_type: "refresh_token", refresh_token}`);
//! an OAuth 2.1 token request is form-encoded with `grant_type=authorization_code`. RFC 8414 lets
//! the metadata name any token endpoint and Claude Code follows it, so two unrelated contracts
//! never share a path.
//!
//! CODES ARE IN MEMORY, CLIENTS ARE IN POSTGRES. A code is a ten-minute one-shot bound to the
//! client, redirect, PKCE challenge, resource and the chosen coworker — the same bargain
//! `loginDeepControl`'s pending logins make, and it moves to a table with them when replicas do.
//! A registration must survive a restart: Claude Code keeps its client_id and reports
//! "incompatible auth server" if it vanishes.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use opengrok_core::id::{AccountId, CoworkerId};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::routes::AuthState;

/// The one scope the door serves. Advertised in the 401 challenge and the metadata; the flow
/// grants exactly this.
pub const SCOPE: &str = "mcp:tools";

/// How long an OAuth-minted bot key authenticates: 90 days, then the browser flow again. Shorter
/// than a hand-minted key's ten years on purpose — this one was handed to a tool, not typed by a
/// person.
pub const OAUTH_KEY_TTL_SECS: i64 = 90 * 24 * 60 * 60;

const CODE_TTL_MS: i64 = 10 * 60 * 1_000;
const CONSENT_TTL_SECS: i64 = 10 * 60;
/// Registrations one peer address may make in an hour, and the table's ceiling: dynamic client
/// registration is unauthenticated by design, so this is what keeps it from filling the database.
const DCR_PER_HOUR: usize = 20;
const DCR_CEILING: i64 = 1_000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn base(public_url: &str) -> String {
    public_url.trim_end_matches('/').to_string()
}

/// The canonical resource identifier of the MCP door (RFC 8707 §2): the public address plus
/// `/mcp`, no trailing slash. What the client must send as `resource`, and what a minted key's
/// `aud` says.
pub fn resource_uri(public_url: &str) -> String {
    format!("{}/mcp", base(public_url))
}

/// Where the protected-resource metadata lives, for the door's 401 challenge.
pub fn protected_resource_metadata_url(public_url: &str) -> String {
    format!(
        "{}/.well-known/oauth-protected-resource/mcp",
        base(public_url)
    )
}

/// An authorization code waiting to be exchanged.
#[derive(Debug, Clone)]
pub struct PendingCode {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub resource: String,
    pub account: AccountId,
    pub coworker: CoworkerId,
    pub at_ms: i64,
}

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/oauth/mcp/register", post(register))
        .route(
            "/oauth/mcp/authorize",
            get(authorize_page).post(authorize_submit),
        )
        .route("/oauth/mcp/token", post(token))
        .with_state(state)
}

// ── Discovery ────────────────────────────────────────────────────────────────────────────────

/// RFC 9728. Both forms of the path answer the same document.
async fn protected_resource(State(state): State<AuthState>) -> Response {
    let public = base(&state.public_url);
    Json(json!({
        "resource": resource_uri(&public),
        "authorization_servers": [public],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [SCOPE],
        "resource_name": "Open Grok MCP door",
    }))
    .into_response()
}

/// RFC 8414. The issuer is the public address; the endpoints are ours under `/oauth/mcp/`.
async fn server_metadata(State(state): State<AuthState>) -> Response {
    let public = base(&state.public_url);
    Json(json!({
        "issuer": public,
        "authorization_endpoint": format!("{public}/oauth/mcp/authorize"),
        "token_endpoint": format!("{public}/oauth/mcp/token"),
        "registration_endpoint": format!("{public}/oauth/mcp/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE],
        "authorization_response_iss_parameter_supported": true,
    }))
    .into_response()
}

// ── Registration (RFC 7591) ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RegistrationRequest {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

/// A redirect the flow will send a code to: the loopback callback Claude Code opens
/// (`http://localhost:PORT/callback`, `127.0.0.1` too), or anything `https://`. Nothing else —
/// an `http://` host on the network would carry the code in the clear.
fn redirect_allowed(uri: &str) -> bool {
    if uri.starts_with("https://") {
        return true;
    }
    let Some(rest) = uri.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    host == "localhost" || host == "127.0.0.1" || host == "[::1]"
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

fn peer_of(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// `POST /oauth/mcp/register` — a public client registers its callback and name.
async fn register(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(request): Json<RegistrationRequest>,
) -> Response {
    // The cap: per peer per hour, and a ceiling on the table.
    let peer = peer_of(&headers);
    let at_ms = now_ms();
    let over = state.dcr_hits.lock().map(|mut hits| {
        let list = hits.entry(peer.clone()).or_default();
        list.retain(|t| at_ms - *t < 60 * 60 * 1_000);
        if list.len() >= DCR_PER_HOUR {
            true
        } else {
            list.push(at_ms);
            false
        }
    });
    if over.unwrap_or(true) {
        return oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "invalid_client_metadata",
            "too many registrations from this address; try again later",
        );
    }
    match state.store.oauth_client_count().await {
        Ok(count) if count < DCR_CEILING => {}
        _ => {
            return oauth_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "invalid_client_metadata",
                "this server is not accepting new client registrations",
            );
        }
    }

    if request.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris is required",
        );
    }
    if let Some(bad) = request
        .redirect_uris
        .iter()
        .find(|uri| !redirect_allowed(uri))
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!(
                "{bad} is not allowed: use http://localhost:<port>/... or an https:// address"
            ),
        );
    }
    if let Some(method) = request.token_endpoint_auth_method.as_deref()
        && method != "none"
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "only public clients (token_endpoint_auth_method \"none\") are served",
        );
    }
    let client_name: String = request
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("MCP client")
        .chars()
        .take(120)
        .collect();
    let client = opengrok_store::OAuthClient {
        client_id: format!("mc_{}", uuid::Uuid::now_v7().simple()),
        client_name,
        redirect_uris: request.redirect_uris,
        created_at_ms: at_ms,
    };
    if let Err(error) = state.store.insert_oauth_client(&client).await {
        tracing::error!(%error, "could not record an OAuth client registration");
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "could not record the registration",
        );
    }
    tracing::info!(client_id = %client.client_id, peer, "mcp oauth: client registered");
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "client_id": client.client_id,
            "client_id_issued_at": at_ms / 1_000,
            "client_name": client.client_name,
            "redirect_uris": client.redirect_uris,
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "scope": SCOPE,
        })),
    )
        .into_response()
}

// ── Authorization ────────────────────────────────────────────────────────────────────────────

/// The authorization request, as it arrives on GET and is carried through the two POSTs. Every
/// field is re-validated on each leg: the browser is not trusted to keep them intact.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthorizeRequest {
    #[serde(default)]
    pub response_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub resource: String,
}

impl AuthorizeRequest {
    /// The hidden inputs that carry the request through the sign-in and consent POSTs.
    fn hidden(&self) -> String {
        let e = super::pages::escape;
        let mut fields = vec![
            ("response_type", self.response_type.clone()),
            ("client_id", self.client_id.clone()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("code_challenge", self.code_challenge.clone()),
            ("code_challenge_method", self.code_challenge_method.clone()),
            ("resource", self.resource.clone()),
        ];
        if let Some(state) = &self.state {
            fields.push(("state", state.clone()));
        }
        if let Some(scope) = &self.scope {
            fields.push(("scope", scope.clone()));
        }
        fields
            .into_iter()
            .map(|(name, value)| format!("<input type=hidden name={name} value=\"{}\">", e(&value)))
            .collect::<Vec<_>>()
            .join("\n  ")
    }
}

/// What a bad authorization request gets. `Page` when the client or redirect cannot be trusted
/// (RFC 6749 §4.1.2.1: never redirect to an unregistered URI); `Redirect` with the OAuth error
/// otherwise, so the client learns what was wrong.
enum AuthorizeRefusal {
    Page(String),
    Redirect(String),
}

/// Validate the request against the registered client. Returns the client on success.
async fn validate_authorize(
    state: &AuthState,
    request: &AuthorizeRequest,
) -> Result<opengrok_store::OAuthClient, AuthorizeRefusal> {
    let Ok(Some(client)) = state.store.oauth_client(&request.client_id).await else {
        return Err(AuthorizeRefusal::Page(
            "This tool is not registered with this server (unknown client_id). Register it \
             again from the tool."
                .to_string(),
        ));
    };
    if !client
        .redirect_uris
        .iter()
        .any(|uri| uri == &request.redirect_uri)
    {
        return Err(AuthorizeRefusal::Page(
            "The tool asked to be sent back to an address it did not register.".to_string(),
        ));
    }
    let err = |kind: &str, why: &str| {
        AuthorizeRefusal::Redirect(error_redirect(
            &request.redirect_uri,
            kind,
            why,
            request.state.as_deref(),
            &state.public_url,
        ))
    };
    if request.response_type != "code" {
        return Err(err(
            "unsupported_response_type",
            "only response_type=code is supported",
        ));
    }
    if request.code_challenge_method != "S256" || request.code_challenge.is_empty() {
        return Err(err(
            "invalid_request",
            "PKCE is required: send code_challenge with code_challenge_method=S256",
        ));
    }
    if request.resource != resource_uri(&state.public_url) {
        return Err(err(
            "invalid_target",
            &format!(
                "resource must be this server's MCP door, {}",
                resource_uri(&state.public_url)
            ),
        ));
    }
    if let Some(scope) = request.scope.as_deref()
        && scope.split_whitespace().any(|s| s != SCOPE)
    {
        return Err(err(
            "invalid_scope",
            &format!("the only scope served is {SCOPE}"),
        ));
    }
    Ok(client)
}

fn error_redirect(
    redirect_uri: &str,
    error: &str,
    description: &str,
    oauth_state: Option<&str>,
    public_url: &str,
) -> String {
    let mut params = vec![
        ("error", error.to_string()),
        ("error_description", description.to_string()),
        ("iss", base(public_url)),
    ];
    if let Some(s) = oauth_state {
        params.push(("state", s.to_string()));
    }
    with_query(redirect_uri, &params)
}

fn with_query(redirect_uri: &str, params: &[(&str, String)]) -> String {
    let query: String = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let joiner = if redirect_uri.contains('?') { '&' } else { '?' };
    format!("{redirect_uri}{joiner}{query}")
}

/// Percent-encode a query value (RFC 3986 unreserved characters pass through).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn refusal_response(refusal: AuthorizeRefusal) -> Response {
    match refusal {
        AuthorizeRefusal::Page(message) => {
            super::pages::message(StatusCode::BAD_REQUEST, "Cannot continue", &message)
        }
        AuthorizeRefusal::Redirect(location) => Redirect::to(&location).into_response(),
    }
}

/// The signed proof that a person authenticated for THIS consent — carried in the consent form
/// so the coworker choice is bound to the account without a console session.
#[derive(Debug, Serialize, Deserialize)]
struct ConsentClaims {
    #[serde(rename = "use")]
    purpose: String,
    sub: String,
    exp: i64,
}

const CONSENT_PURPOSE: &str = "mcp-consent";

fn mint_consent(state: &AuthState, account: &AccountId) -> Option<String> {
    state
        .minter
        .mint_claims(&ConsentClaims {
            purpose: CONSENT_PURPOSE.to_string(),
            sub: account.as_str().to_string(),
            exp: chrono::Utc::now().timestamp() + CONSENT_TTL_SECS,
        })
        .ok()
}

fn account_from_consent(state: &AuthState, token: &str) -> Option<AccountId> {
    let claims = state.minter.verify_claims::<ConsentClaims>(token).ok()?;
    (claims.purpose == CONSENT_PURPOSE).then(|| AccountId::from_stored(claims.sub))
}

/// The signed-in person behind the console's cookie, if any.
fn account_from_cookie(state: &AuthState, headers: &HeaderMap) -> Option<AccountId> {
    let token = super::cookies::read_cookie(headers, super::cookies::ACCESS_COOKIE)?;
    let claims = state.minter.verify_access(&token).ok()?;
    Some(AccountId::from_stored(claims.sub))
}

/// The person's own coworkers, as (id, name) — the only ones a consent page offers (plan §6 Q2:
/// an org admin's roster comes with SSO).
async fn own_coworkers(state: &AuthState, account: &AccountId) -> Vec<(String, String)> {
    state
        .store
        .coworkers_for(account)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|view| !view.retired)
        .map(|view| (view.id.as_str().to_string(), view.name))
        .collect()
}

/// `GET /oauth/mcp/authorize` — validate, then either the consent card (cookie session) or the
/// sign-in card.
async fn authorize_page(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Query(request): Query<AuthorizeRequest>,
) -> Response {
    let client = match validate_authorize(&state, &request).await {
        Ok(client) => client,
        Err(refusal) => return refusal_response(refusal),
    };
    if let Some(account) = account_from_cookie(&state, &headers)
        && let Some(consent) = mint_consent(&state, &account)
    {
        let coworkers = own_coworkers(&state, &account).await;
        return super::pages::oauth_consent(
            &client.client_name,
            &request.hidden(),
            &consent,
            &coworkers,
        );
    }
    super::pages::oauth_login(&client.client_name, &request.hidden(), None)
}

#[derive(Debug, Deserialize)]
struct AuthorizeForm {
    #[serde(flatten)]
    request: AuthorizeRequest,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    consent: Option<String>,
    #[serde(default)]
    coworker: Option<String>,
}

/// `POST /oauth/mcp/authorize` — two legs on one path. Credentials ⇒ authenticate and show the
/// consent card; consent + coworker ⇒ issue the code and send the browser back to the tool.
async fn authorize_submit(
    State(state): State<AuthState>,
    Form(form): Form<AuthorizeForm>,
) -> Response {
    let request = form.request;
    let client = match validate_authorize(&state, &request).await {
        Ok(client) => client,
        Err(refusal) => return refusal_response(refusal),
    };

    // Leg two: a consented choice.
    if let (Some(consent), Some(coworker)) = (form.consent.as_deref(), form.coworker.as_deref()) {
        let Some(account) = account_from_consent(&state, consent) else {
            return super::pages::oauth_login(
                &client.client_name,
                &request.hidden(),
                Some("That sign-in has expired. Sign in again."),
            );
        };
        let coworker_id = CoworkerId::from_stored(coworker.to_string());
        let owns = own_coworkers(&state, &account)
            .await
            .iter()
            .any(|(id, _)| id == coworker);
        if !owns {
            // Not theirs (or retired): the consent card is shown again with the real list.
            let coworkers = own_coworkers(&state, &account).await;
            return super::pages::oauth_consent(
                &client.client_name,
                &request.hidden(),
                consent,
                &coworkers,
            );
        }
        let code = format!("ac_{}", uuid::Uuid::now_v7().simple());
        let at_ms = now_ms();
        if let Ok(mut codes) = state.oauth_codes.lock() {
            codes.retain(|_, pending| at_ms - pending.at_ms < CODE_TTL_MS);
            codes.insert(
                code.clone(),
                PendingCode {
                    client_id: client.client_id.clone(),
                    client_name: client.client_name.clone(),
                    redirect_uri: request.redirect_uri.clone(),
                    code_challenge: request.code_challenge.clone(),
                    resource: request.resource.clone(),
                    account: account.clone(),
                    coworker: coworker_id.clone(),
                    at_ms,
                },
            );
        } else {
            return super::pages::message(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot continue",
                "Could not record the authorization. Try again.",
            );
        }
        tracing::info!(client_id = %client.client_id, account = %account, coworker = %coworker_id, "mcp oauth: consent given");
        let mut params = vec![("code", code), ("iss", base(&state.public_url))];
        if let Some(s) = &request.state {
            params.push(("state", s.clone()));
        }
        return Redirect::to(&with_query(&request.redirect_uri, &params)).into_response();
    }

    // Leg one: credentials.
    let (Some(email), Some(password)) = (form.email.as_deref(), form.password.as_deref()) else {
        return super::pages::oauth_login(&client.client_name, &request.hidden(), None);
    };
    match super::routes::authenticate(&state, email, password).await {
        Ok(view) => {
            let Some(consent) = mint_consent(&state, &view.id) else {
                return super::pages::message(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Cannot continue",
                    "Could not start the consent step.",
                );
            };
            let coworkers = own_coworkers(&state, &view.id).await;
            super::pages::oauth_consent(
                &client.client_name,
                &request.hidden(),
                &consent,
                &coworkers,
            )
        }
        Err((_, message)) => {
            super::pages::oauth_login(&client.client_name, &request.hidden(), Some(&message))
        }
    }
}

// ── Token ────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenForm {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    resource: Option<String>,
}

/// `code_challenge == base64url(sha256(code_verifier))`, no padding (RFC 7636 §4.6).
fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    // Constant-time: a challenge is not a secret, but the habit costs nothing.
    use subtle::ConstantTimeEq as _;
    expected.as_bytes().ct_eq(challenge.as_bytes()).into()
}

/// `POST /oauth/mcp/token` — the code, the verifier, the key.
async fn token(State(state): State<AuthState>, Form(form): Form<TokenForm>) -> Response {
    if form.grant_type != "authorization_code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only authorization_code is supported (no refresh tokens: sign in again when the key expires)",
        );
    }
    if form.code.is_empty() || form.code_verifier.is_empty() || form.client_id.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code, code_verifier and client_id are required",
        );
    }
    // TAKE the code: one exchange, ever. A second presentation — replay, retry — finds nothing.
    let pending = state
        .oauth_codes
        .lock()
        .ok()
        .and_then(|mut codes| codes.remove(&form.code));
    let Some(pending) = pending else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "unknown, expired or already used code",
        );
    };
    let at_ms = now_ms();
    if at_ms - pending.at_ms >= CODE_TTL_MS {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the code has expired",
        );
    }
    if pending.client_id != form.client_id {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "this code was issued to a different client",
        );
    }
    if pending.redirect_uri != form.redirect_uri {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "redirect_uri does not match the authorization request",
        );
    }
    if let Some(resource) = form.resource.as_deref()
        && resource != pending.resource
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource does not match the authorization request",
        );
    }
    if !pkce_matches(&form.code_verifier, &pending.code_challenge) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "code_verifier does not match the code_challenge",
        );
    }
    let label = format!("{} via OAuth", pending.client_name);
    let minted = match super::bot_keys::mint(
        &state.store,
        &state.minter,
        &pending.account,
        &pending.coworker,
        &label,
        Some(&pending.resource),
        OAUTH_KEY_TTL_SECS,
    )
    .await
    {
        Ok(minted) => minted,
        Err(error) => {
            tracing::error!(%error, "mcp oauth: could not mint the key");
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "could not mint the key",
            );
        }
    };
    tracing::info!(client_id = %pending.client_id, coworker = %pending.coworker, jti = %minted.jti, "mcp oauth: key issued");
    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(json!({
            "access_token": minted.token,
            "token_type": "Bearer",
            "expires_in": OAUTH_KEY_TTL_SECS,
            "scope": SCOPE,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_are_loopback_http_or_https_only() {
        assert!(redirect_allowed("http://localhost:8123/callback"));
        assert!(redirect_allowed("http://127.0.0.1:8123/callback"));
        assert!(redirect_allowed("https://tool.example/cb"));
        assert!(!redirect_allowed("http://tool.example/cb"));
        assert!(!redirect_allowed("http://localhost.evil.example/cb"));
        assert!(!redirect_allowed("ftp://localhost/cb"));
    }

    #[test]
    fn pkce_is_s256_of_the_verifier() {
        // RFC 7636 appendix B.
        assert!(pkce_matches(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!pkce_matches(
            "wrong",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(
            with_query("http://localhost:1/cb", &[("state", "a b&c".to_string())]),
            "http://localhost:1/cb?state=a%20b%26c"
        );
        assert_eq!(
            with_query("http://localhost:1/cb?x=1", &[("code", "ac_1".to_string())]),
            "http://localhost:1/cb?x=1&code=ac_1"
        );
    }
}
