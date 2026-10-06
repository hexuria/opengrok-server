//! Signing in at an installed plugin's own MCP server (#364): the MCP authorization spec, so a
//! plugin from the marketplace sends its person to its provider's own consent page, as Grok Bot
//! does, instead of asking them to paste a key.
//!
//! The steps, each a function here: find where the server says to sign in (`discover`: the
//! protected-resource metadata of RFC 9728, then the authorization server's RFC 8414 metadata);
//! register this deployment with that authorization server once (`client`, RFC 7591), kept per
//! issuer and callback; send the browser there with PKCE and the MCP server named as the
//! `resource` (RFC 8707); trade the code back (`exchange`); and refresh before the token lapses
//! (`refresh_due`).
//!
//! EVERY ADDRESS IS A THIRD PARTY'S. A bundle names its server, and that server names the rest, so
//! each URL is held to the rules an installed plugin's server is (`crate::net`): public HTTPS only,
//! no redirects, no proxy. A server that answers none of this has no sign-in of its own, and its
//! person pastes a token as before.
//!
//! The account it makes is kind `mcp`, held as a pasted token is (`ConnectionKind::is_plugin_account`):
//! bound to the install that asked, its token sealed under the account's own id, which the binding
//! names, so the turn reads it exactly as it reads a pasted one.
use crate::accounts;
use base64::Engine as _;
use opengrok_core::connection::{Connection, ConnectionCommand, Owner};
use opengrok_core::id::AccountId;
use opengrok_store::{PgStore, StoreError, StoreResult, Vault};
use serde::Deserialize;
use sha2::Digest as _;
use sqlx::Row;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which addresses may be fetched. Deployments use `crate::net::public_url` (`PUBLIC`): every
/// address here is a third party's. A test's stand-in provider on loopback passes its own.
pub type Guard = fn(&str) -> bool;
pub const PUBLIC: Guard = crate::net::public_url;

