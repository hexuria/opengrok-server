//! The relay-off fallback: saved, named, asked only when the relay is off.

use super::*;

/// THE SWITCH AND ITS FALLBACK, SAVED IN ONE BODY (#332) beside the way the app means, and read
/// back as saved: the relay off, its computer's with it, the Mac still the way, the fallback's
/// model and effort. One save and one read, which the corpus keeps.
#[tokio::test]
async fn a_person_switches_the_relay_off_and_names_a_fallback_in_one_save() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let _asleep = h.enrol(&ada, "Ada's MacBook").await;
    let fallback = json!({ "model": "xai/grok-4.6", "effort": "low" });
    let body = json!({ "kind": "local_proxy", "via": "mac", "relay": { "localModel": "gpt-5.5" },
                       "relayEnabled": false, "planFallback": fallback });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(status, 200, "{saved}");
    let said = (
        &saved["relayEnabled"],
        &saved["planFallback"],
        &saved["via"],
    );
    assert_eq!(said, (&json!(false), &fallback, &json!("mac")), "{saved}");
    assert_eq!(saved["relay"]["localModel"], "gpt-5.5", "{saved}");
    assert_eq!(h.read(&ada).await, saved, "read back as saved");
}

/// THE SWITCH MOVES NO WAY (#332): off and on again, the Mac stays the way, which only a `via`
/// changes; an account with no computer reads the relay off, and one enrolled reads it on, with
/// no fallback. A fallback left out is kept, null clears it, and a body refused in any part saves
/// none of itself.
#[tokio::test]
async fn the_relay_switch_moves_no_way_and_a_fallback_is_kept_until_cleared() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let fresh = h.read(&ada).await;
    let unset = (&fresh["relayEnabled"], &fresh["planFallback"]);
    assert_eq!(unset, (&json!(false), &Value::Null), "{fresh}");
    let _asleep = h.enrol(&ada, "Ada's MacBook").await;
    let enrolled = h.read(&ada).await;
    assert_eq!(enrolled["relayEnabled"], true, "{enrolled}");
    h.by_the_mac(&ada).await;
    h.relay_off(&ada, Value::Null).await;
    let fallback = json!({ "model": "oag/cheap", "effort": "high" });
    let (_, named) = h
        .set(
            &ada,
            json!({ "kind": "local_proxy", "planFallback": fallback }),
        )
        .await;
    let kept = (&named["relayEnabled"], &named["planFallback"]);
    assert_eq!(
        kept,
        (&json!(false), &fallback),
        "left out, the switch is kept"
    );
    let (_, on) = h
        .set(&ada, json!({ "kind": "local_proxy", "relayEnabled": true }))
        .await;
    let on = (&on["relayEnabled"], &on["via"], &on["planFallback"]);
    assert_eq!(on, (&json!(true), &json!("mac"), &fallback), "no way moved");

    let effort =
        "planFallback.effort must be one of inherit, none, low, medium, high, xhigh, max, ultra";
    for (body, why) in [
        (
            json!({ "kind": "local_proxy", "relayEnabled": "off" }),
            "relayEnabled must be true or false",
        ),
        (
            json!({ "kind": "local_proxy", "relayEnabled": false, "planFallback": { "model": " " } }),
            "planFallback.model: a coworker needs a model to think with",
        ),
        (
            json!({ "kind": "local_proxy", "planFallback": { "model": "m", "effort": "loud" } }),
            effort,
        ),
        (
            json!({ "kind": "local_proxy", "planFallback": "xai/grok-4.6" }),
            "planFallback must be an object or null",
        ),
    ] {
        let (status, refused) = h.set(&ada, body.clone()).await;
        assert_eq!((status, &refused["error"]), (400, &json!(why)), "{body}");
    }
    let still = h.read(&ada).await;
    let still = (&still["relayEnabled"], &still["planFallback"]);
    assert_eq!(
        still,
        (&json!(true), &fallback),
        "nothing refused was saved"
    );
    let (_, cleared) = h
        .set(&ada, json!({ "kind": "local_proxy", "planFallback": null }))
        .await;
    assert_eq!(cleared["planFallback"], Value::Null, "{cleared}");
}

