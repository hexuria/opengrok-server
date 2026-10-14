//! The stream itself: ready pings, replacement, timeouts, a mid-relay stop, answers refused.

use super::*;

/// THE STREAM: `ready` first on every connect, a ping every so often, and a second stream from
/// the same machine replaces the first, which is told so and closed. A token that is not a
/// daemon's opens nothing.
#[tokio::test]
async fn a_macs_stream_opens_with_ready_pings_and_is_replaced_by_its_next() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let mut first = h.mac(&ada, "Ada's MacBook").await;
    let ping = first.any().await.expect("a ping");
    assert_eq!(ping, json!({"type": "ping"}));

    let (status, mut second) = h.open(&first.token).await;
    assert_eq!(status, 200);
    let ready = second.recv().await.expect("ready");
    assert_eq!(ready, json!({"type": "ready", "machineId": first.machine}));
    // Within a deadline over the whole drain: a replaced stream left open pings for ever.
    let draining = async {
        let mut said = Vec::new();
        while let Some(frame) = first.frames.recv().await {
            said.push(frame);
        }
        said
    };
    let said = tokio::time::timeout(Duration::from_secs(20), draining).await;
    let said = said.expect("the replaced stream ends within 20 s");
    assert_eq!(said.last(), Some(&json!({"type": "replaced"})), "{said:?}");

    let (status, _) = h.open(&ada.token).await;
    assert_eq!(status, 401, "an access token is not a machine's");
    let (status, _) = h.open("not-a-token").await;
    assert_eq!(status, 401);
}

/// A TURN BY THE MAC IS THE MAC'S TO ANSWER. It is sent the very body the loopback door POSTs for
/// the same turn, with no address and no key anywhere, and its SSE is the run's text; the frame
/// says `via: "mac"`, and the gateway and the loopback proxy are never asked.
#[tokio::test]
async fn a_turn_by_the_mac_is_answered_by_the_mac_with_the_loopbacks_own_body() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let hello = || json!([user("m1", "what does my plan say?")]);

    // The loopback's body for this turn, as its door POSTs it.
    let body = json!({ "kind": "local_proxy", "via": "loopback", "baseUrl": h.proxy_url,
                       "localModel": "gpt-5.5" });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(&ada, &unique("thr"), &run_id(), hello(), props.clone());
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "from the loopback", "{frames:?}");
    let loopback = h.proxy.asked().pop().expect("the loopback was asked");

    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let run = run_id();
    let turn = h.turn(&ada, &unique("thr"), &run, hello(), props);
    let infer = mac.next().await;
    assert_eq!(infer["type"], "infer", "{infer}");
    assert_eq!(infer["runId"], run.as_str());
    assert_eq!(infer["model"], "gpt-5.5");
    assert_eq!(
        infer["request"], loopback,
        "the body the loopback door would POST"
    );
    assert_eq!(infer["request"]["stream"], true);
    let said = infer.to_string();
    for secret in [
        PROXY_KEY,
        h.proxy_url.as_str(),
        "baseUrl",
        "apiKey",
        "oag_live",
    ] {
        assert!(
            !said.contains(secret),
            "no url and no key down the stream: {said}"
        );
    }
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h
        .answer(
            &mac.token,
            id,
            "text/event-stream",
            sse(&words("from the mac")),
        )
        .await;
    assert_eq!(status, 204);
    let (status, frames) = turn.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(text_of(&frames), "from the mac", "{frames:?}");
    assert_eq!(ending(&frames)["type"], "RUN_FINISHED", "{frames:?}");
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})]
    );
    assert_eq!(h.gateway.asked().len(), 0, "never the gateway");
    assert_eq!(
        h.proxy.asked().len(),
        1,
        "the loopback only for its own turn"
    );
    let run = h.run(&run).await;
    assert_eq!(
        (run.inference_source, run.inference_via),
        (SourceKind::LocalProxy, Some(Via::Mac))
    );
}

