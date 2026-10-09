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
//!   `ShellArgs` carries `command` (the readable command), `simpleCommands` (the server's own split
//!   of that command, `local_exec::simple_commands` — never a caller's list), `workingDirectory`,
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
/// server's own split of the line the gate read, so the daemon's own allowlisting sees the same
/// simple commands, not the unsplit string.
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

/// The six ShellResult oneof cases, in field order — what a daemon may report. The audit's
/// `outcome` also holds the server's own `offline` (`ExecOutcome::offline`), which is kept out of
/// this list so no daemon frame can claim it.
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
#[path = "../tests/unit/local_exec_wire.rs"]
mod tests;
