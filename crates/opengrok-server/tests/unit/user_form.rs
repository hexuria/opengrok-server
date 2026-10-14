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

/// A form-mode call accepted by the executor is the moment its card is minted; a refused one,
/// or a login card's result, is not (#220). The model's smuggled values never reach the mint.
#[test]
fn a_held_form_mode_call_is_found_by_its_accepted_result() {
    use opengrok_tools::user_form::form_mode::{FORM_MODE_UNAVAILABLE, FORM_OPEN};
    let mut hold = UserFormSseHold::default();
    assert!(hold.push(form_tool_start("call-fm")).is_none());
    let args = r#"{"formMode":true,"values":{"k":"1"},"form":{"form":"0619E","tin":"123456789000","period":"2026-09"}}"#;
    let delta = Event::new(EventType::ToolCallArgs, 2)
        .with("toolCallId", "call-fm")
        .with("delta", args);
    assert!(hold.push(delta).is_none());
    let result = |content: &str, ok: bool| {
        Event::new(EventType::ToolCallResult, 3)
            .with("toolCallId", "call-fm")
            .with("content", content)
            .with("ok", ok)
    };
    assert!(
        hold.form_mode_call(&result(FORM_MODE_UNAVAILABLE, false))
            .is_none()
    );
    // The result's `ok` decides, not its words: a refusal that quoted the promise mints nothing.
    assert!(hold.form_mode_call(&result(FORM_OPEN, false)).is_none());
    let (call, arguments) = hold
        .form_mode_call(&result("reworded, still open", true))
        .unwrap();
    assert_eq!(call, "call-fm");
    assert!(arguments.get("values").is_none(), "{arguments}");
    assert_eq!(arguments["form"]["form"], json!("0619E"));
    // A login card's call is never minted here, whatever its result says.
    assert!(hold.push(form_tool_start("call-login")).is_none());
    assert!(hold.push(form_tool_args("call-login")).is_none());
    let login = Event::new(EventType::ToolCallResult, 3)
        .with("toolCallId", "call-login")
        .with("content", FORM_OPEN)
        .with("ok", true);
    assert!(hold.form_mode_call(&login).is_none());
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

fn escalated(id: &str, call: Option<&str>) -> Value {
    let mut entry = email_form(id, Some("escalated"), 10);
    if let Some(call) = call {
        entry["callId"] = json!(call);
    }
    entry
}

fn form_park(call: &str) -> FormPark {
    FormPark {
        calls: BTreeSet::from([call.to_string()]),
        pending: opengrok_core::run::PendingApproval {
            call_id: call.to_string(),
            tool: "request_user_form".to_string(),
            arguments: json!({}),
            reason: opengrok_core::run::SuspendReason::UserForm,
        },
        parked_at_ms: 10,
    }
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
        forms: vec![form_park("c-new")],
    };
    assert_eq!(
        handoff_call(&entries, &carded),
        Some(Some("c-new".to_string()))
    );

    let uncarded = WaitingCalls {
        on: BTreeMap::from([("c-legacy".to_string(), "c-legacy".to_string())]),
        forms: vec![form_park("c-legacy")],
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
            effort: Default::default(),
            inference_source: Default::default(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
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
