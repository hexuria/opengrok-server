//! The two endpoints that replace Cursor's OAuth.
//!
//! Setting `SAND_BACKEND_URL` repoints the client's ENTIRE auth backend at us
//! (`cursor-token.ts:39`), and any host that is not `api2.cursor.sh` resolves to the dev client id,
//! which makes `isDevAuthBackend` true (`:42-49`). That unlocks the dev-login path — so the first
//! slice needs no browser, no PKCE and no redirect: two endpoints and a JWT.
//!
//! THE TWO REPLIES USE DIFFERENT CASING AND IT IS NOT A MISTAKE.
//!   - `/auth/cursor_dev_session_token` → `{ accessToken, refreshToken }`  (camelCase)
//!     `cursor-auth.ts:315-316` reads `body.accessToken` and `body.refreshToken`.
//!   - `POST /oauth/token`             → `{ access_token, refresh_token }` (snake_case)
//!     `parseOAuthTokenBody` (`:160-166`) rejects the body outright unless those keys are strings.
//!
//! Normalising either one to match the other is the tidying that breaks a client we do not compile.
//!
//! On `refreshToken` being optional in the dev reply (`:316` falls back to the access token): we
//! always send one. Falling back would make the client hold an access token as its refresh token,
//! and `cursorSessionPresent` would report a session that cannot survive its own first refresh.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use opengrok_core::account::{AccountCommand, AccountError, Plan};
use opengrok_core::id::{AccountId, SessionId};
use opengrok_store::{PgStore, StoreError};
use serde::{Deserialize, Serialize};

use super::token::{ACCESS_TOKEN_TTL_SECONDS, TokenMinter, hash_refresh_token, mint_refresh_token};

/// One in-flight browser login: a challenge registered by `/loginDeepControl`, waiting for the
/// client to poll with the matching verifier. In memory on purpose — a login that does not
/// complete within its TTL is meant to be forgotten, and a restart just means signing in again.
#[derive(Clone)]
struct PendingLogin {
    challenge: String,
    /// The authenticated account's email — `None` until credential login succeeds. `/auth/poll`
    /// releases a token only once this is `Some`, so a registered-but-unauthenticated challenge
    /// never completes.
    email: Option<String>,
    at_ms: i64,
}

/// How long a registered challenge is good for. Long enough for a person to finish in the
/// browser, short enough that an abandoned one does not linger.
const LOGIN_TTL_MS: i64 = 5 * 60 * 1000;

#[derive(Clone)]
pub struct AuthState {
    pub store: PgStore,
    pub minter: Arc<TokenMinter>,
    /// Who a browser login signs in as when the server is in single-user host mode (the pre-org
    /// path). Org signups bind their own account.
    pub login_email: String,
    /// The Resend API key, if configured. `None` ⇒ no mailer, so signup auto-verifies (Uriah's
    /// "if we have set resend api ... if not skip it"). The key never leaves the server.
    pub resend_api_key: Option<String>,
    /// The base URL a verification link points back at (this server, as the client reaches it).
    pub public_url: String,
    /// uuid → the challenge waiting to be completed. Guarded, swept on each poll.
    logins: Arc<Mutex<HashMap<String, PendingLogin>>>,
    /// The reverse-exec transport broker — the live meeting point of a machine's daemon stream and
    /// a caller waiting for one command's result. One per replica, shared by the `/local-exec/*`
    /// routes.
    pub local_exec: Arc<crate::local_exec::LocalExecBroker>,
    /// The other door's key desk: open-ai-gateway's admin API, for minting an org member a key.
    /// Resolved ONCE at boot rather than per request — the environment is not a per-request input,
    /// and a seam a test can substitute is worth more than a `std::env` read in a handler.
    /// `None` ⇒ the deployment has not wired it, and that surface answers 503.
    pub gateway_admin: Option<crate::gateway_admin::GatewayAdmin>,
    /// The model catalogue and pin probe, built from the SAME two variables as the model door so
    /// a picker can never advertise a gateway the runs do not use. `None` on a mock door, where
    /// there is no gateway to ask and the picker says so.
    pub model_catalogue: Option<std::sync::Arc<crate::models::ModelCatalogue>>,
    /// The MCP door's OAuth authorization codes, waiting to be exchanged: one-shot, ten
    /// minutes, bound to the client, redirect, PKCE challenge and resource they were issued
    /// for. In memory like `logins` — a single replica today; both move to a table together.
    pub oauth_codes: Arc<Mutex<HashMap<String, super::oauth_mcp::PendingCode>>>,
    /// The hit table behind every door that takes no credential (`budget.rs`): reset mail,
    /// client registration, wrong passwords, domain lookups. Per replica by design.
    pub budgets: Arc<super::budget::Budgets>,
    /// The TXT lookup behind domain-ownership proof. A seam, not a resolver: production binds the
    /// system resolver at boot, tests answer from a map. The default refuses every lookup with a
    /// reason, so a state that was never given one answers 503 rather than "not verified".
    pub dns: Arc<dyn crate::domain_proof::TxtLookup>,
}

