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
use opengrok_tools::{Executor, USER_MACHINE_SHELL, routine};
use serde_json::{Value, json};

use super::routes::{AgUiState, account_from_bearer, now_ms, owned_coworker};
use crate::health::refusal;

/// The built-ins' rows: one per built-in, and ONE for the four routine tools, which it switches
/// together (#316, the owner's call): a person decides whether a Bot keeps routines, not which
/// verb of it.
fn builtin_rows() -> impl Iterator<Item = &'static str> {
    let rows = Executor::every_builtin().filter(|name| !routine::is_routine_tool(name));
    rows.chain([routine::ROW])
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
            let on = match name {
                routine::ROW => routine::TOOLS.iter().all(|tool| ceiling.allows(tool)),
                name => ceiling.allows(name),
            };
            let mut row = json!({ "name": name, "kind": "builtin",
                "description": description, "enabled": on });
            if name == USER_MACHINE_SHELL {
                row["available"] = json!(machine.is_some());
            }
            if name == routine::ROW {
                row["label"] = json!(routine::ROW_LABEL);
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
        let mut row = json!({ "name": name, "kind": "plugin", "label": name,
            "enabled": ceiling.allows_all_of(name) });
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
    // Named against the rows a GET shows now. A built-in is a row whether or not it is available,
    // so it can be switched on or off either way ON PURPOSE: the ceiling records the intent, and a
    // turn offers the tool only once it is there (the machine, once one is enrolled). A plugin
    // this server does not load has no row until it is switched on, so it may be kept, never
    // added. Nothing is written until every name is one.
    let (now, _) = match state.auth.store.ceiling_at(&coworker_id).await {
        Ok(found) => found,
        Err(error) => return unavailable(&error),
    };
    let shown = match rows(&state, &account_id, &now).await {
        Ok(rows) => rows,
        Err(e) => return unavailable(&e),
    };
    let mut entries = BTreeSet::new();
    for name in enabled {
        match shown.iter().find(|row| row["name"] == name.as_str()) {
            None => return refusal(422, &format!("no tool or plugin named {name}")),
            Some(row) if row["kind"] == "plugin" => {
                entries.insert(opengrok_policy::every_tool_of(&name))
            }
            Some(_) if name == routine::ROW => {
                entries.extend(routine::TOOLS.map(str::to_string));
                true
            }
            Some(_) => entries.insert(name),
        };
    }
    let tools = if entries.is_empty() {
        ToolSet::None
    } else {
        ToolSet::Only(entries)
    };
    let (store, who, whom) = (&state.auth.store, &account_id, &coworker_id);
    match store
        .set_ceiling(who, whom, &tools, version, now_ms())
        .await
    {
        Ok(Some(version)) => reply(&state, &account_id, &tools, version).await,
        Ok(None) => {
            let changed = "the tools changed since you looked";
            let body = json!({ "error": changed, "code": "ceiling-changed" });
            (StatusCode::CONFLICT, Json(body)).into_response()
        }
        Err(error) => unavailable(&error),
    }
}
