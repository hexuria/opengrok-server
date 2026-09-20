//! `submitUserForm` / `dismissUserForm` / box-handoff resolve — gateway verbs and AG-UI REST twins.
//!
//! WHY THIS IS NOT A SECRET DROP. The desktop's `submitSecret` (deleted with seam A) stamped
//! `secretProvided` and DROPPED the value into the connector vault. This path types into the live
//! page. Mixing the two would either vault a Google password or fill a connector secret into
//! Chromium — the reason they were never one path, and the reason this one did not follow that
//! verb out.
//!
//! WHY ESCALATE IS NOT A RESUME. Official `dismissUserForm` mode `escalated` is Grok Bot
//! "Open the screen": emit a **separate** `sand://box` attachment with `boxRequestId` and keep
//! screen-hold until hand-back or decline. Resuming immediately (and clearing hold) races the
//! person on the computer — the Facebook hang after password when phone verify hits. This is
//! not OpenGrok Take over / I'm done / Skip, and it is not `handBackForeverBox` (lifecycle stop).
//!
//! SKIP MAY POST THE FORM ID. NativeChat KeepAlive prefers `handoffEntryId` from the escalate
//! response, then falls back to the form gateway `entryId`. That form never carries
//! `boxRequestId` (a stray one converts the card into a handoff). Resolve must therefore
//! find and settle live `sand://box` siblings when the posted id is the escalated form,
//! or chrome stays "Waiting for you" on a still-suspended UserForm run. Same for
//! `dismissUserForm` mode `dismissed` on an already-escalated form: that is abandon, not
//! an escalate retry.
//!
//! NativeChat talks AG-UI with an account bearer, not the gateway host bearer, so the REST
//! twins live on a router that has `GatewayState` (live emit + resume) but authenticates
//! like AG-UI (`account_from_bearer`), never through `refuse()`.

use std::collections::{BTreeMap, HashSet};
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
use opengrok_wire::agui::{Event, EventType};
use serde_json::{Value, json};

use super::{GatewayState, conversation};

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
    journal_settled_form(state, account_id, &coworker_id, &settled).await;
    if resolution == FormResolution::Submitted {
        super::credential::offer_save_after_submit(
            state,
            account_id,
            &coworker_id,
            &agent_id,
            &entry_id,
            &form,
            &shared,
        )
        .await;
    }

    resume_user_form(
        state,
        account_id,
        &coworker_id,
        &agent_id,
        content,
        call_id_of(&settled),
    )
    .await;
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
        // Skip after Open the screen can land as dismissed on the *form* id while a
        // live sand://box sibling still holds the screen. heal_or_already would
        // no-op because the form is already escalated.
        if resolution == FormResolution::Dismissed && is_escalated_form(&entry) {
            return abandon_escalated_form(state, account_id, &coworker_id, &agent_id, entry).await;
        }
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
    journal_settled_form(state, account_id, &coworker_id, &settled).await;

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
    resume_user_form(
        state,
        account_id,
        &coworker_id,
        &agent_id,
        content,
        call_id_of(&settled),
    )
    .await;
    (200, settled)
}

