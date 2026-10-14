//! Parked runs: stop, restart, deadlines and the sweep; a card that never landed still ends.

use super::*;

/// A ROUTINE THAT NEEDS A LOGIN MUST BE ABLE TO GET ONE (#177). Nobody is watching a 9am run, so
/// its card is the only way the person learns it is stuck — and before this, a routine's form
/// had no card at all: no `entryId` to submit to, a queue row no client could place, and a
/// routine announced as "produced no answer" while it sat waiting.
#[tokio::test]
async fn a_routine_that_asks_for_a_login_says_it_is_waiting_and_its_card_can_be_submitted() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-routine-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Nine").await;

    let (status, created) = h
        .agui(
            &token,
            "/schedules",
            json!({ "coworkerId": agent, "kind": "webhook", "name": "Inbox", "prompt": "sign in" }),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    let schedule = created["id"].as_str().expect("routine id").to_string();
    let key = created["webhook"]["key"].as_str().expect("key").to_string();
    let hook = created["webhook"]["url"]
        .as_str()
        .expect("url")
        .rsplit('/')
        .next()
        .expect("hook id")
        .to_string();
    let fired: Value = h
        .client
        .post(format!("{}/hooks/{hook}", h.base))
        .header("authorization", format!("Bearer {key}"))
        .json(&json!({}))
        .send()
        .await
        .expect("fire the hook")
        .json()
        .await
        .expect("hook json");
    let run_id = fired["runId"].as_str().expect("run id").to_string();

    // The queue says whose card this is and where it came from. The thread id is the routine's,
    // which names no chat, so without these a client has nowhere to put it.
    let row = wait_for_approval(&h, &token, &run_id).await;
    assert_eq!(row["reason"], "user-form", "{row}");
    assert_eq!(row["coworkerId"], json!(agent), "{row}");
    assert_eq!(row["origin"], "webhook", "{row}");

    // The routine's row says it is waiting, and on what — not that it produced nothing.
    let routine = wait_for_routine(&h, &token, &schedule, |last| {
        last["status"] == "awaiting-approval"
    })
    .await;
    let summary = routine["lastRun"]["summary"].as_str().expect("summary");
    assert!(
        summary.starts_with("Routine Inbox is waiting for you"),
        "{summary}"
    );
    assert!(
        summary.contains("Google account"),
        "the card is named: {summary}"
    );
    assert!(!summary.contains("produced no answer"), "{summary}");
    assert_eq!(routine["lastRun"]["runId"], json!(run_id), "{routine}");
    assert_eq!(routine["lastRun"]["finishedAtMs"], Value::Null, "{routine}");

    // The card exists, with an id submit can name — minted exactly as a chat turn's is.
    let card = h.wait_for_form(&agent).await;
    assert_eq!(card["callId"], row["callId"], "{card}");
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    assert!(entry_id.starts_with("e_"), "{card}");

    // And the routine's own thread replays the pause with that id on it, so a client that opens
    // the run from `lastRun` can submit from the history it painted.
    let thread: Value = h
        .client
        .get(format!("{}/ag-ui/threads/{schedule}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("thread")
        .json()
        .await
        .expect("thread json");
    let stamped = thread["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|run| run["events"].as_array().into_iter().flatten())
        .any(|event| {
            event["name"] == "run-awaiting-approval" && event["entryId"] == json!(entry_id)
        });
    assert!(
        stamped,
        "the replayed pause carries the card's entryId: {thread}"
    );

    let (status, settled) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": entry_id,
                "agentId": agent,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 200, "{settled}");
    assert_eq!(settled["formResolution"], "submitted", "{settled}");

    // And once answered, the routine carries on and its row says how it ended.
    let routine =
        wait_for_routine(&h, &token, &schedule, |last| last["status"] == "finished").await;
    let summary = routine["lastRun"]["summary"].as_str().expect("summary");
    assert!(summary.starts_with("Routine Inbox ran: "), "{summary}");
    assert!(!summary.contains(SECRET), "{summary}");
}

/// A card raised by a person's own chat turn says so, and names its coworker too.
///
/// ORIGIN IS READ FROM THE LOG, NOT FROM THE THREAD ID. A thread id is whatever the client sent,
/// and this turn is sent on one spelled like a routine's: it is still a chat.
#[tokio::test]
async fn a_chat_turns_card_is_placed_by_its_coworker() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-chat-origin-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Chatty").await;
    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": format!("sched_{}", uuid::Uuid::now_v7()),
            "runId": uuid::Uuid::now_v7().to_string(),
            "messages": [{ "id": "m1", "role": "user", "content": "sign in" }],
            "forwardedProps": { "coworkerId": agent },
        }))
        .send()
        .await
        .expect("post ag-ui");
    assert_eq!(res.status().as_u16(), 200);
    res.text().await.expect("sse");
    let run_id = h
        .store
        .awaiting_approval(&h.account)
        .await
        .expect("awaiting")
        .into_iter()
        .next()
        .expect("a waiting run")
        .as_str()
        .to_string();
    let row = wait_for_approval(&h, &token, &run_id).await;
    assert_eq!(row["coworkerId"], json!(agent), "{row}");
    assert_eq!(row["origin"], "chat", "{row}");
}

