//! Computer, tool, usage, and key routes.

use super::*;

/// `GET /coworkers/{id}/computer` — live box state and noVNC URL for NativeChat's Open button.
pub(super) async fn computer_status(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    Json(provision::coworker_screen(&state, &headers, &account_id, &coworker_id).await)
        .into_response()
}

/// The person's standing answer, for this coworker's computer, to the tunnel's card.
#[derive(serde::Deserialize)]
pub(super) struct EgressPolicyBody {
    mode: String,
}

/// The coworker's scoped box row for a policy read or write, or the refusal: 401 without a
/// bearer, 404 for another account's coworker or one with no computer. The provider is not
/// needed — the preference is a row keyed by the scope, and the pane paints the control even
/// while the provider cannot be built.
async fn egress_policy_box(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
    coworker_id: String,
) -> Result<(AccountId, CoworkerId, provision::ScopedBoxRow), Response> {
    let (account_id, coworker_id) = owner_of(state, headers, coworker_id).await?;
    match provision::scoped_box_row_for(state, &account_id, &coworker_id).await {
        Some(row) => Ok((account_id, coworker_id, row)),
        None => Err((StatusCode::NOT_FOUND, "this coworker has no computer").into_response()),
    }
}

/// `GET /coworkers/{id}/computer/egress-policy` — `{ scope, scopeId, boxId, mode }`; an unset
/// policy reads as `ask`, which is what it means.
pub(super) async fn get_egress_policy(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (_, _, scoped) = match egress_policy_box(&state, &headers, coworker_id).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let Ok(mode) = provision::egress_policy_read(&state, scoped.scope, &scoped.scope_id).await
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the network policy could not be read right now",
        )
            .into_response();
    };
    Json(serde_json::json!({
        "scope": provision::share_scope_of(scoped.scope),
        "scopeId": scoped.scope_id,
        "boxId": scoped.box_id,
        "mode": mode.as_stored(),
    }))
    .into_response()
}