impl AuthState {
    pub fn new(store: PgStore, minter: Arc<TokenMinter>, login_email: String) -> Self {
        Self {
            store,
            minter,
            login_email,
            resend_api_key: None,
            public_url: String::new(),
            logins: Arc::new(Mutex::new(HashMap::new())),
            local_exec: Arc::new(crate::local_exec::LocalExecBroker::new()),
            gateway_admin: crate::gateway_admin::GatewayAdmin::from_env(),
            model_catalogue: crate::models::ModelCatalogue::from_env().map(std::sync::Arc::new),
            dns: Arc::new(crate::domain_proof::NoResolver),
            oauth_codes: Arc::new(Mutex::new(HashMap::new())),
            budgets: Arc::new(super::budget::Budgets::default()),
        }
    }

    /// Bind the TXT lookup domain proof uses — the system resolver in production, a map in tests.
    #[must_use]
    pub fn with_dns(mut self, dns: Arc<dyn crate::domain_proof::TxtLookup>) -> Self {
        self.dns = dns;
        self
    }

    pub fn with_resend(mut self, key: Option<String>, public_url: String) -> Self {
        self.resend_api_key = key.filter(|k| !k.is_empty());
        self.public_url = public_url;
        self
    }

    /// Point the catalogue at an explicit gateway — what a test uses to stand in for a real one
    /// without touching the process environment.
    #[must_use]
    pub fn with_model_catalogue(
        mut self,
        catalogue: Option<std::sync::Arc<crate::models::ModelCatalogue>>,
    ) -> Self {
        self.model_catalogue = catalogue;
        self
    }

    /// Point the gateway's admin door somewhere explicit — what a test uses to stand in for the
    /// real gateway without touching the process environment.
    #[must_use]
    pub fn with_gateway_admin(mut self, admin: Option<crate::gateway_admin::GatewayAdmin>) -> Self {
        self.gateway_admin = admin;
        self
    }
}

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route("/auth/cursor_dev_session_token", get(dev_session_token))
        .route("/oauth/token", post(oauth_token))
        // The real browser login leg (roadmap 9.1b): a person opens loginDeepControl, and the
        // client polls auth/poll with the PKCE verifier until a token is released.
        .route(
            "/loginDeepControl",
            get(login_deep_control).post(login_submit),
        )
        .route("/auth/poll", get(auth_poll))
        // The web console's cookie login leg — a browser signs in with credentials and
        // the session rides in httpOnly cookies, never a header the page can read.
        .route("/auth/login", post(login_cookie))
        .route("/auth/logout", post(logout_cookie))
        .route("/auth/refresh", post(refresh_cookie))
        .route("/auth/signup", post(super::identity::signup))
        .route("/auth/verify", get(super::identity::verify_email))
        .route(
            "/signup",
            get(super::identity::signup_page).post(super::identity::signup_form),
        )
        // Password reset (12.later): the styled pages a person reaches from the sign-in card, and
        // the JSON the console's own login page calls. Both go through the same start_reset.
        .route(
            "/forgot-password",
            get(super::password_reset::forgot_page).post(super::password_reset::forgot_form),
        )
        .route(
            "/auth/password/forgot",
            post(super::password_reset::forgot_json),
        )
        .route(
            "/reset-password",
            get(super::password_reset::reset_page).post(super::password_reset::reset_form),
        )
        .with_state(state)
}

