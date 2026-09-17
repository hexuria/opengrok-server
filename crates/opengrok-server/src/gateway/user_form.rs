//! `submitUserForm` / `dismissUserForm` — gateway verbs and the AG-UI REST twins.
//!
//! WHY THIS IS NOT `submitSecret`. That verb stamps `secretProvided` and DROPS the value
//! (connector vault). This path types into the live page. Mixing them would either vault a
//! Google password or fill a connector secret into Chromium.
//!
//! NativeChat talks AG-UI with an account bearer, not the gateway host bearer, so the REST
//! twins live on a router that has `GatewayState` (live emit + resume) but authenticates
//! like AG-UI (`account_from_bearer`), never through `refuse()`.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_core::run::{RunCommand, RunView};
use opengrok_tools::user_form::{
    FormRequest, FormResolution, audit_lengths, fill_into_focus, form_request_from, is_unresolved,
    is_user_form_entry, overall_resolution, shared_values, submitted_values, tool_result_content,
};
use serde_json::{Value, json};

use super::{GatewayState, conversation, live};

pub fn agui_router(state: GatewayState) -> Router {
    Router::new()
        .route("/ag-ui/user-form/submit", post(agui_submit))
        .route("/ag-ui/user-form/dismiss", post(agui_dismiss))
        .with_state(state)
}

