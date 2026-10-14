//! Submit, answer and dismiss: the card settles, secrets stay out, the run resumes.

use super::*;

#[tokio::test]
async fn submit_types_into_the_box_settles_the_card_and_strips_secrets() {
    let database_url = database_or_skip!();
    let email = format!("user-form-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    assert_eq!(card["kind"], "send-message");
    assert_eq!(card["message"]["type"], "user-form");
    assert_eq!(card["message"]["formRequest"]["title"], "Google account");
    let pending_dump = card.to_string();
    assert!(
        !pending_dump.contains("s3cret-should-never-land"),
        "the model's smuggled values must not sit on the entry: {pending_dump}"
    );
    assert!(
        !pending_dump.contains(SECRET),
        "no submit value yet: {pending_dump}"
    );
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
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
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert_eq!(body["sharedValues"]["email"], EMAIL, "{body}");
    assert_eq!(body["formFieldOutcomes"][0]["id"], "email", "{body}");
    assert_eq!(body["formFieldOutcomes"][0]["filled"], true, "{body}");
    assert_eq!(body["formFieldOutcomes"][0]["fillFailed"], false, "{body}");
    assert_eq!(body["formFieldOutcomes"][1]["id"], "password", "{body}");
    assert_eq!(body["formFieldOutcomes"][1]["filled"], false, "{body}");
    assert_eq!(body["formFieldOutcomes"][1]["fillFailed"], false, "{body}");
    assert!(
        body.get("boxRequestId").is_none(),
        "user-form must not carry boxRequestId: {body}"
    );
    assert!(
        body.get("sharedValues")
            .and_then(|v| v.get("password"))
            .is_none(),
        "password is not a shared value: {body}"
    );
    let settled_dump = body.to_string();
    assert!(!settled_dump.contains(SECRET), "{settled_dump}");

    let mut acts = Vec::new();
    for _ in 0..50 {
        acts = h.stub.acts();
        if !acts.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        acts,
        vec![CuaAction::Type {
            text: EMAIL.to_string()
        }],
        "default multi-field types only the first field"
    );
    assert!(
        !acts
            .iter()
            .any(|act| matches!(act, CuaAction::Type { text } if text == SECRET)),
        "must not type the password into the email box: {acts:?}"
    );
    assert_eq!(h.stub.shots(), 0, "fill must not screenshot");

    let tail = h.tail(&agent).await;
    let dump = dumped(&tail);
    assert!(!dump.contains(SECRET), "secret in transcript: {dump}");
    assert!(
        dump.contains("submitted") || dump.contains(EMAIL),
        "settled card still visible: {dump}"
    );
}

#[tokio::test]
async fn agui_rest_submit_round_trips_with_an_account_bearer() {
    let database_url = database_or_skip!();
    let email = format!("user-form-agui-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Bea").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();

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
    assert_eq!(res.status().as_u16(), 200, "agui submit status");
    let body: Value = res.json().await.expect("agui json");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");

    for _ in 0..50 {
        if !h.stub.acts().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        h.stub
            .acts()
            .iter()
            .any(|act| matches!(act, CuaAction::Type { text } if text == EMAIL)),
        "AG-UI submit still types the focused field: {:?}",
        h.stub.acts()
    );
    assert!(
        !h.stub
            .acts()
            .iter()
            .any(|act| matches!(act, CuaAction::Type { text } if text == SECRET)),
        "must not type the password into the email box: {:?}",
        h.stub.acts()
    );
    assert!(
        !h.stub
            .acts()
            .iter()
            .any(|act| matches!(act, CuaAction::Key { key } if key == "Return")),
        "default multi-field must not Return: {:?}",
        h.stub.acts()
    );
    assert_eq!(h.stub.shots(), 0);

    let (status, dismissed) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": entry_id,
                "agentId": agent,
                "mode": "dismissed"
            }),
        )
        .await;
    assert_eq!(status, 200, "{dismissed}");
    assert_eq!(
        dismissed["alreadyAnswered"], true,
        "a second answer is a no-op: {dismissed}"
    );
}

