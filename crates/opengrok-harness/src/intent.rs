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
///
/// `strip_intent_keep_facts` does not apply this to a finished answer. One
/// marker inside a long reply is not a reason to drop the reply. The tests
/// still pin the whole-text check.
#[allow(dead_code)]
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

/// Multi-paragraph retry narration, or three or more intent sentences.
///
/// One "I'll …" inside a long answer is not this. A numbered list is allowed
/// to contain a sentence that starts that way, and length is not a diary:
/// the old 400-character rule flattened those lists into one paragraph.
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
    (paragraphs >= 2 && intent_sentences >= 2) || intent_sentences >= 3
}

fn is_sentence_end(ch: char, next: Option<char>) -> bool {
    match ch {
        '!' | '?' | '\n' => true,
        // `xai/grok-4.6` and `3.14` are not sentence ends. Slice 5 greps the
        // mock door's model pin; splitting on the version dot made it `4. 6`.
        '.' => !next.is_some_and(|n| n.is_ascii_digit()),
        _ => false,
    }
}

fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    for (i, ch) in chars.iter().copied().enumerate() {
        current.push(ch);
        if is_sentence_end(ch, chars.get(i + 1).copied()) {
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

/// The original tail after a leading run of "I'll probe…" sentences.
///
/// The tail is a slice of `text`, not a re-join. Joining the kept sentences
/// with spaces is what turned a numbered list into one paragraph whenever a
/// single line looked like intent.
fn drop_leading_intent(text: &str) -> &str {
    let sentences = split_sentences(text);
    let mut rest = text;
    for sentence in &sentences {
        if !sentence_is_intent(sentence) {
            break;
        }
        let Some(rel) = rest.find(sentence.as_str()) else {
            break;
        };
        rest = &rest[rel + sentence.len()..];
    }
    rest.trim()
}

/// Drop a leading "I'll probe…" run. `None` if nothing factual remains.
/// A reply whose first sentence is not intent is returned unchanged — version
/// pins, decimals, and line breaks must not be rewritten.
pub fn strip_intent_keep_facts(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return None;
    }
    if !sentences
        .iter()
        .any(|sentence| sentence_is_intent(sentence))
    {
        return if is_retry_diary(text) {
            None
        } else {
            Some(text.to_string())
        };
    }
    let kept = drop_leading_intent(text);
    if kept.is_empty() || is_retry_diary(kept) {
        None
    } else {
        Some(kept.to_string())
    }
}

/// Parse the exit the tools leave on the result body.
/// Box `shell` appends `[exit code N]`. `user_machine_shell` renders `exit N` as the
/// first line (`ExecOutcome::render` in opengrok-server local_exec). A command that
/// merely prints `exit 1` plus more stdout is not that render.
pub fn shell_exit_code(content: &str) -> Option<i32> {
    if let Some(code) = bracketed_exit_code(content) {
        return Some(code);
    }
    leading_render_exit_code(content)
}

fn bracketed_exit_code(content: &str) -> Option<i32> {
    let marker = "[exit code ";
    let start = content.rfind(marker)?;
    let rest = &content[start + marker.len()..];
    let end = rest.find(']')?;
    rest[..end].trim().parse().ok()
}

fn leading_render_exit_code(content: &str) -> Option<i32> {
    let first = content.lines().next()?.trim();
    let rest = first.strip_prefix("exit ")?;
    let code: i32 = rest.trim().parse().ok()?;
    let is_render = content.lines().skip(1).any(|line| line.starts_with("--- "))
        || content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            == 1;
    is_render.then_some(code)
}

/// `command not found` / exit 127 will not be fixed by rewording the same binary name.
pub fn is_unrecoverable_command_miss(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    if lower.contains("command not found") {
        return true;
    }
    if shell_exit_code(content) == Some(127) {
        return true;
    }
    // macOS / zsh variants
    lower.contains("no such file or directory")
        && (lower.contains("gpui-agent")
            || lower.contains("not found")
            || lower.contains("command"))
}

/// Work-fail streak must see non-zero shell exits even when ToolResult.ok is true
/// (tools intentionally return ok so the model can read stdout — see opengrok-tools shell).
pub fn counts_as_work_failure(ok: bool, content: &str) -> bool {
    if !ok {
        return true;
    }
    match shell_exit_code(content) {
        Some(code) if code != 0 => true,
        _ => is_unrecoverable_command_miss(content),
    }
}

/// One sentence, bounded. Tool dumps must not become the diary we just suppressed.
pub fn short_failure_fact(content: &str) -> String {
    let trimmed = content.trim();
    let line = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_shell_wrapper_line(line))
        .or_else(|| {
            trimmed
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with("[harness]"))
        })
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

