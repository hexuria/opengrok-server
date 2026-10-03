//! Where a person's turns are answered (`opengrok_core::inference`): which models their own
//! subscription may run, which source a turn opens on, and that a run keeps the one it started on.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use opengrok_core::account::{Account, AccountCommand, AccountError, Plan};
use opengrok_core::coworker::Effort;
use opengrok_core::inference::{
    InferenceSource, KeyChange, NewBotDefault, PlanFallback, SourceKind, TurnSource, Via,
    subscription_model,
};
use opengrok_core::run::{Run, RunCommand, RunEvent};
use serde_json::json;

/// The ids opencodex 2.72.0 serves on the owner's machine, bare and provider-prefixed, each with
/// and without its `--fast` tier: every one is OpenAI's or xAI's, so every one is allowed.
#[test]
fn every_openai_and_xai_id_the_proxy_serves_is_allowed() {
    let served = [
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "openai/gpt-6.1-sol",
        "xai/grok-4.7",
        "xai/grok-4.20-multi-agent-0309",
    ];
    for id in served {
        assert_eq!(subscription_model(id), Ok(()), "{id}");
        let fast = format!("{id}--fast");
        assert_eq!(subscription_model(&fast), Ok(()), "{fast}");
    }
    for id in [
        "o1",
        "o3-mini",
        "o4-mini",
        "gpt-5-codex",
        "codex-mini-latest",
        "openai/o3",
        "openai/gpt-5-codex",
        "grok-4.6",
        "xai/grok-4.6",
        "  GPT-5.5  ",
    ] {
        assert_eq!(subscription_model(id), Ok(()), "{id}");
    }
}

/// Anthropic's and Google's terms forbid a consumer subscription through a third-party app, and
/// the refusal says so by name, prefixed or not, fast or not.
#[test]
fn anthropic_and_google_models_are_refused_and_the_refusal_says_whose_terms() {
    for (id, provider) in [
        ("anthropic/claude-sonnet-4.5", "Anthropic"),
        ("claude-opus-4.1", "Anthropic"),
        ("claude-sonnet-4.5--fast", "Anthropic"),
        ("openai/claude-sonnet-4.5", "Anthropic"),
        ("google/gemini-2.5-pro", "Google"),
        ("gemini-2.5-flash", "Google"),
        ("xai/gemini-2.5-pro", "Google"),
    ] {
        let why = subscription_model(id).expect_err(id);
        assert!(
            why.contains(&format!("{provider}'s terms forbid")),
            "{id}: {why}"
        );
    }
}

/// AN ALLOWLIST FAILS CLOSED: anything it does not recognise is refused — another provider, a
/// provider prefix on the other's model, a lookalike, an id that is not plain text.
#[test]
fn an_unrecognised_model_is_refused_and_the_refusal_names_what_is_allowed() {
    for id in [
        "llama-3.3-70b",
        "meta/llama-3.3-70b",
        "mistral-large",
        "deepseek-r1",
        "openai/grok-4.7",
        "xai/gpt-5.5",
        "grok4",
        "grokking-1",
        "xai/grok",
        "gpt5",
        "openai/gpt-5/x",
        "gpt-5.5@sub",
        "gpt-5.5\nx",
        "gpt-5.5 extra",
        "--fast",
    ] {
        let why = subscription_model(id).expect_err(id);
        assert!(
            why.contains("OpenAI") && why.contains("xAI"),
            "{id:?}: {why}"
        );
    }
    for blank in ["", "   "] {
        assert_eq!(
            subscription_model(blank).unwrap_err(),
            "name a model the proxy serves"
        );
    }
}

/// EVERY ALLOWED PATTERN IS ANCHORED: an id that only CONTAINS an allowed word is not OpenAI's or
/// xAI's, prefixed, fast or not. Refused as unrecognised, or by name where it names a provider
/// whose terms forbid it.
#[test]
fn an_id_that_only_contains_an_allowed_word_is_refused() {
    for id in [
        "my-codex-thing",
        "notgrok-1",
        "my-codex-thing--fast",
        "openai/my-codex-thing",
        "xai/notgrok-1",
        "xgpt-5.5",
        "turbo-o3",
        "xai/codex-mini-latest",
    ] {
        let why = subscription_model(id).expect_err(id);
        assert!(
            why.contains("is not a model this server knows"),
            "{id}: {why}"
        );
    }
    for (id, provider) in [("claude-codex", "Anthropic"), ("gemini-gpt-4", "Google")] {
        let why = subscription_model(id).expect_err(id);
        assert!(
            why.contains(&format!("{provider}'s terms forbid")),
            "{id}: {why}"
        );
    }
}

