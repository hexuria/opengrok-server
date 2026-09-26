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
            // THREE OUTCOMES, NOT TWO. Choosing on `verified` alone told a person whose mail was
            // never sent to go and wait for it — the account was then stuck until someone read
            // the server log.
            let msg = match (reply.verified, reply.verification_email_sent) {
                (true, _) => {
                    "Account created. Your administrator will enable it, then you can sign in."
                }
                (false, true) => {
                    "Account created. Check your email for a verification link, then your \
                     administrator will enable your account."
                }
                (false, false) => {
                    "Account created, but the verification email could not be sent. Ask your \
                     administrator to verify and enable your account."
                }
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
    // SPENT FIRST, THEN CREATED (#237). The code used to be spent after the account existed, and
    // a conflict on that append was ignored: two signups with one code both got accounts, and a
    // signup racing any other org write (an invite issued, a domain claimed) kept its account
    // while the code stayed open for the next person. A double-spend is a security bug; a spent
    // code with no account behind it is an admin issuing a new one. So the second is the failure
    // this order can leave, and `append_account` below says so when it does.
    redeem_invite(
        &state.store,
        &org_id,
        (org, org_seq),
        &req.code,
        &domain,
        &account_id,
    )
    .await?;

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
    // Not rare: the email check above is a read, so two codes signing up one address at once both
    // pass it, and the second append conflicts on the unique email after its code is spent.
    if let Err(error) = state
        .store
        .append_account(&account_id, 0, &register, &view)
        .await
    {
        tracing::error!(
            %error,
            org = %org_id.as_str(),
            code = %req.code,
            account = %account_id.as_str(),
            "signup: the invite was spent but the account could not be created"
        );
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "the invite was used but the account could not be created; ask your admin for a \
             new invite"
                .to_string(),
        ));
    }
    if let Some(created) = &state.account_created {
        let _ = created.send(account_id.clone());
    }
    // Send the verification email, if a mailer is configured. A send failure does not fail the
    // signup — the account exists, and its org's admin vouches for the address instead
    // (`POST /admin/users/{id}/verify`, `opengrok admin account verify`); failing here would
    // strand a real account behind a mail hiccup.
    let mut sent = false;
    if let Some(mailer) = state.mailer()
        && let Some(link) = verification_link(state, &account_id, at_ms)
    {
        sent = super::resend::send_verification(&mailer, &req.email, &link).await;
    }

    Ok(SignupReply {
        account_id: account_id.as_str().to_string(),
        verification_email_sent: sent,
        verified: account.verified,
    })
}

/// How many times a redemption re-reads the org after another write took its seq. Each retry
/// follows a write that DID land, so running out means the org is busier than a signup should
/// wait for, not that anything is wrong.
const REDEEM_ATTEMPTS: usize = 5;

/// Spend `code` on `account` in the org loaded as `loaded` (the org and its stream seq). Checking
/// the code is open and the domain matches happens in `decide`, and the append at the seq it was
/// decided on is what makes that check atomic: exactly one writer wins each seq. Only `Conflict`
/// is retried, on a fresh read, where a racing signup now finds the code spent (403). Any other
/// error may be a commit whose reply was lost (`formal/tla/JournalAppend_truth.cfg`), so it is
/// never retried and the reply does not claim the code is still unused.
pub async fn redeem_invite(
    store: &opengrok_store::PgStore,
    org_id: &opengrok_core::id::OrgId,
    loaded: (opengrok_core::org::Org, i64),
    code: &str,
    email_domain: &str,
    account: &AccountId,
) -> Result<(), (StatusCode, String)> {
    let (mut org, mut org_seq) = loaded;
    for _ in 0..REDEEM_ATTEMPTS {
        let at_ms = now_ms();
        let redeem = org
            .decide(OrgCommand::RedeemInvite {
                code: code.to_string(),
                email_domain: email_domain.to_string(),
                account: account.clone(),
                at_ms,
            })
            .map_err(|reason| (StatusCode::FORBIDDEN, reason.to_string()))?;
        let mut after = org.clone();
        for event in &redeem {
            after.apply(event);
        }
        match store
            .append_org(org_id, org_seq, &redeem, &after, at_ms)
            .await
        {
            Ok(_) => return Ok(()),
            Err(opengrok_store::StoreError::Conflict) => {
                (org, org_seq) = store.load_org(org_id).await.map_err(|error| {
                    tracing::error!(%error, org = %org_id.as_str(), "signup: the org could not be re-read");
                    (StatusCode::INTERNAL_SERVER_ERROR, "org unavailable".to_string())
                })?;
            }
            Err(error) => {
                tracing::error!(%error, org = %org_id.as_str(), code, "signup: the invite could not be redeemed");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the invite could not be redeemed, and it may have been used; ask your admin \
                     whether it is still open"
                        .to_string(),
                ));
            }
        }
    }
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        "the organization is busy; try again in a moment".to_string(),
    ))
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

