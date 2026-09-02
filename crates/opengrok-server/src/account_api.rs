//! The account self-service and org-admin endpoints — the JSON the web console (and the desktop
//! app) call.
//!
//! Every route here is authenticated by the account access token. Two authority levels:
//!   - a signed-in person may edit THEIR OWN name, avatar and password (never their email — it is
//!     the identity their org and their invite were bound to);
//!   - the org's ADMIN may list the org's users, enable/disable them, and issue invite codes.
//!
//! Admin authority is the org's `admin` field, checked per request against the caller's account —
//! there is no ambient "is admin" flag, so losing the admin role is immediate, not cached.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, OrgId};
use opengrok_core::org::OrgCommand;

use crate::auth::AuthState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route("/account", get(me))
        .route("/account/profile", post(update_profile))
        .route("/account/password", post(change_password))
        .route("/admin/users", get(list_users))
        .route("/admin/users/{id}/enable", post(enable_user))
        .route("/admin/users/{id}/disable", post(disable_user))
        .route("/admin/invites", get(list_invites).post(issue_invite))
        // Domain ownership (12.later): a console admin claims a domain, publishes the TXT record
        // we hand back, and asks us to check it. Only a verified domain admits signups.
        .route("/admin/domains", get(list_domains).post(claim_domain))
        .route("/admin/domains/{domain}", delete(withdraw_domain))
        .route("/admin/domains/{domain}/verify", post(verify_domain))
        // One identity, two doors: gateway keys for the org's members. See `gateway_admin`.
        .route(
            "/admin/gateway/keys",
            get(list_gateway_keys).post(mint_gateway_key),
        )
        .route("/admin/gateway/keys/{id}", delete(revoke_gateway_key))
        .route("/admin/gateway/keys/{id}/quota", put(set_gateway_key_quota))
        .route("/admin/gateway/budget", put(set_gateway_budget))
        .route("/admin/gateway/usage", get(gateway_usage))
        .with_state(state)
}

/// The caller's account, from the bearer, loaded — the shared front of every handler here.
pub(crate) async fn caller(
    state: &AuthState,
    headers: &axum::http::HeaderMap,
) -> Result<(AccountId, Account, i64), Response> {
    // account_from_bearer lives on AgUiState; the auth store is the same store, so build the check
    // against it directly rather than threading AgUiState in.
    // The desktop/API clients present `Authorization: Bearer`; the browser console presents the
    // `og_access` httpOnly cookie. Either is a valid token source — the cookie is just the header
    // the browser can carry on navigation without a script ever touching the token.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            crate::auth::cookies::read_cookie(headers, crate::auth::cookies::ACCESS_COOKIE)
        });
    let Some(token) = token else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    let Ok(claims) = state.minter.verify_access(&token) else {
        return Err((StatusCode::UNAUTHORIZED, "bad token").into_response());
    };
    let id = AccountId::from_stored(claims.sub);
    match state.store.load_account(&id).await {
        Ok((account, seq)) => Ok((id, account, seq)),
        Err(_) => Err((StatusCode::UNAUTHORIZED, "no such account").into_response()),
    }
}

fn account_json_from_view(view: &AccountView) -> Value {
    json!({
        "id": view.id.as_str(),
        "email": view.email,
        "firstName": view.first_name,
        "lastName": view.last_name,
        "avatarUrl": view.avatar_url,
        "orgId": view.org_id,
        "verified": view.verified,
        "enabled": view.enabled,
    })
}

fn account_json(id: &AccountId, account: &Account) -> Value {
    json!({
        "id": id.as_str(),
        "email": account.email,
        "firstName": account.first_name,
        "lastName": account.last_name,
        "avatarUrl": account.avatar_url,
        "orgId": account.org_id,
        "verified": account.verified,
        "enabled": account.enabled,
    })
}

/// `GET /account` — the signed-in person's own profile (email included; it is theirs to read,
/// not to change).
async fn me(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    match caller(&state, &headers).await {
        Ok((id, account, _)) => {
            // Tell the caller whether they are their org's admin, so the console can hide the admin
            // surface for a member rather than offer a door that only answers 403. The admin checks
            // below still enforce it server-side; this is the client's cue, not the gate.
            let is_admin = caller_is_admin(&state, &id, &account).await;
            let mut body = account_json(&id, &account);
            body["isAdmin"] = json!(is_admin);
            Json(body).into_response()
        }
        Err(refusal) => refusal,
    }
}

