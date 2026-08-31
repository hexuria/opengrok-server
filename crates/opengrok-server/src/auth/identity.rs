//! Signup and email verification — the user-facing half of the identity model.
//!
//! Admin actions (create an org, mint an invite, enable an account, create a test account) are the
//! CLI's job (`crates/opengrok/src/admin.rs`), because the operator has shell on the box and an
//! HTTP admin surface is attack surface we do not need in v1. This module is what a *user* reaches:
//! sign up with an invite code, then verify their email.
//!
//! SIGNUP PASSES BOTH GATES OR IT PASSES NEITHER. The org aggregate's `RedeemInvite` checks that
//! the code is open AND the email's domain is one the org registered — either failing is a
//! distinct, readable refusal. Only then is the account registered (enabled=false: an admin still
//! enables it; verified per the mailer). If Resend is configured a verification email goes out;
//! if not, the address auto-verifies, exactly as Uriah scoped it.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_core::org::{OrgCommand, email_domain};

use super::routes::AuthState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignupRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    /// The invite code the org admin issued. Required.
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct SignupReply {
    pub account_id: String,
    /// True when a verification email was sent; false when it auto-verified (no mailer).
    pub verification_email_sent: bool,
    /// True once the account can attempt login (verified) — still needs an admin to enable it.
    pub verified: bool,
}

/// The claims a verification link carries — a signed one-time proof, not a guessable token.
#[derive(Debug, Serialize, Deserialize)]
struct VerifyClaims {
    #[serde(rename = "use")]
    purpose: String,
    sub: String,
    exp: i64,
}

/// `GET /signup?code=` — the styled signup page the admin's invite link points at.
pub async fn signup_page(
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    super::pages::signup(query.get("code").map(String::as_str), None)
}

/// `POST /signup` — the styled form's target (form-encoded, distinct from the JSON /auth/signup
/// the client calls). Runs the same signup, then renders a page instead of JSON.
pub async fn signup_form(
    State(state): State<AuthState>,
    axum::Form(form): axum::Form<SignupRequest>,
) -> Response {
    let code = form.code.clone();
    match do_signup(&state, form).await {
        Ok(reply) => {
            let msg = if reply.verified {
                "Account created. Your administrator will enable it, then you can sign in."
            } else {
                "Account created. Check your email for a verification link, then your administrator                  will enable your account."
            };
            super::pages::message(StatusCode::OK, "Welcome to Open Grok", msg)
        }
        Err((_, message)) => super::pages::signup(Some(&code), Some(&message)),
    }
}

/// `POST /auth/signup` — create an account under an org, gated by invite code + domain.
/// The signup work, shared by the JSON endpoint and the styled form. `Err((status, message))`
/// carries a client-readable reason both callers render their own way.
async fn do_signup(
    state: &AuthState,
    req: SignupRequest,
) -> Result<SignupReply, (StatusCode, String)> {
    if req.password.len() < 8 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "password must be at least 8 characters".to_string(),
        ));
    }
    let Some(domain) = email_domain(&req.email) else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "that is not an email address".to_string(),
        ));
    };

    // The code names the org; the org checks the code and the domain together.
    let Ok(Some(org_id)) = state.store.org_by_invite(&req.code).await else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown invite code".to_string(),
        ));
    };
    let Ok((org, org_seq)) = state.store.load_org(&org_id).await else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "org unavailable".to_string(),
        ));
    };

    // An email already registered is refused before we spend the invite.
    if let Ok(Some(_)) = state.store.account_by_email(&req.email).await {
        return Err((
            StatusCode::CONFLICT,
            "an account with that email already exists".to_string(),
        ));
    }

    let Ok(password_hash) = super::password::hash_password(&req.password) else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not secure the password".to_string(),
        ));
    };

    let account_id = AccountId::new();
    // Redeem first — this is where invite-open AND domain-match are enforced, atomically, and it
    // is the step that refuses a stranger's gmail even with a real code.
    let redeem = match org.decide(OrgCommand::RedeemInvite {
        code: req.code.clone(),
        email_domain: domain,
        account: account_id.clone(),
        at_ms: now_ms(),
    }) {
        Ok(events) => events,
        Err(reason) => return Err((StatusCode::FORBIDDEN, reason.to_string())),
    };

    // No mailer ⇒ verified immediately; a mailer ⇒ pending until the link is clicked.
    let auto_verified = state.resend_api_key.is_none();
    let at_ms = now_ms();

    // Register the account (enabled=false: an admin still enables it).
    let register = match Account::default().decide(AccountCommand::Register {
        email: req.email.clone(),
        password_hash: password_hash.clone(),
        first_name: req.first_name.clone(),
        last_name: req.last_name.clone(),
        org_id: org_id.as_str().to_string(),
        plan: Plan::Ultra,
        verified: auto_verified,
        enabled: false,
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => return Err((StatusCode::INTERNAL_SERVER_ERROR, reason.to_string())),
    };
    let account = Account::replay(&register);
    let view = AccountView {
        id: account_id.clone(),
        email: req.email.clone(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some(password_hash),
        first_name: req.first_name.clone(),
        last_name: req.last_name.clone(),
        org_id: Some(org_id.as_str().to_string()),
        verified: account.verified,
        enabled: false,
        avatar_url: None,
    };
    if state
        .store
        .append_account(&account_id, 0, &register, &view)
        .await
        .is_err()
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not create the account".to_string(),
        ));
    }
    // The invite is spent only after the account exists.
    let mut org_state = org;
    for event in &redeem {
        org_state.apply(event);
    }
    let _ = state
        .store
        .append_org(&org_id, org_seq, &redeem, &org_state, at_ms)
        .await;

    // Send the verification email, if a mailer is configured. A send failure does not fail the
    // signup — the account exists and the operator can re-trigger; failing here would strand a
    // real account behind a mail hiccup.
    let mut sent = false;
    if let Some(key) = &state.resend_api_key {
        let token = mint_verify_token(state, account_id.as_str(), at_ms);
        if let Some(token) = token {
            let link = format!("{}/auth/verify?token={token}", state.public_url);
            sent = super::resend::send_verification(key, &req.email, &link).await;
        }
    }

    Ok(SignupReply {
        account_id: account_id.as_str().to_string(),
        verification_email_sent: sent,
        verified: account.verified,
    })
}

