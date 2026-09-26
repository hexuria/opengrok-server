//! The HTTP surface for monitors: create, list, edit, pause, resume, delete, run now, history.
//!
//! Ownership answers 404 for both "no such" and "not yours", as schedules and runs do.
//!
//! ON THE HOST STATE, like the schedules half. A monitor has no address of its own, but "run now"
//! spawns a run, and a run that stops on a form mints its card through the host state
//! (`autonomy::fire`).
//!
//! THE LOOP GUARD HOLDS FOR A PERSON'S RUN TOO. Every `Fired` — manual or matched — writes its
//! `monitor_firing` row in the same transaction (`PgStore::append_monitor`), before the run is
//! spawned, so the sweep never mistakes a run this monitor started for a trigger.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use opengrok_core::id::{AccountId, CoworkerId, MonitorId, RunId};
use opengrok_core::monitor::{Monitor, MonitorCommand, MonitorError};

use super::routes::{
    NO_STORE, RUNS_MAX, RunsQuery, history, json_refusal, may_use, takes_work, unprocessable,
};
use crate::agui::routes::{AgUiState, account_from_bearer};
use crate::host_state::HostState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(super) fn router(state: HostState) -> Router {
    Router::new()
        .route("/monitors", post(create_monitor).get(list_monitors))
        .route("/monitors/{id}/pause", post(pause_monitor))
        .route("/monitors/{id}/resume", post(resume_monitor))
        .route("/monitors/{id}/run", post(run_monitor_now))
        .route("/monitors/{id}/runs", get(monitor_runs))
        .route(
            "/monitors/{id}",
            axum::routing::patch(edit_monitor).delete(delete_monitor),
        )
        .with_state(state)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateMonitor {
    coworker_id: String,
    /// The event type to watch, e.g. `run-failed`.
    watches: String,
    prompt: String,
}

/// `PATCH /monitors/{id}`. Every field is optional and an absent one keeps what the monitor has.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditMonitor {
    watches: Option<String>,
    prompt: Option<String>,
    coworker_id: Option<String>,
}

/// One monitor as the wire carries it: the create, list and edit replies are the same shape.
fn monitor_json(
    id: &str,
    coworker_id: Option<&CoworkerId>,
    monitor: &Monitor,
    active: bool,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "coworkerId": coworker_id.map(CoworkerId::as_str),
        "watches": monitor.watches,
        "prompt": monitor.prompt,
        "active": active,
    })
}

/// Load a monitor the caller owns, or answer the 404 that hides whether it exists.
async fn owned_monitor(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
    id: &MonitorId,
) -> Result<(Monitor, AccountId), Response> {
    let Some(account_id) = account_from_bearer(state, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    match state.auth.store.monitor_owner(id).await {
        Ok(Some(owner)) if owner == account_id => {}
        _ => return Err((StatusCode::NOT_FOUND, "no such monitor").into_response()),
    }
    match state.auth.store.load_monitor(id).await {
        Ok((monitor, _)) => Ok((monitor, account_id)),
        Err(error) => {
            tracing::error!(%error, monitor = %id, "could not read a monitor");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response())
        }
    }
}

/// Load, decide, append at the loaded seq, and on a `Conflict` re-read and decide ONCE more —
/// the schedules' `mutate_schedule`, for monitors. Every write to a monitor comes through here:
/// an edit, "run now", pause, resume, delete and the sweep's firings can land a few milliseconds
/// apart, and the loser should decide again against the winner's state rather than fail.
pub(crate) async fn mutate_monitor<F>(
    state: &AgUiState,
    account_id: &AccountId,
    monitor_id: &MonitorId,
    at_ms: i64,
    mut decide: F,
) -> Result<Monitor, (u16, serde_json::Value)>
where
    F: FnMut(
        &Monitor,
    ) -> Result<Vec<opengrok_core::monitor::MonitorEvent>, (u16, serde_json::Value)>,
{
    let conflict = || {
        (
            409,
            serde_json::json!({ "error": "another change to this monitor landed first; reload and retry" }),
        )
    };
    for attempt in 0..2 {
        // A 500, not a 404: every caller checked the monitor exists, so a load that fails now is
        // the store, and "no such monitor" would tell its owner it was deleted.
        let (loaded, seq) = match state.auth.store.load_monitor(monitor_id).await {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::error!(%error, monitor = %monitor_id, "could not read a monitor");
                return Err((500, serde_json::json!({ "error": "storage failed" })));
            }
        };
        let events = decide(&loaded)?;
        let mut after = loaded;
        for event in &events {
            after.apply(event);
        }
        match state
            .auth
            .store
            .append_monitor(monitor_id, account_id, seq, &events, &after, at_ms)
            .await
        {
            Ok(_) => return Ok(after),
            Err(opengrok_store::StoreError::Conflict) if attempt == 0 => {
                tracing::info!(monitor = %monitor_id, "a monitor write lost a race; re-reading and retrying once");
                continue;
            }
            Err(opengrok_store::StoreError::Conflict) => return Err(conflict()),
            Err(error) => {
                tracing::error!(%error, monitor = %monitor_id, "could not write a monitor change");
                return Err((500, serde_json::json!({ "error": "storage failed" })));
            }
        }
    }
    Err(conflict())
}