/// The PKCE binding the client computes (`login.ts:19`): `challenge == base64url(sha256(verifier))`.
fn challenge_for(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[derive(Debug, Deserialize)]
pub struct LoginDeepControlQuery {
    pub challenge: String,
    pub uuid: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub redirect_target: Option<String>,
}

/// `GET /loginDeepControl?challenge=&uuid=&mode=login&redirectTarget=`
///
/// THE HUMAN STEP. On a self-hosted single-user server, a person opening this URL in their own
/// browser IS the authentication — there is no third party to delegate to. It registers the
/// challenge against the uuid and binds it to the one account; the client's poll then completes
/// only when it presents the verifier whose hash matches. That binding is what closes the hole
/// the old blind poll left open: a challenge nobody registered can never be polled into a token.
pub async fn login_deep_control(
    State(state): State<AuthState>,
    Query(query): Query<LoginDeepControlQuery>,
) -> Response {
    if query.challenge.is_empty() || query.uuid.is_empty() {
        return (StatusCode::BAD_REQUEST, "challenge and uuid are required").into_response();
    }
    // Register the challenge, UNAUTHENTICATED. The form below carries challenge+uuid back on POST,
    // where real credentials bind the account. Until then this uuid can be polled forever and
    // completes nothing — the same closed-hole property 9.1b established, now with a login step
    // instead of "whoever opened the URL is host".
    let at_ms = now_ms();
    if let Ok(mut logins) = state.logins.lock() {
        logins.retain(|_, pending| at_ms - pending.at_ms < LOGIN_TTL_MS);
        logins.insert(
            query.uuid.clone(),
            PendingLogin {
                challenge: query.challenge.clone(),
                email: None,
                at_ms,
            },
        );
    }
    tracing::info!(uuid = %query.uuid, "loginDeepControl: served the browser login page");
    super::pages::login(&query.challenge, &query.uuid, None)
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub challenge: String,
    pub uuid: String,
    pub email: String,
    pub password: String,
}

/// `POST /loginDeepControl` — the credential submit. Authenticates, and only on success binds the
/// uuid to THAT account so `/auth/poll` will release its token. Every refusal is distinguishable
/// (not verified / not enabled / wrong credentials) so the page can say which.
pub async fn login_submit(
    State(state): State<AuthState>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<LoginForm>,
) -> Response {
    let refuse = |message: &str| super::pages::login(&form.challenge, &form.uuid, Some(message));

    // Wrong passwords are budgeted per address; the check comes BEFORE the hash so a spent
    // budget costs nothing more, and the credentials are not even looked at.
    let peer = super::budget::peer_key(&headers);
    if let Err(spent) = state.budgets.check(&super::budget::LOGIN_FAILURES, &peer) {
        // The styled page re-renders with its message at 200 for an ordinary refusal (a form
        // post that stays on the page); a spent budget is a 429 so a client reads it as one.
        let mut page = refuse(&too_many_attempts(spent));
        *page.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return super::budget::with_retry_after(page, spent);
    }
    let view = match authenticate(&state, &form.email, &form.password).await {
        Ok(view) => view,
        Err((_, message)) => {
            state.budgets.hit(&super::budget::LOGIN_FAILURES, &peer);
            return refuse(&message);
        }
    };

    // Authenticated. Bind the uuid to this account — poll will now complete for the matching
    // verifier. The challenge was registered on GET; if it has expired, ask them to retry.
    let at_ms = now_ms();
    let bound = if let Ok(mut logins) = state.logins.lock() {
        match logins.get_mut(&form.uuid) {
            Some(pending) if pending.challenge == form.challenge => {
                pending.email = Some(view.email.clone());
                pending.at_ms = at_ms;
                true
            }
            _ => false,
        }
    } else {
        false
    };
    if !bound {
        return refuse("This sign-in link expired. Return to the app and try again.");
    }

    tracing::info!(email = %view.email, uuid = %form.uuid, "loginDeepControl: credentials bound; poll can now complete");
    super::pages::message(
        StatusCode::OK,
        "Signed in",
        &format!(
            "Signed in as {}. Return to the app — it will pick up your session automatically.",
            view.email
        ),
    )
}

/// The sentence a spent login budget shows — the same on the styled page and in the console's
/// JSON, and it names the wait so nobody keeps trying blind.
fn too_many_attempts(spent: super::budget::Spent) -> String {
    format!(
        "Too many failed sign-in attempts from this address. Try again in {} minutes.",
        spent.retry_after_secs.div_ceil(60).max(1)
    )
}

/// Check an email + password against a credential account. `Ok(view)` on success; `Err((status,
/// message))` is a client-readable refusal naming which distinct reason applied — unverified, not
/// enabled, or wrong credentials — the same distinctions the styled login shows. Shared by the
/// desktop PKCE form and the browser console's cookie login.
pub(crate) async fn authenticate(
    state: &AuthState,
    email: &str,
    password: &str,
) -> Result<opengrok_core::account::AccountView, (StatusCode, String)> {
    let wrong = || {
        (
            StatusCode::UNAUTHORIZED,
            "Wrong email or password.".to_string(),
        )
    };
    let Ok(Some(view)) = state.store.account_by_email(email).await else {
        // Same message as a bad password: an attacker must not learn which emails exist.
        return Err(wrong());
    };
    let Ok((account, _)) = state.store.load_account(&view.id).await else {
        return Err(wrong());
    };
    let hash = match account.credential_login_ready() {
        Ok(hash) => hash.to_string(),
        Err(AccountError::NotVerified) => {
            return Err((
                StatusCode::FORBIDDEN,
                "Your email is not verified yet. Check your inbox for the link.".to_string(),
            ));
        }
        Err(AccountError::NotEnabled) => {
            return Err((
                StatusCode::FORBIDDEN,
                "Your account is awaiting an administrator's approval.".to_string(),
            ));
        }
        // NoCredentials (a dev/session-only account) reads as wrong credentials — it has no
        // password to log in with.
        Err(_) => return Err(wrong()),
    };
    if !super::password::verify_password(password, &hash) {
        return Err(wrong());
    }
    Ok(view)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieLoginRequest {
    pub email: String,
    pub password: String,
}

/// `POST /auth/login` — the web console's direct credential login. Authenticates, mints a session,
/// and stores it in httpOnly cookies; the body carries only the email, never a token. Distinct
/// from the desktop client's PKCE `/loginDeepControl`, which is untouched.
pub async fn login_cookie(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(req): Json<CookieLoginRequest>,
) -> Response {
    let peer = super::budget::peer_key(&headers);
    if let Err(spent) = state.budgets.check(&super::budget::LOGIN_FAILURES, &peer) {
        return super::budget::too_many(spent, &too_many_attempts(spent));
    }
    let view = match authenticate(&state, &req.email, &req.password).await {
        Ok(view) => view,
        Err((status, message)) => {
            state.budgets.hit(&super::budget::LOGIN_FAILURES, &peer);
            return (status, Json(serde_json::json!({ "error": message }))).into_response();
        }
    };
    match mint_session(&state, &view.email, view.plan, view.trial).await {
        Ok((access, refresh)) => session_cookie_response(
            StatusCode::OK,
            serde_json::json!({ "email": view.email }),
            &access,
            &refresh,
        ),
        Err(error) => error.into_response(),
    }
}

/// `POST /auth/logout` — clear the session cookies. The access token expires on its own within the
/// hour; dropping the cookies is what ends the browser's session now.
pub async fn logout_cookie() -> Response {
    use super::cookies::{ACCESS_COOKIE, REFRESH_COOKIE, clear_cookie};
    let mut response = (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response();
    append_cookie(&mut response, &clear_cookie(ACCESS_COOKIE));
    append_cookie(&mut response, &clear_cookie(REFRESH_COOKIE));
    response
}

/// `POST /auth/refresh` — the console's rotation. Reads the refresh cookie, rotates the session,
/// and re-sets both cookies. The SPA calls this once on a 401 and retries the original request.
pub async fn refresh_cookie(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
) -> Response {
    use super::cookies::{ACCESS_COOKIE, REFRESH_COOKIE, clear_cookie, read_cookie};
    let Some(refresh) = read_cookie(&headers, REFRESH_COOKIE) else {
        return (StatusCode::UNAUTHORIZED, "no session").into_response();
    };
    match rotate(&state, refresh).await {
        Ok((access, new_refresh)) => session_cookie_response(
            StatusCode::OK,
            serde_json::json!({ "ok": true }),
            &access,
            &new_refresh,
        ),
        Err(_) => {
            // The refresh token is not ours to honour — clear the stale cookies so the SPA stops
            // retrying and shows the sign-in page.
            let mut response = (StatusCode::UNAUTHORIZED, "session expired").into_response();
            append_cookie(&mut response, &clear_cookie(ACCESS_COOKIE));
            append_cookie(&mut response, &clear_cookie(REFRESH_COOKIE));
            response
        }
    }
}

/// A JSON response that also sets both session cookies.
fn session_cookie_response(
    status: StatusCode,
    body: serde_json::Value,
    access: &str,
    refresh: &str,
) -> Response {
    use super::cookies::{
        ACCESS_COOKIE, REFRESH_COOKIE, REFRESH_COOKIE_MAX_AGE_SECONDS, set_cookie,
    };
    let mut response = (status, Json(body)).into_response();
    append_cookie(
        &mut response,
        &set_cookie(ACCESS_COOKIE, access, ACCESS_TOKEN_TTL_SECONDS),
    );
    append_cookie(
        &mut response,
        &set_cookie(REFRESH_COOKIE, refresh, REFRESH_COOKIE_MAX_AGE_SECONDS),
    );
    response
}

/// Append one `Set-Cookie` header. A value that will not parse as a header is dropped rather than
/// panicked on — our cookie values are token text, so this only guards against the impossible.
fn append_cookie(response: &mut Response, value: &str) {
    if let Ok(header) = axum::http::HeaderValue::from_str(value) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, header);
    }
}

#[derive(Debug, Deserialize)]
pub struct AuthPollQuery {
    pub uuid: String,
    pub verifier: String,
}

/// `GET /auth/poll?uuid=&verifier=` — the client's polling half (`login.ts:36`).
///
/// The client reads **404 as "not ready, keep polling"** and only a 200 `{accessToken,
/// refreshToken}` as done. So an unregistered uuid, an expired one, or a verifier whose hash does
/// not match the registered challenge all answer **404** — a blind probe (`uuid=x&verifier=y`)
/// polls forever and is never handed a token. Only the browser that registered the challenge, and
/// the client that holds the matching verifier, complete. The entry is consumed on success:
/// one challenge, one token.
pub async fn auth_poll(
    State(state): State<AuthState>,
    Query(query): Query<AuthPollQuery>,
) -> Response {
    let matched = {
        let at_ms = now_ms();
        let Ok(mut logins) = state.logins.lock() else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "login store unavailable").into_response();
        };
        logins.retain(|_, pending| at_ms - pending.at_ms < LOGIN_TTL_MS);
        match logins.get(&query.uuid) {
            // The PKCE check: the verifier the client holds must hash to the challenge the
            // browser registered. A mismatch reads as pending, never as a distinct error, so a
            // uuid-guesser learns nothing and completes nothing.
            Some(pending)
                if pending.email.is_some()
                    && challenge_for(&query.verifier) == pending.challenge =>
            {
                let email = pending.email.clone();
                logins.remove(&query.uuid);
                email
            }
            _ => None,
        }
    };

    let Some(email) = matched else {
        // Pending — exactly what the client waits on.
        return (StatusCode::NOT_FOUND, "pending").into_response();
    };
    tracing::info!(email = %email, uuid = %query.uuid, "auth/poll: releasing a session token to the client");

    match mint_session(&state, &email, Plan::Ultra, false).await {
        Ok((access_token, refresh_token)) => Json(DevSessionReply {
            access_token,
            refresh_token,
        })
        .into_response(),
        Err(error) => error.into_response(),
    }
}

