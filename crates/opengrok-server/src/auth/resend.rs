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

/// Send the signup verification email. Returns whether Resend accepted it.
pub async fn send_verification(api_key: &str, to: &str, link: &str) -> bool {
    send(
        api_key,
        to,
        "Verify your OpenGrok email",
        &format!(
            "<p>Welcome to OpenGrok. Confirm this address to finish signing up:</p>\
             <p><a href=\"{link}\">Verify my email</a></p>\
             <p style=\"color:#888;font-size:13px\">If you did not sign up, ignore this email.</p>"
        ),
    )
    .await
}

/// Send the password-reset email. The link is good for one hour and for one change.
pub async fn send_password_reset(api_key: &str, to: &str, link: &str) -> bool {
    send(
        api_key,
        to,
        "Reset your OpenGrok password",
        &format!(
            "<p>Somebody asked to reset the password for this OpenGrok account.</p>\
             <p><a href=\"{link}\">Choose a new password</a></p>\
             <p style=\"color:#888;font-size:13px\">The link works once and expires in an hour. \
             If this was not you, ignore this email — your password has not changed.</p>"
        ),
    )
    .await
}

async fn send(api_key: &str, to: &str, subject: &str, html: &str) -> bool {
    // The sender identity. Its DOMAIN must be verified in the Resend account, or Resend rejects
    // the send — so this is a real address under a domain the operator controls, not a placeholder.
    let from_email =
        std::env::var("RESEND_FROM_EMAIL").unwrap_or_else(|_| "support@goldcoders.dev".to_string());
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
        .post("https://api.resend.com/emails")
        .bearer_auth(api_key)
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
