use super::*;

fn email_form(id: &str, resolution: Option<&str>, at: i64) -> Value {
    let mut entry = json!({
        "kind": "send-message",
        "id": id,
        "timestampMs": at,
        "message": {
            "type": "user-form",
            "formRequest": {
                "title": "Sign in",
                "fields": [{"id": "email", "label": "Email", "type": "email", "required": true}]
            }
        }
    });
    if let Some(word) = resolution {
        entry["formResolution"] = json!(word);
    }
    entry
}

#[test]
fn hydrate_overlays_form_resolution_onto_the_awaiting_custom() {
    let form = email_form("e_form", Some("submitted"), 50);
    let events = vec![json!({
        "type": "CUSTOM",
        "name": "run-awaiting-approval",
        "reason": "user-form",
        "callId": "c1",
        "entryId": "e_form",
        "arguments": {
            "title": "Sign in",
            "fields": [{"id": "email", "label": "Email", "type": "email"}]
        }
    })];
    let out = hydrate_agui_events(events, std::slice::from_ref(&form), 0, 100);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["formResolution"], "submitted");
    assert_eq!(out[0]["message"]["type"], "user-form");
    assert_eq!(out[0]["entryId"], "e_form");
    let dump = serde_json::to_string(&out).unwrap();
    assert!(!dump.contains("s3cret"), "{dump}");
}

#[test]
fn hydrate_injects_a_settled_card_when_the_run_never_stamped_entry_id() {
    let form = email_form("e_form", Some("submitted"), 50);
    let events = vec![json!({
        "type": "CUSTOM",
        "name": "run-awaiting-approval",
        "reason": "user-form",
        "callId": "c1",
        "arguments": {
            "title": "Sign in",
            "fields": [{"id": "email", "label": "Email", "type": "email"}]
        }
    })];
    let out = hydrate_agui_events(events, std::slice::from_ref(&form), 0, 100);
    assert_eq!(out[0]["entryId"], "e_form");
    assert_eq!(out[0]["formResolution"], "submitted");
    assert_eq!(out[0]["message"]["type"], "user-form");
}

#[test]
fn hydrate_skips_forms_outside_the_run_window() {
    let other = email_form("e_other", Some("dismissed"), 10_000);
    let events = vec![json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "hi"})];
    let out = hydrate_agui_events(events, std::slice::from_ref(&other), 0, 100);
    assert_eq!(out.len(), 1);
    assert!(out.iter().all(|event| event["name"] != "user-form"));
}

#[test]
fn hydrate_stamps_entry_id_onto_matching_tool_calls() {
    let form = email_form("e_form", None, 50);
    let mut form = form;
    form["callId"] = json!("c1");
    let events = vec![
        json!({
            "type": "TOOL_CALL_START",
            "toolCallId": "c1",
            "toolCallName": "request_user_form"
        }),
        json!({
            "type": "CUSTOM",
            "name": "run-awaiting-approval",
            "reason": "user-form",
            "callId": "c1",
            "entryId": "e_form",
            "arguments": {
                "title": "Sign in",
                "fields": [{"id": "email", "label": "Email", "type": "email"}]
            }
        }),
    ];
    let out = hydrate_agui_events(events, std::slice::from_ref(&form), 0, 100);
    assert_eq!(out[0]["entryId"], "e_form");
    assert_eq!(out[1]["entryId"], "e_form");
}

#[test]
fn hydrate_joins_stacked_same_fingerprint_forms_by_call_id() {
    let mut forms = Vec::new();
    for (id, call) in [("e_3", "c3"), ("e_1", "c1"), ("e_2", "c2")] {
        let mut form = email_form(id, None, 50);
        form["callId"] = json!(call);
        forms.push(form);
    }
    let events = vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "request_user_form"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "request_user_form"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c3", "toolCallName": "request_user_form"}),
        json!({
            "type": "CUSTOM",
            "name": "run-awaiting-approval",
            "reason": "user-form",
            "callId": "c1",
            "arguments": {
                "title": "Sign in",
                "fields": [{"id": "email", "label": "Email", "type": "email"}]
            }
        }),
        json!({
            "type": "CUSTOM",
            "name": "run-awaiting-approval",
            "reason": "user-form",
            "callId": "c2",
            "arguments": {
                "title": "Sign in",
                "fields": [{"id": "email", "label": "Email", "type": "email"}]
            }
        }),
        json!({
            "type": "CUSTOM",
            "name": "run-awaiting-approval",
            "reason": "user-form",
            "callId": "c3",
            "arguments": {
                "title": "Sign in",
                "fields": [{"id": "email", "label": "Email", "type": "email"}]
            }
        }),
    ];
    let out = hydrate_agui_events(events, &forms, 0, 100);
    let by_call: Vec<(&str, &str)> = out
        .iter()
        .filter(|event| event["type"] == "TOOL_CALL_START")
        .map(|event| {
            (
                event["toolCallId"].as_str().unwrap(),
                event["entryId"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(by_call, vec![("c1", "e_1"), ("c2", "e_2"), ("c3", "e_3")]);
}

#[test]
fn an_escalated_form_is_not_a_live_handoff() {
    let form = email_form("e_form", Some("escalated"), 50);
    assert!(is_escalated_form(&form));
    assert!(!is_handoff_entry(&form));
    assert!(!is_live_handoff(&form));
}

fn form_tool_start(call_id: &str) -> Event {
    Event::new(EventType::ToolCallStart, 1)
        .with("toolCallId", call_id)
        .with("toolCallName", opengrok_tools::REQUEST_USER_FORM)
}

fn form_tool_args(call_id: &str) -> Event {
    Event::new(EventType::ToolCallArgs, 2)
        .with("toolCallId", call_id)
        .with("delta", r#"{"title":"Website login"}"#)
}

/// Two same-title Website logins: each TOOL_CALL must leave with its own e_*, not the
/// raw `call-*-1` NativeChat used when live frames streamed before the gateway stamp.
#[test]
fn live_sse_holds_stacked_form_tool_calls_until_each_has_a_gateway_entry_id() {
    let mut hold = UserFormSseHold::default();
    assert!(hold.push(form_tool_start("call-42628be6")).is_none());
    assert!(hold.push(form_tool_args("call-42628be6")).is_none());
    assert!(hold.push(form_tool_start("call-42628be6-1")).is_none());
    assert!(hold.push(form_tool_args("call-42628be6-1")).is_none());

    let first = hold.release_for("call-42628be6", Some("e_aaa"));
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(
        first
            .iter()
            .all(|event| event.extra.get("entryId") == Some(&json!("e_aaa")))
    );

    let second = hold.release_for("call-42628be6-1", Some("e_bbb"));
    assert_eq!(second.len(), 2, "{second:?}");
    assert!(
        second
            .iter()
            .all(|event| event.extra.get("entryId") == Some(&json!("e_bbb")))
    );
    assert_ne!(
        first[0].extra.get("entryId"),
        second[0].extra.get("entryId")
    );
}

#[test]
fn shell_tool_calls_pass_through_the_user_form_hold() {
    let mut hold = UserFormSseHold::default();
    let event = Event::new(EventType::ToolCallStart, 1)
        .with("toolCallId", "c1")
        .with("toolCallName", "shell");
    let passed = hold.push(event).unwrap();
    assert_eq!(
        passed.extra.get("toolCallName").and_then(Value::as_str),
        Some("shell")
    );
    assert!(hold.release_rest().is_empty());
}
