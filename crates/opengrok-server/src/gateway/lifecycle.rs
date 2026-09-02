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
    let template = match args
        .get("templateId")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some(template_id) if !template_id.is_empty() => {
            match crate::templates::for_account(&state.agui, account_id, template_id).await {
                Ok(Some(template)) => Some(template),
                Ok(None) => return (404, json!({ "error": "no such template" })),
                Err(error) => return (503, json!({ "error": error })),
            }
        }
        _ => None,
    };
    // The caller's pin when there is one, the template's when it has one, the deployment's
    // default otherwise. A blank string is "not one" — it would otherwise be stored and asked of
    // the gateway verbatim.
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .or_else(|| template.as_ref().and_then(|t| t.model.clone()))
        .unwrap_or_else(|| state.agui.model.clone());
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
    // A key of its own, so a cap can be written on it (never fails the hire).
    let _key = crate::spend::ensure_key_for(&state.agui, account_id, &id, &coworker.name).await;

    let view = opengrok_core::coworker::CoworkerView {
        id: id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: false,
        members: Vec::new(),
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
    // grants at hire. Without this a hired coworker silently refuses every tool. A `templateId`
    // the desktop passes through (`createAgent {…, templateId?}`) that names one of the org's
    // templates grants that template's ceiling and approval set instead, and copies its limits.
    let tools =
        opengrok_policy::ToolSet::only(opengrok_tools::Executor::builtin_tool_names().to_vec());
    let granted = match template.as_ref() {
        Some(template) => {
            crate::templates::apply_at_hire(&state.agui, account_id, &id, template, at_ms)
                .await
                .map_err(opengrok_store::StoreError::Corrupt)
        }
        None => {
            state
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
        }
    };
    if let Err(error) = granted {
        tracing::error!(%error, "createAgent could not grant access");
        return (
            500,
            json!({ "error": "the coworker was created but could not be granted" }),
        );
    }

    let profile = json!({
        "description": args
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
            .map(str::to_string)
            .or_else(|| template.as_ref().map(|t| t.description.clone()))
            .unwrap_or_default(),
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
            members: after.members.clone(),
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
    //
    // A REFUSED repin is answered, not swallowed. Folding `decide` into the `if let` chain made a
    // rejection indistinguishable from "no model sent": the block simply did not match and the
    // caller got a 200 describing an agent whose pin had not changed. Asking to think with nothing
    // is a mistake worth being told about (CLAUDE.md #8).
    if let Some(model) = profile_args.get("model").and_then(Value::as_str)
        && let Ok((loaded, seq)) = state.agui.auth.store.load_coworker(&coworker_id).await
        && model.trim() != loaded.model
    {
        let events = match loaded.decide(CoworkerCommand::Repin {
            model: model.to_string(),
            at_ms: now_ms(),
        }) {
            Ok(events) => events,
            Err(reason) => return (400, json!({ "error": reason.to_string() })),
        };
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
            members: after.members.clone(),
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
                members: after.members.clone(),
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
            // Its key goes with it: a retired coworker must not keep a live credential.
            crate::spend::revoke_for(&state.agui, &coworker_id).await;
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
                live::emit_transcript_removed(state, agent, id);
            }
            (200, json!({ "deleted": removed.len() }))
        }
        Err(error) => {
            tracing::error!(%error, "could not delete entries");
            (500, json!({ "error": "transcript unavailable" }))
        }
    }
}

