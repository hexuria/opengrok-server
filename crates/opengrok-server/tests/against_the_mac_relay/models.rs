//! The model list and the relay pin a coworker asks for.

use super::*;

/// THE PICKER: with a Mac connected its models are listed beside the gateway's, `via: "mac"`,
/// kept to what a subscription may run, and `localProxy` says the relay is connected.
#[tokio::test]
async fn the_model_list_carries_the_macs_models_and_says_it_is_connected() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let body = json!({ "kind": "local_proxy", "via": "mac" });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let (status, away) = h
        .send(Some(&ada.token), reqwest::Method::GET, "/models", None)
        .await;
    assert_eq!(status, 200, "{away}");
    assert_eq!(
        away["localProxy"],
        json!({"healthy": false, "relayConnected": false})
    );

    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let listing = {
        let (client, base, token) = (h.client.clone(), h.base.clone(), ada.token.clone());
        tokio::spawn(async move {
            let res = client
                .get(format!("{base}/models"))
                .bearer_auth(token)
                .send()
                .await;
            res.unwrap().json::<Value>().await.unwrap()
        })
    };
    let asked = mac.next().await;
    assert_eq!(asked["type"], "models", "{asked}");
    let models = json!({"object": "list", "data": [
        {"id": "gpt-6-sol"}, {"id": "claude-sonnet-4.5"}, {"id": "xai/grok-4.7"}]});
    let id = asked["requestId"].as_str().unwrap();
    let body = reqwest::Body::from(models.to_string());
    assert_eq!(
        h.answer(&mac.token, id, "application/json", body).await.0,
        204
    );
    let listing = listing.await.unwrap();
    let mac_entries: Vec<Value> = listing["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["via"] == "mac")
        .cloned()
        .collect();
    assert_eq!(
        mac_entries,
        [
            json!({"id": "gpt-6-sol", "points": null, "source": "local_proxy", "via": "mac",
                   "efforts": null, "ownEffort": null}),
            json!({"id": "xai/grok-4.7", "points": null, "source": "local_proxy", "via": "mac",
                   "efforts": null, "ownEffort": null}),
        ]
    );
    assert_eq!(
        listing["localProxy"],
        json!({"healthy": false, "relayConnected": true})
    );
    assert!(
        listing["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["source"] == "gateway"),
        "{listing}"
    );
}

/// THE SETTING: `via` and the Mac's own model are saved and merged as the other fields are, the
/// model held to the same allowlist; `relay` reads back whether a Mac is connected and which;
/// `helper` is refused until it is built.
#[tokio::test]
async fn a_person_chooses_their_mac_and_reads_the_relay_back() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let body =
        json!({ "kind": "local_proxy", "via": "mac", "relay": { "localModel": "gpt-6-sol" } });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["via"], "mac");
    assert_eq!(
        saved["relay"],
        json!({"connected": false, "machineId": null, "machineLabel": null,
               "localModel": "gpt-6-sol"})
    );

    let mac = h.mac(&ada, "Ada's MacBook").await;
    let read = h.read(&ada).await;
    assert_eq!(
        read["relay"],
        json!({"connected": true, "machineId": mac.machine, "machineLabel": "Ada's MacBook",
               "localModel": "gpt-6-sol"})
    );

    // Absent keeps; the kind alone changes nothing else.
    let (_, kept) = h.set(&ada, json!({ "kind": "gateway" })).await;
    assert_eq!(
        (kept["via"].clone(), kept["relay"]["localModel"].clone()),
        (json!("mac"), json!("gpt-6-sol"))
    );
    for (body, field) in [
        (json!({ "kind": "local_proxy", "via": "helper" }), "via"),
        (
            json!({ "kind": "local_proxy", "via": "carrier-pigeon" }),
            "via",
        ),
        (
            json!({ "kind": "local_proxy", "relay": { "localModel": "claude-sonnet-4.5" } }),
            "relay.localModel",
        ),
        (
            json!({ "kind": "local_proxy", "relay": "gpt-6-sol" }),
            "relay",
        ),
    ] {
        let (status, refused) = h.set(&ada, body.clone()).await;
        assert_eq!(status, 400, "{body}: {refused}");
        let why = refused["error"].as_str().unwrap();
        assert!(why.starts_with(field), "{body}: {why}");
    }
    let (_, refused) = h
        .set(&ada, json!({ "kind": "local_proxy", "via": "helper" }))
        .await;
    assert!(
        refused["error"].as_str().unwrap().contains("not built yet"),
        "{refused}"
    );
    let read = h.read(&ada).await;
    assert_eq!(
        (read["via"].clone(), read["relay"]["localModel"].clone()),
        (json!("mac"), json!("gpt-6-sol")),
        "nothing refused was saved"
    );

    // `null` and `""` clear, back to the loopback and no model for the Mac.
    let body = json!({ "kind": "local_proxy", "via": null, "relay": { "localModel": "" } });
    let (_, cleared) = h.set(&ada, body).await;
    assert_eq!(cleared["via"], "loopback", "{cleared}");
    assert_eq!(cleared["relay"]["localModel"], Value::Null, "{cleared}");
}

