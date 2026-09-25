//! Sending account email through Resend (resend.com), when configured.
//!
//! Gated by the presence of an API key: no key, no send — signup auto-verifies instead, and the
//! password-reset page says plainly that this server cannot email. The key is passed in from
//! `AuthState`, never read from the environment here, so this module holds no configuration of
//! its own and cannot leak it.
//!
//! A send failure is reported as `false`, not an error that fails the caller — an account exists
//! and the operator can re-trigger; stranding a real account behind a mail hiccup is worse than a
//! delayed email. The link in either mail is a signed one-time token: never log it.

use serde_json::json;

/// Resend's send endpoint. A field on `AuthState` rather than a literal in `send`, so a test can
/// stand a mailbox in for it and read the link a mail carried instead of trusting a reply that is
/// constant by design.
pub const ENDPOINT: &str = "https://api.resend.com/emails";

/// Where a send goes and the key it goes with. Built per send from `AuthState`
/// (`AuthState::mailer`); `None` there means this deployment sends no mail.
#[derive(Debug, Clone)]
pub struct Mailer {
    pub endpoint: String,
    pub key: String,
}

/// Send the signup verification email. Returns whether Resend accepted it.
pub async fn send_verification(mailer: &Mailer, to: &str, link: &str) -> bool {
    send(
        mailer,
        to,
        "Verify your Open Grok email",
        &format!(
            "<p>Welcome to Open Grok. Confirm this address to finish signing up:</p>\
             <p><a href=\"{link}\">Verify my email</a></p>\
             <p style=\"color:#888;font-size:13px\">If you did not sign up, ignore this email.</p>"
        ),
    )
    .await
}

/// Send the password-reset email. The link is good for one hour and for one change.
pub async fn send_password_reset(mailer: &Mailer, to: &str, link: &str) -> bool {
    send(
        mailer,
        to,
        "Reset your Open Grok password",
        &format!(
            "<p>Somebody asked to reset the password for this Open Grok account.</p>\
             <p><a href=\"{link}\">Choose a new password</a></p>\
             <p style=\"color:#888;font-size:13px\">The link works once and expires in an hour. \
             If this was not you, ignore this email — your password has not changed.</p>"
        ),
    )
    .await
}

/// The sender when `RESEND_FROM_EMAIL` is unset. Its DOMAIN must be verified in the Resend
/// account or every send is refused — which on anyone else's deployment it is not, so the binary
/// warns at startup when a key is set without a sender of the operator's own.
pub const DEFAULT_FROM_EMAIL: &str = "support@goldcoders.dev";

async fn send(mailer: &Mailer, to: &str, subject: &str, html: &str) -> bool {
    // An empty value is the `.env` line left blank, not a sender: read as unset.
    let from_email = std::env::var("RESEND_FROM_EMAIL")
        .ok()
        .filter(|from| !from.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_FROM_EMAIL.to_string());
    let from_name =
        std::env::var("RESEND_FROM_NAME").unwrap_or_else(|_| "Open Grok Support Team".to_string());
    let from = format!("{from_name} <{from_email}>");
    let body = json!({
        "from": from,
        "to": [to],
        "subject": subject,
        "html": html,
    });
    let client = reqwest::Client::new();
    match client
        .post(&mailer.endpoint)
        .bearer_auth(&mailer.key)
        .json(&body)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                true
            } else {
                // Resend's rejection reason (unverified from-domain, bad key, …) is in the body;
                // without it a failed send is a silent mystery. Logged, not returned to the client.
                let detail = response.text().await.unwrap_or_default();
                tracing::warn!(%status, detail, subject, "Resend rejected the send");
                false
            }
        }
        Err(error) => {
            tracing::warn!(%error, subject, "Resend send failed");
            false
        }
    }
}