/// `GET /auth/cursor_dev_session_token?plan=&trial=&email=`
///
/// Query parameters as the client builds them at `cursor-auth.ts:313`: `plan` always, `trial=true`
/// only when the tier was a trial tier, `email` only when non-empty.
#[derive(Debug, Deserialize)]
pub struct DevSessionQuery {
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub trial: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// camelCase — see the module note.
#[derive(Debug, Serialize)]
pub struct DevSessionReply {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
}

/// snake_case — see the module note.
#[derive(Debug, Serialize)]
pub struct OAuthTokenReply {
    pub access_token: String,
    pub refresh_token: String,
}

/// What the client POSTs to `/oauth/token` (`cursor-auth.ts:340`).
#[derive(Debug, Deserialize)]
pub struct OAuthTokenRequest {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub grant_type: Option<String>,
    pub refresh_token: String,
}

/// A failure the client can act on.
///
/// `shouldLogout: true` is the client's instruction to drop the session and show the sign-in wall
/// (`cursor-auth.ts:344-345`). We send it only when the refresh token is genuinely not ours to
/// honour — never on a transient database error, which would sign people out during an outage.
#[derive(Debug, Serialize)]
pub struct AuthErrorBody {
    pub error: String,
    #[serde(rename = "shouldLogout", skip_serializing_if = "Option::is_none")]
    pub should_logout: Option<bool>,
}

pub enum AuthFailure {
    /// The session is gone or the token was never ours. Sign out.
    SessionRejected(String),
    /// Something on our side broke. The client keeps its session and retries.
    Unavailable(String),
}

impl IntoResponse for AuthFailure {
    fn into_response(self) -> Response {
        match self {
            Self::SessionRejected(error) => (
                StatusCode::UNAUTHORIZED,
                Json(AuthErrorBody {
                    error,
                    should_logout: Some(true),
                }),
            )
                .into_response(),
            Self::Unavailable(error) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(AuthErrorBody {
                    error,
                    should_logout: None,
                }),
            )
                .into_response(),
        }
    }
}

