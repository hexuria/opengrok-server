//! The HTTP surface for schedules and monitors.
//!
//! Ownership answers 404 for both "no such" and "not yours", exactly as runs do: a wrong guess
//! and a real id belonging to somebody else must be indistinguishable, or the id space is
//! enumerable.
//!
//! A ROUTINE WAKES ON A CLOCK OR ON A HOOK. `POST /schedules` with `"kind": "webhook"` mints the
//! hook id and the bearer (`hooks.rs`, beside the door the outside world then POSTs to) and hands
//! both back; without a `kind` it is a cron routine, which is what every body written against this
//! route before webhooks says. `cron` answers `null` on a webhook: it has no clock, and the sweep
//! never claims it.
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

use opengrok_core::id::{AccountId, CoworkerId, MonitorId, ScheduleId};
use opengrok_core::monitor::{Monitor, MonitorCommand};
use opengrok_core::schedule::{Schedule, ScheduleCommand, Wake, WakeKind};

use crate::agui::routes::{AgUiState, account_from_bearer};
use crate::host_state::HostState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: HostState) -> Router {
    let agui = state.agui.clone();
    Router::new()
        .merge(schedules_router(state))
        .merge(monitors_router(agui))
}

/// THE SCHEDULES HALF CARRIES `HostState`, the monitors half does not. Minting a webhook wake has
/// to say which address to POST to, and that address is `HostState.public_gateway_url` — so this
/// router is mounted with the host state the way `agui::run_router` is, and the monitors below
/// stay on `AgUiState` because nothing about a monitor is addressable from outside.
fn schedules_router(state: HostState) -> Router {
    Router::new()
        .route("/schedules", post(create_schedule).get(list_schedules))
        .route("/schedules/{id}/pause", post(pause_schedule))
        .route("/schedules/{id}/resume", post(resume_schedule))
        .route("/schedules/{id}/rotate-key", post(rotate_schedule_key))
        .route("/schedules/{id}", axum::routing::delete(delete_schedule))
        .with_state(state)
}

fn monitors_router(state: AgUiState) -> Router {
    Router::new()
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
    prompt: String,
    /// What the person called it. Absent ⇒ the prompt's first words, which is what this API
    /// showed before it had a name at all.
    name: Option<String>,
    /// `cron` or `webhook`. ABSENT MEANS CRON, because every body ever written against this
    /// route omits it — a default that changed would turn old callers' routines into hooks
    /// nobody POSTs to. An unrecognised value is refused rather than read as the default: a
    /// typo must not quietly install the wrong kind of wake.
    kind: Option<String>,
    /// The expression, required for a cron wake. Ignored for a webhook, which has no clock.
    cron: Option<String>,
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

/// Every reply on this door that carries a webhook key. A bearer must not sit in a proxy, a
/// browser cache or a `curl` on somebody's disk — the same rule the OAuth door applies to the
/// tokens it mints (`auth/oauth_mcp.rs`). It is set on the cron replies too: a header that is
/// sometimes absent is a header a reader has to think about, and "which routines exist" is not
/// cacheable either.
const NO_STORE: (axum::http::HeaderName, &str) = (axum::http::header::CACHE_CONTROL, "no-store");

/// One routine as the wire carries it — built from the aggregate on create and from the
/// projection on list, which is why it is a shape of its own rather than a method on either.
struct RoutineRow<'a> {
    id: &'a str,
    coworker_id: Option<&'a str>,
    /// What the person called it, or the prompt's first words when they did not.
    name: &'a str,
    prompt: &'a str,
    kind: WakeKind,
    /// Empty on a webhook wake, which answers `null` rather than `""`.
    cron: &'a str,
    hook_id: &'a str,
    webhook_key: &'a str,
    active: bool,
    next_due_ms: Option<i64>,
    /// `autonomy::last_run`: what the newest run came to. `None` on create, and for a routine
    /// that has never run — `null` on the wire, never an empty object a client would read as a
    /// run with no status.
    last_run: Option<serde_json::Value>,
}

impl RoutineRow<'_> {
    /// THE KEY IS SHOWN ON LIST, NOT ONLY ON CREATE. The aggregate keeps the bearer in plaintext
    /// (`Schedule::webhook_key`) for exactly this: a POST URL is useless without its key, and a
    /// person who closed the create response would otherwise have to rotate to see it again —
    /// which breaks whatever they had already wired the old key into. What an inbound POST is
    /// checked against is `secret_hash`, never this; the plaintext is the owner's own copy, behind
    /// the same bearer as the rest of their routines.
    fn json(&self, state: &HostState) -> serde_json::Value {
        let mut row = serde_json::json!({
            "id": self.id,
            "coworkerId": self.coworker_id,
            "name": self.name,
            "cron": match self.kind {
                WakeKind::Cron => serde_json::json!(self.cron),
                WakeKind::Webhook => serde_json::Value::Null,
            },
            "prompt": self.prompt,
            "kind": self.kind.as_str(),
            "active": self.active,
            "nextDueMs": self.next_due_ms,
            "lastRun": self.last_run,
        });
        if self.kind == WakeKind::Webhook {
            row["webhook"] =
                crate::hooks::webhook_trigger_json(state, self.hook_id, self.webhook_key);
        }
        row
    }
}

