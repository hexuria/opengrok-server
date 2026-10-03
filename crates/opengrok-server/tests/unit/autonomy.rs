use super::*;

use std::sync::Arc;

use opengrok_core::inference::{InferenceSource, PlanFallback, Via};
use opengrok_harness::relay::RelayBroker;

/// A person's saved setting; its key is always `k`.
struct Stored(Option<InferenceSource>, Arc<RelayBroker>);

fn stored(setting: InferenceSource) -> Stored {
    Stored(Some(setting), Arc::default())
}

#[async_trait::async_trait]
impl Saved for Stored {
    async fn setting(&self, _: &AccountId) -> Option<InferenceSource> {
        self.0.clone()
    }
    async fn key(&self, _: &AccountId, saved: bool) -> Result<Option<String>, String> {
        Ok(saved.then(|| "k".to_string()))
    }
    fn relay(&self) -> Arc<RelayBroker> {
        self.1.clone()
    }
}

/// A coworker hired on `pin`, with its own door.
fn coworker(source: Option<SourceKind>, pin: &str) -> Coworker {
    Coworker {
        model: pin.to_string(),
        source,
        ..Coworker::default()
    }
}

/// A stand-in on loopback that answers every request `200 ok`.
async fn a_proxy_that_answers() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
            let _ = socket.write_all(ok).await;
            let _ = socket.shutdown().await;
        }
    });
    url
}

/// A FIRING GOES WHERE ITS COWORKER'S OWN DOOR SAYS (#316): on the gateway, on its pin, for a
/// coworker whose own door is none or the gateway, though the person's setting is their proxy;
/// on the person's plan, exactly as a live turn on it, for one on its own plan.
#[tokio::test]
async fn a_routine_asks_the_gateway_unless_its_coworker_is_on_its_own_plan() {
    let (ada, run) = (AccountId::new(), RunId::new());
    let on_proxy = stored(InferenceSource {
        kind: SourceKind::LocalProxy,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        local_model: Some("gpt-5.5".to_string()),
        ..Default::default()
    });
    for source in [None, Some(SourceKind::Gateway)] {
        let grok = coworker(source, "xai/grok-4.6");
        let routed = routine_route(&on_proxy, (&ada, &run), &grok).await;
        let asked = routed.asked("xai/grok-4.6".to_string());
        assert_eq!(asked, ("xai/grok-4.6".to_string(), None), "{source:?}");
    }
    let luna = coworker(Some(SourceKind::LocalProxy), "gpt-6-luna");
    let on = routine_route(&on_proxy, (&ada, &run), &luna).await;
    assert_eq!(
        on.asks("gpt-6-luna"),
        Some(("gpt-6-luna", SourceKind::LocalProxy))
    );
    let (model, endpoint) = on.asked("gpt-6-luna".to_string());
    assert_eq!(model, "gpt-6-luna", "its pin, on its plan");
    let dialled = ModelEndpoint::Proxy {
        base_url: "http://127.0.0.1:8080".to_string(),
        auth: None,
    };
    assert_eq!(endpoint, Some(dialled));
}

/// A FIRING ON THE PLAN IS SKIPPED WHEN NOBODY CAN ANSWER IT, and says why in the words its row
/// carries: a Mac that holds no stream, or a proxy that does not answer `/healthz`. A Mac that
/// is connected, a proxy that answers, and the gateway are reachable; so is a refusal in words,
/// which a live turn on that setting gets too.
#[tokio::test]
async fn a_firing_on_the_plan_is_unreachable_while_its_mac_or_proxy_is_away() {
    let (ada, run) = (AccountId::new(), RunId::new());
    let luna = coworker(Some(SourceKind::LocalProxy), "gpt-6-luna");
    let by_mac = stored(InferenceSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
        ..Default::default()
    });
    let routed = routine_route(&by_mac, (&ada, &run), &luna).await;
    let offline = "Skipped: your computer was off, so your plan couldn't answer";
    let skipped = unreachable_by(&routed).await;
    assert_eq!(skipped, Some(("relay_offline", offline)));
    let _stream = by_mac.1.connect(ada.as_str(), "mac-1");
    assert_eq!(
        unreachable_by(&routed).await,
        None,
        "a connected Mac answers"
    );

    let up = a_proxy_that_answers().await;
    let down = Some(("proxy_down", "Skipped: your plan's proxy didn't answer"));
    for (base, said) in [("http://127.0.0.1:1", down), (up.as_str(), None)] {
        let setting = stored(InferenceSource {
            kind: SourceKind::LocalProxy,
            base_url: Some(base.to_string()),
            ..Default::default()
        });
        let routed = routine_route(&setting, (&ada, &run), &luna).await;
        assert_eq!(unreachable_by(&routed).await, said, "{base}");
    }
    assert_eq!(unreachable_by(&Route::Gateway).await, None);
    let gap = stored(InferenceSource {
        kind: SourceKind::LocalProxy,
        ..Default::default()
    });
    let refused = routine_route(&gap, (&ada, &run), &luna).await;
    let unsaid = unreachable_by(&refused).await;
    assert_eq!(unsaid, None, "refused in words instead");
}

/// RELAY OFF (#332): a firing by the Mac with no fallback is skipped `relay_disabled`, though its
/// Mac is connected; with a fallback it asks the gateway, which nothing skips.
#[tokio::test]
async fn a_firing_by_the_mac_with_the_relay_off_is_skipped_unless_a_fallback_answers() {
    let (ada, run) = (AccountId::new(), RunId::new());
    let luna = coworker(Some(SourceKind::LocalProxy), "gpt-6-luna");
    let off = InferenceSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
        relay_model: Some("gpt-5.5".to_string()),
        relay_off: true,
        ..Default::default()
    };
    let saved = stored(off.clone());
    let _stream = saved.1.connect(ada.as_str(), "mac-1");
    let routed = routine_route(&saved, (&ada, &run), &luna).await;
    let skipped = Some(("relay_disabled", "Skipped: Relay is off for your plan"));
    assert_eq!(unreachable_by(&routed).await, skipped);
    let fallback = PlanFallback {
        model: "xai/grok-4.6".to_string(),
        effort: Default::default(),
    };
    let with = stored(InferenceSource {
        plan_fallback: Some(fallback),
        ..off
    });
    let routed = routine_route(&with, (&ada, &run), &luna).await;
    assert!(matches!(routed, Route::Fallback(_)), "{routed:?}");
    assert_eq!(unreachable_by(&routed).await, None);
}
