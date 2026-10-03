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

/// A turn's gaps are refusals the door will say, never a fall back to the gateway. A gap the
/// person can fill, an address or a model, is `unset`; a key the vault cannot open is not.
#[test]
fn a_setting_with_a_gap_is_a_refusal_that_names_the_gap() {
    let refusal = |endpoint: ModelEndpoint| match endpoint {
        ModelEndpoint::Unavailable {
            why,
            via: Some(Via::Loopback),
            unset,
        } => (why, unset),
        dialled => (format!("a gap must not dial: {dialled:?}"), false),
    };
    let base = Some("http://127.0.0.1:8080");
    let no_base = refusal(endpoint(None, "gpt-5.5", Ok(None)));
    assert!(no_base.0.contains("no proxy address is set"), "{no_base:?}");
    assert!(no_base.1, "the person's own gap: {no_base:?}");
    let no_model = refusal(endpoint(base, "", Ok(None)));
    assert!(
        no_model
            .0
            .starts_with("Choose a model for your own subscription first"),
        "{no_model:?}"
    );
    assert!(no_model.1, "the person's own gap: {no_model:?}");
    let no_key = refusal(endpoint(base, "gpt-5.5", Err("no vault".to_string())));
    assert!(
        no_key.0.contains("could not be opened (no vault)"),
        "{no_key:?}"
    );
    assert!(!no_key.1, "the vault's fault, not a gap: {no_key:?}");

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

/// A turn with no coworker door or pin of its own: what every turn was before a coworker had one.
const NO_BOT: (Option<SourceKind>, Option<String>) = (None, None);

/// A person's saved setting, or none when it cannot be read; its key is always `k`.
struct Stored(Option<InferenceSource>, Arc<crate::relay::RelayBroker>);

fn stored(setting: Option<InferenceSource>) -> Stored {
    Stored(setting, Arc::default())
}

#[async_trait::async_trait]
impl Saved for Stored {
    async fn setting(&self, _: &AccountId) -> Option<InferenceSource> {
        self.0.clone()
    }
    async fn key(&self, _: &AccountId, saved: bool) -> Result<Option<String>, String> {
        Ok(saved.then(|| "k".to_string()))
    }
    fn relay(&self) -> Arc<crate::relay::RelayBroker> {
        self.1.clone()
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
        ..Default::default()
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
    let proxy = Some(TurnSource::from(SourceKind::LocalProxy));
    let gateway = Some(TurnSource::from(SourceKind::Gateway));

    // The account's setting when the turn names none; the turn's own word either way.
    let routed = route(
        &stored(Some(on_proxy.clone())),
        Some(&ada),
        None,
        None,
        "r1",
        NO_BOT,
    )
    .await;
    assert_eq!(routed.kind(), SourceKind::LocalProxy);
    assert_eq!(
        routed.source().via,
        Some(Via::Loopback),
        "a loopback turn says so"
    );
    assert_eq!(asked(routed), ("gpt-5.5".to_string(), dialled.clone()));
    let on_gateway = stored(Some(on_gateway));
    let routed = route(&on_gateway, Some(&ada), proxy, None, "r1", NO_BOT).await;
    assert_eq!(asked(routed), ("gpt-5.5".to_string(), dialled));
    let routed = route(
        &stored(Some(on_proxy.clone())),
        Some(&ada),
        gateway,
        None,
        "r1",
        NO_BOT,
    )
    .await;
    assert_eq!(routed.kind(), SourceKind::Gateway);
    assert_eq!(asked(routed), ("xai/grok-4.6".to_string(), None), "the pin");

    // A carry-on asks on the model its run started on, not the setting's latest.
    let routed = route(
        &stored(Some(on_proxy)),
        Some(&ada),
        proxy,
        Some("gpt-6-sol"),
        "r1",
        NO_BOT,
    )
    .await;
    assert_eq!(asked(routed).0, "gpt-6-sol");

    // A SETTING THAT CANNOT BE READ IS NEVER GUESSED AS THE GATEWAY: a turn that named no source,
    // or the proxy, is refused in words, on no model and never a gateway pin; a carry-on on the
    // model its run started on. Only a turn that named the gateway itself goes there.
    for chosen in [None, proxy] {
        let routed = route(&stored(None), Some(&ada), chosen, None, "r1", NO_BOT).await;
        let (model, endpoint) = asked(routed);
        assert_eq!(model, "", "{chosen:?}");
        let refused = "Your reply source could not be read, so the turn was not sent";
        assert!(
            matches!(&endpoint, Some(ModelEndpoint::Unavailable { why, unset: false, .. })
                if why.starts_with(refused)),
            "this side's fault, not a gap the person can fill: {chosen:?}: {endpoint:?}"
        );
    }
    let carried = route(
        &stored(None),
        Some(&ada),
        proxy,
        Some("gpt-6-sol"),
        "r1",
        NO_BOT,
    )
    .await;
    assert_eq!(asked(carried).0, "gpt-6-sol");
    let routed = route(&stored(None), Some(&ada), gateway, None, "r1", NO_BOT).await;
    assert_eq!(asked(routed), ("xai/grok-4.6".to_string(), None), "named");
}

/// RELAY OFF (#332): a fresh turn that would go by the Mac asks the person's fallback on the
/// gateway instead, on its model and effort, whatever the coworker is pinned to, and says why; a
/// carry-on of a turn the Mac started never changes door, and with no fallback the turn is refused
/// in words, `plan_unavailable`. The switch is the relay's alone: the loopback ignores it, and a
/// fallback is never asked while the relay is on.
#[tokio::test]
async fn a_turn_by_the_mac_with_the_relay_off_asks_the_fallback_or_is_refused() {
    let ada = AccountId::new();
    let fallback = PlanFallback {
        model: "xai/grok-4.6".to_string(),
        effort: Effort::Low,
    };
    let off = InferenceSource {
        kind: SourceKind::LocalProxy,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        local_model: Some("gpt-5.5".to_string()),
        via: Some(Via::Mac),
        relay_model: Some("gpt-6-sol".to_string()),
        relay_off: true,
        plan_fallback: Some(fallback.clone()),
        ..Default::default()
    };
    let luna = (Some(SourceKind::LocalProxy), Some("gpt-6-luna".to_string()));
    for (bot, pin) in [(NO_BOT, "oag/cheap"), (luna, "gpt-6-luna")] {
        let routed = route(
            &stored(Some(off.clone())),
            Some(&ada),
            None,
            None,
            "r1",
            bot,
        )
        .await;
        assert_eq!(routed.source(), SourceKind::Gateway.into(), "{pin}");
        assert_eq!(
            routed.asks(pin),
            Some(("xai/grok-4.6", SourceKind::Gateway))
        );
        assert_eq!(
            routed.fallback(Effort::High),
            (Effort::Low, Some("relay_disabled"))
        );
        let asked = routed.asked(pin.to_string());
        assert_eq!(
            asked,
            ("xai/grok-4.6".to_string(), None),
            "the fallback's, not {pin}"
        );
    }

    let refused_off = |routed: Route| {
        let (_, endpoint) = routed.asked(String::new());
        let Some(ModelEndpoint::Unavailable { why, via, unset }) = endpoint else {
            return Err(format!("{endpoint:?}"));
        };
        let said = why.starts_with(RELAY_OFF) && via == Some(Via::Mac) && unset;
        said.then_some(why)
            .ok_or("not the relay's refusal".to_string())
    };
    let mac = TurnSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
    };
    let carried = Some("gpt-6-sol");
    let saved = stored(Some(off.clone()));
    let carry_on = route(&saved, Some(&ada), Some(mac), carried, "r2", NO_BOT).await;
    assert!(
        refused_off(carry_on).is_ok(),
        "a carry-on never changes door"
    );
    let none = stored(Some(InferenceSource {
        plan_fallback: None,
        ..off.clone()
    }));
    let why = refused_off(route(&none, Some(&ada), None, None, "r3", NO_BOT).await);
    assert_eq!(
        why.as_deref(),
        Ok(
            "Relay is off for your plan, so the turn was not sent. Turn Relay on, or choose a \
            Server model to answer while it is off, or switch this turn to the gateway."
        )
    );

    let loopback = stored(Some(InferenceSource {
        via: Some(Via::Loopback),
        ..off.clone()
    }));
    let routed = route(&loopback, Some(&ada), None, None, "r4", NO_BOT).await;
    let (model, endpoint) = routed.asked(String::new());
    assert_eq!(model, "gpt-5.5", "the loopback ignores the switch");
    assert!(
        matches!(endpoint, Some(ModelEndpoint::Proxy { .. })),
        "{endpoint:?}"
    );
    let on = stored(Some(InferenceSource {
        relay_off: false,
        ..off.clone()
    }));
    let routed = route(&on, Some(&ada), None, None, "r5", NO_BOT).await;
    let (model, endpoint) = routed.asked(String::new());
    assert_eq!(
        model, "gpt-6-sol",
        "on, the Mac answers and the fallback waits"
    );
    assert!(
        matches!(endpoint, Some(ModelEndpoint::Relay(_))),
        "{endpoint:?}"
    );
    let gateway = Some(SourceKind::Gateway.into());
    let named = route(&saved, Some(&ada), gateway, None, "r6", NO_BOT).await;
    assert_eq!(
        named.fallback(Effort::High),
        (Effort::High, None),
        "the turn's own gateway"
    );
}

/// GET SAYS THE SWITCH AND THE FALLBACK ALWAYS (#332): on and null until a person sets them.
#[tokio::test]
async fn the_setting_reads_back_the_relay_switch_and_the_fallback() {
    let unset = described(&InferenceSource::default(), None).await;
    assert_eq!(
        (&unset["relayEnabled"], &unset["planFallback"]),
        (&serde_json::json!(true), &serde_json::Value::Null)
    );
    let off = InferenceSource {
        relay_off: true,
        plan_fallback: Some(PlanFallback {
            model: "xai/grok-4.6".to_string(),
            effort: Effort::High,
        }),
        ..Default::default()
    };
    let said = described(&off, None).await;
    let fallback = serde_json::json!({ "model": "xai/grok-4.6", "effort": "high" });
    assert_eq!(
        (&said["relayEnabled"], &said["planFallback"]),
        (&serde_json::json!(false), &fallback)
    );
}

/// THE MAC IS A WAY, NOT A SOURCE: `via: "mac"` over the same `local_proxy` kind resolves to the
/// account's relay for this run, on the Mac's own model, and never the loopback's address, model
/// or key. The turn's own way beats the setting's, as its kind does, and a carry-on keeps the way
/// its run captured. No model for the Mac is a refusal in words, never the loopback's model.
#[tokio::test]
async fn a_turn_by_the_mac_is_relayed_for_its_run_on_the_macs_own_model() {
    let ada = AccountId::new();
    let by_mac = InferenceSource {
        kind: SourceKind::LocalProxy,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        local_model: Some("gpt-5.5".to_string()),
        has_key: true,
        via: Some(Via::Mac),
        relay_model: Some("gpt-6-sol".to_string()),
        ..Default::default()
    };
    let saved = stored(Some(by_mac.clone()));
    let relayed = |run: &str| {
        Some(ModelEndpoint::Relay(crate::relay::RelayTo {
            broker: saved.1.clone(),
            account: ada.as_str().to_string(),
            run_id: run.to_string(),
        }))
    };
    let routed = route(&saved, Some(&ada), None, None, "r1", NO_BOT).await;
    assert_eq!(routed.source().via, Some(Via::Mac));
    let (model, endpoint) = routed.asked("xai/grok-4.6".to_string());
    assert_eq!((model.as_str(), endpoint), ("gpt-6-sol", relayed("r1")));

    let loopback = TurnSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Loopback),
    };
    let (model, endpoint) = route(&saved, Some(&ada), Some(loopback), None, "r2", NO_BOT)
        .await
        .asked(String::new());
    assert_eq!(model, "gpt-5.5", "the turn's own way wins");
    assert!(
        matches!(endpoint, Some(ModelEndpoint::Proxy { .. })),
        "{endpoint:?}"
    );

    let mac = TurnSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
    };
    let on_loopback = stored(Some(InferenceSource {
        via: None,
        ..by_mac.clone()
    }));
    let carried = Some("gpt-5.5");
    let (model, endpoint) = route(&on_loopback, Some(&ada), Some(mac), carried, "r3", NO_BOT)
        .await
        .asked(String::new());
    assert_eq!(model, "gpt-5.5", "a carry-on on the model it started on");
    assert!(
        matches!(endpoint, Some(ModelEndpoint::Relay(_))),
        "{endpoint:?}"
    );

    let no_model = stored(Some(InferenceSource {
        relay_model: None,
        ..by_mac
    }));
    let (model, endpoint) = route(&no_model, Some(&ada), None, None, "r4", NO_BOT)
        .await
        .asked(String::new());
    assert_eq!(model, "", "never the loopback's model");
    assert!(
        matches!(&endpoint,
            Some(ModelEndpoint::Unavailable { why, via: Some(Via::Mac), unset: true })
                if why.starts_with("Choose a model for your Mac first")),
        "{endpoint:?}"
    );
}