/// The wake the body asks for, with both halves of a webhook minted HERE rather than taken from
/// the caller: a hook id somebody else may pick is a namespace they can collide with, and a key
/// somebody else may pick is a password they chose for us.
fn wake_from(body: &mut CreateSchedule) -> Result<Wake, (StatusCode, String)> {
    match body.kind.as_deref().unwrap_or("cron") {
        "cron" => {
            let cron = body.cron.take().unwrap_or_default();
            if cron.trim().is_empty() {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "a cron routine needs a cron expression".to_string(),
                ));
            }
            Ok(Wake::Cron { cron })
        }
        "webhook" => {
            let key = crate::hooks::mint_webhook_key();
            Ok(Wake::Webhook {
                hook_id: crate::hooks::mint_hook_id(),
                secret_hash: crate::hooks::hash_webhook_key(&key),
                webhook_key: key,
            })
        }
        // NAMED, NOT ECHOED. Handing the caller's own bytes back is how a refusal becomes a
        // reflector: whatever they sent lands in our log line, in their console and in anything
        // that renders this message.
        _ => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "kind must be \"cron\" or \"webhook\"".to_string(),
        )),
    }
}

async fn create_schedule(
    State(state): State<HostState>,
    headers: axum::http::HeaderMap,
    Json(mut body): Json<CreateSchedule>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(std::mem::take(&mut body.coworker_id));
    if let Err(refusal) = may_use(&state.agui, &account_id, &coworker_id).await {
        return refusal;
    }
    let wake = match wake_from(&mut body) {
        Ok(wake) => wake,
        Err(refusal) => return refusal.into_response(),
    };

    let at_ms = now_ms();
    let events = match Schedule::default().decide(ScheduleCommand::Create {
        coworker_id,
        // Unnamed is the pre-pane shape of this API, and the pane shows the prompt's first words.
        name: body
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                body.prompt
                    .split_whitespace()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        prompt: body.prompt,
        wake,
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
        .agui
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
        [NO_STORE],
        Json(
            RoutineRow {
                id: id.as_str(),
                coworker_id: state_after.coworker_id.as_ref().map(|c| c.as_str()),
                name: &state_after.name,
                prompt: &state_after.prompt,
                kind: state_after.kind,
                cron: &state_after.cron,
                hook_id: &state_after.hook_id,
                webhook_key: &state_after.webhook_key,
                active: true,
                next_due_ms: opengrok_core::schedule::next_fire_ms(&state_after.cron, at_ms),
                last_run: None,
            }
            .json(&state),
        ),
    )
        .into_response()
}

async fn list_schedules(
    State(state): State<HostState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let schedules = match state.agui.auth.store.schedules_for(&account_id).await {
        Ok(schedules) => schedules,
        Err(error) => {
            tracing::error!(%error, "could not list schedules");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    let mut rows = Vec::with_capacity(schedules.len());
    for view in schedules {
        // The projection carries the key for every routine written since it was projected, so a
        // listing is one query. A webhook row from before that column existed carries an empty
        // key and is read from its stream instead — once, because the next write to it projects
        // the key like any other.
        let key = match view.kind {
            WakeKind::Cron => String::new(),
            WakeKind::Webhook if !view.webhook_key.is_empty() => view.webhook_key.clone(),
            WakeKind::Webhook => {
                match state
                    .agui
                    .auth
                    .store
                    .load_schedule(&ScheduleId::from_stored(view.id.clone()))
                    .await
                {
                    Ok((schedule, _)) => schedule.webhook_key,
                    // NOT AN EMPTY KEY. A storage failure answered with `"key": ""` would show
                    // the owner a hook they could not fire and no reason why — and they would
                    // rotate a perfectly good key to try to fix it.
                    Err(error) => {
                        tracing::error!(%error, routine = %view.id, "could not read a routine's key");
                        return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed")
                            .into_response();
                    }
                }
            }
        };
        // NOT `null` ON A FAILED READ. `null` means "never ran", and a routine that ran and
        // spent points must not be shown as one that never did.
        let last_run = match crate::autonomy::last_run(
            &state.agui,
            &account_id,
            &view.id,
            &view.name,
        )
        .await
        {
            Ok(last_run) => last_run,
            Err(error) => {
                tracing::error!(%error, routine = %view.id, "could not read a routine's last run");
                return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
            }
        };
        rows.push(
            RoutineRow {
                id: &view.id,
                coworker_id: Some(view.coworker_id.as_str()),
                name: &view.name,
                prompt: &view.prompt,
                kind: view.kind,
                cron: &view.cron,
                hook_id: &view.hook_id,
                webhook_key: &key,
                active: view.active,
                next_due_ms: view.next_due_ms,
                last_run,
            }
            .json(&state),
        );
    }
    ([NO_STORE], Json(rows)).into_response()
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
    state: HostState,
    headers: axum::http::HeaderMap,
    id: String,
    command: fn(i64) -> ScheduleCommand,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let (schedule, seq, account_id) = match owned_schedule(&state.agui, &headers, &id).await {
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
        .agui
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

/// Load the schedule, decide with `decide`, append at the loaded seq — and if another writer got
/// there first, re-read and try ONCE more before answering 409. Why: the desktop's Routines pane
/// autosaves an edit on blur at the same instant a person clicks "Test run", so two mutations on
/// one schedule a few milliseconds apart are the ordinary case, not a race to design away. The
/// loser used to answer 500 "storage failed" (seen live 2 Sep 2026); now it decides again against
/// the winner's state, which is what the person meant anyway.
///
/// `decide` sees the fresh aggregate and returns the events to append, or a refusal already
/// shaped for the wire. Returns the aggregate after the append and the seq it landed at.
///
/// Shared with the webhook door (`hooks.rs`) — its only caller today, and the reason this lives
/// beside `change_schedule` rather than inside it: a fired routine and an edit to the same
/// routine a few milliseconds apart is exactly the race this retry exists for.
pub(crate) async fn mutate_schedule<F>(
    state: &HostState,
    account_id: &AccountId,
    schedule_id: &ScheduleId,
    at_ms: i64,
    mut decide: F,
) -> Result<Schedule, (u16, serde_json::Value)>
where
    F: FnMut(
        &Schedule,
    ) -> Result<Vec<opengrok_core::schedule::ScheduleEvent>, (u16, serde_json::Value)>,
{
    for attempt in 0..2 {
        let Ok((loaded, seq)) = state.agui.auth.store.load_schedule(schedule_id).await else {
            return Err((404, serde_json::json!({ "error": "no such routine" })));
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
            .append_schedule(schedule_id, account_id, seq, &events, &after, at_ms)
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
                    serde_json::json!({ "error": "another change to this routine landed first; reload and retry" }),
                ));
            }
            Err(error) => {
                tracing::error!(%error, schedule = %schedule_id, "could not write a routine change");
                return Err((500, serde_json::json!({ "error": "storage failed" })));
            }
        }
    }
    Err((
        409,
        serde_json::json!({ "error": "another change to this routine landed first; reload and retry" }),
    ))
}

async fn pause_schedule(
    State(state): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Pause { at_ms }).await
}

async fn resume_schedule(
    State(state): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Resume {
        at_ms,
    })
    .await
}