/// The turn's own word wins over the account's setting; without one, the account's stands.
#[test]
fn a_turns_own_source_beats_the_accounts() {
    let on_proxy = InferenceSource {
        kind: SourceKind::LocalProxy,
        ..InferenceSource::default()
    };
    let on_gateway = InferenceSource::default();
    assert_eq!(
        on_proxy.for_turn(Some(SourceKind::Gateway)),
        SourceKind::Gateway
    );
    assert_eq!(
        on_gateway.for_turn(Some(SourceKind::LocalProxy)),
        SourceKind::LocalProxy
    );
    assert_eq!(on_proxy.for_turn(None), SourceKind::LocalProxy);
    assert_eq!(on_gateway.for_turn(None), SourceKind::Gateway);
}

/// What a request names is a wire word or nothing: a misspelling is refused, never read as the
/// gateway (which would bill a key the person chose not to use) nor as the proxy.
#[test]
fn a_named_source_is_a_wire_word_or_a_refusal() {
    assert_eq!(SourceKind::named(None), Ok(None));
    assert_eq!(SourceKind::named(Some(&json!(null))), Ok(None));
    assert_eq!(
        SourceKind::named(Some(&json!("gateway"))),
        Ok(Some(SourceKind::Gateway))
    );
    assert_eq!(
        SourceKind::named(Some(&json!("local_proxy"))),
        Ok(Some(SourceKind::LocalProxy))
    );
    for wrong in [
        json!("local-proxy"),
        json!("LOCAL_PROXY"),
        json!(""),
        json!(7),
        json!({}),
    ] {
        assert!(SourceKind::named(Some(&wrong)).is_err(), "{wrong}");
    }
    assert_eq!(
        serde_json::to_value(SourceKind::LocalProxy).unwrap(),
        json!("local_proxy")
    );
}