/// `resolveBoxHandoff {entryId, agentId, resolution: handed_back|declined|timed_out}`.
/// Stamps `boxResolution` on every live `sand://box` sibling and resumes the waiting
/// user-form run. Accepts the handoff entry id **or** the escalated form entry id
/// (NativeChat KeepAlive falls back to the form when `handoffEntryId` is missing).
/// Does **not** stop the box (`handBackForeverBox` is a lifecycle verb).
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
    let (_seq, entry) = match load_owned_entry(state, account_id, &coworker_id, &entry_id).await {
        Ok(row) => row,
        Err(reply) => return reply,
    };
    let posted_live = is_live_handoff(&entry);
    let posted_handoff = is_handoff_entry(&entry);
    let posted_escalated_form = is_escalated_form(&entry);
    if !posted_live && !posted_handoff && !posted_escalated_form {
        return (
            400,
            json!({ "error": "that entry is not a live box handoff" }),
        );
    }

    let settled_siblings =
        settle_live_handoffs(state, account_id, &coworker_id, word, timed_out).await;
    // Name the call this resume answers. Passing `None` lets it land on whichever
    // call happens to be parked, which for stacked forms is the sibling's -- the
    // twin then gets this form's tool result.
    resume_user_form(
        state,
        account_id,
        &coworker_id,
        &agent_id,
        content,
        call_id_of(&entry),
    )
    .await;

    if posted_live {
        if let Some(card) = settled_siblings
            .into_iter()
            .find(|card| card.get("id") == entry.get("id"))
        {
            return (200, card);
        }
        return (200, json!({ "alreadyAnswered": true }));
    }
    if posted_escalated_form {
        if let Some(card) = settled_siblings.into_iter().next() {
            return (200, card);
        }
        return (200, json!({ "alreadyAnswered": true }));
    }
    (200, json!({ "alreadyAnswered": true }))
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
        journal_settled_form(state, account_id, coworker_id, &settled).await;
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
        // `None` on purpose: every unresolved form just timed out, so the parked
        // call is one of them, and the run should resume with a timed-out result.
        // Naming the last-settled entry here would make `resume_settled` refuse
        // whenever that entry is not the pending one, and the run would stay
        // parked with nothing left to wake it.
        resume_user_form(state, account_id, coworker_id, agent_id, content, None).await;
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
    spawn_handoff_hold_timeout(
        state.clone(),
        account_id.clone(),
        coworker_id.clone(),
        agent_id.to_string(),
    );
    Some(card)
}

/// A new user message interrupted the parked HITL run. Settle leftover form / live
/// handoff chrome without resuming — the new text is steer, not a card answer.
/// Escalate itself never calls this.
pub(crate) async fn dismiss_unresolved_on_interrupt(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) {
    settle_live_handoffs(state, account_id, coworker_id, "declined", false).await;
    let Ok(entries) = state
        .agui
        .auth
        .store
        .gateway_transcript(coworker_id, account_id)
        .await
    else {
        return;
    };
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
        let settled = settle_entry(
            current,
            FormResolution::Dismissed,
            &BTreeMap::new(),
            true,
            &[],
            false,
        );
        if let Err(error) = state
            .agui
            .auth
            .store
            .update_gateway_entry(coworker_id, account_id, seq, &settled)
            .await
        {
            tracing::error!(%error, "could not dismiss a form on interrupt");
            continue;
        }
        journal_settled_form(state, account_id, coworker_id, &settled).await;
    }
}

