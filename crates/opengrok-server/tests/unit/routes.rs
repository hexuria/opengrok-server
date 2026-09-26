//! Unit tests for `agui::routes`, kept out of the crate's source count.

use super::*;
use opengrok_harness::MockDoor;
use opengrok_wire::agui::{EventType, Message};
use serde_json::json;

#[test]
fn a_stopped_turn_continues_and_a_parked_card_does_not() {
    assert!(prior_turn_can_continue(RunStatus::Stopped, &[]));
    assert!(prior_turn_can_continue(RunStatus::Failed, &[]));
    assert!(!prior_turn_can_continue(RunStatus::Finished, &[]));
    assert!(!prior_turn_can_continue(
        RunStatus::Stopped,
        &[json!({"type":"CUSTOM","name":"run-awaiting-approval"})]
    ));

    let tools = unfinished_tool_messages(&[
        json!({"type":"TOOL_CALL_START","toolCallId":"c1","toolCallName":"user_machine_shell"}),
        json!({"type":"TOOL_CALL_ARGS","toolCallId":"c1","delta":"{\"command\":\"gpui-agent invoke profile.create\"}"}),
        json!({"type":"TOOL_CALL_RESULT","toolCallId":"c1","content":"exit 0\n--- stdout ---\n{\"result\":{\"view\":\"profile-manager\"}}"}),
    ]);
    assert_eq!(tools.len(), 1);
    assert!(
        tools[0].content.contains("profile.create"),
        "{}",
        tools[0].content
    );
    assert!(
        tools[0].content.contains("profile-manager"),
        "{}",
        tools[0].content
    );

    let mut messages = vec![
        ChatMessage::text("user", "create Juana Jane"),
        ChatMessage::text("user", "tin number should be 00000000000001"),
    ];
    splice_unfinished_tools(&mut messages, tools);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].content, "create Juana Jane");
    assert!(messages[1].content.contains("profile.create"));
    assert!(messages[2].content.contains("previous turn"));
    assert_eq!(messages[3].content, "tin number should be 00000000000001");

    let again = messages.clone();
    let mut doubled = again.clone();
    splice_unfinished_tools(&mut doubled, again);
    assert_eq!(
        doubled.len(),
        4,
        "a second splice does not repeat the block"
    );
}

fn input(messages: Vec<Message>) -> RunAgentInput {
    RunAgentInput {
        thread_id: "t1".to_string(),
        run_id: "r1".to_string(),
        parent_run_id: None,
        state: json!(null),
        messages,
        tools: json!(null),
        context: json!(null),
        forwarded_props: json!(null),
        extra: Default::default(),
    }
}

#[test]
fn the_recipe_a_person_chose_is_read_off_the_request() {
    let mut bare = input(Vec::new());
    assert!(
        chosen_recipe_from(&bare).is_none(),
        "no props, nothing chosen"
    );

    bare.forwarded_props = json!({ "coworkerId": "cw_1" });
    assert!(chosen_recipe_from(&bare).is_none(), "no recipe key");

    // No values is a chosen recipe all the same: a recipe may declare nothing, and one that
    // declares something still refuses later by name rather than being ignored here.
    bare.forwarded_props = json!({ "recipe": "rcp_1" });
    let (recipe, values) = chosen_recipe_from(&bare).unwrap();
    assert_eq!(recipe, "rcp_1");
    assert!(values.is_empty());

    // A client may send a number or a boolean as itself; a step types text either way.
    bare.forwarded_props = json!({
        "recipe": "rcp_1",
        "recipeValues": { "q": "mundo", "count": 3, "loud": true, "junk": ["no"] }
    });
    let (_, values) = chosen_recipe_from(&bare).unwrap();
    assert_eq!(values.get("q").map(String::as_str), Some("mundo"));
    assert_eq!(values.get("count").map(String::as_str), Some("3"));
    assert_eq!(values.get("loud").map(String::as_str), Some("true"));
    assert!(
        !values.contains_key("junk"),
        "a list is not a value for a field"
    );

    bare.forwarded_props = json!({ "recipe": "   " });
    assert!(
        chosen_recipe_from(&bare).is_none(),
        "a blank id is not an id"
    );
}

