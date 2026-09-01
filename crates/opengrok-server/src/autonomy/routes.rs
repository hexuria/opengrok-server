//! The HTTP surface for schedules and monitors.
//!
//! Ownership answers 404 for both "no such" and "not yours", exactly as runs do: a wrong guess
//! and a real id belonging to somebody else must be indistinguishable, or the id space is
//! enumerable.
//!
//! CREATION CHECKS POLICY TOO. The fire-time check is the one that matters (permission can be
//! revoked later), but accepting a schedule the account may not use today would store a standing
//! instruction that only ever logs refusals — a dead row a person has no way to see the problem
//! with. Refusing up front puts the reason in their hands instead.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use opengrok_core::id::{CoworkerId, MonitorId, ScheduleId};
use opengrok_core::monitor::{Monitor, MonitorCommand};
use opengrok_core::schedule::{Schedule, ScheduleCommand};

use crate::agui::routes::{AgUiState, account_from_bearer};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/schedules", post(create_schedule).get(list_schedules))
        .route("/schedules/{id}/pause", post(pause_schedule))
        .route("/schedules/{id}/resume", post(resume_schedule))
        .route("/schedules/{id}", axum::routing::delete(delete_schedule))
        .route("/monitors", post(create_monitor).get(list_monitors))
        .route("/monitors/{id}/pause", post(pause_monitor))
        .route("/monitors/{id}/resume", post(resume_monitor))
        .route("/monitors/{id}", axum::routing::delete(delete_monitor))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSchedule {
    coworker_id: String,
    cron: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateMonitor {
    coworker_id: String,
    /// The event type to watch, e.g. `run-failed`.
    watches: String,
    prompt: String,
}

/// May this account point this coworker at anything? Shared by both create endpoints.
async fn may_use(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
) -> Result<(), Response> {
    // The coworker must exist — a schedule for a typo'd id would only ever log refusals.
    let known = state
        .auth
        .store
        .load_coworker(coworker_id)
        .await
        .map(|(coworker, _)| coworker.hired)
        .unwrap_or(false);
    if !known {
        return Err((StatusCode::NOT_FOUND, "no such coworker").into_response());
    }
    let policy = state
        .auth
        .store
        .policy_for(account_id, coworker_id)
        .await
        .unwrap_or_default();
    let decision = opengrok_policy::decide(
        account_id,
        coworker_id,
        opengrok_policy::Action::UseCoworker,
        &policy,
    );
    if let Some(reason) = decision.reason() {
        return Err((StatusCode::FORBIDDEN, reason.to_string()).into_response());
    }
    Ok(())
}

async fn create_schedule(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateSchedule>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(body.coworker_id);
    if let Err(refusal) = may_use(&state, &account_id, &coworker_id).await {
        return refusal;
    }

    let at_ms = now_ms();
    let events = match Schedule::default().decide(ScheduleCommand::Create {
        coworker_id,
        cron: body.cron,
        // The pre-pane `/schedules` API has no name; the pane shows the prompt's first words.
        name: body
            .prompt
            .split_whitespace()
            .take(6)
            .collect::<Vec<_>>()
            .join(" "),
        prompt: body.prompt,
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => {
            return (StatusCode::UNPROCESSABLE_ENTITY, reason.to_string()).into_response();
        }
    };
    let state_after = Schedule::replay(&events);

    let id = ScheduleId::new();
    if let Err(error) = state
        .auth
        .store
        .append_schedule(&id, &account_id, 0, &events, &state_after, at_ms)
        .await
    {
        tracing::error!(%error, "could not store a schedule");
        return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id.as_str(),
            "coworkerId": state_after.coworker_id.as_ref().map(|c| c.as_str().to_string()),
            "cron": state_after.cron,
            "prompt": state_after.prompt,
            "active": true,
            "nextDueMs": opengrok_core::schedule::next_fire_ms(&state_after.cron, at_ms),
        })),
    )
        .into_response()
}