/// Is this caller the admin of their own org? False when they are in no org, the org is gone, or
/// someone else is its admin.
async fn caller_is_admin(state: &AuthState, id: &AccountId, account: &Account) -> bool {
    let Some(org_id) = account
        .org_id
        .as_ref()
        .map(|o| OrgId::from_stored(o.clone()))
    else {
        return false;
    };
    match state.store.load_org(&org_id).await {
        Ok((org, _)) => org.admin.as_ref() == Some(id),
        Err(_) => false,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileUpdate {
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
    /// A data: URL. Inline for now — the artifacts store is a later slice. Cleared with `null`.
    #[serde(default)]
    avatar_url: Option<Option<String>>,
}

/// `POST /account/profile` — update your own name and/or avatar. Email is not a field here.
async fn update_profile(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(update): Json<ProfileUpdate>,
) -> Response {
    let (id, account, seq) = match caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    // A data: URL avatar is capped so one person cannot store a megabyte in their row; the real
    // home is the artifacts store, and this cap is the seam that makes moving there painless.
    if let Some(Some(url)) = &update.avatar_url {
        if !url.is_empty() && !url.starts_with("data:image/") {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "avatar must be a data:image/ URL",
            )
                .into_response();
        }
        if url.len() > 512 * 1024 {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "avatar is too large (512 KB max)",
            )
                .into_response();
        }
    }
    let first = update
        .first_name
        .unwrap_or_else(|| account.first_name.clone());
    let last = update
        .last_name
        .unwrap_or_else(|| account.last_name.clone());
    let avatar = match update.avatar_url {
        Some(Some(url)) if url.is_empty() => None,
        Some(value) => value,
        None => account.avatar_url.clone(),
    };

    let events = match account.decide(AccountCommand::UpdateProfile {
        first_name: first,
        last_name: last,
        avatar_url: avatar,
        at_ms: now_ms(),
    }) {
        Ok(events) => events,
        Err(reason) => {
            return (StatusCode::UNPROCESSABLE_ENTITY, reason.to_string()).into_response();
        }
    };
    match persist(&state, &id, account, seq, &events).await {
        Ok(after) => Json(account_json(&id, &after)).into_response(),
        Err(refusal) => refusal,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// `POST /account/password` — change your own password, proving the current one first.
async fn change_password(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(change): Json<PasswordChange>,
) -> Response {
    let (id, account, seq) = match caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    if change.new_password.len() < 8 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "new password must be at least 8 characters",
        )
            .into_response();
    }
    let Some(hash) = &account.password_hash else {
        return (
            StatusCode::CONFLICT,
            "this account has no password to change",
        )
            .into_response();
    };
    if !super::password_verify(&change.current_password, hash) {
        return (StatusCode::FORBIDDEN, "the current password is wrong").into_response();
    }
    let Ok(new_hash) = super::password_hash(&change.new_password) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not secure the password",
        )
            .into_response();
    };
    let events = match account.decide(AccountCommand::ChangePassword {
        password_hash: new_hash,
        at_ms: now_ms(),
    }) {
        Ok(events) => events,
        Err(reason) => return (StatusCode::CONFLICT, reason.to_string()).into_response(),
    };
    match persist(&state, &id, account, seq, &events).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => refusal,
    }
}