async fn agui_submit(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = crate::agui::routes::account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = submit_user_form(&state, &args, &account_id).await;
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

async fn agui_dismiss(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = crate::agui::routes::account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = dismiss_user_form(&state, &args, &account_id).await;
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

pub async fn submit_for_caller(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(account) = account_of(state, caller).await else {
        return (
            401,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    submit_user_form(state, args, &account).await
}

pub async fn dismiss_for_caller(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(account) = account_of(state, caller).await else {
        return (
            401,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    dismiss_user_form(state, args, &account).await
}

async fn account_of(state: &GatewayState, caller: &str) -> Option<AccountId> {
    state
        .agui
        .auth
        .store
        .account_by_email(caller)
        .await
        .ok()
        .flatten()
        .map(|account| account.id)
}

/// `submitUserForm {entryId, values, agentId, platform?}`. Types into the box, settles
/// `formResolution`, resumes with a secret-free tool result.
pub async fn submit_user_form(
    state: &GatewayState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let Some((entry_id, agent_id, coworker_id)) = named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    if !may_use(state, account_id, &coworker_id).await {
        return (200, Value::Null);
    }
    let Ok(Some((seq, entry))) = state
        .agui
        .auth
        .store
        .find_gateway_entry(&coworker_id, account_id, &entry_id)
        .await
    else {
        return (200, Value::Null);
    };
    if !is_user_form_entry(&entry) {
        return (400, json!({ "error": "that entry is not a user-form" }));
    }
    if !is_unresolved(&entry) {
        return heal_or_already(state, account_id, &coworker_id, &agent_id, &entry).await;
    }

    let form = form_request_from(&entry);
    let values = submitted_values(&form, args.get("values").unwrap_or(&Value::Null));
    audit_lengths(&form, &values);

    let outcomes = fill_on_box(state, account_id, &coworker_id, &form, &values).await;
    let resolution = overall_resolution(&outcomes);
    let shared = shared_values(&form, &values);
    let content = tool_result_content(&form, resolution, &shared);

    let settled = settle_entry(entry, resolution, &shared, false);
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker_id, account_id, seq, &settled)
        .await
    {
        tracing::error!(%error, "could not settle a user-form entry");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    live::emit_transcript(state, &agent_id, account_id, "updated", settled.clone());

    resume_user_form(state, account_id, &coworker_id, &agent_id, content).await;
    (200, settled)
}

/// `dismissUserForm {entryId, mode: dismissed|escalated, agentId, platform?}`. No fill.
pub async fn dismiss_user_form(
    state: &GatewayState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let Some((entry_id, agent_id, coworker_id)) = named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or_default();
    let resolution = match mode {
        "dismissed" => FormResolution::Dismissed,
        "escalated" => FormResolution::Escalated,
        _ => {
            return (
                400,
                json!({ "error": "mode must be dismissed or escalated" }),
            );
        }
    };
    if !may_use(state, account_id, &coworker_id).await {
        return (200, Value::Null);
    }
    let Ok(Some((seq, entry))) = state
        .agui
        .auth
        .store
        .find_gateway_entry(&coworker_id, account_id, &entry_id)
        .await
    else {
        return (200, Value::Null);
    };
    if !is_user_form_entry(&entry) {
        return (400, json!({ "error": "that entry is not a user-form" }));
    }
    if !is_unresolved(&entry) {
        return heal_or_already(state, account_id, &coworker_id, &agent_id, &entry).await;
    }

    let form = form_request_from(&entry);
    let content = tool_result_content(&form, resolution, &BTreeMap::new());
    let settled = settle_entry(entry, resolution, &BTreeMap::new(), true);
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker_id, account_id, seq, &settled)
        .await
    {
        tracing::error!(%error, "could not settle a user-form entry");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    live::emit_transcript(state, &agent_id, account_id, "updated", settled.clone());

    resume_user_form(state, account_id, &coworker_id, &agent_id, content).await;
    (200, settled)
}

fn named_entry(args: &Value) -> Option<(String, String, CoworkerId)> {
    let entry_id = args.get("entryId").and_then(Value::as_str)?;
    let agent_id = args
        .get("agentId")
        .or_else(|| args.get("id"))
        .and_then(Value::as_str)?;
    if entry_id.is_empty() || agent_id.is_empty() {
        return None;
    }
    Some((
        entry_id.to_string(),
        agent_id.to_string(),
        CoworkerId::from_stored(agent_id),
    ))
}

async fn may_use(state: &GatewayState, account_id: &AccountId, coworker: &CoworkerId) -> bool {
    state
        .agui
        .auth
        .store
        .may_use_coworker(account_id, coworker)
        .await
        .unwrap_or(false)
}

fn settle_entry(
    mut entry: Value,
    resolution: FormResolution,
    shared: &BTreeMap<String, String>,
    widget_dismissed: bool,
) -> Value {
    if let Some(map) = entry.as_object_mut() {
        map.insert("formResolution".to_string(), json!(resolution.as_str()));
        map.remove("values");
        if widget_dismissed {
            map.insert("widgetDismissed".to_string(), json!(true));
        }
        if shared.is_empty() {
            map.remove("sharedValues");
        } else {
            map.insert("sharedValues".to_string(), json!(shared));
        }
    }
    entry
}

async fn fill_on_box(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    form: &FormRequest,
    values: &BTreeMap<String, String>,
) -> Vec<opengrok_tools::user_form::FieldOutcome> {
    let Some(runner) = crate::agui::routes::tools_for_coworker(
        &state.agui,
        account_id,
        coworker_id,
        &[],
        &[],
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await
    else {
        return form
            .fields
            .iter()
            .map(|field| opengrok_tools::user_form::FieldOutcome {
                id: field.id.clone(),
                filled: false,
                fill_failed: true,
            })
            .collect();
    };
    let Some((computer, box_id)) = runner.fill_target() else {
        return form
            .fields
            .iter()
            .map(|field| opengrok_tools::user_form::FieldOutcome {
                id: field.id.clone(),
                filled: false,
                fill_failed: true,
            })
            .collect();
    };
    fill_into_focus(computer.as_ref(), &box_id, form, values).await
}

async fn heal_or_already(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
    entry: &Value,
) -> (u16, Value) {
    let form = form_request_from(entry);
    let shared = entry
        .get("sharedValues")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(id, value)| Some((id.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let word = entry.get("formResolution").and_then(Value::as_str);
    let resolution = match word {
        Some("submitted") => FormResolution::Submitted,
        Some("fill_failed") => FormResolution::FillFailed,
        Some("escalated") => FormResolution::Escalated,
        _ => FormResolution::Dismissed,
    };
    let content = tool_result_content(&form, resolution, &shared);
    if resume_user_form(state, account_id, coworker_id, agent_id, content).await {
        return (200, entry.clone());
    }
    (200, json!({ "alreadyAnswered": true }))
}

/// Answer a pending `UserForm` run and resume it with a synthesised result. `true` when a
/// pending run was found and answered (a retry can pick up a card that settled before the
/// run did).
async fn resume_user_form(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
    content: String,
) -> bool {
    let Some((run_id, mut run, seq, pending)) =
        pending_user_form(state, account_id, coworker_id).await
    else {
        return false;
    };
    let resumed_seq = run.emitted.len() as u32;
    let at_ms = now_ms();
    let events = match run.decide(RunCommand::Answer {
        call_id: pending.call_id.clone(),
        approved: true,
        by: account_id.to_string(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(opengrok_core::run::RunError::AlreadyAnswered) => return false,
        Err(error) => {
            tracing::warn!(%error, "user-form: could not answer the pending run");
            return false;
        }
    };
    for event in &events {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_run(&run_id, seq, &events, &view, Some(account_id))
        .await
    {
        tracing::error!(%error, "user-form: could not append the answer");
        return false;
    }
    let in_room = conversation::in_a_room(&run, coworker_id);
    let state = state.clone();
    let account_id = account_id.clone();
    let coworker_id = coworker_id.clone();
    let agent_id = agent_id.to_string();
    tokio::spawn(conversation::resume_where_it_lives(
        in_room,
        state,
        account_id,
        run_id,
        coworker_id,
        agent_id,
        pending,
        resumed_seq,
        opengrok_harness::ResumeOutcome::Settled(content),
    ));
    true
}

async fn pending_user_form(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Option<(
    opengrok_core::id::RunId,
    opengrok_core::run::Run,
    i64,
    opengrok_core::run::PendingApproval,
)> {
    let run_ids = state
        .agui
        .auth
        .store
        .awaiting_approval(account_id)
        .await
        .ok()?;
    for run_id in run_ids {
        let Ok((run, seq)) = state.agui.auth.store.load_run(&run_id).await else {
            continue;
        };
        if !conversation::run_belongs_to(&run, coworker_id) {
            continue;
        }
        let Some(pending) = run.pending.clone() else {
            continue;
        };
        if pending.reason != opengrok_core::run::SuspendReason::UserForm {
            continue;
        }
        return Some((run_id, run, seq, pending));
    }
    None
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