fn call_id_of(entry: &Value) -> Option<&str> {
    entry
        .get("callId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
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

fn is_handoff_entry(entry: &Value) -> bool {
    entry
        .get("boxRequestId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

fn is_escalated_form(entry: &Value) -> bool {
    is_user_form_entry(entry)
        && entry.get("formResolution").and_then(Value::as_str) == Some("escalated")
}

/// Stamp `boxResolution` on every still-live sand://box sibling. One Skip must not
/// leave another unanswered handoff holding "Waiting for you".
async fn settle_live_handoffs(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    resolution: &str,
    timed_out: bool,
) -> Vec<Value> {
    let Ok(entries) = state
        .agui
        .auth
        .store
        .gateway_transcript(coworker_id, account_id)
        .await
    else {
        return Vec::new();
    };
    let mut settled = Vec::new();
    for entry in entries {
        if !is_live_handoff(&entry) {
            continue;
        }
        let Some(entry_id) = entry.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let Ok(Some((seq, current))) = state
            .agui
            .auth
            .store
            .find_gateway_entry(coworker_id, account_id, &entry_id)
            .await
        else {
            continue;
        };
        if !is_live_handoff(&current) {
            continue;
        }
        let card = settle_handoff(current, resolution, timed_out);
        if let Err(error) = state
            .agui
            .auth
            .store
            .update_gateway_entry(coworker_id, account_id, seq, &card)
            .await
        {
            tracing::error!(%error, "could not settle a box handoff");
            continue;
        }
        settled.push(card);
    }
    settled
}

/// NativeChat Skip after Open the screen: form is already `escalated`, live handoff
/// still unanswered. Settle siblings as declined and resume. Escalate itself never
/// comes here (`mode: escalated` on a settled form still hits heal_or_already).
async fn abandon_escalated_form(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
    entry: Value,
) -> (u16, Value) {
    settle_live_handoffs(state, account_id, coworker_id, "declined", false).await;
    // Name the call this Skip answers; with `None` a stacked sibling's parked
    // call would take the declined result instead.
    resume_user_form(
        state,
        account_id,
        coworker_id,
        agent_id,
        HANDOFF_DECLINED_TOOL_RESULT.to_string(),
        call_id_of(&entry),
    )
    .await;
    (200, entry)
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
    if resume_user_form(
        state,
        account_id,
        coworker_id,
        agent_id,
        content,
        call_id_of(entry),
    )
    .await
    {
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
    call_id: Option<&str>,
) -> bool {
    resume_settled(
        state,
        account_id,
        coworker_id,
        agent_id,
        opengrok_core::run::SuspendReason::UserForm,
        content,
        call_id,
    )
    .await
}

pub(crate) async fn resume_settled(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
    reason: opengrok_core::run::SuspendReason,
    content: String,
    call_id: Option<&str>,
) -> bool {
    let Some((run_id, mut run, seq, pending)) =
        pending_suspended(state, account_id, coworker_id, reason).await
    else {
        return false;
    };
    if let Some(want) = call_id.filter(|id| !id.is_empty())
        && pending.call_id != want
    {
        // Another stacked same-completion form: settle the card, leave the parked call.
        return false;
    }
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
            tracing::warn!(%error, "could not answer the pending run");
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
        tracing::error!(%error, "could not append the answer");
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

pub(crate) async fn pending_suspended(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    reason: opengrok_core::run::SuspendReason,
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
        if pending.reason != reason {
            continue;
        }
        return Some((run_id, run, seq, pending));
    }
    None
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Journal a settled user-form onto the AG-UI run NativeChat replays.
///
/// Live HITL stays `CUSTOM name: run-awaiting-approval` / `reason: user-form`.
/// NativeChat's assembler already hydrates a later `CUSTOM name: user-form`
/// whose `value` is the gateway send-message envelope (`message.type:
/// user-form`, `formRequest`, sibling `formResolution`, no secrets). Without
/// this frame, `GET /ag-ui/threads/{id}` only has the mint-time CUSTOM and a
/// cold client cannot rebuild ✓ Submitted.
async fn journal_settled_form(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    settled: &Value,
) {
    journal_agui_custom(
        state,
        account_id,
        coworker_id,
        opengrok_core::run::SuspendReason::UserForm,
        agui_user_form_frame(settled),
    )
    .await;
}

/// Append a CUSTOM onto a still-pending run (NativeChat replays this). Scrubs
/// accidental password keys before the payload is stored.
pub(crate) async fn journal_agui_custom(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    reason: opengrok_core::run::SuspendReason,
    frame: Value,
) {
    let Some((run_id, mut run, seq, pending)) =
        pending_suspended(state, account_id, coworker_id, reason).await
    else {
        return;
    };
    let at_ms = now_ms();
    let scrubbed = opengrok_tools::credential::scrub_secret_keys(&frame);
    let mut frame = scrubbed;
    if let Some(map) = frame.as_object_mut() {
        map.insert("threadId".to_string(), json!(run.thread_id.clone()));
        map.insert("runId".to_string(), json!(run_id.as_str()));
        let has_call = map
            .get("callId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
        if !has_call {
            map.insert("callId".to_string(), json!(pending.call_id.clone()));
        }
        map.insert("timestamp".to_string(), json!(at_ms));
    }
    let Ok(events) = run.decide(RunCommand::Emit {
        payload: frame,
        at_ms,
    }) else {
        return;
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
        tracing::error!(%error, "could not journal a custom frame onto a pending run");
    }
}

/// CUSTOM NativeChat already hydrates for settled / cold-load cards. Not live HITL.
pub(crate) fn agui_user_form_frame(entry: &Value) -> Value {
    let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    let message = entry
        .get("message")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "user-form" }));
    let form_request = message.get("formRequest").cloned().unwrap_or(Value::Null);
    let resolution = entry.get("formResolution").cloned().unwrap_or(Value::Null);
    let call_id = entry.get("callId").cloned().unwrap_or(Value::Null);
    json!({
        "type": "CUSTOM",
        "name": "user-form",
        "entryId": entry_id,
        "callId": call_id,
        "formRequest": form_request,
        "formResolution": resolution,
        "message": message,
        "value": entry,
    })
}

fn overlay_form(event: &mut Value, form: &Value) {
    let Some(map) = event.as_object_mut() else {
        return;
    };
    if let Some(id) = form.get("id") {
        map.insert("entryId".to_string(), id.clone());
    }
    if map
        .get("callId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(call_id) = form.get("callId")
    {
        map.insert("callId".to_string(), call_id.clone());
    }
    if let Some(message) = form.get("message") {
        map.insert("message".to_string(), message.clone());
        if let Some(request) = message.get("formRequest") {
            map.insert("formRequest".to_string(), request.clone());
        }
    }
    if let Some(resolution) = form.get("formResolution") {
        map.insert("formResolution".to_string(), resolution.clone());
    }
    map.insert("value".to_string(), form.clone());
}

fn user_form_event_id(event: &Value) -> Option<&str> {
    event
        .get("entryId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| event.pointer("/value/id").and_then(Value::as_str))
        .or_else(|| event.pointer("/value/entryId").and_then(Value::as_str))
}

fn is_user_form_agui(event: &Value) -> bool {
    if event.get("type").and_then(Value::as_str) != Some("CUSTOM") {
        return false;
    }
    let name = event.get("name").and_then(Value::as_str).unwrap_or("");
    if name == "user-form" {
        return true;
    }
    if name == "run-awaiting-approval"
        && event.get("reason").and_then(Value::as_str) == Some("user-form")
    {
        return true;
    }
    event
        .get("message")
        .and_then(|message| message.get("type"))
        .and_then(Value::as_str)
        == Some("user-form")
}

fn form_fingerprint(request: &Value) -> String {
    let title = request.get("title").and_then(Value::as_str).unwrap_or("");
    let ids = request
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| field.get("id").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    format!("{title}|{ids}")
}

fn event_fingerprint(event: &Value) -> Option<String> {
    let request = event
        .get("formRequest")
        .or_else(|| event.get("arguments"))
        .or_else(|| event.pointer("/message/formRequest"))
        .or_else(|| event.pointer("/value/message/formRequest"))?;
    Some(form_fingerprint(request))
}

fn entry_fingerprint(entry: &Value) -> Option<String> {
    entry.pointer("/message/formRequest").map(form_fingerprint)
}

/// Live HITL CUSTOM NativeChat keys Continue off — `run-awaiting-approval` / `user-form`.
pub(crate) fn is_live_user_form_custom(event: &Event) -> bool {
    event.event_type == EventType::Custom
        && event.extra.get("name").and_then(Value::as_str) == Some("run-awaiting-approval")
        && event.extra.get("reason").and_then(Value::as_str) == Some("user-form")
}

/// NativeChat paints a Website login card from live `TOOL_CALL` frames. It uses `entryId`
/// when present, otherwise the raw `toolCallId` (`call-…`). Those frames stream during the
/// model completion, *before* we mint the gateway card — so a second same-title form was
/// left as `call-*-1` and Continue could not `POST /ag-ui/user-form/submit`.
///
/// Hold every `request_user_form` TOOL_CALL until the matching CUSTOM is stamped with `e_*`,
/// then forward the call frames with that id.
#[derive(Default)]
pub(crate) struct UserFormSseHold {
    form_ids: HashSet<String>,
    held: Vec<Event>,
}

impl UserFormSseHold {
    fn tool_call_id(event: &Event) -> Option<&str> {
        event
            .extra
            .get("toolCallId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
    }

    /// Hold a user-form TOOL_CALL; pass every other frame through.
    pub(crate) fn push(&mut self, event: Event) -> Option<Event> {
        match event.event_type {
            EventType::ToolCallStart => {
                let name = event
                    .extra
                    .get("toolCallName")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if name == opengrok_tools::REQUEST_USER_FORM {
                    if let Some(id) = Self::tool_call_id(&event) {
                        self.form_ids.insert(id.to_string());
                    }
                    self.held.push(event);
                    return None;
                }
                Some(event)
            }
            EventType::ToolCallArgs | EventType::ToolCallEnd | EventType::ToolCallResult => {
                if Self::tool_call_id(&event).is_some_and(|id| self.form_ids.contains(id)) {
                    self.held.push(event);
                    return None;
                }
                Some(event)
            }
            _ => Some(event),
        }
    }

    /// Frames for this `toolCallId`, now carrying the gateway `entryId`.
    pub(crate) fn release_for(&mut self, call_id: &str, entry_id: Option<&str>) -> Vec<Event> {
        if call_id.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut rest = Vec::new();
        for mut event in std::mem::take(&mut self.held) {
            if Self::tool_call_id(&event) == Some(call_id) {
                if let Some(id) = entry_id.filter(|id| !id.is_empty()) {
                    event.extra.insert("entryId".to_string(), json!(id));
                }
                out.push(event);
            } else {
                rest.push(event);
            }
        }
        self.held = rest;
        self.form_ids.remove(call_id);
        // Every fragment of this call is in hand here, which the per-delta scrub in
        // the harness never has: assemble and scrub before anything reaches the wire.
        opengrok_harness::scrub_streamed_tool_args(out)
    }

    /// Stream is ending; leftover form TOOL_CALLs (refused, never awaiting) go out as-is.
    pub(crate) fn release_rest(&mut self) -> Vec<Event> {
        self.form_ids.clear();
        opengrok_harness::scrub_streamed_tool_args(std::mem::take(&mut self.held))
    }
}

/// Fold current gateway user-form state into AG-UI replay events so a cold
/// NativeChat rebuilds ✓ Submitted (and idle cards) from `GET /ag-ui/threads/{id}`
/// / `GET /ag-ui/runs/{id}` — not only from live unresolved CUSTOMs.
pub(crate) fn hydrate_agui_events(
    mut events: Vec<Value>,
    forms: &[Value],
    started_at_ms: i64,
    updated_at_ms: i64,
) -> Vec<Value> {
    let forms: Vec<&Value> = forms
        .iter()
        .filter(|entry| is_user_form_entry(entry))
        .collect();
    if forms.is_empty() {
        return events;
    }
    let mut used = HashSet::new();
    for event in &mut events {
        if !is_user_form_agui(event) {
            continue;
        }
        if let Some(id) = user_form_event_id(event).map(str::to_string)
            && let Some(form) = forms
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            overlay_form(event, form);
            used.insert(id);
            continue;
        }
        // Same-completion stacked Website login cards share a fingerprint. Join
        // on `callId` so replay does not stamp the last card onto the first TOOL_CALL.
        if let Some(call_id) = event
            .get("callId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            && let Some(form) = forms.iter().find(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                !used.contains(id) && entry.get("callId").and_then(Value::as_str) == Some(call_id)
            })
        {
            overlay_form(event, form);
            if let Some(id) = form.get("id").and_then(Value::as_str) {
                used.insert(id.to_string());
            }
            continue;
        }
        if let Some(fingerprint) = event_fingerprint(event)
            && let Some(form) = forms.iter().find(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                !used.contains(id)
                    && entry_fingerprint(entry).as_deref() == Some(fingerprint.as_str())
            })
        {
            overlay_form(event, form);
            if let Some(id) = form.get("id").and_then(Value::as_str) {
                used.insert(id.to_string());
            }
        }
    }
    const SLACK_MS: i64 = 5_000;
    for form in forms {
        let Some(id) = form.get("id").and_then(Value::as_str) else {
            continue;
        };
        if used.contains(id) {
            continue;
        }
        let at = form.get("timestampMs").and_then(Value::as_i64).unwrap_or(0);
        if at < started_at_ms.saturating_sub(SLACK_MS)
            || at > updated_at_ms.saturating_add(SLACK_MS)
        {
            continue;
        }
        events.push(agui_user_form_frame(form));
        used.insert(id.to_string());
    }
    stamp_tool_calls(&mut events);
    events
}

/// NativeChat paints TOOL_CALL frames as Website login cards. Live HITL stamps
/// `entryId` on the CUSTOM; replay overlays the same id onto matching TOOL_CALLs
/// so stacked cards stay Continue-able after a reconnect.
fn stamp_tool_calls(events: &mut [Value]) {
    let mut by_call: BTreeMap<String, String> = BTreeMap::new();
    for event in events.iter() {
        if !is_user_form_agui(event) {
            continue;
        }
        let Some(call_id) = event.get("callId").and_then(Value::as_str) else {
            continue;
        };
        if let Some(entry_id) = user_form_event_id(event) {
            by_call.insert(call_id.to_string(), entry_id.to_string());
        }
    }
    if by_call.is_empty() {
        return;
    }
    for event in events.iter_mut() {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "TOOL_CALL_START" && kind != "TOOL_CALL_ARGS" && kind != "TOOL_CALL_END" {
            continue;
        }
        let Some(call_id) = event.get("toolCallId").and_then(Value::as_str) else {
            continue;
        };
        let Some(entry_id) = by_call.get(call_id) else {
            continue;
        };
        if let Some(map) = event.as_object_mut() {
            map.insert("entryId".to_string(), json!(entry_id));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn email_form(id: &str, resolution: Option<&str>, at: i64) -> Value {
        let mut entry = json!({
            "kind": "send-message",
            "id": id,
            "timestampMs": at,
            "message": {
                "type": "user-form",
                "formRequest": {
                    "title": "Sign in",
                    "fields": [{"id": "email", "label": "Email", "type": "email", "required": true}]
                }
            }
        });
        if let Some(word) = resolution {
            entry["formResolution"] = json!(word);
        }
        entry
    }

    #[test]
    fn hydrate_overlays_form_resolution_onto_the_awaiting_custom() {
        let form = email_form("e_form", Some("submitted"), 50);
        let events = vec![json!({
            "type": "CUSTOM",
            "name": "run-awaiting-approval",
            "reason": "user-form",
            "callId": "c1",
            "entryId": "e_form",
            "arguments": {
                "title": "Sign in",
                "fields": [{"id": "email", "label": "Email", "type": "email"}]
            }
        })];
        let out = hydrate_agui_events(events, std::slice::from_ref(&form), 0, 100);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["formResolution"], "submitted");
        assert_eq!(out[0]["message"]["type"], "user-form");
        assert_eq!(out[0]["entryId"], "e_form");
        let dump = serde_json::to_string(&out).unwrap();
        assert!(!dump.contains("s3cret"), "{dump}");
    }

    #[test]
    fn hydrate_injects_a_settled_card_when_the_run_never_stamped_entry_id() {
        let form = email_form("e_form", Some("submitted"), 50);
        let events = vec![json!({
            "type": "CUSTOM",
            "name": "run-awaiting-approval",
            "reason": "user-form",
            "callId": "c1",
            "arguments": {
                "title": "Sign in",
                "fields": [{"id": "email", "label": "Email", "type": "email"}]
            }
        })];
        let out = hydrate_agui_events(events, std::slice::from_ref(&form), 0, 100);
        assert_eq!(out[0]["entryId"], "e_form");
        assert_eq!(out[0]["formResolution"], "submitted");
        assert_eq!(out[0]["message"]["type"], "user-form");
    }

    #[test]
    fn hydrate_skips_forms_outside_the_run_window() {
        let other = email_form("e_other", Some("dismissed"), 10_000);
        let events = vec![json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "hi"})];
        let out = hydrate_agui_events(events, std::slice::from_ref(&other), 0, 100);
        assert_eq!(out.len(), 1);
        assert!(out.iter().all(|event| event["name"] != "user-form"));
    }

    #[test]
    fn hydrate_stamps_entry_id_onto_matching_tool_calls() {
        let form = email_form("e_form", None, 50);
        let mut form = form;
        form["callId"] = json!("c1");
        let events = vec![
            json!({
                "type": "TOOL_CALL_START",
                "toolCallId": "c1",
                "toolCallName": "request_user_form"
            }),
            json!({
                "type": "CUSTOM",
                "name": "run-awaiting-approval",
                "reason": "user-form",
                "callId": "c1",
                "entryId": "e_form",
                "arguments": {
                    "title": "Sign in",
                    "fields": [{"id": "email", "label": "Email", "type": "email"}]
                }
            }),
        ];
        let out = hydrate_agui_events(events, std::slice::from_ref(&form), 0, 100);
        assert_eq!(out[0]["entryId"], "e_form");
        assert_eq!(out[1]["entryId"], "e_form");
    }

    #[test]
    fn hydrate_joins_stacked_same_fingerprint_forms_by_call_id() {
        let mut forms = Vec::new();
        for (id, call) in [("e_3", "c3"), ("e_1", "c1"), ("e_2", "c2")] {
            let mut form = email_form(id, None, 50);
            form["callId"] = json!(call);
            forms.push(form);
        }
        let events = vec![
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "request_user_form"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "request_user_form"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c3", "toolCallName": "request_user_form"}),
            json!({
                "type": "CUSTOM",
                "name": "run-awaiting-approval",
                "reason": "user-form",
                "callId": "c1",
                "arguments": {
                    "title": "Sign in",
                    "fields": [{"id": "email", "label": "Email", "type": "email"}]
                }
            }),
            json!({
                "type": "CUSTOM",
                "name": "run-awaiting-approval",
                "reason": "user-form",
                "callId": "c2",
                "arguments": {
                    "title": "Sign in",
                    "fields": [{"id": "email", "label": "Email", "type": "email"}]
                }
            }),
            json!({
                "type": "CUSTOM",
                "name": "run-awaiting-approval",
                "reason": "user-form",
                "callId": "c3",
                "arguments": {
                    "title": "Sign in",
                    "fields": [{"id": "email", "label": "Email", "type": "email"}]
                }
            }),
        ];
        let out = hydrate_agui_events(events, &forms, 0, 100);
        let by_call: Vec<(&str, &str)> = out
            .iter()
            .filter(|event| event["type"] == "TOOL_CALL_START")
            .map(|event| {
                (
                    event["toolCallId"].as_str().unwrap(),
                    event["entryId"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(by_call, vec![("c1", "e_1"), ("c2", "e_2"), ("c3", "e_3")]);
    }

    #[test]
    fn an_escalated_form_is_not_a_live_handoff() {
        let form = email_form("e_form", Some("escalated"), 50);
        assert!(is_escalated_form(&form));
        assert!(!is_handoff_entry(&form));
        assert!(!is_live_handoff(&form));
    }

    fn form_tool_start(call_id: &str) -> Event {
        Event::new(EventType::ToolCallStart, 1)
            .with("toolCallId", call_id)
            .with("toolCallName", opengrok_tools::REQUEST_USER_FORM)
    }

    fn form_tool_args(call_id: &str) -> Event {
        Event::new(EventType::ToolCallArgs, 2)
            .with("toolCallId", call_id)
            .with("delta", r#"{"title":"Website login"}"#)
    }

    /// Two same-title Website logins: each TOOL_CALL must leave with its own e_*, not the
    /// raw `call-*-1` NativeChat used when live frames streamed before the gateway stamp.
    #[test]
    fn live_sse_holds_stacked_form_tool_calls_until_each_has_a_gateway_entry_id() {
        let mut hold = UserFormSseHold::default();
        assert!(hold.push(form_tool_start("call-42628be6")).is_none());
        assert!(hold.push(form_tool_args("call-42628be6")).is_none());
        assert!(hold.push(form_tool_start("call-42628be6-1")).is_none());
        assert!(hold.push(form_tool_args("call-42628be6-1")).is_none());

        let first = hold.release_for("call-42628be6", Some("e_aaa"));
        assert_eq!(first.len(), 2, "{first:?}");
        assert!(
            first
                .iter()
                .all(|event| event.extra.get("entryId") == Some(&json!("e_aaa")))
        );

        let second = hold.release_for("call-42628be6-1", Some("e_bbb"));
        assert_eq!(second.len(), 2, "{second:?}");
        assert!(
            second
                .iter()
                .all(|event| event.extra.get("entryId") == Some(&json!("e_bbb")))
        );
        assert_ne!(
            first[0].extra.get("entryId"),
            second[0].extra.get("entryId")
        );
    }

    #[test]
    fn shell_tool_calls_pass_through_the_user_form_hold() {
        let mut hold = UserFormSseHold::default();
        let event = Event::new(EventType::ToolCallStart, 1)
            .with("toolCallId", "c1")
            .with("toolCallName", "shell");
        let passed = hold.push(event).unwrap();
        assert_eq!(
            passed.extra.get("toolCallName").and_then(Value::as_str),
            Some("shell")
        );
        assert!(hold.release_rest().is_empty());
    }
}
