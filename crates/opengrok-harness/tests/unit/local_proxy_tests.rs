use super::*;

use std::sync::{Arc, Mutex};

use futures::StreamExt;

use crate::model::{ModelDelta, ModelDoor, ModelError, ModelRequest};

/// Every form the owner's machine can be named by is accepted, and each comes back as the one
/// spelling the door will dial.
#[test]
fn a_loopback_address_is_accepted_and_written_back_as_it_will_be_dialled() {
    for (given, dialled) in [
        ("http://127.0.0.1", "http://127.0.0.1"),
        ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
        ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
        ("https://127.5.5.5:9443", "https://127.5.5.5:9443"),
        ("http://127.5.5.5", "http://127.5.5.5"),
        ("http://[::1]", "http://[::1]"),
        ("http://[::1]:8080/", "http://[::1]:8080"),
        ("http://localhost", "http://localhost"),
        ("http://localhost:8080", "http://localhost:8080"),
        ("  HTTP://LOCALHOST:8080/  ", "http://localhost:8080"),
        // The parser the client dials with reads these as 127.0.0.1, so they are this machine;
        // what is stored is the spelling it dials.
        ("http://0x7f.1:8080", "http://127.0.0.1:8080"),
        ("http://2130706433", "http://127.0.0.1"),
    ] {
        assert_eq!(loopback_base(given).as_deref(), Ok(dialled), "{given}");
    }
}

/// Anything that is not this machine is refused, with a sentence: private and link-local
/// addresses, public names, a name that only starts with `localhost`, credentials in the
/// address, other schemes, and the octal spelling a looser eye reads as 127.
#[test]
fn anything_but_this_machine_is_refused_with_a_sentence() {
    for refused in [
        "http://10.0.0.5:8080",
        "http://10.1.2.3",
        "http://169.254.169.254/",
        "http://169.254.169.254:80",
        "http://192.168.1.10:8080",
        "http://example.com",
        "https://example.com:8080",
        "http://localhost.evil.com",
        "http://localhost.evil.com:8080",
        "http://127.0.0.1.nip.io:8080",
        "http://a@127.0.0.1",
        "http://a:b@127.0.0.1:8080",
        "http://127.0.0.1@example.com",
        "file:///etc/passwd",
        "file://localhost/etc/passwd",
        "ftp://127.0.0.1",
        "ws://127.0.0.1:8080",
        "http://0.0.0.0:8080",
        "http://0",
        "http://0127.0.0.1",
        "http://[::ffff:10.0.0.5]",
        "http://[fe80::1]",
        "127.0.0.1:8080",
        "localhost:8080",
        "",
        "not a url",
        // This machine, with more than an address: a path the door would append to, a query, a
        // fragment, and the backslash a browser reads as a slash.
        "http://127.0.0.1:8080/v1",
        "http://127.0.0.1:8080/?a=b",
        "http://127.0.0.1:8080/#top",
        "http://127.0.0.1\\@example.com",
    ] {
        let why = loopback_base(refused).expect_err(refused);
        assert!(
            why.contains("this server's own machine"),
            "{refused} is refused with a sentence that says what to give instead: {why}"
        );
    }
}

/// A turn's gaps are refusals the door will say, never a fall back to the gateway.
#[test]
fn a_setting_with_a_gap_is_a_refusal_that_names_the_gap() {
    let unavailable = |endpoint: ModelEndpoint| match endpoint {
        ModelEndpoint::Unavailable(sentence) => sentence,
        dialled => format!("a gap must not dial, and this one does: {dialled:?}"),
    };
    let base = Some("http://127.0.0.1:8080");
    let no_base = unavailable(endpoint(None, "gpt-5.5", Ok(None)));
    assert!(no_base.contains("no proxy address is set"), "{no_base}");
    let no_model = unavailable(endpoint(base, "", Ok(None)));
    assert!(
        no_model.starts_with("Choose a model for your own subscription first"),
        "{no_model}"
    );
    let no_key = unavailable(endpoint(base, "gpt-5.5", Err("no vault".to_string())));
    assert!(
        no_key.contains("could not be opened (no vault)"),
        "{no_key}"
    );

    assert_eq!(
        endpoint(base, "gpt-5.5", Ok(Some("k".to_string()))),
        ModelEndpoint::Proxy {
            base_url: "http://127.0.0.1:8080".to_string(),
            auth: Some((KEY_HEADER.to_string(), "k".to_string())),
        }
    );
    assert_eq!(
        endpoint(base, "gpt-5.5", Ok(None)),
        ModelEndpoint::Proxy {
            base_url: "http://127.0.0.1:8080".to_string(),
            auth: None,
        },
        "no key is no header: a loopback bind asks for none"
    );
}

