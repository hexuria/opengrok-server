//! The computer tools (7 Oct 2026, the owner's step 7): a Bot reads and works its own computer,
//! status, start, shut down, restart, reset, update and its network setting, when the person asks
//! in chat, through the same functions the Computer pane's routes use (opengrok-server
//! `computer_desk.rs`), so a tool may do what the routes may, refused in the same words.
//!
//! RUN INSIDE `Executor::execute`, LIKE THE ROUTINE AND PLUGIN TOOLS: the ceiling, the grant and an
//! ask apply as to every built-in. One ceiling row switches all of them (`ROW`). What cannot be
//! undone asks first: a reset (the computer's files go), an update (the computer is rebuilt), and
//! the network setting (what the computer may reach). The rest just happen and are reported.
//!
//! THIS BOT'S OWN COMPUTER ONLY: the desk answers as the `ToolContext`'s account and coworker, and
//! no argument names another Bot's computer.

use serde_json::{Value, json};

use crate::{ToolContext, ToolResult};

pub const COMPUTER_STATUS: &str = "computer_status";
pub const START_COMPUTER: &str = "start_computer";
pub const SHUTDOWN_COMPUTER: &str = "shutdown_computer";
pub const RESTART_COMPUTER: &str = "restart_computer";
pub const RESET_COMPUTER: &str = "reset_computer";
pub const UPDATE_COMPUTER: &str = "update_computer";
pub const SET_NETWORK: &str = "set_network";

/// Every computer tool, in the order they are offered: reading first, then the switches, then
/// what cannot be undone.
pub const TOOLS: [&str; 7] = [
    COMPUTER_STATUS,
    START_COMPUTER,
    SHUTDOWN_COMPUTER,
    RESTART_COMPUTER,
    RESET_COMPUTER,
    UPDATE_COMPUTER,
    SET_NETWORK,
];

/// The ceiling row that switches them all (`GET`/`PUT /coworkers/{id}/ceiling`). Not `computer`,
/// which is the built-in that looks at the screen and clicks.
pub const ROW: &str = "manage_computer";
pub const ROW_LABEL: &str = "Manage computer";
pub const ROW_DESCRIPTION: &str = "Check, start, shut down, restart, reset and update this Bot's own \
     computer, and set what it may reach on the network, when you ask in chat. Resetting, updating \
     and the network setting always ask you first.";

pub fn is_computer_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// The network settings a computer can have, as the Computer pane offers them: everything, only
/// through this Mac (always), ask each time, or nothing.
pub const NETWORK_MODES: [&str; 3] = ["always", "ask", "never"];

/// One call, read and checked as far as its arguments go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Status,
    Start,
    Shutdown,
    Restart,
    Reset,
    Update,
    SetNetwork { mode: String },
}

/// This Bot's computer, as the server keeps it. Every call is answered as `context`'s account and
/// coworker; a refusal is a sentence the model can act on.
#[async_trait::async_trait]
pub trait ComputerDesk: Send + Sync {
    /// Carry out `ask`: what the model is told back, or why not.
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String>;
    /// For a call that asks first, the card's sentence; `None` for one that just happens.
    async fn ask_first(&self, context: &ToolContext, ask: &Ask) -> Result<Option<String>, String>;
}

/// What `Executor::execute` holds before its gates, for a computer tool: the call read, and the
/// card's sentence when it asks first; or the refusal it earns now.
pub async fn admit(
    desk: &dyn ComputerDesk,
    context: &ToolContext,
    (name, arguments): (&str, &Value),
) -> Result<(Ask, Option<String>), String> {
    let ask = read(name, arguments)?;
    let card = desk.ask_first(context, &ask).await?;
    Ok((ask, card))
}

/// Carry out an admitted call: its JSON, or the refusal, as a result either way.
pub async fn run(
    desk: &dyn ComputerDesk,
    context: &ToolContext,
    call_id: &str,
    ask: Ask,
) -> ToolResult {
    match desk.answer(context, ask).await {
        Ok(answer) => ToolResult::ok(call_id, answer.to_string()),
        Err(why) => ToolResult::refused(call_id, why),
    }
}

fn read(name: &str, arguments: &Value) -> Result<Ask, String> {
    match name {
        COMPUTER_STATUS => Ok(Ask::Status),
        START_COMPUTER => Ok(Ask::Start),
        SHUTDOWN_COMPUTER => Ok(Ask::Shutdown),
        RESTART_COMPUTER => Ok(Ask::Restart),
        RESET_COMPUTER => Ok(Ask::Reset),
        UPDATE_COMPUTER => Ok(Ask::Update),
        SET_NETWORK => match arguments.get("mode").and_then(Value::as_str).map(str::trim) {
            Some(mode) if NETWORK_MODES.contains(&mode) => Ok(Ask::SetNetwork {
                mode: mode.to_string(),
            }),
            _ => Err("bad arguments: mode is always, ask or never".to_string()),
        },
        other => Err(format!("there is no computer tool called {other}")),
    }
}

/// What a tool says it is for, as a turn offers it and a ceiling describes it; the group's row
/// for `ROW`.
pub fn description(name: &str) -> Option<&'static str> {
    Some(match name {
        COMPUTER_STATUS => {
            "Read THIS BOT'S OWN computer's state: whether it is running, starting, stopped or \
             updating, whether its screen is up, and its network setting."
        }
        START_COMPUTER => "Start THIS BOT'S OWN computer when it is stopped. Its files are kept.",
        SHUTDOWN_COMPUTER => {
            "Shut down THIS BOT'S OWN computer. Its files are kept, and it starts again on the \
             next turn that needs it or when start_computer is called."
        }
        RESTART_COMPUTER => {
            "Restart THIS BOT'S OWN computer: shut it down and start it again. Its files are kept."
        }
        RESET_COMPUTER => {
            "Reset THIS BOT'S OWN computer to a fresh one: everything on it is deleted for good. \
             It always asks the person first."
        }
        UPDATE_COMPUTER => {
            "Update THIS BOT'S OWN computer to the newest image, keeping its files; it is rebuilt \
             and is unavailable meanwhile. It always asks the person first."
        }
        SET_NETWORK => {
            "Set what THIS BOT'S OWN computer may reach through the person's network: always, ask \
             each time, or never. It always asks the person first."
        }
        ROW => ROW_DESCRIPTION,
        _ => return None,
    })
}

/// The function definition a turn offers for `name`.
pub fn schema(name: &str) -> Option<Value> {
    let description = description(name)?;
    let (properties, required) = match name {
        SET_NETWORK => (
            json!({ "mode": { "type": "string", "enum": NETWORK_MODES,
                "description": "always, ask (each time) or never." } }),
            json!(["mode"]),
        ),
        n if is_computer_tool(n) => (json!({}), json!([])),
        _ => return None,
    };
    let parameters = json!({ "type": "object", "properties": properties, "required": required });
    Some(json!({ "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters } }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Every tool has its words and a schema; set_network takes only the three settings.
    #[test]
    fn every_computer_tool_reads_and_describes_itself() {
        for name in TOOLS {
            assert!(description(name).is_some(), "{name}");
            assert!(schema(name).is_some(), "{name}");
        }
        assert_eq!(
            read(SET_NETWORK, &json!({ "mode": "ask" })),
            Ok(Ask::SetNetwork { mode: "ask".into() })
        );
        assert!(read(SET_NETWORK, &json!({ "mode": "sometimes" })).is_err());
        assert!(read(SET_NETWORK, &json!({})).is_err());
        assert_eq!(read(RESET_COMPUTER, &json!({})), Ok(Ask::Reset));
    }
}
