//! Where a person's turns are answered (`opengrok_core::inference`): which models their own
//! subscription may run, which source a turn opens on, and that a run keeps the one it started on.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use opengrok_core::account::{Account, AccountCommand, AccountError, Plan};
use opengrok_core::inference::{InferenceSource, SourceKind, subscription_model};
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
    };
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
            inference_source: SourceKind::LocalProxy,
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