#[test]
fn a_refusal_names_the_tool_and_the_card_that_refused_it() {
    let pending = |reason| opengrok_core::run::PendingApproval {
        call_id: "call-1".to_string(),
        tool: "shell".to_string(),
        arguments: json!({}),
        reason,
    };

    // A yes is a yes whatever asked.
    assert!(matches!(
        resume_outcome(
            true,
            &pending(opengrok_core::run::SuspendReason::ExecConsent)
        ),
        opengrok_harness::ResumeOutcome::Approved
    ));

    // A no says which card, because the three cards mean different things and the model can
    // only choose something else if it knows what it ran into.
    // An approval coming back from a no would leave this empty, which every assertion below
    // then fails on — said that way round because this module may not panic.
    let said = |reason| match resume_outcome(false, &pending(reason)) {
        opengrok_harness::ResumeOutcome::Refused(why) => why,
        opengrok_harness::ResumeOutcome::Approved => String::new(),
        opengrok_harness::ResumeOutcome::Settled(why) => why,
    };
    let consent = said(opengrok_core::run::SuspendReason::ExecConsent);
    assert!(!consent.is_empty(), "a no is not an approval");
    assert!(consent.contains("shell"), "{consent}");
    assert!(
        consent.contains("did not run"),
        "the model is told the call did not happen, not merely that somebody said no: \
         {consent}"
    );
    let policy = said(opengrok_core::run::SuspendReason::PolicyApproval);
    assert!(policy.contains("policy"), "{policy}");
    assert!(policy.contains("shell"), "{policy}");
    let review = said(opengrok_core::run::SuspendReason::AutoReview);
    assert!(review.contains("auto-review"), "{review}");
    let form = said(opengrok_core::run::SuspendReason::UserForm);
    assert!(
        form.contains("dismissed") || form.contains("without filling"),
        "{form}"
    );
    assert!(
        matches!(
            resume_outcome(true, &pending(opengrok_core::run::SuspendReason::UserForm)),
            opengrok_harness::ResumeOutcome::Settled(_)
        ),
        "a yes on a user-form must not re-run request_user_form"
    );

    // None of them blames the model or reads as an error. A refusal is a decision somebody
    // made, and a sentence that sounds like a fault invites an apology and a retry.
    for why in [consent, policy, review] {
        let lower = why.to_ascii_lowercase();
        for word in ["error", "failed", "sorry", "invalid"] {
            assert!(
                !lower.contains(word),
                "a refusal must not read as a fault ({word}): {why}"
            );
        }
    }
}

/// `/answer` carries no values. A yes on a form through it typed nothing, and the model must
/// not be told a login was filled in when the page is still empty (#177).
#[test]
fn a_yes_on_a_form_through_answer_claims_nothing_was_typed() {
    let pending = opengrok_core::run::PendingApproval {
        call_id: "call-1".to_string(),
        tool: opengrok_tools::REQUEST_USER_FORM.to_string(),
        arguments: json!({ "title": "Google account" }),
        reason: opengrok_core::run::SuspendReason::UserForm,
    };
    let said = match resume_outcome(true, &pending) {
        opengrok_harness::ResumeOutcome::Settled(said) => said,
        _ => String::new(),
    };
    assert!(!said.is_empty(), "a yes on a form still settles the call");
    let lower = said.to_ascii_lowercase();
    assert!(!lower.contains("filled into the page"), "{said}");
    assert!(!lower.contains("were typed"), "{said}");
    assert!(lower.contains("nothing"), "{said}");
}

#[test]
fn named_tools_are_read_off_the_request_and_kept_to_what_is_offered() {
    let mut bare = input(Vec::new());
    assert!(
        preferred_tools_from(&bare).is_empty(),
        "no props, nothing named"
    );

    bare.forwarded_props = json!({ "coworkerId": "cw_1" });
    assert!(preferred_tools_from(&bare).is_empty(), "no preferTools key");

    bare.forwarded_props = json!({ "preferTools": ["open_url", "  ", "", "computer"] });
    assert_eq!(
        preferred_tools_from(&bare),
        vec!["open_url".to_string(), "computer".to_string()],
        "blank names are not names"
    );

    // Nothing is offered without a runner, so nothing is named to the model — a system
    // message naming a tool the model was not given is an instruction it cannot follow.
    assert!(
        honour_preferences(&["open_url".to_string()], None).is_empty(),
        "no tools this turn means no preference to state"
    );
}

