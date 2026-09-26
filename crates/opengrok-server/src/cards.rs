//! Transcript cards the server emits when a run suspends — pure builders, unit-tested, with the
//! renderer's rules written next to the field they constrain. Shapes are TRANSCRIBED from the
//! desktop client (`docs/AUTO-REVIEW.md` §5 for the auto-review card), never invented.

use serde_json::{Value, json};

use opengrok_tools::USER_MACHINE_SHELL;
use opengrok_tools::user_form::sanitize_arguments;

/// The `auto-review-approval` card for the judge's ask. Re-emit with the SAME `entry_id` and a
/// new `status` to settle it — the renderer dedups on `auto-review-approval:${requestId}:${status}`.
///
/// The egress tunnel's ask shares the card and the suspend reason but carries NO rule: its
/// "Always allow" wrote "Allow the computer tool." into the judge's allow text, which switched
/// the judge on for every later call of that coworker instead of answering the tunnel (#165).
/// The tunnel's standing answer is the computer's egress policy.
pub fn auto_review_card(
    entry_id: &str,
    request_id: &str,
    status: &str,
    tool: &str,
    arguments: &Value,
    reason: Option<&str>,
    timestamp_ms: i64,
) -> Value {
    let rule = (reason != Some(opengrok_tools::review::EGRESS_TUNNEL_ASK_REASON))
        .then(|| proposed_rule(tool, arguments));
    approval_card(
        entry_id,
        request_id,
        status,
        tool,
        arguments,
        reason,
        rule,
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

/// The in-chat `user-form` card. Transcribed from official 0.29/0.30 `user-form/view.tsx`:
/// `message.type` is `user-form`, `formRequest` is the field schema, and `formResolution` is a
/// sibling of `message` (not inside it). Identity keys and a login card's field values are
/// dropped so a password cannot sit on the entry. A collect card keeps non-secret `value`
/// prefills. `sanitize_arguments` is what enforces that split.
pub fn user_form_card(
    entry_id: &str,
    arguments: &Value,
    timestamp_ms: i64,
    call_id: &str,
) -> Value {
    let mut card = json!({
        "kind": "send-message",
        "id": entry_id,
        "timestampMs": timestamp_ms,
        "message": {
            "type": "user-form",
            "formRequest": sanitize_arguments(arguments),
        },
    });
    // Join to the TOOL_CALL / CUSTOM `callId` so stacked same-completion cards
    // submit independently (only the matching pending call resumes).
    if !call_id.is_empty()
        && let Some(map) = card.as_object_mut()
    {
        map.insert("callId".to_string(), json!(call_id));
    }
    card
}

/// Grok Bot computer-handoff chrome — transcribed from the recovered renderer:
/// attachment at `sand://box`, box fields on the **entry** (`boxRequestId`,
/// `boxInstruction`, `boxResolution`). There is no `computer-handoff` message type
/// in the shipped contract. A stray `boxRequestId` on any other card (including
/// user-form) converts that card into a handoff, so escalate MUST emit this as a
/// **separate** entry. This is not OpenGrok Take over / I'm done / Skip.
///
/// `boxResolution` is ABSENT while live (locked NativeChat wire). A string
/// (`handed_back` | `declined` | `timed_out`) is stamped only on resolve.
pub fn computer_handoff_card(
    entry_id: &str,
    box_request_id: &str,
    instruction: &str,
    timestamp_ms: i64,
) -> Value {
    json!({
        "kind": "send-message",
        "id": entry_id,
        "timestampMs": timestamp_ms,
        "message": {
            "type": "attachment",
            "url": "sand://box",
            "alt": "Handed to the computer",
        },
        "boxRequestId": box_request_id,
        "boxInstruction": instruction,
    })
}

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
        "computer" | "open_url" => "computer",
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
        "computer" => screen_summary(arguments),
        "open_url" => format!(
            "Open {} in the agent's own browser",
            clip(page_of(arguments), 120)
        ),
        // The recipe's `values` stay off the card: a login recipe is handed a password.
        opengrok_tools::RUN_RECIPE => format!(
            "Play the recipe \"{}\" on the agent's own computer",
            clip(string_arg(arguments, "recipe").unwrap_or("(unnamed)"), 80)
        ),
        other => format!(
            "{other} — a plugin tool this agent wants to call, with {}",
            clip(&opengrok_tools::redact_arguments(arguments), 160)
        ),
    }
}

/// A `computer` call in words, from the same fields `ComputerArgs` reads. An action it does not
/// know is named, not dropped; `into_action` refuses it before it reaches the screen.
fn screen_summary(arguments: &Value) -> String {
    let screen = if string_arg(arguments, "machine") == Some("group") {
        "the group's shared screen"
    } else {
        "the agent's own screen"
    };
    let point = |key: &str| {
        let xy = arguments.get(key).and_then(Value::as_array);
        let at = |i: usize| xy.and_then(|xy| xy.get(i)).and_then(Value::as_i64);
        match (at(0), at(1)) {
            (Some(x), Some(y)) => format!("({x}, {y})"),
            _ => "(no position)".to_string(),
        }
    };
    let at = point("coordinate");
    match string_arg(arguments, "action").unwrap_or("") {
        "screenshot" => format!("Take a screenshot of {screen}"),
        "click" | "left_click" => format!("Click at {at} on {screen}"),
        "right_click" => format!("Right-click at {at} on {screen}"),
        "double_click" => format!("Double-click at {at} on {screen}"),
        "move" | "mouse_move" => format!("Move the pointer to {at} on {screen}"),
        "drag" | "left_click_drag" => format!("Drag from {at} to {} on {screen}", point("to")),
        "type" => format!(
            "Type \"{}\" on {screen}",
            shown(string_arg(arguments, "text").unwrap_or(""), 60)
        ),
        "key" => format!(
            "Press {} on {screen}",
            shown(string_arg(arguments, "key").unwrap_or("a key"), 40)
        ),
        "scroll" => format!("Scroll at {at} on {screen}"),
        other => format!("Screen action \"{}\" on {screen}", clip(other, 30)),
    }
}

/// Typed text as the card may show it. The card is journalled and read by whoever holds the
/// thread, so a key the model is about to type stays off it. The whole text is checked before
/// `clip`, which would otherwise cut a token below the length the check needs. The check is the
/// judge's own and as coarse: a secret in the middle of a sentence still shows.
fn shown(text: &str, max: usize) -> String {
    if opengrok_tools::looks_like_a_secret(text) {
        "«redacted»".to_string()
    } else {
        clip(text, max)
    }
}

/// The page an `open_url` call names, without its query or fragment: a link's token rides
/// there, and the card is journalled.
fn page_of(arguments: &Value) -> &str {
    let url = string_arg(arguments, "url").unwrap_or("a page");
    url.split(['?', '#']).next().unwrap_or(url)
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
        "computer" => format!(
            "Allow the `{}` screen action on the agent's own computer.",
            clip(string_arg(arguments, "action").unwrap_or(""), 30)
        ),
        "open_url" => format!(
            "Allow opening {} in the agent's own browser.",
            clip(page_of(arguments), 120)
        ),
        opengrok_tools::RUN_RECIPE => format!(
            "Allow the recipe \"{}\" on the agent's own computer.",
            clip(string_arg(arguments, "recipe").unwrap_or(""), 80)
        ),
        other => format!("Allow the {other} tool."),
    }
}

#[cfg(test)]
#[path = "../tests/unit/cards.rs"]
mod tests;
