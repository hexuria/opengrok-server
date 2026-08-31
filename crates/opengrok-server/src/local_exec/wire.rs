//! The exec WIRE shape — the ONE place the reverse-exec `serverMessage` is built and a daemon's
//! result frame is read back into a plain outcome.
//!
//! Isolated on purpose. The gate and the audit deal in a human-readable command STRING; the wire
//! carries an opaque protobuf-JSON message that the server constructs and the daemon replays. The
//! exact encoding is the one thing still to be confirmed byte-for-byte against the client's daemon
//! (`⚠ CONFIRM` below), so it lives behind two functions and nothing else in the server touches the
//! shape. If the daemon disagrees on a field name, this file is the only edit.
//!
//! Schema, from `source/packages/proto/generated/agent/v1/{exec_pb.ts,shell_exec_pb.ts}`:
//! - **server→daemon** `serverMessage` = `ExecServerMessage`, protobuf-JSON. A protobuf oneof is
//!   FLATTENED in JSON (that is how `@bufbuild/protobuf` `fromJson` reads it), so the shell case is
//!   the top-level key `shellArgs`, not a `{case, value}` wrapper:
//!   `{ "id": <u32>, "execId": <string>, "shellArgs": ShellArgs }`.
//!   `ShellArgs` carries `command` (the readable command), `simpleCommands` (the app's OWN parse —
//!   the gate already matched against THIS list, never a re-parse here), `workingDirectory`,
//!   `timeout`, `toolCallId`, and `skipApproval` — which the server ALWAYS sets to `false`: a
//!   caller does not get to wave a command past the gate.
//! - **daemon→server** result = `ExecClientMessage`, protobuf-JSON:
//!   `{ "id", "execId", "shellResult": ShellResult }` where `ShellResult` is ITSELF a oneof —
//!   flattened to one of `success | failure | timeout | rejected | spawnError | permissionDenied`.
//!   A refusal is its own case, not a non-zero exit, which is why the audit records the CASE.

use serde_json::{json, Value};

use super::broker::ExecOutcome;

/// Build the `serverMessage` for one shell command destined for the user's machine. `exec_id` is
/// the request id we correlate the result by; it doubles as `toolCallId`. `simple_commands` is the
/// app's pre-parsed command list the gate judged — passed through verbatim so the daemon's own
/// allowlisting sees exactly what we judged.
pub fn shell_server_message(
    exec_id: &str,
    command: &str,
    simple_commands: &[String],
    working_directory: &str,
    timeout_ms: u64,
) -> Value {
    json!({
        "id": 0,
        "execId": exec_id,
        // ⚠ CONFIRM with the client daemon: flattened oneof member `shellArgs` (protobuf-JSON), not
        // a `{ case, value }` wrapper. Change here only if their `fromJson` expects otherwise.
        "shellArgs": {
            "command": command,
            "simpleCommands": simple_commands,
            "workingDirectory": working_directory,
            "timeout": timeout_ms,
            "toolCallId": exec_id,
            // NEVER honor a caller's skipApproval — the server already decided at the gate. Sending
            // it explicitly false stops a daemon that trusts the field from bypassing the prompt.
            "skipApproval": false,
            "isBackground": false,
            "closeStdin": true,
        }
    })
}

/// The six ShellResult oneof cases, in field order — the vocabulary the audit's `outcome` records.
const RESULT_CASES: &[&str] = &[
    "success",
    "failure",
    "timeout",
    "rejected",
    "spawnError",
    "permissionDenied",
];

/// Read a daemon's `ExecClientMessage` result JSON into a plain [`ExecOutcome`]. Tolerant by design:
/// a message that is not a shell result, or names no known case, becomes a `spawnError`-shaped
/// outcome with a reason rather than a panic — the caller still gets a definite answer and the
/// audit still gets a case.
pub fn outcome_from_client_message(message: &Value) -> ExecOutcome {
    let Some(shell) = message.get("shellResult") else {
        return ExecOutcome::malformed("the daemon returned a non-shell result");
    };
    let Some(case) = RESULT_CASES.iter().find(|c| shell.get(*c).is_some()) else {
        return ExecOutcome::malformed("the daemon returned a shell result with no known case");
    };
    let body = &shell[*case];
    let string = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let exit_code = body
        .get("exitCode")
        .and_then(Value::as_i64)
        .map(|code| code as i32);
    let detail = match *case {
        "timeout" => {
            let ms = body.get("timeoutMs").and_then(Value::as_i64).unwrap_or(0);
            format!("timed out after {ms}ms")
        }
        "rejected" => string("reason"),
        "spawnError" | "permissionDenied" => string("error"),
        _ => String::new(),
    };
    ExecOutcome {
        case: (*case).to_string(),
        exit_code: if *case == "success" || *case == "failure" {
            exit_code
        } else {
            None
        },
        stdout: string("stdout"),
        stderr: string("stderr"),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_message_never_carries_a_caller_skip_approval() {
        let msg = shell_server_message("req-1", "git status", &["git status".into()], "/repo", 5000);
        assert_eq!(msg["shellArgs"]["skipApproval"], json!(false));
        assert_eq!(msg["shellArgs"]["command"], "git status");
        assert_eq!(msg["execId"], "req-1");
        assert_eq!(msg["shellArgs"]["simpleCommands"][0], "git status");
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
}
