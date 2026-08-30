//! Sending verification email through Resend (resend.com), when configured.
//!
//! Gated by the presence of an API key: no key, no send, and signup auto-verifies instead. The
//! key is passed in from `AuthState`, never read from the environment here, so this module holds
//! no configuration of its own and cannot leak it.
//!
//! A send failure is reported as `false`, not an error that fails the signup — the account exists
//! and the operator can re-trigger; stranding a real account behind a mail hiccup is worse than a
//! delayed email.

use serde_json::json;

/// Send the verification email. Returns whether Resend accepted it.
pub async fn send_verification(api_key: &str, to: &str, link: &str) -> bool {
    let from = std::env::var("OG_RESEND_FROM")
        .unwrap_or_else(|_| "OpenGrok <onboarding@resend.dev>".to_string());
    let body = json!({
        "from": from,
        "to": [to],
        "subject": "Verify your OpenGrok email",
        "html": format!(
            "<p>Welcome to OpenGrok. Confirm this address to finish signing up:</p>\
             <p><a href=\"{link}\">Verify my email</a></p>\
             <p style=\"color:#888;font-size:13px\">If you did not sign up, ignore this email.</p>"
        ),
    });
    let client = reqwest::Client::new();
    match client
        .post("https://api.resend.com/emails")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(response) => response.status().is_success(),
        Err(error) => {
            tracing::warn!(%error, "Resend send failed; the account still exists unverified");
            false
        }
    }
}
