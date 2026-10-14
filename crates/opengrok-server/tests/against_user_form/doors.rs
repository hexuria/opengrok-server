//! Doors that shape the model's asks: passkey pipes, collection forms, ambiguous answers.

use super::*;

/// A passkey card typed nothing and asked the box for its DevTools pipe. The stub box has
/// none, so the card settles as Not filled with the reason, and the bot is told not to type
/// a password; on a shared box the card is refused before anything is asked.
#[tokio::test]
async fn a_passkey_card_asks_the_box_for_its_pipe_and_settles_honestly_without_one() {
    let database_url = database_or_skip!();
    let email = format!("passkey-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(MockDoor::with_script(vec![
            opengrok_harness::ModelDelta::Text("passkey time".to_string()),
            opengrok_harness::ModelDelta::ToolCallStart {
                id: "pk-1".to_string(),
                name: "request_user_form".to_string(),
            },
            opengrok_harness::ModelDelta::ToolCallArgs {
                id: "pk-1".to_string(),
                delta: r#"{"title":"Sign in with your passkey","challengeKind":"passkey","passkeyMode":"use","liveHost":"webauthn.io","fields":[]}"#.to_string(),
            },
            opengrok_harness::ModelDelta::ToolCallEnd {
                id: "pk-1".to_string(),
            },
        ])),
    )
    .await;
    let token = h.access_token(&email);

    // Shared box (the default sharing mode): refused, the card stays open.
    let shared = h.hire(&token, "Ada").await;
    h.turn(&token, &shared, "sign in").await;
    let card = h.wait_for_form(&shared).await;
    assert_eq!(
        card["message"]["formRequest"]["challengeKind"], "passkey",
        "{card}"
    );
    assert_eq!(
        card["message"]["formRequest"]["passkeyMode"], "use",
        "{card}"
    );
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": card["id"].as_str().expect("id"), "agentId": shared, "savedLoginId": "sl_x", "values": {} }),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    // Sharing logins with all the person's Bots does not bring passkeys to that computer:
    // readying one there restarts the browser every Bot on it is using.
    let res = h
        .client
        .put(format!("{}/site-logins/sharing", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "allBots": true }))
        .send()
        .await
        .expect("switch");
    assert_eq!(res.status().as_u16(), 200);
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": card["id"].as_str().expect("id"), "agentId": shared, "savedLoginId": "sl_x", "values": {} }),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "shared-computer", "{body}");
    assert!(h.stub.acts().is_empty(), "nothing done for the passkey");

    // Own box: the pipe is asked for; the stub has none; the card says so.
    h.store
        .set_sharing_mode("account", h.account.as_str(), "per-bot", 1)
        .await
        .expect("per-bot");
    let own = h.hire(&token, "Bea").await;
    // The passkey is one the person shared with this Bot; its key is not in this harness's
    // vault, which is the honest failure below.
    let passkey = format!("sl_pk_{}", uuid::Uuid::now_v7().simple());
    seed_login(
        &h.store,
        &h.account,
        &passkey,
        "passkey",
        Some(own.as_str()),
    )
    .await;
    h.turn(&token, &own, "sign in").await;
    let card = h.wait_for_form(&own).await;
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({ "entryId": card["id"].as_str().expect("id"), "agentId": own, "savedLoginId": passkey, "values": {} }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "fill_failed", "{body}");
    assert!(
        h.stub.acts.lock().expect("acts").is_empty(),
        "a passkey card types nothing"
    );
    // A computer without a pipe has nothing in it touched: no browser is killed.
    let ran = h.stub.ran.lock().expect("ran").clone();
    assert!(
        ran.iter().all(|c| !c.contains("pkill")),
        "nothing is killed on a computer without a pipe: {ran:?}"
    );
}

