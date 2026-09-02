//! Password reset by email — the second half of 12.later.
//!
//! THE LINK IS THE PROOF. A reset token is a signed claim (`use = password-reset`) carrying the
//! account and a fingerprint of the password hash it was minted against. It expires in an hour,
//! and it works ONCE without the server keeping a table of spent tokens: changing the password
//! changes the fingerprint, so the same link presented again no longer matches. A person who
//! opens the link twice sees "already used", not a second password change.
//!
//! NOTHING HERE SAYS WHETHER AN ACCOUNT EXISTS. "Forgot" answers the same way for a real address
//! and a stranger's guess. The one thing it does disclose is whether this server can send email
//! at all — that is deployment configuration, not identity, and lying about it would strand a
//! person waiting for a mail that will never come. With no mailer the page says to ask the
//! administrator, who resets from the shell (`opengrok admin account password`).
//!
//! The link is never logged. It is a credential until it is spent.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use opengrok_core::account::AccountCommand;
use opengrok_core::id::AccountId;

use super::routes::AuthState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const PURPOSE: &str = "password-reset";
const LIFETIME_SECS: i64 = 60 * 60;

#[derive(Debug, Serialize, Deserialize)]
struct ResetClaims {
    #[serde(rename = "use")]
    purpose: String,
    sub: String,
    exp: i64,
    /// Fingerprint of the password hash at mint time — what makes the token single-use.
    fp: String,
}

/// A short, non-reversible tag of the current hash. Sixteen hex chars is plenty to tell "the
/// password this link was minted for" from "any later one"; it is not a secret.
fn fingerprint(password_hash: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(password_hash.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Mint a reset token for an account. Pub so an integration test can walk the reset page
/// without a mailbox; the server itself only hands it to Resend.
pub fn mint_reset_token(
    state: &AuthState,
    account_id: &AccountId,
    password_hash: &str,
    at_ms: i64,
) -> Option<String> {
    state
        .minter
        .mint_claims(&ResetClaims {
            purpose: PURPOSE.to_string(),
            sub: account_id.as_str().to_string(),
            exp: at_ms / 1_000 + LIFETIME_SECS,
            fp: fingerprint(password_hash),
        })
        .ok()
}

#[derive(Debug, Deserialize)]
pub struct ForgotRequest {
    pub email: String,
}

/// Start a reset for `email` if — and only if — it names a credential account. The mail goes
/// out on its own task so a known address answers in the same time as an unknown one: the
/// Resend round-trip is the one thing that would otherwise tell them apart. Whether it was sent
/// is logged, never returned — there is nothing the caller may do with it.
async fn start_reset(state: &AuthState, email: &str) {
    let Some(key) = state.resend_api_key.clone() else {
        return;
    };
    let Ok(Some(view)) = state.store.account_by_email(email.trim()).await else {
        return;
    };
    let Some(hash) = view.password_hash.as_deref() else {
        // A dev-login account has no password to reset.
        return;
    };
    let Some(token) = mint_reset_token(state, &view.id, hash, now_ms()) else {
        return;
    };
    let link = format!("{}/reset-password?token={token}", state.public_url);
    tokio::spawn(async move {
        let sent = super::resend::send_password_reset(&key, &view.email, &link).await;
        tracing::info!(account = %view.id, sent, "password reset requested");
    });
}

/// `GET /forgot-password` — the styled "send me a link" card, honest about the mailer.
pub async fn forgot_page(State(state): State<AuthState>) -> Response {
    super::pages::forgot_password(state.resend_api_key.is_some(), None)
}

/// `POST /forgot-password` — the card's target. One answer regardless of whether the address is
/// known.
pub async fn forgot_form(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<ForgotRequest>,
) -> Response {
    if let Err(spent) = forgot_budget(&state, &headers, &form.email) {
        return super::budget::with_retry_after(
            super::pages::message(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests",
                &too_many_resets(spent),
            ),
            spent,
        );
    }
    if state.resend_api_key.is_none() {
        return super::pages::forgot_password(false, None);
    }
    start_reset(&state, &form.email).await;
    super::pages::message(
        StatusCode::OK,
        "Check your email",
        "If that address has an account here, a reset link is on its way. It works once and \
         expires in an hour.",
    )
}

/// `POST /auth/password/forgot` — the JSON the console's login page calls. `202` either way;
/// `mailer` tells the page whether to say "check your email" or "ask your administrator".
pub async fn forgot_json(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ForgotRequest>,
) -> Response {
    if let Err(spent) = forgot_budget(&state, &headers, &req.email) {
        return super::budget::too_many(spent, &too_many_resets(spent));
    }
    let mailer = state.resend_api_key.is_some();
    if mailer {
        start_reset(&state, &req.email).await;
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true, "mailer": mailer })),
    )
        .into_response()
}