/// `PUT /coworkers/{id}/computer/egress-policy` `{ "mode": "bypass" | "ask" | "never" }` — 204.
/// The choice is kept whether or not the tunnel is on right now: it says what happens when it is.
pub(super) async fn set_egress_policy(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    Json(body): Json<EgressPolicyBody>,
) -> Response {
    if !opengrok_tools::EgressPolicy::is_valid(&body.mode) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown mode").into_response();
    }
    let (_, _, scoped) = match egress_policy_box(&state, &headers, coworker_id).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    // An org-shared computer is every member's: its standing consent is the org admin's to
    // set, like the org's sharing mode. A member's own box, or the box of a group they run,
    // is theirs.
    if scoped.scope == "org"
        && let Err(refusal) = crate::account_api::admin_org(&state.auth, &headers).await
    {
        return refusal;
    }
    match state
        .auth
        .store
        .set_egress_policy_mode(
            scoped.scope,
            &scoped.scope_id,
            &body.mode,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not set the egress policy");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// `GET /coworkers/{id}/screen` — the box's display as a PNG, for the Computer pane's tile
/// and for an explicit observe / Open the screen. Same shape as `TOOL_CALL_RESULT.image`,
/// with `visibility: transcript` so a client that fetches this on purpose may persist it.
/// Step shots on the run are `agent` and must not flood the transcript.
/// `GET /coworkers/{id}/tools` — what this bot would be offered on a turn RIGHT NOW.
///
/// The set is assembled per turn from the coworker's grant, its computer and its plugins, and
/// until now it was only ever built inside the run handler and thrown away. A client that wants
/// to name a tool has to be able to see the names, and a name it cannot see is a name it would
/// guess wrong — so this answers with exactly what the model is told, nothing invented.
pub(super) async fn list_tools(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    // No approvals are pending on a listing, so the two gates are empty; the patience is short
    // because nobody is waiting on a turn — a sleeping box should not hold a menu open. No
    // computer is no box tools, and still `use_skill` when a turn would offer it (#270).
    let runner = tools_for_coworker(
        &state,
        &account_id,
        &coworker_id,
        &[],
        &[],
        std::time::Duration::from_secs(5),
    )
    .await;
    let runner = runner.unwrap_or_else(ToolRunner::local_only);
    let runner = crate::skills::onto(&state, &account_id, &coworker_id, runner).await;
    let who = (&account_id, &coworker_id);
    let runner = crate::pairs::onto(&state, who, None, None, Some(runner)).await;
    let runner = runner.unwrap_or_else(ToolRunner::local_only);
    let tools: Vec<serde_json::Value> = runner
        .tool_schemas()
        .into_iter()
        .filter_map(|schema| {
            let function = schema.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            // OpenAI-safe plugin names have no dots (`gmail_api_send`). Kind is
            // "not a builtin", not "contains a dot".
            let kind = if opengrok_tools::Executor::every_builtin().any(|builtin| builtin == name)
                || name == opengrok_tools::skill::USE_SKILL
            {
                "builtin"
            } else {
                "plugin"
            };
            let mut row = serde_json::json!({
                "name": name,
                "description": function.get("description").and_then(serde_json::Value::as_str).unwrap_or(""),
                "kind": kind,
            });
            // Which plugin a tool is from, and what its server calls it for a person (#359), so
            // an app can group a plugin's tools and show "@cloudflare:" without parsing names.
            let mut policy_name = name.clone();
            if let Some((executor, _)) = runner.executor()
                && let Some((qualified, title)) = executor.plugin_tool_named(&name)
            {
                row["plugin"] = serde_json::json!(qualified.split('.').next().unwrap_or_default());
                row["qualified"] = serde_json::json!(qualified);
                if let Some(title) = title {
                    row["title"] = serde_json::json!(title);
                }
                policy_name = qualified;
            }
            // This Bot's choice for the tool (#359): `always`, `ask` (a card first) or `never`.
            row["mode"] = serde_json::json!(runner.executor().map_or("always", |(executor, context)| {
                executor.mode_of(&context.account_id, &context.coworker_id, &policy_name)
            }));
            Some(row)
        })
        .collect();
    // The routine tools are listed as their ceiling's one row (#316), as the Tools card shows it,
    // and so are the plugin tools (#359).
    use opengrok_tools::{computer_desk, office_desk, plugin_desk, routine};
    let routines =
        |row: &serde_json::Value| row["name"].as_str().is_some_and(routine::is_routine_tool);
    let (offered, tools): (Vec<_>, Vec<_>) = tools.into_iter().partition(routines);
    let desk = |row: &serde_json::Value| {
        row["name"]
            .as_str()
            .is_some_and(plugin_desk::is_plugin_desk_tool)
    };
    let (desk_offered, tools): (Vec<_>, Vec<_>) = tools.into_iter().partition(desk);
    let computer = |row: &serde_json::Value| {
        row["name"]
            .as_str()
            .is_some_and(computer_desk::is_computer_tool)
    };
    let (computer_offered, tools): (Vec<_>, Vec<_>) = tools.into_iter().partition(computer);
    let office = |row: &serde_json::Value| {
        row["name"]
            .as_str()
            .is_some_and(office_desk::is_office_tool)
    };
    let (office_offered, mut tools): (Vec<_>, Vec<_>) = tools.into_iter().partition(office);
    // A plugin's tools the person switched off for this Bot are listed too, as `never`, so its
    // page can switch them back on; they are not offered to the Bot.
    for tool in runner
        .executor()
        .into_iter()
        .flat_map(|(executor, _)| executor.switched_off())
    {
        let plugin = tool
            .qualified_name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string();
        let mut row = serde_json::json!({
            "name": opengrok_tools::openai_safe_tool_name(&tool.qualified_name),
            "description": tool.description.clone().unwrap_or_default(),
            "kind": "plugin", "plugin": plugin, "qualified": tool.qualified_name, "mode": "never",
        });
        let title = tool
            .annotations
            .as_ref()
            .and_then(|notes| notes.get("title"))
            .and_then(serde_json::Value::as_str);
        if let Some(title) = title {
            row["title"] = serde_json::json!(title);
        }
        tools.push(row);
    }
    // Each group lists every one of its tools in the group's own order, the offered ones with
    // this Bot's choice and the rest as `never`, so a list never moves under the person's pointer
    // and a group with every tool off still has a page to switch them back on from (#359).
    let group = |offered: Vec<serde_json::Value>,
                 members: &[&str],
                 describe: fn(&str) -> Option<&'static str>| {
        members
            .iter()
            .map(|name| {
                offered
                    .iter()
                    .find(|row| row["name"] == *name)
                    .cloned()
                    .unwrap_or_else(|| {
                        // Not offered: at Never, or its group is switched off, which keeps the
                        // tool's own choice and shows it.
                        serde_json::json!({ "name": name,
                            "description": describe(name).unwrap_or_default(),
                            "kind": "builtin", "mode": runner.executor().map_or("always", |(executor, context)| {
                                executor.mode_of_alone(&context.account_id, &context.coworker_id, name)
                            }) })
                    })
            })
            .collect::<Vec<_>>()
    };
    let offered = group(offered, &routine::TOOLS, routine::description);
    let desk_offered = group(desk_offered, &plugin_desk::TOOLS, plugin_desk::description);
    let computer_offered = group(
        computer_offered,
        &computer_desk::TOOLS,
        computer_desk::description,
    );
    let office_offered = group(
        office_offered,
        &office_desk::TOOLS,
        office_desk::description,
    );
    // `enabled` is the group's own switch (7 Oct 2026), apart from its tools' choices.
    tools.push(serde_json::json!({ "name": routine::ROW,
        "description": routine::ROW_DESCRIPTION, "kind": "builtin", "tools": offered,
        "enabled": !runner.executor().is_some_and(|(executor, _)| executor.group_off(routine::ROW)) }));
    tools.push(serde_json::json!({ "name": plugin_desk::ROW,
        "description": plugin_desk::ROW_DESCRIPTION, "kind": "builtin", "tools": desk_offered,
        "enabled": !runner.executor().is_some_and(|(executor, _)| executor.group_off(plugin_desk::ROW)) }));
    tools.push(serde_json::json!({ "name": computer_desk::ROW,
        "description": computer_desk::ROW_DESCRIPTION, "kind": "builtin", "tools": computer_offered,
        "enabled": !runner.executor().is_some_and(|(executor, _)| executor.group_off(computer_desk::ROW)) }));
    tools.push(serde_json::json!({ "name": office_desk::ROW,
        "description": office_desk::ROW_DESCRIPTION, "kind": "builtin", "tools": office_offered,
        "enabled": !runner.executor().is_some_and(|(executor, _)| executor.group_off(office_desk::ROW)) }));
    // What a person reads about each built-in and group, beside the model's words (#359).
    fn for_people(row: &mut serde_json::Value) {
        if let Some((label, summary)) = row["name"]
            .as_str()
            .and_then(opengrok_tools::Executor::builtin_for_people)
            .filter(|_| row["kind"] == "builtin")
        {
            row["label"] = serde_json::json!(label);
            row["summary"] = serde_json::json!(summary);
        }
        if let Some(members) = row
            .get_mut("tools")
            .and_then(serde_json::Value::as_array_mut)
        {
            members.iter_mut().for_each(for_people);
        }
    }
    tools.iter_mut().for_each(for_people);
    Json(serde_json::json!({ "tools": tools })).into_response()
}