async fn list_schedules(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    match state.auth.store.schedules_for(&account_id).await {
        Ok(schedules) => {
            let rows: Vec<_> = schedules
                .into_iter()
                .map(|view| {
                    serde_json::json!({
                        "id": view.id,
                        "coworkerId": view.coworker_id.as_str(),
                        "cron": view.cron,
                        "prompt": view.prompt,
                        "active": view.active,
                        "nextDueMs": view.next_due_ms,
                    })
                })
                .collect();
            Json(rows).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "could not list schedules");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// Load a schedule the caller owns, or answer the 404 that hides whether it exists.
async fn owned_schedule(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
    id: &ScheduleId,
) -> Result<(Schedule, i64, opengrok_core::id::AccountId), Response> {
    let Some(account_id) = account_from_bearer(state, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    match state.auth.store.schedule_owner(id).await {
        Ok(Some(owner)) if owner == account_id => {}
        _ => return Err((StatusCode::NOT_FOUND, "no such schedule").into_response()),
    }
    match state.auth.store.load_schedule(id).await {
        Ok((schedule, seq)) => Ok((schedule, seq, account_id)),
        Err(_) => Err((StatusCode::NOT_FOUND, "no such schedule").into_response()),
    }
}

async fn change_schedule(
    state: AgUiState,
    headers: axum::http::HeaderMap,
    id: String,
    command: fn(i64) -> ScheduleCommand,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let (schedule, seq, account_id) = match owned_schedule(&state, &headers, &id).await {
        Ok(loaded) => loaded,
        Err(refusal) => return refusal,
    };
    let at_ms = now_ms();
    let events = match schedule.decide(command(at_ms)) {
        Ok(events) => events,
        Err(reason) => return (StatusCode::CONFLICT, reason.to_string()).into_response(),
    };
    let mut after = schedule;
    for event in &events {
        after.apply(event);
    }
    match state
        .auth
        .store
        .append_schedule(&id, &account_id, seq, &events, &after, at_ms)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not store a schedule change");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

async fn pause_schedule(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Pause { at_ms }).await
}

async fn resume_schedule(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Resume {
        at_ms,
    })
    .await
}

async fn delete_schedule(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Delete {
        at_ms,
    })
    .await
}

async fn create_monitor(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateMonitor>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(body.coworker_id);
    if let Err(refusal) = may_use(&state, &account_id, &coworker_id).await {
        return refusal;
    }

    let at_ms = now_ms();
    let events = match Monitor::default().decide(MonitorCommand::Create {
        coworker_id,
        watches: body.watches,
        prompt: body.prompt,
        at_ms,
    }) {
        Ok(events) => events,
        Err(reason) => {
            return (StatusCode::UNPROCESSABLE_ENTITY, reason.to_string()).into_response();
        }
    };
    let state_after = Monitor::replay(&events);

    let id = MonitorId::new();
    if let Err(error) = state
        .auth
        .store
        .append_monitor(&id, &account_id, 0, &events, &state_after, at_ms)
        .await
    {
        tracing::error!(%error, "could not store a monitor");
        return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id.as_str(),
            "coworkerId": state_after.coworker_id.as_ref().map(|c| c.as_str().to_string()),
            "watches": state_after.watches,
            "prompt": state_after.prompt,
            "active": true,
        })),
    )
        .into_response()
}

async fn list_monitors(State(state): State<AgUiState>, headers: axum::http::HeaderMap) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    match state.auth.store.monitors_for(&account_id).await {
        Ok(monitors) => {
            let rows: Vec<_> = monitors
                .into_iter()
                .map(|view| {
                    serde_json::json!({
                        "id": view.id,
                        "coworkerId": view.coworker_id.as_str(),
                        "watches": view.watches,
                        "prompt": view.prompt,
                        "active": view.active,
                    })
                })
                .collect();
            Json(rows).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "could not list monitors");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

async fn change_monitor(
    state: AgUiState,
    headers: axum::http::HeaderMap,
    id: String,
    command: fn(i64) -> MonitorCommand,
) -> Response {
    let id = MonitorId::from_stored(id);
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    match state.auth.store.monitor_owner(&id).await {
        Ok(Some(owner)) if owner == account_id => {}
        _ => return (StatusCode::NOT_FOUND, "no such monitor").into_response(),
    }
    let (monitor, seq) = match state.auth.store.load_monitor(&id).await {
        Ok(loaded) => loaded,
        Err(_) => return (StatusCode::NOT_FOUND, "no such monitor").into_response(),
    };
    let at_ms = now_ms();
    let events = match monitor.decide(command(at_ms)) {
        Ok(events) => events,
        Err(reason) => return (StatusCode::CONFLICT, reason.to_string()).into_response(),
    };
    let mut after = monitor;
    for event in &events {
        after.apply(event);
    }
    match state
        .auth
        .store
        .append_monitor(&id, &account_id, seq, &events, &after, at_ms)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not store a monitor change");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

async fn pause_monitor(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_monitor(state, headers, id, |at_ms| MonitorCommand::Pause { at_ms }).await
}

async fn resume_monitor(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_monitor(state, headers, id, |at_ms| MonitorCommand::Resume { at_ms }).await
}

async fn delete_monitor(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_monitor(state, headers, id, |at_ms| MonitorCommand::Delete { at_ms }).await
}
