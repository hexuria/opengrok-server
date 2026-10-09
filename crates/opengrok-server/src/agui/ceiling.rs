//! `GET`/`PUT /coworkers/{id}/ceiling` (#268): a coworker's tool ceiling as the rows its owner
//! switches — every built-in, the person's machine, every plugin — in the shape NativeChat agreed
//! on #268: `{"tools": [{name, kind, description, enabled, available?, label?, connector?}],
//! "version"}` for both verbs. `available` is sent only where it can be false.
//!
//! A PUT IS THE CEILING A TURN READS, written into the `ceiling_view` that `decide` reads every
//! turn, with the owner's profile equal to it as hire and every template write it. The layers
//! intersect: a plugin switched on in the ceiling alone would still be refused by the profile, and
//! the row would say on while every turn said no. A PUT naming the `version` it read is refused
//! with a 409 if the ceiling has changed since, so two screens cannot quietly undo each other.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_policy::ToolSet;
use opengrok_tools::{Executor, USER_MACHINE_SHELL, computer_desk, plugin_desk, routine};
use serde_json::{Value, json};

use super::routes::{AgUiState, account_from_bearer, now_ms, owned_coworker};
use crate::health::refusal;

/// The built-ins' rows: one per built-in, ONE for the routine tools, which it switches together
/// (#316, the owner's call): a person decides whether a Bot keeps routines, not which verb of it;
/// and one for the plugin tools, the same way (#359).
fn builtin_rows() -> impl Iterator<Item = &'static str> {
    let rows = Executor::every_builtin().filter(|name| {
        !routine::is_routine_tool(name)
            && !plugin_desk::is_plugin_desk_tool(name)
            && !computer_desk::is_computer_tool(name)
    });
    rows.chain([routine::ROW, plugin_desk::ROW, computer_desk::ROW])
}

/// The rows as `ceiling` sets them, plus one per plugin it still switches on that this server no
/// longer loads: kept, so it can be seen and switched off.
async fn rows(
    state: &AgUiState,
    owner: &AccountId,
    ceiling: &ToolSet,
) -> Result<Vec<Value>, opengrok_store::StoreError> {
    // By reference: the deployment's plugins are not copied for every ceiling read.
    let installed = opengrok_integrations::installed::list(&state.auth.store, owner).await?;
    let installed: Vec<_> = installed.iter().map(|i| i.bundle.plugin()).collect();
    let mut plugins: std::collections::BTreeMap<&str, _> = state
        .plugins
        .iter()
        .map(|(name, p)| (name.as_str(), p))
        .collect();
    for plugin in &installed {
        plugins
            .entry(plugin.manifest.name.as_str())
            .or_insert(plugin);
    }
    // Available exactly when a turn would bind one (`tools_for_coworker`).
    let machine = crate::local_exec::enabled_machine(&state.auth.store, owner.as_str()).await;
    let mut rows: Vec<Value> = builtin_rows()
        .map(|name| {
            let description = Executor::builtin_description(name).unwrap_or_default();
            // A group's switch is its own (7 Oct 2026): on unless the person switched the whole
            // group off, whatever its tools' own choices are.
            let on = match name {
                routine::ROW | plugin_desk::ROW | computer_desk::ROW => group_on(ceiling, name),
                name => ceiling.allows(name),
            };
            let mut row = json!({ "name": name, "kind": "builtin",
                "description": description, "enabled": on });
            if name == USER_MACHINE_SHELL {
                row["available"] = json!(machine.is_some());
            }
            // One sentence for people, beside the model's words in `description` (#359).
            if let Some((_, summary)) = Executor::builtin_for_people(name) {
                row["summary"] = json!(summary);
            }
            if name == routine::ROW {
                row["label"] = json!(routine::ROW_LABEL);
            }
            if name == plugin_desk::ROW {
                row["label"] = json!(plugin_desk::ROW_LABEL);
            }
            if name == computer_desk::ROW {
                row["label"] = json!(computer_desk::ROW_LABEL);
            }
            row
        })
        .collect();
    // A plugin named like a built-in cannot be told apart from it here, so the built-in keeps the
    // name: one name must never switch two things. Nor a plugin with a dot in its name, which is
    // never dialled (`connect_plugins`): a switch for it would say on while no turn offered it.
    let free = |name: &str| {
        !name.contains('.')
            && !Executor::every_builtin()
                .chain(builtin_rows())
                .any(|b| b == name)
    };
    for plugin in plugins.values().filter(|p| free(&p.manifest.name)) {
        let name = &plugin.manifest.name;
        // Agent Plugins 1.0.0 has no label; `name` is its human-readable name.
        // An account's install is on only where it is named (`opengrok_policy::names_plugin`).
        let on = match state.plugins.contains_key(name) {
            true => ceiling.allows_all_of(name),
            false => ceiling.whole_plugins().any(|named| named == name),
        };
        let mut row = json!({ "name": name, "kind": "plugin", "label": name, "enabled": on });
        if let Some(description) = &plugin.manifest.description {
            row["description"] = json!(description);
        }
        if let Some(connector) = plugin.connector() {
            row["connector"] = json!(connector);
        }
        rows.push(row);
    }
    let loaded = |name: &str| plugins.values().any(|p| p.manifest.name == name);
    for name in ceiling
        .whole_plugins()
        .filter(|name| free(name) && !loaded(name))
    {
        rows.push(json!({ "name": name, "kind": "plugin", "enabled": true, "available": false }));
    }
    Ok(rows)
}