/// RELAY OFF, A FALLBACK SET (#332): a turn that would go by the person's Mac asks the gateway on
/// the fallback's model and effort instead, never the Mac, connected or not, nor the loopback;
/// its frame says why. The run keeps where it asked, and the Bot keeps its own pin and door. One
/// replay, which the corpus keeps.
#[tokio::test]
async fn a_turn_by_the_mac_with_the_relay_off_asks_the_fallback_and_says_so() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    h.relay_off(&ada, json!({ "model": "oag/cheap", "effort": "low" }))
        .await;
    let (thread, run) = (unique("thr"), run_id());
    let props = json!({ "coworkerId": coworker });
    let (status, frames) = h
        .turn(&ada, &thread, &run, json!([user("m1", "hello")]), props)
        .await
        .unwrap();
    assert_eq!(
        (status, text_of(&frames).as_str()),
        (200, "from the gateway")
    );
    let said = json!({ "kind": "gateway", "model": "oag/cheap", "fallbackFor": "relay_disabled" });
    assert_eq!(sources(&frames), vec![said.clone()]);
    let asked = h.gateway.asked();
    let asked = (
        asked.len(),
        &asked[0]["model"],
        &asked[0]["reasoning_effort"],
    );
    assert_eq!(asked, (1, &json!("oag/cheap"), &json!("low")));
    assert!(h.proxy.asked().is_empty(), "never the loopback");
    let told = Harness::said_until_closed(&mut mac).await;
    let off = [json!({"type": "disabled"})];
    assert_eq!(told, off, "switched off with the rest, and never asked");

    let started = h.run(&run).await;
    let kept = (
        started.model.as_deref(),
        started.inference_source,
        started.effort,
    );
    use opengrok_core::coworker::Effort;
    assert_eq!(kept, (Some("oag/cheap"), SourceKind::Gateway, Effort::Low));
    let id = opengrok_core::id::CoworkerId::from_stored(coworker.clone());
    let (bot, _) = h.store.load_coworker(&id).await.expect("the Bot");
    assert_eq!(
        (bot.model.as_str(), bot.source),
        ("xai/grok-4.6", None),
        "untouched"
    );
    let replay = h.settled(&ada, &thread, 1).await;
    let events = replay["runs"][0]["events"].as_array().unwrap();
    assert_eq!(sources(events), vec![said], "the replay says it too");
}

/// RELAY OFF AND NO FALLBACK (#332): the turn is refused in words, `plan_unavailable`, before any
/// model is asked anywhere; its frame names the way and the Mac's model it did not ask.
#[tokio::test]
async fn a_turn_by_the_mac_with_the_relay_off_and_no_fallback_is_refused_in_words() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    h.relay_off(&ada, Value::Null).await;
    let props = json!({ "coworkerId": coworker });
    let (status, frames) = h
        .turn(
            &ada,
            &unique("thr"),
            &run_id(),
            json!([user("m1", "hello")]),
            props,
        )
        .await
        .unwrap();
    assert_eq!(status, 200);
    let end = ending(&frames);
    let refused = (&end["type"], &end["code"], &end["message"]);
    let expected = (
        &json!("RUN_ERROR"),
        &json!("plan_unavailable"),
        &json!(RELAY_OFF),
    );
    assert_eq!(refused, expected, "{end}");
    let way = json!({ "kind": "local_proxy", "via": "mac", "model": "gpt-5.5" });
    assert_eq!(sources(&frames), vec![way]);
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
    let told = Harness::said_until_closed(&mut mac).await;
    let off = [json!({"type": "disabled"})];
    assert_eq!(told, off, "switched off with the rest, and never asked");
}

