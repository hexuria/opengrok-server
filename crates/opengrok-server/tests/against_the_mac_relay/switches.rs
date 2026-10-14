//! Computer switches: named per computer, refused while off, never relay-off for this server.

use super::*;

/// A COMPUTER'S ROW SAYS ITS OWN SWITCH AND WHETHER IT RELAYS NOW: every field the contract names,
/// `relayEnabled` on for a computer just enrolled, and `relaying` true while its relay stream is
/// open here and false for one asleep; `connected` is its command stream's, as before. One read,
/// which the corpus keeps.
#[tokio::test]
async fn a_computers_row_says_its_switch_and_whether_it_relays_now() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let mac = h.mac(&ada, "Ada's MacBook").await;
    let (asleep, _) = h.enrol(&ada, "Ada's iMac").await;
    let rows = h.computers(&ada).await;
    let row = |id: &str| rows.iter().find(|row| row["machineId"] == id).cloned();
    let awake = row(&mac.machine).expect("listed");
    let at = awake["enrolledAtMs"].as_i64().expect("enrolledAtMs");
    let expected = json!({ "machineId": mac.machine, "label": "Ada's MacBook", "enrolledAtMs": at,
        "revoked": false, "connected": false, "relayEnabled": true, "relaying": true });
    assert_eq!(awake, expected);
    let other = row(&asleep).expect("listed");
    let said = (&other["relayEnabled"], &other["relaying"]);
    assert_eq!(said, (&json!(true), &json!(false)), "{other}");
}

/// SWITCHED OFF, A COMPUTER IS TOLD SO AND ITS STREAM CLOSED: the PATCH answers its row, off and no
/// longer relaying; its stream is sent `disabled` and ends; and while it is off a stream it opens
/// is refused, 409 `relay_disabled`, before any frame. Switched on again, it opens as before. One
/// switch off, one refusal, which the corpus keeps.
#[tokio::test]
async fn a_computer_switched_off_is_told_disabled_and_refused_until_it_is_on_again() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let (status, row) = h
        .switch(&ada, &mac.machine, json!({ "relayEnabled": false }))
        .await;
    assert_eq!(status, 200, "{row}");
    let at = row["enrolledAtMs"].as_i64().expect("enrolledAtMs");
    let expected = json!({ "machineId": mac.machine, "label": "Ada's MacBook", "enrolledAtMs": at,
        "revoked": false, "connected": false, "relayEnabled": false, "relaying": false });
    assert_eq!(row, expected);
    let told = Harness::said_until_closed(&mut mac).await;
    assert_eq!(told, [json!({"type": "disabled"})], "told, then closed");

    let refused = h.refused_a_stream(&mac.token).await;
    let off = json!({ "error": RELAY_IS_OFF, "code": "relay_disabled" });
    assert_eq!(refused, (409, off));
    assert_eq!(h.computers(&ada).await[0]["relayEnabled"], false);

    let (status, row) = h
        .switch(&ada, &mac.machine, json!({ "relayEnabled": true }))
        .await;
    assert_eq!((status, &row["relayEnabled"]), (200, &json!(true)), "{row}");
    let _again = h.hold((mac.machine.clone(), mac.token.clone())).await;
    assert_eq!(h.computers(&ada).await[0]["relaying"], true);
}

