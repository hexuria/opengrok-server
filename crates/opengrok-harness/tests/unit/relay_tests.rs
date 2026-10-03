use super::*;

use crate::model::{ChatMessage, ModelDelta};

/// The frames a stream sends, parsed, one per call: each on one line, as an SSE field must be.
async fn any_frame(stream: &mut (impl Stream<Item = String> + Unpin)) -> Option<Value> {
    let json = stream.next().await?;
    assert!(!json.contains('\n'), "{json:?}");
    Some(serde_json::from_str(&json).unwrap())
}

/// The next frame that is not a ping: the clocks here are quick, so pings fall anywhere.
async fn frame(stream: &mut (impl Stream<Item = String> + Unpin)) -> Option<Value> {
    loop {
        let said = any_frame(stream).await?;
        if said["type"] != "ping" {
            return Some(said);
        }
    }
}

fn quick() -> Arc<RelayBroker> {
    Arc::new(RelayBroker::new(Clocks {
        first_byte: Duration::from_millis(300),
        idle: Duration::from_millis(300),
        listing: Duration::from_millis(300),
        ping: Duration::from_millis(100),
    }))
}

fn turn(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        messages: vec![ChatMessage::text("user", "hello")],
        ..ModelRequest::default()
    }
}

fn to(broker: &Arc<RelayBroker>, account: &str) -> RelayTo {
    RelayTo {
        broker: broker.clone(),
        account: account.to_string(),
        run_id: "run-1".to_string(),
    }
}

async fn words(stream: DeltaStream) -> Result<String, ModelError> {
    let mut stream = stream;
    let mut text = String::new();
    while let Some(delta) = stream.next().await {
        if let ModelDelta::Text(piece) = delta? {
            text.push_str(&piece);
        }
    }
    Ok(text)
}

#[tokio::test]
async fn a_stream_opens_with_ready_pings_and_is_replaced_by_the_next_from_its_machine() {
    let broker = quick();
    let mut first = Box::pin(broker.connect("acct-a", "mac-1"));
    assert_eq!(
        frame(&mut first).await,
        Some(serde_json::json!({"type": "ready", "machineId": "mac-1"}))
    );
    assert_eq!(any_frame(&mut first).await.unwrap()["type"], "ping");
    let mut second = Box::pin(broker.connect("acct-a", "mac-1"));
    assert_eq!(frame(&mut second).await.unwrap()["type"], "ready");
    let mut rest = Vec::new();
    while let Some(said) = frame(&mut first).await {
        rest.push(said["type"].as_str().unwrap().to_string());
    }
    assert_eq!(
        rest.last().map(String::as_str),
        Some("replaced"),
        "{rest:?}"
    );
    assert_eq!(broker.connected("acct-a").as_deref(), Some("mac-1"));
    assert_eq!(broker.connected("acct-b"), None);
}