/// Apply events to the account, project, and store — the write half every self-service edit shares.
pub(crate) async fn persist(
    state: &AuthState,
    id: &AccountId,
    account: Account,
    seq: i64,
    events: &[opengrok_core::account::AccountEvent],
) -> Result<Account, Response> {
    let mut after = account;
    for event in events {
        after.apply(event);
    }
    let view = AccountView {
        id: id.clone(),
        email: after.email.clone(),
        plan: after.plan.unwrap_or(Plan::Ultra),
        trial: after.trial,
        updated_at_ms: now_ms(),
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
        .append_account(id, seq, events, &view)
        .await
        .map_err(|error| {
            tracing::error!(%error, "could not persist an account change");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        })?;
    Ok(after)
}

// ---- Admin: the org's admin manages its users ----

/// The caller's org, the seq it was loaded at, and a confirmation they are its admin — or a
/// refusal. The seq travels with the org so a write decided against THIS state appends at THIS
/// seq: two admins acting at once cannot both pass `decide` and both land, because the second
/// append conflicts and is told to retry.
pub async fn admin_org(
    state: &AuthState,
    headers: &axum::http::HeaderMap,
) -> Result<(OrgId, opengrok_core::org::Org, i64), Response> {
    let (id, account, _) = caller(state, headers).await?;
    let Some(org_id) = account
        .org_id
        .as_ref()
        .map(|o| OrgId::from_stored(o.clone()))
    else {
        return Err((StatusCode::FORBIDDEN, "you are not in an organization").into_response());
    };
    let Ok((org, org_seq)) = state.store.load_org(&org_id).await else {
        return Err((StatusCode::NOT_FOUND, "no such organization").into_response());
    };
    if org.admin.as_ref() != Some(&id) {
        // Not the admin — 403, and the message does not confirm the org's shape to a member.
        return Err((
            StatusCode::FORBIDDEN,
            "only the organization's admin may do that",
        )
            .into_response());
    }
    Ok((org_id, org, org_seq))
}

/// `GET /admin/users` — the org's members with their state.
async fn list_users(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    let (org_id, _org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    // Every account in the org — CLI-created, signed-up, and the admin themselves — not only those
    // who happened to redeem an invite (which is all `org.members` records).
    match state.store.accounts_by_org(org_id.as_str()).await {
        Ok(views) => {
            let mut users = Vec::new();
            for view in &views {
                let mut json = account_json_from_view(view);
                // Each member's per-account override (null = follows the org default).
                let mode = state
                    .store
                    .sharing_mode("account", view.id.as_str())
                    .await
                    .ok()
                    .flatten();
                if let Some(object) = json.as_object_mut() {
                    object.insert("computerMode".to_string(), json!(mode));
                }
                users.push(json);
            }
            Json(json!({ "users": users })).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not list users").into_response(),
    }
}

async fn set_enabled(state: &AuthState, id: &str, enabled: bool) -> Response {
    let account_id = AccountId::from_stored(id.to_string());
    let Ok((account, seq)) = state.store.load_account(&account_id).await else {
        return (StatusCode::NOT_FOUND, "no such user").into_response();
    };
    let command = if enabled {
        AccountCommand::Enable { at_ms: now_ms() }
    } else {
        AccountCommand::Disable { at_ms: now_ms() }
    };
    let events = match account.decide(command) {
        Ok(events) => events,
        Err(reason) => return (StatusCode::CONFLICT, reason.to_string()).into_response(),
    };
    match persist(state, &account_id, account, seq, &events).await {
        Ok(after) => Json(account_json(&account_id, &after)).into_response(),
        Err(refusal) => refusal,
    }
}

async fn enable_user(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(refusal) = admin_org(&state, &headers).await {
        return refusal;
    }
    set_enabled(&state, &id, true).await
}

async fn disable_user(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let caller = match caller(&state, &headers).await {
        Ok((caller_id, _, _)) => caller_id,
        Err(refusal) => return refusal,
    };
    if let Err(refusal) = admin_org(&state, &headers).await {
        return refusal;
    }
    // Disabling yourself could lock the org out (a disabled account cannot sign in). Refuse it —
    // an admin who wants to leave hands the role over first.
    if caller.as_str() == id {
        return (StatusCode::CONFLICT, "you cannot disable your own account").into_response();
    }
    set_enabled(&state, &id, false).await
}

/// `POST /admin/invites` — issue a code, and hand back both the code and a paste-or-click signup
/// link. No expiry, single-use, no seat limit (all v1 decisions).
async fn issue_invite(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    let (org_id, org, org_seq) = match admin_org(&state, &headers).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let code = format!("inv_{}", uuid::Uuid::now_v7().simple());
    let at_ms = now_ms();
    let events = match org.decide(OrgCommand::IssueInvite {
        code: code.clone(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => return (StatusCode::CONFLICT, reason.to_string()).into_response(),
    };
    if let Err(refusal) = append_org_events(&state, &org_id, org, org_seq, &events).await {
        return refusal;
    }
    // The link the admin can send: the signup page with the code prefilled. Bare code included so
    // they can also just paste it, exactly as Uriah asked.
    let link = format!("{}/signup?code={code}", state.public_url);
    (
        StatusCode::CREATED,
        Json(json!({ "code": code, "link": link })),
    )
        .into_response()
}

/// `GET /admin/invites` — the org's outstanding codes and their state.
async fn list_invites(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    let (_, org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let invites: Vec<Value> = org
        .invites
        .iter()
        .map(|(code, invite_state)| {
            let state_label = match invite_state {
                opengrok_core::org::InviteState::Open => "open",
                opengrok_core::org::InviteState::Redeemed(_) => "redeemed",
                opengrok_core::org::InviteState::Revoked => "revoked",
            };
            json!({ "code": code, "state": state_label })
        })
        .collect();
    Json(json!({ "invites": invites })).into_response()
}

// ── Domain ownership: claim, publish, verify ──────────────────────────────────────────────────
//
// The org aggregate holds the claim and decides what admits a signup; this surface is the only
// thing that turns "the TXT record resolved" into `VerifyDomain`. The check is a live lookup on
// every verify click — there is no background poller, because the admin is sitting there and a
// button that says exactly why it failed beats a status that changes on its own.

fn domain_json(domain: &str, pending_token: Option<&str>) -> Value {
    match pending_token {
        None => json!({ "domain": domain, "status": "verified" }),
        Some(token) => json!({
            "domain": domain,
            "status": "pending",
            "record": {
                "name": opengrok_core::org::challenge_record_name(domain),
                "type": "TXT",
                "value": opengrok_core::org::challenge_record_value(token),
            },
        }),
    }
}

/// Apply events to the org and append them at `org_seq` — the seq the org was LOADED at, from
/// `admin_org`. Not a fresh reload: the decision was made against that state, and appending at a
/// later seq would let two concurrent writers both pass `decide` and both land (the second
/// domain claim would silently replace the first admin's token). A stale seq is the store's
/// `Conflict`, answered 409 so the caller reloads and tries again.
async fn append_org_events(
    state: &AuthState,
    org_id: &OrgId,
    org: opengrok_core::org::Org,
    org_seq: i64,
    events: &[opengrok_core::org::OrgEvent],
) -> Result<opengrok_core::org::Org, Response> {
    let mut after = org;
    for event in events {
        after.apply(event);
    }
    state
        .store
        .append_org(org_id, org_seq, events, &after, now_ms())
        .await
        .map_err(|error| match error {
            opengrok_store::StoreError::Conflict => (
                StatusCode::CONFLICT,
                Json(json!({ "error": "another change to the organization landed first; reload and retry" })),
            )
                .into_response(),
            other => {
                tracing::error!(error = %other, "could not persist an org change");
                (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
            }
        })?;
    Ok(after)
}

/// `GET /admin/domains` — verified domains and pending claims, each pending one with the exact
/// TXT record to publish.
async fn list_domains(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    let (_, org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let mut domains: Vec<Value> = org
        .domains
        .iter()
        .map(|domain| domain_json(domain, None))
        .collect();
    domains.extend(
        org.pending_domains
            .iter()
            .map(|(domain, token)| domain_json(domain, Some(token))),
    );
    Json(json!({ "domains": domains })).into_response()
}

#[derive(Debug, Deserialize)]
struct DomainClaim {
    domain: String,
}

/// `POST /admin/domains` — claim a domain. Answers with the record to publish; admits nothing yet.
async fn claim_domain(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(claim): Json<DomainClaim>,
) -> Response {
    let (org_id, org, org_seq) = match admin_org(&state, &headers).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let token = {
        use rand::RngExt;
        let bytes: [u8; 16] = rand::rng().random();
        format!(
            "dv_{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    };
    let domain = opengrok_core::org::normalize_domain(&claim.domain);
    let events = match org.decide(OrgCommand::ClaimDomain {
        domain: domain.clone(),
        token: token.clone(),
        at_ms: now_ms(),
    }) {
        Ok(events) => events,
        Err(reason) => {
            let status = match reason {
                opengrok_core::org::OrgError::InvalidDomain => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::CONFLICT,
            };
            return (status, Json(json!({ "error": reason.to_string() }))).into_response();
        }
    };
    match append_org_events(&state, &org_id, org, org_seq, &events).await {
        Ok(_) => (
            StatusCode::CREATED,
            Json(domain_json(&domain, Some(&token))),
        )
            .into_response(),
        Err(refusal) => refusal,
    }
}

/// `POST /admin/domains/{domain}/verify` — look the record up now. 200 verified, 409 with the
/// reason it is not, 503 when the resolver itself could not answer.
async fn verify_domain(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Path(domain): Path<String>,
) -> Response {
    let (org_id, org, org_seq) = match admin_org(&state, &headers).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let domain = opengrok_core::org::normalize_domain(&domain);
    if org.domains.contains(&domain) {
        return Json(domain_json(&domain, None)).into_response();
    }
    // Every verify is a resolver round trip; the org, not the admin, holds the budget.
    if let Err(spent) = state
        .budgets
        .take(&crate::auth::budget::DOMAIN_VERIFY, org_id.as_str())
    {
        return crate::auth::budget::too_many(
            spent,
            &format!(
                "too many verification attempts for this org; try again in {} minutes",
                spent.retry_after_secs.div_ceil(60).max(1)
            ),
        );
    }
    let Some(token) = org.pending_domains.get(&domain).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": opengrok_core::org::OrgError::DomainNotClaimed.to_string() })),
        )
            .into_response();
    };
    use crate::domain_proof::ProofOutcome;
    match crate::domain_proof::check(state.dns.as_ref(), &domain, &token).await {
        ProofOutcome::Proven => {}
        ProofOutcome::NotFound(reason) => {
            return (StatusCode::CONFLICT, Json(json!({ "error": reason }))).into_response();
        }
        ProofOutcome::Unavailable(reason) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": reason })),
            )
                .into_response();
        }
    }
    let events = match org.decide(OrgCommand::VerifyDomain {
        domain: domain.clone(),
        at_ms: now_ms(),
    }) {
        Ok(events) => events,
        Err(reason) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": reason.to_string() })),
            )
                .into_response();
        }
    };
    match append_org_events(&state, &org_id, org, org_seq, &events).await {
        Ok(_) => {
            tracing::info!(org = %org_id, domain, "domain ownership proven by DNS");
            Json(domain_json(&domain, None)).into_response()
        }
        Err(refusal) => refusal,
    }
}

/// `DELETE /admin/domains/{domain}` — withdraw a PENDING claim. A verified domain is not
/// removable here: that would lock members out of their own sign-in, and is the operator's call.
async fn withdraw_domain(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Path(domain): Path<String>,
) -> Response {
    let (org_id, org, org_seq) = match admin_org(&state, &headers).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let events = match org.decide(OrgCommand::WithdrawDomainClaim {
        domain,
        at_ms: now_ms(),
    }) {
        Ok(events) => events,
        Err(reason) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": reason.to_string() })),
            )
                .into_response();
        }
    };
    match append_org_events(&state, &org_id, org, org_seq, &events).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => refusal,
    }
}

// ── Gateway keys: one identity, two doors ─────────────────────────────────────────────────────
//
// The org's admin hands a member a key that opens open-ai-gateway (the model door). We do not
// invent a second credential system for it: the org IS a gateway principal and the member's key IS
// an api_key on that principal, so the org's budget, the member's cap, that member's spend and an
// individual revoke are all the gateway's own machinery (`gateway_admin`). What we keep is
// attribution — which key belongs to whom — because the console must be able to list an org's keys
// without reading every key in the gateway, and must know whose key an id is BEFORE asking the
// gateway to revoke it.
//
// Authority is the same `admin_org` gate as users and invites: only the org's admin, checked per
// request. A key id that belongs to another org answers 404, not 403 — a member of one org must
// not be able to learn that another org's key exists by probing ids.

/// The admin surface this deployment was booted with, or a refusal saying it has none.
///
/// Read from the state, not the environment: it is resolved once at boot, so a handler cannot see
/// a different answer than the one the server started with, and a test can substitute a stand-in.
///
/// `Box`ed refusal: `Response` is large, and a `Result` whose error dwarfs its success value is
/// paid for on every call that succeeds.
fn gateway_admin(state: &AuthState) -> Result<crate::gateway_admin::GatewayAdmin, Box<Response>> {
    state.gateway_admin.clone().ok_or_else(|| {
        Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "this deployment has no gateway admin connection (set OG_GATEWAY_ADMIN_URL and \
                 OG_GATEWAY_ADMIN_TOKEN)",
            )
                .into_response(),
        )
    })
}

