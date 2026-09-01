//! Auto-review at the tool seam: the policy a run carries, the judge it asks, and the ladder that
//! combines the judge's word with the primary gate's verdict. Design: `docs/AUTO-REVIEW.md` §4.
//!
//! This crate defines the SEAM only. The model-backed judge lives in `opengrok-harness` (this
//! crate cannot depend on it — the harness depends on us), and the server resolves the policy
//! once per run from its tiers and attaches both to the `Executor`. What lives here is pure and
//! total: `combine` and `redact_arguments` cannot fail, which is what lets the executor promise
//! that a judge outage becomes a card and never an exception.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The instruction texts the judge reads, resolved ONCE per run and carried on the executor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewPolicy {
    pub allow_instructions: String,
    pub block_instructions: String,
}

impl ReviewPolicy {
    /// The per-call short-circuit (`docs/AUTO-REVIEW.md` §3): nothing written means no judge
    /// call. One in-memory test; no DB read.
    pub fn is_active(&self) -> bool {
        !(self.allow_instructions.trim().is_empty() && self.block_instructions.trim().is_empty())
    }
}

/// One question for the judge. Arguments arrive already redacted and clipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewAsk<'a> {
    pub tool: &'a str,
    pub arguments: &'a str,
    pub allow_instructions: &'a str,
    pub block_instructions: &'a str,
}

/// The judge's word. `Unavailable` is a judge that could not answer (unreachable, timed out,
/// spoke more than one word) — it lands on the same rung as `Ask`, with a different explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Allow,
    Ask,
    Block,
    Unavailable,
}

/// The judge. Implementations MUST be total: never panic, never return an error — an outage is
/// `Unavailable`, which the ladder turns into a card (CLAUDE.md #8).
#[async_trait::async_trait]
pub trait ReviewJudge: Send + Sync {
    async fn judge(&self, ask: ReviewAsk<'_>) -> ReviewVerdict;
}

/// Why a call is waiting. Travels on the result so the run's suspension can say which card to
/// raise and which verb may answer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AwaitingReason {
    /// The remote-control gate wants the machine owner's consent for this command.
    ExecConsent,
    /// The coworker's policy grant marks this tool `needs_approval`.
    PolicyApproval,
    /// The auto-review judge said "ask".
    AutoReview,
}

impl AwaitingReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExecConsent => "exec-consent",
            Self::PolicyApproval => "policy-approval",
            Self::AutoReview => "auto-review",
        }
    }
}

/// The primary gate's verdict for one call, before auto-review is consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Allow,
    Ask(AwaitingReason, String),
    Deny(String),
}

/// What the judge said, with the text the card or refusal shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    Allow,
    Ask(String),
    Block(String),
}

/// What the executor does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Run,
    Ask(AwaitingReason, String),
    Refuse(String),
}

/// The paragraph a card shows when the judge itself said "ask".
pub const REVIEW_ASK_REASON: &str = "Your auto-review instructions did not clearly allow this, so it is being asked rather than allowed.";
/// The paragraph a card shows when the judge could not answer at all.
pub const REVIEW_UNAVAILABLE_REASON: &str =
    "The reviewer did not answer, so this is being asked rather than allowed.";

/// THE LADDER. block > ask > allow; ask beats allow; and the primary gate's ask subsumes a review
/// ask — at most one card per call (`docs/AUTO-REVIEW.md` §4.3). `approved` means a person has
/// already said yes to THIS call, which releases the gate's ask and nothing else: a deny stays a
/// deny, and a review block still blocks — a standing written rule outranks a click.
pub fn combine(gate: Gate, review: Option<ReviewOutcome>, approved: bool) -> Outcome {
    if let Gate::Deny(why) = gate {
        return Outcome::Refuse(why);
    }
    if let Some(ReviewOutcome::Block(why)) = review {
        return Outcome::Refuse(why);
    }
    if let Gate::Ask(reason, why) = gate
        && !approved
    {
        return Outcome::Ask(reason, why);
    }
    if let Some(ReviewOutcome::Ask(why)) = review {
        return Outcome::Ask(AwaitingReason::AutoReview, why);
    }
    Outcome::Run
}

/// The refusal the model reads when a block instruction stops a call. Names the instruction the
/// user actually wrote — one word from the judge means the text IS the rule.
pub fn block_refusal(block_instructions: &str) -> String {
    format!(
        "auto-review blocked this — your block instructions say: \"{}\"",
        clip(block_instructions.trim(), 200)
    )
}

/// Keys the executor overwrites with the session's identity. The judge must never see a value the
/// model chose for one of them, and it needs none of them to judge an action.
///
/// This is also the single source of truth for `overwrite_identity`/`strip_identity` (in
/// `lib.rs`): every spelling here is a value the model must never get to choose, so every spelling
/// here must be removed before a tool runs or an argument leaves the process. Listing a key here
/// but stripping only some of them is the exact gap that let a camelCase `coworkerId` ride through
/// to a remote plugin.
pub(crate) const IDENTITY_KEYS: &[&str] = &[
    "account_id",
    "accountId",
    "coworker_id",
    "coworkerId",
    "box_id",
    "boxId",
];

/// Keys whose values are secrets by name.
const SECRET_KEY_FRAGMENTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "api_key",
    "apikey",
    "credential",
    "cookie",
];

const VALUE_CLIP: usize = 500;
const TOTAL_CLIP: usize = 2_000;

/// What the judge sees. Identity keys are stripped, secret-looking values are replaced, every
/// string is clipped, and the clipping is STATED, never implied — a judge reading "rm -rf /" with
/// a silent "…tmp/scratch" cut off would be judging a different command.
pub fn redact_arguments(arguments: &Value) -> String {
    let redacted = redact_value(arguments, None);
    let text = serde_json::to_string(&redacted).unwrap_or_else(|_| "{}".to_string());
    clip(&text, TOTAL_CLIP)
}

fn redact_value(value: &Value, key: Option<&str>) -> Value {
    if let Some(key) = key {
        let lower = key.to_ascii_lowercase();
        if SECRET_KEY_FRAGMENTS
            .iter()
            .any(|fragment| lower.contains(fragment))
        {
            return Value::String("«redacted»".to_string());
        }
    }
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !IDENTITY_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), redact_value(value, Some(key))))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| redact_value(v, None)).collect()),
        Value::String(text) => Value::String(if looks_like_a_secret(text) {
            "«redacted»".to_string()
        } else {
            clip(text, VALUE_CLIP)
        }),
        other => other.clone(),
    }
}

/// A bearer token or key-shaped string: one long run of token characters with no spaces, or a
/// known key prefix. Deliberately coarse — a false positive hides a value from the judge (it asks),
/// a false negative shows a secret to a model.
fn looks_like_a_secret(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.starts_with("Bearer ") || trimmed.starts_with("sk-") || trimmed.starts_with("xoxb-")
    {
        return true;
    }
    trimmed.len() >= 40
        && !trimmed.contains(' ')
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '='))
}

/// Clip on a char boundary and SAY SO.
fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept} …[clipped {} chars]", count - max)
}

#[cfg(test)]
mod tests {
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
        assert!(
            !ReviewPolicy {
                allow_instructions: "  ".into(),
                block_instructions: String::new()
            }
            .is_active()
        );
        assert!(
            ReviewPolicy {
                allow_instructions: String::new(),
                block_instructions: "never touch prod".into()
            }
            .is_active()
        );
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
}