/// A call goes to the account's newest stream, as the body the loopback door would POST, and no
/// frame carries an address or a key. Its answer is piped into the call that asked, verbatim.
#[tokio::test]
async fn a_call_goes_to_the_accounts_newest_mac_and_its_answer_back_to_the_call() {
    let broker = quick();
    let mut older = Box::pin(broker.connect("acct-a", "mac-old"));
    let mut newer = Box::pin(broker.connect("acct-a", "mac-new"));
    let mut other = Box::pin(broker.connect("acct-b", "mac-b"));
    for stream in [&mut older, &mut newer, &mut other] {
        frame(stream).await;
    }
    let asked = tokio::spawn({
        let broker = broker.clone();
        async move {
            words(
                broker
                    .stream(&to(&broker, "acct-a"), &turn("gpt-5.5"))
                    .await?,
            )
            .await
        }
    });
    let infer = frame(&mut newer).await.unwrap();
    assert_eq!(infer["type"], "infer");
    assert_eq!(infer["runId"], "run-1");
    assert_eq!(
        infer["request"],
        crate::gateway::chat_body(&turn("gpt-5.5"))
    );
    assert_eq!(infer["request"]["stream"], true);
    let said = infer.to_string();
    assert!(!said.contains("http") && !said.contains("apiKey"), "{said}");
    let id = infer["requestId"].as_str().unwrap().to_string();
    assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");

    let answer = |account: &str, machine: &str, id: &str| broker.answer(account, machine, id).err();
    assert_eq!(answer("acct-a", "mac-old", &id), Some(Refused::NotYours));
    assert_eq!(answer("acct-b", "mac-b", &id), Some(Refused::NotYours));
    // The same machine id enrolled to another account is another machine.
    assert_eq!(answer("acct-b", "mac-new", &id), Some(Refused::NotYours));
    assert_eq!(
        answer("acct-a", "mac-new", "no-such-id"),
        Some(Refused::Unknown)
    );
    let answering = broker.answer("acct-a", "mac-new", &id).unwrap();
    assert_eq!(answer("acct-a", "mac-new", &id), Some(Refused::Answered));
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"from the \"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"mac\"}}]}\n\ndata: [DONE]\n\n";
    let halves: Vec<Result<&[u8], std::io::Error>> =
        vec![Ok(&sse.as_bytes()[..30]), Ok(&sse.as_bytes()[30..])];
    assert_eq!(
        answering.pipe(true, futures::stream::iter(halves)).await,
        Piped::Accepted
    );
    assert_eq!(asked.await.unwrap().unwrap(), "from the mac");
    let answer = |account: &str, machine: &str, id: &str| broker.answer(account, machine, id).err();
    assert_eq!(
        answer("acct-a", "mac-new", &id),
        Some(Refused::Answered),
        "still once"
    );
    assert_eq!(answer("acct-a", "mac-old", &id), Some(Refused::NotYours));
}

/// A MAC THAT DOES NOT ANSWER IS TOLD TO CANCEL: no first byte, and a quiet stream, are each
/// `relay_timeout`. Its own `{error}` is `relay_failed`, in its words; no Mac is `relay_offline`.
#[tokio::test]
async fn silence_times_out_and_cancels_and_a_macs_error_reaches_the_run_in_its_words() {
    let broker = quick();
    let offline = broker
        .stream(&to(&broker, "acct-a"), &turn("gpt-5.5"))
        .await;
    assert_eq!(offline.err().and_then(|e| e.code()), Some("relay_offline"));

    let mut mac = Box::pin(broker.connect("acct-a", "mac-1"));
    frame(&mut mac).await;
    let silent = broker
        .stream(&to(&broker, "acct-a"), &turn("gpt-5.5"))
        .await;
    let error = silent.err().unwrap();
    assert_eq!(error.code(), Some("relay_timeout"));
    assert!(
        error.sentence().contains("did not start answering"),
        "{error}"
    );
    let infer = frame(&mut mac).await.unwrap();
    let cancel = frame(&mut mac).await.unwrap();
    assert_eq!(cancel["type"], "cancel");
    assert_eq!(cancel["requestId"], infer["requestId"]);
    let late = infer["requestId"].as_str().unwrap();
    let expired = broker.answer("acct-a", "mac-1", late).err();
    assert_eq!(expired, Some(Refused::Unknown), "expired");

    // Mid-stream: a first line, then nothing.
    let asked = tokio::spawn({
        let broker = broker.clone();
        async move {
            words(
                broker
                    .stream(&to(&broker, "acct-a"), &turn("gpt-5.5"))
                    .await?,
            )
            .await
        }
    });
    let infer = frame(&mut mac).await.unwrap();
    let answering = broker
        .answer("acct-a", "mac-1", infer["requestId"].as_str().unwrap())
        .unwrap();
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::io::Error>>();
    let piping = tokio::spawn(answering.pipe(true, rx));
    let line = b"data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n".to_vec();
    tx.unbounded_send(Ok(line)).unwrap();
    let error = asked.await.unwrap().unwrap_err();
    assert_eq!(error.code(), Some("relay_timeout"));
    assert!(error.sentence().contains("stopped answering"), "{error}");
    assert_eq!(frame(&mut mac).await.unwrap()["type"], "cancel");
    drop(tx);
    assert_eq!(piping.await.unwrap(), Piped::Accepted);

    let asked = tokio::spawn({
        let broker = broker.clone();
        async move {
            broker
                .stream(&to(&broker, "acct-a"), &turn("gpt-5.5"))
                .await
                .map(|_| ())
        }
    });
    let infer = frame(&mut mac).await.unwrap();
    let answering = broker
        .answer("acct-a", "mac-1", infer["requestId"].as_str().unwrap())
        .unwrap();
    let body = br#"{"error": "The usage limit was reached for your plan."}"#;
    let pieces = futures::stream::iter([Ok::<_, std::io::Error>(&body[..])]);
    answering.pipe(false, pieces).await;
    let error = asked.await.unwrap().unwrap_err();
    assert_eq!(error.code(), Some("relay_failed"));
    assert_eq!(
        error.sentence(),
        "The usage limit was reached for your plan."
    );
}

