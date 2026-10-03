//! A model's own reasoning levels (`opengrok_core::catalogue`): read from a `/v1/models` row the
//! same way whichever door listed it, put on a `GET /models` row as `efforts` and `ownEffort`, and
//! the one rule a write's effort is held to. The rows are opencodex 2.75.0's own, captured from
//! its `/v1/models` on 127.0.0.1:8080 on 3 Oct 2026 (trimmed of fields nothing here reads).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use opengrok_core::catalogue::{Level, Levels, Model, models, models_in, refusal};
use opengrok_core::coworker::Effort;
use opengrok_core::inference::{SourceKind, Via};
use serde_json::{Value, json};

fn captured() -> Value {
    json!({ "object": "list", "data": [
        { "id": "gpt-6-luna", "object": "model", "created": 0, "owned_by": "openai",
          "supports_reasoning_effort": true, "reasoning_effort": "medium",
          "reasoning_efforts": [
              { "value": "low", "label": "Low Effort" },
              { "value": "medium", "label": "Medium Effort", "default": true },
              { "value": "high", "label": "High Effort" },
              { "value": "xhigh", "label": "Xhigh Effort" },
              { "value": "max", "label": "Max Effort" } ],
          "context_window": 872000 },
        { "id": "xai/grok-4.6", "object": "model", "created": 0, "owned_by": "xai" }
    ]})
}

fn levels(values: &[&str], own: Option<&str>) -> Levels {
    let level = |value: &&str| Level {
        value: value.to_string(),
        label: format!("{}{} Effort", value[..1].to_uppercase(), &value[1..]),
    };
    Levels {
        efforts: Some(values.iter().map(level).collect()),
        own: own.map(str::to_string),
    }
}

fn row(levels: Value) -> Levels {
    Levels::of(&levels)
}

#[test]
fn a_model_with_levels_has_its_own_and_one_without_has_none() {
    let listed = models(&captured());
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "gpt-6-luna");
    assert_eq!(
        listed[0].levels,
        levels(&["low", "medium", "high", "xhigh", "max"], Some("medium")),
        "low to high as listed, `default` dropped, the named level its own"
    );
    assert_eq!(
        listed[0].context_window, None,
        "opencodex's own window is not the gateway's `oag` one"
    );
    assert_eq!(listed[1].id, "xai/grok-4.6");
    assert_eq!(
        listed[1].levels,
        Levels::default(),
        "nothing published, nothing inferred"
    );
}

#[test]
fn its_own_level_is_the_entry_marked_default_when_none_is_named() {
    let entries = json!([{ "value": "low", "label": "Low Effort" },
                         { "value": "high", "label": "High Effort", "default": true }]);
    let marked = row(json!({ "id": "m", "reasoning_efforts": entries }));
    assert_eq!(marked, levels(&["low", "high"], Some("high")));
    let named = row(json!({ "id": "m", "reasoning_efforts": entries, "reasoning_effort": "low" }));
    assert_eq!(named.own.as_deref(), Some("low"), "the named level wins");
    let unmarked =
        row(json!({ "id": "m", "reasoning_efforts": [{ "value": "low", "label": "L" }] }));
    assert_eq!(unmarked.own, None, "and with neither there is none");
    let alone = row(json!({ "id": "m", "reasoning_effort": "medium" }));
    assert_eq!(
        alone.efforts, None,
        "a level named alone is no list to choose from"
    );
    assert_eq!(alone.own.as_deref(), Some("medium"));
}

#[test]
fn a_model_that_says_it_takes_no_effort_or_lists_none_has_none() {
    let refused = row(json!({ "id": "m", "supports_reasoning_effort": false,
        "reasoning_effort": "low", "reasoning_efforts": [{ "value": "low", "label": "Low" }] }));
    assert_eq!(refused, Levels::default());
    let empty = row(json!({ "id": "m", "supports_reasoning_effort": true,
                            "reasoning_efforts": [] }));
    assert_eq!(empty, Levels::default());
    let entries = json!([{ "label": "No value" }, "high", { "value": " ", "label": "Blank" },
                         { "value": "max" }, { "value": "xhigh", "label": "" }]);
    let odd = row(json!({ "id": "m", "reasoning_efforts": entries, "reasoning_effort": "" }));
    let level = |word: &str| Level {
        value: word.to_string(),
        label: word.to_string(),
    };
    assert_eq!(
        odd,
        Levels {
            efforts: Some(vec![level("max"), level("xhigh")]),
            own: None
        },
        "an entry with no value is no level, one with no label is shown as its value, and blank \
         is no word"
    );
}

#[test]
fn a_gateway_row_keeps_its_window_beside_its_levels() {
    let body = json!({ "data": [
        { "id": "openai/gpt-6-luna@sub", "oag": { "context_window": 400000,
                                                  "alias_of": "openai/gpt-6-luna" },
          "supports_reasoning_effort": true, "reasoning_effort": "high",
          "reasoning_efforts": [{ "value": "low", "label": "Low Effort" },
                                { "value": "high", "label": "High Effort", "default": true }] },
        { "id": "oag/auto", "oag": { "virtual": true, "context_window": null } }
    ]});
    let listed = models(&body);
    assert_eq!(listed[0].context_window, Some(400_000));
    assert_eq!(listed[0].alias_of.as_deref(), Some("openai/gpt-6-luna"));
    assert_eq!(listed[0].levels, levels(&["low", "high"], Some("high")));
    assert_eq!(listed[1].levels, Levels::default(), "absent is null");
    assert!(models(&json!({ "data": "not a list" })).is_empty());
}