#[tokio::test]
async fn dismiss_settles_without_typing() {
    let database_url = database_or_skip!();
    let email = format!("user-form-d-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Cam").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    assert!(h.pending_user_form_runs().await >= 1);

    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": entry_id,
                "agentId": agent,
                "mode": "escalated"
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "escalated", "{body}");
    assert_eq!(body["widgetDismissed"], true, "{body}");
    assert!(
        body.get("boxRequestId").is_none(),
        "escalate must not put boxRequestId on the user-form: {body}"
    );
    let handoff_id = body["handoffEntryId"]
        .as_str()
        .expect("handoffEntryId on escalate response")
        .to_string();
    assert!(h.stub.acts().is_empty(), "dismiss must not Type");
    assert_eq!(h.stub.shots(), 0);
    assert!(!body.to_string().contains(SECRET), "{body}");

    let handoff = h.wait_for_handoff(&agent).await;
    assert_eq!(handoff["id"], handoff_id);
    assert_eq!(handoff["message"]["type"], "attachment");
    assert_eq!(handoff["message"]["url"], "sand://box");
    assert!(
        handoff["boxRequestId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{handoff}"
    );
    assert!(
        handoff.get("boxResolution").is_none(),
        "boxResolution absent while live: {handoff}"
    );
    let dumped = handoff.to_string().to_lowercase();
    assert!(!dumped.contains("take over"), "{dumped}");
    assert!(!dumped.contains("i'm done"), "{dumped}");
    assert!(!dumped.contains("skip"), "{dumped}");

    // Escalated must KEEP hold: the user-form run is still awaiting hand-back.
    assert!(
        h.pending_user_form_runs().await >= 1,
        "escalate must not resume the run"
    );

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let (status, resolved) = h
        .agui(
            &token,
            "/ag-ui/box-handoff/resolve",
            json!({
                "entryId": handoff_id,
                "agentId": agent,
                "resolution": "handed_back"
            }),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["boxResolution"], "handed_back", "{resolved}");
    assert!(
        resolved.get("formResolution").is_none(),
        "handoff is not a user-form: {resolved}"
    );

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        h.pending_user_form_runs().await,
        0,
        "hand-back resumes the waiting run"
    );
    assert!(h.stub.acts().is_empty(), "hand-back must not Type");
    assert_eq!(h.stub.shots(), 0, "hand-back must not screenshot");
    // handBackForeverBox would stop the box; this path only stamps boxResolution.
    let still = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert_eq!(still["boxResolution"], "handed_back");
}