/// The owner, or the refusal: another account's coworker reads as "no such coworker", never as a
/// refused one. An owner whose own grant is withdrawn is told so on both verbs, in words: that
/// coworker is not theirs to use, nor to widen again through its own ceiling.
pub(crate) async fn owner(
    state: &AgUiState,
    headers: &HeaderMap,
    id: String,
) -> Result<(AccountId, CoworkerId), Response> {
    let Some(account_id) = account_from_bearer(state, headers) else {
        return Err(refusal(401, "sign in first"));
    };
    let coworker_id = CoworkerId::from_stored(id);
    match owned_coworker(state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return Err(refusal(404, "no such coworker")),
        Err(_) => return Err(unavailable(&"the roster did not answer")),
    }
    let policy = state.auth.store.policy_for(&account_id, &coworker_id).await;
    let policy = policy.map_err(|error| unavailable(&error))?;
    let action = opengrok_policy::Action::UseCoworker;
    match opengrok_policy::decide(&account_id, &coworker_id, action, &policy).reason() {
        Some(why) => Err(refusal(403, why)),
        None => Ok((account_id, coworker_id)),
    }
}

fn unavailable(error: &dyn std::fmt::Display) -> Response {
    tracing::error!(%error, "a coworker's tool ceiling could not be read or saved");
    refusal(503, "this coworker's tools could not be read or saved now")
}

pub(super) async fn get_ceiling(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match state.auth.store.ceiling_at(&coworker_id).await {
        Ok((ceiling, version)) => reply(&state, &account_id, &ceiling, version).await,
        Err(error) => unavailable(&error),
    }
}

async fn reply(state: &AgUiState, owner: &AccountId, ceiling: &ToolSet, version: i64) -> Response {
    let tools = match rows(state, owner, ceiling).await {
        Ok(rows) => rows,
        Err(e) => return unavailable(&e),
    };
    Json(json!({ "tools": tools, "version": version })).into_response()
}

/// `enabled` is required: a body without it is a client's mistake, and reading that as "none"
/// would switch every tool off. `version` is the one a GET answered; without it the write is
/// unconditional, as it was before there was one.
#[derive(serde::Deserialize)]
pub(super) struct Enabled {
    enabled: Vec<String>,
    version: Option<i64>,
}

pub(super) async fn put_ceiling(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Enabled>, JsonRejection>,
) -> Response {
    let (account_id, coworker_id) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let Enabled { enabled, version } = match body {
        Ok(Json(body)) => body,
        Err(rejection) => {
            let why = rejection.body_text();
            return refusal(422, &format!("send {{\"enabled\": [names]}}: {why}"));
        }
    };
    match save(&state, &account_id, &coworker_id, enabled, version).await {
        Ok((tools, version)) => reply(&state, &account_id, &tools, version).await,
        Err(refused) => refused,
    }
}

/// Whether a tool group's own switch is on in `ceiling`: not switched off as a whole, and at
/// least one of its tools there by its own choice (a group left out entirely is off).
fn group_on(ceiling: &ToolSet, group: &str) -> bool {
    !ceiling.group_off(group)
        && opengrok_policy::TOOL_GROUPS
            .iter()
            .find(|(g, _)| *g == group)
            .is_some_and(|(_, members)| members.iter().any(|t| ceiling.allows_alone(t)))
}