fn registered() -> Account {
    let mut account = Account::default();
    for event in account
        .decide(AccountCommand::Register {
            email: "ada@og.local".to_string(),
            password_hash: "x".to_string(),
            first_name: "Ada".to_string(),
            last_name: "L".to_string(),
            org_id: String::new(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms: 1,
        })
        .unwrap()
    {
        account.apply(&event);
    }
    account
}

/// The account keeps the whole setting it was last given, and an account that never set one is
/// on the gateway — exactly what every turn did before a person could choose.
#[test]
fn an_account_keeps_the_setting_it_was_given_and_starts_on_the_gateway() {
    let mut account = registered();
    assert_eq!(account.inference_source, InferenceSource::default());
    assert_eq!(account.inference_source.kind, SourceKind::Gateway);
    let setting = InferenceSource {
        kind: SourceKind::LocalProxy,
        base_url: Some("http://127.0.0.1:18080".to_string()),
        local_model: Some("gpt-5.5".to_string()),
        has_key: true,
        via: Some(Via::Mac),
        relay_model: Some("gpt-6-sol".to_string()),
        new_bot_default: Some(NewBotDefault {
            source: SourceKind::LocalProxy,
            model: "gpt-6-sol--fast".to_string(),
            effort: Effort::High,
        }),
        relays: Vec::new(),
        plan_fallback: Some(PlanFallback {
            model: "xai/grok-4.6".to_string(),
            effort: Effort::Low,
        }),
    };
    let read = InferenceSource {
        relays: vec!["mac-1".to_string()],
        ..setting.clone()
    };
    let logged = |source: &InferenceSource| serde_json::to_value(source).unwrap();
    assert_eq!(
        logged(&read),
        logged(&setting),
        "its computers are never logged"
    );
    let events = account
        .decide(AccountCommand::SetInferenceSource {
            source: setting.clone(),
            at_ms: 2,
        })
        .unwrap();
    assert_eq!(events[0].event_type(), "account-inference-source-set");
    for event in &events {
        account.apply(event);
    }
    assert_eq!(account.inference_source, setting);
    let stored = serde_json::to_value(&events[0]).unwrap();
    assert!(
        !stored.to_string().contains("key\":\"") && stored["source"]["has_key"] == json!(true),
        "the log says a key exists, never what it is: {stored}"
    );
    let replayed = Account::replay(&[serde_json::from_value(stored).unwrap()]);
    assert_eq!(replayed.inference_source, setting);

    assert_eq!(
        Account::default()
            .decide(AccountCommand::SetInferenceSource {
                source: setting,
                at_ms: 3
            })
            .unwrap_err(),
        AccountError::NotRegistered
    );
}

/// A run captures where it started, and a log written before the field existed reads as the
/// gateway — the only place any turn went then.
#[test]
fn a_run_keeps_the_source_it_started_on_and_old_logs_read_as_the_gateway() {
    let mut run = Run::default();
    for event in run
        .decide(RunCommand::Start {
            thread_id: "t1".to_string(),
            coworker_id: None,
            model: Some("gpt-5.5".to_string()),
            effort: Default::default(),
            inference_source: SourceKind::LocalProxy.into(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms: 1,
        })
        .unwrap()
    {
        let stored: RunEvent =
            serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        run.apply(&stored);
    }
    assert_eq!(run.inference_source, SourceKind::LocalProxy);

    let old: RunEvent = serde_json::from_value(json!({
        "type": "started", "thread_id": "t1", "coworker_id": null,
        "model": "xai/grok-4.6", "at_ms": 1
    }))
    .unwrap();
    assert_eq!(Run::replay([&old]).inference_source, SourceKind::Gateway);
}

/// A RELAYED RUN CARRIES ON AT THE MAC (#292): the way it went is captured with its kind, and a
/// resume asks there, whatever the account's default by then. A proxy run logged before the relay
/// went by the loopback, the only way there was, so it resumes there and never at a Mac the
/// account has since made its default; a gateway run names no way at all.
#[test]
fn a_run_keeps_the_way_it_went_and_an_old_proxy_log_went_by_the_loopback() {
    let start = |source: TurnSource| {
        let mut run = Run::default();
        let events = run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: Some("gpt-5.5".to_string()),
                effort: Default::default(),
                inference_source: source,
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            })
            .unwrap();
        let logged: Vec<serde_json::Value> = events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();
        for event in &logged {
            run.apply(&serde_json::from_value(event.clone()).unwrap());
        }
        (run, logged)
    };
    let mac = TurnSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
    };
    let (run, logged) = start(mac);
    assert_eq!(run.inference_via, Some(Via::Mac));
    assert_eq!(logged[0]["inference_via"], "mac", "{logged:?}");
    assert_eq!(run.source_for_resume(), mac);

    let loopback = TurnSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Loopback),
    };
    assert_eq!(start(loopback).0.source_for_resume(), loopback);
    // A via named on a gateway turn says nothing, and is not kept.
    let (run, logged) = start(TurnSource {
        kind: SourceKind::Gateway,
        via: Some(Via::Mac),
    });
    assert!(logged[0].get("inference_via").is_none(), "{logged:?}");
    assert_eq!(run.source_for_resume(), SourceKind::Gateway.into());

    let old: RunEvent = serde_json::from_value(json!({
        "type": "started", "thread_id": "t1", "coworker_id": null,
        "model": "gpt-5.5", "inference_source": "local_proxy", "at_ms": 1
    }))
    .unwrap();
    assert_eq!(Run::replay([&old]).source_for_resume(), loopback);
}