fn message(role: &str, content: Option<&str>) -> Message {
    Message {
        id: "m1".to_string(),
        role: role.to_string(),
        content: content.map(str::to_string),
        name: None,
        extra: Default::default(),
    }
}

#[test]
fn chat_roles_the_model_understands_are_kept() {
    let messages = to_chat_messages(&input(vec![
        message("system", Some("be brief")),
        message("user", Some("hello")),
        message("assistant", Some("hi")),
    ]));
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].content, "hello");
}

/// AG-UI carries roles a chat completion has no place for. Passing one through fails the whole
/// turn on providers that reject unknown roles.
#[test]
fn roles_the_model_does_not_understand_are_dropped() {
    let messages = to_chat_messages(&input(vec![
        message("developer", Some("internal")),
        message("user", Some("hello")),
    ]));
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "user");
}

#[test]
fn a_tool_result_answers_its_call_by_id() {
    let mut tool = message("tool", Some("shown in the chat"));
    tool.id = "m9".to_string();
    tool.extra.insert("toolCallId".to_string(), json!("c1"));
    let messages = to_chat_messages(&input(vec![tool]));
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "tool");
    assert_eq!(messages[0].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(messages[0].content, "shown in the chat");
}

/// The desktop's reply chip has to reach the model as words, or "what am I replying to?"
/// arrives with nothing to answer from.
#[test]
fn a_reply_is_read_to_the_model_as_a_quote_ahead_of_its_own_words() {
    let mut quoted = message("assistant", Some("The build is green."));
    quoted.id = "m1".to_string();
    let mut reply = message("user", Some("what am I replying to?"));
    reply.id = "m2".to_string();
    reply.extra.insert(
        "replyTo".to_string(),
        json!({"messageId": "m1", "preview": "The build is green.", "isMe": false}),
    );
    let messages = to_chat_messages(&input(vec![quoted, reply]));
    assert_eq!(
        messages[1].content,
        "[Replying to your earlier message: \"The build is green.\"]\n\nwhat am I replying to?"
    );
}

/// Answering yourself is a different sentence, and the model has to be able to tell.
#[test]
fn a_reply_to_the_persons_own_message_says_whose_it_was() {
    let mut quoted = message("user", Some("remind me at five"));
    quoted.id = "m1".to_string();
    let mut reply = message("user", Some("make that six"));
    reply.id = "m2".to_string();
    reply.extra.insert("replyTo".to_string(), json!("m1"));
    let messages = to_chat_messages(&input(vec![quoted, reply]));
    assert_eq!(
        messages[1].content,
        "[Replying to their own earlier message: \"remind me at five\"]\n\nmake that six"
    );
}

/// NativeChat writes the quote into `content` as well, so a reply works against a server that
/// has never heard of `replyTo`. Reading the field must not say it twice.
#[test]
fn a_quote_the_client_already_wrote_is_not_written_again() {
    let mut quoted = message("assistant", Some("The build is green."));
    quoted.id = "m1".to_string();
    let mut reply = message(
        "user",
        Some("[Replying to your earlier message: \"The build is green.\"]\n\nwhy?"),
    );
    reply.id = "m2".to_string();
    reply.extra.insert(
        "replyTo".to_string(),
        json!({"messageId": "m1", "preview": "The build is green.", "isMe": false}),
    );
    let messages = to_chat_messages(&input(vec![quoted, reply]));
    assert_eq!(
        messages[1].content,
        "[Replying to your earlier message: \"The build is green.\"]\n\nwhy?"
    );
}

/// The quoted message may be gone from the array the client sends; the preview it saved with
/// the reply is what is left of it.
#[test]
fn a_quote_whose_message_is_not_in_the_array_falls_back_to_the_preview() {
    let mut reply = message("user", Some("why?"));
    reply.id = "m2".to_string();
    reply.extra.insert(
        "replyTo".to_string(),
        json!({"messageId": "gone", "preview": "The build is green.", "isMe": false}),
    );
    let messages = to_chat_messages(&input(vec![reply]));
    assert_eq!(
        messages[0].content,
        "[Replying to your earlier message: \"The build is green.\"]\n\nwhy?"
    );
}

