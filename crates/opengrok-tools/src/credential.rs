//! Site-login credential protocol (Phase A). Statuses and metadata only.
//!
//! WHY THIS IS NOT THE VAULT. `opengrok-store::Vault` seals connector / API secrets. A Google
//! password must never enter it, Postgres, the journal, or a tool result.
//!
//! SEMANTIC LOCK. `credential.result` `status: "filled"` means the **authenticated session is
//! ready**: NativeChat brokered login out of agent view and applied cookies/profile to the box.
//! It does **not** mean a password was typed into the agent-observable browser. The model and
//! the tool never receive or type a password. OpenGrok orchestrates statuses; it never relays
//! the secret. `session_established` on the wire is accepted as an alias of `filled`.
//!
//! WHY THE MODEL NEVER SEES A PASSWORD. `offer_save` is `{ origin, username, formEntryId }`.
//! `request` is `{ origin, username? }` plus `requestId`. `result` is a status. Accidental
//! `password` keys are scrubbed before anything is journaled.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::user_form::FormRequest;
use std::collections::BTreeMap;

/// Builtin the model calls when a login page is up and a saved match is likely.
pub const REQUEST_CREDENTIAL: &str = "credential.request";

/// CUSTOM NativeChat paints after a successful in-chat user-form fill: save origin+username
/// so the *next* login can be brokered (`credential.request`) instead of typed into the box.
pub const OFFER_SAVE: &str = "credential.offer_save";

/// Keys that must never persist on a credential event, a tool result, or the journal.
/// `secret` / `pass` as boolean flags (`fields[].secret: true` on `request_user_form`) stay;
/// a string under those names is treated as a smuggled value.
const SECRET_KEYS: &[&str] = &["password", "passwd", "pwd"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    /// LOCKED: `filled` = authenticated session ready. Cookies/profile applied to the box after
    /// NativeChat brokered login out of agent view. NOT a password typed into the agent-observable
    /// browser. The model/tool never received or typed a password.
    Filled,
    Denied,
    Missing,
    Error,
}

