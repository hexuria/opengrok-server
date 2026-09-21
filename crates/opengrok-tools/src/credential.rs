//! Save-for-next-time after a site login (`credential.offer_save`), and the scrub that keeps a
//! password off every event, tool result, and journal row.
//!
//! WHY THIS IS NOT THE VAULT. `opengrok-store::Vault` seals connector / API secrets. A Google
//! password must never enter it, Postgres, the journal, or a tool result.
//!
//! WHY THE MODEL NEVER SEES A PASSWORD. `offer_save` is `{ origin, username, formEntryId }`.
//! The saved login itself is offered by NativeChat on the `request_user_form` card and typed
//! into the page out of the model's view. Accidental `password` keys are scrubbed before
//! anything is journaled.

use serde_json::{Map, Value, json};

use crate::user_form::FormRequest;
use std::collections::BTreeMap;

/// CUSTOM NativeChat paints after a successful in-chat user-form fill: save origin+username
/// so the next login at this site is offered on the card instead of typed by the person.
pub const OFFER_SAVE: &str = "credential.offer_save";

/// Keys that must never persist on a credential event, a tool result, or the journal.
/// `secret` / `pass` as boolean flags (`fields[].secret: true` on `request_user_form`) stay;
/// a string under those names is treated as a smuggled value.
const SECRET_KEYS: &[&str] = &["password", "passwd", "pwd"];

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
    fn scrubbing_drops_password_keys_even_inside_a_delta_string() {
        let dirty = json!({
            "name": "request_user_form",
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
}
