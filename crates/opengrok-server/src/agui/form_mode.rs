//! Form mode (#220, Tier 1): a card the chat box drives, whose boxes a plugin holds.
//!
//! The plugin's `extensions.formMode` names its tools (`opengrok_plugins::form_mode`); this module
//! is where the server acts on them: the tools that always ask, the card a `request_user_form`
//! with `formMode` mints without parking its run, and the card's actions, which a person's press
//! runs through the same executor and gates as a model's call, with no turn in between.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_plugins::FormModeTools;
use opengrok_tools::user_form::{FormRequest, form_mode, form_request_from, submitted_values};
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

/// `POST /ag-ui/user-form/action`: one press on a form-mode card. Authenticated like the card's
/// other verbs (`/ag-ui/user-form/submit`): the account bearer, never a host or bot key.
pub fn router(host: HostState) -> Router {
    Router::new()
        .route("/ag-ui/user-form/action", post(action_route))
        .with_state(host)
}

async fn action_route(
    State(host): State<HostState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account) = super::routes::account_from_bearer(&host.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = act(&host, &args, &account).await;
    let code = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, axum::Json(body)).into_response()
}

/// `{entryId, agentId, action: save|submit|view|focus|refresh, values?, fingerprint?, key?,
/// page?}`. The action's tool comes from the card's plugin's manifest; the arguments are built
/// here from the card as stored, so a press can name only the card's own return. Replies with
/// the plugin's answer (`reply`) and, when it answered a CardReply, the card it makes.
pub async fn act(host: &HostState, args: &Value, account: &AccountId) -> (u16, Value) {
    let Some((entry_id, coworker)) = super::user_form::named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if !opengrok_plugins::form_mode::ACTIONS.contains(&action) {
        let error = "action must be save, submit, view, focus or refresh";
        return (400, json!({ "error": error }));
    }
    let loaded = super::user_form::load_owned_entry(host, account, &coworker, &entry_id).await;
    let (seq, entry) = match loaded {
        Ok(row) => row,
        Err(reply) => return reply,
    };
    if !form_mode::is_form_mode_entry(&entry) {
        return (
            400,
            json!({ "error": "that entry is not a form-mode card" }),
        );
    }
    let form = form_request_from(&entry);
    let (Some(plugin), Some(key)) = (form.plugin.clone(), form.form.clone()) else {
        return (
            409,
            json!({ "error": "this card names no form; it cannot be acted on" }),
        );
    };
    let arguments = match arguments_for(action, &form, &key, args) {
        Ok(arguments) => arguments,
        Err(why) => return (400, json!({ "error": why })),
    };
    let tools = match plugin_for(&host.agui, account, &coworker, Some(&plugin)).await {
        Ok((_, tools)) => tools,
        Err(why) => return (409, json!({ "error": why })),
    };
    let Some(tool) = tools.action_tool(&plugin, action) else {
        let error = format!("the {plugin} plugin has no {action} action");
        return (409, json!({ "error": error }));
    };
    let call = PluginCall {
        plugin: &plugin,
        tool,
        arguments,
        yes: action == "submit",
        request_id: &entry_id,
    };
    let result = run_plugin_tool(host, account, &coworker, call).await;
    if result.awaiting_approval {
        let error = format!("this needs a person's yes first: {}", result.content);
        return (409, json!({ "ok": false, "error": error }));
    }
    let reply = match reply_of(&result) {
        Ok(reply) => reply,
        Err(why) => return (502, json!({ "ok": false, "error": why })),
    };
    let mut body = json!({ "ok": true, "action": action, "reply": reply });
    if action == "submit" {
        return submitted(host, (account, &coworker), seq, entry, body).await;
    }
    // A CardReply (save, refresh) is the card as it now is. Kept on the entry only on refresh,
    // which is the card being (re)opened: a save runs on every blur, and the draft it saves is
    // the plugin's to keep, not the transcript's.
    if matches!(action, "save" | "refresh")
        && let Ok(card) = form_mode::card_request(&plugin, &body["reply"])
    {
        body["formRequest"] = json!(card);
        if action == "refresh" {
            let mut opened = entry.clone();
            opened["message"]["formRequest"] = json!(card);
            if let Err(error) = host
                .agui
                .auth
                .store
                .update_gateway_entry(&coworker, account, seq, &opened)
                .await
            {
                tracing::error!(%error, "a refreshed form-mode card could not be kept");
            }
        }
    }
    (200, body)
}