/// RELAY OFF, A FALLBACK SET: A PLAN BOT'S ROUTINE RUNS ON IT (#332), on the gateway at the
/// fallback's model and effort, and the history says it ran; the Bot's pin and door are untouched.
#[tokio::test]
async fn a_plan_bots_routine_runs_on_the_fallback_while_the_relay_is_off() {
    let database_url = database_or_skip!();
    let _sweep = one_sweeper(&database_url).await;
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let routine = h.plan_routine(&ada, "cron").await;
    h.relay_off(&ada, json!({ "model": "oag/cheap", "effort": "high" }))
        .await;

    assert_eq!(h.tick_when_due(&routine).await, 1, "it fired");
    let mut last = Value::Null;
    for _ in 0..100 {
        last = h.routine_row(&ada, &routine).await["lastRun"].clone();
        if last["status"] == "finished" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        last["summary"], "Routine Weekly ran: from the gateway",
        "{last}"
    );
    let asked = h.gateway.asked();
    let asked = (
        asked.len(),
        &asked[0]["model"],
        &asked[0]["reasoning_effort"],
    );
    assert_eq!(asked, (1, &json!("oag/cheap"), &json!("high")));
    assert!(h.proxy.asked().is_empty());
    let history = h.history(&ada, &routine).await;
    assert_eq!(
        (&history[0]["cause"], &history[0]["status"]),
        (&json!("clock"), &json!("ok"))
    );
    let bot = h.routine_bot(&ada, &routine).await;
    let own = (bot.model.as_str(), bot.source);
    assert_eq!(
        own,
        ("gpt-6-luna", Some(SourceKind::LocalProxy)),
        "untouched"
    );
}

/// RELAY OFF AND NO FALLBACK: A PLAN BOT'S ROUTINE IS SKIPPED (#332), by the clock and by "run
/// now" alike, though its Mac is connected: no run, nothing asked, and its history says so in
/// the contract's words. One history read, which the corpus keeps.
#[tokio::test]
async fn a_plan_bots_routine_is_skipped_while_the_relay_is_off_with_no_fallback() {
    let database_url = database_or_skip!();
    let _sweep = one_sweeper(&database_url).await;
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let routine = h.plan_routine(&ada, "cron").await;
    h.relay_off(&ada, Value::Null).await;
    let skipped = "Skipped: Relay is off for your plan";

    assert_eq!(h.tick_when_due(&routine).await, 0, "nothing fired");
    let path = format!("/schedules/{routine}/run");
    let post = reqwest::Method::POST;
    let (status, refused) = h.send(Some(&ada.token), post, &path, Some(json!({}))).await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        refused,
        json!({ "error": skipped, "code": "relay_disabled" })
    );
    let history = h.history(&ada, &routine).await;
    let rows = history.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{history}");
    for (row, cause) in rows.iter().zip(["manual", "clock"]) {
        let at = row["at"].as_i64().expect("at");
        let expected = json!({ "runId": null, "cause": cause, "status": null,
            "startedAtMs": null, "endedAtMs": null, "at": at, "state": "skipped",
            "skipped": "relay_disabled", "reason": skipped, "by": null });
        assert_eq!(row, &expected);
    }
    assert_eq!(h.runs_of(&ada, &routine).await, 0, "no run");
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
    let told = Harness::said_until_closed(&mut mac).await;
    let off = [json!({"type": "disabled"})];
    assert_eq!(told, off, "switched off with the rest, and never asked");
}

/// RELAY ON, THE COMPUTER AWAY: NOTHING CHANGES (#332). A fallback named for when the relay is off
/// is never asked while a computer's relay is on and it is asleep: the routine's firing is skipped
/// `relay_offline`, and a turn ends `relay_offline`, as before.
#[tokio::test]
async fn with_the_relay_on_and_the_computer_away_the_fallback_is_never_asked() {
    let database_url = database_or_skip!();
    let _sweep = one_sweeper(&database_url).await;
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let _asleep = h.enrol(&ada, "Ada's MacBook").await;
    let fallback = json!({ "kind": "local_proxy", "planFallback": { "model": "oag/cheap" } });
    let (status, saved) = h.set(&ada, fallback).await;
    assert_eq!(
        (status, &saved["relayEnabled"]),
        (200, &json!(true)),
        "{saved}"
    );
    let routine = h.plan_routine(&ada, "cron").await;

    assert_eq!(h.tick_when_due(&routine).await, 0, "skipped");
    let history = h.history(&ada, &routine).await;
    assert_eq!(history[0]["skipped"], "relay_offline", "{history}");
    let props = json!({ "coworkerId": coworker });
    let (status, frames) = h
        .turn(
            &ada,
            &unique("thr"),
            &run_id(),
            json!([user("m1", "hello")]),
            props,
        )
        .await
        .unwrap();
    assert_eq!(
        (status, &ending(&frames)["code"]),
        (200, &json!("relay_offline"))
    );
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}