fn gateway_refusal(error: &crate::gateway_admin::AdminError) -> Response {
    use crate::gateway_admin::AdminError;
    match error {
        // The gateway's own sentence, which names the real problem, reaches the operator.
        AdminError::Refused(detail) => (
            StatusCode::BAD_GATEWAY,
            format!("the gateway refused: {detail}"),
        )
            .into_response(),
        AdminError::Unreachable(detail) => (
            StatusCode::BAD_GATEWAY,
            format!("the gateway is unreachable: {detail}"),
        )
            .into_response(),
    }
}

/// `GET /admin/gateway/keys` — the org's keys, newest first, with the member each belongs to.
async fn list_gateway_keys(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (org_id, _org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let Ok(rows) = state.store.gateway_keys_for_org(org_id.as_str()).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not list gateway keys",
        )
            .into_response();
    };

    // RECONCILE AGAINST THE AUTHORITY. Our rows are attribution (which member a key belongs
    // to); whether a key still authenticates is the gateway's fact. A revoke that landed in the
    // gateway but failed to mirror here left the console reading "active" (ROADMAP 17.later);
    // now the listing asks the gateway, heals its own rows, and also shows keys the gateway
    // holds for this org's principal that we never recorded (a mint whose attribution insert
    // failed). Unreachable gateway ⇒ the local rows, marked `reconciled: false`, rather than an
    // empty list that reads as "no keys" (CLAUDE.md #3).
    let mut keys: Vec<Value> = Vec::with_capacity(rows.len());
    let mut reconciled = false;
    let mut listed = None;
    if let Some(admin) = state.gateway_admin.as_ref() {
        match admin.org_keys(org_id.as_str()).await {
            Ok(remote) => {
                reconciled = true;
                listed = Some(remote);
            }
            Err(error) => {
                tracing::warn!(%error, org = %org_id, "key listing served unreconciled: the gateway did not answer");
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let mut revoked = row.revoked;
        if let Some(remote) = listed.as_ref() {
            seen.insert(row.key_id.clone());
            if let Some(key) = remote.iter().find(|key| key.id == row.key_id)
                && !key.active
                && !row.revoked
            {
                // The gateway already refuses this key; the row is what was stale.
                tracing::info!(key_id = %row.key_id, "healing a key the gateway revoked but the mirror missed");
                let _ = state
                    .store
                    .mark_gateway_key_revoked(&row.key_id, org_id.as_str())
                    .await;
                revoked = true;
            }
        }
        keys.push(json!({
            "id": row.key_id,
            "memberId": row.member_account_id,
            // The prefix is the only part of a key that is safe to show, and it is
            // what an operator can match against a gateway log during an incident.
            "keyPrefix": row.key_prefix,
            "label": row.label,
            "revoked": revoked,
            "createdAtMs": row.created_at_ms,
            "unattributed": false,
        }));
    }
    if let Some(remote) = listed {
        for key in remote.into_iter().filter(|key| !seen.contains(&key.id)) {
            // Real in the gateway, unknown here: shown so it can be revoked, never hidden.
            keys.push(json!({
                "id": key.id,
                "memberId": Value::Null,
                "keyPrefix": key.key_prefix,
                "label": key.name,
                "revoked": !key.active,
                "createdAtMs": Value::Null,
                "unattributed": true,
            }));
        }
    }
    Json(json!({ "keys": keys, "reconciled": reconciled })).into_response()
}

#[derive(Deserialize)]
struct MintKeyRequest {
    #[serde(rename = "memberId")]
    member_id: String,
    /// A per-member spend cap, as a STRING — money is not a float. Absent means uncapped, bounded
    /// only by the org's own monthly budget.
    #[serde(rename = "quotaUsd")]
    quota_usd: Option<String>,
    /// The console's nonce for this press. A repeat with the same nonce (a reply lost to a
    /// timeout, a double click) finds the key that press already minted instead of minting a
    /// second real one. Optional: a caller that sends none gets the old non-idempotent mint.
    #[serde(rename = "clientNonce")]
    client_nonce: Option<String>,
}

/// `POST /admin/gateway/keys` — mint one member a key. Shown ONCE, exactly like a bot key.
async fn mint_gateway_key(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<MintKeyRequest>,
) -> Response {
    let (org_id, _org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let admin = match gateway_admin(&state) {
        Ok(admin) => admin,
        Err(refusal) => return *refusal,
    };

    // The member is looked up WITHIN the org's own roster, so membership is intrinsic rather than a
    // separate comparison somebody could forget: an id outside this org is simply not found. Minting
    // for another org's user would attribute a key — and its spend — to the wrong org.
    let member = match state.store.accounts_by_org(org_id.as_str()).await {
        Ok(views) => match views
            .into_iter()
            .find(|view| view.id.as_str() == request.member_id)
        {
            Some(view) => view,
            None => return (StatusCode::NOT_FOUND, "no such member").into_response(),
        },
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not read the org").into_response();
        }
    };

    // The same press again: answer with what it minted, and say the secret is gone. The
    // plaintext existed only in the reply that was lost; handing it out twice is not possible,
    // and minting again would be the duplicate this nonce exists to prevent.
    let nonce = request
        .client_nonce
        .as_deref()
        .map(str::trim)
        .filter(|nonce| !nonce.is_empty() && nonce.len() <= 128);
    if let Some(nonce) = nonce
        && let Ok(Some(existing)) = state
            .store
            .gateway_key_by_nonce(org_id.as_str(), nonce)
            .await
    {
        return (
            StatusCode::OK,
            Json(json!({
                "id": existing.key_id,
                "memberId": existing.member_account_id,
                "keyPrefix": existing.key_prefix,
                "label": existing.label,
                "key": Value::Null,
                "alreadyMinted": true,
                "note": "this press already minted a key; its secret was shown once and cannot be shown again — revoke it and mint anew if it was lost",
            })),
        )
            .into_response();
    }

    // Idempotent, and cheap: binding the org to its principal before every mint means we never
    // have to remember whether we already did.
    if let Err(error) = admin.ensure_org_principal(org_id.as_str(), None).await {
        return gateway_refusal(&error);
    }
    let label = member.email.clone();
    let minted = match admin
        .mint_member_key(org_id.as_str(), &label, request.quota_usd.as_deref())
        .await
    {
        Ok(minted) => minted,
        Err(error) => return gateway_refusal(&error),
    };

    // Attribution is recorded AFTER the gateway minted, so a row never claims a key that does not
    // exist. If this insert fails the key is real but unattributed — the operator is told, rather
    // than being handed a key the console will never list.
    //
    // The gateway assigns the id, so the mint itself cannot be replayed; the nonce recorded
    // with the row is what makes a repeat of the SAME press find this key (above) rather than
    // mint another. A press that carried no nonce stays non-idempotent — survivable, because
    // every key is listed and individually revocable, and the reconciled listing shows even a
    // key whose attribution insert failed.
    if let Err(error) = state
        .store
        .insert_gateway_key(&opengrok_store::NewGatewayKey {
            key_id: &minted.id,
            org_id: org_id.as_str(),
            member_account_id: member.id.as_str(),
            key_prefix: &minted.key_prefix,
            label: &label,
            mint_nonce: nonce,
            at_ms: now_ms(),
        })
        .await
    {
        // Two presses with one nonce racing: both passed the lookup above, both minted, and the
        // second insert hit the (org, nonce) unique index. Its key is real, live, and nobody has
        // seen its secret — so it is revoked in the gateway here and now, and this press is
        // answered exactly as a repeat of the winner would be. Without this the loser's key sat
        // in the listing as "unattributed" until somebody noticed (review of #21).
        if matches!(error, opengrok_store::StoreError::Conflict)
            && let Some(nonce) = nonce
        {
            if let Err(revoke) = admin.revoke_key(&minted.id).await {
                tracing::error!(%revoke, key_id = %minted.id, "a raced mint's key could not be revoked; it will list as unattributed");
            } else {
                tracing::info!(key_id = %minted.id, "a raced mint lost to its twin; its key is revoked");
            }
            if let Ok(Some(existing)) = state
                .store
                .gateway_key_by_nonce(org_id.as_str(), nonce)
                .await
            {
                return (
                    StatusCode::OK,
                    Json(json!({
                        "id": existing.key_id,
                        "memberId": existing.member_account_id,
                        "keyPrefix": existing.key_prefix,
                        "label": existing.label,
                        "key": Value::Null,
                        "alreadyMinted": true,
                        "note": "this press already minted a key; its secret was shown once and cannot be shown again — revoke it and mint anew if it was lost",
                    })),
                )
                    .into_response();
            }
        }
        tracing::error!(%error, key_id = %minted.id, "minted a gateway key but could not record it");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the key was minted but could not be recorded; it will appear unattributed in the \
             listing — revoke it there and mint again",
        )
            .into_response();
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "id": minted.id,
            "memberId": member.id.as_str(),
            "keyPrefix": minted.key_prefix,
            "label": label,
            // Shown exactly once. We never store it; the gateway has only its hash.
            "key": minted.key,
            "alreadyMinted": false,
        })),
    )
        .into_response()
}

