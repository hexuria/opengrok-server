#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

fn row() -> PendingUserMessageRow {
    PendingUserMessageRow {
        id: "pum_1".to_string(),
        thread_id: "th_1".to_string(),
        account_id: "acct_1".to_string(),
        content: "later".to_string(),
        reply_to: Some(json!({"messageId": "m1", "preview": "hi"})),
        recipe_id: Some("rec_1".to_string()),
        recipe_values: Some(json!({"q": "x"})),
        skill_id: Some("skl_1".to_string()),
        client_message_id: Some("msg_1".to_string()),
        status: "pending".to_string(),
        created_at_ms: 10,
        updated_at_ms: 20,
        drained_at_ms: None,
        drained_run_id: None,
        inference_source: None,
    }
}

#[test]
fn the_payload_is_versioned_and_camel_cased() {
    let json = message_json(&row());
    assert_eq!(json["v"], PAYLOAD_V);
    assert_eq!(json["threadId"], "th_1");
    assert_eq!(json["clientMessageId"], "msg_1");
    assert_eq!(json["recipeId"], "rec_1");
    assert_eq!(json["skillId"], "skl_1");
    assert_eq!(json["createdAtMs"], 10);
    assert_eq!(json["replyTo"]["messageId"], "m1");
}

#[test]
fn a_custom_event_names_the_op_inside_value() {
    let event = custom_event("created", "th_1", Some(&row()));
    assert_eq!(event["type"], "CUSTOM");
    assert_eq!(event["name"], CUSTOM_NAME);
    assert_eq!(event["value"]["v"], PAYLOAD_V);
    assert_eq!(event["value"]["op"], "created");
    assert_eq!(event["value"]["threadId"], "th_1");
    assert_eq!(event["value"]["message"]["id"], "pum_1");
}

#[test]
fn cancel_omits_the_message_so_the_text_is_not_re_sent() {
    let event = custom_event("canceled", "th_1", None);
    assert!(event["value"].get("message").is_none(), "{event}");
    assert_eq!(event["value"]["op"], "canceled");
}

#[test]
fn pending_id_prefers_the_short_name() {
    let mut input = RunAgentInput {
        thread_id: "t".to_string(),
        run_id: "r".to_string(),
        parent_run_id: None,
        state: json!({}),
        messages: vec![],
        tools: json!({}),
        context: json!({}),
        forwarded_props: json!({
            "pendingId": "pum_a",
            "pendingUserMessageId": "pum_b",
        }),
        extra: Default::default(),
    };
    assert_eq!(named_id(&input, PENDING_ID), Some("pum_a"));
    input.forwarded_props = json!({ "pendingUserMessageId": "pum_b" });
    assert_eq!(named_id(&input, PENDING_ID), Some("pum_b"));
    input.forwarded_props = json!({ "pendingId": "" });
    assert_eq!(named_id(&input, PENDING_ID), None);
}

/// #300: a `retryOf` that is not a string, or has no id in it, is no retry at all.
#[test]
fn a_retry_names_a_run_only_as_a_string_with_an_id_in_it() {
    let mut input: RunAgentInput =
        serde_json::from_value(json!({ "threadId": "t", "runId": "r" })).unwrap();
    for (props, named) in [
        (json!({ "retryOf": " run_a " }), Some("run_a")),
        (json!({ "retryOf": "" }), None),
        (json!({ "retryOf": "  " }), None),
        (json!({ "retryOf": 7 }), None),
        (json!({ "retryOf": { "runId": "run_a" } }), None),
        (json!({ "retryOf": null }), None),
        (json!({}), None),
    ] {
        input.forwarded_props = props;
        assert_eq!(
            named_id(&input, &["retryOf"]),
            named,
            "{}",
            input.forwarded_props
        );
    }
}