/// A person's saved setting, or none when it cannot be read; its key is always `k`.
struct Stored(Option<InferenceSource>);

#[async_trait::async_trait]
impl Saved for Stored {
    async fn setting(&self, _: &AccountId) -> Option<InferenceSource> {
        self.0.clone()
    }
    async fn key(&self, _: &AccountId, saved: bool) -> Result<Option<String>, String> {
        Ok(saved.then(|| "k".to_string()))
    }
}

/// EVERY PATH ASKS ONE FUNCTION WHERE A TURN GOES — a fresh turn, a drained queued send, a
/// carry-on — and it never guesses the gateway for a person who chose their own subscription.
#[tokio::test]
async fn a_turns_source_is_resolved_in_one_place_and_never_guessed() {
    let ada = AccountId::new();
    let on_proxy = InferenceSource {
        kind: SourceKind::LocalProxy,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        local_model: Some("gpt-5.5".to_string()),
        has_key: true,
    };
    let on_gateway = InferenceSource {
        kind: SourceKind::Gateway,
        ..on_proxy.clone()
    };
    let asked = |route: Route| route.asked("xai/grok-4.6".to_string());
    let dialled = Some(ModelEndpoint::Proxy {
        base_url: "http://127.0.0.1:8080".to_string(),
        auth: Some((KEY_HEADER.to_string(), "k".to_string())),
    });
    let (proxy, gateway) = (Some(SourceKind::LocalProxy), Some(SourceKind::Gateway));

    // The account's setting when the turn names none; the turn's own word either way.
    let routed = route(&Stored(Some(on_proxy.clone())), Some(&ada), None, None).await;
    assert_eq!(routed.kind(), SourceKind::LocalProxy);
    assert_eq!(asked(routed), ("gpt-5.5".to_string(), dialled.clone()));
    let routed = route(&Stored(Some(on_gateway)), Some(&ada), proxy, None).await;
    assert_eq!(asked(routed), ("gpt-5.5".to_string(), dialled));
    let routed = route(&Stored(Some(on_proxy.clone())), Some(&ada), gateway, None).await;
    assert_eq!(routed.kind(), SourceKind::Gateway);
    assert_eq!(asked(routed), ("xai/grok-4.6".to_string(), None), "the pin");

    // A carry-on asks on the model its run started on, not the setting's latest.
    let routed = route(
        &Stored(Some(on_proxy)),
        Some(&ada),
        proxy,
        Some("gpt-6-sol"),
    )
    .await;
    assert_eq!(asked(routed).0, "gpt-6-sol");

    // A setting that cannot be read keeps the gateway, unless the proxy was named: then it is
    // refused in words, on no model, and never the coworker's pin.
    let routed = route(&Stored(None), Some(&ada), None, None).await;
    assert_eq!(routed.kind(), SourceKind::Gateway);
    let (model, endpoint) = asked(route(&Stored(None), Some(&ada), proxy, None).await);
    assert_eq!(model, "");
    assert!(
        matches!(&endpoint, Some(ModelEndpoint::Unavailable(why)) if why.contains("could not be read")),
        "{endpoint:?}"
    );
}

/// What a stand-in proxy was sent: each request's head and body, as text.
type Seen = Arc<Mutex<Vec<String>>>;

/// A stand-in on loopback that answers every request with `reply`, keeping what it was sent.
async fn a_proxy_answering(reply: String) -> (String, Seen) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 65536];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            kept.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buffer[..read]).to_string());
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (url, seen)
}

fn streaming(text: &str) -> String {
    let body = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
    );
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn to_the_proxy(base: &str, key: Option<&str>, model: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        messages: vec![crate::model::ChatMessage::text("user", "hello")],
        // The coworker's own gateway key, which a proxy turn must never carry out.
        gateway_key: Some(crate::model::GatewayKey::new("oag_live_coworker_secret")),
        endpoint: Some(ModelEndpoint::Proxy {
            base_url: base.to_string(),
            auth: key.map(|key| (KEY_HEADER.to_string(), key.to_string())),
        }),
        ..ModelRequest::default()
    }
}

async fn said(door: &crate::GatewayDoor, request: ModelRequest) -> Result<String, ModelError> {
    let mut stream = door.stream(request).await?;
    let mut text = String::new();
    while let Some(delta) = stream.next().await {
        if let ModelDelta::Text(piece) = delta? {
            text.push_str(&piece);
        }
    }
    Ok(text)
}