/// RELAY OFF, NOTHING WAITS FOR THE MAC (#332): a queued send that would go by it is not held,
/// and fired, it runs on the fallback at once.
#[tokio::test]
async fn with_the_relay_off_a_queued_send_is_not_held_for_the_mac() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let fallback = json!({ "model": "oag/cheap", "effort": "inherit" });
    h.relay_off(&ada, fallback).await;
    let thread = unique("thr");
    let first = json!([user("m0", "first")]);
    let props = json!({ "coworkerId": coworker, "inferenceSource": "gateway" });
    let (status, _) = h
        .turn(&ada, &thread, &run_id(), first, props)
        .await
        .unwrap();
    assert_eq!(status, 200, "a thread to queue on");
    let path = format!("/ag-ui/threads/{thread}/pending");
    let body = json!({ "v": 1, "content": "by my Mac", "clientMessageId": "m1",
                       "inferenceSource": { "kind": "local_proxy", "via": "mac" } });
    let post = reqwest::Method::POST;
    let (status, created) = h.send(Some(&ada.token), post, &path, Some(body)).await;
    assert_eq!(status, 201, "{created}");
    let pending_id = created["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, listed) = h
        .send(Some(&ada.token), reqwest::Method::GET, &path, None)
        .await;
    let entry = &listed["pendingUserMessages"][0];
    assert_eq!(entry["id"], pending_id.as_str(), "{listed}");
    assert!(entry.get("heldFor").is_none(), "not held: {listed}");

    let props = json!({ "coworkerId": coworker, "pendingId": pending_id });
    let (status, frames) = h
        .turn(
            &ada,
            &thread,
            &run_id(),
            json!([user("m0", "first"), user("m1", "by my Mac")]),
            props,
        )
        .await
        .unwrap();
    assert_eq!(
        (status, text_of(&frames).as_str()),
        (200, "from the gateway")
    );
    let said = json!({ "kind": "gateway", "model": "oag/cheap", "fallbackFor": "relay_disabled" });
    assert_eq!(sources(&frames), vec![said]);
}

/// AFTER SKIPS, THE NORMAL NEXT TICK (#332): each skip moves the clock to the routine's next slot
/// exactly as a firing would, never to a retry; and with the Mac back nothing missed is caught up:
/// a tick fires nothing until that slot comes.
#[tokio::test]
async fn after_relay_offline_skips_the_next_due_is_the_normal_next_tick() {
    use opengrok_core::schedule::next_fire_ms;
    let database_url = database_or_skip!();
    let _sweep = one_sweeper(&database_url).await;
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let asleep = h.enrol(&ada, "Ada's MacBook").await;
    let routine = h.plan_routine(&ada, "cron").await;
    for skips in 1..=2 {
        let before = now_ms();
        assert_eq!(h.tick_when_due(&routine).await, 0, "skipped");
        let row = h.routine_row(&ada, &routine).await;
        let next = row["nextDueMs"].as_i64().expect("its next slot");
        let normal = [before, now_ms()].map(|from| next_fire_ms("0 9 * * 1", "UTC", from));
        assert!(normal.contains(&Some(next)), "the normal next tick: {row}");
        let history = h.history(&ada, &routine).await;
        assert_eq!(history.as_array().unwrap().len(), skips, "{history}");
    }
    let _mac = h.hold(asleep).await;
    let tick = opengrok_server::autonomy::sweep::schedule_tick(&h.host).await;
    assert_eq!(tick.expect("a tick"), 0, "nothing missed is caught up");
    assert_eq!(h.runs_of(&ada, &routine).await, 0);
    let history = h.history(&ada, &routine).await;
    assert_eq!(history.as_array().unwrap().len(), 2, "{history}");
}