/// The link a verification mail carries: a signed claim good for 24 hours from `at_ms`. Minted
/// fresh for every mail, so a resent link has its own full day whatever became of the first.
fn verification_link(state: &AuthState, account_id: &AccountId, at_ms: i64) -> Option<String> {
    state
        .minter
        .mint_claims(&VerifyClaims {
            purpose: "email-verify".to_string(),
            sub: account_id.as_str().to_string(),
            exp: at_ms / 1_000 + 24 * 60 * 60,
        })
        .ok()
        .map(|token| format!("{}/auth/verify?token={token}", state.public_url))
}

#[derive(Debug, Deserialize)]
pub struct ResendRequest {
    pub email: String,
}

/// Mail a fresh link to `email` if — and only if — it names a credential account still waiting to
/// be verified. The mail goes out on its own task so every address answers in the same time: the
/// Resend round trip is the one thing that would tell a stranded account from a stranger's guess.
/// Whether it was sent is logged, never returned.
async fn start_resend(state: &AuthState, email: &str) {
    let Some(mailer) = state.mailer() else {
        return;
    };
    let Ok(Some(view)) = state.store.account_by_email(email.trim()).await else {
        return;
    };
    // A verified address has nothing to prove; a password-less (dev) account has no login that
    // verification could unlock.
    if view.verified || view.password_hash.is_none() {
        return;
    }
    let Some(link) = verification_link(state, &view.id, now_ms()) else {
        return;
    };
    tokio::spawn(async move {
        let sent = super::resend::send_verification(&mailer, &view.email, &link).await;
        tracing::info!(account = %view.id, sent, "verification mail resent");
    });
}

/// Charged on the REQUEST, per address and per mailbox, whether or not a mailer is wired and
/// whatever the address names — the same bargain as a reset, so the reply stays constant and one
/// mailbox cannot be flooded from many peers.
fn resend_budget(
    state: &AuthState,
    headers: &axum::http::HeaderMap,
    email: &str,
) -> Result<(), super::budget::Spent> {
    use super::budget::{VERIFY_RESEND, email_key, peer_key};
    state.budgets.take(&VERIFY_RESEND, &peer_key(headers))?;
    state.budgets.take(&VERIFY_RESEND, &email_key(email))
}

fn too_many_resends(spent: super::budget::Spent) -> String {
    format!(
        "Too many verification-email requests. Try again in {} minutes.",
        spent.retry_after_secs.div_ceil(60).max(1)
    )
}

/// `POST /auth/verify/resend` — the JSON the console's login page calls. `202` whether the address
/// is unknown, already verified or waiting; `mailer` tells the page whether to say "check your
/// email" or "ask your administrator", which is deployment configuration, not identity.
pub async fn resend_json(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ResendRequest>,
) -> Response {
    if let Err(spent) = resend_budget(&state, &headers, &req.email) {
        return super::budget::too_many(spent, &too_many_resends(spent));
    }
    start_resend(&state, &req.email).await;
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true, "mailer": state.resend_api_key.is_some() })),
    )
        .into_response()
}

/// `GET /resend-verification` — the styled card the sign-in page links to, honest about the mailer.
pub async fn resend_page(State(state): State<AuthState>) -> Response {
    super::pages::resend_verification(state.resend_api_key.is_some())
}

/// `POST /resend-verification` — the card's target. One answer for every address.
pub async fn resend_form(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<ResendRequest>,
) -> Response {
    if let Err(spent) = resend_budget(&state, &headers, &form.email) {
        return super::budget::with_retry_after(
            super::pages::message(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests",
                &too_many_resends(spent),
            ),
            spent,
        );
    }
    if state.resend_api_key.is_none() {
        return super::pages::resend_verification(false);
    }
    start_resend(&state, &form.email).await;
    super::pages::message(
        StatusCode::OK,
        "Check your email",
        "If that address has an account here that is still waiting to be verified, a new link is \
         on its way. It expires in 24 hours.",
    )
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