/// A COWORKER'S OWN DOOR sits under the turn's word and over the setting. Its pin is asked of the
/// proxy only on its own plan — its `source` is `local_proxy` — under the run's captured model and
/// over the setting's, and only where a subscription may answer it; off its plan the setting's
/// model is asked, whatever it is pinned to. The plan is always the driving person's: with none
/// set, a coworker on its own door is refused in words and never sent to the gateway.
#[tokio::test]
async fn a_coworkers_own_door_and_pin_sit_between_the_turn_and_the_setting() {
    let ada = AccountId::new();
    let on_gateway = InferenceSource {
        kind: SourceKind::Gateway,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        local_model: Some("gpt-5.5".to_string()),
        ..Default::default()
    };
    let unset = InferenceSource {
        local_model: None,
        ..on_gateway.clone()
    };
    let on_proxy = stored(Some(InferenceSource {
        kind: SourceKind::LocalProxy,
        ..on_gateway.clone()
    }));
    let (on_gateway, unset) = (stored(Some(on_gateway)), stored(Some(unset)));
    let (proxy, gateway) = (Some(SourceKind::LocalProxy), Some(SourceKind::Gateway));
    let (by_proxy, by_gateway) = (proxy.map(TurnSource::from), gateway.map(TurnSource::from));
    let own = |pin: &str| (proxy, Some(pin.to_string()));
    let asked = |route: Route| route.asked("xai/grok-4.6@sub".to_string());
    let dialled = Some(ModelEndpoint::Proxy {
        base_url: "http://127.0.0.1:8080".to_string(),
        auth: None,
    });

    // Its own door over the setting's, asked on its own pin.
    let routed = route(&on_gateway, Some(&ada), None, None, "r1", own("gpt-6-luna")).await;
    assert_eq!(asked(routed), ("gpt-6-luna".to_string(), dialled.clone()));
    // The turn's word over its door, either way.
    let routed = route(
        &on_proxy,
        Some(&ada),
        by_gateway,
        None,
        "r1",
        own("gpt-6-luna"),
    )
    .await;
    assert_eq!(asked(routed), ("xai/grok-4.6@sub".to_string(), None));
    // Off its own plan its pin is never asked, even one the allowlist takes: a coworker on the
    // gateway door whose turn picks the proxy, and one that follows the setting (a default hire,
    // pinned `xai/grok-4.6`), both ask the setting's model, as before coworkers had doors.
    let door = || (gateway, Some("gpt-6-luna".to_string()));
    let routed = route(&on_proxy, Some(&ada), by_proxy, None, "r1", door()).await;
    assert_eq!(asked(routed).0, "gpt-5.5", "its pin only on its own plan");
    let hired = (None, Some("xai/grok-4.6".to_string()));
    let routed = route(&on_proxy, Some(&ada), None, None, "r1", hired).await;
    assert_eq!(asked(routed), ("gpt-5.5".to_string(), dialled.clone()));
    // A coworker on the gateway reads no setting, as a turn that named the gateway does.
    let routed = route(&stored(None), Some(&ada), None, None, "r1", door()).await;
    assert_eq!(routed.kind(), SourceKind::Gateway);
    // On its plan a pin the allowlist refuses falls through to the setting's model; the run's
    // captured one beats both.
    let gateway_pin = own("xai/grok-4.6@sub");
    let routed = route(&on_proxy, Some(&ada), None, None, "r1", gateway_pin).await;
    assert_eq!(asked(routed), ("gpt-5.5".to_string(), dialled));
    let captured = Some("gpt-6-sol");
    let routed = route(
        &on_proxy,
        Some(&ada),
        by_proxy,
        captured,
        "r1",
        own("gpt-6-luna"),
    )
    .await;
    assert_eq!(asked(routed).0, "gpt-6-sol");
    // None of the three is the "no model" refusal.
    let routed = route(
        &unset,
        Some(&ada),
        None,
        None,
        "r1",
        own("xai/grok-4.6@sub"),
    )
    .await;
    let (model, endpoint) = asked(routed);
    assert_eq!(model, "");
    assert!(
        matches!(&endpoint, Some(ModelEndpoint::Unavailable { why, .. }) if why.starts_with("Choose a model")),
        "{endpoint:?}"
    );

    // A teammate with no proxy of their own, on a coworker whose owner has one.
    let teammate = stored(Some(InferenceSource::default()));
    let routed = route(&teammate, Some(&ada), None, None, "r1", own("gpt-6-luna")).await;
    assert_eq!(routed.kind(), SourceKind::LocalProxy, "never the gateway");
    assert_eq!(
        routed.asks("xai/grok-4.6@sub"),
        None,
        "a refusal asks no model"
    );
    let (model, endpoint) = asked(routed);
    assert_eq!(model, "gpt-6-luna");
    assert!(
        matches!(&endpoint, Some(ModelEndpoint::Unavailable { why, unset: true, .. })
            if why.contains("no proxy address is set")),
        "the teammate's own gap: {endpoint:?}"
    );
}

