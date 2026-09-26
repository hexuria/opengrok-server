use super::*;

#[test]
fn ill_pull_and_curly_apostrophe_are_intent() {
    assert!(sentence_is_intent("I'll pull the BIR profile"));
    assert!(sentence_is_intent("I’ll look up the dues"));
    assert!(sentence_is_intent("First I'll probe the host"));
    assert!(sentence_is_intent("The BIR agent isn't answering on 17421"));
    assert!(sentence_is_intent("Let me check the forms"));
}

#[test]
fn facts_are_not_intent() {
    assert!(!sentence_is_intent("TIN 123-456-789. Forms: 1701, 2550M."));
    assert!(!sentence_is_intent("deadline 15 April"));
}

#[test]
fn mixed_intent_then_facts_keeps_the_facts() {
    let visible =
        strip_intent_keep_facts("I'll look up the TIN.\n\nTIN 123-456-789. Forms: 1701.").unwrap();
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

/// A markdown answer with a closing offer came back as
/// `Here are the files in your project: - README. md - src/main. rs`: the closing
/// "Let me know" read as intent, every `.` split a sentence and the rest was re-joined
/// with spaces.
#[test]
fn a_markdown_answer_with_a_closing_offer_is_not_rewritten() {
    let reply = "Here are the files in your project:\n\n- README.md\n- src/main.rs\n\nLet me know if you want more.";
    assert_eq!(visible_chat(reply, None).as_deref(), Some(reply));
}

#[test]
fn let_me_know_is_not_intent() {
    assert!(!sentence_is_intent("Let me know if you want more."));
    assert!(!sentence_is_intent("I'll be happy to help with the rest."));
    assert!(sentence_is_intent("Let me check the forms."));
}

/// `tin` as a substring made "setting", "testing" and "continue" facts; the demo's words are
/// the skill's now, and a fact is a number.
#[test]
fn a_fact_signal_is_a_number_not_a_domain_word() {
    assert!(!has_fact_signal("setting up the testing"));
    assert!(!has_fact_signal("continue"));
    assert!(!has_fact_signal("the deadline for form 1701"));
    assert!(!has_fact_signal("isn't answering on 17421"));
    assert!(has_fact_signal("TIN 123-456-789"));
}

#[test]
fn a_non_empty_answer_is_never_blanked() {
    assert!(visible_chat("Let me know if you want more.", None).is_some());
}

/// A status phrase in the middle of real prose is not a diary. "trying to" in one sentence
/// of a multi-paragraph answer used to drop that sentence and flatten the rest.
#[test]
fn a_status_phrase_inside_an_answer_does_not_rewrite_it() {
    let reply = "Rust checks borrows at compile time.\n\nWhen you are trying to mutate a borrowed value, the compiler refuses. Clone it, or end the borrow first.";
    assert_eq!(visible_chat(reply, None).as_deref(), Some(reply));
}

/// Leading intent goes; what follows keeps its own bytes, newlines and list markers.
#[test]
fn leading_intent_is_dropped_and_the_rest_is_kept_as_written() {
    let reply = "I'll look that up.\n\nHere are the files:\n\n- README.md\n- src/main.rs";
    assert_eq!(
        visible_chat(reply, None).as_deref(),
        Some("Here are the files:\n\n- README.md\n- src/main.rs")
    );
}

/// Streaming starts once the text past any opening intent is long enough to be an answer,
/// and not while it still opens with intent.
#[test]
fn withheld_text_goes_live_after_its_opening_intent() {
    let answer = "The borrow checker tracks who owns each value. ".repeat(6);
    assert!(goes_live(&format!("I'll explain. {answer}"), 200));
    assert!(!goes_live("I'll explain. The borrow checker.", 200));
    assert!(!goes_live("I'll check the host. ", 10));
    assert!(!goes_live(&"I'll check the host and then ".repeat(20), 200));
    assert!(!goes_live("Short answer.", 200));
}

#[test]
fn text_with_no_intent_is_not_trimmed() {
    let code = "    let x = 1;\n    let y = 2;";
    assert_eq!(strip_intent_keep_facts(code).as_deref(), Some(code));
}

#[test]
fn a_retry_diary_is_not_an_answer() {
    let diary =
        "The BIR agent isn't answering.\n\nI'll try port 17423 next.\n\nThen I'll list profiles.";
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
    assert_eq!(
        short_failure_fact("test parse_empty ... FAILED\ntest result: FAILED. 1 failed"),
        "test parse_empty ... FAILED"
    );
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
    assert!(!counts_as_work_failure(
        true,
        "exit 0\n--- stdout ---\nstep 4: bash: rustup: command not found"
    ));
    assert!(!counts_as_work_failure(
        true,
        "step 4: bash: rustup: command not found\n[exit code 0]"
    ));
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
        opened_target_sentence("exit 0\n--- stdout ---\nTalk to an embedded AgentHost\n").is_none()
    );
    assert!(
        opened_target_sentence(
            "exit 1\n--- stderr ---\nerror: connect 127.0.0.1:17421 failed: Connection refused\n"
        )
        .is_none()
    );
}

#[test]
fn a_view_only_create_is_an_open_editor_and_quote_variants_are_one_invoke() {
    let rendered =
        "exit 0\n--- stdout ---\n{\"v\":2,\"ok\":true,\"result\":{\"view\":\"profile-manager\"}}\n";
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