/// Where and how a server's sign-in happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The MCP server the token is for, as the browser and the token endpoint are told.
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpAuthError {
    /// The server publishes no sign-in of its own; a token is pasted instead.
    #[error("{0}")]
    NoSignIn(String),
    /// The provider answered, but not with something this server can use.
    #[error("{0}")]
    Provider(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn no_sign_in(why: impl Into<String>) -> McpAuthError {
    McpAuthError::NoSignIn(why.into())
}

/// The client every request here goes through: hardened, short, and small.
pub fn http() -> reqwest::Client {
    crate::net::harden(reqwest::Client::builder())
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// A metadata document is a few kilobytes; one larger than this is not one.
const DOCUMENT_LIMIT: usize = 256 * 1024;

async fn get_json(
    http: &reqwest::Client,
    guard: Guard,
    url: &str,
) -> Result<serde_json::Value, McpAuthError> {
    if !guard(url) {
        return Err(no_sign_in(format!("{url} is not a public HTTPS address")));
    }
    let response = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| no_sign_in(format!("{url} did not answer: {error}")))?;
    if !response.status().is_success() {
        return Err(no_sign_in(format!("{url} answered {}", response.status())));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| no_sign_in(error.to_string()))?;
    if bytes.len() > DOCUMENT_LIMIT {
        return Err(no_sign_in(format!(
            "{url} sent more than a metadata document"
        )));
    }
    serde_json::from_slice(&bytes).map_err(|_| no_sign_in(format!("{url} sent no JSON")))
}

/// `https://host/path` as its origin, `https://host`, and its path, `/path` (empty for `/`).
fn split(url: &str) -> Option<(String, String)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/').to_string();
    Some((origin, path))
}

/// The `resource_metadata` a 401's `WWW-Authenticate` names (RFC 9728 §5.1), if any.
fn resource_metadata_hint(header: &str) -> Option<String> {
    let at = header.find("resource_metadata=")?;
    let rest = &header[at + "resource_metadata=".len()..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split([',', ' ']).next()?,
    };
    Some(value.to_string())
}

/// Find where `mcp_url` says to sign in.
///
/// First the server's own word: an unauthenticated request answered 401 names its
/// protected-resource metadata. Failing that, the well-known address for it, path-scoped then at
/// the root. A server with no protected-resource metadata but an authorization server at its own
/// origin (the MCP spec's first revision) is read that way too. None of these: no sign-in.
pub async fn discover(
    http: &reqwest::Client,
    guard: Guard,
    mcp_url: &str,
) -> Result<Metadata, McpAuthError> {
    if !guard(mcp_url) {
        return Err(no_sign_in(
            "the plugin's server is not a public HTTPS address",
        ));
    }
    let (origin, path) =
        split(mcp_url).ok_or_else(|| no_sign_in("the plugin's server address is unreadable"))?;
    let mut candidates = Vec::new();
    // An MCP `initialize` with no token, which a server that signs people in refuses with 401.
    let probe = http
        .post(mcp_url)
        .header("accept", "application/json, text/event-stream")
        .json(
            &serde_json::json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"opengrok","version":"1"}}}),
        )
        .send()
        .await;
    if let Ok(response) = &probe
        && response.status() == reqwest::StatusCode::UNAUTHORIZED
        && let Some(hint) = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .and_then(resource_metadata_hint)
    {
        candidates.push(hint);
    }
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));

    let mut protected = None;
    for candidate in candidates {
        if let Ok(document) = get_json(http, guard, &candidate).await
            && document["authorization_servers"].is_array()
        {
            protected = Some(document);
            break;
        }
    }
    let (issuer, scopes) = match &protected {
        Some(document) => {
            let issuer = document["authorization_servers"][0]
                .as_str()
                .ok_or_else(|| no_sign_in("the server names no authorization server"))?
                .trim_end_matches('/')
                .to_string();
            let scopes = document["scopes_supported"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            (issuer, scopes)
        }
        None => (origin.clone(), Vec::new()),
    };
    let server = authorization_server(http, guard, &issuer).await?;
    let text = |key: &str| server[key].as_str().map(str::to_string);
    let authorization_endpoint = text("authorization_endpoint")
        .ok_or_else(|| no_sign_in("the authorization server names no authorization endpoint"))?;
    let token_endpoint = text("token_endpoint")
        .ok_or_else(|| no_sign_in("the authorization server names no token endpoint"))?;
    for url in [&authorization_endpoint, &token_endpoint] {
        if !guard(url) {
            return Err(no_sign_in(format!("{url} is not a public HTTPS address")));
        }
    }
    // PKCE with S256 is required by the MCP spec; a server that says it offers other methods only
    // cannot be signed in to safely.
    if let Some(methods) = server["code_challenge_methods_supported"].as_array()
        && !methods.iter().any(|m| m == "S256")
    {
        return Err(no_sign_in(
            "the authorization server does not offer PKCE with S256",
        ));
    }
    let scopes = if scopes.is_empty() {
        server["scopes_supported"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        scopes
    };
    Ok(Metadata {
        resource: mcp_url.to_string(),
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: text("registration_endpoint"),
        scopes,
    })
}

/// The authorization server's own metadata, at the addresses RFC 8414 and OpenID Connect give it.
async fn authorization_server(
    http: &reqwest::Client,
    guard: Guard,
    issuer: &str,
) -> Result<serde_json::Value, McpAuthError> {
    let (origin, path) =
        split(issuer).ok_or_else(|| no_sign_in("the issuer address is unreadable"))?;
    let mut candidates = vec![format!(
        "{origin}/.well-known/oauth-authorization-server{path}"
    )];
    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/openid-configuration{path}"));
    }
    candidates.push(format!("{issuer}/.well-known/openid-configuration"));
    for candidate in candidates {
        if let Ok(document) = get_json(http, guard, &candidate).await
            && document["authorization_endpoint"].is_string()
        {
            return Ok(document);
        }
    }
    Err(no_sign_in("the server publishes no sign-in of its own"))
}

/// What a probe found, kept a while: a detail page asks it each time it opens.
static PROBES: Mutex<BTreeMap<String, (Instant, bool)>> = Mutex::new(BTreeMap::new());
const PROBE_TTL: Duration = Duration::from_secs(600);

