use super::*;

#[test]
fn server_message_never_carries_a_caller_skip_approval() {
    let msg = shell_server_message("req-1", "git status", &["git status".into()], "/repo", 5000);
    assert_eq!(msg["shellStreamArgs"]["skipApproval"], json!(false));
    assert_eq!(msg["shellStreamArgs"]["command"], "git status");
    assert!(
        msg.get("shellArgs").is_none(),
        "the shell path is shellStreamArgs, not shellArgs"
    );
    assert!(
        msg.get("execId").is_none(),
        "ExecServerMessage has no execId field"
    );
    assert_eq!(msg["shellStreamArgs"]["simpleCommands"][0], "git status");
}

#[test]
fn reads_a_success_result() {
    let message = json!({
        "id": 0, "execId": "req-1",
        "shellResult": { "success": {
            "command": "echo hi", "exitCode": 0, "stdout": "hi\n", "stderr": ""
        }}
    });
    let out = outcome_from_client_message(&message);
    assert_eq!(out.case, "success");
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, "hi\n");
}

#[test]
fn reads_a_failure_with_its_exit_code() {
    let message = json!({
        "shellResult": { "failure": {
            "command": "false", "exitCode": 1, "stdout": "", "stderr": "boom"
        }}
    });
    let out = outcome_from_client_message(&message);
    assert_eq!(out.case, "failure");
    assert_eq!(out.exit_code, Some(1));
    assert_eq!(out.stderr, "boom");
}

#[test]
fn a_refusal_is_a_case_not_an_exit_code() {
    let message = json!({
        "shellResult": { "permissionDenied": {
            "command": "rm -rf /", "error": "not permitted", "isReadonly": false
        }}
    });
    let out = outcome_from_client_message(&message);
    assert_eq!(out.case, "permissionDenied");
    assert_eq!(out.exit_code, None);
    assert_eq!(out.detail, "not permitted");
}

#[test]
fn a_timeout_reports_its_duration() {
    let message = json!({ "shellResult": { "timeout": { "timeoutMs": 3000 } } });
    let out = outcome_from_client_message(&message);
    assert_eq!(out.case, "timeout");
    assert_eq!(out.exit_code, None);
    assert!(out.detail.contains("3000"));
}

#[test]
fn a_non_shell_message_is_malformed_not_a_panic() {
    let out = outcome_from_client_message(&json!({ "id": 0, "readResult": {} }));
    assert_eq!(out.case, "spawnError");
    assert!(out.detail.contains("non-shell"));
}

#[test]
fn stream_action_reads_chunks_and_a_terminal_exit() {
    let stdout = json!({ "id": 0, "shellStream": { "stdout": { "data": "Darwin\n" } } });
    assert_eq!(
        stream_action(&stdout),
        Some(StreamAction::Stdout("Darwin\n".to_string()))
    );

    let stderr = json!({ "shellStream": { "stderr": { "data": "oops" } } });
    assert_eq!(
        stream_action(&stderr),
        Some(StreamAction::Stderr("oops".to_string()))
    );

    let exit = json!({ "shellStream": { "exit": { "code": 0 } } });
    assert_eq!(stream_action(&exit), Some(StreamAction::Exit(0)));

    let start = json!({ "shellStream": { "start": {} } });
    assert_eq!(stream_action(&start), Some(StreamAction::Ignore));

    let denied = json!({ "shellStream": { "permissionDenied": { "error": "nope" } } });
    assert_eq!(
        stream_action(&denied),
        Some(StreamAction::Terminal {
            case: "permissionDenied".to_string(),
            detail: "nope".to_string()
        })
    );

    // A shellResult message is NOT a stream frame.
    assert_eq!(
        stream_action(&json!({ "shellResult": { "success": {} } })),
        None
    );
}