/// A message with no content is a placeholder the client is still filling in.
#[test]
fn a_message_without_content_is_skipped() {
    let messages = to_chat_messages(&input(vec![message("user", None)]));
    assert!(messages.is_empty());
}

/// End to end through the mock door: no provider, no key, and still a complete run.
#[tokio::test]
async fn a_run_through_the_mock_door_is_well_formed() {
    let door = MockDoor::echoing();
    let events = opengrok_harness::run_conversation(
        &door,
        None,
        &opengrok_harness::MemoryJournal::new(),
        ModelRequest {
            gateway_key: None,
            spend_scope: None,
            spend_actor: None,
            context_tokens: None,
            model: "mock".to_string(),
            system: None,
            messages: to_chat_messages(&input(vec![message("user", Some("ping"))])),
            tools: Vec::new(),
        },
        "t1",
        "r1",
        1,
    )
    .await;

    assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
    assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
    assert_eq!(events.first().unwrap().extra.get("threadId").unwrap(), "t1");

    for event in &events {
        let frame = event.to_sse_frame().unwrap();
        assert!(frame.starts_with("data: "));
        assert_eq!(frame.matches("\n\n").count(), 1, "{frame:?}");
    }
}

/// NativeChat mounts CUSTOM `run-awaiting-approval` + `reason: user-form`. `entryId` is a
/// flattened extra field (same envelope as `callId` / `reason`), not a nested object.
#[test]
fn a_user_form_custom_frame_carries_entry_id_at_the_top_level() {
    let event = Event::new(EventType::Custom, 42)
        .with("name", "run-awaiting-approval")
        .with("threadId", "thr-1")
        .with("runId", "run-1")
        .with("callId", "mock-form-1")
        .with("tool", "request_user_form")
        .with("reason", "user-form")
        .with("why", "Waiting for you")
        .with("entryId", "e_form")
        .with(
            "arguments",
            json!({
                "title": "Google account",
                "instruction": "Enter the address and password.",
                "liveHost": "accounts.google.com",
                "fields": [{
                    "id": "email",
                    "label": "Email",
                    "type": "email",
                    "required": true,
                    "secret": false
                }]
            }),
        )
        .with(
            "formRequest",
            json!({
                "title": "Google account",
                "instruction": "Enter the address and password.",
                "liveHost": "accounts.google.com",
                "fields": [{
                    "id": "email",
                    "label": "Email",
                    "type": "email",
                    "required": true,
                    "secret": false
                }]
            }),
        );
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "CUSTOM");
    assert_eq!(wire["name"], "run-awaiting-approval");
    assert_eq!(wire["reason"], "user-form");
    assert_eq!(wire["entryId"], "e_form");
    assert_eq!(wire["formRequest"]["title"], "Google account");
    assert_eq!(wire["arguments"]["title"], "Google account");
    assert!(wire.get("values").is_none());
}

fn a_run(status: RunStatus, failure: Option<&str>) -> opengrok_core::run::Run {
    opengrok_core::run::Run {
        thread_id: "th".to_string(),
        status,
        failure: failure.map(str::to_string),
        ..opengrok_core::run::Run::default()
    }
}

fn closer_types(events: &[Event]) -> Vec<(EventType, Option<String>)> {
    events
        .iter()
        .map(|event| {
            let name = event
                .extra
                .get("name")
                .or_else(|| event.extra.get("message"))
                .and_then(|value| value.as_str())
                .map(str::to_string);
            (event.event_type, name)
        })
        .collect()
}