/// What a turn or a queued send may name: the kind as a word, or `{kind, via}`. An omitted via is
/// the account's default; `helper` is refused by name until it is built (#293), and anything else
/// is refused rather than read as either way.
#[test]
fn a_turn_names_its_source_as_a_word_or_with_the_way_it_goes() {
    let named = |value: serde_json::Value| TurnSource::named(Some(&value), "inferenceSource");
    let mac = TurnSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
    };
    let loopback = TurnSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Loopback),
    };
    let proxy: TurnSource = SourceKind::LocalProxy.into();
    assert_eq!(named(json!("local_proxy")), Ok(Some(proxy)));
    assert_eq!(
        named(json!({"kind": "local_proxy", "via": "mac"})),
        Ok(Some(mac))
    );
    assert_eq!(named(json!({"kind": "local_proxy"})), Ok(Some(proxy)));
    assert_eq!(
        named(json!({"kind": "local_proxy", "via": ""})),
        Ok(Some(proxy))
    );
    assert_eq!(
        named(json!({"kind": "gateway", "via": null})),
        Ok(Some(SourceKind::Gateway.into()))
    );
    assert_eq!(TurnSource::named(None, "inferenceSource"), Ok(None));
    assert_eq!(named(json!(null)), Ok(None));
    let helper = named(json!({"kind": "local_proxy", "via": "helper"})).unwrap_err();
    assert!(
        helper.starts_with("inferenceSource.via \"helper\" is not built yet"),
        "{helper}"
    );
    for refused in [
        json!({"kind": "local-proxy", "via": "mac"}),
        json!({"via": "mac"}),
        json!({"kind": "local_proxy", "via": "Mac"}),
        json!({"kind": "local_proxy", "via": 1}),
        json!(7),
    ] {
        let why = named(refused.clone()).unwrap_err();
        assert!(why.starts_with("inferenceSource"), "{refused}: {why}");
    }
    assert_eq!(
        named(json!("local-proxy")).unwrap_err(),
        "inferenceSource must be \"gateway\" or \"local_proxy\"",
        "the word alone is refused as it always was"
    );

    // A queued send keeps what it named, and a row from before the relay reads as its word.
    for source in [SourceKind::Gateway.into(), proxy, mac, loopback] {
        assert_eq!(TurnSource::from_stored(&source.stored()), Some(source));
        assert_eq!(named(source.to_value()), Ok(Some(source)));
    }
    assert_eq!(proxy.to_value(), json!("local_proxy"));
    assert_eq!(TurnSource::from_stored("local_proxy:helper"), None);
    assert_eq!(TurnSource::from_stored("proxy"), None);

    // Each word wins on its own over the setting: the kind, and the way.
    let setting = InferenceSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
        ..Default::default()
    };
    assert_eq!(setting.resolve(None), (SourceKind::LocalProxy, Via::Mac));
    assert_eq!(
        setting.resolve(Some(loopback)),
        (SourceKind::LocalProxy, Via::Loopback)
    );
    assert_eq!(
        setting.resolve(Some(proxy)),
        (SourceKind::LocalProxy, Via::Mac)
    );
    assert_eq!(
        InferenceSource::default().resolve(Some(proxy)),
        (SourceKind::LocalProxy, Via::Loopback),
        "no default way is the loopback"
    );
}

/// A setting logged before a default for new bots existed reads as none, and a setting without one
/// is logged as it was before, so neither an old log nor an older replica sees anything new.
#[test]
fn a_setting_from_before_the_default_for_new_bots_reads_as_none() {
    let logged = json!({ "type": "inference-source-set", "at_ms": 2,
                         "source": { "kind": "local_proxy", "base_url": "http://127.0.0.1:9",
                                     "local_model": "gpt-5.5", "has_key": false } });
    let event = serde_json::from_value(logged).expect("an event from before");
    let account = Account::replay(&[event]);
    assert_eq!(account.inference_source.new_bot_default, None);
    let unset = serde_json::to_value(InferenceSource::default()).unwrap();
    assert!(unset.get("new_bot_default").is_none(), "{unset}");
}

