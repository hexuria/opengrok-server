use super::*;

fn policy(mode: LocalExecMode, allow: &[&str], deny: &[&str]) -> LocalExecPolicy {
    LocalExecPolicy {
        mode,
        allow: allow.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
        session_allow: Vec::new(),
    }
}

#[test]
fn default_is_closed() {
    // The default policy (no config at all) denies everything — the channel is off.
    assert!(matches!(
        decide(&LocalExecPolicy::default(), "echo hi"),
        LocalExecDecision::Deny(_)
    ));
}

#[test]
fn never_denies_even_an_allowlisted_command() {
    // Mode is the baseline: Never denies regardless of the lists.
    let p = policy(LocalExecMode::Never, &["echo"], &[]);
    assert!(matches!(decide(&p, "echo hi"), LocalExecDecision::Deny(_)));
}

#[test]
fn ask_with_no_rules_asks() {
    let p = policy(LocalExecMode::Ask, &[], &[]);
    assert_eq!(decide(&p, "echo hi"), LocalExecDecision::Ask);
}

#[test]
fn ask_allowlist_allows_on_word_boundary_only() {
    let p = policy(LocalExecMode::Ask, &["git status"], &[]);
    assert_eq!(decide(&p, "git status"), LocalExecDecision::Allow);
    assert_eq!(decide(&p, "git status --short"), LocalExecDecision::Allow);
    // A shared prefix that is NOT a word boundary must not match — closed by default.
    assert_eq!(decide(&p, "git statusx"), LocalExecDecision::Ask);
    assert_eq!(decide(&p, "git log"), LocalExecDecision::Ask);
}

#[test]
fn ask_denylist_denies() {
    let p = policy(LocalExecMode::Ask, &[], &["rm"]);
    assert!(matches!(decide(&p, "rm -rf /"), LocalExecDecision::Deny(_)));
}

#[test]
fn deny_wins_over_allow() {
    // The same command both allowed and denied: deny wins.
    let p = policy(LocalExecMode::Ask, &["sudo rm"], &["sudo"]);
    assert!(matches!(
        decide(&p, "sudo rm -rf /"),
        LocalExecDecision::Deny(_)
    ));
}

#[test]
fn sudo_cannot_stand_as_allow() {
    let refused = Some("sudo cannot be a standing allow");
    assert_eq!(standing_rule_refusal("allow", "sudo"), refused);
    assert_eq!(standing_rule_refusal("allow", "sudo rm -rf /"), refused);
    assert_eq!(standing_rule_refusal("allow", "/usr/bin/sudo"), refused);
    assert_eq!(
        standing_rule_refusal("allow", r"C:\Windows\System32\sudo.exe"),
        refused
    );
    assert_eq!(standing_rule_refusal("allow", "  SUDO -u root id"), refused);
    // Deny of sudo is the safety net; it is not refused.
    assert_eq!(standing_rule_refusal("deny", "sudo"), None);
    assert_eq!(standing_rule_refusal("deny", "sudo rm"), None);
    // A different command that merely starts with the letters, or sudo later.
    assert_eq!(standing_rule_refusal("allow", "sudoedit"), None);
    assert_eq!(standing_rule_refusal("allow", "echo sudo"), None);
    assert_eq!(standing_rule_refusal("allow", "uname"), None);
}

#[test]
fn session_allow_runs_under_ask_and_dies_with_the_struct() {
    let mut p = policy(LocalExecMode::Ask, &[], &[]);
    p.session_allow = vec!["git status".into()];
    assert_eq!(decide(&p, "git status --short"), LocalExecDecision::Allow);
    assert_eq!(decide(&p, "git log"), LocalExecDecision::Ask);
    let mut denied = p.clone();
    denied.deny = vec!["git".into()];
    assert!(matches!(
        decide(&denied, "git status"),
        LocalExecDecision::Deny(_)
    ));
}

#[test]
fn bypass_allows_everything() {
    // Bypass is the user's deliberate "allow all" — even a command that would be denylisted.
    let p = policy(LocalExecMode::Bypass, &[], &["rm"]);
    assert_eq!(decide(&p, "rm -rf /"), LocalExecDecision::Allow);
    assert_eq!(decide(&p, "anything at all"), LocalExecDecision::Allow);
}

#[test]
fn an_empty_pattern_never_matches() {
    // A blank rule must not become a wildcard that allows or denies everything.
    assert!(!matches("", "echo hi"));
    assert!(!matches("   ", "echo hi"));
    let p = policy(LocalExecMode::Ask, &[""], &[]);
    assert_eq!(decide(&p, "echo hi"), LocalExecDecision::Ask);
}

#[test]
fn the_gate_sees_gpui_agent_and_the_wire_command_has_the_user_bin_path() {
    let command = "gpui-agent hello";
    let p = policy(LocalExecMode::Ask, &["gpui-agent"], &[]);
    assert_eq!(decide(&p, command), LocalExecDecision::Allow);
    let dispatched = command_with_user_bin_path(command);
    assert_ne!(decide(&p, &dispatched), LocalExecDecision::Allow);
    let message = user_machine_shell_message("req", command, &[command.to_string()], 1);
    assert_eq!(
        message["shellStreamArgs"]["command"].as_str(),
        Some(dispatched.as_str())
    );
    assert_eq!(
        message["shellStreamArgs"]["simpleCommands"][0].as_str(),
        Some(command)
    );
    assert!(
        dispatched.starts_with(
            "export PATH=\"$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\"; "
        )
    );
    assert!(dispatched.contains("export GPUI_AGENT_TOKEN=\"$_gpui_token\""));
    assert!(!dispatched.contains("gpui-agent()"));
    assert!(!dispatched.contains("mktemp"));
    assert!(dispatched.ends_with("fi; gpui-agent hello"));
    // The value is read from the host process. The audited string must not
    // carry a literal secret, and it must not name a temp shim.
    assert!(!dispatched.contains("dev-secret"));
    assert!(!dispatched.contains("/T/tmp."));
}