/// A SWITCH NAMES ONLY THE CALLER'S OWN COMPUTER, AND IS REFUSED IN WORDS: another account's
/// machine is unknown, the same 404 as an id nobody enrolled, and stays on; a revoked one is a 409;
/// a body without a true or false `relayEnabled` is a 400. Nothing refused is switched. One call
/// for each refusal, which the corpus keeps.
#[tokio::test]
async fn a_switch_names_only_the_callers_own_computer_and_is_refused_in_words() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (ada, eve) = (h.person().await, h.person().await);
    let (theirs, _) = h.enrol(&eve, "Eve's MacBook").await;
    let off = || json!({ "relayEnabled": false });
    let unknown = json!({ "error": "no computer of yours has that id", "code": "not_found" });
    assert_eq!(h.switch(&ada, &theirs, off()).await, (404, unknown.clone()));
    assert_eq!(h.switch(&ada, "mac_nobody", off()).await, (404, unknown));
    assert_eq!(
        h.computers(&eve).await[0]["relayEnabled"],
        true,
        "Eve's own"
    );

    let (mine, _) = h.enrol(&ada, "Ada's MacBook").await;
    let (old, _) = h.enrol(&ada, "Ada's old Mac").await;
    let delete = reqwest::Method::DELETE;
    let revoke = format!("/local-exec/daemon/{old}");
    assert_eq!(h.send(Some(&ada.token), delete, &revoke, None).await.0, 204);
    let why = "this computer was revoked; enrol it again to use it";
    let revoked = json!({ "error": why, "code": "revoked" });
    assert_eq!(h.switch(&ada, &old, off()).await, (409, revoked));

    let why = "relayEnabled must be true or false";
    let not_a_switch = json!({ "error": why, "code": "bad_request" });
    let bodies = [
        json!({ "relayEnabled": "off" }),
        json!({}),
        json!({ "relayEnabled": null }),
    ];
    for body in bodies.into_iter().chain([json!([false])]) {
        let refused = h.switch(&ada, &mine, body.clone()).await;
        assert_eq!(refused, (400, not_a_switch.clone()), "{body}");
    }
    let path = format!("{}/local-exec/daemon/{mine}", h.base);
    let words = h.client.patch(path).bearer_auth(&ada.token).body("off");
    let res = words.send().await.expect("a body that is not JSON");
    assert_eq!(res.status().as_u16(), 400);
    assert_eq!(res.json::<Value>().await.expect("json"), not_a_switch);
    let rows = h.computers(&ada).await;
    let on = rows.iter().all(|row| row["relayEnabled"] == true);
    assert!(on, "nothing refused was switched: {rows:?}");
}

/// A COMPUTER SWITCHED OFF IS NEVER ASKED, though its stream is the newest and still open here, as
/// it is while another replica's switch closes nothing on this one: the next turn reads the
/// switches anew and goes to the one still on. Switched on again it is the newest, and the turn
/// after goes there. Nothing waits for a restart.
#[tokio::test]
async fn a_turn_skips_a_computer_switched_off_while_another_serves() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let mut older = h.mac(&ada, "Ada's iMac").await;
    let mut newer = h.mac(&ada, "Ada's MacBook").await;
    h.switched_elsewhere(&ada, &newer.machine, false).await;
    let said = h
        .carried(&ada, &coworker, &mut older, "from the iMac")
        .await;
    assert_eq!(said, "from the iMac");
    assert!(Harness::quiet(&mut newer).await, "never asked");
    let read = h.read(&ada).await;
    assert_eq!(read["relay"]["machineId"], older.machine.as_str(), "{read}");

    h.switched_elsewhere(&ada, &newer.machine, true).await;
    let said = h
        .carried(&ada, &coworker, &mut newer, "from the MacBook")
        .await;
    assert_eq!(said, "from the MacBook");
    assert!(Harness::quiet(&mut older).await);
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}

/// RELAY OFF IS EVERY COMPUTER OFF, AND ONE ASLEEP IS STILL ON: with the person's one computer on
/// but asleep, a turn by the Mac ends `relay_offline` and the fallback is never asked; switched
/// off, the next turn asks the fallback, its frame saying `fallbackFor: "relay_disabled"`.
#[tokio::test]
async fn every_computer_off_is_the_fallback_and_one_asleep_is_relay_offline() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    h.by_the_mac(&ada).await;
    let fallback = json!({ "kind": "local_proxy", "planFallback": { "model": "oag/cheap" } });
    assert_eq!(h.set(&ada, fallback).await.0, 200);
    let (asleep, _) = h.enrol(&ada, "Ada's MacBook").await;
    let props = json!({ "coworkerId": coworker });
    let hello = || json!([user("m1", "hello")]);
    let turn = h.turn(&ada, &unique("thr"), &run_id(), hello(), props.clone());
    let (_, frames) = turn.await.unwrap();
    assert_eq!(ending(&frames)["code"], "relay_offline", "{frames:?}");
    let waits = "the fallback waits while a computer is on";
    assert!(h.gateway.asked().is_empty(), "{waits}");

    let (status, _) = h
        .switch(&ada, &asleep, json!({ "relayEnabled": false }))
        .await;
    assert_eq!(status, 200);
    let turn = h.turn(&ada, &unique("thr"), &run_id(), hello(), props);
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "from the gateway", "{frames:?}");
    let said = json!({ "kind": "gateway", "model": "oag/cheap", "fallbackFor": "relay_disabled" });
    assert_eq!(sources(&frames), [said]);
}

