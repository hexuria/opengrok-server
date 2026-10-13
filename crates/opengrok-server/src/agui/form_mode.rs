//! Form mode (#220, Tier 1): a card the chat box drives, whose boxes a plugin holds.
//!
//! The plugin's `extensions.formMode` names its tools (`opengrok_plugins::form_mode`); this module
//! is where the server acts on them: the tools that always ask, the card a `request_user_form`
//! with `formMode` mints without parking its run, and the card's actions, which a person's press
//! runs through the same executor and gates as a model's call, with no turn in between.

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_plugins::FormModeTools;
use opengrok_tools::user_form::form_mode;
use opengrok_tools::{ToolCall, ToolResult};
use serde_json::{Value, json};

use super::AgUiState;
use crate::host_state::HostState;
use crate::now_ms;

/// `policy` with every form-mode plugin's `alwaysAsk` tools held for a person's yes. Read on
/// every turn and every door, like the grant it narrows, so a Bot's model can never send a tax
/// return, or a queued one early, without somebody saying yes.
pub(crate) async fn always_asking(
    state: &AgUiState,
    account: &AccountId,
    coworker: &CoworkerId,
    policy: opengrok_policy::Context,
) -> opengrok_policy::Context {
    let store = &state.auth.store;
    let plugins =
        opengrok_integrations::turn::form_mode_plugins(store, account, coworker, &state.plugins);
    let tools: Vec<String> = plugins
        .await
        .iter()
        .flat_map(|(plugin, tools)| tools.always_ask_tools(plugin))
        .collect();
    opengrok_policy::with_always_ask(policy, &tools)
}

/// The plugin a form-mode card is for: the one the call names, or the only one this Bot has that
/// declares form mode. `Err` says why there is none, in words the person reads on the card.
pub(crate) async fn plugin_for(
    state: &AgUiState,
    account: &AccountId,
    coworker: &CoworkerId,
    named: Option<&str>,
) -> Result<(String, FormModeTools), String> {
    let store = &state.auth.store;
    let plugins =
        opengrok_integrations::turn::form_mode_plugins(store, account, coworker, &state.plugins);
    let mut plugins = plugins.await;
    match named {
        Some(name) => plugins
            .into_iter()
            .find(|(plugin, _)| plugin == name)
            .ok_or_else(|| format!("the {name} plugin has no form mode for this Bot")),
        None if plugins.len() > 1 => {
            Err("several plugins have a form mode; name one as plugin".to_string())
        }
        None => plugins
            .pop()
            .ok_or_else(|| "no plugin of this Bot has a form mode".to_string()),
    }
}

/// One plugin call a card makes: which plugin and tool, with what, and whether a person's press
/// is the yes the tool asks for.
pub(crate) struct PluginCall<'a> {
    pub plugin: &'a str,
    pub tool: String,
    pub arguments: Value,
    /// The press is the person's yes (Submit). Stamped as an allow-once for exactly this tool and
    /// these arguments, as the person, and spent by the gate: the gate still runs, and a model's
    /// call to the same tool still parks on its own card.
    pub yes: bool,
    /// What the audit row is filed under: the card's entry id.
    pub request_id: &'a str,
}

