#![allow(clippy::expect_used, clippy::unwrap_used)]
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