/// THE ACCOUNT'S SWITCH IS ITS COMPUTERS': `relayEnabled` reads on while any un-revoked one is on,
/// and off with none, none enrolled included; a computer enrolled while it is off is enrolled off,
/// so a new computer never turns it back on (review of #342); a PUT of it, as an app from before
/// each computer had its own sends it, switches every one, and one switched off with its stream
/// open is told so.
#[tokio::test]
async fn the_accounts_switch_reads_its_computers_and_a_put_switches_every_one() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let relay = |read: Value| read["relayEnabled"].clone();
    assert_eq!(relay(h.read(&ada).await), false, "no computer, no relay");
    let (imac, imac_token) = h.enrol(&ada, "Ada's iMac").await;
    assert_eq!(relay(h.read(&ada).await), true);
    assert_eq!(
        h.switch(&ada, &imac, json!({ "relayEnabled": false }))
            .await
            .0,
        200
    );
    assert_eq!(relay(h.read(&ada).await), false, "its one computer off");
    let (old, _) = h.enrol(&ada, "Ada's old Mac").await;
    assert_eq!(relay(h.read(&ada).await), false, "a new one enrolled off");
    let switched = h.switch(&ada, &old, json!({ "relayEnabled": true }));
    assert_eq!(switched.await.0, 200);
    assert_eq!(relay(h.read(&ada).await), true, "any one on");
    let revoke = format!("/local-exec/daemon/{old}");
    let delete = reqwest::Method::DELETE;
    assert_eq!(h.send(Some(&ada.token), delete, &revoke, None).await.0, 204);
    assert_eq!(
        relay(h.read(&ada).await),
        false,
        "a revoked one is no computer"
    );

    let on = json!({ "kind": "local_proxy", "relayEnabled": true });
    let (status, saved) = h.set(&ada, on).await;
    assert_eq!((status, relay(saved)), (200, json!(true)));
    let rows = h.computers(&ada).await;
    assert!(
        rows.iter().all(|row| row["relayEnabled"] == true),
        "{rows:?}"
    );
    let mut imac = h.hold((imac, imac_token)).await;
    let off = json!({ "kind": "local_proxy", "relayEnabled": false });
    let (status, saved) = h.set(&ada, off).await;
    assert_eq!((status, relay(saved)), (200, json!(false)));
    let rows = h.computers(&ada).await;
    assert!(
        rows.iter().all(|row| row["relayEnabled"] == false),
        "{rows:?}"
    );
    let told = Harness::said_until_closed(&mut imac).await;
    assert_eq!(told, [json!({"type": "disabled"})]);
}

/// A PLAN ON THIS SERVER'S OWN MACHINE IS NEVER "RELAY OFF": with no computer enrolled the account
/// reads its relay off, yet its turns go to its proxy by the loopback though a fallback is set, and
/// so does its plan Bot's routine. The switch is the Mac's way's alone.
#[tokio::test]
async fn a_plan_on_this_servers_own_machine_is_never_relay_off() {
    let database_url = database_or_skip!();
    let _sweep = one_sweeper(&database_url).await;
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada, "Ada").await;
    let body = json!({ "kind": "local_proxy", "via": "loopback", "baseUrl": h.proxy_url,
                       "localModel": "gpt-5.5", "planFallback": { "model": "oag/cheap" } });
    let (status, saved) = h.set(&ada, body).await;
    assert_eq!(
        (status, &saved["relayEnabled"]),
        (200, &json!(false)),
        "{saved}"
    );
    let props = json!({ "coworkerId": coworker });
    let turn = h.turn(
        &ada,
        &unique("thr"),
        &run_id(),
        json!([user("m1", "hi")]),
        props,
    );
    let (_, frames) = turn.await.unwrap();
    assert_eq!(text_of(&frames), "from the loopback", "{frames:?}");
    let way = json!({ "kind": "local_proxy", "via": "loopback", "model": "gpt-5.5" });
    assert_eq!(sources(&frames), [way]);

    let routine = h.plan_routine(&ada, "cron").await;
    assert_eq!(h.tick_when_due(&routine).await, 1, "it fired");
    let mut last = Value::Null;
    for _ in 0..100 {
        last = h.routine_row(&ada, &routine).await["lastRun"].clone();
        if last["status"] == "finished" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ran = "Routine Weekly ran: from the loopback";
    assert_eq!(last["summary"], ran, "{last}");
    let asked = h.proxy.asked();
    let models: Vec<&Value> = asked.iter().map(|asked| &asked["model"]).collect();
    assert_eq!(
        models,
        [&json!("gpt-5.5"), &json!("gpt-6-luna")],
        "the loopback both times"
    );
    assert!(h.gateway.asked().is_empty(), "never the fallback");
}
