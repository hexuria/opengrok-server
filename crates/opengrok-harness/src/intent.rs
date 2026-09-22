//! Quiet-loop classifiers: what may become NativeChat chat, and what must not.
//!
//! NativeChat paints every `TEXT_MESSAGE` as a bubble. Grok Bot hid the same
//! hops behind status chrome. Intent / status / retry-diary prose is therefore
//! a user-visible failure even when a tool is about to run — seen live as
//! “I’ll pull…”, “I’ll look up…”, “The BIR agent isn’t answering…” with no
//! facts while the shell missed the port, then the profiles.
//!
//! Pure, so the loop can decide before it emits.

/// After a successful listing/show, the next hop should answer from that result.
pub const READONLY_SHELL_NUDGE: &str = "[harness] The listing or show result is above. \
Answer the user with the facts from it. An empty result is the answer: say what was not found. \
Do not announce another probe or run the same listing again. Do not narrate I'll / let me / isn't answering.";

/// Empty `result: []` / `not_found` is a success on the wire (F5). Without this
/// sentence the loop searches until `MAX_ROUNDS`.
pub const EMPTY_RESULT_NUDGE: &str = "[harness] No matches. Do not repeat the same query. \
Answer with what was not found.";

/// After the first failed work tool: one silent fix, not a diary.
pub const FAILED_TOOL_NUDGE: &str = "[harness] That call failed. If a one-step fix is obvious \
(wrong port, missing flag), retry once with no narration. If it fails again, stop — the harness \
will report one short failure fact. Do not write a diary of retries.";

/// First failure plus one silent retry. A third work-tool round is the diary Uriah saw.
pub const MAX_FAILED_WORK_ROUNDS: u32 = 2;

const INTENT_OPENERS: &[&str] = &[
    "i'll ",
    "i will ",
    "first i'll ",
    "first i will ",
    "first i ",
    "let me ",
    "i'm going ",
    "i am going ",
    "going to ",
    "i am about to ",
    "i'm about to ",
    "now i'll ",
    "next i'll ",
    "then i'll ",
];

const STATUS_MARKERS: &[&str] = &[
    "isn't answering",
    "is not answering",
    "isn't responding",
    "is not responding",
    "isn't running",
    "is not running",
    "isn't live",
    "already live",
    "i'll probe",
    "i'll list",
    "i'll pull",
    "i'll look",
    "i'll try",
    "i'll check",
    "i'll fetch",
    "let me check",
    "let me look",
    "let me pull",
    "let me try",
    "one moment",
    "hold on",
    "give me a",
    "trying to",
    "trying again",
    "retrying",
    "looking up",
    "pulling the",
    "probing ",
];

fn normalize(text: &str) -> String {
    text.replace(['\u{2019}', '\u{2018}'], "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn has_fact_signal(text: &str) -> bool {
    let digits = text.chars().filter(char::is_ascii_digit).count();
    // Ports like 17421 are not facts; a TIN / form list is.
    if digits >= 6 {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    lower.contains("tin")
        || lower.contains("deadline")
        || lower.contains("form 1")
        || lower.contains("1701")
        || lower.contains("2550")
}

fn sentence_is_intent(sentence: &str) -> bool {
    let n = normalize(sentence);
    if n.is_empty() {
        return true;
    }
    if INTENT_OPENERS.iter().any(|opener| n.starts_with(opener)) {
        return true;
    }
    if STATUS_MARKERS.iter().any(|marker| n.contains(marker)) && !has_fact_signal(&n) {
        return true;
    }
    false
}

/// Pre-tool / between-tool CoT: “I'll pull…”, “First I'll…”, “The X isn't answering…”.
pub fn is_intent_or_status_prose(text: &str) -> bool {
    let n = normalize(text);
    if n.is_empty() {
        return true;
    }
    if has_fact_signal(&n) && !is_retry_diary(text) {
        return false;
    }
    INTENT_OPENERS.iter().any(|opener| n.starts_with(opener))
        || STATUS_MARKERS.iter().any(|marker| n.contains(marker))
}

/// Multi-paragraph or multi-sentence retry narration. One short failure fact is not this.
pub fn is_retry_diary(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let paragraphs = trimmed
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .count();
    let intent_sentences = split_sentences(trimmed)
        .into_iter()
        .filter(|s| sentence_is_intent(s))
        .count();
    (paragraphs >= 2 && intent_sentences >= 1)
        || intent_sentences >= 3
        || (trimmed.chars().count() > 400 && intent_sentences >= 1 && !has_fact_signal(trimmed))
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let piece = current.trim();
            if !piece.is_empty() {
                out.push(piece.to_string());
            }
            current.clear();
        }
    }
    let piece = current.trim();
    if !piece.is_empty() {
        out.push(piece.to_string());
    }
    out
}