/// A MONITOR RUNS ON THE SERVER'S KEYS, as every firing did before routines ran on the plan
/// (#316, review of #334): on the gateway for a coworker whose own door is none or the gateway, on
/// its pin; refused in words, asking nothing, for one on its own plan.
#[test]
fn a_monitor_asks_the_gateway_unless_its_coworker_is_on_its_own_plan() {
    for source in [None, Some(SourceKind::Gateway)] {
        let routed = Route::for_monitor(source, "xai/grok-4.6");
        let asked = routed.asked("xai/grok-4.6".to_string());
        assert_eq!(asked, ("xai/grok-4.6".to_string(), None), "{source:?}");
    }
    let routed = Route::for_monitor(Some(SourceKind::LocalProxy), "gpt-6-luna");
    assert_eq!(routed.source(), SourceKind::LocalProxy.into());
    assert_eq!(routed.asks("gpt-6-luna"), None, "a refusal asks no model");
    let (model, endpoint) = routed.asked("gpt-6-luna".to_string());
    assert_eq!(model, "gpt-6-luna");
    let said = "This Bot answers on your own plan, and routines run on the server's keys, so this \
                routine did not run. Give the Bot a Server model to run it on a schedule.";
    let refused = ModelEndpoint::Unavailable {
        why: said.to_string(),
        via: None,
        unset: false,
    };
    assert_eq!(endpoint, Some(refused), "not the person's gap to fill");
}