/// Whether `mcp_url` offers a sign-in of its own, remembered for ten minutes.
pub async fn offers_sign_in(http: &reqwest::Client, mcp_url: &str) -> bool {
    if let Some((at, answer)) = PROBES.lock().ok().and_then(|p| p.get(mcp_url).copied())
        && at.elapsed() < PROBE_TTL
    {
        return answer;
    }
    let answer = discover(http, PUBLIC, mcp_url).await.is_ok();
    if let Ok(mut probes) = PROBES.lock() {
        if probes.len() > 512 {
            probes.clear();
        }
        probes.insert(mcp_url.to_string(), (Instant::now(), answer));
    }
    answer
}

/// A client this deployment registered with an authorization server.
#[derive(Debug, Clone)]
pub struct Client {
    pub client_id: String,
    pub client_secret: Option<String>,
}

#[derive(Deserialize)]
struct Registered {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

fn client_secret_id(issuer: &str, redirect_uri: &str) -> String {
    let digest = sha2::Sha256::digest(format!("{issuer}\n{redirect_uri}"));
    format!("mcp-client:{}", hex(&digest[..16]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The client for `metadata`'s authorization server and this callback: the one registered before,
/// or a new registration (RFC 7591) kept for everyone after. A server with no registration
/// endpoint cannot be signed in to without an operator's client, which an installed plugin has not.
pub async fn client(
    http: &reqwest::Client,
    guard: Guard,
    store: &PgStore,
    vault: &Vault,
    metadata: &Metadata,
    redirect_uri: &str,
    at_ms: i64,
) -> Result<Client, McpAuthError> {
    let row = sqlx::query(
        "select client_id, secret_id from mcp_oauth_client where issuer = $1 and redirect_uri = $2",
    )
    .bind(&metadata.issuer)
    .bind(redirect_uri)
    .fetch_optional(store.pool())
    .await
    .map_err(StoreError::from)?;
    if let Some(row) = row {
        let secret_id: Option<String> = row.try_get("secret_id").map_err(StoreError::from)?;
        let client_secret = match secret_id {
            Some(id) => store.open_credential(vault, &id).await?,
            None => None,
        };
        return Ok(Client {
            client_id: row.try_get("client_id").map_err(StoreError::from)?,
            client_secret,
        });
    }
    let endpoint = metadata
        .registration_endpoint
        .as_deref()
        .filter(|url| guard(url))
        .ok_or_else(|| no_sign_in("the authorization server does not let this server register"))?;
    let response = http
        .post(endpoint)
        .json(&serde_json::json!({
            "client_name": "OpenGrok",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .await
        .map_err(|error| McpAuthError::Provider(format!("registration did not answer: {error}")))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(McpAuthError::Provider(format!(
            "registration was refused ({status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    let registered: Registered = serde_json::from_str(&body)
        .map_err(|_| McpAuthError::Provider("registration sent no client_id".into()))?;
    let secret_id = match &registered.client_secret {
        Some(secret) => {
            let id = client_secret_id(&metadata.issuer, redirect_uri);
            store
                .put_secret(&id, &vault.seal(&id, secret)?, at_ms)
                .await?;
            Some(id)
        }
        None => None,
    };
    // Two first sign-ins at once both register; the first row stays, and the other's client is
    // simply not reused.
    sqlx::query(
        "insert into mcp_oauth_client (issuer, redirect_uri, client_id, secret_id, registered_at_ms)
         values ($1, $2, $3, $4, $5) on conflict do nothing",
    )
    .bind(&metadata.issuer)
    .bind(redirect_uri)
    .bind(&registered.client_id)
    .bind(&secret_id)
    .bind(at_ms)
    .execute(store.pool())
    .await
    .map_err(StoreError::from)?;
    Ok(Client {
        client_id: registered.client_id,
        client_secret: registered.client_secret,
    })
}

/// A fresh PKCE pair (RFC 7636, S256).
pub fn pkce() -> crate::oauth::Pkce {
    let raw = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(raw.as_bytes()));
    crate::oauth::Pkce {
        verifier: raw,
        challenge,
    }
}

fn encode(value: &str) -> String {
    crate::oauth::encode(value)
}

/// The provider's consent page for this sign-in. No secret is in it: it goes to a browser.
pub fn authorize_url(
    metadata: &Metadata,
    client: &Client,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let mut params = vec![
        ("response_type", "code".to_string()),
        ("client_id", client.client_id.clone()),
        ("redirect_uri", redirect_uri.to_string()),
        ("state", state.to_string()),
        ("code_challenge", challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("resource", metadata.resource.clone()),
    ];
    if !metadata.scopes.is_empty() {
        params.push(("scope", metadata.scopes.join(" ")));
    }
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let separator = if metadata.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{separator}{query}", metadata.authorization_endpoint)
}

async fn token_request(
    http: &reqwest::Client,
    guard: Guard,
    token_endpoint: &str,
    form: &[(&str, String)],
) -> Result<crate::oauth::TokenResponse, McpAuthError> {
    if !guard(token_endpoint) {
        return Err(McpAuthError::Provider(
            "the token endpoint is not a public HTTPS address".into(),
        ));
    }
    let response = http
        .post(token_endpoint)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .map_err(|error| {
            McpAuthError::Provider(format!("the token endpoint did not answer: {error}"))
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let why = serde_json::from_str::<crate::oauth::TokenError>(&body)
            .map(|e| e.error_description.unwrap_or(e.error))
            .unwrap_or_else(|_| status.to_string());
        return Err(McpAuthError::Provider(format!(
            "the provider refused: {why}"
        )));
    }
    crate::oauth::TokenResponse::parse(&body).map_err(McpAuthError::Provider)
}

/// What a sign-in's callback needs to finish it, kept on its unfinished sign-in.
#[derive(Debug, Clone)]
pub struct Pending {
    pub plugin: String,
    pub connector: String,
    pub label: String,
    pub verifier: String,
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub resource: String,
    /// The MCP account a reconnect refreshes, when it is one.
    pub target: Option<String>,
    /// The callback the client was registered with and the browser came back to.
    pub redirect_uri: String,
}

/// Trade the code for a token at the provider (RFC 6749 §4.1.3, with PKCE and the resource).
pub async fn exchange(
    http: &reqwest::Client,
    guard: Guard,
    store: &PgStore,
    vault: &Vault,
    pending: &Pending,
    code: &str,
) -> Result<crate::oauth::TokenResponse, McpAuthError> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", pending.redirect_uri.clone()),
        ("client_id", pending.client_id.clone()),
        ("code_verifier", pending.verifier.clone()),
        ("resource", pending.resource.clone()),
    ];
    if let Some(secret) =
        stored_client_secret(store, vault, &pending.issuer, &pending.redirect_uri).await?
    {
        form.push(("client_secret", secret));
    }
    token_request(http, guard, &pending.token_endpoint, &form).await
}

async fn stored_client_secret(
    store: &PgStore,
    vault: &Vault,
    issuer: &str,
    redirect_uri: &str,
) -> StoreResult<Option<String>> {
    let id: Option<Option<String>> = sqlx::query_scalar(
        "select secret_id from mcp_oauth_client where issuer = $1 and redirect_uri = $2",
    )
    .bind(issuer)
    .bind(redirect_uri)
    .fetch_optional(store.pool())
    .await?;
    match id.flatten() {
        Some(id) => store.open_credential(vault, &id).await,
        None => Ok(None),
    }
}

/// Keep the signed-in account: a new MCP account bound to the install that asked, or the one a
/// reconnect named, refreshed. `None` when the plugin was uninstalled (or no longer names the
/// service) while the person was at the provider: nothing is kept for an install that is gone.
#[allow(clippy::too_many_arguments)]
pub async fn connect(
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    pending: &Pending,
    token: &crate::oauth::TokenResponse,
    at_ms: i64,
) -> StoreResult<Option<String>> {
    let mut tx = store.pool().begin().await?;
    let bundle: Option<serde_json::Value> = sqlx::query_scalar(
        "select bundle from plugin_installation where account_id = $1 and name = $2 for update",
    )
    .bind(account.as_str())
    .bind(&pending.plugin)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(bundle) = bundle else {
        return Ok(None);
    };
    let bundle: opengrok_plugins::bundle::Bundle =
        serde_json::from_value(bundle).map_err(|_| StoreError::Corrupt("invalid bundle".into()))?;
    if !bundle.connectors().contains(&pending.connector) {
        return Ok(None);
    }
    // A reconnect refreshes its account only while it is still this install's live MCP account.
    let mut existing = None;
    if let Some(target) = &pending.target {
        let bound: Option<String> = sqlx::query_scalar(
            "select connection_id from plugin_credential
              where account_id = $1 and plugin_name = $2 and connector = $3 and connection_id = $4",
        )
        .bind(account.as_str())
        .bind(&pending.plugin)
        .bind(&pending.connector)
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?;
        if bound.is_some() {
            let (connection, seq) = store.load_connection(target).await?;
            if connection.connected && !connection.disconnected {
                existing = Some((target.clone(), connection, seq));
            }
        }
    }
    let (id, mut connection, seq, command) = match existing {
        Some((id, connection, seq)) => (id, connection, seq, ConnectionCommand::Refresh { at_ms }),
        None => {
            let owner = Owner::User(account.clone());
            let id = accounts::new_id(&pending.connector, &owner);
            let command = ConnectionCommand::ConnectMcp {
                connector: pending.connector.clone(),
                owner: account.clone(),
                label: pending.label.clone(),
                at_ms,
            };
            (id, Connection::default(), 0, command)
        }
    };
    let events = connection
        .decide(command)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    for event in &events {
        connection.apply(event);
    }
    // Sealed under the account's own id, which its binding names as its secret: the turn reads it
    // through the binding as it reads a pasted token, and a disconnect deletes it by that id.
    let sealed = vault.seal(&id, &token.access_token)?;
    let update =
        opengrok_store::CredentialUpdate::sealed(&sealed, token.expires_at_ms(at_ms), at_ms);
    PgStore::append_connection_in(&mut tx, &id, seq, &events, &connection, &update).await?;
    sqlx::query(
        "insert into plugin_credential(account_id, plugin_name, connector, secret_id, connection_id)
         values ($1, $2, $3, $4, $4) on conflict (secret_id) do nothing",
    )
    .bind(account.as_str())
    .bind(&pending.plugin)
    .bind(&pending.connector)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "insert into mcp_oauth_grant (connection_id, issuer, token_endpoint, client_id, resource,
           redirect_uri)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (connection_id) do update set issuer = excluded.issuer,
           token_endpoint = excluded.token_endpoint, client_id = excluded.client_id,
           resource = excluded.resource, redirect_uri = excluded.redirect_uri",
    )
    .bind(&id)
    .bind(&pending.issuer)
    .bind(&pending.token_endpoint)
    .bind(&pending.client_id)
    .bind(&pending.resource)
    .bind(&pending.redirect_uri)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // The refresh token is its own row, kept when a provider sends none with a later exchange.
    let refresh_id = format!("{id}_refresh");
    let previous = store
        .open_credential(vault, &refresh_id)
        .await
        .ok()
        .flatten();
    if let Some(refresh) = token.refresh_token_to_store(previous.as_deref()) {
        store
            .put_secret(&refresh_id, &vault.seal(&refresh_id, &refresh)?, at_ms)
            .await?;
    }
    Ok(Some(id))
}

/// Refresh this install's MCP accounts that lapse within a minute, before a turn reads them. A
/// refresh the provider refuses leaves the account as it was: its server refuses the stale token,
/// and the plugin sits the turn out, as one that will not connect does, until its person reconnects.
pub async fn refresh_due(
    http: &reqwest::Client,
    guard: Guard,
    store: &PgStore,
    vault: &Vault,
    account: &AccountId,
    plugin: &str,
    now_ms: i64,
) {
    let rows = sqlx::query(
        "select v.id, g.issuer, g.token_endpoint, g.client_id, g.resource, g.redirect_uri
           from plugin_credential c
           join connection_view v on v.id = c.connection_id and v.kind = 'mcp' and not v.disconnected
           join mcp_oauth_grant g on g.connection_id = v.id
          where c.account_id = $1 and c.plugin_name = $2
            and v.expires_at_ms is not null and v.expires_at_ms < $3",
    )
    .bind(account.as_str())
    .bind(plugin)
    .bind(now_ms + 60_000)
    .fetch_all(store.pool())
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, plugin, "MCP accounts due a refresh could not be read");
            return;
        }
    };
    for row in rows {
        let id: String = row.get("id");
        let refresh_id = format!("{id}_refresh");
        let Ok(Some(refresh)) = store.open_credential(vault, &refresh_id).await else {
            continue;
        };
        let issuer: String = row.get("issuer");
        let redirect_uri: String = row.get("redirect_uri");
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh.clone()),
            ("client_id", row.get::<String, _>("client_id")),
            ("resource", row.get::<String, _>("resource")),
        ];
        if let Ok(Some(secret)) = stored_client_secret(store, vault, &issuer, &redirect_uri).await {
            form.push(("client_secret", secret));
        }
        let token_endpoint: String = row.get("token_endpoint");
        match token_request(http, guard, &token_endpoint, &form).await {
            Ok(token) => {
                let saved = async {
                    store
                        .put_secret(&id, &vault.seal(&id, &token.access_token)?, now_ms)
                        .await?;
                    store
                        .touch_expiry(&id, token.expires_at_ms(now_ms), now_ms)
                        .await?;
                    if let Some(next) = token.refresh_token_to_store(Some(&refresh)) {
                        store
                            .put_secret(&refresh_id, &vault.seal(&refresh_id, &next)?, now_ms)
                            .await?;
                    }
                    StoreResult::Ok(())
                };
                if let Err(error) = saved.await {
                    tracing::warn!(%error, plugin, "a refreshed MCP token could not be kept");
                }
            }
            Err(error) => tracing::info!(%error, plugin, "an MCP account could not be refreshed"),
        }
    }
}

/// The label of `id` when it is a live MCP account this install holds for `connector`.
pub async fn account_label(
    store: &PgStore,
    account: &AccountId,
    plugin: &str,
    connector: &str,
    id: &str,
) -> StoreResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "select v.label from plugin_credential c
           join connection_view v on v.id = c.connection_id
          where c.account_id = $1 and c.plugin_name = $2 and c.connector = $3
            and c.connection_id = $4 and v.kind = 'mcp' and not v.disconnected",
    )
    .bind(account.as_str())
    .bind(plugin)
    .bind(connector)
    .bind(id)
    .fetch_optional(store.pool())
    .await?)
}

