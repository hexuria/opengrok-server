//! `submitUserForm` / `dismissUserForm` / box-handoff resolve — gateway verbs and AG-UI REST twins.
//!
//! WHY THIS IS NOT `submitSecret`. That verb stamps `secretProvided` and DROPS the value
//! (connector vault). This path types into the live page. Mixing them would either vault a
//! Google password or fill a connector secret into Chromium.
//!
//! WHY ESCALATE IS NOT A RESUME. Official `dismissUserForm` mode `escalated` is Grok Bot
//! "Open the screen": emit a **separate** `sand://box` attachment with `boxRequestId` and keep
//! screen-hold until hand-back or decline. Resuming immediately (and clearing hold) races the
//! person on the computer — the Facebook hang after password when phone verify hits. This is
//! not OpenGrok Take over / I'm done / Skip, and it is not `handBackForeverBox` (lifecycle stop).
//!
//! NativeChat talks AG-UI with an account bearer, not the gateway host bearer, so the REST
//! twins live on a router that has `GatewayState` (live emit + resume) but authenticates
//! like AG-UI (`account_from_bearer`), never through `refuse()`.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_core::run::{RunCommand, RunView};
use opengrok_tools::user_form::{
    FieldOutcome, FormRequest, FormResolution, HAND_BACK_TOOL_RESULT, HANDOFF_DECLINED_TOOL_RESULT,
    HOLD_TIMED_OUT_TOOL_RESULT, audit_lengths, fill_into_focus, form_request_from,
    handoff_instruction, is_live_handoff, is_unresolved, is_user_form_entry, overall_resolution,
    shared_values, submitted_values, tool_result_content,
};
use serde_json::{Value, json};

use super::{GatewayState, conversation, live};

