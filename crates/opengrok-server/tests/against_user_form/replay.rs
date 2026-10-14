//! Handoff replay: an ended handoff replays on its form, never settling the run twice.

use super::*;

/// #143. "Open the screen", then "I'm done". The escalated form's frames — its park and the
/// `user-form` CUSTOM its escalation journalled — replay with how the handoff ended, from the run
/// and from the thread: without it a cold NativeChat painted a live "Action needed" for good. The
/// run the hand-back resumed has left the approvals queue.
#[tokio::test]
async fn a_handed_back_form_replays_as_handed_back_and_leaves_the_approvals_queue() {
    let database_url = database_or_skip!();
    let email = format!("handoff-back-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let a = thread("a");

    h.turn_on(&token, &agent, &a, "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let form_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    wait_for_approval(&h, &token, run.as_str()).await;
    let handoff_id = escalate(&h, &token, &agent, &form_id).await;
    let replayed = get_json(&h, &token, &format!("/ag-ui/runs/{}", run.as_str())).await;
    assert_frames_say(&replayed["events"], &form_id, None, true);

    end_handoff(&h, &token, &agent, &handoff_id, "handed_back").await;
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
    let stored = stored_card(&h, &agent, &form_id).await;
    assert_eq!(stored["formResolution"], "escalated", "{stored}");
    assert_eq!(stored["boxResolution"], "handed_back", "{stored}");
    assert!(stored.get("timedOut").is_none(), "{stored}");
    assert!(
        stored.get("boxRequestId").is_none(),
        "still a form: {stored}"
    );

    let replayed = get_json(&h, &token, &format!("/ag-ui/runs/{}", run.as_str())).await;
    assert_frames_say(&replayed["events"], &form_id, Some("handed_back"), true);
    let history = get_json(&h, &token, &format!("/ag-ui/threads/{a}")).await;
    let events: Vec<Value> = history["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["events"].as_array())
        .flatten()
        .cloned()
        .collect();
    assert_frames_say(&json!(events), &form_id, Some("handed_back"), true);
    assert_no_approval_row(&h, &token, &run).await;
}

/// #143. Skip on the handoff: the form replays as declined, and its run is no longer waiting.
#[tokio::test]
async fn a_declined_handoff_replays_on_its_form_as_declined() {
    let database_url = database_or_skip!();
    let email = format!("handoff-skip-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let form_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    let handoff_id = escalate(&h, &token, &agent, &form_id).await;

    end_handoff(&h, &token, &agent, &handoff_id, "declined").await;
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
    let stored = stored_card(&h, &agent, &form_id).await;
    assert_eq!(stored["boxResolution"], "declined", "{stored}");
    let replayed = get_json(&h, &token, &format!("/ag-ui/runs/{}", run.as_str())).await;
    assert_frames_say(&replayed["events"], &form_id, Some("declined"), true);
    assert_no_approval_row(&h, &token, &run).await;
}

/// #143. A handoff left past its deadline times out: its form says so, `timedOut` and all, on
/// the frames that replay it.
#[tokio::test]
async fn a_timed_out_handoff_replays_on_its_form_as_timed_out() {
    let database_url = database_or_skip!();
    let email = format!("handoff-late-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let form_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    let handoff_id = escalate(&h, &token, &agent, &form_id).await;

    // Ten minutes on, as far as the handoff's own stamp is concerned.
    let written = opengrok_server::agui::user_form::settle_dead_holds(
        &h.gateway,
        &h.account,
        &coworker,
        now_ms() + hold_ms() + 1_000,
    )
    .await;
    assert!(written, "the timeout could read and write the transcript");
    assert_eq!(
        stored_card(&h, &agent, &handoff_id).await["boxResolution"],
        "timed_out"
    );
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
    let stored = stored_card(&h, &agent, &form_id).await;
    assert_eq!(stored["boxResolution"], "timed_out", "{stored}");
    assert_eq!(stored["timedOut"], true, "{stored}");
    let replayed = get_json(&h, &token, &format!("/ag-ui/runs/{}", run.as_str())).await;
    assert_frames_say(&replayed["events"], &form_id, Some("timed_out"), true);
    assert_no_approval_row(&h, &token, &run).await;
}

/// #143. The run stopped waiting first — stopped where no route closed its card — and only then
/// was the form escalated and the computer handed back. No `user-form` frame could be journalled
/// onto a run that no longer waits, so the park is all that paints the form, and the park says
/// how the handoff ended. Nothing was answered, so the form is found as its handoff's own.
#[tokio::test]
async fn a_form_escalated_after_its_run_stopped_replays_its_hand_back_on_its_park() {
    let database_url = database_or_skip!();
    let email = format!("handoff-park-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let form_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    stop_in_store(&h, &run).await;
    let handoff_id = escalate(&h, &token, &agent, &form_id).await;

    end_handoff(&h, &token, &agent, &handoff_id, "handed_back").await;
    assert_eq!(
        status_of(&h, &run).await,
        opengrok_core::run::RunStatus::Stopped,
        "nothing was resumed"
    );
    let stored = stored_card(&h, &agent, &form_id).await;
    assert_eq!(stored["boxResolution"], "handed_back", "{stored}");
    let replayed = get_json(&h, &token, &format!("/ag-ui/runs/{}", run.as_str())).await;
    assert_frames_say(&replayed["events"], &form_id, Some("handed_back"), false);
    assert_no_approval_row(&h, &token, &run).await;
    assert!(!holds_any(&h.tail(&agent).await));
}

/// #143. A hand-back that cannot read the run log cannot name the call it answers, and settles
/// nothing: 503, the handoff still live, the form still open to its handoff, the run still parked.
/// Settled anyway, the run stayed parked behind a handoff that read as over — holding the screen
/// and its approvals row with nothing left to answer it. Once the log reads, the same post lands.
#[tokio::test]
async fn a_hand_back_that_cannot_read_the_run_log_settles_nothing() {
    let database_url = database_or_skip!();
    end_with_an_unreadable_log(&database_url, "handoff-unread", Ending::HandBackByHandoff).await;
}

/// The same, posted with the escalated form's own id, which names its call: the log is read
/// before anything settles on that path too (review of #277).
#[tokio::test]
async fn a_hand_back_by_the_forms_id_that_cannot_read_the_run_log_settles_nothing() {
    let database_url = database_or_skip!();
    end_with_an_unreadable_log(&database_url, "handoff-unread-form", Ending::HandBackByForm).await;
}

/// And Skip on an escalated form (`dismissUserForm`, mode dismissed), which declines its handoff:
/// the same read first, and the same 503 (review of #277).
#[tokio::test]
async fn a_skip_that_cannot_read_the_run_log_settles_nothing() {
    let database_url = database_or_skip!();
    end_with_an_unreadable_log(&database_url, "handoff-unread-skip", Ending::Skip).await;
}

/// #143, the backfill. A form handed back before its end was stamped on it reads that end from its
/// run's own answer, when that answer is exactly the server's hand-back sentence. Its own test, so
/// the corpus records this replay rather than the refusal below (a recording keeps one reply per
/// test and route, and NativeChat reads this one as "an old conversation, settled").
#[tokio::test]
async fn an_escalated_form_from_before_the_stamp_reads_its_end_from_its_runs_answer() {
    let database_url = database_or_skip!();
    let (h, token, agent, form_id, path) =
        a_form_handed_back_before_the_stamp(&database_url, "handoff-old").await;
    let replayed = get_json(&h, &token, &path).await;
    assert_frames_say(&replayed["events"], &form_id, Some("handed_back"), true);
    assert!(
        stored_card(&h, &agent, &form_id)
            .await
            .get("boxResolution")
            .is_none(),
        "the replay reads the end back; it writes nothing"
    );
}

/// Any other answer is not a handoff's end, and the form stays as it was: live. Nothing is guessed
/// from a sentence that only resembles the server's.
#[tokio::test]
async fn an_escalated_form_whose_answer_is_other_words_stays_live() {
    let database_url = database_or_skip!();
    let (h, token, _, form_id, path) =
        a_form_handed_back_before_the_stamp(&database_url, "handoff-other").await;
    let run_id = path.trim_start_matches("/ag-ui/runs/").to_string();
    sqlx::query(
        "update events set payload = replace(payload::text, $2, $3)::jsonb where stream_id = $1",
    )
    .bind(opengrok_store::run_stream(
        &opengrok_core::id::RunId::from_stored(run_id),
    ))
    .bind(opengrok_tools::user_form::HAND_BACK_TOOL_RESULT)
    .bind("Person finished on computer.")
    .execute(h.store.pool())
    .await
    .expect("reword the answer");
    let replayed = get_json(&h, &token, &path).await;
    assert!(
        !replayed
            .to_string()
            .contains(opengrok_tools::user_form::HAND_BACK_TOOL_RESULT),
        "the run's answer is another sentence now: {replayed}"
    );
    assert_frames_say(&replayed["events"], &form_id, None, true);
}