fn is_shell_wrapper_line(line: &str) -> bool {
    if line.starts_with("[harness]") || line.starts_with("--- ") {
        return true;
    }
    line.strip_prefix("exit ")
        .is_some_and(|rest| rest.trim().parse::<i32>().is_ok())
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
    fn a_real_answer_with_a_version_pin_is_not_rewritten() {
        let reply = "You said: hello. This is the mock door standing in for xai/grok-4.6 — no model was called.";
        assert_eq!(strip_intent_keep_facts(reply).as_deref(), Some(reply));
        assert_eq!(visible_chat(reply, None).as_deref(), Some(reply));
        assert!(
            !visible_chat(reply, None).unwrap().contains("grok-4. 6"),
            "sentence-split must not break a model pin"
        );
    }

    #[test]
    fn a_long_list_with_one_intent_sentence_keeps_its_line_breaks() {
        let mut lines = vec!["Here are the jokes.".to_string(), String::new()];
        for n in 1..30 {
            lines.push(format!("{n}. The clock struck {n}."));
        }
        lines.push("I'll meet you at the corner.".to_string());
        let reply = lines.join("\n");
        assert!(reply.chars().count() > 400, "{}", reply.chars().count());
        assert!(reply.contains("\n\n"));
        assert!(
            !is_retry_diary(&reply),
            "one I'll sentence does not make a long answer a diary"
        );
        assert_eq!(
            strip_intent_keep_facts(&reply).as_deref(),
            Some(reply.as_str())
        );
        assert!(
            !strip_intent_keep_facts(&reply)
                .unwrap()
                .contains("29. The clock struck 29. I'll"),
            "line breaks must not be joined into spaces"
        );
    }

    #[test]
    fn a_leading_probe_is_dropped_and_the_list_keeps_its_breaks() {
        let reply = "I'll probe the host.\n\n1. One.\n2. Two.";
        assert_eq!(
            strip_intent_keep_facts(reply).as_deref(),
            Some("1. One.\n2. Two.")
        );
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

    #[test]
    fn non_zero_exit_code_counts_as_work_failure_even_when_ok() {
        let box_shell = "zsh: command not found: gpui-agent\n[exit code 127]";
        assert_eq!(shell_exit_code(box_shell), Some(127));
        assert!(is_unrecoverable_command_miss(box_shell));
        assert!(counts_as_work_failure(true, box_shell));
        assert!(!counts_as_work_failure(
            true,
            "listed 3 profiles\n[exit code 0]"
        ));
        assert!(!counts_as_work_failure(true, "listed 3 profiles"));
    }

    #[test]
    fn user_machine_render_exit_127_counts_as_work_failure_even_when_ok() {
        // ExecOutcome::render in opengrok-server local_exec: first line is `exit N`.
        let rendered = "exit 127\n--- stderr ---\nzsh: command not found: gpui-agent";
        assert_eq!(shell_exit_code(rendered), Some(127));
        assert!(is_unrecoverable_command_miss(rendered));
        assert!(counts_as_work_failure(true, rendered));
        assert_eq!(
            short_failure_fact(rendered),
            "zsh: command not found: gpui-agent"
        );
        // Empty streams still render as a single `exit N` line.
        assert_eq!(shell_exit_code("exit 127"), Some(127));
        assert!(is_unrecoverable_command_miss("exit 127"));
        assert!(counts_as_work_failure(true, "exit 127"));
        assert!(!counts_as_work_failure(true, "exit 0"));
        assert!(!counts_as_work_failure(
            true,
            "exit 0\n--- stdout ---\nlisted 3 profiles"
        ));
        // A successful command that happens to print `exit 1` is not the render format.
        assert_eq!(
            shell_exit_code("exit 1\nmore output from the command"),
            None
        );
        assert!(!counts_as_work_failure(
            true,
            "exit 1\nmore output from the command"
        ));
    }
}