/// NO MAC, NO FALL BACK: a turn by the Mac with none connected, its computer's relay on but the
/// computer asleep, ends `relay_offline`, in words, and nothing is asked anywhere, not the gateway
/// and not the loopback.
#[tokio::test]
async fn with_no_mac_connected_a_turn_by_the_mac_ends_relay_offline_and_asks_nothing() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let _asleep = h.enrol(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let (thread, run) = (unique("thr"), run_id());
    let turn = h.turn(&ada, &thread, &run, json!([user("m1", "hi")]), props);
    let (status, frames) = turn.await.unwrap();
    assert_eq!(status, 200);
    let end = ending(&frames);
    assert_eq!(end["type"], "RUN_ERROR", "{frames:?}");
    assert_eq!(end["code"], "relay_offline", "{end}");
    let said = end["message"].as_str().unwrap();
    assert!(
        said.contains("your Mac isn't connected") || said.contains("Your Mac isn't connected"),
        "{said}"
    );
    assert_eq!(
        sources(&frames),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})]
    );
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));

    // THE CODE IS THE LOG'S: a replay, of the run or of its thread, ends on the same frame.
    let get = reqwest::Method::GET;
    let (_, replay) = h
        .send(
            Some(&ada.token),
            get.clone(),
            &format!("/ag-ui/runs/{run}"),
            None,
        )
        .await;
    let replayed = replay["events"].as_array().and_then(|events| events.last());
    assert_eq!(
        replayed.map(|frame| &frame["code"]),
        Some(&json!("relay_offline"))
    );
    let (_, replay) = h
        .send(
            Some(&ada.token),
            get,
            &format!("/ag-ui/threads/{thread}"),
            None,
        )
        .await;
    let events = replay["runs"][0]["events"].as_array().unwrap();
    assert_eq!(events.last().unwrap()["code"], "relay_offline", "{replay}");
}

/// A CALL IN FLIGHT OUTLIVES ITS STREAM: an answer is the machine's, by its daemon token, not the
/// connection's, so a call sent down a stream since replaced is answered all the same; its
/// cancel, when it comes, goes to the stream the machine holds now.
#[tokio::test]
async fn a_call_in_flight_when_its_stream_is_replaced_is_answered_all_the_same() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = mac.next().await;
    let (status, mut again) = h.open(&mac.token).await;
    assert_eq!(status, 200);
    assert_eq!(again.recv().await.unwrap()["type"], "ready");
    assert_eq!(mac.next().await, json!({"type": "replaced"}));
    let id = infer["requestId"].as_str().unwrap();
    let body = sse(&words("answered after the swap"));
    assert_eq!(
        h.answer(&mac.token, id, "text/event-stream", body).await.0,
        204
    );
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "answered after the swap", "{frames:?}");
}

/// A MAC THAT DOES NOT ANSWER IS TOLD TO CANCEL: no first byte, and a stream that goes quiet
/// halfway, each end the run `relay_timeout` and send the Mac `cancel` for that request.
#[tokio::test]
async fn a_mac_that_never_starts_or_goes_quiet_times_out_and_is_told_to_cancel() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });

    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props.clone(),
    );
    let infer = mac.next().await;
    let cancel = mac.next().await;
    assert_eq!(
        cancel,
        json!({"type": "cancel", "requestId": infer["requestId"]})
    );
    let (_, frames) = turn.await.unwrap();
    let end = ending(&frames);
    assert_eq!(
        (end["type"].clone(), end["code"].clone()),
        (json!("RUN_ERROR"), json!("relay_timeout"))
    );
    assert!(
        end["message"]
            .as_str()
            .unwrap()
            .contains("did not start answering"),
        "{end}"
    );
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h
        .answer(&mac.token, id, "text/event-stream", sse(&words("late")))
        .await;
    assert_eq!(status, 404, "a request given up on is expired");

    // Halfway: a first line, then nothing.
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = mac.next().await;
    let id = infer["requestId"].as_str().unwrap().to_string();
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::io::Error>>();
    let line = "data: {\"choices\":[{\"delta\":{\"content\":\"half an answer\"}}]}\n\n";
    tx.unbounded_send(Ok(line.as_bytes().to_vec())).unwrap();
    let answering = {
        let (client, base, token) = (h.client.clone(), h.base.clone(), mac.token.clone());
        let id = id.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/inference-relay/responses/{id}"))
                .bearer_auth(token)
                .header("content-type", "text/event-stream")
                .body(reqwest::Body::wrap_stream(rx))
                .send()
                .await
                .map(|res| res.status().as_u16())
        })
    };
    let cancel = mac.next().await;
    assert_eq!(cancel, json!({"type": "cancel", "requestId": id}));
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "half an answer", "{frames:?}");
    let end = ending(&frames);
    assert_eq!(end["code"], "relay_timeout", "{end}");
    assert!(
        end["message"]
            .as_str()
            .unwrap()
            .contains("stopped answering"),
        "{end}"
    );
    drop(tx);
    assert_eq!(answering.await.unwrap().unwrap(), 204);
    assert_eq!(h.gateway.asked().len(), 0);
}