// ---- Automations: slice 6's schedules wearing the desktop's Routines pane ----
//
// THE PANE'S SHAPES, TRANSCRIBED. The renderer's routines controller
// (`frontend/src/recovered/features/automations/routines/controller.ts:43-54`) sends
// `{id, spec: {name, prompt, trigger, isEnabled}}` to create, `{id, automationId, spec}` to
// update, `{id, automationId, isEnabled}` to enable, `{id, automationId}` to delete and run; every
// mutating verb answers the FULL array for the agent. `parseAutomation` (`:92-106`) rejects the
// whole reply unless each record has string `id`, `name`, `prompt`, `triggerDescription`, an object
// `trigger`, boolean `isEnabled` and an array `runs` whose items parse. The host's record
// (`source/host/automations/automation.ts:84-89`) adds `schedule`, `nextRunAt`, `createdAt`,
// `lastRunAt`, `raisedNotices`, `filePath`, and runs as `{id, trigger, startedAt, finishedAt,
// status, detail?}` with status `running | ok | error` and trigger `schedule | manual`.
//
// The pre-pane body (`{agentId, cron, instruction}`) and its keys (`cron`, `instruction`,
// `enabled`, `nextDueMs`) are still accepted and still answered, as a superset — the smokes speak
// it and the pane ignores keys it does not know.
//
// ONLY SCHEDULES. The pane's trigger picker also offers slack, git, teams, linear, sentry and
// pagerduty; the server has no such wake sources, so a non-cron trigger is refused with a 400 the
// pane shows, rather than accepted and silently never firing.

/// The desktop's Routines pane refreshes from an `agents-automation` frame carrying
/// `{agentId, automations}` (`gateway-event-families.ts:10` → the controller's `ingest`), so a
/// mutation, a firing, or a finished run reaches an open pane without a poll.
pub(crate) async fn emit_automations(state: &GatewayState, agent: &str) {
    let automations = automations_array(state, Some(agent)).await;
    // UNSTAMPED: the renderer's automations family has no replica and its own emitter sends no
    // stamp; a roster sequence spent here was a roster gap on every routine change.
    live::emit_unstamped(
        state,
        "agents-automation",
        json!({ "agentId": agent, "automations": automations }),
    );
}

/// A sentence for the pane's "when" column. Covers the shapes the picker writes; anything else
/// shows the expression itself, which is honest and still readable.
fn describe_cron(display: &str) -> String {
    let fields: Vec<&str> = display.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = fields.as_slice() else {
        return format!("On schedule {display}");
    };
    let time = |m: &str, h: &str| -> Option<String> {
        let m: u32 = m.parse().ok()?;
        let h: u32 = h.parse().ok()?;
        Some(format!("{h:02}:{m:02}"))
    };
    let weekday_name = |d: &str| -> Option<&'static str> {
        Some(match d {
            "0" | "7" | "sun" | "SUN" => "Sunday",
            "1" | "mon" | "MON" => "Monday",
            "2" | "tue" | "TUE" => "Tuesday",
            "3" | "wed" | "WED" => "Wednesday",
            "4" | "thu" | "THU" => "Thursday",
            "5" | "fri" | "FRI" => "Friday",
            "6" | "sat" | "SAT" => "Saturday",
            _ => return None,
        })
    };
    if let Some(every) = minute.strip_prefix("*/")
        && (*hour, *day, *month, *weekday) == ("*", "*", "*", "*")
    {
        return format!("Every {every} minutes");
    }
    if *minute == "*" && (*hour, *day, *month, *weekday) == ("*", "*", "*", "*") {
        return "Every minute".to_string();
    }
    if let Some(every) = hour.strip_prefix("*/")
        && (*day, *month, *weekday) == ("*", "*", "*")
        && minute.parse::<u32>().is_ok()
    {
        return format!("Every {every} hours");
    }
    if let Some(at) = time(minute, hour)
        && (*day, *month) == ("*", "*")
    {
        return match *weekday {
            "*" => format!("Every day at {at}"),
            "1-5" => format!("Weekdays at {at}"),
            "0,6" | "6,0" => format!("Weekends at {at}"),
            single => match weekday_name(single) {
                Some(name) => format!("Every {name} at {at}"),
                None => format!("At {at} on days {single}"),
            },
        };
    }
    if let Some(at) = time(minute, hour)
        && *month == "*"
        && *weekday == "*"
        && day.parse::<u32>().is_ok()
    {
        return format!("Monthly on day {day} at {at}");
    }
    format!("On schedule {display}")
}