/// Write `enabled`, named as the rows a GET shows, as `coworker`'s ceiling: the PUT's body, and
/// what a Bot's `set_plugin_for_bot` sends (#359), so the two cannot store different things.
async fn save(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    enabled: Vec<String>,
    version: Option<i64>,
) -> Result<(ToolSet, i64), Response> {
    // Named against the rows a GET shows now. A built-in is a row whether or not it is available,
    // so it can be switched on or off either way ON PURPOSE: the ceiling records the intent, and a
    // turn offers the tool only once it is there (the machine, once one is enrolled). A plugin
    // this server does not load has no row until it is switched on, so it may be kept, never
    // added. Nothing is written until every name is one.
    let (now, _) = match state.auth.store.ceiling_at(coworker_id).await {
        Ok(found) => found,
        Err(error) => return Err(unavailable(&error)),
    };
    let shown = match rows(state, account_id, &now).await {
        Ok(rows) => rows,
        Err(e) => return Err(unavailable(&e)),
    };
    let mut entries = BTreeSet::new();
    let mut named_groups: BTreeSet<String> = BTreeSet::new();
    for name in enabled {
        match shown.iter().find(|row| row["name"] == name.as_str()) {
            None => return Err(refusal(422, &format!("no tool or plugin named {name}"))),
            Some(row) if row["kind"] == "plugin" => {
                entries.insert(opengrok_policy::every_tool_of(&name))
            }
            // A group is switched by its own entry: its tools' choices are carried below.
            Some(_)
                if name == routine::ROW
                    || name == plugin_desk::ROW
                    || name == computer_desk::ROW =>
            {
                named_groups.insert(name);
                true
            }
            Some(_) => entries.insert(name),
        };
    }
    // Each group keeps its tools' own choices, whichever way its switch goes: on is those choices
    // (all of its tools when it has none yet), off is `-<group>` beside them (7 Oct 2026).
    for (group, members) in opengrok_policy::TOOL_GROUPS {
        let chosen: Vec<&str> = members
            .iter()
            .copied()
            .filter(|tool| now.allows_alone(tool))
            .collect();
        let on = named_groups.contains(*group);
        let custom = !chosen.is_empty() && chosen.len() < members.len();
        if on && chosen.is_empty() {
            entries.extend(members.iter().map(|t| t.to_string()));
        } else if on || custom {
            entries.extend(chosen.iter().map(|t| t.to_string()));
        }
        // Off with choices of its own kept beside `-<group>`; off with none (every tool at its
        // default) is the group's tools left out, as before, and on gives them back whole.
        if !on && custom {
            entries.insert(opengrok_policy::excluding(group));
        }
    }
    // A tool's own Never (`-<tool>`) and a lifted ask (`+<tool>`) are the person's choices, not
    // switches on this page: a write of the switches keeps them.
    if let ToolSet::Only(names) = &now {
        let groups: Vec<String> = opengrok_policy::TOOL_GROUPS
            .iter()
            .map(|(g, _)| opengrok_policy::excluding(g))
            .collect();
        entries.extend(
            names
                .iter()
                .filter(|e| (e.starts_with('-') || e.starts_with('+')) && !groups.contains(e))
                .cloned(),
        );
    }
    let tools = if entries.iter().all(|e| e.starts_with('-')) {
        ToolSet::None
    } else {
        ToolSet::Only(entries)
    };
    let (store, who, whom) = (&state.auth.store, account_id, coworker_id);
    match store
        .set_ceiling(who, whom, &tools, version, now_ms())
        .await
    {
        Ok(Some(version)) => Ok((tools, version)),
        Ok(None) => {
            let changed = "the tools changed since you looked";
            let body = json!({ "error": changed, "code": "ceiling-changed" });
            Err((StatusCode::CONFLICT, Json(body)).into_response())
        }
        Err(error) => Err(unavailable(&error)),
    }
}