/// AN OWNER THE STORE COULD NOT CONFIRM IS TOLD TO RETRY, NOT TO START OVER. A 409 sends the
/// owner of a dropped stream to a new run id, and the turn runs twice.
#[test]
fn a_failed_ownership_read_is_a_503_not_run_exists() {
    assert_eq!(
        not_attached(Err(opengrok_store::StoreError::Corrupt("gone".to_string()))).status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(not_attached(Ok(false)).status(), StatusCode::CONFLICT);
}

/// AN ATTACHED STREAM ENDS THE WAY THE RUN ENDED, NOT THE WAY ITS LOG LAST CLOSED. A parked
/// run that was stopped, or swept after its card was answered, still ends its log with the
/// park's `RUN_FINISHED`; the closer used to be that one, which left a live card on screen.
#[test]
fn an_attached_stream_closes_with_what_the_run_is() {
    let run_id = RunId::from_stored("run-1".to_string());
    let park_closer = Event::new(EventType::RunFinished, 1);
    let run_error = Event::new(EventType::RunError, 1).with("message", "the model hung up");

    let stopped = attached_closer(
        &a_run(RunStatus::Stopped, None),
        &run_id,
        Some(&park_closer),
        false,
        2,
    );
    assert_eq!(
        closer_types(&stopped),
        vec![
            (EventType::Custom, Some("run-stopped".to_string())),
            (EventType::RunFinished, None)
        ]
    );

    let swept = attached_closer(
        &a_run(RunStatus::Failed, Some("interrupted by a restart")),
        &run_id,
        Some(&park_closer),
        false,
        2,
    );
    assert_eq!(
        closer_types(&swept),
        vec![(
            EventType::RunError,
            Some("interrupted by a restart".to_string())
        )]
    );

    let failed = attached_closer(
        &a_run(RunStatus::Failed, Some("the model hung up")),
        &run_id,
        Some(&run_error),
        false,
        2,
    );
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].timestamp, run_error.timestamp,
        "the log's own closer"
    );

    // A loop that ended as a stop already sent its notice; only the closer is left.
    let stopped_by_loop = attached_closer(
        &a_run(RunStatus::Stopped, None),
        &run_id,
        Some(&park_closer),
        true,
        2,
    );
    assert_eq!(
        closer_types(&stopped_by_loop),
        vec![(EventType::RunFinished, None)]
    );

    let parked = attached_closer(
        &a_run(RunStatus::AwaitingApproval, None),
        &run_id,
        Some(&park_closer),
        false,
        2,
    );
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].timestamp, park_closer.timestamp);
}

/// A RUN WITH NO FRAMES YET GETS NO CARDS. Hydration appends the transcript cards it cannot
/// place, and a run with no timestamps has no window to place them in — it used to be
/// "everything", so every card this coworker ever showed reached an attached stream, and the
/// next look skipped as many real frames. Only the log's own frames go out.
#[test]
fn an_attached_stream_sends_only_the_logs_own_frames() {
    let card = json!({
        "kind": "send-message",
        "id": "e_old",
        "timestampMs": 5,
        "message": {
            "type": "user-form",
            "formRequest": {"title": "Sign in", "fields": [{"id": "email", "label": "Email"}]}
        }
    });
    let (from, to) = run_time_window(&[]).unwrap_or(EMPTY_WINDOW);
    let hydrated = crate::agui::user_form::hydrate_agui_events(
        Vec::new(),
        std::slice::from_ref(&card),
        from,
        to,
    );
    assert!(
        hydrated.is_empty(),
        "no card for an empty run: {hydrated:?}"
    );
    assert!(
        log_frames(hydrated, 0).is_empty(),
        "the attached stream sends none"
    );
}

/// #188. A run whose frames carry no timestamps is not a window onto the whole transcript.
#[test]
fn a_run_with_no_timestamps_gets_no_transcript_cards() {
    let card = json!({
        "kind": "send-message",
        "id": "e_old",
        "timestampMs": 5,
        "message": {
            "type": "user-form",
            "formRequest": {"title": "Sign in", "fields": [{"id": "email", "label": "Email"}]}
        }
    });
    let emitted = vec![json!({"type": "RUN_STARTED"})];
    let window = run_time_window(&emitted);
    assert_eq!(
        started_at_ms(window),
        0,
        "a run with no timestamp yet reports no start, not the empty window's bound"
    );
    let (from, to) = window.unwrap_or(EMPTY_WINDOW);
    let out =
        crate::agui::user_form::hydrate_agui_events(emitted, std::slice::from_ref(&card), from, to);
    assert_eq!(out.len(), 1, "{out:?}");
}