/// A proxy turn goes to the proxy, with the person's key in the one header opencodex reads and
/// no `Authorization` at all: never the deployment's gateway key, never the coworker's.
#[tokio::test]
async fn a_proxy_turn_carries_the_persons_key_and_never_the_gateways() {
    let (proxy, seen) = a_proxy_answering(streaming("from the proxy")).await;
    let door = crate::GatewayDoor::new("http://127.0.0.1:1", "oag_live_deployment_secret");
    let turn = ModelRequest {
        // A scoped turn, which on the gateway carries the conversation pin as `user`.
        spend_scope: Some("cw-1".to_string()),
        spend_actor: Some("acct-1".to_string()),
        ..to_the_proxy(&proxy, Some("proxy-key-1"), "gpt-5.5")
    };
    let text = said(&door, turn).await.expect("the proxy answers");
    assert_eq!(text, "from the proxy");
    let sent = seen.lock().unwrap().join("\n");
    let (head, body) = sent.split_once("\r\n\r\n").unwrap();
    let head = head.to_ascii_lowercase();
    assert!(head.starts_with("post /v1/chat/completions "), "{head}");
    assert!(head.contains("x-opencodex-api-key: proxy-key-1"), "{head}");
    assert!(
        !head.contains("authorization"),
        "no bearer of any kind: {head}"
    );
    assert!(
        !sent.contains("oag_live"),
        "no gateway key anywhere: {sent}"
    );
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["model"], "gpt-5.5", "the proxy's own id: {body}");
    assert!(
        body.get("user").is_none(),
        "the gateway's affinity pin stays home: {body}"
    );

    // Without a key, no key header either: opencodex on loopback asks for none.
    let (proxy, seen) = a_proxy_answering(streaming("ok")).await;
    said(&door, to_the_proxy(&proxy, None, "gpt-5.5"))
        .await
        .expect("the proxy answers");
    let sent = seen.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(!sent.contains("x-opencodex-api-key"), "{sent}");
    assert!(!sent.contains("authorization"), "{sent}");
}

/// A LOOPBACK ADDRESS MUST NOT BOUNCE ANYWHERE. The proxy answers 302 to another port; nothing
/// follows it — not the door, not the settings page's probe, not its model listing — and the
/// turn fails with a sentence naming the proxy.
#[tokio::test]
async fn a_proxy_that_redirects_elsewhere_is_not_followed() {
    let (elsewhere, reached) = a_proxy_answering(streaming("followed")).await;
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nlocation: {elsewhere}/v1/chat/completions\r\n\
         content-length: 0\r\nconnection: close\r\n\r\n"
    );
    let (proxy, seen) = a_proxy_answering(redirect).await;
    let door = crate::GatewayDoor::new("http://127.0.0.1:1", "oag_live_deployment_secret");
    let error = said(&door, to_the_proxy(&proxy, None, "gpt-5.5"))
        .await
        .expect_err("a redirect is not an answer");
    assert!(
        matches!(&error, ModelError::Proxy(sentence) if sentence.contains("(302)")),
        "{error:?}"
    );
    assert!(!healthy(&proxy).await, "a 302 is not healthy");
    assert!(models(&proxy, None).await.is_err(), "nor a list of models");
    assert_eq!(seen.lock().unwrap().len(), 3, "the proxy itself was asked");
    assert!(
        reached.lock().unwrap().is_empty(),
        "and the place it pointed at never was"
    );
}

/// The door asks the address and the model again, whoever built the request: a stored address
/// that is not loopback, or a model the terms forbid, is refused before anything is dialled.
#[tokio::test]
async fn the_door_refuses_a_request_its_setting_should_never_have_allowed() {
    let (elsewhere, reached) = a_proxy_answering(streaming("reached")).await;
    let door = crate::GatewayDoor::new("http://127.0.0.1:1", "k");
    let wide = to_the_proxy(&elsewhere.replace("127.0.0.1", "10.0.0.1"), None, "gpt-5.5");
    let error = said(&door, wide).await.expect_err("not loopback");
    assert!(
        matches!(&error, ModelError::Proxy(sentence) if sentence.contains("address cannot be used")),
        "{error:?}"
    );
    for forbidden in [
        "claude-sonnet-4.5",
        "anthropic/claude-opus",
        "gemini-2.5-pro",
        "llama-3",
    ] {
        let error = said(&door, to_the_proxy(&elsewhere, None, forbidden))
            .await
            .expect_err(forbidden);
        assert!(
            matches!(&error, ModelError::Proxy(sentence) if sentence.contains("was not sent")),
            "{forbidden}: {error:?}"
        );
    }
    let error = said(
        &door,
        ModelRequest {
            endpoint: Some(ModelEndpoint::Unavailable(
                "no proxy address is set".to_string(),
            )),
            ..to_the_proxy(&elsewhere, None, "gpt-5.5")
        },
    )
    .await
    .expect_err("a gap is a refusal");
    assert_eq!(error.sentence(), "no proxy address is set");
    assert!(reached.lock().unwrap().is_empty(), "nothing was dialled");
}