/// A CARRY-ON GOES WHERE ITS START WENT, on what the start captured, a routine's as a turn's: one
/// that started on its coworker's plan goes on at the person's proxy, on the model it started on;
/// one that started on the gateway goes on there, though the person's setting is that proxy. A
/// Bot's turn on another Bot's message (#314) is not a routine's: on its plan it is refused again,
/// in `for_message`'s words, and never sent to the proxy.
#[tokio::test]
async fn a_routines_carry_on_goes_where_its_start_went() {
    use opengrok_core::run::{Run, RunCommand, routine_prompt};
    let ada = AccountId::new();
    let on_proxy = stored(Some(InferenceSource {
        kind: SourceKind::LocalProxy,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        local_model: Some("gpt-5.5".to_string()),
        ..Default::default()
    }));
    let run_id = RunId::new();
    let started = |kind: SourceKind, thread_id: &str| {
        let mut run = Run::default();
        let start = RunCommand::Start {
            thread_id: thread_id.to_string(),
            coworker_id: None,
            model: Some("gpt-6-luna".to_string()),
            effort: Default::default(),
            inference_source: kind.into(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: Some(routine_prompt(&run_id, "write the weekly report")),
            limits: Default::default(),
            at_ms: 1,
        };
        for event in run.decide(start).unwrap() {
            run.apply(&event);
        }
        run
    };
    let pin = "gpt-6-luna";
    let on_its_plan = started(SourceKind::LocalProxy, "sched_weekly");
    let (model, endpoint) = resumed(&on_proxy, &ada, (&on_its_plan, &run_id), pin)
        .await
        .asked(pin.to_string());
    assert_eq!(model, "gpt-6-luna", "the model it started on");
    assert!(
        matches!(endpoint, Some(ModelEndpoint::Proxy { .. })),
        "{endpoint:?}"
    );
    let on_the_gateway = started(SourceKind::Gateway, "sched_weekly");
    let routed = resumed(&on_proxy, &ada, (&on_the_gateway, &run_id), pin).await;
    assert_eq!(routed.asked(pin.to_string()), (pin.to_string(), None));

    let pair = opengrok_wire::pair::pair_thread("cw_a", "cw_b");
    let message = started(SourceKind::LocalProxy, &pair);
    let routed = resumed(&on_proxy, &ada, (&message, &run_id), pin).await;
    let again = Route::for_message(Some(SourceKind::LocalProxy), pin);
    assert_eq!(
        routed.asked(pin.to_string()),
        again.asked(pin.to_string()),
        "a message's turn is refused again in its own words, never sent to the proxy"
    );
}

/// THE SAME RULE BY THE MAC: a coworker on its own plan asks the person's Mac for its pin; one off
/// it asks the Mac for the setting's `relay.localModel`, whatever it is pinned to.
#[tokio::test]
async fn a_coworkers_pin_goes_to_the_mac_only_on_its_own_plan() {
    let ada = AccountId::new();
    let by_mac = stored(Some(InferenceSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
        relay_model: Some("gpt-6-sol".to_string()),
        ..Default::default()
    }));
    let own = (Some(SourceKind::LocalProxy), Some("gpt-6-luna".to_string()));
    let (model, endpoint) = route(&by_mac, Some(&ada), None, None, "r1", own)
        .await
        .asked(String::new());
    assert_eq!(model, "gpt-6-luna", "its pin, by the Mac");
    assert!(
        matches!(endpoint, Some(ModelEndpoint::Relay(_))),
        "{endpoint:?}"
    );
    for off in [None, Some(SourceKind::Gateway)] {
        let chosen = Some(TurnSource::from(SourceKind::LocalProxy));
        let hired = (off, Some("xai/grok-4.6".to_string()));
        let (model, _) = route(&by_mac, Some(&ada), chosen, None, "r2", hired)
            .await
            .asked(String::new());
        assert_eq!(
            model, "gpt-6-sol",
            "{off:?}: the setting's model for the Mac"
        );
    }
}

