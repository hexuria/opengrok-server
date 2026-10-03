use super::*;

fn row(
    kind: &str,
    enabled: Option<bool>,
    allow: Option<&str>,
    ask: Option<&str>,
    block: Option<&str>,
) -> AutoReviewRow {
    AutoReviewRow {
        scope_kind: kind.to_string(),
        scope_id: String::new(),
        enabled,
        allow_instructions: allow.map(str::to_string),
        ask_instructions: ask.map(str::to_string),
        block_instructions: block.map(str::to_string),
        updated_at_ms: 0,
    }
}

#[test]
fn nothing_written_is_off_and_inactive() {
    let effective = resolve(None, None);
    assert!(!effective.enabled);
    assert!(!effective.is_active());
    assert_eq!(effective.decided_by.enabled, DecidedBy::Default);
    assert_eq!(effective.decided_by.ask_instructions, DecidedBy::Default);
}

#[test]
fn precedence_is_per_field_coworker_over_global() {
    let global = row(
        "global",
        Some(true),
        Some("g-allow"),
        Some("g-ask"),
        Some("g-block"),
    );
    let coworker = row("coworker", Some(false), Some("c-allow"), None, None);
    let effective = resolve(Some(&global), Some(&coworker));
    // enabled: coworker said false — it wins even though global said true.
    assert!(!effective.enabled);
    assert_eq!(effective.decided_by.enabled, DecidedBy::Coworker);
    // allow: coworker overrides global.
    assert_eq!(effective.allow_instructions, "c-allow");
    assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Coworker);
    // ask and block: coworker inherits; only global wrote them.
    assert_eq!(effective.ask_instructions, "g-ask");
    assert_eq!(effective.decided_by.ask_instructions, DecidedBy::Global);
    assert_eq!(effective.block_instructions, "g-block");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Global);
}

/// The global ask-first list applies to a Bot that wrote none; a Bot's own list replaces it
/// whole (no merge); and a Bot that wrote nothing for the ask-first list but something for the
/// others still inherits it, because every field answers for itself.
#[test]
fn the_ask_first_list_inherits_overrides_and_never_merges() {
    let global = row("global", Some(true), None, Some("g-ask"), None);
    let bare = row("coworker", None, Some("c-allow"), None, Some("c-block"));
    let effective = resolve(Some(&global), Some(&bare));
    assert_eq!(effective.ask_instructions, "g-ask");
    assert_eq!(effective.decided_by.ask_instructions, DecidedBy::Global);
    assert_eq!(effective.block_instructions, "c-block");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Coworker);

    let own = row("coworker", None, None, Some("c-ask"), None);
    let effective = resolve(Some(&global), Some(&own));
    assert_eq!(effective.ask_instructions, "c-ask");
    assert_eq!(effective.decided_by.ask_instructions, DecidedBy::Coworker);

    // With no row of its own at all, the Bot gets global's, and nothing from a tier that is absent.
    let effective = resolve(Some(&global), None);
    assert_eq!(effective.ask_instructions, "g-ask");
    assert_eq!(resolve(None, Some(&own)).ask_instructions, "c-ask");
    assert_eq!(resolve(None, None).ask_instructions, "");
}

#[test]
fn an_explicit_empty_string_stops_inheritance() {
    // The user cleared this coworker's rules on purpose; global's must not leak back in.
    let global = row("global", Some(true), None, Some("g-ask"), Some("g-block"));
    let coworker = row("coworker", None, None, Some(""), Some(""));
    let effective = resolve(Some(&global), Some(&coworker));
    assert_eq!(effective.ask_instructions, "");
    assert_eq!(effective.decided_by.ask_instructions, DecidedBy::Coworker);
    assert_eq!(effective.block_instructions, "");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Coworker);
}

#[test]
fn short_circuit_needs_enabled_and_at_least_one_instruction() {
    let on_but_empty = row("global", Some(true), Some(""), None, None);
    assert!(!resolve(Some(&on_but_empty), None).is_active());
    let off_with_rules = row("global", Some(false), Some("x"), Some("z"), Some("y"));
    assert!(!resolve(Some(&off_with_rules), None).is_active());
    let on_with_block = row("global", Some(true), None, None, Some("never touch prod"));
    assert!(resolve(Some(&on_with_block), None).is_active());
    // An ask-first list alone is enough to need a judge.
    let on_with_ask = row("global", Some(true), None, Some("check first"), None);
    assert!(resolve(Some(&on_with_ask), None).is_active());
}

/// What the executor carries is the three lists as resolved, and nothing at all when off or empty.
#[test]
fn the_executor_is_handed_all_three_lists_or_nothing() {
    let global = row(
        "global",
        Some(true),
        Some("g-allow"),
        Some("g-ask"),
        Some("g-block"),
    );
    let policy = resolve(Some(&global), None).review_policy();
    assert_eq!(
        policy,
        Some(opengrok_tools::ReviewPolicy {
            allow_instructions: "g-allow".to_string(),
            ask_instructions: "g-ask".to_string(),
            block_instructions: "g-block".to_string(),
        })
    );
    let off = row("coworker", Some(false), None, None, None);
    assert_eq!(resolve(Some(&global), Some(&off)).review_policy(), None);
    assert_eq!(resolve(None, None).review_policy(), None);
}

/// The wire shape of `GET /auto-review/effective`: the ask-first list and its decider sit beside
/// the other two, in the same nesting, with the same words for a tier.
#[test]
fn the_effective_policy_serialises_the_ask_tier_and_who_decided_it() {
    let global = row("global", Some(true), None, Some("check first"), None);
    let coworker = row("coworker", None, Some("git is fine"), None, None);
    let effective = resolve(Some(&global), Some(&coworker));
    assert_eq!(
        serde_json::to_value(&effective).ok(),
        Some(serde_json::json!({
            "enabled": true,
            "allowInstructions": "git is fine",
            "askInstructions": "check first",
            "blockInstructions": "",
            "decidedBy": {
                "enabled": "global",
                "allowInstructions": "coworker",
                "askInstructions": "global",
                "blockInstructions": "default",
            },
        }))
    );
}

/// The wire shape of a row in `GET /auto-review/policy`: nulls stay null (they mean "inherits").
#[test]
fn a_stored_row_shows_its_ask_first_list_and_null_means_inherits() {
    let stored = row("global", Some(true), None, Some("check first"), Some(""));
    assert_eq!(
        row_json(&stored),
        serde_json::json!({
            "enabled": true,
            "allowInstructions": null,
            "askInstructions": "check first",
            "blockInstructions": "",
            "updatedAtMs": 0,
        })
    );
    assert_eq!(
        row_json(&row("coworker", None, None, None, None))["askInstructions"],
        serde_json::Value::Null
    );
}

#[test]
fn the_only_scopes_are_global_and_coworker() {
    // A device tier would be a second answer to "what on this machine" — the standing rules
    // already answer it. If someone re-adds it, this is the test that asks them why.
    assert_eq!(SCOPE_KINDS, &["global", "coworker"]);
}
