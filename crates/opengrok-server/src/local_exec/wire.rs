//! The exec WIRE shape — the ONE place the reverse-exec `serverMessage` is built and a daemon's
//! result frame is read back into a plain outcome.
//!
//! Isolated on purpose. The gate and the audit deal in a human-readable command STRING; the wire
//! carries an opaque protobuf-JSON message the server constructs and the daemon replays, so it lives
//! behind two functions and nothing else in the server touches the shape.
//!
//! Schema confirmed against the client daemon (`production-executor.ts`,
//! `source/packages/proto/generated/agent/v1/{exec_pb.ts,shell_exec_pb.ts}`):
//! - **server→daemon** `serverMessage` = `ExecServerMessage`, protobuf-JSON. A protobuf oneof is
//!   FLATTENED in JSON (`ExecServerMessage.fromJson` with `ignoreUnknownFields`), so the shell case
//!   is the top-level key `shellStreamArgs` (the daemon only has a streaming shell executor;
//!   plain `shellArgs` is undescribable and refused before the ask dialog):
//!   `{ "id": <u32>, "shellStreamArgs": ShellArgs }`. There is NO `execId` field — the request is
//!   correlated by the ENVELOPE `requestId`, not anything inside the message.
//!   `ShellArgs` carries `command` (the readable command), `simpleCommands` (the app's OWN parse —
//!   the gate already matched against THIS list, never a re-parse here), `workingDirectory`,
//!   `timeout`, `toolCallId`, and `skipApproval` — which the server ALWAYS sets to `false`: a
//!   caller does not get to wave a command past the gate.
//! - **daemon→server** result: the STREAMING shell sends a series of `ExecClientMessage`s carrying
//!   `shellStream` — `{ "id", "shellStream": { <event> } }` where the event oneof is flattened to
//!   `start | stdout{data} | stderr{data} | exit{code} | rejected | permissionDenied | backgrounded`.
//!   We accumulate stdout/stderr and resolve on the terminal event. A non-streaming
//!   `{ "shellResult": ShellResult }` (success|failure|timeout|rejected|spawnError|permissionDenied)
//!   is still read if it ever arrives. Either way the audit records the CASE, not just an exit code.

use serde_json::{Value, json};

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
    // Confirmed against the client daemon (production-executor.ts: `ExecServerMessage.fromJson`,
    // `ignoreUnknownFields`): flattened protobuf-JSON, oneof member `shellArgs` at the top level.
    // The message's own field is `id` (a uint32) — there is NO `execId` on `ExecServerMessage`, so
    // we do not send one; the request is correlated by the ENVELOPE `requestId`, which the daemon
    // echoes back. `toolCallId` (a real ShellArgs field) still carries our id for the daemon's logs.
    json!({
        "id": 0,
        // shellStreamArgs, NOT shellArgs: the daemon only wires a STREAMING shell executor, and its
        // frame describer names shellStreamArgs (shellArgs falls to default:undefined and is refused
        // outright in ask mode, before the dialog). Same ShellArgs payload — only the oneof key.
        "shellStreamArgs": {
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
    let string = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
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

/// One interpreted `shellStream` event from the streaming shell. `Stdout`/`Stderr` are chunks to
/// accumulate; the rest are terminal (or ignorable). Field/case names are the flattened protobuf-JSON
/// of `ShellStream`'s `event` oneof.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamAction {
    Stdout(String),
    Stderr(String),
    /// The process exited with this code — terminal.
    Exit(i32),
    /// A terminal non-exit outcome (rejected / permissionDenied / backgrounded / sandboxUnsupported),
    /// with the case name the audit records and a human reason.
    Terminal {
        case: String,
        detail: String,
    },
    /// A non-terminal event we do not act on (start / hookContext / anything unknown).
    Ignore,
}

/// Interpret one daemon `client` frame's message as a `shellStream` event, or `None` if it is not a
/// stream frame (e.g. a `shellResult`, handled separately).
pub fn stream_action(message: &Value) -> Option<StreamAction> {
    let stream = message.get("shellStream")?;
    let data = |key: &str| {
        stream
            .get(key)
            .and_then(|v| v.get("data"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    if stream.get("stdout").is_some() {
        Some(StreamAction::Stdout(data("stdout")))
    } else if stream.get("stderr").is_some() {
        Some(StreamAction::Stderr(data("stderr")))
    } else if let Some(exit) = stream.get("exit") {
        Some(StreamAction::Exit(
            exit.get("code").and_then(Value::as_i64).unwrap_or(0) as i32,
        ))
    } else if let Some(rejected) = stream.get("rejected") {
        Some(StreamAction::Terminal {
            case: "rejected".to_string(),
            detail: rejected
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    } else if let Some(denied) = stream.get("permissionDenied") {
        Some(StreamAction::Terminal {
            case: "permissionDenied".to_string(),
            detail: denied
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    } else if stream.get("backgrounded").is_some() {
        Some(StreamAction::Terminal {
            case: "backgrounded".to_string(),
            detail: "the command was backgrounded".to_string(),
        })
    } else if stream.get("sandboxUnsupported").is_some() {
        Some(StreamAction::Terminal {
            case: "spawnError".to_string(),
            detail: "the sandbox is unsupported for this command".to_string(),
        })
    } else {
        // start / hookContext / anything else — nothing to accumulate or resolve.
        Some(StreamAction::Ignore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_message_never_carries_a_caller_skip_approval() {
        let msg =
            shell_server_message("req-1", "git status", &["git status".into()], "/repo", 5000);
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
}