/// A RESUMED RUN GOES ON AT THE MAC. It parks on a card while its Mac answers it; its person
/// moves their setting to the loopback while the card waits; the answer carries it on at the
/// Mac all the same, on the model it started on, and it says where it asks only the once.
#[tokio::test]
async fn a_run_carried_on_after_a_card_keeps_asking_the_mac() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let path = format!("/coworkers/{coworker}/approvals");
    let body = Some(json!({ "tools": ["shell"] }));
    let (status, set) = h
        .send(Some(&ada.token), reqwest::Method::POST, &path, body)
        .await;
    assert_eq!(status, 200, "shell needs a person's yes: {set}");
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;

    let run = run_id();
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run,
        json!([user("m1", "list it")]),
        props,
    );
    let infer = mac.next().await;
    let id = infer["requestId"].as_str().unwrap();
    let call = a_shell_call("call_ls", "ls");
    assert_eq!(
        h.answer(&mac.token, id, "text/event-stream", sse(&call))
            .await
            .0,
        204
    );
    let (_, frames) = turn.await.unwrap();
    let card = frames
        .iter()
        .find(|frame| frame["name"] == "run-awaiting-approval")
        .cloned()
        .unwrap_or_else(|| panic!("the run parked on a card: {frames:?}"));

    let body = json!({ "kind": "local_proxy", "via": "loopback" });
    assert_eq!(h.set(&ada, body).await.0, 200);
    let answer = format!("/ag-ui/runs/{run}/answer");
    let body = Some(json!({ "call_id": card["callId"], "approved": true }));
    let (status, answered) = h
        .send(Some(&ada.token), reqwest::Method::POST, &answer, body)
        .await;
    assert_eq!(status, 200, "{answered}");
    let carried = mac.next().await;
    assert_eq!(
        carried["type"], "infer",
        "the carry-on asks the Mac: {carried}"
    );
    assert_eq!(carried["runId"], run.as_str());
    assert_eq!(carried["model"], "gpt-5.5");
    let id = carried["requestId"].as_str().unwrap();
    let done = words("listed on the mac");
    assert_eq!(
        h.answer(&mac.token, id, "text/event-stream", sse(&done))
            .await
            .0,
        204
    );
    let mut ended = None;
    for _ in 0..100 {
        let loaded = h.run(&run).await;
        if loaded.status.is_terminal() {
            ended = Some(loaded);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ended = ended.expect("the run ended in 10s");
    assert_eq!(ended.status, RunStatus::Finished, "{:?}", ended.failure);
    assert_eq!(ended.inference_via, Some(Via::Mac));
    assert_eq!(
        sources(&ended.emitted),
        [json!({"kind": "local_proxy", "via": "mac", "model": "gpt-5.5"})],
        "said once, at the start"
    );
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}

/// A COWORKER'S PIN GOES TO THE MAC ONLY ON ITS OWN PLAN. With the person's turns by their Mac, a
/// coworker whose own source is `local_proxy` asks the Mac for its pin and is told so first in its
/// system message; one that follows the person's setting (a default hire, pinned `xai/grok-4.6`)
/// asks the Mac for `relay.localModel`, whatever it is pinned to.
#[tokio::test]
async fn a_coworker_on_its_own_plan_asks_the_mac_for_its_pin_and_one_off_it_the_setting() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let own = h.hire(&ada, "Luna").await;
    let body = Some(json!({ "source": "local_proxy", "model": "gpt-6-luna" }));
    let patch = reqwest::Method::PATCH;
    let path = format!("/coworkers/{own}");
    let (status, row) = h.send(Some(&ada.token), patch, &path, body).await;
    assert_eq!(status, 200, "{row}");
    let off = h.hire(&ada, "Ada").await;

    for (coworker, model) in [(&own, "gpt-6-luna"), (&off, "gpt-5.5")] {
        let props = json!({ "coworkerId": coworker });
        let hello = json!([user("m1", "what do you run on?")]);
        let turn = h.turn(&ada, &unique("thr"), &run_id(), hello, props);
        let infer = mac.next().await;
        assert_eq!(infer["type"], "infer", "{infer}");
        assert_eq!(infer["model"], model, "{infer}");
        let told = format!("Your replies come from {model} on the person's own plan.");
        let system = infer["request"]["messages"][0]["content"].as_str();
        assert!(
            system.is_some_and(|said| said.starts_with(&told)),
            "{infer}"
        );
        let id = infer["requestId"].as_str().unwrap();
        let answered = sse(&words("from the mac"));
        let (status, _) = h
            .answer(&mac.token, id, "text/event-stream", answered)
            .await;
        assert_eq!(status, 204);
        let (_, frames) = turn.await.unwrap();
        let by_mac = json!({"kind": "local_proxy", "via": "mac", "model": model});
        assert_eq!(
            sources(&frames),
            std::slice::from_ref(&by_mac),
            "{frames:?}"
        );
    }
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}
