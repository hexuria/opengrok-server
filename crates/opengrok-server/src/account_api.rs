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
use axum::routing::{get, post};
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
        .with_state(state)
}

/// The caller's account, from the bearer, loaded — the shared front of every handler here.
async fn caller(
    state: &AuthState,
    headers: &axum::http::HeaderMap,
) -> Result<(AccountId, Account, i64), Response> {
    // account_from_bearer lives on AgUiState; the auth store is the same store, so build the check
    // against it directly rather than threading AgUiState in.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = token else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    let Ok(claims) = state.minter.verify_access(token) else {
        return Err((StatusCode::UNAUTHORIZED, "bad token").into_response());
    };
    let id = AccountId::from_stored(claims.sub);
    match state.store.load_account(&id).await {
        Ok((account, seq)) => Ok((id, account, seq)),
        Err(_) => Err((StatusCode::UNAUTHORIZED, "no such account").into_response()),
    }
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
        Ok((id, account, _)) => Json(account_json(&id, &account)).into_response(),
        Err(refusal) => refusal,
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
async fn persist(
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

/// The caller's org and a confirmation they are its admin — or a refusal.
async fn admin_org(
    state: &AuthState,
    headers: &axum::http::HeaderMap,
) -> Result<(OrgId, opengrok_core::org::Org), Response> {
    let (id, account, _) = caller(state, headers).await?;
    let Some(org_id) = account
        .org_id
        .as_ref()
        .map(|o| OrgId::from_stored(o.clone()))
    else {
        return Err((StatusCode::FORBIDDEN, "you are not in an organization").into_response());
    };
    let Ok((org, _)) = state.store.load_org(&org_id).await else {
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
    Ok((org_id, org))
}

/// `GET /admin/users` — the org's members with their state.
async fn list_users(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    let (_, org) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let mut users = Vec::new();
    for member in &org.members {
        if let Ok((account, _)) = state.store.load_account(member).await {
            users.push(account_json(member, &account));
        }
    }
    // The admin themselves, who is not always in `members` (they created the org).
    Json(json!({ "users": users })).into_response()
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
    if let Err(refusal) = admin_org(&state, &headers).await {
        return refusal;
    }
    set_enabled(&state, &id, false).await
}

/// `POST /admin/invites` — issue a code, and hand back both the code and a paste-or-click signup
/// link. No expiry, single-use, no seat limit (all v1 decisions).
async fn issue_invite(State(state): State<AuthState>, headers: axum::http::HeaderMap) -> Response {
    let (org_id, org) = match admin_org(&state, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let (_, _, seq) = match caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let _ = seq;
    let code = format!("inv_{}", uuid::Uuid::now_v7().simple());
    let at_ms = now_ms();
    let events = match org.decide(OrgCommand::IssueInvite {
        code: code.clone(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => return (StatusCode::CONFLICT, reason.to_string()).into_response(),
    };
    let mut after = org;
    for event in &events {
        after.apply(event);
    }
    // The org's own seq — reload to append correctly.
    let Ok((_, org_seq)) = state.store.load_org(&org_id).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "org unavailable").into_response();
    };
    if state
        .store
        .append_org(&org_id, org_seq, &events, &after, at_ms)
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not issue the invite",
        )
            .into_response();
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
    let (_, org) = match admin_org(&state, &headers).await {
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