/// How long an unanswered form or live handoff may block the turn. Tests call the settlers
/// directly rather than waiting this out. Facebook hang: password fill reported submitted and
/// the OTP wait never ended.
pub const FORM_HOLD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub fn agui_router(state: GatewayState) -> Router {
    Router::new()
        .route("/ag-ui/user-form/submit", post(agui_submit))
        .route("/ag-ui/user-form/dismiss", post(agui_dismiss))
        .route("/ag-ui/box-handoff/resolve", post(agui_resolve_handoff))
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

async fn agui_resolve_handoff(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = crate::agui::routes::account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = resolve_box_handoff(&state, &args, &account_id).await;
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

pub async fn resolve_for_caller(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(account) = account_of(state, caller).await else {
        return (
            401,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    resolve_box_handoff(state, args, &account).await
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
/// `formResolution` plus `formFieldOutcomes`, resumes with a secret-free tool result that says
/// the values were filled into the page — not that login succeeded.
pub async fn submit_user_form(
    state: &GatewayState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let Some((entry_id, agent_id, coworker_id)) = named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    let (seq, entry) = match load_owned_entry(state, account_id, &coworker_id, &entry_id).await {
        Ok(row) => row,
        Err(reply) => return reply,
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
    let content = tool_result_content(&form, resolution, &shared, false);

    let settled = settle_entry(entry, resolution, &shared, false, &outcomes, false);
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
/// `dismissed` resumes. `escalated` starts a box handoff and does **not** resume.
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
    let (seq, entry) = match load_owned_entry(state, account_id, &coworker_id, &entry_id).await {
        Ok(row) => row,
        Err(reply) => return reply,
    };
    if !is_user_form_entry(&entry) {
        return (400, json!({ "error": "that entry is not a user-form" }));
    }
    if !is_unresolved(&entry) {
        return heal_or_already(state, account_id, &coworker_id, &agent_id, &entry).await;
    }

    let form = form_request_from(&entry);
    let settled = settle_entry(entry, resolution, &BTreeMap::new(), true, &[], false);
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

    if resolution == FormResolution::Escalated {
        let handoff = start_box_handoff(state, account_id, &coworker_id, &agent_id, &form).await;
        let mut response = settled;
        if let Some(id) = handoff
            .as_ref()
            .and_then(|card| card.get("id"))
            .and_then(Value::as_str)
        {
            // HTTP convenience for NativeChat. Not persisted on the user-form: a `boxRequestId`
            // on that card would convert it into a handoff.
            response["handoffEntryId"] = json!(id);
        }
        return (200, response);
    }

    let content = tool_result_content(&form, resolution, &BTreeMap::new(), false);
    resume_user_form(state, account_id, &coworker_id, &agent_id, content).await;
    (200, settled)
}

/// `resolveBoxHandoff {entryId, agentId, resolution: handed_back|declined|timed_out}`.
/// Stamps `boxResolution` on the attachment and resumes the waiting user-form run. Does **not**
/// stop the box (`handBackForeverBox` is a lifecycle verb).
pub async fn resolve_box_handoff(
    state: &GatewayState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let Some((entry_id, agent_id, coworker_id)) = named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    let word = args
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (content, timed_out) = match word {
        "handed_back" => (HAND_BACK_TOOL_RESULT.to_string(), false),
        "declined" => (HANDOFF_DECLINED_TOOL_RESULT.to_string(), false),
        "timed_out" => (HOLD_TIMED_OUT_TOOL_RESULT.to_string(), true),
        _ => {
            return (
                400,
                json!({ "error": "resolution must be handed_back, declined, or timed_out" }),
            );
        }
    };
    let (seq, entry) = match load_owned_entry(state, account_id, &coworker_id, &entry_id).await {
        Ok(row) => row,
        Err(reply) => return reply,
    };
    if !is_live_handoff(&entry) {
        if entry.get("boxRequestId").and_then(Value::as_str).is_some() {
            resume_user_form(state, account_id, &coworker_id, &agent_id, content).await;
            return (200, json!({ "alreadyAnswered": true }));
        }
        return (
            400,
            json!({ "error": "that entry is not a live box handoff" }),
        );
    }

    let settled = settle_handoff(entry, word, timed_out);
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker_id, account_id, seq, &settled)
        .await
    {
        tracing::error!(%error, "could not settle a box handoff");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    live::emit_transcript(state, &agent_id, account_id, "updated", settled.clone());
    resume_user_form(state, account_id, &coworker_id, &agent_id, content).await;
    (200, settled)
}

/// Spawned when a user-form card is minted. No-ops if the form already settled (including
/// escalate — that wait is the handoff timer).
pub fn spawn_form_hold_timeout(
    state: GatewayState,
    account_id: AccountId,
    coworker_id: CoworkerId,
    agent_id: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(FORM_HOLD_TIMEOUT).await;
        timeout_unresolved_form(&state, &account_id, &coworker_id, &agent_id).await;
    });
}

pub fn spawn_handoff_hold_timeout(
    state: GatewayState,
    account_id: AccountId,
    coworker_id: CoworkerId,
    agent_id: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(FORM_HOLD_TIMEOUT).await;
        timeout_live_handoff(&state, &account_id, &coworker_id, &agent_id).await;
    });
}

/// Settle every still-unresolved user-form as `dismissed` + `timedOut` and resume. Idempotent.
pub async fn timeout_unresolved_form(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
) -> bool {
    let Ok(entries) = state
        .agui
        .auth
        .store
        .gateway_transcript(coworker_id, account_id)
        .await
    else {
        return false;
    };
    let mut settled_any = false;
    let mut form_for_result: Option<FormRequest> = None;
    for entry in entries {
        if !is_unresolved(&entry) {
            continue;
        }
        let Some(entry_id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Ok(Some((seq, current))) = state
            .agui
            .auth
            .store
            .find_gateway_entry(coworker_id, account_id, entry_id)
            .await
        else {
            continue;
        };
        if !is_unresolved(&current) {
            continue;
        }
        let form = form_request_from(&current);
        let settled = settle_entry(
            current,
            FormResolution::Dismissed,
            &BTreeMap::new(),
            true,
            &[],
            true,
        );
        if let Err(error) = state
            .agui
            .auth
            .store
            .update_gateway_entry(coworker_id, account_id, seq, &settled)
            .await
        {
            tracing::error!(%error, "could not time out a user-form");
            continue;
        }
        live::emit_transcript(state, agent_id, account_id, "updated", settled);
        form_for_result = Some(form);
        settled_any = true;
    }
    if settled_any {
        let content = match form_for_result {
            Some(form) => {
                tool_result_content(&form, FormResolution::Dismissed, &BTreeMap::new(), true)
            }
            None => HOLD_TIMED_OUT_TOOL_RESULT.to_string(),
        };
        resume_user_form(state, account_id, coworker_id, agent_id, content).await;
    }
    settled_any
}

/// Stamp `boxResolution: timed_out` on a live handoff and resume. Idempotent.
pub async fn timeout_live_handoff(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
) -> bool {
    let Ok(entries) = state
        .agui
        .auth
        .store
        .gateway_transcript(coworker_id, account_id)
        .await
    else {
        return false;
    };
    let Some(entry) = entries.into_iter().find(is_live_handoff) else {
        return false;
    };
    let Some(entry_id) = entry.get("id").and_then(Value::as_str).map(str::to_string) else {
        return false;
    };
    let args = json!({
        "entryId": entry_id,
        "agentId": agent_id,
        "resolution": "timed_out",
    });
    let (code, _) = resolve_box_handoff(state, &args, account_id).await;
    code == 200
}

async fn start_box_handoff(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
    form: &FormRequest,
) -> Option<Value> {
    let card = super::cards::computer_handoff_card(
        &format!("e_{}", uuid::Uuid::now_v7()),
        &format!("req_{}", uuid::Uuid::now_v7().simple()),
        &handoff_instruction(form),
        now_ms(),
    );
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(coworker_id, account_id, &card, now_ms())
        .await
    {
        tracing::error!(%error, "could not append the box handoff entry");
        return None;
    }
    live::emit_transcript(state, agent_id, account_id, "appended", card.clone());
    spawn_handoff_hold_timeout(
        state.clone(),
        account_id.clone(),
        coworker_id.clone(),
        agent_id.to_string(),
    );
    Some(card)
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

/// Null stays the disclosure answer for a coworker the caller may not use. A stamped
/// `entryId` that is missing from the transcript is an error — NativeChat collapses on Null.
async fn load_owned_entry(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    entry_id: &str,
) -> Result<(i64, Value), (u16, Value)> {
    if !may_use(state, account_id, coworker_id).await {
        return Err((200, Value::Null));
    }
    match state
        .agui
        .auth
        .store
        .find_gateway_entry(coworker_id, account_id, entry_id)
        .await
    {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err((404, json!({ "error": "form entry missing" }))),
        Err(error) => {
            tracing::error!(%error, "could not load a form or handoff entry");
            Err((500, json!({ "error": "transcript unavailable" })))
        }
    }
}

fn settle_entry(
    mut entry: Value,
    resolution: FormResolution,
    shared: &BTreeMap<String, String>,
    widget_dismissed: bool,
    outcomes: &[FieldOutcome],
    timed_out: bool,
) -> Value {
    if let Some(map) = entry.as_object_mut() {
        map.insert("formResolution".to_string(), json!(resolution.as_str()));
        map.remove("values");
        // Never a boxRequestId on the user-form: that converts the card into a handoff.
        map.remove("boxRequestId");
        if widget_dismissed {
            map.insert("widgetDismissed".to_string(), json!(true));
        }
        if timed_out {
            map.insert("timedOut".to_string(), json!(true));
        }
        if shared.is_empty() {
            map.remove("sharedValues");
        } else {
            map.insert("sharedValues".to_string(), json!(shared));
        }
        if outcomes.is_empty() {
            map.remove("formFieldOutcomes");
        } else {
            map.insert("formFieldOutcomes".to_string(), json!(outcomes));
        }
    }
    entry
}

fn settle_handoff(mut entry: Value, resolution: &str, timed_out: bool) -> Value {
    if let Some(map) = entry.as_object_mut() {
        map.insert("boxResolution".to_string(), json!(resolution));
        if timed_out {
            map.insert("timedOut".to_string(), json!(true));
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
) -> Vec<FieldOutcome> {
    let failed = || {
        form.fields
            .iter()
            .map(|field| FieldOutcome {
                id: field.id.clone(),
                filled: false,
                fill_failed: true,
            })
            .collect()
    };
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
        return failed();
    };
    let Some((computer, box_id)) = runner.fill_target() else {
        return failed();
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
    // Escalated means the person is on the computer. Resume is hand-back / decline / timeout,
    // never this retry — otherwise hold clears and the model types into their session.
    if resolution == FormResolution::Escalated {
        return (200, entry.clone());
    }
    let timed_out = entry.get("timedOut").and_then(Value::as_bool) == Some(true);
    let content = tool_result_content(&form, resolution, &shared, timed_out);
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
