//! Sends held for an absent Mac: they drain on reconnect and never go to another machine.

use super::*;

/// A QUEUED SEND FOR AN ABSENT MAC IS HELD, NOT DROPPED OR RE-ROUTED: its snapshot says
/// `heldFor: "relay_offline"`, firing it answers 202 and leaves it queued, and when the Mac
/// connects the server sends it, as the person's own turn, to the Mac.
#[tokio::test]
async fn a_queued_send_held_for_an_absent_mac_drains_when_the_mac_reconnects() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let asleep = h.enrol(&ada, "Ada's MacBook").await;
    let thread = unique("thr");
    let first = json!([user("m1", "first")]);
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let (status, frames) = h
        .turn(&ada, &thread, &run_id(), first, props)
        .await
        .unwrap();
    assert_eq!(
        (status, text_of(&frames).as_str()),
        (200, "from the gateway")
    );

    let path = format!("/ag-ui/threads/{thread}/pending");
    let body = json!({ "v": 1, "content": "then this, by my Mac", "clientMessageId": "m2",
                       "inferenceSource": { "kind": "local_proxy", "via": "mac" } });
    let post = reqwest::Method::POST;
    let (status, created) = h.send(Some(&ada.token), post, &path, Some(body)).await;
    assert_eq!(status, 201, "{created}");
    let queued = created["pendingUserMessage"].clone();
    assert_eq!(
        queued["inferenceSource"],
        json!({"kind": "local_proxy", "via": "mac"})
    );
    let pending_id = queued["id"].as_str().unwrap().to_string();

    let (_, listed) = h
        .send(Some(&ada.token), reqwest::Method::GET, &path, None)
        .await;
    assert_eq!(
        listed["pendingUserMessages"][0]["heldFor"], "relay_offline",
        "{listed}"
    );
    assert_eq!(
        listed["pendingEvents"][0]["value"]["message"]["heldFor"],
        "relay_offline"
    );

    let second = json!([user("m1", "first"), user("m2", "then this, by my Mac")]);
    let props = json!({ "coworkerId": coworker, "pendingId": pending_id });
    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .bearer_auth(&ada.token)
        .json(
            &json!({ "threadId": thread, "runId": run_id(), "messages": second,
                       "forwardedProps": props }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 202);
    let held: Value = res.json().await.unwrap();
    assert_eq!(held["heldFor"], "relay_offline", "{held}");
    assert_eq!(held["id"], pending_id.as_str());
    assert_eq!(
        held["event"]["value"]["message"]["heldFor"],
        "relay_offline"
    );
    let (_, listed) = h
        .send(Some(&ada.token), reqwest::Method::GET, &path, None)
        .await;
    assert_eq!(
        listed["pendingUserMessages"][0]["id"],
        pending_id.as_str(),
        "still queued"
    );

    let mut mac = h.hold(asleep).await;
    let infer = mac.next().await;
    assert_eq!(
        infer["type"], "infer",
        "the held send, sent by the server: {infer}"
    );
    let said = infer["request"]["messages"].to_string();
    assert!(
        said.contains("then this, by my Mac") && said.contains("first"),
        "{said}"
    );
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h
        .answer(
            &mac.token,
            id,
            "text/event-stream",
            sse(&words("done by the mac")),
        )
        .await;
    assert_eq!(status, 204);

    let mut replay = Value::Null;
    for _ in 0..100 {
        let thread_path = format!("/ag-ui/threads/{thread}");
        replay = h
            .send(Some(&ada.token), reqwest::Method::GET, &thread_path, None)
            .await
            .1;
        let runs = replay["runs"].as_array().map(Vec::len).unwrap_or_default();
        if runs == 2 && replay["runs"][1]["status"] == "finished" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(replay["runs"][1]["status"], "finished", "{replay}");
    assert_eq!(
        replay["pendingUserMessages"],
        json!([]),
        "drained: {replay}"
    );
    let events = replay["runs"][1]["events"].as_array().unwrap();
    // The replay draws the person's words too: theirs, then the Mac's answer.
    assert_eq!(text_of(events), "then this, by my Macdone by the mac");
    assert_eq!(
        sources(events),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})]
    );
    assert_eq!(h.gateway.asked().len(), 1, "only the first turn");
}

