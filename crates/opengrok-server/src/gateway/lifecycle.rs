//! P5 and friends: the agent lifecycle, entry mutation, and the automations bridge.
//!
//! Everything here follows one honesty rule from the tiers below P4: answer with the truth we
//! have, an empty of the CORRECT CONTAINER TYPE when we have none, and a `< 500` error a person
//! can read when the feature genuinely does not exist yet — never a shape that pretends.
//!
//! The automations commands are not new machinery: they are slice 6's schedules wearing the
//! client's names. Creating an automation creates a schedule; enabling pauses and resumes it;
//! "run now" fires the same `autonomy::fire` the sweep uses. One scheduler, two vocabularies.

use serde_json::{Value, json};

use opengrok_core::id::{CoworkerId, RunId, ScheduleId};
use opengrok_core::schedule::{Schedule, ScheduleCommand};

use super::{GatewayState, live};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn account(state: &GatewayState, email: &str) -> Option<opengrok_core::account::AccountView> {
    state
        .agui
        .auth
        .store
        .account_by_email(email)
        .await
        .ok()
        .flatten()
}

/// `createAgent` — hiring, in the client's vocabulary. Dedupes by `clientNonce`, because the
/// client retries creates and two coworkers named the same are not one coworker. `caller` is the
/// account the bot is hired FOR — the signed-in person (per-account pivot), not a fixed host email.
pub async fn create_agent(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Grok");
    let Some(account) = account(state, caller).await else {
        return (
            500,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };

    // The nonce, when present, is the dedupe key. The record remembers which coworker the first
    // create made, so the retry answers with the SAME agent instead of a twin. Keyed by the caller
    // so two accounts' nonces never collide.
    if let Some(nonce) = args.get("clientNonce").and_then(Value::as_str) {
        let slot = format!("createAgent:{caller}");
        let digest = super::conversation::input_digest(args, name);
        let probe = json!({ "pending": true });
        match state
            .agui
            .auth
            .store
            .accept_nonce(&slot, nonce, &digest, &probe, now_ms())
            .await
        {
            Ok(Ok(stored)) if stored.get("pending").is_none() => {
                // The earlier create finished; answer with its coworker.
                if let Some(id) = stored.get("agentId").and_then(Value::as_str) {
                    return agent_reply(state, id).await;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(())) => {
                return (
                    409,
                    json!({ "error": "clientNonce was reused with different input" }),
                );
            }
            Err(_) => return (500, json!({ "error": "ledger unavailable" })),
        }

        let (code, reply) = hire(state, &account.id, name, model_arg(args), args).await;
        if code == 200
            && let Some(id) = reply.pointer("/agent/id").and_then(Value::as_str)
        {
            // Overwrite the pending marker with the settled fact.
            let record = json!({ "agentId": id });
            let _ = sqlx_upsert_nonce(state, &slot, nonce, &digest, &record).await;
        }
        return (code, reply);
    }

    hire(state, &account.id, name, model_arg(args), args).await
}

/// The nonce record's second write: the row exists (the probe), the fact replaces it.
async fn sqlx_upsert_nonce(
    state: &GatewayState,
    slot: &str,
    nonce: &str,
    digest: &str,
    record: &Value,
) -> Result<(), opengrok_store::StoreError> {
    // accept_nonce inserts-or-compares; updating the record afterwards needs plain SQL, which
    // lives store-side as an upsert through the same table.
    state
        .agui
        .auth
        .store
        .overwrite_nonce_record(slot, nonce, digest, record)
        .await
}

/// Hire, on a given route. `model` is the pin the caller asked for; `None` (or blank) falls back
/// to the deployment default, which is what every client that has no model field sends.
async fn hire(
    state: &GatewayState,
    account_id: &opengrok_core::id::AccountId,
    name: &str,
    model: Option<&str>,
    args: &Value,
) -> (u16, Value) {
    use crate::agui::provision;
    use opengrok_core::coworker::{Coworker, CoworkerCommand};

    let id = CoworkerId::new();
    let at_ms = now_ms();
    // The caller's pin when there is one, the deployment's default when there is not. A blank
    // string is "not one" — it would otherwise be stored and asked of the gateway verbatim.
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map_or_else(|| state.agui.model.clone(), str::to_string);
    let mut events = match Coworker::default().decide(CoworkerCommand::Hire {
        name: name.to_string(),
        model,
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => return (400, json!({ "error": reason.to_string() })),
    };
    let mut coworker = Coworker::default();
    for event in &events {
        coworker.apply(event);
    }

    // 1 account = 1 computer: the account's first agent creates it, later agents share it. Every
    // create path uses the same helper. Non-fatal: a failure leaves a boxless agent and a reason.
    let provisioned =
        provision::ensure_computer_for(&state.agui, account_id, &id, &mut coworker, at_ms).await;
    events.extend(provisioned.events);

    let view = opengrok_core::coworker::CoworkerView {
        id: id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: false,
        updated_at_ms: at_ms,
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_coworker(&id, account_id, 0, &events, &view)
        .await
    {
        tracing::error!(%error, "createAgent could not hire");
        return (500, json!({ "error": "hire failed" }));
    }

    // Grant the builtin tools so a coworker with a box can actually use it — the same ceiling REST
    // grants at hire. Without this a hired coworker silently refuses every tool.
    let tools =
        opengrok_policy::ToolSet::only(opengrok_tools::Executor::builtin_tool_names().to_vec());
    if let Err(error) = state
        .agui
        .auth
        .store
        .grant_access(
            account_id,
            &id,
            &tools,
            &tools,
            &opengrok_policy::ToolSet::None,
            at_ms,
        )
        .await
    {
        tracing::error!(%error, "createAgent could not grant access");
        return (
            500,
            json!({ "error": "the coworker was created but could not be granted" }),
        );
    }

    let profile = json!({
        "description": args.get("description").and_then(Value::as_str).unwrap_or(""),
        "title": args.get("title").and_then(Value::as_str).unwrap_or(""),
        "avatarShape": args.get("avatarShape").and_then(Value::as_str).unwrap_or(""),
        "avatarColor": args.get("avatarColor").and_then(Value::as_str).unwrap_or(""),
    });
    let _ = state
        .agui
        .auth
        .store
        .put_seamb_profile(&id, &profile, now_ms())
        .await;
    live::emit_roster(state).await;

    // Report a failed box the way REST does — without failing the create.
    let (code, mut reply) = agent_reply(state, id.as_str()).await;
    if code == 200
        && provisioned.error.is_some()
        && let Some(object) = reply.as_object_mut()
    {
        object.insert(
            "computerError".to_string(),
            provision::error_json(&provisioned.error),
        );
    }
    (code, reply)
}

/// `{agent, transcript}` — what create and duplicate answer.
async fn agent_reply(state: &GatewayState, id: &str) -> (u16, Value) {
    let Ok(rows) = live::roster_rows(state).await else {
        return (500, json!({ "error": "roster unavailable" }));
    };
    match rows.into_iter().find(|row| row["id"] == id) {
        Some(agent) => (200, json!({ "agent": agent, "transcript": [] })),
        None => (500, json!({ "error": "the hired coworker did not appear" })),
    }
}

/// `updateAgent {id, profile}` → the updated summary, or null for a stranger.
pub async fn update_agent(state: &GatewayState, args: &Value) -> (u16, Value) {
    use opengrok_core::coworker::CoworkerCommand;
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return (400, json!({ "error": "id is required" }));
    };
    let Some(account) = account(state, &state.email).await else {
        return (200, Value::Null);
    };
    let coworker_id = CoworkerId::from_stored(id.to_string());
    let owned = state
        .agui
        .auth
        .store
        .coworkers_for(&account.id)
        .await
        .map(|roster| {
            roster
                .iter()
                .any(|view| view.id == coworker_id && !view.retired)
        })
        .unwrap_or(false);
    if !owned {
        return (200, Value::Null);
    }
    let profile_args = args.get("profile").cloned().unwrap_or_else(|| args.clone());

    if let Some(name) = profile_args.get("name").and_then(Value::as_str)
        && let Ok((loaded, seq)) = state.agui.auth.store.load_coworker(&coworker_id).await
        && name != loaded.name
        && let Ok(events) = loaded.decide(CoworkerCommand::Rename {
            name: name.to_string(),
            at_ms: now_ms(),
        })
    {
        let mut after = loaded.clone();
        for event in &events {
            after.apply(event);
        }
        let view = opengrok_core::coworker::CoworkerView {
            id: coworker_id.clone(),
            name: after.name.clone(),
            model: after.model.clone(),
            box_id: after.box_id.clone(),
            retired: after.retired,
            updated_at_ms: now_ms(),
        };
        let _ = state
            .agui
            .auth
            .store
            .append_coworker(&coworker_id, &account.id, seq, &events, &view)
            .await;
    }

    // A pin can be changed the same way a name can. Its own command, so a client that sends only
    // a model does not have to restate the name (and cannot rename by accident).
    if let Some(model) = profile_args.get("model").and_then(Value::as_str)
        && let Ok((loaded, seq)) = state.agui.auth.store.load_coworker(&coworker_id).await
        && model.trim() != loaded.model
        && let Ok(events) = loaded.decide(CoworkerCommand::Repin {
            model: model.to_string(),
            at_ms: now_ms(),
        })
    {
        let mut after = loaded.clone();
        for event in &events {
            after.apply(event);
        }
        let view = opengrok_core::coworker::CoworkerView {
            id: coworker_id.clone(),
            name: after.name.clone(),
            model: after.model.clone(),
            box_id: after.box_id.clone(),
            retired: after.retired,
            updated_at_ms: now_ms(),
        };
        let _ = state
            .agui
            .auth
            .store
            .append_coworker(&coworker_id, &account.id, seq, &events, &view)
            .await;
    }

    let mut profile = state
        .agui
        .auth
        .store
        .seamb_profile(&coworker_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    if let Some(map) = profile.as_object_mut() {
        for key in ["description", "title", "avatarShape", "avatarColor"] {
            if let Some(value) = profile_args.get(key).and_then(Value::as_str) {
                map.insert(key.to_string(), json!(value));
            }
        }
    }
    let _ = state
        .agui
        .auth
        .store
        .put_seamb_profile(&coworker_id, &profile, now_ms())
        .await;
    live::emit_roster(state).await;
    let (code, reply) = agent_reply(state, id).await;
    if code != 200 {
        return (code, reply);
    }
    (200, reply["agent"].clone())
}

/// `deleteAgents {ids}` (and `deleteAgent {id}`) — retirement, in the client's vocabulary.
pub async fn delete_agents(state: &GatewayState, ids: &[String]) -> (u16, Value) {
    use opengrok_core::coworker::CoworkerCommand;
    let Some(account) = account(state, &state.email).await else {
        return (200, json!({ "deleted": 0 }));
    };
    let mut deleted = 0;
    for id in ids {
        let coworker_id = CoworkerId::from_stored(id.clone());
        if let Ok((loaded, seq)) = state.agui.auth.store.load_coworker(&coworker_id).await {
            let at_ms = now_ms();
            let mut after = loaded;
            // Retire the agent. Its box is the ACCOUNT's shared computer, not the agent's, so it is
            // not destroyed here — that happens once the account's last agent is gone (below).
            let Ok(events) = after.decide(CoworkerCommand::Retire { at_ms }) else {
                continue;
            };
            for event in &events {
                after.apply(event);
            }
            let view = opengrok_core::coworker::CoworkerView {
                id: coworker_id.clone(),
                name: after.name.clone(),
                model: after.model.clone(),
                box_id: after.box_id.clone(),
                retired: after.retired,
                updated_at_ms: at_ms,
            };
            if state
                .agui
                .auth
                .store
                .append_coworker(&coworker_id, &account.id, seq, &events, &view)
                .await
                .is_ok()
            {
                deleted += 1;
            }
            // Tear down this agent's computer per the account's mode (per-bot destroys its own box;
            // per-account destroys the account box once its last agent is gone; per-org leaves it).
            crate::agui::provision::teardown_computer_for(&state.agui, &account.id, &coworker_id)
                .await;
        }
    }
    live::emit_roster(state).await;
    (200, json!({ "deleted": deleted }))
}

/// The pin a client asked for, if it sent one. Clients that predate the field send nothing, and
/// get the deployment default — the same shape the REST hire has always had.
fn model_arg(args: &Value) -> Option<&str> {
    args.get("model").and_then(Value::as_str)
}

/// `duplicateAgent {id}` — a fresh hire wearing the original's profile.
pub async fn duplicate_agent(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return (400, json!({ "error": "id is required" }));
    };
    let source = CoworkerId::from_stored(id.to_string());
    let Ok((loaded, _)) = state.agui.auth.store.load_coworker(&source).await else {
        return (404, json!({ "error": "no such agent" }));
    };
    let profile = state
        .agui
        .auth
        .store
        .seamb_profile(&source)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    let Some(account) = account(state, &state.email).await else {
        return (
            500,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    let mut hire_args = profile.clone();
    if let Some(map) = hire_args.as_object_mut() {
        map.remove("avatarDataUrl");
    }
    // The copy thinks with the same route as its original. Re-hiring on the deployment default
    // would silently retarget a duplicate of a deliberately-pinned coworker.
    hire(
        state,
        &account.id,
        &format!("{} copy", loaded.name),
        Some(loaded.model.as_str()),
        &hire_args,
    )
    .await
}

/// `searchAgents {query}` — the roster, filtered by name. Honest and small.
pub async fn search_agents(state: &GatewayState, args: &Value) -> (u16, Value) {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let rows = live::roster_rows(state).await.unwrap_or_default();
    let hits: Vec<Value> = rows
        .into_iter()
        .filter(|row| {
            row["name"]
                .as_str()
                .map(|name| name.to_lowercase().contains(&query))
                .unwrap_or(false)
        })
        .collect();
    (200, Value::Array(hits))
}

/// `setAgentAvatarBytes {id, pngBase64|null}` and `getAgentAvatar {id}` share the profile row.
pub async fn set_avatar(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return (400, json!({ "error": "id is required" }));
    };
    let coworker = CoworkerId::from_stored(id.to_string());
    let mut profile = state
        .agui
        .auth
        .store
        .seamb_profile(&coworker)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    let Some(map) = profile.as_object_mut() else {
        return (500, json!({ "error": "profile unavailable" }));
    };
    match args.get("pngBase64") {
        Some(Value::String(png)) => {
            map.insert(
                "avatarDataUrl".to_string(),
                json!(format!("data:image/png;base64,{png}")),
            );
            map.insert("avatarVersion".to_string(), json!(now_ms().to_string()));
        }
        _ => {
            map.remove("avatarDataUrl");
            map.remove("avatarVersion");
        }
    }
    let _ = state
        .agui
        .auth
        .store
        .put_seamb_profile(&coworker, &profile, now_ms())
        .await;
    live::emit_roster(state).await;
    let (code, reply) = agent_reply(state, id).await;
    if code != 200 {
        return (200, Value::Null);
    }
    (200, reply["agent"].clone())
}

pub async fn get_avatar(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return (400, json!({ "error": "id is required" }));
    };
    let profile = state
        .agui
        .auth
        .store
        .seamb_profile(&CoworkerId::from_stored(id.to_string()))
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    (
        200,
        json!({
            "dataUrl": profile.get("avatarDataUrl").cloned().unwrap_or(Value::Null),
            "version": profile.get("avatarVersion").cloned().unwrap_or(Value::Null),
        }),
    )
}

/// Entry mutation: reactions, widget answers, deletion — all edits to a stored entry plus the
/// SSE frame that tells every open window.
pub async fn mutate_entry(
    state: &GatewayState,
    args: &Value,
    edit: impl FnOnce(&mut Value),
) -> (u16, Value) {
    let Some(agent) = args.get("agentId").and_then(Value::as_str) else {
        return (400, json!({ "error": "agentId is required" }));
    };
    let Some(entry_id) = args.get("entryId").and_then(Value::as_str) else {
        return (400, json!({ "error": "entryId is required" }));
    };
    let coworker = CoworkerId::from_stored(agent.to_string());
    let Ok(Some((seq, mut entry))) = state
        .agui
        .auth
        .store
        .find_gateway_entry(&coworker, entry_id)
        .await
    else {
        return (200, Value::Null);
    };
    edit(&mut entry);
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker, seq, &entry)
        .await
    {
        tracing::error!(%error, "could not mutate an entry");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    live::emit_transcript(state, agent, "updated", entry.clone());
    (200, entry)
}

/// `deleteTranscriptEntries {agentId, ids}` → emits `removed` per entry that went.
pub async fn delete_entries(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(agent) = args.get("agentId").and_then(Value::as_str) else {
        return (400, json!({ "error": "agentId is required" }));
    };
    let ids: Vec<String> = args
        .get("ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let coworker = CoworkerId::from_stored(agent.to_string());
    match state
        .agui
        .auth
        .store
        .delete_gateway_entries(&coworker, &ids)
        .await
    {
        Ok(removed) => {
            for id in &removed {
                let payload = json!({
                    "type": "removed",
                    "id": id,
                    "agentId": agent,
                    "ordered": live::ordered(state, &format!("transcript:{agent}")),
                });
                live::emit(state, "transcript", payload);
            }
            (200, json!({ "deleted": removed.len() }))
        }
        Err(error) => {
            tracing::error!(%error, "could not delete entries");
            (500, json!({ "error": "transcript unavailable" }))
        }
    }
}

// ---- Automations: slice 6's schedules wearing the client's names ----

fn automation_json(view: &opengrok_core::schedule::ScheduleView) -> Value {
    json!({
        "id": view.id,
        "agentId": view.coworker_id.as_str(),
        "cron": view.cron,
        "instruction": view.prompt,
        "enabled": view.active,
        "nextDueMs": view.next_due_ms,
    })
}

async fn automations_array(state: &GatewayState, agent: Option<&str>) -> Vec<Value> {
    let Some(account) = account(state, &state.email).await else {
        return Vec::new();
    };
    let schedules = state
        .agui
        .auth
        .store
        .schedules_for(&account.id)
        .await
        .unwrap_or_default();
    schedules
        .iter()
        .filter(|view| agent.is_none_or(|agent| view.coworker_id.as_str() == agent))
        .map(automation_json)
        .collect()
}

pub async fn get_automations(state: &GatewayState, args: &Value) -> (u16, Value) {
    let agent = args
        .get("id")
        .or_else(|| args.get("agentId"))
        .and_then(Value::as_str);
    (200, Value::Array(automations_array(state, agent).await))
}

/// The mutating automation commands answer the NEW FULL ARRAY, per the contract note.
pub async fn create_automation(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(agent) = args.get("agentId").and_then(Value::as_str) else {
        return (400, json!({ "error": "agentId is required" }));
    };
    let Some(cron) = args.get("cron").and_then(Value::as_str) else {
        return (400, json!({ "error": "cron is required" }));
    };
    let prompt = args
        .get("instruction")
        .or_else(|| args.get("prompt"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(account) = account(state, &state.email).await else {
        return (
            500,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    let at_ms = now_ms();
    let events = match Schedule::default().decide(ScheduleCommand::Create {
        coworker_id: CoworkerId::from_stored(agent.to_string()),
        cron: cron.to_string(),
        prompt: prompt.to_string(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => return (400, json!({ "error": reason.to_string() })),
    };
    let after = Schedule::replay(&events);
    let id = ScheduleId::new();
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_schedule(&id, &account.id, 0, &events, &after, at_ms)
        .await
    {
        tracing::error!(%error, "could not create an automation");
        return (500, json!({ "error": "storage failed" }));
    }
    (
        200,
        Value::Array(automations_array(state, Some(agent)).await),
    )
}

/// setEnabled / delete: pause, resume, delete on the schedule aggregate; the reply is the array.
pub async fn change_automation(state: &GatewayState, args: &Value, action: &str) -> (u16, Value) {
    let Some(id) = args
        .get("id")
        .or_else(|| args.get("automationId"))
        .and_then(Value::as_str)
    else {
        return (400, json!({ "error": "id is required" }));
    };
    let Some(account) = account(state, &state.email).await else {
        return (200, json!([]));
    };
    let schedule_id = ScheduleId::from_stored(id.to_string());
    // Ownership by the same rule as /schedules: not yours reads as not there.
    match state.agui.auth.store.schedule_owner(&schedule_id).await {
        Ok(Some(owner)) if owner == account.id => {}
        _ => return (200, Value::Array(automations_array(state, None).await)),
    }
    let Ok((loaded, seq)) = state.agui.auth.store.load_schedule(&schedule_id).await else {
        return (200, Value::Array(automations_array(state, None).await));
    };
    let at_ms = now_ms();
    let command = match action {
        "enable" => ScheduleCommand::Resume { at_ms },
        "disable" => ScheduleCommand::Pause { at_ms },
        _ => ScheduleCommand::Delete { at_ms },
    };
    if let Ok(events) = loaded.decide(command) {
        let mut after = loaded;
        for event in &events {
            after.apply(event);
        }
        let _ = state
            .agui
            .auth
            .store
            .append_schedule(&schedule_id, &account.id, seq, &events, &after, at_ms)
            .await;
    }
    (200, Value::Array(automations_array(state, None).await))
}

/// `runAgentAutomationNow` — the sweep's firing path, on demand.
pub async fn run_automation_now(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(id) = args
        .get("id")
        .or_else(|| args.get("automationId"))
        .and_then(Value::as_str)
    else {
        return (400, json!({ "error": "id is required" }));
    };
    let Some(account) = account(state, &state.email).await else {
        return (200, Value::Null);
    };
    let schedule_id = ScheduleId::from_stored(id.to_string());
    let Ok((loaded, seq)) = state.agui.auth.store.load_schedule(&schedule_id).await else {
        return (200, Value::Null);
    };
    let Some(coworker_id) = loaded.coworker_id.clone() else {
        return (200, Value::Null);
    };
    let run_id = RunId::new();
    if let Ok(events) = loaded.decide(ScheduleCommand::Fire {
        run_id: run_id.clone(),
        at_ms: now_ms(),
    }) {
        let _ = state
            .agui
            .auth
            .store
            .append_schedule(&schedule_id, &account.id, seq, &events, &loaded, now_ms())
            .await;
        tokio::spawn(crate::autonomy::fire(
            state.agui.clone(),
            format!("automation {schedule_id} (run now)"),
            account.id,
            coworker_id,
            loaded.prompt.clone(),
            schedule_id.as_str().to_string(),
            run_id,
        ));
    }
    (200, Value::Null)
}
