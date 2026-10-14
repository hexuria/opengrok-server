//! Many forms at once, on the wire and in the run — each gets its own entry id.

use super::*;

#[tokio::test]
async fn stacked_user_forms_in_one_completion_each_get_an_entry_id() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-stacked-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_stacked_user_forms()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Stack").await;
    let thread_id = format!("thr-{}", uuid::Uuid::now_v7());
    let run_id = uuid::Uuid::now_v7().to_string();

    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": thread_id,
            "runId": run_id,
            "messages": [{ "id": "m1", "role": "user", "content": "sign in" }],
            "forwardedProps": { "coworkerId": agent },
        }))
        .send()
        .await
        .expect("post ag-ui");
    assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
    let sse = res.text().await.expect("sse");
    let events = sse_events(&sse);
    assert_eq!(
        events.last().and_then(|event| event["type"].as_str()),
        Some("RUN_FINISHED"),
        "HITL park must close Waiting: {sse}"
    );
    let customs: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "CUSTOM"
                && event["name"] == "run-awaiting-approval"
                && event["reason"] == "user-form"
        })
        .collect();
    assert_eq!(customs.len(), 3, "one CUSTOM per stacked form: {sse}");
    let mut entry_ids = Vec::new();
    for custom in &customs {
        let id = custom["entryId"]
            .as_str()
            .expect("CUSTOM extra.entryId")
            .to_string();
        assert!(id.starts_with("e_"), "entryId is a gateway entry id: {id}");
        assert!(
            !entry_ids.contains(&id),
            "stacked forms must not share an entryId: {entry_ids:?} {id}"
        );
        entry_ids.push(id);
        let dump = custom.to_string();
        assert!(
            !dump.contains("s3cret-should-never-land"),
            "CUSTOM arguments are sanitised: {dump}"
        );
    }
    assert_live_form_tool_calls_carry_gateway_entry_ids(&events, &entry_ids);

    let replay = h
        .client
        .get(format!("{}/ag-ui/runs/{run_id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay run");
    assert_eq!(replay.status().as_u16(), 200);
    let replayed: Value = replay.json().await.expect("replay json");
    assert_eq!(
        replayed["status"].as_str(),
        Some("awaiting-approval"),
        "RUN_FINISHED on the stream must not Finish the parked run: {replayed}"
    );
    let replay_events = replayed["events"].as_array().cloned().unwrap_or_default();
    assert!(
        replay_events
            .iter()
            .any(|event| event["type"] == "RUN_FINISHED"),
        "replay must keep the SSE closer: {replayed}"
    );
    for id in &entry_ids {
        assert!(
            replay_events.iter().any(|event| {
                event["toolCallId"].as_str().is_some() && event["entryId"].as_str() == Some(id)
            }),
            "TOOL_CALL replay must carry entryId {id}: {replayed}"
        );
    }

    let cards = h.wait_for_forms(&agent, 3).await;
    assert_eq!(cards.len(), 3, "{cards:?}");
    // Tail is newest-first; SSE CUSTOMs are oldest-first. Join on id, not zip order.
    let card_ids: Vec<&str> = cards
        .iter()
        .filter_map(|card| card["id"].as_str())
        .collect();
    for id in &entry_ids {
        assert!(
            card_ids.contains(&id.as_str()),
            "gateway card missing for {id}: {cards:?}"
        );
    }
    for card in &cards {
        assert!(
            card.get("callId")
                .and_then(Value::as_str)
                .is_some_and(|call| !call.is_empty()),
            "callId joins Continue to the parked TOOL_CALL: {card}"
        );
    }

    for (index, entry_id) in entry_ids.iter().enumerate() {
        let res = h
            .client
            .post(format!("{}/ag-ui/user-form/submit", h.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "entryId": entry_id,
                "agentId": agent,
                "values": { "email": EMAIL, "password": SECRET }
            }))
            .send()
            .await
            .expect("agui submit");
        assert_eq!(res.status().as_u16(), 200, "submit {index} status");
        let body: Value = res.json().await.expect("agui json");
        assert_eq!(
            body["formResolution"], "submitted",
            "submit {index}: {body}"
        );
        assert!(!body.to_string().contains(SECRET), "{body}");
        if index < 2 {
            assert!(
                h.pending_user_form_runs().await >= 1,
                "extra stacked Continues must not consume the parked call"
            );
        }
    }
    h.wait_user_form_idle().await;
    assert_eq!(h.pending_user_form_runs().await, 0);
}