/// THE UPSTREAM'S OWN WORDS REACH THE RUN. OpenAI refusing a model a ChatGPT account cannot use
/// says why in the body, as a bare `detail` or in the error envelope; the run's `RUN_ERROR`
/// carries that sentence, bounded, rather than a generic one.
#[tokio::test]
async fn an_upstream_refusal_reaches_the_runs_error_in_its_own_words() {
    let why = "The 'gpt-6-terra' model is not supported when using Codex with a ChatGPT account.";
    for body in [
        serde_json::json!({ "detail": why }).to_string(),
        serde_json::json!({ "error": { "message": why, "type": "invalid_request_error" } })
            .to_string(),
        why.to_string(),
    ] {
        let (proxy, _) = a_proxy_answering(format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let door = crate::GatewayDoor::new("http://127.0.0.1:1", "k");
        let events = crate::run_conversation(
            &door,
            None,
            &crate::MemoryJournal::new(),
            to_the_proxy(&proxy, None, "gpt-6-terra"),
            "t1",
            "r1",
            1,
        )
        .await;
        let ending = events.last().unwrap();
        assert_eq!(
            ending.event_type,
            opengrok_wire::agui::EventType::RunError,
            "{body}"
        );
        let message = ending.extra["message"].as_str().unwrap();
        assert_eq!(
            message,
            format!("Your proxy refused this turn (400): {why}")
        );
    }

    // Bounded: a page of an upstream's words is not a run's error.
    let long = format!("{{\"detail\":\"{}\"}}", "word ".repeat(400));
    let (proxy, _) = a_proxy_answering(format!(
        "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{long}",
        long.len()
    ))
    .await;
    let door = crate::GatewayDoor::new("http://127.0.0.1:1", "k");
    let error = said(&door, to_the_proxy(&proxy, None, "gpt-5.5"))
        .await
        .expect_err("a 400");
    assert!(
        error.sentence().chars().count() < 300,
        "{}",
        error.sentence()
    );
}

/// A proxy that is not running says so in the proxy's words, not the gateway's.
#[tokio::test]
async fn a_proxy_that_is_not_running_is_named_as_the_proxy() {
    let door = crate::GatewayDoor::new("http://127.0.0.1:1", "k");
    let error = said(&door, to_the_proxy("http://127.0.0.1:1", None, "gpt-5.5"))
        .await
        .expect_err("nothing listens on port 1");
    let sentence = error.sentence();
    assert!(
        sentence.contains("Your proxy could not be reached"),
        "{sentence}"
    );
    assert!(!sentence.contains("gateway could not"), "{sentence}");
    assert!(!healthy("http://127.0.0.1:1").await);
}

/// The settings page's two questions: healthy is a 2xx from `/healthz`, and the model list is
/// the proxy's, kept to what a person's subscription may run.
#[tokio::test]
async fn the_proxy_is_asked_whether_it_is_up_and_what_it_serves() {
    let listing = r#"{"object":"list","data":[{"id":"gpt-5.5"},{"id":"gpt-5-codex"},{"id":"claude-sonnet-4.5"},{"id":"gemini-2.5-pro"},{"id":"o3"},{"id":"mistral-large"}]}"#;
    let (proxy, seen) = a_proxy_answering(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{listing}",
        listing.len()
    ))
    .await;
    assert!(healthy(&proxy).await);
    assert_eq!(
        models(&proxy, Some("proxy-key-1")).await.unwrap(),
        ["gpt-5.5", "gpt-5-codex", "o3"]
    );
    let sent = seen.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(sent.contains("get /healthz "), "{sent}");
    assert!(sent.contains("get /v1/models "), "{sent}");
    assert!(sent.contains("x-opencodex-api-key: proxy-key-1"), "{sent}");
    assert!(
        !healthy("http://10.0.0.1:8080").await,
        "never asked off this machine"
    );
}