/// Switch `plugin` on or off for `coworker`, as its Tools card would: every other row stays as a
/// GET shows it. `Ok(false)` when it already was. The caller has checked the Bot is the person's.
pub(crate) async fn set_plugin(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    plugin: &str,
    on: bool,
) -> Result<bool, String> {
    let said = |why: &str| why.to_string();
    let (now, version) = state
        .auth
        .store
        .ceiling_at(coworker_id)
        .await
        .map_err(|_| said("this Bot's tools could not be read now"))?;
    let shown = rows(state, account_id, &now)
        .await
        .map_err(|_| said("this Bot's tools could not be read now"))?;
    let row = shown
        .iter()
        .find(|row| row["kind"] == "plugin" && row["name"] == plugin);
    let Some(row) = row else {
        return Err(format!("{plugin} is not installed; call list_plugins."));
    };
    if row["enabled"] == on {
        return Ok(false);
    }
    let mut enabled: Vec<String> = shown
        .iter()
        .filter(|row| row["enabled"] == true && row["name"] != plugin)
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect();
    if on {
        enabled.push(plugin.to_string());
    }
    match save(state, account_id, coworker_id, enabled, Some(version)).await {
        Ok(_) => Ok(true),
        Err(response) if response.status() == StatusCode::CONFLICT => {
            Err(said("this Bot's tools changed just now; try again."))
        }
        Err(_) => Err(said("this Bot's tools could not be saved now")),
    }
}

/// `PUT /coworkers/{id}/tool-mode` `{"tool", "mode"}`: one tool's choice for this Bot (#359).
#[derive(serde::Deserialize)]
pub(super) struct ToolMode {
    /// A built-in's name, or a plugin tool's dotted `<plugin>.<server>.<tool>`.
    tool: String,
    /// `always`, `ask` or `never`.
    mode: String,
}

/// Set one tool's choice for this Bot, as its page's chip does (#359).
///
/// - `never`: out of the Bot's ceiling. A plugin admitted whole keeps the rest of its tools:
///   the tool gets an exclusion (`-<tool>`) beside `<plugin>.*`.
/// - `ask`: in the ceiling and in the grant's ask-first list, so a card asks each time.
/// - `always`: in the ceiling and out of the ask-first list. For a tool that asks by rule (a
///   delete, an uninstall, a removal), `+<tool>` records that the person lifted that card.
///
/// Answers `{tool, mode}`. 422 for a mode or a tool this Bot has no row for, 409 when the
/// ceiling is not a list of names (`all` or none) or changed meanwhile.
pub(super) async fn put_tool_mode(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<ToolMode>, JsonRejection>,
) -> Response {
    let (account_id, coworker_id) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let Ok(Json(ToolMode { tool, mode })) = body else {
        return refusal(422, "send {\"tool\", \"mode\"}");
    };
    if !matches!(mode.as_str(), "always" | "ask" | "never") {
        return refusal(422, "mode is always, ask or never");
    }
    let store = &state.auth.store;
    let (now, version) = match store.ceiling_at(&coworker_id).await {
        Ok(found) => found,
        Err(error) => return unavailable(&error),
    };
    let ToolSet::Only(mut names) = now else {
        return refusal(
            409,
            "this Bot's tools are not chosen by name yet; switch them in its Tools first",
        );
    };
    let plugin = tool.split_once('.').map(|(plugin, _)| plugin.to_string());
    let whole = plugin
        .as_ref()
        .is_some_and(|plugin| names.contains(&opengrok_policy::every_tool_of(plugin)));
    let builtin = Executor::every_builtin().any(|name| name == tool);
    if !builtin
        && !whole
        && !names.contains(&tool)
        && !names.contains(&opengrok_policy::excluding(&tool))
    {
        return refusal(422, &format!("this Bot has no tool named {tool}"));
    }
    let unasked = format!("+{tool}");
    names.remove(&opengrok_policy::excluding(&tool));
    names.remove(&unasked);
    match mode.as_str() {
        "never" => {
            names.remove(&tool);
            // A group's tool keeps its Never as an entry, so a group with every tool at Never
            // is told apart from a group left out entirely (switched off, at its defaults).
            if whole || opengrok_policy::group_of(&tool).is_some() {
                names.insert(opengrok_policy::excluding(&tool));
            }
        }
        _ => {
            if !whole {
                names.insert(tool.clone());
            }
            if mode == "always" && Executor::ASK_BY_RULE.contains(&tool.as_str()) {
                names.insert(unasked);
            }
        }
    }
    let ceiling = if names.is_empty() {
        ToolSet::None
    } else {
        ToolSet::Only(names)
    };
    match store
        .set_ceiling(&account_id, &coworker_id, &ceiling, Some(version), now_ms())
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            let body =
                json!({ "error": "the tools changed since you looked", "code": "ceiling-changed" });
            return (StatusCode::CONFLICT, Json(body)).into_response();
        }
        Err(error) => return unavailable(&error),
    }
    // The ask-first list is the grant's, beside the ceiling: the card for `ask`, none otherwise.
    let policy = match store.policy_for(&account_id, &coworker_id).await {
        Ok(policy) => policy,
        Err(error) => return unavailable(&error),
    };
    let mut asks: BTreeSet<String> = match policy.grant.map(|grant| grant.needs_approval) {
        Some(ToolSet::Only(names)) => names,
        Some(ToolSet::All) => {
            return refusal(
                409,
                "every tool of this Bot asks first; change that in its Tools",
            );
        }
        _ => BTreeSet::new(),
    };
    if mode == "ask" && !Executor::ASK_BY_RULE.contains(&tool.as_str()) {
        asks.insert(tool.clone());
    } else {
        asks.remove(&tool);
    }
    let asks = if asks.is_empty() {
        ToolSet::None
    } else {
        ToolSet::Only(asks)
    };
    match store
        .set_needs_approval(&account_id, &coworker_id, &asks, now_ms())
        .await
    {
        Ok(true) => Json(json!({ "tool": tool, "mode": mode })).into_response(),
        Ok(false) => refusal(403, "no grant to change"),
        Err(error) => unavailable(&error),
    }
}