/// The MCP server a connector of `bundle` names: the hosted server of that name, or the one whose
/// headers carry its `<CONNECTOR>_TOKEN`.
pub fn server_url(bundle: &opengrok_plugins::bundle::Bundle, connector: &str) -> Option<String> {
    use opengrok_plugins::McpServer;
    let key = format!("${{{}}}", opengrok_plugins::token_key(connector));
    bundle
        .mcp
        .servers
        .iter()
        .find_map(|(name, server)| match server {
            McpServer::StreamableHttp { url, headers, .. }
                if name == connector || headers.values().any(|v| v.contains(&key)) =>
            {
                Some(url.clone())
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_401_names_its_resource_metadata_quoted_or_bare() {
        assert_eq!(
            resource_metadata_hint(r#"Bearer error="invalid_token", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#).as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            resource_metadata_hint("Bearer resource_metadata=https://x.example/m, realm=x")
                .as_deref(),
            Some("https://x.example/m")
        );
        assert_eq!(resource_metadata_hint("Bearer realm=x"), None);
    }

    #[test]
    fn the_consent_page_carries_pkce_the_resource_and_no_secret() {
        let metadata = Metadata {
            resource: "https://mcp.example.com/mcp".into(),
            issuer: "https://auth.example.com".into(),
            authorization_endpoint: "https://auth.example.com/authorize".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            registration_endpoint: None,
            scopes: vec!["read".into(), "write".into()],
        };
        let client = Client {
            client_id: "abc".into(),
            client_secret: Some("never-shown".into()),
        };
        let pair = pkce();
        let url = authorize_url(
            &metadata,
            &client,
            "https://og.example/connections/callback",
            "st",
            &pair.challenge,
        );
        assert!(
            url.starts_with("https://auth.example.com/authorize?response_type=code&client_id=abc")
        );
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={}", pair.challenge)));
        assert!(url.contains("resource=https%3A%2F%2Fmcp.example.com%2Fmcp"));
        assert!(url.contains("scope=read%20write"));
        assert!(!url.contains("never-shown"));
        assert!(!url.contains(&pair.verifier));
        // S256 is the SHA-256 of the verifier, unpadded base64url.
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(pair.verifier.as_bytes()));
        assert_eq!(pair.challenge, expected);
    }
}