async fn create_monitor(
    State(host): State<HostState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateMonitor>,
) -> Response {
    let state = &host.agui;
    let Some(account_id) = account_from_bearer(state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(body.coworker_id);
    if let Err(refusal) = may_use(state, &account_id, &coworker_id).await {
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
        Err(reason) => return unprocessable(&reason.to_string()),
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
        Json(monitor_json(
            id.as_str(),
            state_after.coworker_id.as_ref(),
            &state_after,
            true,
        )),
    )
        .into_response()
}

async fn list_monitors(State(host): State<HostState>, headers: axum::http::HeaderMap) -> Response {
    let state = &host.agui;
    let Some(account_id) = account_from_bearer(state, &headers) else {
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

/// Pause, resume and delete, through `mutate_monitor` so a conflict earns the one retry. A refused
/// change answers `{error}` JSON, as the monitor's decision refusals all do; a missing bearer or
/// an unknown monitor keeps the plain-text answer every schedule and run route gives.
async fn change_monitor(
    host: HostState,
    headers: axum::http::HeaderMap,
    id: String,
    command: fn(i64) -> MonitorCommand,
) -> Response {
    let id = MonitorId::from_stored(id);
    let account_id = match owned_monitor(&host.agui, &headers, &id).await {
        Ok((_, account_id)) => account_id,
        Err(refusal) => return refusal,
    };
    let at_ms = now_ms();
    match mutate_monitor(&host.agui, &account_id, &id, at_ms, |loaded| {
        loaded.decide(command(at_ms)).map_err(|reason| {
            (
                StatusCode::CONFLICT.as_u16(),
                serde_json::json!({ "error": reason.to_string() }),
            )
        })
    })
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => json_refusal(refusal),
    }
}

async fn pause_monitor(
    State(host): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_monitor(host, headers, id, |at_ms| MonitorCommand::Pause { at_ms }).await
}

async fn resume_monitor(
    State(host): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_monitor(host, headers, id, |at_ms| MonitorCommand::Resume { at_ms }).await
}

async fn delete_monitor(
    State(host): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_monitor(host, headers, id, |at_ms| MonitorCommand::Delete { at_ms }).await
}

/// Edit a monitor in place: same id, same thread, same firings. What it watches is checked the
/// way create checks it — watching `monitor-fired` is refused on an edit too.
async fn edit_monitor(
    State(host): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<EditMonitor>,
) -> Response {
    let state = &host.agui;
    let id = MonitorId::from_stored(id);
    let (loaded, account_id) = match owned_monitor(state, &headers, &id).await {
        Ok(loaded) => loaded,
        Err(refusal) => return refusal,
    };
    // An empty edit would still write an `Updated` and answer 200: a success that changed nothing
    // reads as a save that worked.
    if body.watches.is_none() && body.prompt.is_none() && body.coworker_id.is_none() {
        return unprocessable("nothing to change: send watches, prompt or coworkerId");
    }
    // Only a real handover is checked: a client echoing the whole row sends the coworker it has.
    let coworker_id = body
        .coworker_id
        .map(CoworkerId::from_stored)
        .filter(|coworker_id| loaded.coworker_id.as_ref() != Some(coworker_id));
    if let Some(coworker_id) = &coworker_id
        && let Err(refusal) = may_use(state, &account_id, coworker_id).await
    {
        return refusal;
    }
    let at_ms = now_ms();
    let after = match mutate_monitor(state, &account_id, &id, at_ms, |loaded| {
        loaded
            .decide(MonitorCommand::Update {
                watches: body
                    .watches
                    .clone()
                    .unwrap_or_else(|| loaded.watches.clone()),
                prompt: body.prompt.clone().unwrap_or_else(|| loaded.prompt.clone()),
                coworker_id: coworker_id.clone(),
                at_ms,
            })
            .map_err(|reason| {
                // What create refuses is the caller's input (422); anything else is the
                // monitor's state.
                let code = match reason {
                    MonitorError::NothingWatched
                    | MonitorError::EmptyPrompt
                    | MonitorError::WatchingItself
                    | MonitorError::NotWatchable(_) => StatusCode::UNPROCESSABLE_ENTITY,
                    _ => StatusCode::CONFLICT,
                };
                (
                    code.as_u16(),
                    serde_json::json!({ "error": reason.to_string() }),
                )
            })
    })
    .await
    {
        Ok(after) => after,
        Err(refusal) => return json_refusal(refusal),
    };
    (
        [NO_STORE],
        Json(monitor_json(
            id.as_str(),
            after.coworker_id.as_ref(),
            &after,
            !after.paused,
        )),
    )
        .into_response()
}

/// `POST /monitors/{id}/run` — the person's "Run now".
///
/// A PAUSED MONITOR RUNS, AND STAYS PAUSED: the pause keeps the log from waking it, and a person
/// asking is the one exception, as for schedules. THE CAP IS THE SWEEP'S OWN COUNT
/// (`monitor_runs_in_flight`), which sees firings not yet journaled and only this monitor's own —
/// a thread id somebody else chose cannot hold it at 429. It is a brake, not a lock: two presses
/// in the same instant can both read two and both fire. What it stops is the stampede.
async fn run_monitor_now(
    State(host): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let state = &host.agui;
    let id = MonitorId::from_stored(id);
    let (loaded, account_id) = match owned_monitor(state, &headers, &id).await {
        Ok(loaded) => loaded,
        Err(refusal) => return refusal,
    };
    let coworker_id = match loaded.coworker_id.as_ref() {
        Some(coworker_id) if takes_work(state, coworker_id).await => coworker_id.clone(),
        _ => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "this monitor's coworker is no longer hired; hand it to another coworker"
                })),
            )
                .into_response();
        }
    };
    if let Err(refusal) = may_use(state, &account_id, &coworker_id).await {
        return refusal;
    }
    match state
        .auth
        .store
        .monitor_runs_in_flight(&id, now_ms() - super::sweep::FIRING_PENDING_MS)
        .await
    {
        Ok(in_flight) if in_flight >= super::MAX_RUNS_IN_FLIGHT => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "this monitor already has three runs in flight; wait for one to end"
                })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, monitor = %id, "could not count a monitor's runs in flight");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "storage failed" })),
            )
                .into_response();
        }
    }
    let run_id = RunId::new();
    let after = match mutate_monitor(state, &account_id, &id, now_ms(), |loaded| {
        loaded
            .decide(MonitorCommand::Fire {
                run_id: run_id.clone(),
                matched_stream: String::new(),
                manual: true,
                at_ms: now_ms(),
            })
            .map_err(|reason| {
                (
                    StatusCode::CONFLICT.as_u16(),
                    serde_json::json!({ "error": reason.to_string() }),
                )
            })
    })
    .await
    {
        Ok(after) => after,
        Err(refusal) => return json_refusal(refusal),
    };
    // A monitor's prompt is written expecting the event that woke it; told nothing, the coworker
    // goes looking for one.
    let prompt = format!("{}\n\n[run by hand; no event woke it]", after.prompt);
    super::start_fired(
        &host,
        after.coworker_id.clone(),
        account_id,
        id.as_str(),
        run_id,
        prompt,
        format!("monitor {id} (run now)"),
    )
}

