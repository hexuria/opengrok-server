//! Whose door a queued send waits on: the coworker's own holds, the gateway's does not.

use super::*;

/// A COWORKER'S OWN DOOR HOLDS ITS QUEUE AS THE PERSON'S DOES (review of #304). Its person keeps
/// the Mac as their way while their kind is still the gateway (`via` is kept on its own), and the
/// coworker is on its own plan, so `route` sends its turns to the Mac. A send queued behind one
/// while no Mac is connected is held by the same resolution: the thread's snapshot says
/// `heldFor`, firing it answers 202 and leaves it queued, and when the Mac connects the server
/// sends it there, on the coworker's pin. The old hold read only the send's pick and the account,
/// so the fire drained and ended `relay_offline`.
#[tokio::test]
async fn a_coworkers_own_door_holds_its_queued_send_for_an_absent_mac() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let body = json!({ "kind": "gateway", "via": "mac", "relay": { "localModel": "gpt-5.5" } });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(status, 200, "{saved}");
    let asleep = h.enrol(&ada, "Ada's MacBook").await;
    let coworker = h.hire(&ada, "Luna").await;
    let own = Some(json!({ "source": "local_proxy", "model": "gpt-6-luna" }));
    let path = format!("/coworkers/{coworker}");
    let (status, row) = h
        .send(Some(&ada.token), reqwest::Method::PATCH, &path, own)
        .await;
    assert_eq!(status, 200, "{row}");
    let thread = unique("thr");
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let first = json!([user("m1", "first")]);
    let (status, frames) = h
        .turn(&ada, &thread, &run_id(), first, props)
        .await
        .unwrap();
    assert_eq!(
        (status, text_of(&frames).as_str()),
        (200, "from the gateway")
    );

    let pending = format!("/ag-ui/threads/{thread}/pending");
    let body = json!({ "v": 1, "content": "then this, on my plan", "clientMessageId": "m2" });
    let post = reqwest::Method::POST;
    let (status, created) = h.send(Some(&ada.token), post, &pending, Some(body)).await;
    assert_eq!(status, 201, "{created}");
    let id = created["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let get = || reqwest::Method::GET;
    let replay_path = format!("/ag-ui/threads/{thread}");
    let (_, replay) = h.send(Some(&ada.token), get(), &replay_path, None).await;
    assert_eq!(
        replay["pendingUserMessages"][0]["heldFor"], "relay_offline",
        "{replay}"
    );
    let (_, listed) = h.send(Some(&ada.token), get(), &pending, None).await;
    assert_eq!(
        listed["pendingUserMessages"][0]["heldFor"], "relay_offline",
        "{listed}"
    );
    assert_eq!(
        listed["pendingEvents"][0]["value"]["message"]["heldFor"],
        "relay_offline"
    );

    let second = json!([user("m1", "first"), user("m2", "then this, on my plan")]);
    let props = json!({ "coworkerId": coworker, "pendingId": id });
    let (status, held) = fire(&h, &ada, &thread, second, props).await;
    assert_eq!(status, 202, "{held}");
    assert_eq!(
        (&held["heldFor"], &held["id"]),
        (&json!("relay_offline"), &json!(id))
    );
    let (_, listed) = h.send(Some(&ada.token), get(), &pending, None).await;
    assert_eq!(
        listed["pendingUserMessages"][0]["id"],
        id.as_str(),
        "still queued: {listed}"
    );

    let mut mac = h.hold(asleep).await;
    let infer = mac.next().await;
    assert_eq!(
        infer["type"], "infer",
        "the held send, sent by the server: {infer}"
    );
    assert_eq!(
        infer["model"], "gpt-6-luna",
        "on the coworker's own plan: {infer}"
    );
    let said = infer["request"]["messages"].to_string();
    assert!(said.contains("then this, on my plan"), "{said}");
    let answered = sse(&words("done by the mac"));
    let (status, _) = h
        .answer(
            &mac.token,
            infer["requestId"].as_str().unwrap(),
            "text/event-stream",
            answered,
        )
        .await;
    assert_eq!(status, 204);
    let replay = h.settled(&ada, &thread, 2).await;
    assert_eq!(
        replay["pendingUserMessages"],
        json!([]),
        "drained: {replay}"
    );
    let events = replay["runs"][1]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let by_mac = json!({"kind": "local_proxy", "via": "mac", "model": "gpt-6-luna"});
    assert_eq!(sources(&events), std::slice::from_ref(&by_mac), "{replay}");
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (1, 0));
}

/// The converse: a coworker on the gateway door, on an account whose kind is its own plan by the
/// Mac, goes to the gateway, so its queued send is never held for a Mac and fires at once.
#[tokio::test]
async fn a_gateway_coworkers_queued_send_is_not_held_for_the_mac() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "via": "mac", "relay": { "localModel": "gpt-5.5" } });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(status, 200, "{saved}");
    let coworker = h.hire(&ada, "Ada").await;
    let door = Some(json!({ "source": "gateway" }));
    let path = format!("/coworkers/{coworker}");
    let (status, row) = h
        .send(Some(&ada.token), reqwest::Method::PATCH, &path, door)
        .await;
    assert_eq!(status, 200, "{row}");
    let thread = unique("thr");
    let props = json!({ "coworkerId": coworker });
    let first = json!([user("m1", "first")]);
    let (status, frames) = h
        .turn(&ada, &thread, &run_id(), first, props)
        .await
        .unwrap();
    assert_eq!(
        (status, text_of(&frames).as_str()),
        (200, "from the gateway")
    );

    let pending = format!("/ag-ui/threads/{thread}/pending");
    let body = json!({ "v": 1, "content": "then this", "clientMessageId": "m2" });
    let post = reqwest::Method::POST;
    let (status, created) = h.send(Some(&ada.token), post, &pending, Some(body)).await;
    assert_eq!(status, 201, "{created}");
    let id = created["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, listed) = h
        .send(Some(&ada.token), reqwest::Method::GET, &pending, None)
        .await;
    let queued = &listed["pendingUserMessages"][0];
    assert!(queued.get("heldFor").is_none(), "not held: {listed}");

    let second = json!([user("m1", "first"), user("m2", "then this")]);
    let props = json!({ "coworkerId": coworker, "pendingId": id });
    let turn = h.turn(&ada, &thread, &run_id(), second, props);
    let (status, frames) = turn.await.unwrap();
    assert_eq!(
        (status, text_of(&frames).as_str()),
        (200, "from the gateway"),
        "{frames:?}"
    );
    assert_eq!(h.gateway.asked().len(), 2, "both turns at the gateway");
}