impl CredentialStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Filled => "filled",
            Self::Denied => "denied",
            Self::Missing => "missing",
            Self::Error => "error",
        }
    }

    #[must_use]
    pub fn from_wire(word: &str) -> Option<Self> {
        match word {
            // Locked success word. `session_established` is the same meaning, accepted as alias.
            "filled" | "session_established" => Some(Self::Filled),
            "denied" => Some(Self::Denied),
            "missing" => Some(Self::Missing),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// `filled`: authenticated session ready (cookies/profile on the box).
    #[must_use]
    pub fn session_ready(self) -> bool {
        matches!(self, Self::Filled)
    }
}

/// Arguments stored on the wait CUSTOM / pending row: origin + optional username. Never a secret.
#[must_use]
pub fn sanitize_request(arguments: &Value) -> Value {
    let origin = origin_of(arguments).unwrap_or_default();
    let mut body = json!({ "origin": origin });
    if let Some(username) = username_of(arguments) {
        body["username"] = json!(username);
    }
    body
}

#[must_use]
pub fn origin_of(value: &Value) -> Option<String> {
    value
        .get("origin")
        .or_else(|| value.get("liveHost"))
        .or_else(|| value.get("domain"))
        .and_then(Value::as_str)
        .map(normalize_origin)
        .filter(|origin| !origin.is_empty())
}

#[must_use]
pub fn username_of(value: &Value) -> Option<String> {
    value
        .get("username")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Host only, lowercase. `https://Accounts.Google.com/login` → `accounts.google.com`.
#[must_use]
pub fn normalize_origin(raw: &str) -> String {
    let trimmed = raw.trim();
    let after_scheme = match trimmed.split_once("://") {
        Some((_, rest)) => rest,
        None => trimmed,
    };
    after_scheme
        .split('/')
        .next()
        .unwrap_or(after_scheme)
        .split(':')
        .next()
        .unwrap_or(after_scheme)
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

#[must_use]
pub fn origin_from_form(form: &FormRequest) -> Option<String> {
    form.live_host
        .as_deref()
        .or(form.domain.as_deref())
        .map(normalize_origin)
        .filter(|origin| !origin.is_empty())
}

#[must_use]
pub fn username_from_shared(shared: &BTreeMap<String, String>) -> String {
    const PREFERRED: &[&str] = &["email", "username", "user", "login", "identifier"];
    for key in PREFERRED {
        if let Some(value) = shared
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
        {
            return value.to_string();
        }
    }
    shared
        .values()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

#[must_use]
pub fn offer_save_payload(origin: &str, username: &str, form_entry_id: &str) -> Value {
    json!({
        "origin": origin,
        "username": username,
        "formEntryId": form_entry_id,
    })
}

/// CUSTOM envelope NativeChat mounts after a successful user-form fill (save for next time).
#[must_use]
pub fn offer_save_frame(origin: &str, username: &str, form_entry_id: &str) -> Value {
    let value = offer_save_payload(origin, username, form_entry_id);
    json!({
        "type": "CUSTOM",
        "name": OFFER_SAVE,
        "origin": origin,
        "username": username,
        "formEntryId": form_entry_id,
        "value": value,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialResult {
    pub status: CredentialStatus,
    pub credential_id: Option<String>,
    pub request_id: Option<String>,
}

/// Parse a client `credential.result`. Status (+ optional ids) only. Password keys ignored.
#[must_use]
pub fn result_from(value: &Value) -> Option<CredentialResult> {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .and_then(CredentialStatus::from_wire)?;
    let credential_id = value
        .get("credentialId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let request_id = value
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    Some(CredentialResult {
        status,
        credential_id,
        request_id,
    })
}

/// What the model reads. Status only. `filled` = authenticated session ready — never that
/// a password was received or typed.
#[must_use]
pub fn tool_result_content(status: CredentialStatus) -> String {
    match status {
        CredentialStatus::Filled => {
            "filled: the authenticated session is ready. Cookies and profile were applied to the box out of your view. You did not receive a password and must not type one. Screenshot and confirm what the page shows. Do not type secrets with `computer`. If another challenge appears, prefer `credential.request` when a saved login is likely, otherwise `request_user_form`."
                .to_string()
        }
        CredentialStatus::Denied => {
            "The person declined to use a saved login. If a password is still needed, call `request_user_form`. Do not type secrets with `computer`."
                .to_string()
        }
        CredentialStatus::Missing => {
            "No saved login matched this origin. Call `request_user_form`. Do not type secrets with `computer`."
                .to_string()
        }
        CredentialStatus::Error => {
            "The saved-login session could not be established. Call `request_user_form`, or try `credential.request` again. Do not type secrets with `computer`."
                .to_string()
        }
    }
}

/// Drop accidental password keys. Recursive. JSON-looking strings (tool-call deltas) are
/// parsed, scrubbed, and re-emitted so a smuggled `password` cannot sit on the journal.
#[must_use]
pub fn scrub_secret_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if is_secret_key(key, child) {
                    continue;
                }
                out.insert(key.clone(), scrub_secret_keys(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(scrub_secret_keys).collect()),
        Value::String(text) => {
            let trimmed = text.trim();
            if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
            {
                let scrubbed = scrub_secret_keys(&parsed);
                return Value::String(
                    serde_json::to_string(&scrubbed).unwrap_or_else(|_| "{}".to_string()),
                );
            }
            Value::String(text.clone())
        }
        other => other.clone(),
    }
}

fn is_secret_key(key: &str, value: &Value) -> bool {
    let lower = key.to_ascii_lowercase();
    if SECRET_KEYS.iter().any(|name| lower == *name) || lower.contains("password") {
        return true;
    }
    // Accidental string secrets; not the user-form field flag `secret: true`.
    if lower == "secret" || lower == "pass" {
        return !value.is_boolean();
    }
    false
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn offer_save_never_includes_a_password() {
        let frame = offer_save_frame("accounts.google.com", "ada@example.com", "e_form");
        let dumped = frame.to_string();
        assert!(
            !dumped.to_ascii_lowercase().contains("password"),
            "{dumped}"
        );
        assert_eq!(frame["name"], OFFER_SAVE);
        assert_eq!(frame["origin"], "accounts.google.com");
        assert_eq!(frame["username"], "ada@example.com");
        assert_eq!(frame["formEntryId"], "e_form");
        assert!(frame.get("password").is_none());
        assert!(frame["value"].get("password").is_none());
    }

    #[test]
    fn result_statuses_round_trip_and_drop_a_password() {
        for status in ["filled", "denied", "missing", "error"] {
            let parsed = result_from(&json!({
                "status": status,
                "credentialId": "cred_1",
                "requestId": "req_1",
                "password": "s3cret-should-never-land",
            }))
            .expect("status");
            assert_eq!(parsed.status.as_str(), status);
            assert_eq!(parsed.credential_id.as_deref(), Some("cred_1"));
            assert_eq!(parsed.request_id.as_deref(), Some("req_1"));
        }
        let alias = result_from(&json!({ "status": "session_established" })).expect("alias");
        assert_eq!(alias.status, CredentialStatus::Filled);
        assert_eq!(alias.status.as_str(), "filled");
        assert!(result_from(&json!({ "status": "nope" })).is_none());
        assert!(CredentialStatus::Filled.session_ready());
        assert!(!CredentialStatus::Denied.session_ready());
    }

    #[test]
    fn filled_means_authenticated_session_ready_not_a_typed_password() {
        let content = tool_result_content(CredentialStatus::Filled);
        let lower = content.to_ascii_lowercase();
        assert!(
            content.contains("authenticated session is ready"),
            "{content}"
        );
        assert!(
            !lower.contains("filled into"),
            "filled must not mean a password was typed into the box: {content}"
        );
        assert!(
            content.contains("You did not receive a password"),
            "{content}"
        );
        assert!(content.contains("Do not type secrets"), "{content}");
        assert!(!content.contains("s3cret"), "{content}");
    }

    #[test]
    fn scrubbing_drops_password_keys_even_inside_a_delta_string() {
        let dirty = json!({
            "name": "credential.request",
            "origin": "accounts.google.com",
            "password": "s3cret-should-never-land",
            "arguments": {
                "origin": "accounts.google.com",
                "password": "s3cret-should-never-land",
                "username": "ada@example.com"
            },
            "delta": "{\"origin\":\"accounts.google.com\",\"password\":\"s3cret-should-never-land\"}"
        });
        let clean = scrub_secret_keys(&dirty);
        let dumped = clean.to_string();
        assert!(!dumped.contains("s3cret-should-never-land"), "{dumped}");
        assert!(clean.get("password").is_none(), "{dumped}");
        assert!(clean["arguments"].get("password").is_none(), "{dumped}");
        assert_eq!(clean["arguments"]["username"], "ada@example.com");
        assert_eq!(clean["origin"], "accounts.google.com");
        let delta = clean["delta"].as_str().expect("delta");
        assert!(!delta.contains("s3cret"), "{delta}");
        assert!(delta.contains("accounts.google.com"), "{delta}");
    }

    #[test]
    fn scrubbing_keeps_a_boolean_secret_flag_on_a_form_field() {
        let form = json!({
            "name": "run-awaiting-approval",
            "arguments": {
                "fields": [{
                    "id": "password",
                    "label": "Password",
                    "type": "password",
                    "secret": true
                }],
                "values": { "password": "s3cret-should-never-land" }
            }
        });
        let clean = scrub_secret_keys(&form);
        assert_eq!(clean["arguments"]["fields"][0]["secret"], true);
        assert_eq!(clean["arguments"]["fields"][0]["type"], "password");
        assert!(clean["arguments"]["values"].get("password").is_none());
        assert!(!clean.to_string().contains("s3cret-should-never-land"));
    }

    #[test]
    fn sanitize_request_keeps_origin_and_username_only() {
        let cleaned = sanitize_request(&json!({
            "origin": "https://Accounts.Google.com/signin",
            "username": "ada@example.com",
            "password": "s3cret-should-never-land",
            "values": { "password": "nope" }
        }));
        assert_eq!(cleaned["origin"], "accounts.google.com");
        assert_eq!(cleaned["username"], "ada@example.com");
        assert!(cleaned.get("password").is_none(), "{cleaned}");
        assert!(cleaned.get("values").is_none(), "{cleaned}");
    }
}