pub(super) async fn computer_screen(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    match provision::coworker_screenshot(&state, &account_id, &coworker_id).await {
        Ok(shot) => Json(serde_json::json!({
            "mime": shot.mime,
            "base64": shot.png_base64,
            "width": shot.width,
            "height": shot.height,
            // Explicit observe / Open the screen: this PNG is a transcript event
            // the client may persist. Step shots on TOOL_CALL_RESULT are `agent`.
            "visibility": "transcript",
        }))
        .into_response(),
        Err((status, message)) => (status, message).into_response(),
    }
}

/// `POST /coworkers/{id}/computer/update` — rebuild the coworker's computer on the newest image,
/// keeping its files. Answers 202 with the status as accepted; the phases and the outcome arrive
/// on `GET …/computer`.
pub(super) async fn computer_update(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    let update = match provision::begin_update_for_coworker(&state, &account_id, &coworker_id).await
    {
        Ok(update) => update,
        Err((status, message)) => return (status, message).into_response(),
    };
    let accepted = provision::coworker_screen(&state, &headers, &account_id, &coworker_id).await;
    let _ = update.send(());
    (StatusCode::ACCEPTED, Json(accepted)).into_response()
}

/// `POST /coworkers/{id}/computer/reset` — destroy the computer, data and all, and start fresh.
pub(super) async fn computer_reset(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    if let Err((code, message)) =
        provision::reset_for_coworker(&state, &account_id, &coworker_id).await
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{code}: {message}"),
        )
            .into_response();
    }
    Json(provision::coworker_screen(&state, &headers, &account_id, &coworker_id).await)
        .into_response()
}