#[tokio::test]
async fn two_same_completion_website_logins_live_tool_calls_carry_e_ids() {
    let database_url = database_or_skip!();
    let email = format!("user-form-twin-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_two_website_logins()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Twin").await;
    let thread_id = format!("thr-{}", uuid::Uuid::now_v7());
    let run_id = uuid::Uuid::now_v7().to_string();

    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "threadId": thread_id,
            "runId": run_id,
            "messages": [{ "id": "m1", "role": "user", "content": "sign in" }],
            "forwardedProps": { "coworkerId": agent },
        }))
        .send()
        .await
        .expect("post ag-ui");
    assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
    let sse = res.text().await.expect("sse");
    let events = sse_events(&sse);
    let customs: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "CUSTOM"
                && event["name"] == "run-awaiting-approval"
                && event["reason"] == "user-form"
        })
        .collect();
    assert_eq!(customs.len(), 2, "two Website login CUSTOMs: {sse}");
    let mut entry_ids = Vec::new();
    let mut call_ids = Vec::new();
    for custom in &customs {
        let id = custom["entryId"]
            .as_str()
            .expect("CUSTOM extra.entryId")
            .to_string();
        assert!(id.starts_with("e_"), "gateway entryId, not call-*: {id}");
        assert!(
            !entry_ids.contains(&id),
            "twins must not share an entryId: {entry_ids:?}"
        );
        entry_ids.push(id);
        call_ids.push(
            custom["callId"]
                .as_str()
                .expect("CUSTOM callId")
                .to_string(),
        );
    }
    assert_eq!(
        call_ids,
        vec!["call-42628be6".to_string(), "call-42628be6-1".to_string()],
        "{call_ids:?}"
    );
    assert_live_form_tool_calls_carry_gateway_entry_ids(&events, &entry_ids);

    for (index, entry_id) in entry_ids.iter().enumerate() {
        let res = h
            .client
            .post(format!("{}/ag-ui/user-form/submit", h.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "entryId": entry_id,
                "agentId": agent,
                "values": { "email": EMAIL, "password": SECRET }
            }))
            .send()
            .await
            .expect("agui submit");
        assert_eq!(res.status().as_u16(), 200, "submit {index} status");
        let body: Value = res.json().await.expect("agui json");
        assert_eq!(
            body["formResolution"], "submitted",
            "submit {index} of {entry_id}: {body}"
        );
        if index == 0 {
            assert!(
                h.pending_user_form_runs().await >= 1,
                "first Continue must not consume the parked twin"
            );
        }
    }
    h.wait_user_form_idle().await;
    assert_eq!(h.pending_user_form_runs().await, 0);
}

#[tokio::test]
async fn a_new_prompt_while_waiting_interrupts_the_parked_run_as_steer() {
    let database_url = database_or_skip!();
    let email = format!("user-form-steer-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::asking_for_user_form_until_steered()),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Steer").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    assert!(h.pending_user_form_runs().await >= 1);

    let steered = h
        .turn(
            &token,
            &agent,
            "forget the form and list the open tabs instead",
        )
        .await;

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        h.pending_user_form_runs().await,
        0,
        "parked HITL must not remain beside the new turn"
    );

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let form = h
        .store
        .find_gateway_entry(&coworker, &h.account, &entry_id)
        .await
        .expect("load form")
        .expect("form row")
        .1;
    assert_eq!(
        form["formResolution"], "dismissed",
        "interrupt settles leftover cards without filling: {form}"
    );
    assert!(
        h.stub.acts().is_empty(),
        "steer must not Type into the form"
    );

    // The steer reached the MODEL, not just the run log: the mock answers by quoting what it
    // was told, so its answer naming the new text is the proof the parked card did not swallow it.
    let answer = answer_text(&steered);
    assert!(
        answer.contains("list the open tabs")
            && answer.contains("You said:")
            && answer.contains("mock door"),
        "steer must reach the model as a new turn: {answer}"
    );
}

#[tokio::test]
async fn escalate_still_holds_until_hand_back_and_is_not_an_interrupt() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-escalate-hold-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Hold").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let form_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "mode": "escalated"
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let _ = h.wait_for_handoff(&agent).await;
    assert!(
        h.pending_user_form_runs().await >= 1,
        "escalate must not resume or interrupt"
    );
}