#[tokio::test]
async fn collect_saved_login_never_reaches_first_resumed_model_request() {
    let database_url = database_or_skip!();
    let email = format!("collect-saved-{}@og.local", uuid::Uuid::now_v7().simple());
    let door = Arc::new(CollectDoor::new());
    let h = harness_with_door(&database_url, &email, door.clone()).await;
    h.store
        .set_sharing_mode("account", h.account.as_str(), "per-bot", 1)
        .await
        .expect("per-bot");
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Collect").await;
    // NativeChat names the saved login it fills from; this one is shared with the Bot.
    let login = format!("sl_{}", uuid::Uuid::now_v7().simple());
    seed_login(
        &h.store,
        &h.account,
        &login,
        "password",
        Some(agent.as_str()),
    )
    .await;
    h.turn(&token, &agent, "collect answers").await;
    let card = h.wait_for_form(&agent).await;
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"], "agentId": agent, "savedLogin": true,
                "savedLoginId": login,
                "values": {"email": EMAIL, "password": SECRET}
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let resumed = door.first_resume().await;
    assert!(
        !resumed.contains(EMAIL),
        "saved-login email leaked to first resume: {resumed}"
    );
    assert!(
        !resumed.contains(SECRET),
        "saved-login password leaked: {resumed}"
    );
    assert!(
        !body.to_string().contains(EMAIL),
        "saved card shares no vault values: {body}"
    );
    h.wait_user_form_idle().await;
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let stored = h
        .store
        .find_gateway_entry(&coworker, &h.account, card["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap()
        .1;
    let history = opengrok_tools::user_form::history_line(&stored).expect("settled history");
    assert!(
        !history.contains(EMAIL),
        "saved-login email leaked in history: {history}"
    );
    assert!(
        !history.contains(SECRET),
        "saved-login secret leaked in history: {history}"
    );
    let replay: Value = h
        .client
        .get(format!("{}/ag-ui/threads/gateway-{agent}", h.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !replay.to_string().contains("credential.offer_save"),
        "{replay}"
    );
    assert!(h.stub.acts().is_empty());
    assert_eq!(h.stub.shots(), 0);
}

#[tokio::test]
async fn collect_submit_returns_answers_without_computer_effects_and_retry_keeps_them() {
    let database_url = database_or_skip!();
    let email = format!("collect-normal-{}@og.local", uuid::Uuid::now_v7().simple());
    let door = Arc::new(CollectDoor::new());
    let h = harness_with_door(&database_url, &email, door.clone()).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Collect").await;
    h.turn(&token, &agent, "collect answers").await;
    let card = h.wait_for_form(&agent).await;
    let resumes = *h.stub.resumes.lock().expect("resumes");
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"], "agentId": agent,
                "values": {"email": EMAIL, "password": SECRET}
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["sharedValues"]["email"], EMAIL);
    let resumed = door.first_resume().await;
    assert!(resumed.contains(EMAIL), "{resumed}");
    assert!(
        resumed.contains("Nothing was typed into a page"),
        "{resumed}"
    );
    assert!(!resumed.contains(SECRET), "{resumed}");
    h.wait_user_form_idle().await;
    let (status, retry) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"], "agentId": agent, "values": {"email": "changed@example.com"}
            }),
        )
        .await;
    assert_eq!(status, 200, "{retry}");
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let stored = h
        .store
        .find_gateway_entry(&coworker, &h.account, card["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(stored["sharedValues"]["email"], EMAIL);
    assert_eq!(
        door.asked.lock().expect("requests").len(),
        2,
        "retry must not resume the model twice"
    );
    assert_eq!(
        door.first_resume().await,
        resumed,
        "retry must preserve first model result"
    );
    assert!(h.stub.acts().is_empty());
    assert_eq!(h.stub.shots(), 0);
    assert_eq!(*h.stub.resumes.lock().expect("resumes"), resumes);
    let replay: Value = h
        .client
        .get(format!("{}/ag-ui/threads/gateway-{agent}", h.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !replay.to_string().contains("credential.offer_save"),
        "{replay}"
    );
}

#[tokio::test]
async fn collect_dismiss_and_timeout_resume_without_fabricated_answers() {
    let database_url = database_or_skip!();
    for timed_out in [false, true] {
        let email = format!("collect-dismiss-{}@og.local", uuid::Uuid::now_v7().simple());
        let door = Arc::new(CollectDoor::new());
        let h = harness_with_door(&database_url, &email, door.clone()).await;
        let token = h.access_token(&email);
        let agent = h.hire(&token, "Collect").await;
        h.turn(&token, &agent, "collect answers").await;
        let card = h.wait_for_form(&agent).await;
        if timed_out {
            let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
            assert!(
                opengrok_server::agui::user_form::settle_dead_holds(
                    &h.gateway,
                    &h.account,
                    &coworker,
                    now_ms() + hold_ms() + 1_000,
                )
                .await
            );
        } else {
            let (status, body) = h
                .agui(
                    &token,
                    "/ag-ui/user-form/dismiss",
                    json!({
                        "entryId": card["id"], "agentId": agent, "mode": "dismissed"
                    }),
                )
                .await;
            assert_eq!(status, 200, "{body}");
        }
        let resumed = door.first_resume().await;
        assert!(
            resumed.contains(if timed_out {
                "timed out"
            } else {
                "without answering"
            }),
            "{resumed}"
        );
        assert!(resumed.contains("without those fields"), "{resumed}");
        assert!(!resumed.contains(EMAIL), "{resumed}");
        assert!(!resumed.contains("credentials"), "{resumed}");
        assert!(h.stub.acts().is_empty());
        assert_eq!(h.stub.shots(), 0);
    }
}

#[tokio::test]
async fn collect_retry_heals_legacy_settled_card_and_filters_ambiguous_answers() {
    let database_url = database_or_skip!();
    for duplicate_kind in ["password", "text"] {
        let email = format!("collect-heal-{}@og.local", uuid::Uuid::now_v7().simple());
        let door = Arc::new(CollectDoor::new());
        let h = harness_with_door(&database_url, &email, door.clone()).await;
        let token = h.access_token(&email);
        let agent = h.hire(&token, "Collect").await;
        h.turn(&token, &agent, "collect answers").await;
        let card = h.wait_for_form(&agent).await;
        let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
        let (seq, mut stored) = h
            .store
            .find_gateway_entry(&coworker, &h.account, card["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        stored["message"]["formRequest"]["fields"] = json!([
            {"id": "email", "label": "Email", "type": "text"},
            {"id": "ambiguous", "label": "Visible", "type": "text"},
            {"id": "ambiguous", "label": "Duplicate", "type": duplicate_kind}
        ]);
        stored["formResolution"] = json!("submitted");
        stored["sharedValues"] = json!({"email": EMAIL, "ambiguous": SECRET});
        h.store
            .update_gateway_entry(&coworker, &h.account, seq, &stored)
            .await
            .unwrap();
        assert_eq!(
            h.pending_user_form_runs().await,
            1,
            "fixture must retain a truly suspended run"
        );
        let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"], "agentId": agent, "values": {"email": "changed@example.com"}
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let resumed = door.first_resume().await;
        assert!(resumed.contains(EMAIL), "{resumed}");
        assert!(
            !resumed.contains(SECRET),
            "legacy ambiguous value leaked during heal: {resumed}"
        );
        assert!(!resumed.contains("changed@example.com"), "{resumed}");
        assert!(
            resumed.contains("Nothing was typed into a page"),
            "{resumed}"
        );
        h.wait_user_form_idle().await;
        // Read the actual persisted malformed row through the public history formatter.
        let stored = h
            .store
            .find_gateway_entry(&coworker, &h.account, card["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .1;
        let history = opengrok_tools::user_form::history_line(&stored).expect("settled history");
        assert!(
            !history.contains(SECRET),
            "legacy ambiguous value leaked in history: {history}"
        );
        assert!(history.contains(EMAIL), "{history}");
        assert!(h.stub.acts().is_empty());
    }
}

#[tokio::test]
async fn collect_network_off_still_waits_and_submits_without_computer_effects() {
    let database_url = database_or_skip!();
    let email = format!("collect-network-{}@og.local", uuid::Uuid::now_v7().simple());
    let door = Arc::new(CollectDoor::new());
    let h = harness_with_door(&database_url, &email, door.clone()).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Collect").await;
    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: true,
    }));
    h.patch_host_settings(&token, json!({"egressTunnelEnabled": true}))
        .await;
    let policy = h
        .client
        .put(format!(
            "{}/coworkers/{agent}/computer/egress-policy",
            h.base
        ))
        .bearer_auth(&token)
        .json(&json!({"mode": "never"}))
        .send()
        .await
        .unwrap();
    assert_eq!(policy.status().as_u16(), 204);
    let resumes = *h.stub.resumes.lock().expect("resumes");
    let sse = h.turn(&token, &agent, "collect answers").await;
    assert!(sse.contains("run-awaiting-approval"), "{sse}");
    let card = h.wait_for_form(&agent).await;
    {
        let asked = door.asked.lock().expect("requests");
        let names: Vec<_> = asked[0]
            .tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"request_user_form"), "{names:?}");
        for browser_tool in ["computer", "open_url"] {
            assert!(
                !names.contains(&browser_tool),
                "browser tool offered with network off: {names:?}"
            );
        }
    }
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"], "agentId": agent, "values": {"email": EMAIL}
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(door.first_resume().await.contains(EMAIL));
    assert!(h.stub.acts().is_empty());
    assert_eq!(h.stub.shots(), 0);
    assert_eq!(*h.stub.resumes.lock().expect("resumes"), resumes);
}

/// A SUBMITTED FORM RESUMES KNOWING WHAT IT WAS FOR (#187). The continuation used to be rebuilt
/// from the run's emitted frames alone, so the model was handed a form result with neither the
/// request that led to it nor the call that asked for it.
#[tokio::test]
async fn a_submitted_form_resumes_with_the_request_it_was_asked() {
    let database_url = database_or_skip!();
    let email = format!(
        "collect-grounded-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let door = Arc::new(CollectDoor::new());
    let h = harness_with_door(&database_url, &email, door.clone()).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Collect").await;
    h.turn(&token, &agent, "collect answers").await;
    let card = h.wait_for_form(&agent).await;
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"], "agentId": agent,
                "values": {"email": EMAIL, "password": SECRET}
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let resumed = door.first_resume().await;
    let at = |needle: &str| {
        resumed
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is missing from the resume: {resumed}"))
    };
    assert!(
        at("collect answers") < at("request_user_form"),
        "the request comes first: {resumed}"
    );
    assert!(
        at("request_user_form") < at(EMAIL),
        "the call comes before what it returned: {resumed}"
    );
    assert!(!resumed.contains(SECRET), "{resumed}");
}
