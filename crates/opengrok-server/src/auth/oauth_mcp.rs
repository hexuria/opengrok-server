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
//!   by default. Client ID Metadata Documents (the spec's SHOULD,
//!   draft-ietf-oauth-client-id-metadata-document): a client id that is an https URL is fetched,
//!   must name itself as `client_id`, and its `redirect_uris` are the registration — no table
//!   row. Fetched documents are cached; errors and malformed documents never are; private and
//!   loopback addresses are never fetched (SSRF), except by a test that says so.
//! - PKCE S256 only; `resource` (RFC 8707) required on both legs and must be OUR `/mcp`; the
//!   token carries it as `aud`, and the door refuses a key minted for another server.
//! - `iss` on the authorization response (RFC 9207), advertised in the metadata.
//! - Refresh tokens: the access key lives a day (`ACCESS_TTL_SECS`); the refresh token — opaque,
//!   stored hashed — lives 90 days and is ROTATED on every use, the old access key revoked with
//!   it. Revoking the key from the coworker's list revokes its refresh tokens too. A revoked
//!   refresh token presented again is a replay: the whole family is revoked and logged.
//!
//! THE ENDPOINTS LIVE UNDER `/oauth/mcp/*`, NOT `/oauth/token`. That path is the desktop client's
//! refresh (`cursor-auth.ts:450`, JSON `{client_id, grant_type: "refresh_token", refresh_token}`);
//! an OAuth 2.1 token request is form-encoded with `grant_type=authorization_code`. RFC 8414 lets
//! the metadata name any token endpoint and Claude Code follows it, so two unrelated contracts
//! never share a path.
//!
//! CODES AND CLIENTS ARE BOTH IN POSTGRES, FOR DIFFERENT REASONS. A code is a ten-minute
//! one-shot bound to the client, redirect, PKCE challenge, resource and the chosen coworker; it
//! is a row (`oauth_code`, `opengrok_store::replica`) so the exchange can land on a replica
//! other than the one that gave consent, and `delete … returning` is what makes "once" true
//! across them. A registration must survive a restart: Claude Code keeps its client_id and
//! reports "incompatible auth server" if it vanishes.

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

/// How long an OAuth-minted access key authenticates before the client must refresh: a day.
/// Short on purpose — this one was handed to a tool, not typed by a person — and painless,
/// because the refresh below is silent.
pub const ACCESS_TTL_SECS: i64 = 24 * 60 * 60;

/// How long a refresh token lives: 90 days, then the browser flow again — the right cadence for
/// a key that leaked or a machine that changed hands.
pub const REFRESH_TTL_SECS: i64 = 90 * 24 * 60 * 60;

/// How long a fetched client id metadata document is trusted before it is fetched again.
const CIMD_CACHE_MS: i64 = 60 * 60 * 1_000;
/// The draft's recommended maximum size of a metadata document.
const CIMD_MAX_BYTES: usize = 5 * 1024;

const CODE_TTL_MS: i64 = 10 * 60 * 1_000;
const CONSENT_TTL_SECS: i64 = 10 * 60;
/// The table's ceiling: dynamic client registration is unauthenticated by design, so beyond the
/// per-address budget (`budget::CLIENT_REGISTRATION`) this is what keeps it from filling the
/// database.
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