/// THE MAC'S OWN WORDS REACH THE RUN: an `{error}` answer is `relay_failed`, its sentence as sent.
#[tokio::test]
async fn a_macs_error_reaches_the_run_in_its_own_words() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = mac.next().await;
    let why = "You have reached your plan's usage limit; it resets at 14:00.";
    let body = reqwest::Body::from(json!({ "error": why }).to_string());
    let id = infer["requestId"].as_str().unwrap();
    let (status, _) = h.answer(&mac.token, id, "application/json", body).await;
    assert_eq!(status, 204);
    let (_, frames) = turn.await.unwrap();
    let end = ending(&frames);
    assert_eq!(end["type"], "RUN_ERROR", "{frames:?}");
    assert_eq!(end["code"], "relay_failed");
    assert_eq!(end["message"], why);
}

/// STOP REACHES THE MAC: a person stops a run while their Mac is answering it, the Mac is sent
/// `cancel` for that request, and the run ends stopped with what it had said.
#[tokio::test]
async fn stopping_a_run_mid_relay_cancels_the_call_on_the_mac() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let run = run_id();
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run,
        json!([user("m1", "write me a novel")]),
        props,
    );
    let infer = mac.next().await;
    let id = infer["requestId"].as_str().unwrap().to_string();
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::io::Error>>();
    let line = "data: {\"choices\":[{\"delta\":{\"content\":\"Chapter one.\"}}]}\n\n";
    tx.unbounded_send(Ok(line.as_bytes().to_vec())).unwrap();
    let answering = {
        let (client, base, token) = (h.client.clone(), h.base.clone(), mac.token.clone());
        let id = id.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/inference-relay/responses/{id}"))
                .bearer_auth(token)
                .header("content-type", "text/event-stream")
                .body(reqwest::Body::wrap_stream(rx))
                .send()
                .await
                .map(|res| res.status().as_u16())
        })
    };
    // The run has its first words before the person stops it; well inside the Mac's idle clock.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let path = format!("/ag-ui/runs/{run}/stop");
    let (status, stopped) = h
        .send(Some(&ada.token), reqwest::Method::POST, &path, None)
        .await;
    assert_eq!(status, 202, "{stopped}");
    let cancel = mac.next().await;
    assert_eq!(cancel, json!({"type": "cancel", "requestId": id}));
    let (_, frames) = turn.await.unwrap();
    assert!(
        frames.iter().any(|frame| frame["name"] == "run-stopped"),
        "{frames:?}"
    );
    assert_eq!(ending(&frames)["type"], "RUN_FINISHED", "{frames:?}");
    assert_eq!(h.run(&run).await.status, RunStatus::Stopped);
    drop(tx);
    assert_eq!(answering.await.unwrap().unwrap(), 204);
}

/// ONLY THE MACHINE IT WENT TO MAY ANSWER, AND ONCE: another machine of the same person's is
/// 401, as is a token that is not a daemon's; a second answer is 409; an id nothing waits on 404.
#[tokio::test]
async fn an_answer_is_refused_from_another_machine_twice_or_for_no_request() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut older = h.mac(&ada, "Ada's old Mac").await;
    let mut newer = h.mac(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let infer = newer.next().await;
    assert_eq!(infer["type"], "infer", "the newest stream is the one asked");
    let id = infer["requestId"].as_str().unwrap();
    let answer = |token: &str| {
        let token = token.to_string();
        let id = id.to_string();
        let h = &h;
        async move {
            h.answer(&token, &id, "text/event-stream", sse(&words("mine")))
                .await
        }
    };
    let (status, refused) = answer(&older.token).await;
    assert_eq!(status, 401, "{refused}");
    assert!(refused["error"].is_string(), "{refused}");
    assert_eq!(
        answer(&ada.token).await.0,
        401,
        "an access token is not a machine's"
    );
    assert_eq!(answer(&newer.token).await.0, 204);
    let (status, refused) = answer(&newer.token).await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        answer(&older.token).await.0,
        401,
        "and still not the other machine's"
    );
    let (status, refused) = h
        .answer(
            &newer.token,
            &uuid::Uuid::new_v4().to_string(),
            "text/event-stream",
            sse(""),
        )
        .await;
    assert_eq!(status, 404, "{refused}");
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "mine", "{frames:?}");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), older.next())
            .await
            .is_err(),
        "the older stream was sent nothing"
    );
}