/// A default is named whole or refused in the words its parts are refused in elsewhere: on the
/// gateway its model as a hire's pin is, on the person's plan as a `localModel` is, its effort as
/// a coworker's. Null is none, and an effort left out is inherit.
#[test]
fn a_default_for_new_bots_is_named_whole_or_refused_in_its_parts_words() {
    let named = |value| NewBotDefault::named(&value);
    assert_eq!(named(json!(null)), Ok(None));
    let gateway = named(json!({ "source": "gateway", "model": " xai/grok-4.6 " }));
    let gateway = gateway.unwrap().unwrap();
    assert_eq!(gateway.model, "xai/grok-4.6", "trimmed, as a pin is");
    assert_eq!(gateway.effort, Effort::Inherit);
    let fast = json!({ "source": "local_proxy", "model": "gpt-6-sol--fast", "effort": "xhigh" });
    let fast = named(fast).unwrap().unwrap();
    assert_eq!(
        (fast.source, fast.effort),
        (SourceKind::LocalProxy, Effort::XHigh)
    );
    let plan_words = subscription_model("xai/grok-4.6@sub").unwrap_err();
    assert_eq!(
        named(json!({ "source": "local_proxy", "model": "xai/grok-4.6@sub" })),
        Err(format!("newBotDefault.model: {plan_words}"))
    );
    assert_eq!(
        named(json!({ "source": "gateway", "model": "" })),
        Err("newBotDefault.model: a coworker needs a model to think with".to_string())
    );
    let off = named(json!({ "source": "gateway", "model": "m", "effort": "none" }));
    assert_eq!(off.unwrap().unwrap().effort, Effort::Off);
    let loud = named(json!({ "source": "gateway", "model": "m", "effort": "loud" }));
    assert!(
        loud.unwrap_err()
            .starts_with("newBotDefault.effort must be one of inherit, none")
    );
    let stored = serde_json::to_value(&fast).unwrap();
    let wire = json!({ "source": "local_proxy", "model": "gpt-6-sol--fast", "effort": "xhigh" });
    assert_eq!(stored, wire, "the log and the wire spell it alike");
}

/// AN EVENT FROM BEFORE EACH COMPUTER HAD ITS SWITCH STILL REPLAYS, #332's account-wide one in it
/// unread, with no fallback: whether a turn goes by the Mac is the person's computers' now
/// (`relays`, read beside the setting and never logged), so the same setting goes by the Mac with
/// one of them on and not with none, whatever was logged; one that never set either logs neither.
#[test]
fn a_setting_logged_with_the_account_wide_switch_replays_with_it_unread() {
    for off in [None, Some(true), Some(false)] {
        let mut source = json!({ "kind": "local_proxy", "via": "mac", "has_key": false });
        if let Some(off) = off {
            source["relay_off"] = json!(off);
        }
        let logged = json!({ "type": "inference-source-set", "at_ms": 2, "source": source });
        let event = serde_json::from_value(logged).expect("an event from before");
        let mut setting = Account::replay(&[event]).inference_source;
        assert_eq!((setting.relays.len(), &setting.plan_fallback), (0, &None));
        assert!(
            !setting.by_mac(None, None),
            "{off:?}: none on, not by the Mac"
        );
        setting.relays = vec!["mac-1".to_string()];
        assert!(setting.by_mac(None, None), "{off:?}: one on, by the Mac");
    }
    let unset = serde_json::to_value(InferenceSource::default()).unwrap();
    let logs = |key: &str| unset.get(key).is_some();
    assert!(
        !logs("relay_off") && !logs("relays") && !logs("plan_fallback"),
        "{unset}"
    );
}

/// A FALLBACK IS NAMED WHOLE OR REFUSED in its parts' words: its model as a hire's pin is on the
/// gateway, its effort as a coworker's. Null is none, and an effort left out is inherit.
#[test]
fn a_plan_fallback_is_named_whole_or_refused_in_its_parts_words() {
    let named = |value| PlanFallback::named(&value);
    assert_eq!(named(json!(null)), Ok(None));
    let grok = named(json!({ "model": " xai/grok-4.6 " }))
        .unwrap()
        .unwrap();
    assert_eq!(
        (grok.model.as_str(), grok.effort),
        ("xai/grok-4.6", Effort::Inherit)
    );
    let high = named(json!({ "model": "oag/cheap", "effort": "high" }))
        .unwrap()
        .unwrap();
    assert_eq!(high.effort, Effort::High);
    assert_eq!(
        named(json!({ "model": "  " })),
        Err("planFallback.model: a coworker needs a model to think with".to_string())
    );
    let loud = named(json!({ "model": "m", "effort": "loud" })).unwrap_err();
    assert!(
        loud.starts_with("planFallback.effort must be one of inherit, none"),
        "{loud}"
    );
    let refused = Err("planFallback must be an object or null".to_string());
    assert_eq!(named(json!("xai/grok-4.6")), refused);
    let stored = serde_json::to_value(&high).unwrap();
    assert_eq!(
        stored,
        json!({ "model": "oag/cheap", "effort": "high" }),
        "log and wire alike"
    );
}

