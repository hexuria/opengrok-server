//! Routines and webhooks against an absent Mac: skipped honestly, clocks moved on.

use super::*;

/// A DUE ROUTINE ON A PLAN BOT RUNS THROUGH THE PERSON'S MAC (#316): the clock's firing is a turn
/// by the Mac, on the Bot's pin, exactly as a live turn on that plan, and never the gateway.
#[tokio::test]
async fn a_due_routine_on_a_plan_bot_runs_through_the_mac() {
    let database_url = database_or_skip!();
    let _sweep = one_sweeper(&database_url).await;
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let mut mac = h.mac(&ada, "Ada's MacBook").await;
    let routine = h.plan_routine(&ada, "cron").await;

    assert_eq!(h.tick_when_due(&routine).await, 1, "it fired");
    let infer = mac.next().await;
    assert_eq!(infer["type"], "infer", "{infer}");
    assert_eq!(infer["model"], "gpt-6-luna", "on its pin, by the Mac");
    let id = infer["requestId"].as_str().unwrap();
    let answered = sse(&words("from the mac"));
    let (status, _) = h
        .answer(&mac.token, id, "text/event-stream", answered)
        .await;
    assert_eq!(status, 204);
    let mut last = Value::Null;
    for _ in 0..100 {
        last = h.routine_row(&ada, &routine).await["lastRun"].clone();
        if last["status"] == "finished" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        last["summary"], "Routine Weekly ran: from the mac",
        "{last}"
    );
    let history = h.history(&ada, &routine).await;
    assert_eq!(history[0]["cause"], "clock", "{history}");
    assert_eq!(history[0]["status"], "ok", "{history}");
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}

/// WITH NO MAC THE FIRING IS SKIPPED (#316), its computer's relay on but the computer asleep: no
/// run starts and nothing is asked anywhere; the routine's history and its row say so in the
/// owner's words, with the code; and its clock moves to its next slot, with nothing caught up after.
#[tokio::test]
async fn a_due_routine_with_no_mac_is_skipped_and_its_clock_moves_on() {
    let database_url = database_or_skip!();
    let _sweep = one_sweeper(&database_url).await;
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let _asleep = h.enrol(&ada, "Ada's MacBook").await;
    let routine = h.plan_routine(&ada, "cron").await;
    let before = now_ms();

    assert_eq!(h.tick_when_due(&routine).await, 0, "nothing fired");
    let history = h.history(&ada, &routine).await;
    let skipped = history.as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{history}");
    let at = skipped[0]["at"].as_i64().expect("at");
    assert!(at >= before, "{history}");
    let expected = json!({ "runId": null, "cause": "clock", "status": null, "startedAtMs": null,
        "endedAtMs": null, "at": at, "state": "skipped", "skipped": "relay_offline",
        "reason": SKIPPED, "by": null });
    assert_eq!(skipped[0], expected);
    let row = h.routine_row(&ada, &routine).await;
    let last = &row["lastRun"];
    let said = (
        &last["state"],
        &last["skipped"],
        &last["reason"],
        &last["summary"],
    );
    let skip = (json!("skipped"), json!("relay_offline"), json!(SKIPPED));
    assert_eq!(said, (&skip.0, &skip.1, &skip.2, &skip.2), "{last}");
    assert_eq!(last["runId"], Value::Null, "{last}");
    let next = row["nextDueMs"].as_i64().expect("its next slot");
    assert!(next > now_ms(), "the clock moved on: {row}");
    assert_eq!(h.runs_of(&ada, &routine).await, 0, "no run");
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));

    let again = opengrok_server::autonomy::sweep::schedule_tick(&h.host).await;
    assert_eq!(again.expect("a tick"), 0);
    let history = h.history(&ada, &routine).await;
    assert_eq!(history.as_array().unwrap().len(), 1, "nothing caught up");
}

/// "RUN NOW" WITH NO MAC is refused with the skip's words and code, as a 409, and the press is
/// recorded as a skip all the same. One call on each route, which the corpus keeps.
#[tokio::test]
async fn run_now_with_no_mac_answers_409_and_records_the_skip() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let _asleep = h.enrol(&ada, "Ada's MacBook").await;
    let routine = h.plan_routine(&ada, "cron").await;
    let path = format!("/schedules/{routine}/run");
    let post = reqwest::Method::POST;
    let (status, refused) = h.send(Some(&ada.token), post, &path, Some(json!({}))).await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        refused,
        json!({ "error": SKIPPED, "code": "relay_offline" })
    );
    let history = h.history(&ada, &routine).await;
    assert_eq!(history[0]["cause"], "manual", "{history}");
    assert_eq!(history[0]["skipped"], "relay_offline", "{history}");
    assert_eq!(h.runs_of(&ada, &routine).await, 0, "no run");
    assert_eq!((h.gateway.asked().len(), h.proxy.asked().len()), (0, 0));
}

/// A HOOK'S WAKE WITH NO MAC is skipped too: its caller is told why, and the routine's history
/// says the webhook asked.
#[tokio::test]
async fn a_webhook_for_a_plan_bot_with_no_mac_is_skipped() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.by_the_mac(&ada).await;
    let _asleep = h.enrol(&ada, "Ada's MacBook").await;
    let routine = h.plan_routine(&ada, "webhook").await;
    let row = h.routine_row(&ada, &routine).await;
    let url = row["webhook"]["url"].as_str().unwrap();
    let hook = url.rsplit('/').next().unwrap();
    let key = row["webhook"]["key"].as_str().unwrap();
    let res = h
        .client
        .post(format!("{}/hooks/{hook}", h.base))
        .bearer_auth(key)
        .send()
        .await
        .expect("a hook");
    assert_eq!(res.status().as_u16(), 409);
    let said: Value = res.json().await.expect("json");
    assert_eq!(said, json!({ "error": SKIPPED, "code": "relay_offline" }));
    let history = h.history(&ada, &routine).await;
    assert_eq!(history[0]["cause"], "webhook", "{history}");
    assert_eq!(h.runs_of(&ada, &routine).await, 0, "no run");
}