/// What a route will ask, as a turn's system message names it.
#[test]
fn a_route_names_the_model_it_asks_and_the_door() {
    assert_eq!(
        Route::Gateway.asks("xai/grok-4.6@sub"),
        Some(("xai/grok-4.6@sub", SourceKind::Gateway))
    );
    let dialled = Route::LocalProxy {
        model: "gpt-6-luna".to_string(),
        endpoint: ModelEndpoint::Proxy {
            base_url: "http://127.0.0.1:8080".to_string(),
            auth: None,
        },
    };
    assert_eq!(
        dialled.asks("xai/grok-4.6@sub"),
        Some(("gpt-6-luna", SourceKind::LocalProxy)),
        "the proxy's model, never the pin"
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
    // The person's own gap carries `plan_unavailable` onto the run's RUN_ERROR; a fault on this
    // side carries no code.
    for (unset, code) in [(true, Some("plan_unavailable")), (false, None)] {
        let error = said(
            &door,
            ModelRequest {
                endpoint: Some(ModelEndpoint::Unavailable {
                    why: "no proxy address is set".to_string(),
                    via: Some(Via::Loopback),
                    unset,
                }),
                ..to_the_proxy(&elsewhere, None, "gpt-5.5")
            },
        )
        .await
        .expect_err("a gap is a refusal");
        assert_eq!(error.sentence(), "no proxy address is set");
        assert_eq!(error.code(), code, "{error:?}");
    }
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
    let listed = models(&proxy, Some("proxy-key-1")).await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["gpt-5.5", "gpt-5-codex", "o3"]);
    let sent = seen.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(sent.contains("get /healthz "), "{sent}");
    assert!(sent.contains("get /v1/models "), "{sent}");
    assert!(sent.contains("x-opencodex-api-key: proxy-key-1"), "{sent}");
    assert!(
        !healthy("http://10.0.0.1:8080").await,
        "never asked off this machine"
    );
}