/// `POST /coworkers/{id}/computer` — ensure the box is running, then return the same status.
pub(super) async fn ensure_computer(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    let Ok((mut coworker, seq)) = state.auth.store.load_coworker(&coworker_id).await else {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    };
    let at_ms = now_ms();
    let provisioned =
        provision::ensure_computer_for(&state, &account_id, &coworker_id, &mut coworker, at_ms)
            .await;
    if !provisioned.events.is_empty() {
        for event in &provisioned.events {
            coworker.apply(event);
        }
        let view = CoworkerView::of(coworker_id.clone(), &coworker, at_ms);
        if let Err(error) = state
            .auth
            .store
            .append_coworker(&coworker_id, &account_id, seq, &provisioned.events, &view)
            .await
        {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }
    provision::wake_coworker_computer(&state, &account_id, &coworker_id).await;
    Json(provision::coworker_screen(&state, &headers, &account_id, &coworker_id).await)
        .into_response()
}

/// `GET /coworkers/{id}/spend` — the coworker's three meters and the limits it is under.
pub(super) async fn get_spend(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    match crate::spend::spend_for(&state, &account_id, &coworker_id).await {
        Ok(spend) => Json(spend).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub window: Option<String>,
}

/// `GET /coworkers/{id}/usage?window=5h|24h|7d|month` — what the coworker used, per model.
pub(super) async fn get_usage(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<UsageQuery>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    let window = query.window.as_deref().unwrap_or("month");
    match crate::points::usage_for(&state, &account_id, &coworker_id, window).await {
        Ok(usage) => Json(usage).into_response(),
        Err((code, error)) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `GET /coworkers/{id}/limit` — the coworker's cap and brake, what it has used, and the pool
/// it draws on.
pub(super) async fn get_limit(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    match crate::points::limit_for(&state, &account_id, &coworker_id).await {
        Ok(limit) => Json(limit).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `PUT /coworkers/{id}/limit` ← `{ cap, dayCap }` — the owner's; null clears, absent leaves.
pub(super) async fn set_limit(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    match crate::points::set_limit(&state, &account_id, &coworker_id, &body).await {
        Ok(limit) => Json(limit).into_response(),
        Err((code, error)) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `GET /coworkers/{id}/keys` — the owner's bot keys for this coworker. Anyone else's id, a
/// shared coworker's included, is a 404 like every other coworker route: the query alone
/// answered `[]` to a stranger and an unknown id alike, which is the empty success that reads
/// as "no keys" rather than "not yours".
pub(super) async fn list_bot_keys(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let (account_id, coworker_id) = match owner_of(&state, &headers, coworker_id).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    let keys = state.auth.store.bot_keys_for(&account_id, &coworker_id);
    match keys.await {
        Ok(keys) => Json(keys).into_response(),
        Err(error) => {
            tracing::error!(%error, "could not list bot keys");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct McpCallsQuery {
    limit: Option<i64>,
}

/// `GET /coworkers/{id}/mcp-calls?limit=` — what this coworker's bot keys have been used for,
/// newest first. The owner only: another account's coworker id is a 404, not an empty list
/// (an empty success is the dangerous reply).
pub(super) async fn list_mcp_calls(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<McpCallsQuery>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    let owned = match state.auth.store.coworkers_for(&account_id).await {
        Ok(coworkers) => coworkers.iter().any(|c| c.id == coworker_id),
        Err(error) => {
            tracing::error!(%error, "could not list coworkers");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    if !owned {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    match state
        .auth
        .store
        .mcp_calls_for(&account_id, &coworker_id, limit)
        .await
    {
        Ok(calls) => Json(calls).into_response(),
        Err(error) => {
            tracing::error!(%error, "could not list mcp door calls");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// `DELETE /coworkers/{id}/keys/{jti}` — keyed by the caller's own key, not by the path's
/// coworker, and deliberately not gated on `owned_coworker`: retiring a coworker does not revoke
/// its bot keys, so a gate on the retired (off-roster) row would leave a key the owner still holds
/// live with no door to revoke it through. Somebody else's key, or an id that is no key, is the
/// same 404, so nothing about the path's coworker is confirmed.
pub(super) async fn revoke_bot_key(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((_coworker_id, jti)): axum::extract::Path<(String, String)>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    // An OAuth-minted key has refresh tokens that would mint it a successor; they die with it
    // in ONE transaction, or "revoke" would mean "revoke until the next refresh". A failure is
    // a 500 the person sees, never a 204 over a key that can still come back.
    match state
        .auth
        .store
        .revoke_bot_key_with_refresh(&account_id, &jti)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such key").into_response(),
        Err(error) => {
            tracing::error!(%error, "could not revoke a bot key");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}
