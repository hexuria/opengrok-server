use super::apply_user_form_stamp;
use serde_json::json;

#[test]
fn a_failed_append_does_not_stamp_entry_id() {
    let mut extra = serde_json::Map::new();
    extra.insert("name".into(), json!("run-awaiting-approval"));
    extra.insert("reason".into(), json!("user-form"));
    extra.insert(
        "arguments".into(),
        json!({ "title": "Sign in", "fields": [] }),
    );
    assert!(apply_user_form_stamp(&mut extra, "e_ghost".into(), false).is_none());
    assert!(
        extra.get("entryId").is_none(),
        "ghost entryId is the collapse blocker: {extra:?}"
    );
    assert_eq!(
        apply_user_form_stamp(&mut extra, "e_1".into(), true).as_deref(),
        Some("e_1")
    );
    assert_eq!(extra["entryId"], "e_1");
    assert_eq!(extra["formRequest"], extra["arguments"]);
}