/// `POST /auth/signup` — the JSON endpoint the client calls. Wraps `do_signup`.
pub async fn signup(State(state): State<AuthState>, Json(req): Json<SignupRequest>) -> Response {
    match do_signup(&state, req).await {
        Ok(reply) => (StatusCode::CREATED, Json(reply)).into_response(),
        Err((status, message)) => {
            (status, Json(serde_json::json!({ "error": message }))).into_response()
        }
    }
}

fn mint_verify_token(state: &AuthState, account_id: &str, at_ms: i64) -> Option<String> {
    state
        .minter
        .mint_claims(&VerifyClaims {
            purpose: "email-verify".to_string(),
            sub: account_id.to_string(),
            exp: at_ms / 1_000 + 24 * 60 * 60,
        })
        .ok()
}

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    pub token: String,
}

/// `GET /auth/verify?token=` — the link in the verification email. Marks the account verified.
pub async fn verify_email(
    State(state): State<AuthState>,
    axum::extract::Query(query): axum::extract::Query<VerifyQuery>,
) -> Response {
    let Ok(claims) = state.minter.verify_claims::<VerifyClaims>(&query.token) else {
        return super::pages::message(
            StatusCode::BAD_REQUEST,
            "Verification",
            "This verification link is invalid or expired.",
        );
    };
    if claims.purpose != "email-verify" {
        return super::pages::message(
            StatusCode::BAD_REQUEST,
            "Verification",
            "This link cannot be used to verify email.",
        );
    }
    let account_id = AccountId::from_stored(claims.sub);
    let Ok((account, seq)) = state.store.load_account(&account_id).await else {
        return super::pages::message(StatusCode::NOT_FOUND, "Verification", "No such account.");
    };
    if account.verified {
        return super::pages::message(
            StatusCode::OK,
            "Verification",
            "Your email is already verified. You can sign in.",
        );
    }
    let events = match account.decide(AccountCommand::VerifyEmail { at_ms: now_ms() }) {
        Ok(events) => events,
        Err(reason) => {
            return super::pages::message(
                StatusCode::CONFLICT,
                "Verification",
                &reason.to_string(),
            );
        }
    };
    let mut after = account;
    for event in &events {
        after.apply(event);
    }
    let view = AccountView {
        id: account_id.clone(),
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
    if state
        .store
        .append_account(&account_id, seq, &events, &view)
        .await
        .is_err()
    {
        return super::pages::message(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Verification",
            "Could not record verification.",
        );
    }
    super::pages::message(
        StatusCode::OK,
        "Email verified",
        "Your email is verified. Your account is active once an administrator enables it.",
    )
}
