use super::*;

#[test]
fn ids_are_read_from_an_openai_shaped_listing() {
    let body = r#"{"object":"list","data":[{"id":"oag/auto"},{"id":"openai/gpt-5.5"}]}"#;
    let models = parse_models(body);
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "oag/auto");
    assert_eq!(models[1].id, "openai/gpt-5.5");
}

/// A body we did not expect yields nothing — never a guessed id a person could pin to.
#[test]
fn an_unexpected_body_yields_no_models_rather_than_a_guess() {
    assert!(parse_models("not json").is_empty());
    assert!(parse_models(r#"{"error":"nope"}"#).is_empty());
    assert!(parse_models(r#"{"data":"not an array"}"#).is_empty());
}

/// The reason `probe` may pass a gateway sentence on at all: anything key-shaped in it does
/// not travel. A gateway that echoes the failed request would otherwise leak our credential
/// through us — the one thing this module exists to prevent.
#[test]
fn a_gateway_sentence_travels_but_a_credential_in_it_does_not() {
    let leaked =
        redact_secrets("invalid Authorization header: Bearer oag_live_deadbeefdeadbeefdeadbeef");
    assert!(
        !leaked.contains("oag_live_deadbeefdeadbeefdeadbeef"),
        "{leaked}"
    );
    assert!(leaked.contains("«redacted»"), "{leaked}");
    assert!(leaked.contains("invalid Authorization header"), "{leaked}");

    // Built rather than written: a key-shaped literal in source trips secret scanners, and a
    // test about redaction should not be the thing that looks like a leak.
    let openai_shaped = format!("sk-{}", "abcdefghijklmnopqrstuv");
    let scrubbed = redact_secrets(&format!("key {openai_shaped}"));
    assert!(!scrubbed.contains(&openai_shaped), "{scrubbed}");

    // The useful case is untouched — this is the sentence the whole feature turns on.
    let real = "no credential available for provider anthropic on this route";
    assert_eq!(redact_secrets(real), real);
}

/// A gateway that dumps a whole request cannot dump it into a browser.
#[test]
fn an_enormous_detail_is_clipped() {
    let flood = "word ".repeat(400);
    let scrubbed = redact_secrets(&flood);
    assert!(
        scrubbed.chars().count() <= DETAIL_CLIP + 16,
        "{}",
        scrubbed.len()
    );
    assert!(scrubbed.ends_with("(clipped)"));
}

/// One person clicking Test is fine; a loop spending the deployment's money is not.
#[test]
fn an_account_cannot_probe_in_a_loop() {
    let catalogue = ModelCatalogue::new("http://gateway.local", "oag_live_x");
    assert!(catalogue.may_probe("acct_1"), "the first probe is allowed");
    assert!(!catalogue.may_probe("acct_1"), "an immediate second is not");
    assert!(
        catalogue.may_probe("acct_2"),
        "one account's limit is not another's"
    );
}

#[test]
fn debug_never_prints_the_key() {
    let catalogue = ModelCatalogue::new("http://gateway.local:29080", "oag_live_supersecret");
    let rendered = format!("{catalogue:?}");
    assert!(!rendered.contains("supersecret"), "{rendered}");
    assert!(rendered.contains("«redacted»"), "{rendered}");
}

/// The gateway's own window, per entry; null where it has none (a virtual route).
#[test]
fn the_context_window_is_read_from_the_oag_block() {
    let body = r#"{"data":[
        {"id":"oag/auto","oag":{"virtual":true,"context_window":null,"alias_of":null}},
        {"id":"xai/grok-4.6@sub","oag":{"context_window":200000,"alias_of":"xai/grok-4.6"}},
        {"id":"openai/gpt-5.5"}
    ]}"#;
    let models = parse_models(body);
    assert_eq!(models[0].context_window, None);
    assert_eq!(models[1].context_window, Some(200_000));
    assert_eq!(models[1].alias_of.as_deref(), Some("xai/grok-4.6"));
    assert_eq!(
        models[2].context_window, None,
        "no oag block is no window, not a guess"
    );
}

/// A pin finds its window on its own entry, then on the entry it is a channel of, then on the
/// same model under another channel. A virtual entry's null is final.
#[test]
fn a_pin_finds_its_window_through_aliases_and_channels() {
    let model = |id: &str, window: Option<u64>, alias: Option<&str>| Model {
        id: id.to_string(),
        context_window: window,
        alias_of: alias.map(str::to_string),
    };
    let models = vec![
        model("oag/auto", None, None),
        model("xai/grok-4.6@sub", Some(200_000), Some("xai/grok-4.6")),
        model("openai/gpt-5.5@api", Some(400_000), None),
    ];
    assert_eq!(context_of(&models, "xai/grok-4.6@sub"), Some(200_000));
    assert_eq!(
        context_of(&models, "xai/grok-4.6"),
        Some(200_000),
        "the shipping pin, unlisted"
    );
    assert_eq!(context_of(&models, "openai/gpt-5.5@sub"), Some(400_000));
    assert_eq!(context_of(&models, "oag/auto"), None);
    assert_eq!(context_of(&models, "anthropic/opus"), None);
}

/// No catalogue (the mock doors) is the setting; a setting of none is no guard at all.
#[tokio::test]
async fn without_a_catalogue_the_setting_decides() {
    assert_eq!(context_for(None, Some(64_000), "any").await, Some(64_000));
    assert_eq!(context_for(None, None, "any").await, None);
}

/// A gateway that cannot be reached costs a turn one short lookup, and falls back.
#[tokio::test]
async fn an_unreachable_catalogue_falls_back_to_the_setting() {
    let catalogue = ModelCatalogue::new("http://127.0.0.1:9", "oag_live_x");
    let started = Instant::now();
    assert_eq!(
        context_for(Some(&catalogue), Some(64_000), "xai/grok-4.6").await,
        Some(64_000)
    );
    // The second turn inside the minute does not ask again.
    assert_eq!(
        context_for(Some(&catalogue), Some(64_000), "xai/grok-4.6").await,
        Some(64_000)
    );
    assert!(
        started.elapsed() < CONTEXT_LOOKUP * 2,
        "{:?}",
        started.elapsed()
    );
}