/// A CARD ANSWERS ITS OWN RUN. A coworker's chat turn and one of its routines can both be parked
/// on a form now that a routine mints its card, and the submit used to resume whichever run had
/// waited longest — so the other run's card settled, the call ids disagreed, and the run the
/// person actually answered stayed parked for good with its card already marked submitted.
#[tokio::test]
async fn a_forms_submit_resumes_its_own_run_when_a_chat_and_a_routine_both_wait() {
    let database_url = database_or_skip!();
    let email = format!("user-form-two-{}@og.local", uuid::Uuid::now_v7().simple());
    let door = Arc::new(EachAskDoor {
        both_asking: tokio::sync::Barrier::new(2),
    });
    let h = harness_with_door(&database_url, &email, door).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Both").await;

    let (status, created) = h
        .agui(
            &token,
            "/schedules",
            json!({ "coworkerId": agent, "kind": "webhook", "name": "Inbox", "prompt": "sign in" }),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    let schedule = created["id"].as_str().expect("routine id").to_string();
    let key = created["webhook"]["key"].as_str().expect("key").to_string();
    let hook = created["webhook"]["url"]
        .as_str()
        .expect("url")
        .rsplit('/')
        .next()
        .expect("hook id")
        .to_string();

    // The person's chat turn and the routine, under way together.
    let chat = tokio::spawn({
        let client = h.client.clone();
        let url = format!("{}/ag-ui", h.base);
        let token = token.clone();
        let agent = agent.clone();
        async move {
            client
                .post(url)
                .header("authorization", format!("Bearer {token}"))
                .json(&json!({
                    "threadId": format!("gateway-{agent}"),
                    "runId": uuid::Uuid::now_v7().to_string(),
                    "messages": [{ "id": "m1", "role": "user", "content": "sign in" }],
                    "forwardedProps": { "coworkerId": agent },
                }))
                .send()
                .await
                .expect("chat turn")
                .text()
                .await
                .expect("sse")
        }
    });
    let fired: Value = h
        .client
        .post(format!("{}/hooks/{hook}", h.base))
        .header("authorization", format!("Bearer {key}"))
        .json(&json!({}))
        .send()
        .await
        .expect("fire the hook")
        .json()
        .await
        .expect("hook json");
    let routine_run = fired["runId"].as_str().expect("run id").to_string();
    chat.await.expect("the chat turn parks");
    wait_for_approval(&h, &token, &routine_run).await;
    let cards = h.wait_for_forms(&agent, 2).await;

    // Answer the run that has NOT waited longest: the one a first-come lookup would pass over.
    let waiting = h
        .store
        .awaiting_approval(&h.account)
        .await
        .expect("awaiting");
    assert_eq!(waiting.len(), 2, "both runs are parked on a form");
    let (longest, answered) = (waiting[0].clone(), waiting[1].clone());
    let row = wait_for_approval(&h, &token, answered.as_str()).await;
    let card = cards
        .iter()
        .find(|card| card["callId"] == row["callId"])
        .expect("the answered run's own card");

    let (status, settled) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"],
                "agentId": agent,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 200, "{settled}");

    let mut resumed = false;
    for _ in 0..100 {
        let still = h
            .store
            .awaiting_approval(&h.account)
            .await
            .expect("awaiting");
        if !still.contains(&answered) {
            resumed = true;
            assert!(
                still.contains(&longest),
                "the run whose card was not answered is still waiting on it"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(resumed, "the run whose card was submitted carried on");
    if answered.as_str() == routine_run {
        let routine =
            wait_for_routine(&h, &token, &schedule, |last| last["status"] == "finished").await;
        assert_eq!(routine["lastRun"]["runId"], json!(routine_run), "{routine}");
    }
}

// ---------------------------------------------------------------------------------------------
// A card and the run that raised it are one thing (#186, #188). Park, stop, interrupt, submit,
// time out and replay each used to find "the" card or "the" run by the coworker alone.
// ---------------------------------------------------------------------------------------------

/// #188. A message in one conversation is steer for THAT conversation. A sign-in waiting in
/// another one is left exactly as it was — and, because the coworker has one computer, the other
/// conversation may not use the screen meanwhile, and is told which conversation holds it.
#[tokio::test]
async fn a_message_in_another_thread_leaves_this_threads_form_open() {
    let database_url = database_or_skip!();
    let email = format!("hold-threads-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let a = thread("a");
    let b = thread("b");

    h.turn_on(&token, &agent, &a, "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let run_a = parked(&h).await.remove(0);

    h.turn_on(&token, &agent, &b, "what is the time").await;
    assert_eq!(
        parked(&h).await,
        vec![run_a.clone()],
        "thread A still waits"
    );
    let card = stored_card(&h, &agent, &entry_id).await;
    assert!(card.get("formResolution").is_none(), "{card}");

    let sse = h.turn_on(&token, &agent, &b, "look at the screen").await;
    assert_eq!(
        h.stub.shots(),
        0,
        "one computer, and a person is on it: {sse}"
    );
    assert!(
        sse.contains(&a),
        "the refusal names the conversation holding it: {sse}"
    );
    assert!(h.stub.acts().is_empty());

    // And a message in A itself is still steer for A.
    h.turn_on(&token, &agent, &a, "never mind").await;
    assert!(parked(&h).await.is_empty());
    let card = stored_card(&h, &agent, &entry_id).await;
    assert_eq!(card["formResolution"], "dismissed", "{card}");
}

/// #188. The card names its call, and the call names its run: a submit resumes the run parked on
/// THIS card, not whichever of the coworker's parked runs is oldest.
#[tokio::test]
async fn submit_resumes_the_run_parked_on_its_own_call_not_the_oldest() {
    use opengrok_core::run::{Run, RunCommand, RunView, SuspendReason};
    let database_url = database_or_skip!();
    let email = format!("hold-decoy-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let real = parked(&h).await.remove(0);

    // An older run of the same coworker, parked on a different form call in another thread.
    let decoy = opengrok_core::id::RunId::new();
    let at_ms = now_ms() - 60_000;
    let mut run = Run::default();
    let mut events = run
        .decide(RunCommand::Start {
            thread_id: thread("decoy"),
            coworker_id: Some(opengrok_core::id::CoworkerId::from_stored(agent.clone())),
            model: None,
            effort: Default::default(),
            inference_source: Default::default(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms,
        })
        .expect("start");
    for event in &events {
        run.apply(event);
    }
    let suspended = run
        .decide(RunCommand::Suspend {
            call_id: "decoy-call".to_string(),
            tool: "request_user_form".to_string(),
            arguments: json!({}),
            reason: SuspendReason::UserForm,
            at_ms,
        })
        .expect("suspend");
    for event in &suspended {
        run.apply(event);
    }
    events.extend(suspended);
    let view = RunView {
        id: decoy.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: 0,
        updated_at_ms: at_ms,
    };
    h.store
        .append_run(&decoy, 0, &events, &view, Some(&h.account))
        .await
        .expect("seed decoy");
    assert_eq!(
        parked(&h).await[0],
        decoy,
        "the decoy is the oldest parked run"
    );

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": entry_id, "agentId": agent, "values": { "email": EMAIL, "password": SECRET } }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert_eq!(
        wait_for_status(&h, &real, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished,
        "the card's own run resumed"
    );
    let (decoy_run, _) = h.store.load_run(&decoy).await.expect("load decoy");
    assert_eq!(
        decoy_run.status,
        opengrok_core::run::RunStatus::AwaitingApproval
    );
    assert!(
        !decoy_run
            .emitted
            .iter()
            .any(|frame| frame["entryId"] == json!(entry_id)),
        "the settled card is journaled onto its own run, not the decoy: {:?}",
        decoy_run.emitted
    );
}

/// #188. Submit types into a live page. A card whose run is no longer waiting on it — stopped by
/// a process that did not close the card — types nothing, says the fill failed, and leaves no
/// secret anywhere.
#[tokio::test]
async fn a_submit_for_a_card_whose_run_is_no_longer_parked_types_nothing() {
    let database_url = database_or_skip!();
    let email = format!("hold-stale-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let runs = parked(&h).await;
    stop_in_store(&h, &runs[0]).await;

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": form["id"], "agentId": agent, "values": { "email": EMAIL, "password": SECRET } }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "fill_failed", "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");
    assert!(h.stub.acts().is_empty(), "nothing typed into the page");
    assert!(!holds_any(&h.tail(&agent).await));
}

/// #188, the review's own case. Two cards from one completion; the parked one is answered first
/// and the run moves on. The other card is still on screen, but its run no longer waits on it:
/// answering it must not type an email into whatever the page now has focused.
#[tokio::test]
async fn a_twin_answered_after_its_run_moved_on_types_nothing() {
    let database_url = database_or_skip!();
    let email = format!("hold-twin-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_two_website_logins()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Twin").await;
    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let cards = h.wait_for_forms(&agent, 2).await;
    let pending_twin = cards
        .iter()
        .find(|card| card["callId"] == "call-42628be6-1")
        .expect("the parked twin");
    let other_twin = cards
        .iter()
        .find(|card| card["callId"] == "call-42628be6")
        .expect("the other twin");
    let run = parked(&h).await.remove(0);

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": pending_twin["id"], "agentId": agent, "values": { "email": EMAIL, "password": SECRET } }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
    let typed = h.stub.acts().len();
    assert!(typed > 0, "the parked twin fills");

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": other_twin["id"], "agentId": agent, "values": { "email": EMAIL, "password": SECRET } }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "fill_failed", "{body}");
    assert_eq!(h.stub.acts().len(), typed, "the late twin typed nothing");
}

/// #186. Stopping a run parked on a form closes the form's card BEFORE the stop answers, so the
/// answer's "the card is closed" is true, and the coworker's next turn may use its screen.
#[tokio::test]
async fn stopping_a_run_parked_on_a_form_closes_its_card_and_frees_the_screen() {
    let database_url = database_or_skip!();
    let email = format!("hold-stop-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let a = thread("a");

    h.turn_on(&token, &agent, &a, "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let runs = parked(&h).await;
    assert_eq!(runs.len(), 1, "{runs:?}");

    let (status, answer) = h
        .agui(
            &token,
            &format!("/ag-ui/runs/{}/stop", runs[0].as_str()),
            json!({}),
        )
        .await;
    assert_eq!(status, 202, "{answer}");
    assert_eq!(answer["takesEffect"], "immediately", "{answer}");
    assert!(
        answer["note"]
            .as_str()
            .is_some_and(|note| note.contains("is closed")),
        "{answer}"
    );

    let card = stored_card(&h, &agent, &entry_id).await;
    assert_eq!(card["formResolution"], "dismissed", "{card}");
    assert_eq!(card["widgetDismissed"], true, "{card}");
    assert!(
        card.get("timedOut").is_none(),
        "a stop is not a timeout: {card}"
    );
    assert!(
        !holds_any(&h.tail(&agent).await),
        "nothing holds the screen"
    );
    assert!(h.stub.acts().is_empty(), "a stop types nothing");

    let sse = h.turn_on(&token, &agent, &a, "look at the screen").await;
    assert!(!sse.contains("handoff is open"), "{sse}");
    assert!(h.stub.shots() >= 1, "the next turn has its screen: {sse}");
}

/// #186. A card whose run was stopped by something that did not close it — another replica, a
/// process that died before it could, or this server before the fix — holds the screen for
/// nobody. After a restart the next turn settles it, whichever conversation that turn is in, and
/// the screen is the coworker's again.
#[tokio::test]
async fn a_card_a_stop_left_open_does_not_hold_the_screen_after_a_restart() {
    let database_url = database_or_skip!();
    let email = format!("hold-orphan-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let runs = parked(&h).await;
    stop_in_store(&h, &runs[0]).await;
    assert!(
        holds_any(&h.tail(&agent).await),
        "the stop left the card open"
    );

    let h2 = harness_on(
        &database_url,
        &email,
        Arc::new(HoldDoor),
        Some(h.account.clone()),
    )
    .await;
    let sse = h2
        .turn_on(&token, &agent, &thread("b"), "look at the screen")
        .await;
    assert!(!sse.contains("handoff is open"), "{sse}");
    assert!(h2.stub.shots() >= 1, "the screen is free again: {sse}");
    let card = stored_card(&h2, &agent, &entry_id).await;
    assert_eq!(card["formResolution"], "dismissed", "{card}");
    assert!(card.get("timedOut").is_none(), "{card}");
    assert!(h2.stub.acts().is_empty());
}

/// #186. The hold's deadline is the card's own timestamp, so a restart does not lose it. A form
/// left past `FORM_HOLD_TIMEOUT` times out on the next turn — even one in another conversation —
/// its run resumes with the timed-out answer rather than being stopped, and the screen is free.
#[tokio::test]
async fn a_form_past_its_deadline_times_out_on_the_next_turn_after_a_restart() {
    let database_url = database_or_skip!();
    let email = format!("hold-deadline-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let run_a = parked(&h).await.remove(0);
    let (seq, mut aged) = h
        .store
        .find_gateway_entry(&coworker, &h.account, &entry_id)
        .await
        .expect("load")
        .expect("row");
    aged["timestampMs"] = json!(now_ms() - hold_ms() - 1_000);
    h.store
        .update_gateway_entry(&coworker, &h.account, seq, &aged)
        .await
        .expect("age the card");

    let h2 = harness_on(
        &database_url,
        &email,
        Arc::new(HoldDoor),
        Some(h.account.clone()),
    )
    .await;
    let sse = h2
        .turn_on(&token, &agent, &thread("b"), "look at the screen")
        .await;
    assert!(!sse.contains("handoff is open"), "{sse}");
    assert!(h2.stub.shots() >= 1, "{sse}");
    let card = stored_card(&h2, &agent, &entry_id).await;
    assert_eq!(card["formResolution"], "dismissed", "{card}");
    assert_eq!(card["timedOut"], true, "{card}");
    assert_eq!(
        wait_for_status(&h2, &run_a, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished,
        "a timed-out form resumes its run with the timeout; it does not stop it"
    );
}

/// #186. And with nobody typing at all: the deadline sweep finds the parked run in the log after
/// a restart, leaves it alone while a person may still answer, and times it out after.
#[tokio::test]
async fn the_deadline_sweep_times_out_a_parked_form_after_a_restart() {
    let database_url = database_or_skip!();
    let email = format!("hold-sweep-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    let before = now_ms();
    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let after = now_ms();
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let run_a = parked(&h).await.remove(0);

    let h2 = harness_on(
        &database_url,
        &email,
        Arc::new(HoldDoor),
        Some(h.account.clone()),
    )
    .await;
    let found = h2
        .store
        .parked_between(before - 1, after)
        .await
        .expect("parked runs");
    assert!(
        found
            .iter()
            .any(|(run, account)| run == &run_a && account == &h.account),
        "the sweep's window finds the parked run and whose it is: {found:?}"
    );
    let mine = [(run_a.clone(), h.account.clone())];

    opengrok_server::agui::user_form::expire_parked_forms(&h2.gateway, &mine, now_ms()).await;
    assert!(
        stored_card(&h2, &agent, &entry_id)
            .await
            .get("formResolution")
            .is_none(),
        "a form inside its deadline is the person's"
    );
    assert_eq!(
        status_of(&h2, &run_a).await,
        opengrok_core::run::RunStatus::AwaitingApproval
    );

    opengrok_server::agui::user_form::expire_parked_forms(
        &h2.gateway,
        &mine,
        now_ms() + hold_ms() + 1_000,
    )
    .await;
    let card = stored_card(&h2, &agent, &entry_id).await;
    assert_eq!(card["formResolution"], "dismissed", "{card}");
    assert_eq!(card["timedOut"], true, "{card}");
    assert_eq!(
        wait_for_status(&h2, &run_a, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
}

/// #186. "Open the screen" hands the computer to the person; the handoff's own timestamp is its
/// deadline. Past it the next turn times the handoff out and resumes the escalated form's run.
#[tokio::test]
async fn a_live_handoff_past_its_deadline_times_out_and_resumes_its_run() {
    let database_url = database_or_skip!();
    let email = format!("hold-handoff-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let run_a = parked(&h).await.remove(0);
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({ "entryId": form["id"], "agentId": agent, "mode": "escalated" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let handoff = h.wait_for_handoff(&agent).await;
    let handoff_id = handoff["id"].as_str().expect("handoff id").to_string();

    // Inside its deadline, another conversation's turn neither ends the handoff nor the run.
    h.turn_on(&token, &agent, &thread("b"), "what is the time")
        .await;
    assert_eq!(parked(&h).await, vec![run_a.clone()]);
    assert!(
        stored_card(&h, &agent, &handoff_id)
            .await
            .get("boxResolution")
            .is_none()
    );

    let (seq, mut aged) = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load")
        .expect("row");
    aged["timestampMs"] = json!(now_ms() - hold_ms() - 1_000);
    h.store
        .update_gateway_entry(&coworker, &h.account, seq, &aged)
        .await
        .expect("age the handoff");

    h.turn_on(&token, &agent, &thread("b"), "what is the time now")
        .await;
    let settled = stored_card(&h, &agent, &handoff_id).await;
    assert_eq!(settled["boxResolution"], "timed_out", "{settled}");
    assert_eq!(
        wait_for_status(&h, &run_a, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
}

/// #186. A stop while the person is on the computer ("Open the screen") ends the handoff too:
/// the run it would hand back to is gone, so the handoff reads declined and holds nothing.
#[tokio::test]
async fn stopping_a_run_whose_form_was_escalated_declines_its_handoff() {
    let database_url = database_or_skip!();
    let email = format!(
        "hold-stop-handoff-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let run = parked(&h).await.remove(0);
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({ "entryId": form["id"], "agentId": agent, "mode": "escalated" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let handoff = h.wait_for_handoff(&agent).await;
    let handoff_id = handoff["id"].as_str().expect("handoff id").to_string();

    let (status, answer) = h
        .agui(
            &token,
            &format!("/ag-ui/runs/{}/stop", run.as_str()),
            json!({}),
        )
        .await;
    assert_eq!(status, 202, "{answer}");
    let settled = stored_card(&h, &agent, &handoff_id).await;
    assert_eq!(settled["boxResolution"], "declined", "{settled}");
    // Nothing is answered, so the form is found as the handoff's own, not by a call (#143).
    let escalated = stored_card(&h, &agent, form["id"].as_str().expect("form id")).await;
    assert_eq!(escalated["boxResolution"], "declined", "{escalated}");
    assert!(!holds_any(&h.tail(&agent).await));
    assert_eq!(
        status_of(&h, &run).await,
        opengrok_core::run::RunStatus::Stopped
    );
}

/// #186. Timing out one card of a pair raised in one completion wakes the run even when the run
/// is parked on the OTHER card and that card was settled without its answer landing: nothing
/// else would ever wake it, and it would hold the screen for good.
#[tokio::test]
async fn a_timed_out_twin_wakes_a_run_parked_on_a_settled_sibling() {
    let database_url = database_or_skip!();
    let email = format!(
        "hold-twin-timeout-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_two_website_logins()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Twin").await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let cards = h.wait_for_forms(&agent, 2).await;
    let run = parked(&h).await.remove(0);
    let parked_on = cards
        .iter()
        .find(|card| card["callId"] == "call-42628be6-1")
        .expect("the parked twin");
    let entry_id = parked_on["id"].as_str().expect("entry id");
    let (seq, mut settled) = h
        .store
        .find_gateway_entry(&coworker, &h.account, entry_id)
        .await
        .expect("load")
        .expect("row");
    settled["formResolution"] = json!("dismissed");
    h.store
        .update_gateway_entry(&coworker, &h.account, seq, &settled)
        .await
        .expect("settle without an answer");

    opengrok_server::agui::user_form::settle_dead_holds(
        &h.gateway,
        &h.account,
        &coworker,
        now_ms() + hold_ms() + 1_000,
    )
    .await;
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
}

/// #186 / #188. A card judged dead from a waiting set read before its park — the order
/// `settle_dead_holds` used to read in, with another conversation parking a sign-in between the
/// two reads — was dismissed as it appeared, and its run sat parked behind a closed card. A dead
/// card is judged again against the runs as they are just before it is closed, so the stale pair
/// settles nothing; once the run really has moved on, the same pair closes it.
#[tokio::test]
async fn a_form_parked_after_the_waiting_set_was_read_stays_open() {
    use opengrok_server::agui::user_form::{settle_holds_seen, waiting_calls};
    let database_url = database_or_skip!();
    let email = format!("hold-stale-set-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    let before = waiting_calls(&h.gateway, &h.account, &coworker)
        .await
        .expect("the log reads");
    h.turn_on(&token, &agent, &thread("b"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    let entries = h
        .store
        .gateway_transcript(&coworker, &h.account)
        .await
        .expect("transcript");

    let written = settle_holds_seen(
        &h.gateway,
        &h.account,
        &coworker,
        &entries,
        &before,
        now_ms(),
    )
    .await;
    assert!(written, "nothing failed to write");
    let card = stored_card(&h, &agent, &entry_id).await;
    assert!(
        card.get("formResolution").is_none(),
        "a sign-in whose run waits on it is the person's: {card}"
    );
    assert_eq!(
        status_of(&h, &run).await,
        opengrok_core::run::RunStatus::AwaitingApproval
    );

    stop_in_store(&h, &run).await;
    settle_holds_seen(
        &h.gateway,
        &h.account,
        &coworker,
        &entries,
        &before,
        now_ms(),
    )
    .await;
    let card = stored_card(&h, &agent, &entry_id).await;
    assert_eq!(card["formResolution"], "dismissed", "{card}");
    assert!(card.get("timedOut").is_none(), "{card}");
}

/// #188. A submit that cannot read the run log cannot tell whether the card's run still waits, and
/// says so: 503, nothing typed, the card left open for a retry. Settled fill_failed instead, a run
/// still parked would sit behind a closed card that no settle or sweep looks at again.
#[tokio::test]
async fn a_submit_that_cannot_read_the_run_log_leaves_the_card_open() {
    let database_url = database_or_skip!();
    let email = format!("hold-unread-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let entry_id = form["id"].as_str().expect("entry id").to_string();
    let run = parked(&h).await.remove(0);
    let stream = opengrok_store::run_stream(&run);
    let original: Value =
        sqlx::query_scalar("select payload from events where stream_id = $1 and stream_seq = 1")
            .bind(&stream)
            .fetch_one(h.store.pool())
            .await
            .expect("first event");
    let rewrite = |payload: Value| {
        sqlx::query("update events set payload = $2 where stream_id = $1 and stream_seq = 1")
            .bind(stream.clone())
            .bind(payload)
            .execute(h.store.pool())
    };
    rewrite(json!({"unreadable": true}))
        .await
        .expect("break the log");

    let submit = json!({ "entryId": entry_id, "agentId": agent, "values": { "email": EMAIL, "password": SECRET } });
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", submit.clone())
        .await;
    assert_eq!(status, 503, "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");
    assert!(h.stub.acts().is_empty(), "nothing typed");
    let card = stored_card(&h, &agent, &entry_id).await;
    assert!(card.get("formResolution").is_none(), "{card}");

    rewrite(original).await.expect("mend the log");
    let (status, body) = h.agui(&token, "/ag-ui/user-form/submit", submit).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
}

/// #186. A run parked on a form whose card was never appended — the process died between the park
/// and the card, or the append failed — holds the coworker's screen in every conversation, and
/// nothing that settles cards reaches it. Past the deadline counted from its park it is answered
/// as timed out, as its card would have been.
#[tokio::test]
async fn a_form_run_whose_card_never_landed_times_out_from_its_park() {
    let database_url = database_or_skip!();
    let email = format!("hold-cardless-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let run = parked(&h).await.remove(0);
    sqlx::query("delete from gateway_entry where coworker_id = $1 and entry->>'id' = $2")
        .bind(agent.as_str())
        .bind(form["id"].as_str().expect("entry id"))
        .execute(h.store.pool())
        .await
        .expect("lose the card");

    opengrok_server::agui::user_form::settle_dead_holds(
        &h.gateway,
        &h.account,
        &coworker,
        now_ms(),
    )
    .await;
    assert_eq!(
        status_of(&h, &run).await,
        opengrok_core::run::RunStatus::AwaitingApproval,
        "inside its deadline the park is the person's"
    );

    opengrok_server::agui::user_form::settle_dead_holds(
        &h.gateway,
        &h.account,
        &coworker,
        now_ms() + hold_ms() + 1_000,
    )
    .await;
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished,
        "a park with no card is woken by its deadline, not left holding the screen"
    );
}

/// #186. The deadline sweep reads each run once, when its last write crosses the deadline. A
/// hold minted after that write — a handoff whose escalation could not be journaled onto the run
/// — reaches its own deadline later, so the sweep says which runs it did not finish with and looks
/// at them again.
#[tokio::test]
async fn the_sweep_looks_again_at_a_run_whose_handoff_is_not_yet_due() {
    let database_url = database_or_skip!();
    let email = format!(
        "hold-sweep-again-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness_with_door(&database_url, &email, Arc::new(HoldDoor)).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn_on(&token, &agent, &thread("a"), "sign in").await;
    let form = h.wait_for_form(&agent).await;
    let run = parked(&h).await.remove(0);
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({ "entryId": form["id"], "agentId": agent, "mode": "escalated" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let handoff = h.wait_for_handoff(&agent).await;
    let handoff_id = handoff["id"].as_str().expect("handoff id").to_string();
    let mine = [(run.clone(), h.account.clone())];

    let again =
        opengrok_server::agui::user_form::expire_parked_forms(&h.gateway, &mine, now_ms()).await;
    assert_eq!(
        again,
        mine.to_vec(),
        "a handoff inside its deadline: look again"
    );
    assert!(
        stored_card(&h, &agent, &handoff_id)
            .await
            .get("boxResolution")
            .is_none()
    );

    let again = opengrok_server::agui::user_form::expire_parked_forms(
        &h.gateway,
        &mine,
        now_ms() + hold_ms() + 1_000,
    )
    .await;
    assert!(again.is_empty(), "{again:?}");
    let settled = stored_card(&h, &agent, &handoff_id).await;
    assert_eq!(settled["boxResolution"], "timed_out", "{settled}");
    assert_eq!(
        wait_for_status(&h, &run, opengrok_core::run::RunStatus::Finished).await,
        opengrok_core::run::RunStatus::Finished
    );
}
