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
