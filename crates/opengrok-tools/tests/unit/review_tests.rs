use super::*;
use serde_json::json;

fn ask(reason: AwaitingReason) -> Gate {
    Gate::Ask(reason, "why".to_string())
}

#[test]
fn a_denied_gate_refuses_whatever_the_judge_would_say() {
    for review in [
        None,
        Some(ReviewOutcome::Allow),
        Some(ReviewOutcome::Ask("a".into())),
    ] {
        assert_eq!(
            combine(Gate::Deny("off".into()), review, true),
            Outcome::Refuse("off".into())
        );
    }
}

#[test]
fn a_review_block_beats_a_pending_consent_and_a_click() {
    // A standing written rule outranks a click: even an approved call stays refused.
    assert_eq!(
        combine(
            ask(AwaitingReason::ExecConsent),
            Some(ReviewOutcome::Block("no".into())),
            true
        ),
        Outcome::Refuse("no".into())
    );
    assert_eq!(
        combine(Gate::Allow, Some(ReviewOutcome::Block("no".into())), false),
        Outcome::Refuse("no".into())
    );
}

#[test]
fn the_gates_ask_subsumes_a_review_ask_one_card_per_call() {
    assert_eq!(
        combine(
            ask(AwaitingReason::ExecConsent),
            Some(ReviewOutcome::Ask("r".into())),
            false
        ),
        Outcome::Ask(AwaitingReason::ExecConsent, "why".into())
    );
}

#[test]
fn approval_releases_the_gates_ask_but_a_review_ask_still_asks() {
    assert_eq!(
        combine(ask(AwaitingReason::PolicyApproval), None, true),
        Outcome::Run
    );
    assert_eq!(
        combine(Gate::Allow, Some(ReviewOutcome::Ask("r".into())), false),
        Outcome::Ask(AwaitingReason::AutoReview, "r".into())
    );
}

#[test]
fn allow_all_round_runs() {
    assert_eq!(
        combine(Gate::Allow, Some(ReviewOutcome::Allow), false),
        Outcome::Run
    );
    assert_eq!(combine(Gate::Allow, None, false), Outcome::Run);
}

#[test]
fn a_policy_with_nothing_written_is_inactive() {
    assert!(!ReviewPolicy::default().is_active());
    let only = |allow: &str, ask: &str, block: &str| ReviewPolicy {
        allow_instructions: allow.into(),
        ask_instructions: ask.into(),
        block_instructions: block.into(),
    };
    assert!(!only("  ", "\n", "").is_active());
    assert!(only("", "", "never touch prod").is_active());
    assert!(only("", "check with me first", "").is_active());
    assert!(only("git is fine", "", "").is_active());
}

#[test]
fn redaction_strips_identity_hides_secrets_and_states_its_clip() {
    // Spaces keep it from looking like a key (a long run of token characters IS redacted).
    let long = "word ".repeat(120);
    let args = json!({
        "command": "ls",
        "coworker_id": "cw_evil",
        "boxId": "box_evil",
        "api_key": "abc",
        "nested": { "password": "p", "token_value": "t", "note": long, "auth": "Bearer zzz" },
        "blob": "A".repeat(50),
    });
    let text = redact_arguments(&args);
    assert!(!text.contains("cw_evil"));
    assert!(!text.contains("box_evil"));
    assert!(!text.contains("\"abc\""));
    assert!(!text.contains("\"p\""));
    assert!(!text.contains("zzz"));
    assert!(!text.contains(&"A".repeat(50)));
    assert!(text.contains("clipped 100 chars"), "{text}");
    assert!(text.contains("\"command\":\"ls\""));
}

#[test]
fn the_whole_payload_is_clipped_with_a_statement() {
    let text_400 = "word ".repeat(80);
    let args = json!({ "a": text_400, "b": text_400, "c": text_400,
                       "d": text_400, "e": text_400, "f": text_400 });
    let text = redact_arguments(&args);
    assert!(text.chars().count() < 2_100);
    assert!(text.contains("…[clipped"));
}

#[test]
fn a_block_refusal_names_the_instruction() {
    let text = block_refusal("  never touch prod  ");
    assert_eq!(
        text,
        "auto-review blocked this — your block instructions say: \"never touch prod\""
    );
}

#[test]
fn an_ask_first_reason_names_the_instruction() {
    let text = ask_first_reason("  always ask about rm -rf  ");
    assert_eq!(
        text,
        "Your auto-review instructions asked to check this first: \"always ask about rm -rf\""
    );
}
