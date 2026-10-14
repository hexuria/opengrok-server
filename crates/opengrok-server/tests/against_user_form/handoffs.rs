//! Live handoffs: a form escalated to the person holds the run until hand-back, skip or timeout.

use super::*;

#[tokio::test]
async fn dismissed_resumes_without_a_handoff() {
    let database_url = database_or_skip!();
    let email = format!("user-form-skip-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Eve").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();

    let (status, body) = h
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
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "dismissed", "{body}");
    assert!(body.get("handoffEntryId").is_none(), "{body}");
    assert!(h.stub.acts().is_empty());

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(h.pending_user_form_runs().await, 0);
    let tail = h.tail(&agent).await;
    let has_handoff = tail["entries"].as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry
                .get("boxRequestId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        })
    });
    assert!(
        !has_handoff,
        "dismissed must not start a box handoff: {tail}"
    );
}

#[tokio::test]
async fn an_unanswered_form_times_out_and_resumes() {
    let database_url = database_or_skip!();
    let email = format!("user-form-to-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Fay").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    assert!(h.pending_user_form_runs().await >= 1);

    // Ten minutes on, as far as the card's own stamp is concerned.
    let settled = opengrok_server::agui::user_form::settle_dead_holds(
        &h.gateway,
        &h.account,
        &coworker,
        now_ms() + hold_ms() + 1_000,
    )
    .await;
    assert!(settled, "the timeout could read and write the transcript");

    let stored = h
        .store
        .find_gateway_entry(&coworker, &h.account, &entry_id)
        .await
        .expect("load")
        .expect("row")
        .1;
    assert_eq!(stored["formResolution"], "dismissed", "{stored}");
    assert_eq!(stored["timedOut"], true, "{stored}");
    assert_eq!(stored["widgetDismissed"], true, "{stored}");

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(h.pending_user_form_runs().await, 0);
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn agui_handoff_resolve_declines_without_stopping_the_box() {
    let database_url = database_or_skip!();
    let email = format!("user-form-hb-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Gia").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();

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
    let handoff_id = body["handoffEntryId"]
        .as_str()
        .expect("handoff id")
        .to_string();
    assert!(h.pending_user_form_runs().await >= 1);

    let res = h
        .client
        .post(format!("{}/ag-ui/box-handoff/resolve", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "entryId": handoff_id,
            "agentId": agent,
            "resolution": "declined"
        }))
        .send()
        .await
        .expect("agui resolve");
    assert_eq!(res.status().as_u16(), 200, "agui resolve status");
    let resolved: Value = res.json().await.expect("agui json");
    assert_eq!(resolved["boxResolution"], "declined", "{resolved}");

    for _ in 0..50 {
        if h.pending_user_form_runs().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(h.pending_user_form_runs().await, 0);
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn skip_via_form_entry_id_settles_the_live_handoff_and_resumes() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-skip-form-id-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ivy").await;

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
    let handoff_id = body["handoffEntryId"]
        .as_str()
        .expect("handoff id")
        .to_string();
    let _ = h.wait_for_handoff(&agent).await;
    assert!(
        h.pending_user_form_runs().await >= 1,
        "escalate must not resume"
    );

    // NativeChat KeepAlive: Skip posts the form gateway id when handoffEntryId is missing.
    let (status, resolved) = h
        .agui(
            &token,
            "/ag-ui/box-handoff/resolve",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "resolution": "declined"
            }),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["boxResolution"], "declined", "{resolved}");

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let form = h
        .store
        .find_gateway_entry(&coworker, &h.account, &form_id)
        .await
        .expect("load form")
        .expect("form row")
        .1;
    assert_eq!(form["formResolution"], "escalated", "{form}");
    assert!(
        form.get("boxRequestId").is_none(),
        "resolve must not convert the form into a handoff: {form}"
    );
    assert_eq!(
        form["boxResolution"], "declined",
        "the form says how its handoff ended, or it replays live (#143): {form}"
    );
    let handoff = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert_eq!(handoff["boxResolution"], "declined", "{handoff}");

    h.wait_user_form_idle().await;
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn dismiss_dismissed_after_escalate_abandons_the_live_handoff() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-abandon-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Jen").await;

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
    let handoff_id = body["handoffEntryId"]
        .as_str()
        .expect("handoff id")
        .to_string();
    let _ = h.wait_for_handoff(&agent).await;
    assert!(h.pending_user_form_runs().await >= 1);

    let (status, dismissed) = h
        .agui(
            &token,
            "/ag-ui/user-form/dismiss",
            json!({
                "entryId": form_id,
                "agentId": agent,
                "mode": "dismissed"
            }),
        )
        .await;
    assert_eq!(status, 200, "{dismissed}");
    assert_eq!(dismissed["formResolution"], "escalated", "{dismissed}");
    assert!(dismissed.get("boxRequestId").is_none(), "{dismissed}");

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let handoff = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert_eq!(handoff["boxResolution"], "declined", "{handoff}");
    let form = stored_card(&h, &agent, &form_id).await;
    assert_eq!(form["boxResolution"], "declined", "{form}");
    h.wait_user_form_idle().await;
    assert!(h.stub.acts().is_empty());
}

#[tokio::test]
async fn a_second_escalate_does_not_resume_while_the_handoff_is_live() {
    let database_url = database_or_skip!();
    let email = format!("user-form-reesc-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Kim").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let form_id = card["id"].as_str().expect("entry id").to_string();

    let (status, first) = h
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
    assert_eq!(status, 200, "{first}");
    let handoff_id = first["handoffEntryId"]
        .as_str()
        .expect("handoff id")
        .to_string();
    let _ = h.wait_for_handoff(&agent).await;

    let (status, again) = h
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
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["formResolution"], "escalated", "{again}");
    assert!(
        h.pending_user_form_runs().await >= 1,
        "a second escalate must not resume"
    );

    let coworker = opengrok_core::id::CoworkerId::from_stored(agent.clone());
    let handoff = h
        .store
        .find_gateway_entry(&coworker, &h.account, &handoff_id)
        .await
        .expect("load handoff")
        .expect("handoff row")
        .1;
    assert!(
        handoff.get("boxResolution").is_none(),
        "escalate retry must leave the handoff live: {handoff}"
    );

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
    h.wait_user_form_idle().await;
}

#[tokio::test]
async fn a_missing_form_entry_is_an_error_not_null() {
    let database_url = database_or_skip!();
    let email = format!("user-form-miss-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Hal").await;

    let res = h
        .client
        .post(format!("{}/ag-ui/user-form/submit", h.base))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "entryId": "e_ghost_never_appended",
            "agentId": agent,
            "values": { "email": EMAIL }
        }))
        .send()
        .await
        .expect("agui submit ghost");
    assert_eq!(res.status().as_u16(), 404, "ghost entryId must not be Null");
    let body: Value = res.json().await.expect("agui json");
    assert_ne!(body, Value::Null, "{body}");
    assert_eq!(body["error"], "form entry missing", "{body}");
}