/// The budget is on the REQUEST, not the mail: it is charged whether or not a mailer is wired
/// and whether or not the address has an account, so the constant reply stays constant.
fn forgot_budget(
    state: &AuthState,
    headers: &axum::http::HeaderMap,
    email: &str,
) -> Result<(), super::budget::Spent> {
    use super::budget::{FORGOT, email_key, peer_key};
    state.budgets.take(&FORGOT, &peer_key(headers))?;
    state.budgets.take(&FORGOT, &email_key(email))
}

fn too_many_resets(spent: super::budget::Spent) -> String {
    format!(
        "Too many password-reset requests. Try again in {} minutes.",
        spent.retry_after_secs.div_ceil(60).max(1)
    )
}

#[derive(Debug, Deserialize)]
pub struct ResetQuery {
    pub token: String,
}

/// `GET /reset-password?token=` — the emailed link. Checks the token before showing the form so
/// a dead link says so immediately instead of after the person typed a password twice.
pub async fn reset_page(
    State(state): State<AuthState>,
    Query(query): Query<ResetQuery>,
) -> Response {
    match account_for(&state, &query.token).await {
        Ok(_) => super::pages::reset_password(&query.token, None),
        Err(message) => super::pages::message(StatusCode::BAD_REQUEST, "Reset password", message),
    }
}

#[derive(Debug, Deserialize)]
pub struct ResetForm {
    pub token: String,
    pub password: String,
    pub confirm: String,
}

/// `POST /reset-password` — set the new password. The token is re-checked here: the page check
/// was a courtesy, this is the gate.
pub async fn reset_form(
    State(state): State<AuthState>,
    axum::Form(form): axum::Form<ResetForm>,
) -> Response {
    let (account_id, account, seq) = match account_for(&state, &form.token).await {
        Ok(found) => found,
        Err(message) => {
            return super::pages::message(StatusCode::BAD_REQUEST, "Reset password", message);
        }
    };
    if form.password.len() < 8 {
        return super::pages::reset_password(
            &form.token,
            Some("the password must be at least 8 characters"),
        );
    }
    if form.password != form.confirm {
        return super::pages::reset_password(&form.token, Some("the two passwords do not match"));
    }
    let Ok(new_hash) = super::password::hash_password(&form.password) else {
        return super::pages::message(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Reset password",
            "Could not secure the password.",
        );
    };
    let events = match account.decide(AccountCommand::ChangePassword {
        password_hash: new_hash,
        at_ms: now_ms(),
    }) {
        Ok(events) => events,
        Err(reason) => {
            return super::pages::message(
                StatusCode::CONFLICT,
                "Reset password",
                &reason.to_string(),
            );
        }
    };
    if crate::account_api::persist(&state, &account_id, account, seq, &events)
        .await
        .is_err()
    {
        return super::pages::message(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Reset password",
            "Could not record the new password.",
        );
    }
    tracing::info!(account = %account_id, "password reset completed");
    super::pages::message(
        StatusCode::OK,
        "Password updated",
        "Your password has been changed. You can sign in with it now.",
    )
}

/// The account a token is good for right now — or the sentence to show instead.
async fn account_for(
    state: &AuthState,
    token: &str,
) -> Result<(AccountId, opengrok_core::account::Account, i64), &'static str> {
    let Ok(claims) = state.minter.verify_claims::<ResetClaims>(token) else {
        return Err("This reset link is invalid or has expired. Request a new one.");
    };
    if claims.purpose != PURPOSE {
        return Err("This link cannot be used to reset a password.");
    }
    let account_id = AccountId::from_stored(claims.sub);
    let Ok((account, seq)) = state.store.load_account(&account_id).await else {
        return Err("This reset link is invalid or has expired. Request a new one.");
    };
    let Some(hash) = account.password_hash.as_deref() else {
        return Err("This account has no password to reset.");
    };
    if fingerprint(hash) != claims.fp {
        return Err("This reset link has already been used. Request a new one if you need to.");
    }
    Ok((account_id, account, seq))
}