/// One installed plugin skill and whether it is on for this Bot.
async fn plugin_skill_rows(
    state: &AgUiState,
    account: &AccountId,
    bot: &CoworkerId,
) -> Result<Vec<Value>, Response> {
    let store = &state.auth.store;
    let installs = opengrok_integrations::installed::skills_for_turn(store, account, bot)
        .await
        .map_err(|error| unavailable(&error))?;
    let policy = store
        .policy_for(account, bot)
        .await
        .map_err(|error| unavailable(&error))?;
    let mut rows = Vec::new();
    for (plugin, _revision, skills) in installs {
        for (skill, text) in skills {
            let front = opengrok_plugins::split_frontmatter(&text);
            rows.push(json!({
                "plugin": plugin,
                "skill": skill,
                "description": front.description.unwrap_or_default(),
                "on": !opengrok_policy::skill_switched_off(&policy, &plugin, &skill),
                "body": front.body,
            }));
        }
    }
    Ok(rows)
}

/// `GET /coworkers/{id}/plugin-skills`: every installed plugin's skills with their switch for this
/// Bot, so its Plugins page can switch one off without the rest of the plugin. `on` is the skill's
/// own switch; the plugin's switch still gates them all.
pub(super) async fn list_plugin_skills(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (account, bot) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match plugin_skill_rows(&state, &account, &bot).await {
        Ok(mut rows) => {
            for row in &mut rows {
                if let Some(row) = row.as_object_mut() {
                    row.remove("body");
                }
            }
            Json(json!({ "skills": rows })).into_response()
        }
        Err(refused) => refused,
    }
}

/// `GET /coworkers/{id}/plugin-skills/{plugin}/{skill}`: one plugin skill with its text, for its
/// read-only page.
pub(super) async fn get_plugin_skill(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((id, plugin, skill)): Path<(String, String, String)>,
) -> Response {
    let (account, bot) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match plugin_skill_rows(&state, &account, &bot).await {
        Ok(rows) => rows
            .into_iter()
            .find(|row| row["plugin"] == plugin.as_str() && row["skill"] == skill.as_str())
            .map_or_else(
                || refusal(404, "no such plugin skill"),
                |row| Json(row).into_response(),
            ),
        Err(refused) => refused,
    }
}

#[derive(serde::Deserialize)]
pub(super) struct PluginSkillSwitch {
    plugin: String,
    skill: String,
    on: bool,
}