/// Drop leading (and leftover) intent sentences. `None` if nothing factual remains.
pub fn strip_intent_keep_facts(text: &str) -> Option<String> {
    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return None;
    }
    let kept: Vec<&str> = sentences
        .iter()
        .map(String::as_str)
        .skip_while(|sentence| sentence_is_intent(sentence))
        .filter(|sentence| !sentence_is_intent(sentence) || has_fact_signal(sentence))
        .collect();
    let joined = kept.join(" ").trim().to_string();
    if joined.is_empty() || is_intent_or_status_prose(&joined) || is_retry_diary(&joined) {
        None
    } else {
        Some(joined)
    }
}

/// One sentence, bounded. Tool dumps must not become the diary we just suppressed.
pub fn short_failure_fact(content: &str) -> String {
    let trimmed = content.trim();
    let line = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("[harness]"))
        .unwrap_or("the tool failed");
    let sentence = line
        .split_once(". ")
        .map(|(head, _)| head.trim())
        .filter(|head| !head.is_empty())
        .unwrap_or(line);
    let mut fact = sentence.to_string();
    const MAX: usize = 240;
    if fact.chars().count() > MAX {
        fact = fact.chars().take(MAX).collect();
        fact.push('…');
    }
    if fact.is_empty() {
        "the tool failed".to_string()
    } else {
        fact
    }
}

/// Dead-end search body: empty `result` array or explicit `not_found`.
/// Malformed JSON is not a dead end — pass it through unchanged.
pub fn is_empty_or_not_found(content: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content.trim()) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if object
        .get("result")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        return true;
    }
    match object.get("not_found") {
        Some(serde_json::Value::Bool(true)) => true,
        Some(serde_json::Value::String(flag)) if !flag.is_empty() => true,
        Some(serde_json::Value::Number(n)) if n.as_i64() == Some(1) => true,
        _ => object
            .values()
            .any(|value| value.as_str() == Some("not_found")),
    }
}

/// Append the dead-end sentence, or return `content` untouched.
pub fn annotate_empty_result(content: &str) -> String {
    if !is_empty_or_not_found(content) {
        return content.to_string();
    }
    let mut out = content.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(EMPTY_RESULT_NUDGE);
    out
}

/// What NativeChat may paint from withheld model text this round.
pub fn visible_chat(withheld: &str, last_failure: Option<&str>) -> Option<String> {
    if let Some(facts) = strip_intent_keep_facts(withheld) {
        if is_retry_diary(&facts) {
            return last_failure.map(str::to_string);
        }
        return Some(facts);
    }
    last_failure.map(str::to_string)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ill_pull_and_curly_apostrophe_are_intent() {
        assert!(is_intent_or_status_prose("I'll pull the BIR profile"));
        assert!(is_intent_or_status_prose("I’ll look up the dues"));
        assert!(is_intent_or_status_prose("First I'll probe the host"));
        assert!(is_intent_or_status_prose(
            "The BIR agent isn't answering on 17421"
        ));
        assert!(is_intent_or_status_prose("Let me check the forms"));
    }

    #[test]
    fn facts_are_not_intent() {
        assert!(!is_intent_or_status_prose(
            "TIN 123-456-789. Forms: 1701, 2550M."
        ));
        assert!(!is_intent_or_status_prose("deadline 15 April"));
    }

    #[test]
    fn mixed_intent_then_facts_keeps_the_facts() {
        let visible =
            strip_intent_keep_facts("I'll look up the TIN.\n\nTIN 123-456-789. Forms: 1701.")
                .unwrap();
        assert!(!visible.to_ascii_lowercase().contains("i'll look"));
        assert!(visible.contains("TIN 123-456-789"));
    }

    #[test]
    fn a_retry_diary_is_not_an_answer() {
        let diary = "The BIR agent isn't answering.\n\nI'll try port 17423 next.\n\nThen I'll list profiles.";
        assert!(is_retry_diary(diary));
        assert!(
            visible_chat(diary, Some("connection refused on 17421"))
                .unwrap()
                .contains("connection refused")
        );
        assert!(visible_chat(diary, None).is_none());
    }

    #[test]
    fn short_failure_is_one_sentence() {
        let fact = short_failure_fact(
            "connection refused on 17421. retrying with 17423 would be the next idea.\n[harness] ignore",
        );
        assert_eq!(fact, "connection refused on 17421");
        assert!(short_failure_fact("").contains("failed"));
    }

    #[test]
    fn empty_result_array_is_annotated_and_a_full_one_is_not() {
        let empty = annotate_empty_result(r#"{"ok":true,"result":[]}"#);
        assert!(empty.contains(EMPTY_RESULT_NUDGE), "{empty}");
        assert!(empty.contains(r#""result":[]"#), "{empty}");
        let full = annotate_empty_result(r#"{"ok":true,"result":[{"tin":"00000000000000"}]}"#);
        assert_eq!(full, r#"{"ok":true,"result":[{"tin":"00000000000000"}]}"#);
        let not_found = annotate_empty_result(r#"{"ok":true,"not_found":true}"#);
        assert!(not_found.contains(EMPTY_RESULT_NUDGE), "{not_found}");
        let garbage = annotate_empty_result("not json at all");
        assert_eq!(garbage, "not json at all");
    }
}