/// A submit's answer: on a queued return the card is settled submitted, with the plugin's
/// snapshot, before the person is told; on a refusal the card stays a draft and the first error
/// goes back as the plugin said it.
async fn submitted(
    host: &HostState,
    (account, coworker): (&AccountId, &CoworkerId),
    seq: i64,
    entry: Value,
    mut body: Value,
) -> (u16, Value) {
    let form_mode::SubmitOutcome::Queued {
        queue_ref,
        fingerprint,
        values,
    } = form_mode::submit_outcome(&body["reply"])
    else {
        body["ok"] = json!(false);
        return (200, body);
    };
    let settled = settle_submitted(entry, &queue_ref, fingerprint.as_deref(), &values);
    let store = &host.agui.auth.store;
    if let Err(error) = store
        .update_gateway_entry(coworker, account, seq, &settled)
        .await
    {
        // The return IS queued: saying otherwise would invite a second press. The card is left a
        // draft in the transcript, and the plugin's queue still refuses a second send.
        tracing::error!(%error, %queue_ref, "a submitted form-mode card could not be settled");
    }
    body["entry"] = settled;
    (200, body)
}

/// A press's arguments, built from the card as stored: its identity always, and from the press
/// only what that action takes. Values are kept for the card's own boxes the person may type in;
/// a key the card does not have, or a box the plugin computes, is dropped.
fn arguments_for(
    action: &str,
    form: &FormRequest,
    key: &form_mode::FormKey,
    args: &Value,
) -> Result<Value, &'static str> {
    let identity = key.to_value();
    Ok(match action {
        "save" => {
            let raw = args.get("values").unwrap_or(&Value::Null);
            let values: serde_json::Map<String, Value> = submitted_values(form, raw)
                .into_iter()
                .filter(|(id, _)| form.fields.iter().any(|f| &f.id == id && !f.read_only))
                .map(|(id, value)| (id, json!(value)))
                .collect();
            if values.is_empty() {
                return Err("save needs values for the card's own boxes");
            }
            json!({ "form": identity, "values": values })
        }
        // The fingerprint the person saw: the plugin refuses a draft that has changed since.
        "submit" => {
            let seen = args.get("fingerprint").and_then(Value::as_str);
            let Some(fingerprint) = seen.or(form.fingerprint.as_deref()) else {
                return Err("submit needs the fingerprint of the draft the person saw");
            };
            json!({ "form": identity, "fingerprint": fingerprint })
        }
        "view" => json!({ "form": identity, "inline": true }),
        "focus" => {
            let field = args.get("key").and_then(Value::as_str);
            let Some(field) = field.filter(|k| form.fields.iter().any(|f| f.id == *k)) else {
                return Err("focus needs the key of one of the card's boxes");
            };
            json!({ "form": identity, "key": field })
        }
        _ => match args.get("page").and_then(Value::as_u64) {
            Some(page) => json!({ "form": identity, "mode": "page", "page": page }),
            None => json!({ "form": identity, "mode": "index" }),
        },
    })
}
/// The person's submit was queued: the card becomes its submitted self. The values the plugin
/// says it queued are the snapshot the read-only view is drawn from from now on, never BIR
/// again (a queued return may not reopen as a draft), and what the Bot is told on its next turn.
pub(crate) fn settle_submitted(
    mut entry: Value,
    queue_ref: &str,
    fingerprint: Option<&str>,
    values: &serde_json::Map<String, Value>,
) -> Value {
    if let Some(fields) = entry
        .pointer_mut("/message/formRequest/fields")
        .and_then(Value::as_array_mut)
    {
        for field in fields {
            let id = field.get("id").and_then(Value::as_str).unwrap_or_default();
            if let Some(value) = values.get(id) {
                field["value"] = value.clone();
            }
            field["readOnly"] = json!(true);
            if let Some(map) = field.as_object_mut() {
                map.remove("error");
            }
        }
    }
    if let Some(request) = entry.pointer_mut("/message/formRequest") {
        request["status"] = json!("queued");
    }
    entry["formResolution"] = json!("submitted");
    entry["formOutcome"] = json!({
        "queueRef": queue_ref,
        "fingerprint": fingerprint,
        "values": values,
        "atMs": now_ms(),
    });
    entry
}

/// What this thread's form-mode cards tell the Bot on its next turn (`form_mode::history_line`):
/// the last few, oldest first. Empty when there are none, so a turn without a form is
/// byte-for-byte the turn it was.
pub(crate) async fn turn_line(
    state: &AgUiState,
    account: &AccountId,
    coworker: &CoworkerId,
    thread_id: &str,
) -> String {
    let Ok(entries) = state.auth.store.gateway_transcript(coworker, account).await else {
        return String::new();
    };
    let lines: Vec<String> = entries
        .iter()
        .filter(|entry| entry.get("threadId").and_then(Value::as_str) == Some(thread_id))
        .filter_map(form_mode::history_line)
        .collect();
    let recent = &lines[lines.len().saturating_sub(TURN_LINE_CARDS)..];
    if recent.is_empty() {
        return String::new();
    }
    format!("\n\nForms in this conversation:\n- {}", recent.join("\n- "))
}

/// How many of a thread's form-mode cards a turn is told about.
const TURN_LINE_CARDS: usize = 3;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../../tests/unit/form_mode.rs"]
mod tests;