/// `PUT /coworkers/{id}/plugin-skills` `{plugin, skill, on}`: one plugin skill on or off for this
/// Bot. Off is a `-skill:<plugin>.<skill>` ceiling entry, kept across ceiling saves like a tool's
/// Never, so the plugin's own switch never forgets it.
pub(super) async fn put_plugin_skill(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<PluginSkillSwitch>, JsonRejection>,
) -> Response {
    let (account_id, coworker_id) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let Ok(Json(PluginSkillSwitch { plugin, skill, on })) = body else {
        return refusal(422, "send {\"plugin\", \"skill\", \"on\"}");
    };
    let known = match plugin_skill_rows(&state, &account_id, &coworker_id).await {
        Ok(rows) => rows
            .iter()
            .any(|row| row["plugin"] == plugin.as_str() && row["skill"] == skill.as_str()),
        Err(refused) => return refused,
    };
    if !known {
        return refusal(422, &format!("no installed plugin skill {plugin}.{skill}"));
    }
    let store = &state.auth.store;
    let (now, version) = match store.ceiling_at(&coworker_id).await {
        Ok(found) => found,
        Err(error) => return unavailable(&error),
    };
    let ToolSet::Only(mut names) = now else {
        return refusal(
            409,
            "this Bot's tools are not chosen by name yet; switch them in its Tools first",
        );
    };
    let entry = opengrok_policy::excluding(&opengrok_policy::skill_entry(&plugin, &skill));
    if on {
        names.remove(&entry);
    } else {
        names.insert(entry);
    }
    match store
        .set_ceiling(
            &account_id,
            &coworker_id,
            &ToolSet::Only(names),
            Some(version),
            now_ms(),
        )
        .await
    {
        Ok(Some(_)) => Json(json!({ "plugin": plugin, "skill": skill, "on": on })).into_response(),
        Ok(None) => {
            let body =
                json!({ "error": "the tools changed since you looked", "code": "ceiling-changed" });
            (StatusCode::CONFLICT, Json(body)).into_response()
        }
        Err(error) => unavailable(&error),
    }
}

/// `GET /coworkers/{id}/saved-login`: whether a saved login can be filled for this Bot, asked by
/// the app BEFORE Touch ID so a refusal never follows a fingerprint (8 Oct 2026).
/// `{usable, reason, ownComputer, ownBotsOnly}`; reason is `shared-computer` or `shared-bot`, or null.
pub(super) async fn get_saved_login(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (account, bot) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let reason = super::user_form::saved_login_refusal(&state, &account, &bot).await;
    let (_, _, scope, _, mode) = super::provision::scope_of(&state, &account, bot.as_str()).await;
    Json(json!({
        "usable": reason.is_none(),
        "reason": reason,
        "ownComputer": mode == opengrok_core::coworker::BoxMode::Dedicated,
        // Only the person's own Bots use this computer, so "share my logins with all my Bots"
        // would let a saved login fill here (9 Oct 2026).
        "ownBotsOnly": scope == "account",
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
pub(super) struct OwnComputer {
    on: bool,
}

/// `PUT /coworkers/{id}/own-computer` `{on}`: this Bot gets a computer of its own while the
/// account's other Bots keep sharing theirs, or goes back to the account's setting. It starts
/// on a fresh computer; what it left on the shared one stays there. A group always has its own.
pub(super) async fn put_own_computer(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<OwnComputer>, JsonRejection>,
) -> Response {
    let (account, bot) = match owner(&state, &headers, id).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let Ok(Json(OwnComputer { on })) = body else {
        return refusal(422, "send {\"on\"}");
    };
    let store = &state.auth.store;
    if store
        .load_coworker(&bot)
        .await
        .is_ok_and(|(coworker, _)| coworker.is_group())
    {
        return refusal(422, "a group already has a computer of its own");
    }
    let saved = if on {
        store
            .set_sharing_mode("bot", bot.as_str(), "per-bot", now_ms())
            .await
    } else {
        store.clear_sharing_mode("bot", bot.as_str()).await
    };
    if let Err(error) = saved {
        return unavailable(&error);
    }
    // Its own computer is made now, as the pane's Start makes one: a turn finds a Bot's computer
    // by its scope and does not make one, so without this the Bot had no computer at all.
    if on {
        let Ok((mut coworker, seq)) = store.load_coworker(&bot).await else {
            return unavailable(&"this Bot could not be read");
        };
        let at_ms = now_ms();
        let provisioned =
            super::provision::ensure_computer_for(&state, &account, &bot, &mut coworker, at_ms)
                .await;
        if let Some(why) = provisioned.error.as_ref() {
            tracing::warn!(?why, bot = %bot, "a Bot's own computer could not be made now");
        }
        if !provisioned.events.is_empty() {
            let view = opengrok_core::coworker::CoworkerView::of(bot.clone(), &coworker, at_ms);
            if let Err(error) = store
                .append_coworker(&bot, &account, seq, &provisioned.events, &view)
                .await
            {
                return unavailable(&error);
            }
        }
    }
    let (_, _, _, _, mode) = super::provision::scope_of(&state, &account, bot.as_str()).await;
    Json(json!({ "ownComputer": mode == opengrok_core::coworker::BoxMode::Dedicated }))
        .into_response()
}