/// Run one plugin tool for a card, through the same executor and gates as a model's call, with
/// the card's plugin admitted for this call by `turn::mentioned`'s rule (the owner's own Bot and
/// plugin only). Held under the coworker's lock, so a press cannot interleave with an MCP door
/// call or another press on the same Bot, and audited like a door call: a press has no run to
/// journal it.
pub(crate) async fn run_plugin_tool(
    host: &HostState,
    account: &AccountId,
    coworker: &CoworkerId,
    call: PluginCall<'_>,
) -> ToolResult {
    let state = &host.agui;
    let store = &state.auth.store;
    let lock = crate::mcp_door::coworker_lock(coworker);
    let _guard = lock.lock().await;
    let mut id = format!("form_{}", uuid::Uuid::now_v7().simple());
    let mut approved = Vec::new();
    if call.yes {
        let (tool, arguments) = (&call.tool, &call.arguments);
        let owner = Some(account.as_str());
        let remembered = crate::mcp_door::remember_mcp_allow_once(
            store, coworker, owner, tool, arguments, &id, true,
        )
        .await;
        if let Err(error) = remembered {
            tracing::error!(%error, "a card's yes could not be recorded; nothing was sent");
        }
        let taken =
            crate::mcp_door::take_mcp_allow_once_stamped(store, coworker, owner, tool, arguments);
        if let Some((yes, true, _)) = taken.await {
            id = yes;
            approved.push(id.clone());
        }
    }
    let asked = vec![call.plugin.to_string()];
    let mentioned =
        opengrok_integrations::turn::mentioned(store, account, coworker, &asked, &state.plugins);
    let turn = opengrok_integrations::turn::TurnPlugins {
        mentioned: mentioned.await,
        ..Default::default()
    };
    let gates: (&[String], &[String]) = (&approved, &[]);
    let patience = super::routes::TURN_WAKE_PATIENCE;
    let runner = super::routes::tools_for_turn(state, account, coworker, gates, patience, &turn);
    let tool_call = ToolCall {
        id,
        name: call.tool,
        arguments: call.arguments,
    };
    let result = match runner.await {
        Some(runner) => runner.run_one(&tool_call).await,
        None => ToolResult::refused(&tool_call.id, "this Bot's tools could not be loaded"),
    };
    let outcome = match (result.ok, result.awaiting_approval) {
        (_, true) => "awaiting",
        (true, _) => "ok",
        (false, _) => "failed",
    };
    // The judge's redaction is JSON text, clipped with a stated cut when long; a clipped one no
    // longer parses, and is kept as the string it is, as the door keeps it.
    let redacted = opengrok_tools::redact_arguments(&tool_call.arguments);
    let row = opengrok_store::NewMcpCall {
        call_id: &tool_call.id,
        tool: &tool_call.name,
        arguments: serde_json::from_str(&redacted).unwrap_or(Value::String(redacted.clone())),
        outcome,
        request_id: call.request_id,
        at_ms: now_ms(),
    };
    if let Err(error) = store.insert_mcp_call(account, coworker, &row).await {
        tracing::error!(%error, tool = %tool_call.name, "a card's plugin call ran but its audit row was not written");
    }
    result
}

/// A plugin's answer as the JSON it sent, or why it is not one. A refusal's words are kept: they
/// are the plugin's or the gate's reason, and the person needs it.
pub(crate) fn reply_of(result: &ToolResult) -> Result<Value, String> {
    if result.awaiting_approval {
        return Err(format!(
            "this needs a person's yes first: {}",
            result.content
        ));
    }
    if !result.ok {
        return Err(result.content.clone());
    }
    serde_json::from_str(&result.content)
        .map_err(|_| "the plugin did not answer with a form".to_string())
}

/// Mint the card a form-mode `request_user_form` asked for, from the plugin's card tool, and put
/// it in the transcript. The call has already ended its turn; this is the card it promised.
///
/// NO `callId`. Nothing is parked on this card, and every path that reads a `callId` (the hold
/// sweep, a submit's resume, the replay's join) would tie it to a run it is not. The model's call
/// is kept as `requestCallId`, so a client can join the call's frames to the card on a replay.
///
/// A card the plugin could not open is still minted, settled `fill_failed` with the reason as
/// its note: the model was told the form is open, and both the person and the Bot's next turn
/// must be able to see that it is not.
pub(crate) async fn mint(
    host: &HostState,
    (account, coworker): (&AccountId, &CoworkerId),
    thread_id: &str,
    call_id: &str,
    arguments: &Value,
) -> Option<Value> {
    let entry_id = format!("e_{}", uuid::Uuid::now_v7());
    let key = arguments.get("form").cloned().unwrap_or(Value::Null);
    let named = arguments.get("plugin").and_then(Value::as_str);
    let card = match plugin_for(&host.agui, account, coworker, named).await {
        Ok((plugin, tools)) => {
            let call = PluginCall {
                plugin: &plugin,
                tool: tools.card_tool(&plugin),
                arguments: json!({ "form": key, "mode": "index" }),
                yes: false,
                request_id: &entry_id,
            };
            let result = run_plugin_tool(host, account, coworker, call).await;
            reply_of(&result).and_then(|reply| form_mode::card_request(&plugin, &reply))
        }
        Err(why) => Err(why),
    };
    let mut entry = json!({
        "kind": "send-message",
        "id": entry_id,
        "timestampMs": now_ms(),
        "threadId": thread_id,
        "requestCallId": call_id,
        "message": { "type": "user-form" },
    });
    match card {
        Ok(card) => entry["message"]["formRequest"] = json!(card),
        Err(why) => {
            let mut failed = form_mode::sanitize_request(arguments);
            failed["fields"] = json!([]);
            failed["notes"] = json!([format!("The form could not be opened: {why}")]);
            entry["message"]["formRequest"] = failed;
            entry["formResolution"] = json!("fill_failed");
        }
    }
    let appended = host
        .agui
        .auth
        .store
        .append_gateway_entry(coworker, account, &entry, now_ms())
        .await;
    match appended {
        Ok(_) => Some(entry),
        Err(error) => {
            tracing::error!(%error, "could not append a form-mode card");
            None
        }
    }
}