impl From<StoreError> for AuthFailure {
    fn from(error: StoreError) -> Self {
        Self::Unavailable(error.to_string())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Sign in. Idempotent per email: a second call adds a session, never a second account.
pub async fn dev_session_token(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<DevSessionQuery>,
) -> Result<Json<DevSessionReply>, AuthFailure> {
    // LOOPBACK ONLY. This mints a real account token with no browser step, which is exactly right
    // for the smoke scripts (they hit 127.0.0.1) and exactly wrong for a LAN host — the desktop
    // test binds 0.0.0.0, and an unauthenticated token mint reachable across the network is the
    // hole the browser login leg exists to close. Off-loopback callers use /loginDeepControl.
    if !is_loopback(&headers) {
        return Err(AuthFailure::SessionRejected(
            "dev sign-in is loopback-only; use the browser login".to_string(),
        ));
    }
    let plan = Plan::from_wire(query.plan.as_deref().unwrap_or_default());
    // The client only ever sends `trial=true`, and only when it means it (`cursor-auth.ts:313`).
    let trial = query.trial.as_deref() == Some("true");
    let email = query
        .email
        .filter(|email| !email.is_empty())
        // No email arrives when the tier had none; a stable placeholder keeps the account
        // identifiable across launches instead of minting a new one each time.
        .unwrap_or_else(|| "dev@opengrok.local".to_string());

    let (access_token, refresh_token) = mint_session(&state, &email, plan, trial).await?;
    Ok(Json(DevSessionReply {
        access_token,
        refresh_token,
    }))
}

/// Is this request from loopback? Matches the gateway's own posture — the `Host` header names a
/// loopback address. A missing or non-loopback host is treated as remote.
fn is_loopback(headers: &axum::http::HeaderMap) -> bool {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// Sign an email in and mint its `{access, refresh}` pair — the shared core of every login path,
/// browser or dev. Read side answers "does this person exist"; the write side replays their log
/// and appends a fresh session in one transaction.
async fn mint_session(
    state: &AuthState,
    email: &str,
    plan: Plan,
    trial: bool,
) -> Result<(String, String), AuthFailure> {
    let account_id = match state.store.account_by_email(email).await? {
        Some(view) => view.id,
        None => AccountId::new(),
    };
    let (account, seq) = state.store.load_account(&account_id).await?;

    let session_id = SessionId::new();
    let refresh_token = mint_refresh_token();
    let at_ms = now_ms();

    let events = account
        .decide(AccountCommand::SignIn {
            email: email.to_string(),
            plan,
            trial,
            session_id: session_id.clone(),
            refresh_token_hash: hash_refresh_token(&refresh_token),
            at_ms,
        })
        .map_err(|error: AccountError| AuthFailure::SessionRejected(error.to_string()))?;

    // Write the account's REAL state, not a session stub. append_account upserts the projection,
    // so a bare session_only view would clobber the person's name, avatar and enabled flag on every
    // sign-in — invisible to /account (which reads the aggregate) but corrupting to every reader of
    // the projection, the admin user list included.
    let mut after = account;
    for event in &events {
        after.apply(event);
    }
    let view = opengrok_core::account::AccountView {
        id: account_id.clone(),
        email: email.to_string(),
        plan: after.plan.unwrap_or(plan),
        trial,
        updated_at_ms: at_ms,
        password_hash: after.password_hash.clone(),
        first_name: after.first_name.clone(),
        last_name: after.last_name.clone(),
        org_id: after.org_id.clone(),
        verified: after.verified,
        enabled: after.enabled,
        avatar_url: after.avatar_url.clone(),
    };
    state
        .store
        .append_account(&account_id, seq, &events, &view)
        .await?;

    let access_token = state
        .minter
        .mint_access(
            account_id.as_str(),
            session_id.as_str(),
            email,
            plan.as_wire(),
            at_ms / 1_000,
            ACCESS_TOKEN_TTL_SECONDS,
        )
        .map_err(|error| AuthFailure::Unavailable(error.to_string()))?;

    Ok((access_token, refresh_token))
}

/// Rotate. The client calls this constantly — `shouldRefreshAccessToken` is unconditionally true
/// against a dev backend (`cursor-token.ts:50`), so this is the hot path, not the rare one.
pub async fn oauth_token(
    State(state): State<AuthState>,
    Json(request): Json<OAuthTokenRequest>,
) -> Result<Json<OAuthTokenReply>, AuthFailure> {
    if let Some(grant) = request.grant_type.as_deref()
        && grant != "refresh_token"
    {
        return Err(AuthFailure::SessionRejected(format!(
            "unsupported grant_type {grant}"
        )));
    }

    let (access_token, refresh_token) = rotate(&state, request.refresh_token).await?;
    Ok(Json(OAuthTokenReply {
        access_token,
        refresh_token,
    }))
}

/// Rotate a refresh token into a fresh `{access, refresh}` pair. The shared core of both the
/// desktop client's `/oauth/token` and the browser console's cookie `/auth/refresh`: look the
/// presented token up by hash, append the rotation to the session's log, and mint a new access
/// token bound to that session.
async fn rotate(state: &AuthState, refresh_token: String) -> Result<(String, String), AuthFailure> {
    let presented_hash = hash_refresh_token(&refresh_token);
    let account_id = state
        .store
        .account_by_refresh_hash(&presented_hash)
        .await?
        .ok_or_else(|| AuthFailure::SessionRejected("unknown refresh token".to_string()))?;

    let (account, seq) = state.store.load_account(&account_id).await?;
    let new_refresh = mint_refresh_token();
    let at_ms = now_ms();

    let events = account
        .decide(AccountCommand::Refresh {
            presented_hash,
            new_hash: hash_refresh_token(&new_refresh),
            at_ms,
        })
        .map_err(|error| AuthFailure::SessionRejected(error.to_string()))?;

    // The session the rotation belongs to — needed for the new access token's `sid`.
    let session_id = events
        .iter()
        .find_map(|event| match event {
            opengrok_core::account::AccountEvent::SessionRefreshed { session_id, .. } => {
                Some(session_id.clone())
            }
            _ => None,
        })
        .ok_or_else(|| AuthFailure::Unavailable("refresh produced no session".to_string()))?;

    let plan = account.plan.unwrap_or(Plan::Ultra);
    // Preserve the projection on refresh too — a rotation must not wipe the profile (see the note
    // in mint_session).
    let mut after = account;
    for event in &events {
        after.apply(event);
    }
    let view = opengrok_core::account::AccountView {
        id: account_id.clone(),
        email: after.email.clone(),
        plan: after.plan.unwrap_or(plan),
        trial: after.trial,
        updated_at_ms: at_ms,
        password_hash: after.password_hash.clone(),
        first_name: after.first_name.clone(),
        last_name: after.last_name.clone(),
        org_id: after.org_id.clone(),
        verified: after.verified,
        enabled: after.enabled,
        avatar_url: after.avatar_url.clone(),
    };
    state
        .store
        .append_account(&account_id, seq, &events, &view)
        .await?;

    let access_token = state
        .minter
        .mint_access(
            account_id.as_str(),
            session_id.as_str(),
            &after.email,
            plan.as_wire(),
            at_ms / 1_000,
            ACCESS_TOKEN_TTL_SECONDS,
        )
        .map_err(|error| AuthFailure::Unavailable(error.to_string()))?;

    Ok((access_token, new_refresh))
}