/// THE SWITCH MOVES NO WAY (#332): `relayEnabled` is read out of the body for the person's
/// computers, and leaves `via` as it was, which only a `via` in the body changes, and the setting's
/// own computers as they were read; a fallback absent is kept and null clears it. A body refused
/// in any part saves none of itself.
#[test]
fn switching_the_relay_moves_no_way_and_a_fallback_is_kept_until_cleared() {
    let loopback = |raw: &str| Ok(raw.to_string());
    let by_mac = InferenceSource {
        kind: SourceKind::LocalProxy,
        via: Some(Via::Mac),
        relays: vec!["mac-1".to_string()],
        ..Default::default()
    };
    let apply = |current: &InferenceSource, body| {
        let (setting, key, switched) = current.applied(&body, loopback).expect("applied");
        assert!(matches!(key, KeyChange::Keep));
        (setting, switched)
    };
    let fallback = json!({ "model": "xai/grok-4.6", "effort": "low" });
    let body = json!({ "kind": "local_proxy", "relayEnabled": false, "planFallback": fallback });
    let (off, switched) = apply(&by_mac, body);
    assert_eq!(
        (switched, off.via),
        (Some(false), Some(Via::Mac)),
        "no way moved"
    );
    let kept = PlanFallback::named(&fallback).unwrap();
    assert_eq!((&off.plan_fallback, &off.relays), (&kept, &by_mac.relays));

    let (on, switched) = apply(&off, json!({ "kind": "local_proxy", "relayEnabled": true }));
    assert_eq!(
        (switched, on.via, &on.plan_fallback),
        (Some(true), Some(Via::Mac), &kept)
    );
    let (loopback_way, switched) = apply(&on, json!({ "kind": "local_proxy", "via": "loopback" }));
    assert_eq!(
        (switched, loopback_way.via),
        (None, Some(Via::Loopback)),
        "left out, nothing is switched"
    );
    let (cleared, _) = apply(&off, json!({ "kind": "local_proxy", "planFallback": null }));
    assert_eq!(cleared.plan_fallback, None);

    for (body, why) in [
        (
            json!({ "kind": "local_proxy", "relayEnabled": "no" }),
            "relayEnabled must be true or false",
        ),
        (
            json!({ "kind": "local_proxy", "relayEnabled": null }),
            "relayEnabled must be true or false",
        ),
        (
            json!({ "kind": "local_proxy", "relayEnabled": false, "planFallback": { "model": "" } }),
            "planFallback.model: a coworker needs a model to think with",
        ),
    ] {
        let refused = by_mac.applied(&body, loopback).map(|_| ()).unwrap_err();
        assert_eq!(refused, why, "{body}");
    }
}

/// A person's time zone is an IANA name or nothing: an unknown one is refused before it is logged,
/// the same one again is no event, and none clears it. An account that never set one has none.
#[test]
fn an_account_keeps_an_iana_time_zone_and_refuses_any_other() {
    let mut account = registered();
    assert_eq!(account.time_zone, None);
    let set = |zone: Option<&str>| AccountCommand::SetTimeZone {
        time_zone: zone.map(str::to_string),
        at_ms: 2,
    };
    let events = account.decide(set(Some(" Asia/Manila "))).unwrap();
    assert_eq!(events[0].event_type(), "account-time-zone-set");
    for event in &events {
        account.apply(event);
    }
    assert_eq!(account.time_zone.as_deref(), Some("Asia/Manila"));
    assert_eq!(account.decide(set(Some("Asia/Manila"))), Ok(Vec::new()));
    for unknown in ["Mars/Olympus_Mons", "asia/manila", "", "+08:00"] {
        assert_eq!(
            account.decide(set(Some(unknown))),
            Err(AccountError::UnknownTimeZone(unknown.to_string())),
            "{unknown:?}"
        );
    }
    let why = AccountError::UnknownTimeZone("Mars/Olympus_Mons".to_string()).to_string();
    assert!(
        why.contains("IANA") && why.contains("Europe/London"),
        "{why}"
    );
    for event in &account.decide(set(None)).unwrap() {
        account.apply(event);
    }
    assert_eq!(account.time_zone, None, "none clears it");
    assert_eq!(
        Account::default().decide(set(Some("UTC"))),
        Err(AccountError::NotRegistered)
    );
}
