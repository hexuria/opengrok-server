//! Transcript cards the server emits when a run suspends — pure builders, unit-tested, with the
//! renderer's rules written next to the field they constrain. Shapes are TRANSCRIBED from the
//! desktop client (`docs/AUTO-REVIEW.md` §5 for the auto-review card), never invented.

use serde_json::{Value, json};

use opengrok_tools::USER_MACHINE_SHELL;

/// The `auto-review-approval` card for the judge's ask. Re-emit with the SAME `entry_id` and a
/// new `status` to settle it — the renderer dedups on `auto-review-approval:${requestId}:${status}`.
pub fn auto_review_card(
    entry_id: &str,
    request_id: &str,
    status: &str,
    tool: &str,
    arguments: &Value,
    reason: Option<&str>,
    timestamp_ms: i64,
) -> Value {
    approval_card(
        entry_id,
        request_id,
        status,
        tool,
        arguments,
        reason,
        Some(proposed_rule(tool, arguments)),
        timestamp_ms,
    )
}

/// The SAME card for a policy grant's "needs a human yes" — the client's shape, reused rather
/// than a new type its closed card inventory would reject. Two differences, both in optional
/// fields: `reason` is the grant's sentence, and `proposedRule` is absent. Without a rule the
/// client's "Always allow" is a plain approve that writes nothing
/// (`transcript-card/auto-review-actions.ts:149-150`), which is right: a policy grant is widened
/// in policy, never from a card. The server tells the two asks apart by the run's suspend reason.
pub fn policy_approval_card(
    entry_id: &str,
    request_id: &str,
    status: &str,
    tool: &str,
    arguments: &Value,
    why: Option<&str>,
    timestamp_ms: i64,
) -> Value {
    approval_card(
        entry_id,
        request_id,
        status,
        tool,
        arguments,
        Some(
            why.filter(|why| !why.is_empty())
                .unwrap_or(POLICY_ASK_REASON),
        ),
        None,
        timestamp_ms,
    )
}

/// What the card says when the grant gave no reason of its own.
pub const POLICY_ASK_REASON: &str =
    "This coworker's policy needs a person to say yes before it may run this tool.";

#[allow(clippy::too_many_arguments)]
fn approval_card(
    entry_id: &str,
    request_id: &str,
    status: &str,
    tool: &str,
    arguments: &Value,
    reason: Option<&str>,
    proposed_rule: Option<String>,
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
    if let Some(rule) = proposed_rule {
        approval["proposedRule"] = json!(rule);
    }
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
        for tool in [
            USER_MACHINE_SHELL,
            "shell",
            "read_file",
            "write_file",
            "gmail.api.send",
        ] {
            let summary = summary_for(tool, &args);
            assert!(!summary.is_empty());
            assert!(!boilerplate(&summary), "{tool}: {summary}");
        }
        assert!(summary_for("write_file", &args).contains("3 bytes to /etc/hosts"));
    }

    #[test]
    fn the_card_carries_the_transcribed_shape_and_settles_on_the_same_id() {
        let args = json!({ "command": "brew install jq" });
        let pending = auto_review_card(
            "e_1",
            "call_1",
            "pending",
            USER_MACHINE_SHELL,
            &args,
            Some("why"),
            7,
        );
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
        assert_eq!(
            approval["proposedRule"],
            "Allow `brew install jq` on my own computer."
        );

        let settled = auto_review_card(
            "e_1",
            "call_1",
            "approved",
            USER_MACHINE_SHELL,
            &args,
            Some("why"),
            8,
        );
        assert_eq!(settled["id"], pending["id"]);
        assert_eq!(settled["message"]["approval"]["status"], "approved");
    }

    #[test]
    fn a_policy_ask_is_the_same_card_with_the_grants_reason_and_no_rule() {
        let args = json!({ "command": "rm -rf build" });
        let card = policy_approval_card(
            "e_2",
            "call_2",
            "pending",
            "shell",
            &args,
            Some("only the on-call may clean builds"),
            9,
        );
        assert_eq!(card["message"]["type"], "auto-review-approval");
        let approval = &card["message"]["approval"];
        assert_eq!(approval["requestId"], "call_2");
        assert_eq!(approval["surface"], "box_shell");
        assert_eq!(approval["command"], "rm -rf build");
        assert_eq!(approval["reason"], "only the on-call may clean builds");
        assert!(
            approval.get("proposedRule").is_none(),
            "a policy card offers no rule: {approval}"
        );
        let unexplained = policy_approval_card("e_3", "call_3", "pending", "shell", &args, None, 9);
        assert_eq!(
            unexplained["message"]["approval"]["reason"],
            POLICY_ASK_REASON
        );
    }
}