/// `GET /monitors/{id}/runs?limit=N` — what this monitor started, newest first, in the same
/// shape as a routine's history. `cause` is `event` (the log woke it) or `manual`.
async fn monitor_runs(
    State(host): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> Response {
    let state = &host.agui;
    let id = MonitorId::from_stored(id);
    let account_id = match owned_monitor(state, &headers, &id).await {
        Ok((_, account_id)) => account_id,
        Err(refusal) => return refusal,
    };
    let runs = match state
        .auth
        .store
        .runs_for_thread_owned_by(id.as_str(), &account_id, RUNS_MAX)
        .await
    {
        Ok(runs) => runs,
        Err(error) => {
            tracing::error!(%error, monitor = %id, "could not read a monitor's runs");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    // Read after the runs, so a run fired between the two reads is labelled, not dropped.
    let loaded = match state.auth.store.load_monitor(&id).await {
        Ok((loaded, _)) => loaded,
        Err(error) => {
            tracing::error!(%error, monitor = %id, "could not read a monitor");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    let rows = history(&runs, query.limit, |run| {
        if loaded.manual_runs.contains(run) {
            Some("manual")
        } else if loaded.event_runs.contains(run) {
            Some("event")
        } else {
            None
        }
    });
    ([NO_STORE], Json(rows)).into_response()
}