#[tokio::test]
async fn an_agui_only_turn_still_mints_a_user_form_entry_id() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-agui-only-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Dot").await;
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
    // NativeChat mounts this CUSTOM frame. `entryId` must be on the SSE, not only on the
    // gateway transcript: they never watch `transcript:{agentId}`. TOOL_CALL_ARGS is the
    // model's raw delta (the mock smuggles `values` there on purpose).
    let mut custom_entry_id = None;
    for chunk in sse.split("\n\n") {
        let Some(payload) = chunk.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if event["type"] == "CUSTOM" && event["name"] == "run-awaiting-approval" {
            assert_eq!(event["reason"], "user-form", "{event}");
            let dump = event.to_string();
            assert!(
                !dump.contains("s3cret-should-never-land"),
                "CUSTOM arguments are sanitised: {dump}"
            );
            assert!(
                event
                    .get("arguments")
                    .and_then(|args| args.get("values"))
                    .is_none(),
                "values must not sit on CUSTOM: {event}"
            );
            assert_eq!(
                event.get("formRequest"),
                event.get("arguments"),
                "formRequest is the same sanitised schema: {event}"
            );
            let id = event["entryId"]
                .as_str()
                .expect("CUSTOM extra.entryId")
                .to_string();
            assert!(id.starts_with("e_"), "entryId is a gateway entry id: {id}");
            custom_entry_id = Some(id);
        }
    }
    let entry_id = custom_entry_id.expect("CUSTOM must still stream with entryId");

    let card = h.wait_for_form(&agent).await;
    assert_eq!(card["kind"], "send-message");
    assert_eq!(card["message"]["type"], "user-form");
    assert_eq!(
        card["id"].as_str().expect("card id"),
        entry_id.as_str(),
        "SSE entryId is the gateway card id"
    );
    let pending_dump = card.to_string();
    assert!(
        !pending_dump.contains("s3cret-should-never-land"),
        "the model's smuggled values must not sit on the entry: {pending_dump}"
    );

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
    assert_eq!(res.status().as_u16(), 200, "agui submit status");
    let body: Value = res.json().await.expect("agui json");
    assert!(
        body.is_object(),
        "NativeChat collapses on Null submit body: {body}"
    );
    assert!(
        matches!(
            body["formResolution"].as_str(),
            Some("submitted") | Some("fill_failed")
        ),
        "submit of a stamped entryId must carry formResolution: {body}"
    );
    assert!(
        body.get("formFieldOutcomes")
            .and_then(Value::as_array)
            .is_some(),
        "formFieldOutcomes required on the happy path: {body}"
    );
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(!body.to_string().contains(SECRET), "{body}");

    let tail = h.tail(&agent).await;
    let tail_dump = dumped(&tail);
    assert!(
        !tail_dump.contains(SECRET),
        "secret in gateway tail: {tail_dump}"
    );
    let settled_tail = tail["entries"].as_array().and_then(|entries| {
        entries.iter().find(|entry| {
            entry["message"]["type"] == "user-form" && entry.get("formResolution").is_some()
        })
    });
    let settled_tail = settled_tail.expect("getAgentTranscriptTail must emit the settled card");
    assert_eq!(
        settled_tail["formResolution"], "submitted",
        "{settled_tail}"
    );
    assert_eq!(
        settled_tail["message"]["type"], "user-form",
        "{settled_tail}"
    );

    let replay = h
        .client
        .get(format!("{}/ag-ui/threads/{thread_id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay thread");
    assert_eq!(replay.status().as_u16(), 200, "thread replay");
    let replayed: Value = replay.json().await.expect("replay json");
    let replay_dump = replayed.to_string();
    assert!(
        !replay_dump.contains(SECRET),
        "secret in AG-UI replay: {replay_dump}"
    );
    let settled_replay = replayed["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["events"].as_array())
        .flatten()
        .find(|event| {
            event.get("formResolution").and_then(Value::as_str) == Some("submitted")
                && (event["message"]["type"] == "user-form"
                    || event["name"] == "user-form"
                    || event["reason"] == "user-form")
        });
    let settled_replay = settled_replay.unwrap_or_else(|| {
        panic!("GET /ag-ui/threads/{{id}} must emit settled user-form for NativeChat hydrate: {replay_dump}")
    });
    assert_eq!(settled_replay["formResolution"], "submitted");
    assert!(
        settled_replay["message"]["type"] == "user-form"
            || settled_replay.pointer("/value/message/type") == Some(&json!("user-form")),
        "message.type user-form: {settled_replay}"
    );
    let offer = replayed["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["events"].as_array())
        .flatten()
        .find(|event| event["name"] == "credential.offer_save");
    let offer = offer.expect("submitted fill must offer_save on AG-UI replay");
    assert_eq!(offer["origin"], "accounts.google.com", "{offer}");
    assert_eq!(offer["username"], EMAIL, "{offer}");
    assert_eq!(offer["formEntryId"], entry_id, "{offer}");
    let offer_dump = offer.to_string();
    assert!(
        !offer_dump.contains(SECRET),
        "offer_save must never include the password: {offer_dump}"
    );
    assert!(offer.get("password").is_none(), "{offer}");

    for _ in 0..50 {
        if !h.stub.acts().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        h.stub.acts(),
        vec![CuaAction::Type {
            text: EMAIL.to_string()
        }],
        "default multi-field types only the first field"
    );
    assert_eq!(h.stub.shots(), 0, "fill must not screenshot");
}
