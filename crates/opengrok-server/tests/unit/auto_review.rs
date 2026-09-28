use super::*;

fn row(
    kind: &str,
    enabled: Option<bool>,
    allow: Option<&str>,
    block: Option<&str>,
) -> AutoReviewRow {
    AutoReviewRow {
        scope_kind: kind.to_string(),
        scope_id: String::new(),
        enabled,
        allow_instructions: allow.map(str::to_string),
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
}

#[test]
fn precedence_is_per_field_coworker_over_global() {
    let global = row("global", Some(true), Some("g-allow"), Some("g-block"));
    let coworker = row("coworker", Some(false), Some("c-allow"), None);
    let effective = resolve(Some(&global), Some(&coworker));
    // enabled: coworker said false — it wins even though global said true.
    assert!(!effective.enabled);
    assert_eq!(effective.decided_by.enabled, DecidedBy::Coworker);
    // allow: coworker overrides global.
    assert_eq!(effective.allow_instructions, "c-allow");
    assert_eq!(effective.decided_by.allow_instructions, DecidedBy::Coworker);
    // block: coworker inherits; only global wrote one.
    assert_eq!(effective.block_instructions, "g-block");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Global);
}

#[test]
fn an_explicit_empty_string_stops_inheritance() {
    // The user cleared this coworker's block rules on purpose; global's must not leak back in.
    let global = row("global", Some(true), None, Some("g-block"));
    let coworker = row("coworker", None, None, Some(""));
    let effective = resolve(Some(&global), Some(&coworker));
    assert_eq!(effective.block_instructions, "");
    assert_eq!(effective.decided_by.block_instructions, DecidedBy::Coworker);
}

#[test]
fn short_circuit_needs_enabled_and_at_least_one_instruction() {
    let on_but_empty = row("global", Some(true), Some(""), None);
    assert!(!resolve(Some(&on_but_empty), None).is_active());
    let off_with_rules = row("global", Some(false), Some("x"), Some("y"));
    assert!(!resolve(Some(&off_with_rules), None).is_active());
    let on_with_block = row("global", Some(true), None, Some("never touch prod"));
    assert!(resolve(Some(&on_with_block), None).is_active());
}

#[test]
fn the_only_scopes_are_global_and_coworker() {
    // A device tier would be a second answer to "what on this machine" — the standing rules
    // already answer it. If someone re-adds it, this is the test that asks them why.
    assert_eq!(SCOPE_KINDS, &["global", "coworker"]);
}