#[test]
fn each_row_is_the_wire_agreed_with_nativechat() {
    let listed = models(&captured());
    let plan = listed[0].entry(Value::Null, SourceKind::LocalProxy, Some(Via::Loopback));
    assert_eq!(
        plan,
        json!({ "id": "gpt-6-luna", "points": null, "source": "local_proxy", "via": "loopback",
                "efforts": [{ "value": "low", "label": "Low Effort" },
                            { "value": "medium", "label": "Medium Effort" },
                            { "value": "high", "label": "High Effort" },
                            { "value": "xhigh", "label": "Xhigh Effort" },
                            { "value": "max", "label": "Max Effort" }],
                "ownEffort": "medium" })
    );
    let points = json!({ "inputX": 1.0 });
    let gateway = listed[1].entry(points.clone(), SourceKind::Gateway, None);
    assert_eq!(
        gateway,
        json!({ "id": "xai/grok-4.6", "points": points, "source": "gateway",
                "efforts": null, "ownEffort": null }),
        "a gateway row has no `via`, and a model with no levels says so with nulls"
    );
}

fn listing(id: &str, levels: Levels) -> Model {
    let id = id.to_string();
    Model {
        id,
        levels,
        ..Model::default()
    }
}

#[test]
fn an_effort_its_model_does_not_list_is_refused_naming_the_levels_it_does() {
    let luna = levels(&["low", "medium", "high", "xhigh", "max"], Some("medium"));
    let rows = [
        listing("gpt-6-luna", luna),
        listing("m", levels(&["low"], None)),
    ];
    assert_eq!(
        refusal("gpt-6-luna", Effort::Off, &rows).as_deref(),
        Some("gpt-6-luna takes low, medium, high, xhigh or max, not \"none\"")
    );
    assert_eq!(
        refusal("m", Effort::Max, &rows).as_deref(),
        Some("m takes low, not \"max\"")
    );
}

#[test]
fn inherit_a_listed_level_and_a_model_with_no_levels_are_never_refused() {
    let luna = levels(&["low", "medium", "high", "xhigh", "max"], Some("medium"));
    let rows = [
        listing("gpt-6-luna", luna),
        listing("xai/grok-4.6", Levels::default()),
    ];
    let refused = |model, effort| refusal(model, effort, &rows);
    assert_eq!(
        refused("gpt-6-luna", Effort::Inherit),
        None,
        "its own level"
    );
    assert_eq!(refused("gpt-6-luna", Effort::Max), None);
    assert_eq!(refused("xai/grok-4.6", Effort::Off), None);
    assert_eq!(refused("unlisted", Effort::Off), None, "nothing known");
    assert_eq!(refusal("gpt-6-luna", Effort::Off, &[]), None, "no listing");
}

/// The same id on two doors (a person's proxy and their Mac): a level either lists is taken, a
/// refusal names every level any of them lists, once each, and another model's are not its own.
#[test]
fn a_level_any_row_of_the_model_lists_is_taken() {
    let rows = [
        listing("m", levels(&["low", "medium"], None)),
        listing("m", Levels::default()),
        listing("openai/m", levels(&["max"], None)),
        listing("m", levels(&["medium", "high"], None)),
    ];
    assert_eq!(refusal("m", Effort::High, &rows), None);
    assert_eq!(
        refusal("m", Effort::Max, &rows).as_deref(),
        Some("m takes low, medium or high, not \"max\"")
    );
}

/// `ultra` alone needs a row that names it: no gateway reads the word, so where nothing is known
/// it is refused, in words of its own, while every other word is taken there as before.
#[test]
fn ultra_needs_a_row_that_names_it() {
    let sol = levels(&["low", "medium", "high", "xhigh", "max", "ultra"], None);
    let luna = levels(&["low", "medium", "high", "xhigh", "max"], None);
    let rows = [
        listing("gpt-6-sol", sol),
        listing("gpt-6-luna", luna),
        listing("bare", Levels::default()),
    ];
    assert_eq!(refusal("gpt-6-sol", Effort::Ultra, &rows), None);
    assert_eq!(
        refusal("gpt-6-luna", Effort::Ultra, &rows).as_deref(),
        Some("gpt-6-luna takes low, medium, high, xhigh or max, not \"ultra\"")
    );
    let unknown = "no listing of bare names \"ultra\", and it is taken only where one does";
    assert_eq!(
        refusal("bare", Effort::Ultra, &rows).as_deref(),
        Some(unknown)
    );
    assert_eq!(refusal("bare", Effort::Max, &rows), None, "any other word");
    assert!(
        refusal("unlisted", Effort::Ultra, &[]).is_some(),
        "nothing read"
    );
}

#[test]
fn a_row_that_names_only_its_id_is_that_and_nothing_more() {
    let bare = models(&json!({ "data": [{ "id": "bare" }] }));
    assert_eq!(bare, [listing("bare", Levels::default())]);
    assert_eq!(models_in(r#"{"data": [{"id": "bare"}]}"#), bare);
    assert!(models_in("not json").is_empty(), "never a guess");
}
