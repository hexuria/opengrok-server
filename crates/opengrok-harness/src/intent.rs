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

/// A click or invoke already selected a named target. Say that and stop.
pub const OPENED_TARGET_NUDGE: &str = "[harness] That selected the target. Answer with the name \
and where it opened. Do not run the same command again.";

/// `profile.create` opens an editor and writes nothing. The next hop fills
/// fields with a different command, or answers. Another create does not.
pub const OPENED_EDITOR_NUDGE: &str = "[harness] That opened the editor and wrote nothing. \
Arguments on this command were ignored. Ask for the missing fields with request_user_form \
and collect set to true. Put values the person already gave on each field as value. \
Do not call profile.save until that form is answered. set-value is not an invoke name.";

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

/// Drop leading (and leftover) intent sentences. `None` if nothing factual remains.
/// A reply with no intent sentences is returned unchanged — version pins and
/// decimals must not be rewritten.
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

/// The invoke name when the command is `gpui-agent invoke NAME`. Quote variants
/// of one invoke are one action. A click or `set-value` has no invoke token, so
/// the key stays the whole command.
pub fn shell_action_key(command: &str) -> String {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(index) = tokens.iter().position(|token| *token == "invoke") else {
        return command.to_string();
    };
    let Some(name) = tokens.get(index + 1) else {
        return command.to_string();
    };
    let name = name.trim_matches(|c: char| c == '\'' || c == '"' || c == '\\');
    if name.is_empty() || name.starts_with('-') {
        command.to_string()
    } else {
        name.to_string()
    }
}

fn stdout_json_result(content: &str) -> Option<serde_json::Value> {
    if counts_as_work_failure(true, content) {
        return None;
    }
    let stdout = content.split_once("--- stdout ---")?.1;
    let start = stdout.find('{')?;
    let end = stdout.rfind('}')?;
    if end < start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&stdout[start..=end]).ok()?;
    Some(value.get("result").cloned().unwrap_or(value))
}

/// A successful shell whose stdout JSON names what it opened.
///
/// `gpui-agent click` returns `{result:{name, view}}`. The first such result is
/// the navigation. A later identical command is not more work.
pub fn opened_target_sentence(content: &str) -> Option<String> {
    let result = stdout_json_result(content)?;
    let name = result
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())?;
    let view = result
        .get("view")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|view| !view.is_empty());
    Some(match view {
        Some(view) => format!("{name} is selected on {view}."),
        None => format!("{name} is selected."),
    })
}

/// A successful invoke whose result is only `{view}`. `profile.create` does
/// this: the editor opened and nothing was written. A later call of the same
/// invoke is not more work.
pub fn opened_editor_sentence(content: &str) -> Option<String> {
    let result = stdout_json_result(content)?;
    if result
        .get("name")
        .and_then(serde_json::Value::as_str)
        .is_some()
    {
        return None;
    }
    let object = result.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let view = object
        .get("view")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|view| !view.is_empty())?;
    Some(format!("Opened {view}. Nothing was saved."))
}

/// The host rejected the invoke's arguments. The next call can pass `--arg`.
/// Exit 127 is not this: a missing binary does not become a flag fix.
pub fn is_invoke_argv_mistake(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("requires args.") || lower.contains("unexpected argument")
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

    #[test]
    fn a_click_that_selects_a_named_target_is_the_navigation() {
        let rendered = "exit 0\n--- stdout ---\n{\n  \"ok\": true,\n  \"result\": {\n    \"name\": \"Juan Dela Cruz\",\n    \"view\": \"dashboard\"\n  }\n}\n";
        assert_eq!(
            opened_target_sentence(rendered).as_deref(),
            Some("Juan Dela Cruz is selected on dashboard.")
        );
        assert!(
            opened_target_sentence("exit 0\n--- stdout ---\nTalk to an embedded AgentHost\n")
                .is_none()
        );
        assert!(opened_target_sentence(
            "exit 1\n--- stderr ---\nerror: connect 127.0.0.1:17421 failed: Connection refused\n"
        )
        .is_none());
    }

    #[test]
    fn a_view_only_create_is_an_open_editor_and_quote_variants_are_one_invoke() {
        let rendered = "exit 0\n--- stdout ---\n{\"v\":2,\"ok\":true,\"result\":{\"view\":\"profile-manager\"}}\n";
        assert_eq!(
            opened_editor_sentence(rendered).as_deref(),
            Some("Opened profile-manager. Nothing was saved.")
        );
        assert!(opened_target_sentence(rendered).is_none());
        let named = "exit 0\n--- stdout ---\n{\"result\":{\"name\":\"Juan Dela Cruz\",\"view\":\"dashboard\"}}\n";
        assert!(opened_editor_sentence(named).is_none());
        assert_eq!(
            shell_action_key(
                "gpui-agent invoke profile.create --arg name='Juana Jane' --arg tin=00000000000001"
            ),
            "profile.create"
        );
        assert_eq!(
            shell_action_key(
                "gpui-agent invoke profile.create --arg 'name=Juana Jane' --arg tin=00000000000001"
            ),
            "profile.create"
        );
        assert_eq!(
            shell_action_key("gpui-agent set-value profile-name 'Juana Jane'"),
            "gpui-agent set-value profile-name 'Juana Jane'"
        );
    }

    #[test]
    fn a_rejected_invoke_argument_is_not_a_missing_binary() {
        let year = "exit 1\n--- stderr ---\nerror: profile.forms_set.get requires args.year as a JSON number\n";
        let blob = "exit 2\n--- stderr ---\nerror: unexpected argument '{\"query\":\"juan dela cruz\"}' found\n";
        assert!(is_invoke_argv_mistake(year));
        assert!(is_invoke_argv_mistake(blob));
        assert!(counts_as_work_failure(true, year));
        assert!(!is_unrecoverable_command_miss(year));
        assert!(!is_invoke_argv_mistake(
            "exit 127\n--- stderr ---\nsh: gpui-agent: command not found\n"
        ));
    }
}