/// `DELETE /admin/gateway/keys/{id}` — revoke in the gateway, then mirror it locally.
async fn revoke_gateway_key(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Path(key_id): Path<String>,
) -> Response {
    let (org_id, _org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    // Ownership first, and a key belonging to another org is simply "no such key".
    match state
        .store
        .gateway_key_in_org(&key_id, org_id.as_str())
        .await
    {
        Ok(Some(_)) => {}
        _ => return (StatusCode::NOT_FOUND, "no such key").into_response(),
    }
    let admin = match gateway_admin(&state) {
        Ok(admin) => admin,
        Err(refusal) => return *refusal,
    };
    // The gateway is the authority on whether the key still authenticates, so it is revoked THERE
    // first. Mirroring locally afterwards only keeps the listing honest; if the mirror fails the
    // key is still dead, which is the safe direction to fail in.
    if let Err(error) = admin.revoke_key(&key_id).await {
        return gateway_refusal(&error);
    }
    // Mirror it, with one retry: the key is already dead either way, but a row still reading
    // "active" makes the console state something untrue, and that is worth a second attempt.
    let mut mirrored = state
        .store
        .mark_gateway_key_revoked(&key_id, org_id.as_str())
        .await;
    if mirrored.is_err() {
        mirrored = state
            .store
            .mark_gateway_key_revoked(&key_id, org_id.as_str())
            .await;
    }
    if let Err(error) = mirrored {
        // ERROR, not warn: the key is revoked but the console will keep showing it as active until
        // somebody reconciles. Reconciling the listing against the gateway is ROADMAP 17.later.
        tracing::error!(%error, %key_id, "revoked in the gateway but could not mirror it locally; the listing will read stale");
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct QuotaRequest {
    #[serde(rename = "quotaUsd")]
    quota_usd: Option<String>,
}

/// `PUT /admin/gateway/keys/{id}/quota` — set or clear one member's spend cap.
async fn set_gateway_key_quota(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Path(key_id): Path<String>,
    Json(request): Json<QuotaRequest>,
) -> Response {
    let (org_id, _org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    match state
        .store
        .gateway_key_in_org(&key_id, org_id.as_str())
        .await
    {
        Ok(Some(_)) => {}
        _ => return (StatusCode::NOT_FOUND, "no such key").into_response(),
    }
    let admin = match gateway_admin(&state) {
        Ok(admin) => admin,
        Err(refusal) => return *refusal,
    };
    match admin
        .set_key_quota(&key_id, request.quota_usd.as_deref())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => gateway_refusal(&error),
    }
}

#[derive(Deserialize)]
struct BudgetRequest {
    #[serde(rename = "monthlyBudgetUsd")]
    monthly_budget_usd: Option<String>,
}

/// `PUT /admin/gateway/budget` — the org's monthly cap, enforced by the gateway on every request.
async fn set_gateway_budget(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<BudgetRequest>,
) -> Response {
    let (org_id, _org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let admin = match gateway_admin(&state) {
        Ok(admin) => admin,
        Err(refusal) => return *refusal,
    };
    // The principal may not exist yet (no keys minted), and setting a budget is a reasonable first
    // move — so bind it, then set.
    if let Err(error) = admin
        .ensure_org_principal(org_id.as_str(), request.monthly_budget_usd.as_deref())
        .await
    {
        return gateway_refusal(&error);
    }
    match admin
        .set_org_budget(org_id.as_str(), request.monthly_budget_usd.as_deref())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => gateway_refusal(&error),
    }
}

/// `GET /admin/gateway/usage` — the org's budget and month-to-date spend, read live.
async fn gateway_usage(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    let (org_id, _org, _) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let admin = match gateway_admin(&state) {
        Ok(admin) => admin,
        Err(refusal) => return *refusal,
    };
    match admin.org_usage(org_id.as_str()).await {
        // No principal yet simply means nobody has been given a key — a zeroed reading, not an error.
        Ok(None) => Json(json!({
            "monthlyBudgetUsd": Value::Null,
            "monthToDateUsd": "0.000000",
            "requests": 0,
            "provisioned": false,
        }))
        .into_response(),
        Ok(Some(usage)) => Json(json!({
            "monthlyBudgetUsd": usage.monthly_budget_usd,
            "monthToDateUsd": usage.month_to_date_usd,
            "requests": usage.requests,
            "provisioned": true,
        }))
        .into_response(),
        Err(error) => gateway_refusal(&error),
    }
}
