use super::*;
use crate::user_form::HAND_BACK_TOOL_RESULT;

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

/// #188. Two runs whose windows overlap — two turns a few seconds apart in one thread — each
/// hydrate against the coworker's whole transcript. A card is appended only to the run that
/// made its call, so it paints once, not once per run.
#[test]
fn hydrate_does_not_inject_another_runs_form() {
    let mut theirs = email_form("e_theirs", Some("submitted"), 50);
    theirs["callId"] = json!("c-theirs");
    let mine = vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": "c-mine", "toolCallName": "request_user_form"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "hi"}),
    ];
    let out = hydrate_agui_events(mine.clone(), std::slice::from_ref(&theirs), 0, 100);
    assert_eq!(
        out.len(),
        2,
        "another run's card is not painted here: {out:?}"
    );

    let ours = vec![json!({
        "type": "TOOL_CALL_START", "toolCallId": "c-theirs", "toolCallName": "request_user_form"
    })];
    let out = hydrate_agui_events(ours, std::slice::from_ref(&theirs), 0, 100);
    assert!(
        out.iter()
            .any(|event| event["entryId"] == "e_theirs" && event["name"] == "user-form"),
        "the run that made the call still gets its card: {out:?}"
    );
}

fn escalated(id: &str, call: Option<&str>) -> Value {
    let mut entry = email_form(id, Some("escalated"), 10);
    if let Some(call) = call {
        entry["callId"] = json!(call);
    }
    entry
}

fn park(call: &str) -> Value {
    json!({"type": "CUSTOM", "name": "run-awaiting-approval", "reason": "user-form", "callId": call})
}

/// An escalated form, parked on `c1`, with the `user-form` frame its escalation journalled.
fn escalated_run(form: &Value) -> Vec<Value> {
    vec![
        park("c1"),
        json!({
            "type": "CUSTOM", "name": "user-form", "entryId": form["id"], "callId": "c1",
            "formResolution": "escalated", "value": {"id": form["id"]}
        }),
    ]
}

/// #143. How the handoff ended rides beside `formResolution` on BOTH frames that paint the form,
/// the park included, and in `value`; an escalated form whose handoff is still live has neither.
#[test]
fn hydrate_puts_a_handoffs_end_on_the_park_and_the_user_form_frame() {
    let mut form = escalated("e_form", Some("c1"));
    let out = hydrate_agui_events(escalated_run(&form), std::slice::from_ref(&form), 0, 100);
    assert!(
        out.iter().all(|frame| frame.get("boxResolution").is_none()),
        "absent while the handoff is live: {out:?}"
    );

    form["boxResolution"] = json!("timed_out");
    form["timedOut"] = json!(true);
    let out = hydrate_agui_events(escalated_run(&form), std::slice::from_ref(&form), 0, 100);
    assert_eq!(out.len(), 2, "{out:?}");
    for frame in &out {
        assert_eq!(frame["entryId"], "e_form", "{frame}");
        assert_eq!(frame["formResolution"], "escalated", "{frame}");
        assert_eq!(frame["boxResolution"], "timed_out", "{frame}");
        assert_eq!(frame["timedOut"], true, "{frame}");
        assert_eq!(frame["value"]["boxResolution"], "timed_out", "{frame}");
    }
}

/// #143, the backfill. A form escalated before the stamp existed reads its handoff's end from its
/// run's answer to its call — and only from the server's own sentences, exactly as written. Any
/// other answer, an answer to another call, a form that was never escalated, and a form that
/// already says how its handoff ended all stay as they are.
#[test]
fn an_old_escalated_form_reads_its_end_only_from_the_servers_own_sentences() {
    let answered = |call: &str, content: &str| {
        vec![
            park("c1"),
            json!({"type": "TOOL_CALL_RESULT", "toolCallId": call, "content": content}),
        ]
    };
    let ended = |form: &Value, events: Vec<Value>| {
        let out = hydrate_agui_events(events, std::slice::from_ref(form), 0, 100);
        (
            out[0].get("boxResolution").cloned(),
            out[0].get("timedOut").cloned(),
        )
    };
    let form = escalated("e_form", Some("c1"));
    for (word, sentence) in HANDOFF_ENDINGS {
        let timed_out = (word == "timed_out").then_some(json!(true));
        assert_eq!(
            ended(&form, answered("c1", sentence)),
            (Some(json!(word)), timed_out),
            "{word}"
        );
    }
    let near_miss = HAND_BACK_TOOL_RESULT.trim_end_matches('.');
    assert_eq!(ended(&form, answered("c1", near_miss)), (None, None));
    assert_eq!(ended(&form, answered("c1", "done")), (None, None));
    assert_eq!(
        ended(&form, answered("c2", HAND_BACK_TOOL_RESULT)),
        (None, None),
        "another call's answer is not this form's"
    );

    let mut dismissed = email_form("e_form", Some("dismissed"), 10);
    dismissed["callId"] = json!("c1");
    assert_eq!(
        ended(&dismissed, answered("c1", HAND_BACK_TOOL_RESULT)),
        (None, None)
    );
    let mut stamped = form.clone();
    stamped["boxResolution"] = json!("declined");
    assert_eq!(
        ended(&stamped, answered("c1", HAND_BACK_TOOL_RESULT)),
        (Some(json!("declined")), None),
        "a stamped end is the truth"
    );
}
