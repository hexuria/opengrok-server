//! Transcript cards the server emits when a run suspends — pure builders, unit-tested, with the
//! renderer's rules written next to the field they constrain. Shapes are TRANSCRIBED from the
//! desktop client (`docs/AUTO-REVIEW.md` §5 for the auto-review card), never invented.

use serde_json::{Value, json};

use opengrok_tools::USER_MACHINE_SHELL;

/// The `auto-review-approval` card. Re-emit with the SAME `entry_id` and a new `status` to settle
/// it — the renderer dedups on `auto-review-approval:${requestId}:${status}`.
pub fn auto_review_card(
    entry_id: &str,
    request_id: &str,
    status: &str,
    tool: &str,
    arguments: &Value,
    reason: Option<&str>,
    timestamp_ms: i64,
) -> Value {
    let mut approval = json!({
        "requestId": request_id,
        "status": status,
        // Never absent: the client's fallback "unknown" is not in its own enum.
        "surface": surface_for(tool),
        // Required. Hidden by the renderer when `command` is present or when it matches the
        // renderer's boilerplate, so it carries the meaning for the non-shell tools.
        "summary": summary_for(tool, arguments),
    });
    if let Some(reason) = reason {
        approval["reason"] = json!(reason);
    }
    if let Some(command) = command_for(tool, arguments) {
        approval["command"] = json!(command);
    }
    approval["proposedRule"] = json!(proposed_rule(tool, arguments));
    json!({
        "kind": "send-message",
        "id": entry_id,
        "timestampMs": timestamp_ms,
        "message": {
            "type": "auto-review-approval",
            "approval": approval,
        },
    })
}

/// The renderer's `surface` enum, by tool. A plugin tool is a qualified `plugin.server.tool`.
pub fn surface_for(tool: &str) -> &'static str {
    match tool {
        USER_MACHINE_SHELL => "host_shell",
        "shell" | "read_file" | "write_file" => "box_shell",
        other if other.matches('.').count() >= 2 => "mcp",
        _ => "computer",
    }
}

fn string_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

/// Set for the two shell tools only; the renderer shows it in place of `summary`.
pub fn command_for(tool: &str, arguments: &Value) -> Option<String> {
    match tool {
        "shell" | USER_MACHINE_SHELL => string_arg(arguments, "command").map(|c| clip(c, 500)),
        _ => None,
    }
}

/// Required and meaningful. Must dodge the renderer's boilerplate patterns (`Run a command on
/// your local computer`, `…the agent's VM`, `^Run "…`, `^Use … tool … with …`), which it hides.
pub fn summary_for(tool: &str, arguments: &Value) -> String {
    match tool {
        USER_MACHINE_SHELL => format!(
            "Command on your own computer: {}",
            clip(string_arg(arguments, "command").unwrap_or("(none)"), 200)
        ),
        "shell" => format!(
            "Command on the agent's own box: {}",
            clip(string_arg(arguments, "command").unwrap_or("(none)"), 200)
        ),
        "read_file" => format!(
            "Read {} on the agent's own box",
            clip(string_arg(arguments, "path").unwrap_or("a file"), 200)
        ),
        "write_file" => format!(
            "Write {} bytes to {} on the agent's own box",
            string_arg(arguments, "content").map_or(0, str::len),
            clip(string_arg(arguments, "path").unwrap_or("a file"), 200)
        ),
        other => format!(
            "{other} — a plugin tool this agent wants to call, with {}",
            clip(&opengrok_tools::redact_arguments(arguments), 160)
        ),
    }
}

/// The pre-filled "Always allow" text. The client appends it to the coworker tier's allow
/// instructions, so it must read as an instruction, not a label.
pub fn proposed_rule(tool: &str, arguments: &Value) -> String {
    match tool {
        USER_MACHINE_SHELL => format!(
            "Allow `{}` on my own computer.",
            clip(string_arg(arguments, "command").unwrap_or(""), 200)
        ),
        "shell" => format!(
            "Allow `{}` on the agent's own box.",
            clip(string_arg(arguments, "command").unwrap_or(""), 200)
        ),
        "read_file" => format!(
            "Allow reading {} on the agent's own box.",
            clip(string_arg(arguments, "path").unwrap_or(""), 200)
        ),
        "write_file" => format!(
            "Allow writing {} on the agent's own box.",
            clip(string_arg(arguments, "path").unwrap_or(""), 200)
        ),
        other => format!("Allow the {other} tool."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boilerplate(summary: &str) -> bool {
        summary.starts_with("Run \"")
            || summary.starts_with("Use ")
            || summary.contains("Run a command on your local computer")
            || summary.contains("the agent's VM")
    }

    #[test]
    fn surfaces_by_tool() {
        assert_eq!(surface_for(USER_MACHINE_SHELL), "host_shell");
        assert_eq!(surface_for("shell"), "box_shell");
        assert_eq!(surface_for("read_file"), "box_shell");
        assert_eq!(surface_for("gmail.api.send"), "mcp");
        assert_eq!(surface_for("something_else"), "computer");
    }

    #[test]
    fn command_only_for_the_shells_and_summaries_never_boilerplate() {
        let args = json!({ "command": "brew install jq", "path": "/etc/hosts", "content": "abc" });
        for tool in [USER_MACHINE_SHELL, "shell"] {
            assert_eq!(command_for(tool, &args).as_deref(), Some("brew install jq"));
        }
        for tool in ["read_file", "write_file", "gmail.api.send"] {
            assert_eq!(command_for(tool, &args), None);
        }
        for tool in [USER_MACHINE_SHELL, "shell", "read_file", "write_file", "gmail.api.send"] {
            let summary = summary_for(tool, &args);
            assert!(!summary.is_empty());
            assert!(!boilerplate(&summary), "{tool}: {summary}");
        }
        assert!(summary_for("write_file", &args).contains("3 bytes to /etc/hosts"));
    }

    #[test]
    fn the_card_carries_the_transcribed_shape_and_settles_on_the_same_id() {
        let args = json!({ "command": "brew install jq" });
        let pending = auto_review_card("e_1", "call_1", "pending", USER_MACHINE_SHELL, &args, Some("why"), 7);
        assert_eq!(pending["kind"], "send-message");
        assert_eq!(pending["id"], "e_1");
        assert_eq!(pending["message"]["type"], "auto-review-approval");
        let approval = &pending["message"]["approval"];
        assert_eq!(approval["requestId"], "call_1");
        assert_eq!(approval["status"], "pending");
        assert_eq!(approval["surface"], "host_shell");
        assert_eq!(approval["command"], "brew install jq");
        assert_eq!(approval["reason"], "why");
        assert!(approval["summary"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(approval["proposedRule"], "Allow `brew install jq` on my own computer.");

        let settled = auto_review_card("e_1", "call_1", "approved", USER_MACHINE_SHELL, &args, Some("why"), 8);
        assert_eq!(settled["id"], pending["id"]);
        assert_eq!(settled["message"]["approval"]["status"], "approved");
    }
}