async fn delete_schedule(
    State(state): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Delete {
        at_ms,
    })
    .await
}

/// A new bearer for a webhook routine. The hook id — and so the POST URL — is unchanged; only
/// the key moves, which is what makes this a rotation rather than a new routine: whoever holds
/// the old key stops working at the moment the new one starts, and nothing else has to be
/// reconfigured.
///
/// THE OLD KEY IS NOT SHOWN A GRACE PERIOD. A key is rotated because it leaked or because
/// somebody left; a window in which both work is a window in which the reason for rotating still
/// holds. `SecretRotated` replaces the hash, and the very next POST with the old key is a 401.
///
/// Same auth and ownership as pause/resume: not-yours and no-such are both 404, and a cron
/// routine is a 409 — the aggregate refuses (`ScheduleError::NotWebhook`) and it is refused
/// there rather than here, so "there is no key to rotate" is one answer, not two.
async fn rotate_schedule_key(
    State(state): State<HostState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let (_, _, account_id) = match owned_schedule(&state.agui, &headers, &id).await {
        Ok(loaded) => loaded,
        Err(refusal) => return refusal,
    };
    let key = crate::hooks::mint_webhook_key();
    let secret_hash = crate::hooks::hash_webhook_key(&key);
    let at_ms = now_ms();
    // Through `mutate_schedule` rather than `change_schedule`: the answer is the new key, so the
    // aggregate AFTER the append is the thing being asked for — and the retry it does is worth
    // having here, where a rotation racing an edit is the ordinary case rather than a rare one.
    let after = match mutate_schedule(&state, &account_id, &id, at_ms, |loaded| {
        loaded
            .decide(ScheduleCommand::RotateWebhookSecret {
                secret_hash: secret_hash.clone(),
                webhook_key: key.clone(),
                at_ms,
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
        Err((code, body)) => {
            return (
                StatusCode::from_u16(code).unwrap_or(StatusCode::CONFLICT),
                Json(body),
            )
                .into_response();
        }
    };
    (
        [NO_STORE],
        Json(serde_json::json!({
            "id": id.as_str(),
            "name": after.name,
            "kind": after.kind.as_str(),
            "webhook": crate::hooks::webhook_trigger_json(
                &state,
                &after.hook_id,
                &after.webhook_key,
            ),
        })),
    )
        .into_response()
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