/// An authorization code waiting to be exchanged — the `oauth_code` row with its ids typed.
#[derive(Debug, Clone)]
struct PendingCode {
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
        "grant_types_supported": ["authorization_code", "refresh_token"],
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

/// `POST /oauth/mcp/register` — a public client registers its callback and name.
async fn register(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(request): Json<RegistrationRequest>,
) -> Response {
    // The cap: per peer per hour, and a ceiling on the table.
    let at_ms = now_ms();
    let peer = super::budget::peer_key(&headers);
    if let Err(spent) = state
        .budgets
        .take(&super::budget::CLIENT_REGISTRATION, &peer)
    {
        return super::budget::with_retry_after(
            oauth_error(
                StatusCode::TOO_MANY_REQUESTS,
                "invalid_client_metadata",
                &format!(
                    "too many registrations from this address; try again in {} seconds",
                    spent.retry_after_secs
                ),
            ),
            spent,
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
            "grant_types": ["authorization_code", "refresh_token"],
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

/// A client id that is a URL, in the shape the draft requires: https, a path, no fragment, no
/// dot segments, no credentials. Loopback http only when a test allows it.
fn cimd_url_allowed(client_id: &str, allow_loopback: bool) -> bool {
    // The parser resolves dot segments away, so the RAW string is what the draft's rule is
    // checked against.
    if client_id.contains("/./") || client_id.contains("/../") || client_id.ends_with("/..") {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(client_id) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1";
    let private = host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
        || (host.starts_with("172.")
            && host
                .split('.')
                .nth(1)
                .and_then(|o| o.parse::<u8>().ok())
                .is_some_and(|o| (16..=31).contains(&o)));
    let scheme_ok =
        url.scheme() == "https" || (allow_loopback && loopback && url.scheme() == "http");
    scheme_ok
        && !host.is_empty()
        && (!loopback || allow_loopback)
        && !private
        && url.path().len() > 1
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

/// Fetch a client id metadata document and turn it into the same shape a registration row has.
/// `Err` is a sentence for the page; nothing about a failed or malformed fetch is cached.
async fn fetch_cimd(
    state: &AuthState,
    client_id: &str,
) -> Result<opengrok_store::OAuthClient, String> {
    let at_ms = now_ms();
    if let Some((client, fetched_at)) = state
        .cimd_cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(client_id).cloned())
        && at_ms - fetched_at < CIMD_CACHE_MS
    {
        return Ok(client);
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|error| format!("could not build a fetcher: {error}"))?;
    let response = http
        .get(client_id)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| format!("the tool's client id document could not be fetched: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "the tool's client id document answered {}",
            response.status()
        ));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("the tool's client id document could not be read: {error}"))?;
    if body.len() > CIMD_MAX_BYTES {
        return Err("the tool's client id document is larger than 5 KB".to_string());
    }
    let doc: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| "the tool's client id document is not JSON".to_string())?;
    // RFC 3986 §6.2.1 simple string comparison: the document must name itself, exactly.
    if doc.get("client_id").and_then(serde_json::Value::as_str) != Some(client_id) {
        return Err(
            "the tool's client id document does not name its own URL as client_id".to_string(),
        );
    }
    if let Some(method) = doc
        .get("token_endpoint_auth_method")
        .and_then(serde_json::Value::as_str)
        && method != "none"
    {
        return Err(
            "only public clients are served (token_endpoint_auth_method must be none)".to_string(),
        );
    }
    let redirect_uris: Vec<String> = doc
        .get("redirect_uris")
        .and_then(serde_json::Value::as_array)
        .map(|uris| {
            uris.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if redirect_uris.is_empty() || redirect_uris.iter().any(|uri| !redirect_allowed(uri)) {
        return Err(
            "the tool's client id document registers no acceptable redirect_uris".to_string(),
        );
    }
    let host = reqwest::Url::parse(client_id)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    // The hostname is shown with the name (draft §security: the person must see WHERE the
    // client comes from, not only what it calls itself).
    let name = doc
        .get("client_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| format!("{name} ({host})"))
        .unwrap_or(host);
    let client = opengrok_store::OAuthClient {
        client_id: client_id.to_string(),
        client_name: name.chars().take(160).collect(),
        redirect_uris,
        created_at_ms: at_ms,
    };
    if let Ok(mut cache) = state.cimd_cache.lock() {
        cache.insert(client_id.to_string(), (client.clone(), at_ms));
    }
    Ok(client)
}

/// The client behind a client id: a registered row, or a client id metadata document.
async fn resolve_client(
    state: &AuthState,
    client_id: &str,
) -> Result<opengrok_store::OAuthClient, String> {
    if let Ok(Some(client)) = state.store.oauth_client(client_id).await {
        return Ok(client);
    }
    if cimd_url_allowed(client_id, state.cimd_allow_loopback) {
        return fetch_cimd(state, client_id).await;
    }
    Err(
        "This tool is not registered with this server (unknown client_id). Register it again \
         from the tool."
            .to_string(),
    )
}

/// Validate the request against the client. Returns the client on success.
async fn validate_authorize(
    state: &AuthState,
    request: &AuthorizeRequest,
) -> Result<opengrok_store::OAuthClient, AuthorizeRefusal> {
    let client = match resolve_client(state, &request.client_id).await {
        Ok(client) => client,
        Err(message) => return Err(AuthorizeRefusal::Page(message)),
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
        let row = opengrok_store::OAuthCodeRow {
            code: code.clone(),
            client_id: client.client_id.clone(),
            client_name: client.client_name.clone(),
            redirect_uri: request.redirect_uri.clone(),
            code_challenge: request.code_challenge.clone(),
            resource: request.resource.clone(),
            account_id: account.as_str().to_string(),
            coworker_id: coworker_id.as_str().to_string(),
            at_ms,
        };
        if let Err(error) = state.store.insert_oauth_code(&row, CODE_TTL_MS).await {
            tracing::error!(%error, "mcp oauth: could not record the authorization code");
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
    #[serde(default)]
    refresh_token: String,
}

fn hash_refresh(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Mint the access key and its refresh token for (client, account, coworker), recording the
/// refresh hashed. One place for both grants, so rotation issues exactly what consent did.
async fn issue_pair(
    state: &AuthState,
    client_id: &str,
    client_name: &str,
    account: &AccountId,
    coworker: &CoworkerId,
    resource: &str,
    // The rotation chain this pair continues; `None` starts one (the new key's jti).
    family: Option<&str>,
) -> Result<Response, Response> {
    let label = format!("{client_name} via OAuth");
    let minted = super::bot_keys::mint(
        &state.store,
        &state.minter,
        account,
        coworker,
        &label,
        Some(resource),
        ACCESS_TTL_SECS,
    )
    .await
    .map_err(|error| {
        tracing::error!(%error, "mcp oauth: could not mint the key");
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "could not mint the key",
        )
    })?;
    let refresh = {
        use rand::RngExt;
        let bytes: [u8; 32] = rand::rng().random();
        format!(
            "rt_{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    };
    let at_ms = now_ms();
    let row = opengrok_store::RefreshTokenRow {
        token_hash: hash_refresh(&refresh),
        jti: minted.jti.clone(),
        client_id: client_id.to_string(),
        account_id: account.as_str().to_string(),
        coworker_id: coworker.as_str().to_string(),
        created_at_ms: at_ms,
        expires_at_ms: at_ms + REFRESH_TTL_SECS * 1_000,
        revoked: false,
        family: family.unwrap_or(&minted.jti).to_string(),
    };
    if let Err(error) = state.store.insert_refresh_token(&row).await {
        // The access key stands (it is recorded and works for a day); only the silent renewal
        // is missing, and the log says so. Not worth failing a flow that already succeeded.
        tracing::error!(%error, jti = %minted.jti, "mcp oauth: key issued but its refresh token could not be recorded");
    }
    tracing::info!(client_id, coworker = %coworker, jti = %minted.jti, "mcp oauth: key issued");
    Ok((
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(json!({
            "access_token": minted.token,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS,
            "refresh_token": refresh,
            "scope": SCOPE,
        })),
    )
        .into_response())
}

/// `grant_type=refresh_token`: rotate. The presented token is spent, its access key revoked,
/// and a fresh pair issued. A revoked token presented again is a replay — somebody has the old
/// token — so the whole family (the key and every refresh for it) is revoked and logged.
async fn refresh(state: &AuthState, form: &TokenForm) -> Response {
    if form.refresh_token.is_empty() || form.client_id.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token and client_id are required",
        );
    }
    let Ok(Some(row)) = state
        .store
        .refresh_token(&hash_refresh(&form.refresh_token))
        .await
    else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "unknown refresh token",
        );
    };
    if row.client_id != form.client_id {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "this refresh token belongs to a different client",
        );
    }
    if row.revoked {
        tracing::warn!(family = %row.family, client_id = %row.client_id, "mcp oauth: a spent refresh token was presented again; revoking its whole chain");
        match state.store.revoke_refresh_family(&row.family).await {
            Ok(jtis) => {
                tracing::info!(family = %row.family, keys = jtis.len(), "mcp oauth: chain revoked")
            }
            Err(error) => {
                tracing::error!(%error, family = %row.family, "mcp oauth: could not revoke the chain")
            }
        }
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "this refresh token was already used; sign in again",
        );
    }
    if now_ms() >= row.expires_at_ms {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "this refresh token has expired; sign in again",
        );
    }
    let resource = resource_uri(&state.public_url);
    if let Some(asked) = form.resource.as_deref()
        && asked != resource
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource must be this server's MCP door",
        );
    }
    // Rotate: the old pair dies before the new one exists, so a crash in between leaves the
    // client signing in again rather than holding two live keys.
    let _ = state.store.revoke_refresh_tokens_for(&row.jti).await;
    let _ = state.store.revoke_bot_key_by_jti(&row.jti).await;
    let client_name = state
        .store
        .oauth_client(&row.client_id)
        .await
        .ok()
        .flatten()
        .map(|client| client.client_name)
        .unwrap_or_else(|| row.client_id.clone());
    match issue_pair(
        state,
        &row.client_id,
        &client_name,
        &AccountId::from_stored(row.account_id.clone()),
        &CoworkerId::from_stored(row.coworker_id.clone()),
        &resource,
        Some(&row.family),
    )
    .await
    {
        Ok(response) | Err(response) => response,
    }
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

/// `POST /oauth/mcp/token` — the code, the verifier, the key; or a refresh token, rotated.
async fn token(State(state): State<AuthState>, Form(form): Form<TokenForm>) -> Response {
    if form.grant_type == "refresh_token" {
        return refresh(&state, &form).await;
    }
    if form.grant_type != "authorization_code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only authorization_code and refresh_token are supported",
        );
    }
    if form.code.is_empty() || form.code_verifier.is_empty() || form.client_id.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code, code_verifier and client_id are required",
        );
    }
    // TAKE the code: one exchange, ever, on whichever replica this lands. A second
    // presentation — replay, retry — finds nothing.
    let pending = match state.store.take_oauth_code(&form.code).await {
        Ok(Some(row)) => PendingCode {
            client_id: row.client_id,
            client_name: row.client_name,
            redirect_uri: row.redirect_uri,
            code_challenge: row.code_challenge,
            resource: row.resource,
            account: AccountId::from_stored(row.account_id),
            coworker: CoworkerId::from_stored(row.coworker_id),
            at_ms: row.at_ms,
        },
        Ok(None) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "unknown, expired or already used code",
            );
        }
        Err(error) => {
            tracing::error!(%error, "mcp oauth: could not take the code");
            return oauth_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "the code could not be checked right now; try again",
            );
        }
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
    match issue_pair(
        &state,
        &pending.client_id,
        &pending.client_name,
        &pending.account,
        &pending.coworker,
        &pending.resource,
        None,
    )
    .await
    {
        Ok(response) | Err(response) => response,
    }
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
    fn a_client_id_url_is_https_with_a_path_and_never_private() {
        assert!(cimd_url_allowed(
            "https://tool.example/oauth/client.json",
            false
        ));
        assert!(
            !cimd_url_allowed("https://tool.example", false),
            "a path is required"
        );
        assert!(!cimd_url_allowed("https://tool.example/c.json#x", false));
        assert!(!cimd_url_allowed("https://u:p@tool.example/c.json", false));
        assert!(!cimd_url_allowed("https://tool.example/a/../c.json", false));
        assert!(!cimd_url_allowed("http://tool.example/c.json", false));
        assert!(!cimd_url_allowed("https://10.0.0.5/c.json", false));
        assert!(!cimd_url_allowed("https://172.20.1.1/c.json", false));
        assert!(!cimd_url_allowed("http://127.0.0.1:9/c.json", false));
        assert!(
            cimd_url_allowed("http://127.0.0.1:9/c.json", true),
            "tests may allow loopback"
        );
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