/// NO RUN IN SIGHT IS AN IDLE THREAD (review of #298): a send held on a thread whose one turn
/// was hidden, and one on a thread with no turn at all, each go when the Mac connects. The drain
/// read "no turn in sight" as "a turn in flight", and they waited for good.
#[tokio::test]
async fn held_sends_on_threads_with_no_turn_in_sight_go_when_the_mac_connects() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let (hidden, first) = (unique("thr"), run_id());
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let turn = h.turn(&ada, &hidden, &first, json!([user("m1", "first")]), props);
    assert_eq!(turn.await.unwrap().0, 200);
    let (path, post) = (
        format!("/ag-ui/threads/{hidden}/pending"),
        reqwest::Method::POST,
    );
    let body = json!({ "v": 1, "content": "behind a hidden turn", "clientMessageId": "m2" });
    let (status, queued) = h
        .send(Some(&ada.token), post.clone(), &path, Some(body))
        .await;
    assert_eq!(status, 201, "{queued}");
    let hide = format!("/ag-ui/runs/{first}/hide");
    assert_eq!(h.send(Some(&ada.token), post, &hide, None).await.0, 204);
    // No turn at all: the route queues only behind one, and the store is told directly.
    let (fresh, id) = (
        unique("thr"),
        opengrok_core::id::PendingUserMessageId::new(),
    );
    let first_words = opengrok_store::NewPendingUserMessage {
        id: id.as_str(),
        thread_id: &fresh,
        account_id: ada.id.as_str(),
        content: "the first words on a new thread",
        reply_to: None,
        recipe_id: None,
        recipe_values: None,
        skill_id: None,
        client_message_id: Some("m1"),
        inference_source: None,
    };
    let queued = h.store.enqueue_pending_user_message(first_words, now_ms());
    queued.await.expect("queued");

    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let mut asked = Vec::new();
    for _ in 0..2 {
        let infer = mac.next().await;
        assert_eq!(infer["type"], "infer", "{infer}");
        asked.push(infer["request"]["messages"].to_string());
        let id = infer["requestId"].as_str().unwrap();
        let body = sse(&words("sent by the mac"));
        assert_eq!(
            h.answer(&mac.token, id, "text/event-stream", body).await.0,
            204
        );
    }
    let said = |words: &str| asked.iter().any(|asked| asked.contains(words));
    assert!(said("behind a hidden turn") && said("the first words on a new thread"));
    for thread in [&hidden, &fresh] {
        let replay = h.settled(&ada, thread, 1).await;
        assert_eq!(replay["runs"][0]["status"], "finished", "{replay}");
        assert_eq!(replay["pendingUserMessages"], json!([]), "{replay}");
    }
}