/// A STOP CANCELS THE CALL AT THE MAC AND ENDS ITS STREAM where it is, once.
#[tokio::test]
async fn a_stop_cancels_the_call_at_the_mac_once() {
    let broker = quick();
    let mut mac = Box::pin(broker.connect("acct-a", "mac-1"));
    frame(&mut mac).await;
    let asked = tokio::spawn({
        let broker = broker.clone();
        async move {
            words(
                broker
                    .stream(&to(&broker, "acct-a"), &turn("gpt-5.5"))
                    .await?,
            )
            .await
        }
    });
    let infer = frame(&mut mac).await.unwrap();
    broker.stop("run-1");
    assert_eq!(asked.await.unwrap().unwrap(), "", "ended where it was");
    let cancel = frame(&mut mac).await.unwrap();
    assert_eq!(
        (cancel["type"].clone(), cancel["requestId"].clone()),
        ("cancel".into(), infer["requestId"].clone())
    );
    assert_eq!(
        any_frame(&mut mac).await.unwrap()["type"],
        "ping",
        "and only once"
    );
}

/// A model list is the Mac's, kept to what a subscription may run; no Mac lists nothing.
#[tokio::test]
async fn a_macs_model_list_is_kept_to_the_allowlist() {
    let broker = quick();
    assert!(broker.models("acct-a").await.is_empty());
    let mut mac = Box::pin(broker.connect("acct-a", "mac-1"));
    frame(&mut mac).await;
    let listing = tokio::spawn({
        let broker = broker.clone();
        async move { broker.models("acct-a").await }
    });
    let asked = frame(&mut mac).await.unwrap();
    assert_eq!(asked["type"], "models");
    let answering = broker
        .answer("acct-a", "mac-1", asked["requestId"].as_str().unwrap())
        .unwrap();
    let body =
        br#"{"data": [{"id": "gpt-6-sol"}, {"id": "claude-sonnet-4.5"}, {"id": "xai/grok-4.7"}]}"#;
    answering
        .pipe(
            false,
            futures::stream::iter([Ok::<_, std::io::Error>(&body[..])]),
        )
        .await;
    let listed = listing.await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["gpt-6-sol", "xai/grok-4.7"]);
}

/// ONE SENDING OF A THREAD'S HELD SENDS AT A TIME, AND NO TRIGGER LOST TO IT: a second claim
/// while one runs is refused and has the running one go round again, once; threads and accounts
/// are claimed apart.
#[test]
fn a_threads_held_sends_are_sent_by_one_claim_that_goes_round_again_for_a_trigger() {
    let broker = RelayBroker::default();
    assert!(broker.draining("acct-a", "thr-1"));
    assert!(broker.draining("acct-a", "thr-2"));
    assert!(broker.draining("acct-b", "thr-1"));
    assert!(!broker.draining("acct-a", "thr-1"), "one is running");
    assert!(!broker.draining("acct-a", "thr-1"), "and still is");
    assert!(
        broker.drained("acct-a", "thr-1"),
        "a trigger came: round again"
    );
    assert!(!broker.drained("acct-a", "thr-1"), "none since: let go");
    assert!(broker.draining("acct-a", "thr-1"), "claimable again");
    assert!(!broker.drained("acct-a", "thr-2"));
}