/// The pane's record for one schedule, with its run history.
async fn automation_json(
    state: &GatewayState,
    view: &opengrok_core::schedule::ScheduleView,
) -> Value {
    let schedule_id = ScheduleId::from_stored(view.id.clone());
    // Which runs a person started: the aggregate knows; the projection does not.
    let manual = state
        .agui
        .auth
        .store
        .load_schedule(&schedule_id)
        .await
        .map(|(schedule, _)| schedule.manual_runs)
        .unwrap_or_default();
    let runs: Vec<Value> = state
        .agui
        .auth
        .store
        .runs_for_thread(&view.id, 20)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|run| {
            let (status, finished_at) = match run.status.as_str() {
                "finished" => ("ok", Some(run.updated_at_ms)),
                "failed" => ("error", Some(run.updated_at_ms)),
                // Running, or paused on a card — the pane has no word for "waiting", and
                // "running" is the one that keeps it from reading as done.
                _ => ("running", None),
            };
            let mut entry = json!({
                "id": run.id.as_str(),
                "trigger": if manual.contains(run.id.as_str()) { "manual" } else { "schedule" },
                "startedAt": run.started_at_ms,
                "finishedAt": finished_at,
                "status": status,
            });
            if status == "error" {
                entry["detail"] = json!("The run failed. Its run log has the reason.");
            }
            entry
        })
        .collect();
    let schedule = opengrok_core::schedule::display_cron(&view.cron);
    json!({
        "id": view.id,
        "name": view.name,
        "prompt": view.prompt,
        "trigger": { "type": "cron", "schedule": schedule },
        "isEnabled": view.active,
        "createdAt": view.created_at_ms,
        "lastRunAt": view.last_fired_ms,
        "raisedNotices": [],
        "schedule": schedule,
        "triggerDescription": describe_cron(&schedule),
        "nextRunAt": view.next_due_ms,
        "runs": runs,
        "filePath": "",
        // The pre-pane keys, kept for the smokes and anything else that learned them.
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
    let mut out = Vec::with_capacity(schedules.len());
    for view in schedules
        .iter()
        .filter(|view| agent.is_none_or(|agent| view.coworker_id.as_str() == agent))
    {
        out.push(automation_json(state, view).await);
    }
    out
}

pub async fn get_automations(state: &GatewayState, args: &Value) -> (u16, Value) {
    let agent = args
        .get("id")
        .or_else(|| args.get("agentId"))
        .and_then(Value::as_str);
    (200, Value::Array(automations_array(state, agent).await))
}

/// What both body shapes boil down to. `None` is a refusal already shaped for the wire.
struct RoutineSpec {
    name: String,
    cron: String,
    prompt: String,
    enabled: bool,
}

fn parse_spec(args: &Value) -> Result<RoutineSpec, (u16, Value)> {
    if let Some(spec) = args.get("spec") {
        // The pane's shape.
        let trigger = spec.get("trigger").unwrap_or(&Value::Null);
        let kind = trigger.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "cron" {
            return Err((
                400,
                json!({ "error": "only schedules are supported on this server" }),
            ));
        }
        let Some(cron) = trigger.get("schedule").and_then(Value::as_str) else {
            return Err((400, json!({ "error": "trigger.schedule is required" })));
        };
        return Ok(RoutineSpec {
            name: spec
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Routine")
                .to_string(),
            cron: cron.to_string(),
            prompt: spec
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            enabled: spec
                .get("isEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }
    // The pre-pane shape.
    let Some(cron) = args.get("cron").and_then(Value::as_str) else {
        return Err((400, json!({ "error": "cron is required" })));
    };
    Ok(RoutineSpec {
        name: args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Routine")
            .to_string(),
        cron: cron.to_string(),
        prompt: args
            .get("instruction")
            .or_else(|| args.get("prompt"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        enabled: args.get("enabled").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// The agent a routine call is about: the pane's `id`, or the pre-pane `agentId`.
fn agent_arg(args: &Value) -> Option<&str> {
    args.get("id")
        .or_else(|| args.get("agentId"))
        .and_then(Value::as_str)
}

/// The routine a mutation names: the pane's `automationId`, or the pre-pane `id` (which the pane
/// uses for the AGENT — so `automationId` is looked at first).
fn automation_arg(args: &Value) -> Option<&str> {
    args.get("automationId")
        .or_else(|| args.get("id"))
        .and_then(Value::as_str)
}

/// The mutating automation commands answer the NEW FULL ARRAY, per the contract note.
pub async fn create_automation(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(agent) = agent_arg(args) else {
        return (400, json!({ "error": "id (the agent) is required" }));
    };
    let spec = match parse_spec(args) {
        Ok(spec) => spec,
        Err(refusal) => return refusal,
    };
    let Some(account) = account(state, &state.email).await else {
        return (
            500,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    let at_ms = now_ms();
    let mut events = match Schedule::default().decide(ScheduleCommand::Create {
        coworker_id: CoworkerId::from_stored(agent.to_string()),
        cron: spec.cron,
        prompt: spec.prompt,
        name: spec.name,
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => return (400, json!({ "error": reason.to_string() })),
    };
    let mut after = Schedule::replay(&events);
    if !spec.enabled
        && let Ok(paused) = after.decide(ScheduleCommand::Pause { at_ms })
    {
        for event in &paused {
            after.apply(event);
        }
        events.extend(paused);
    }
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
    emit_automations(state, agent).await;
    (
        200,
        Value::Array(automations_array(state, Some(agent)).await),
    )
}

/// Load the schedule, decide with `decide`, append at the loaded seq — and if another writer got
/// there first, re-read and try ONCE more before answering 409. Why: the desktop's Routines pane
/// autosaves an edit on blur at the same instant a person clicks "Test run", so two mutations on
/// one schedule a few milliseconds apart are the ordinary case, not a race to design away. The
/// loser used to answer 500 "storage failed" (seen live 2 Sep 2026); now it decides again against
/// the winner's state, which is what the person meant anyway.
///
/// `decide` sees the fresh aggregate and returns the events to append, or a refusal already
/// shaped for the wire. Returns the aggregate after the append and the seq it landed at.
async fn mutate_schedule<F>(
    state: &GatewayState,
    account: &opengrok_core::account::AccountView,
    schedule_id: &ScheduleId,
    at_ms: i64,
    mut decide: F,
) -> Result<Schedule, (u16, Value)>
where
    F: FnMut(&Schedule) -> Result<Vec<opengrok_core::schedule::ScheduleEvent>, (u16, Value)>,
{
    for attempt in 0..2 {
        let Ok((loaded, seq)) = state.agui.auth.store.load_schedule(schedule_id).await else {
            return Err((404, json!({ "error": "no such routine" })));
        };
        let events = decide(&loaded)?;
        let mut after = loaded;
        for event in &events {
            after.apply(event);
        }
        match state
            .agui
            .auth
            .store
            .append_schedule(schedule_id, &account.id, seq, &events, &after, at_ms)
            .await
        {
            Ok(_) => return Ok(after),
            Err(opengrok_store::StoreError::Conflict) if attempt == 0 => {
                tracing::info!(schedule = %schedule_id, "a routine write lost a race; re-reading and retrying once");
                continue;
            }
            Err(opengrok_store::StoreError::Conflict) => {
                return Err((
                    409,
                    json!({ "error": "another change to this routine landed first; reload and retry" }),
                ));
            }
            Err(error) => {
                tracing::error!(%error, schedule = %schedule_id, "could not write a routine change");
                return Err((500, json!({ "error": "storage failed" })));
            }
        }
    }
    Err((
        409,
        json!({ "error": "another change to this routine landed first; reload and retry" }),
    ))
}

/// The routine a mutation names, owned by this account — or the refusal. Ownership by the same
/// rule as /schedules: not yours reads as not there.
async fn owned_schedule(
    state: &GatewayState,
    args: &Value,
) -> Result<(opengrok_core::account::AccountView, ScheduleId), (u16, Value)> {
    let Some(id) = automation_arg(args) else {
        return Err((400, json!({ "error": "automationId is required" })));
    };
    let Some(account) = account(state, &state.email).await else {
        return Err((200, json!([])));
    };
    let schedule_id = ScheduleId::from_stored(id.to_string());
    match state.agui.auth.store.schedule_owner(&schedule_id).await {
        Ok(Some(owner)) if owner == account.id => Ok((account, schedule_id)),
        _ => Err((404, json!({ "error": "no such routine" }))),
    }
}

/// `updateAgentAutomation {id, automationId, spec}` — edit in place: name, schedule, prompt and
/// enabled state on the SAME row, never a second one.
pub async fn update_automation(state: &GatewayState, args: &Value) -> (u16, Value) {
    let spec = match parse_spec(args) {
        Ok(spec) => spec,
        Err(refusal) => return refusal,
    };
    let (account, schedule_id) = match owned_schedule(state, args).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let at_ms = now_ms();
    let after = match mutate_schedule(state, &account, &schedule_id, at_ms, |loaded| {
        let mut events = loaded
            .decide(ScheduleCommand::Update {
                name: spec.name.clone(),
                cron: spec.cron.clone(),
                prompt: spec.prompt.clone(),
                at_ms,
            })
            .map_err(|reason| (400, json!({ "error": reason.to_string() })))?;
        let mut after = loaded.clone();
        for event in &events {
            after.apply(event);
        }
        // The editor's toggle rides along: flip only when it differs, so an unchanged edit does
        // not log a pause that never happened.
        let flip = match (spec.enabled, after.paused) {
            (false, false) => Some(ScheduleCommand::Pause { at_ms }),
            (true, true) => Some(ScheduleCommand::Resume { at_ms }),
            _ => None,
        };
        if let Some(command) = flip
            && let Ok(more) = after.decide(command)
        {
            events.extend(more);
        }
        Ok(events)
    })
    .await
    {
        Ok(after) => after,
        Err(refusal) => return refusal,
    };
    let agent = after
        .coworker_id
        .as_ref()
        .map(|c| c.as_str().to_string())
        .unwrap_or_default();
    emit_automations(state, &agent).await;
    (
        200,
        Value::Array(automations_array(state, Some(&agent)).await),
    )
}

/// setEnabled / delete: pause, resume, delete on the schedule aggregate; the reply is the array.
pub async fn change_automation(state: &GatewayState, args: &Value, action: &str) -> (u16, Value) {
    let (account, schedule_id) = match owned_schedule(state, args).await {
        Ok(found) => found,
        // Not yours (or nothing named) reads as the unchanged array, as before.
        Err((404, _)) => return (200, Value::Array(automations_array(state, None).await)),
        Err(refusal) => return refusal,
    };
    let at_ms = now_ms();
    let result = mutate_schedule(state, &account, &schedule_id, at_ms, |loaded| {
        let command = match action {
            "enable" => ScheduleCommand::Resume { at_ms },
            "disable" => ScheduleCommand::Pause { at_ms },
            _ => ScheduleCommand::Delete { at_ms },
        };
        // Already in the asked-for state (paused twice, enabled twice): nothing to write, and
        // not an error the pane should see.
        Ok(loaded.decide(command).unwrap_or_default())
    })
    .await;
    let agent = match result {
        Ok(after) => after.coworker_id.as_ref().map(|c| c.as_str().to_string()),
        Err((code @ (409 | 500), body)) => return (code, body),
        Err(_) => None,
    };
    if let Some(agent) = &agent {
        emit_automations(state, agent).await;
    }
    (
        200,
        Value::Array(automations_array(state, agent.as_deref()).await),
    )
}

/// `runAgentAutomationNow` — the sweep's firing path, on demand. The run is a `manual` one in the
/// pane's history, and it posts into the coworker's chat when it finishes, like a clock firing.
pub async fn run_automation_now(state: &GatewayState, args: &Value) -> (u16, Value) {
    let (account, schedule_id) = match owned_schedule(state, args).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let run_id = RunId::new();
    let at_ms = now_ms();
    let after = match mutate_schedule(state, &account, &schedule_id, at_ms, |loaded| {
        loaded
            .decide(ScheduleCommand::Fire {
                run_id: run_id.clone(),
                manual: true,
                at_ms,
            })
            .map_err(|reason| (409, json!({ "error": reason.to_string() })))
    })
    .await
    {
        Ok(after) => after,
        Err(refusal) => return refusal,
    };
    let Some(coworker_id) = after.coworker_id.clone() else {
        return (200, Value::Null);
    };
    tokio::spawn(crate::autonomy::fire(
        state.agui.clone(),
        crate::autonomy::Firing {
            origin: format!("automation {schedule_id} (run now)"),
            account_id: account.id,
            coworker_id: coworker_id.clone(),
            prompt: after.prompt.clone(),
            thread_id: schedule_id.as_str().to_string(),
            run_id,
            announce: Some(crate::autonomy::Announce {
                gateway: state.clone(),
                name: after.name.clone(),
            }),
        },
    ));
    emit_automations(state, coworker_id.as_str()).await;
    (200, Value::Null)
}

// ---- Groups (`plan-rooms.md` §2): a coworker with members ----

/// The member list a group may hold, from what the client sent: de-duplicated, the group itself
/// dropped, only the account's own living coworkers kept, no group inside a group (the client's
/// own `assertMembersAreNotGroups`), capped at the client's `GROUP_MAX_MEMBERS`.
async fn group_members(
    state: &GatewayState,
    account_id: &opengrok_core::id::AccountId,
    requested: &[String],
    exclude: Option<&str>,
) -> Result<Vec<CoworkerId>, (u16, Value)> {
    let Ok(roster) = state.agui.auth.store.coworkers_for(account_id).await else {
        return Err((500, json!({ "error": "roster unavailable" })));
    };
    let mut nested = Vec::new();
    let mut members: Vec<CoworkerId> = Vec::new();
    for id in requested {
        if exclude == Some(id.as_str()) || members.iter().any(|m| m.as_str() == id) {
            continue;
        }
        let Some(view) = roster.iter().find(|view| view.id.as_str() == id) else {
            // Somebody else's, retired, or made up: not a member, not an error — the client's
            // own filter (`existing.has(id)`), transcribed.
            continue;
        };
        if !view.members.is_empty() {
            nested.push(id.clone());
            continue;
        }
        members.push(view.id.clone());
    }
    if !nested.is_empty() {
        return Err((
            400,
            json!({
                "error": format!(
                    "A group chat can only contain individual agents, not other group chats. \
                     Remove the group chat{} from the member list.",
                    if nested.len() == 1 { "" } else { "s" }
                ),
                "nestedGroupIds": nested,
            }),
        ));
    }
    members.truncate(opengrok_core::coworker::GROUP_MAX_MEMBERS);
    Ok(members)
}

/// `createGroup {name, description, memberAgentIds}` → `{agent, transcript}`, the createAgent
/// shape. A second create with the same member set answers the EXISTING group (the client's
/// own rule: `isSameMemberSet`). A group has no computer, no key and no model of its own.
pub async fn create_group(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    use opengrok_core::coworker::{Coworker, CoworkerCommand};
    let Some(account) = account(state, caller).await else {
        return (
            500,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("Group")
        .to_string();
    let requested: Vec<String> = args
        .get("memberAgentIds")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let members = match group_members(state, &account.id, &requested, None).await {
        Ok(members) => members,
        Err(refusal) => return refusal,
    };
    if members.is_empty() {
        return (
            400,
            json!({ "error": "A group needs at least one existing member agent." }),
        );
    }
    // The same members already make a group: that group, not a twin.
    if let Ok(roster) = state.agui.auth.store.coworkers_for(&account.id).await
        && let Some(existing) = roster.iter().find(|view| {
            view.members.len() == members.len() && members.iter().all(|m| view.members.contains(m))
        })
    {
        return agent_reply(state, existing.id.as_str()).await;
    }
    let id = CoworkerId::new();
    let at_ms = now_ms();
    let events = match Coworker::default().decide(CoworkerCommand::HireGroup {
        name,
        members,
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => return (400, json!({ "error": reason.to_string() })),
    };
    let group = Coworker::replay(&events);
    let view = opengrok_core::coworker::CoworkerView {
        id: id.clone(),
        name: group.name.clone(),
        model: group.model.clone(),
        box_id: None,
        retired: false,
        updated_at_ms: at_ms,
        members: group.members.clone(),
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_coworker(&id, &account.id, 0, &events, &view)
        .await
    {
        tracing::error!(%error, "createGroup could not hire the group");
        return (500, json!({ "error": "hire failed" }));
    }
    let profile = json!({
        "description": args.get("description").and_then(Value::as_str).unwrap_or(""),
        "title": "",
        "avatarShape": "",
        "avatarColor": "",
    });
    let _ = state
        .agui
        .auth
        .store
        .put_seamb_profile(&id, &profile, now_ms())
        .await;
    live::emit_roster(state).await;
    agent_reply(state, id.as_str()).await
}

/// `setGroupMembers {id, memberAgentIds}` → the group's summary, or `null` when `id` is not one
/// of the caller's groups. An empty cleaned list changes nothing (the client's rule) and still
/// answers the summary.
pub async fn set_group_members(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    use opengrok_core::coworker::CoworkerCommand;
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return (400, json!({ "error": "id is required" }));
    };
    let Some(account) = account(state, caller).await else {
        return (200, Value::Null);
    };
    let coworker_id = CoworkerId::from_stored(id.to_string());
    let Ok(roster) = state.agui.auth.store.coworkers_for(&account.id).await else {
        return (500, json!({ "error": "roster unavailable" }));
    };
    if !roster
        .iter()
        .any(|view| view.id == coworker_id && !view.members.is_empty())
    {
        return (200, Value::Null);
    }
    let requested: Vec<String> = args
        .get("memberAgentIds")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let members = match group_members(state, &account.id, &requested, Some(id)).await {
        Ok(members) => members,
        Err(refusal) => return refusal,
    };
    if !members.is_empty() {
        let Ok((group, seq)) = state.agui.auth.store.load_coworker(&coworker_id).await else {
            return (200, Value::Null);
        };
        let at_ms = now_ms();
        let events = match group.decide(CoworkerCommand::SetMembers { members, at_ms }) {
            Ok(events) => events,
            Err(reason) => return (400, json!({ "error": reason.to_string() })),
        };
        let mut after = group;
        for event in &events {
            after.apply(event);
        }
        let view = opengrok_core::coworker::CoworkerView {
            id: coworker_id.clone(),
            name: after.name.clone(),
            model: after.model.clone(),
            box_id: None,
            retired: false,
            updated_at_ms: at_ms,
            members: after.members.clone(),
        };
        if let Err(error) = state
            .agui
            .auth
            .store
            .append_coworker(&coworker_id, &account.id, seq, &events, &view)
            .await
        {
            tracing::error!(%error, "setGroupMembers could not save");
            return (500, json!({ "error": "save failed" }));
        }
        live::emit_roster(state).await;
    }
    let Ok(rows) = live::roster_rows(state).await else {
        return (500, json!({ "error": "roster unavailable" }));
    };
    match rows.into_iter().find(|row| row["id"] == id) {
        Some(summary) => (200, summary),
        None => (200, Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::describe_cron;

    #[test]
    fn the_when_column_reads_as_a_sentence() {
        assert_eq!(describe_cron("0 9 * * 1"), "Every Monday at 09:00");
        assert_eq!(describe_cron("30 17 * * 1-5"), "Weekdays at 17:30");
        assert_eq!(describe_cron("0 8 * * *"), "Every day at 08:00");
        assert_eq!(describe_cron("*/15 * * * *"), "Every 15 minutes");
        assert_eq!(describe_cron("0 */2 * * *"), "Every 2 hours");
        assert_eq!(describe_cron("0 9 1 * *"), "Monthly on day 1 at 09:00");
        assert_eq!(describe_cron("*/2 * * * * *"), "On schedule */2 * * * * *");
    }
}