/// A TURN IN FLIGHT IS WAITED OUT, NOT GIVEN UP ON (review of #298): the Mac connects while a
/// turn still runs on the thread, and its held send goes when that turn ends. The drain gave up
/// at a busy thread, and with the app closed nothing sent it after. The Mac's `Content-Type` is
/// read case-insensitively.
#[tokio::test]
async fn a_held_send_waits_out_the_turn_in_flight_when_the_mac_connects() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let thread = unique("thr");
    let holding = h.gateway.gate.clone().lock_owned().await;
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let running = h.turn(
        &ada,
        &thread,
        &run_id(),
        json!([user("m1", "first")]),
        props,
    );
    let path = format!("/ag-ui/threads/{thread}/pending");
    let body = json!({ "v": 1, "content": "after the turn in flight", "clientMessageId": "m2" });
    let mut status = 0;
    for _ in 0..100 {
        let post = reqwest::Method::POST;
        status = h
            .send(Some(&ada.token), post, &path, Some(body.clone()))
            .await
            .0;
        if status == 201 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, 201, "queued behind the turn in flight");

    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let early = tokio::time::timeout(Duration::from_millis(700), mac.next()).await;
    assert!(
        early.is_err(),
        "nothing goes while the turn runs: {early:?}"
    );
    drop(holding);
    let (_, frames) = running.await.unwrap();
    assert_eq!(text_of(&frames), "from the gateway", "{frames:?}");
    let infer = mac.next().await;
    assert_eq!(infer["type"], "infer", "{infer}");
    let said = infer["request"]["messages"].to_string();
    assert!(said.contains("after the turn in flight"), "{said}");
    let id = infer["requestId"].as_str().unwrap();
    let kind = "Text/Event-Stream; charset=utf-8";
    assert_eq!(
        h.answer(&mac.token, id, kind, sse(&words("then mine")))
            .await
            .0,
        204
    );
    let replay = h.settled(&ada, &thread, 2).await;
    assert_eq!(replay["runs"][1]["status"], "finished", "{replay}");
    let events = replay["runs"][1]["events"].as_array().unwrap();
    assert_eq!(text_of(events), "after the turn in flightthen mine");
    assert_eq!(replay["pendingUserMessages"], json!([]), "{replay}");
}

/// A RE-ENROLLED MAC'S OLD STREAM IS CLOSED (review of #298): re-enrolment rotates the daemon
/// token, and the stream the old one opened was still sent the person's turns, though no answer
/// from it was taken. It ends at once, sent nothing, and the old token opens nothing again; the
/// new token's stream carries the next turn.
#[tokio::test]
async fn re_enrolling_a_mac_closes_the_stream_its_old_token_opened() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut old = h.mac(&ada, "Ada's MacBook").await;
    let (post, path) = (reqwest::Method::POST, "/local-exec/daemon");
    let body = json!({ "label": "Ada's MacBook", "machineId": old.machine });
    let (status, again) = h.send(Some(&ada.token), post, path, Some(body)).await;
    assert_eq!(
        (status, &again["machineId"]),
        (200, &json!(old.machine)),
        "{again}"
    );

    let ending = async {
        let mut said = Vec::new();
        while let Some(frame) = old.frames.recv().await {
            said.push(frame);
        }
        said
    };
    let said = tokio::time::timeout(Duration::from_secs(5), ending).await;
    let said = said.expect("the old stream ends");
    assert!(said.iter().all(|frame| frame["type"] == "ping"), "{said:?}");
    assert_eq!(
        h.open(&old.token).await.0,
        401,
        "the old token opens nothing"
    );

    let (thread, props) = (unique("thr"), json!({ "coworkerId": coworker }));
    let turn = h.turn(
        &ada,
        &thread,
        &run_id(),
        json!([user("m1", "hi")]),
        props.clone(),
    );
    let (_, frames) = turn.await.unwrap();
    assert_eq!(
        frames.last().unwrap()["code"],
        "relay_offline",
        "{frames:?}"
    );

    let token = again["token"].as_str().unwrap().to_string();
    let (status, frames) = h.open(&token).await;
    assert_eq!(status, 200);
    let machine = old.machine.clone();
    let mut new = Mac {
        machine,
        token,
        frames,
    };
    assert_eq!(new.next().await["type"], "ready");
    let turn = h.turn(
        &ada,
        &thread,
        &run_id(),
        json!([user("m2", "again")]),
        props,
    );
    let infer = new.next().await;
    assert_eq!(infer["type"], "infer", "{infer}");
    let id = infer["requestId"].as_str().unwrap();
    let body = sse(&words("on the new token"));
    assert_eq!(
        h.answer(&new.token, id, "text/event-stream", body).await.0,
        204
    );
    assert_eq!(text_of(&turn.await.unwrap().1), "on the new token");
}
