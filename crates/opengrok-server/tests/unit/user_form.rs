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

/// A run that ENDS IN AN ERROR shows no card: a held login form would be painted from its
/// TOOL_CALL frames with no suspension behind it, and its Continue could only answer 409.
/// A clean ending still lets a refused form's frames through.
#[test]
fn held_form_frames_go_out_only_after_a_clean_ending() {
    let mut failed = UserFormSseHold::default();
    assert!(failed.push(form_tool_start("call-1")).is_none());
    assert!(failed.push(form_tool_args("call-1")).is_none());
    assert!(failed.release_at_end(false).is_empty());
    assert!(
        failed.release_rest().is_empty(),
        "and nothing is left behind"
    );

    let mut finished = UserFormSseHold::default();
    assert!(finished.push(form_tool_start("call-2")).is_none());
    assert!(finished.push(form_tool_args("call-2")).is_none());
    assert_eq!(finished.release_at_end(true).len(), 2);
}

/// A STOP'S `RUN_FINISHED` IS NOT A FINISH. A form that parked after a Stop became a stop, so its
/// stamping CUSTOM never went out; releasing its held frames on the closer painted a login card
/// with a raw `call-…` id and no suspension behind it.
#[test]
fn held_form_frames_do_not_go_out_after_a_stop() {
    let mut stopped = UserFormSseHold::default();
    assert!(stopped.push(form_tool_start("call-1")).is_none());
    assert!(stopped.push(form_tool_args("call-1")).is_none());
    let notice = Event::new(EventType::Custom, 3).with("name", "run-stopped");
    assert!(stopped.push(notice).is_some(), "the notice itself goes out");
    assert!(stopped.release_at_end(true).is_empty());
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

/// #188. An old escalated form with no call used to make every later handoff look waited on, so a
/// stopped run's handoff was never declined. It counts only while a form run whose card carries
/// no call still waits.
#[test]
fn an_old_escalated_form_with_no_call_does_not_keep_every_handoff_alive() {
    let entries = vec![escalated("e_old", None), escalated("e_new", Some("c-new"))];
    assert_eq!(handoff_call(&entries, &WaitingCalls::default()), None);

    let carded = WaitingCalls {
        on: BTreeMap::from([("c-new".to_string(), "c-new".to_string())]),
        forms: vec![FormPark {
            calls: BTreeSet::from(["c-new".to_string()]),
        }],
    };
    assert_eq!(
        handoff_call(&entries, &carded),
        Some(Some("c-new".to_string()))
    );

    let uncarded = WaitingCalls {
        on: BTreeMap::from([("c-legacy".to_string(), "c-legacy".to_string())]),
        forms: vec![FormPark {
            calls: BTreeSet::from(["c-legacy".to_string()]),
        }],
    };
    assert_eq!(handoff_call(&entries, &uncarded), Some(None));
}

fn park(call: &str) -> Value {
    json!({"type": "CUSTOM", "name": "run-awaiting-approval", "reason": "user-form", "callId": call})
}

/// #188. A twin raised with the call its run was answered on belongs to a completion the run has
/// moved past: when the run parks again, that twin's card is not waited on.
#[test]
fn a_run_parked_again_no_longer_waits_on_a_twin_from_before_its_answer() {
    use opengrok_core::run::{Run, RunCommand, SuspendReason};
    let mut run = Run::default();
    let step = |run: &mut Run, command: RunCommand| {
        for event in run.decide(command).unwrap() {
            run.apply(&event);
        }
    };
    step(
        &mut run,
        RunCommand::Start {
            thread_id: "thr".into(),
            coworker_id: None,
            model: None,
            system: None,
            skill_id: None,
            at_ms: 1,
        },
    );
    let suspend = |call: &str| RunCommand::Suspend {
        call_id: call.into(),
        tool: "request_user_form".into(),
        arguments: json!({}),
        reason: SuspendReason::UserForm,
        at_ms: 2,
    };
    for call in ["x", "y"] {
        step(
            &mut run,
            RunCommand::Emit {
                payload: park(call),
                at_ms: 2,
            },
        );
    }
    step(&mut run, suspend("y"));
    assert_eq!(
        resume::parked_calls(&run),
        BTreeSet::from(["x".to_string(), "y".to_string()]),
        "twins from one completion both wait"
    );
    step(
        &mut run,
        RunCommand::Answer {
            call_id: "y".into(),
            approved: true,
            by: "me".into(),
            at_ms: 3,
        },
    );
    step(
        &mut run,
        RunCommand::Emit {
            payload: park("z"),
            at_ms: 4,
        },
    );
    step(&mut run, suspend("z"));
    assert_eq!(
        resume::parked_calls(&run),
        BTreeSet::from(["z".to_string()])
    );
}
