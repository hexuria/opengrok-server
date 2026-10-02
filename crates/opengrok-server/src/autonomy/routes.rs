//! The HTTP surface for schedules (monitors are `monitors.rs`), and what the two share. A
//! routine's writes are `desk.rs`'s, which a Bot's routine tools use too (#316); these handlers
//! read the request, ask the desk, and answer its refusal as `{error}`.
//!
//! Ownership answers 404 for both "no such" and "not yours", exactly as runs do: a wrong guess
//! and a real id belonging to somebody else must be indistinguishable, or the id space is
//! enumerable.
//!
//! A ROUTINE WAKES ON A CLOCK OR ON A HOOK. `POST /schedules` with `"kind": "webhook"` mints the
//! hook id and the bearer (`hooks.rs`, beside the door the outside world then POSTs to) and hands
//! both back; without a `kind` it is a cron routine, which is what every body written against this
//! route before webhooks says. `cron` answers `null` on a webhook: it has no clock, and the sweep
//! never claims it. A cron is read in the routine's own `tz` (#316), which every row carries.
//!
//! CREATION CHECKS POLICY TOO. The fire-time check is the one that matters (permission can be
//! revoked later), but accepting a schedule the account may not use today would store a standing
//! instruction that only ever logs refusals — a dead row a person has no way to see the problem
//! with. Refusing up front puts the reason in their hands instead.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use opengrok_core::id::{AccountId, CoworkerId, RunId, ScheduleId};
use opengrok_core::schedule::{FireCause, Schedule, ScheduleCommand, ScheduleView, Skip, WakeKind};

use super::desk::{self, Draft, Refusal};
use crate::agui::routes::{AgUiState, account_from_bearer};
use crate::host_state::HostState;
use crate::now_ms;

pub(super) use super::desk::takes_work;

pub fn router(state: HostState) -> Router {
    Router::new()
        .merge(schedules_router(state.clone()))
        .merge(super::monitors::router(state))
}

/// ON THE HOST STATE. Minting a webhook wake has to say which address to POST to, and that
/// address is `HostState.public_gateway_url`; "run now" spawns a run, which mints a form's card
/// through it. Both halves are mounted with it the way `agui::run_router` is.
fn schedules_router(state: HostState) -> Router {
    Router::new()
        .route("/schedules", post(create_schedule).get(list_schedules))
        .route("/schedules/{id}/pause", post(pause_schedule))
        .route("/schedules/{id}/resume", post(resume_schedule))
        .route("/schedules/{id}/rotate-key", post(rotate_schedule_key))
        .route("/schedules/{id}/run", post(run_schedule_now))
        .route("/schedules/{id}/runs", get(schedule_runs))
        .route(
            "/schedules/{id}",
            axum::routing::patch(edit_schedule).delete(delete_schedule),
        )
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
    /// The IANA zone the cron is read in. Absent ⇒ the person's own (`PUT /account`), then UTC.
    tz: Option<String>,
    /// The routine's own limits on each run it starts, `{maxRounds, maxComputerRounds,
    /// maxWallMs}`, under its org's ceiling. Absent or `null` sets none.
    #[serde(default)]
    run_limits: Value,
}

/// May this account point this coworker at anything? `desk::may_use`, answered in the plain
/// text the monitors' routes have always answered it in.
pub(super) async fn may_use(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<(), Response> {
    let used = desk::may_use(state, account_id, coworker_id).await;
    used.map_err(IntoResponse::into_response)
}

/// Every reply on this door that carries a webhook key. A bearer must not sit in a proxy, a
/// browser cache or a `curl` on somebody's disk — the same rule the OAuth door applies to the
/// tokens it mints (`auth/oauth_mcp.rs`). It is set on the cron replies too: a header that is
/// sometimes absent is a header a reader has to think about, and "which routines exist" is not
/// cacheable either.
pub(super) const NO_STORE: (axum::http::HeaderName, &str) =
    (axum::http::header::CACHE_CONTROL, "no-store");

/// A desk's refusal, as this door answers every one: `{error}`.
fn refused((status, why): Refusal) -> Response {
    (status, Json(json!({ "error": why }))).into_response()
}

fn signed_out() -> Response {
    (StatusCode::UNAUTHORIZED, "sign in first").into_response()
}

/// One routine as every reply carries it — `GET /schedules`, and a create's or an edit's reply,
/// all from its projection row — with `key` the webhook's bearer.
///
/// THE KEY IS SHOWN ON LIST, NOT ONLY ON CREATE. The aggregate keeps the bearer in plaintext
/// (`Schedule::webhook_key`) for exactly this: a POST URL is useless without its key, and a
/// person who closed the create response would otherwise have to rotate to see it again — which
/// breaks whatever they had already wired the old key into. What an inbound POST is checked
/// against is `secret_hash`, never this; the plaintext is the owner's own copy, behind the same
/// bearer as the rest of their routines.
///
/// `lastRun` IS NOT `null` ON A FAILED READ: `null` means "never ran", and a routine that ran and
/// spent points must not be shown as one that never did.
async fn row(
    host: &HostState,
    account: &AccountId,
    view: &ScheduleView,
    key: &str,
) -> Result<Value, Response> {
    let last_run = match crate::autonomy::last_run(&host.agui, account, view).await {
        Ok(last_run) => last_run,
        Err(error) => {
            tracing::error!(%error, routine = %view.id, "could not read a routine's last run");
            return Err(refused((
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage failed".into(),
            )));
        }
    };
    let webhook = view.kind == WakeKind::Webhook;
    let mut row = json!({
        "id": view.id,
        "coworkerId": view.coworker_id.as_str(),
        "name": view.name,
        // Empty on a webhook wake, which answers `null` rather than `""`.
        "cron": (!webhook).then_some(&view.cron),
        "prompt": view.prompt,
        "kind": view.kind.as_str(),
        "active": view.active,
        "nextDueMs": view.next_due_ms,
        "runLimits": view.run_limits.to_json(),
        "lastRun": last_run,
        "tz": view.tz,
    });
    if webhook {
        row["webhook"] = crate::hooks::webhook_trigger_json(host, &view.hook_id, key);
    }
    Ok(row)
}

/// A routine's row, read back from its projection after a write, as the reply to it. `key` is
/// the aggregate's after an edit: a row projected before the key column existed carries an empty
/// one, and the reply must not show it.
async fn reply(
    host: &HostState,
    account: &AccountId,
    id: &ScheduleId,
    key: Option<&str>,
    status: StatusCode,
) -> Response {
    let view = match desk::view_of(&host.agui, account, id).await {
        Ok(view) => view,
        Err(refusal) => return refused(refusal),
    };
    match row(host, account, &view, key.unwrap_or(&view.webhook_key)).await {
        Ok(row) => (status, [NO_STORE], Json(row)).into_response(),
        Err(refusal) => refusal,
    }
}

async fn create_schedule(
    State(state): State<HostState>,
    headers: HeaderMap,
    Json(body): Json<CreateSchedule>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state.agui, &headers) else {
        return signed_out();
    };
    let draft = Draft {
        coworker: Some(CoworkerId::from_stored(body.coworker_id)),
        name: body.name,
        prompt: Some(body.prompt),
        kind: body.kind,
        cron: body.cron,
        tz: body.tz,
        run_limits: Some(body.run_limits),
    };
    match desk::create(&state.agui, &account_id, draft).await {
        Ok(id) => reply(&state, &account_id, &id, None, StatusCode::CREATED).await,
        Err(refusal) => refused(refusal),
    }
}

async fn list_schedules(State(state): State<HostState>, headers: HeaderMap) -> Response {
    let Some(account_id) = account_from_bearer(&state.agui, &headers) else {
        return signed_out();
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
        // the key like any other. NOT AN EMPTY KEY on a failed read: that would show the owner a
        // hook they could not fire and no reason why, and they would rotate a good key.
        let key = match view.kind {
            WakeKind::Webhook if view.webhook_key.is_empty() => {
                let id = ScheduleId::from_stored(view.id.clone());
                match state.agui.auth.store.load_schedule(&id).await {
                    Ok((schedule, _)) => schedule.webhook_key,
                    Err(error) => {
                        tracing::error!(%error, routine = %view.id, "could not read a routine's key");
                        return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed")
                            .into_response();
                    }
                }
            }
            _ => view.webhook_key.clone(),
        };
        match row(&state, &account_id, &view, &key).await {
            Ok(row) => rows.push(row),
            Err(refusal) => return refusal,
        }
    }
    ([NO_STORE], Json(rows)).into_response()
}

/// Load a schedule the caller owns, or answer the 404 that hides whether it exists.
async fn owned_schedule(
    state: &AgUiState,
    headers: &HeaderMap,
    id: &ScheduleId,
) -> Result<(Schedule, AccountId), Response> {
    let Some(account_id) = account_from_bearer(state, headers) else {
        return Err(signed_out());
    };
    let loaded = desk::owned(state, &account_id, id).await.map_err(refused)?;
    Ok((loaded, account_id))
}

/// Pause, resume and delete: the desk's, which a Bot's `delete_routine` asks too (#316). With
/// the pane's autosave, "Run now", the clock and a hook all writing one stream, a conflict here
/// is ordinary and earns `mutate_schedule`'s retry.
async fn change_schedule(
    state: HostState,
    headers: HeaderMap,
    id: String,
    command: fn(i64) -> ScheduleCommand,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let account_id = match owned_schedule(&state.agui, &headers, &id).await {
        Ok((_, account_id)) => account_id,
        Err(refusal) => return refusal,
    };
    match desk::change(&state.agui, (&account_id, &id), command).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => refused(refusal),
    }
}

/// `PATCH /schedules/{id}`. Every field is optional and an absent one keeps what the routine
/// has, so the pane can save the one field a person changed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditSchedule {
    name: Option<String>,
    prompt: Option<String>,
    cron: Option<String>,
    coworker_id: Option<String>,
    /// Accepted only when it names the kind the routine already is. A clock and a hook are two
    /// different promises to whatever is wired to them; switching one into the other would move
    /// the hook's address and key out from under it — or leave a clock nothing ever reads.
    kind: Option<String>,
    /// The zone its cron is read in from now on (#316).
    tz: Option<String>,
    /// The routine's limits, replaced whole: a limit the object leaves out is unset, and `{}`
    /// clears them all. Absent, or `null`, keeps what the routine has.
    run_limits: Option<Value>,
}

/// Edit a routine in place: same id, same thread, same history (`desk::edit`).
async fn edit_schedule(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<EditSchedule>,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let (loaded, account_id) = match owned_schedule(&state.agui, &headers, &id).await {
        Ok(loaded) => loaded,
        Err(refusal) => return refusal,
    };
    let draft = Draft {
        coworker: body.coworker_id.map(CoworkerId::from_stored),
        name: body.name,
        prompt: body.prompt,
        kind: body.kind,
        cron: body.cron,
        tz: body.tz,
        run_limits: body.run_limits,
    };
    match desk::edit(&state.agui, (&account_id, &id), &loaded, draft).await {
        Ok(after) => {
            reply(
                &state,
                &account_id,
                &id,
                Some(&after.webhook_key),
                StatusCode::OK,
            )
            .await
        }
        Err(refusal) => refused(refusal),
    }
}

pub(super) fn unprocessable(message: &str) -> Response {
    refused((StatusCode::UNPROCESSABLE_ENTITY, message.to_string()))
}

pub(super) fn json_refusal((code, body): (u16, Value)) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::CONFLICT),
        Json(body),
    )
        .into_response()
}

/// `POST /schedules/{id}/run` — the person's "Run now".
///
/// A PAUSED ROUTINE RUNS, AND STAYS PAUSED. That is the core's rule (`Schedule::decide`, `Fire`):
/// a person asking is the one wake a pause does not refuse, and pressing it is not a resume — the
/// clock and the hook stay off. POLICY IS ASKED FIRST, like create: `fire` refuses a revoked
/// grant silently, and a 202 with a run id that never appears is a lie the person cannot see.
/// A ROUTINE ON THE PERSON'S PLAN WITH NOBODY TO ANSWER IS SKIPPED (#316): recorded as their
/// press, no run starts, and the 409 says why in the row's words, with the skip's code.
async fn run_schedule_now(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let (loaded, account_id) = match owned_schedule(&state.agui, &headers, &id).await {
        Ok(loaded) => loaded,
        Err(refusal) => return refusal,
    };
    // A 409, not may_use's 404: on this route a 404 reads as "the routine is gone", and the
    // routine is right there — it is the coworker behind it that cannot work.
    let coworker_id = match loaded.coworker_id.as_ref() {
        Some(coworker_id) if takes_work(&state.agui, coworker_id).await => coworker_id.clone(),
        _ => {
            let why = "this routine's coworker is no longer hired; hand it to another coworker";
            return refused((StatusCode::CONFLICT, why.to_string()));
        }
    };
    if let Err(refusal) = desk::may_use(&state.agui, &account_id, &coworker_id).await {
        return refused(refusal);
    }
    if let Some(refusal) = crate::autonomy::too_busy(&state.agui, &account_id, id.as_str()).await {
        return refusal;
    }
    let run_id = RunId::new();
    let skip = crate::autonomy::unreachable(&state.agui, &account_id, &coworker_id, &run_id).await;
    let fired = desk::mutate_schedule(&state.agui, &account_id, &id, now_ms(), |loaded| {
        let firing = crate::autonomy::firing(skip, FireCause::Manual, &run_id);
        let refused =
            |why: opengrok_core::schedule::ScheduleError| (StatusCode::CONFLICT, why.to_string());
        loaded.decide(firing).map_err(refused)
    });
    let after = match fired.await {
        Ok(after) => after,
        Err(refusal) => return refused(refusal),
    };
    if let Some((code, why)) = skip {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": why, "code": code })),
        )
            .into_response();
    }
    let prompt = after.prompt.clone();
    crate::autonomy::start_fired(
        &state,
        after.coworker_id.clone(),
        account_id,
        id.as_str(),
        run_id,
        prompt,
        format!("schedule {id} (run now)"),
        after.run_limits,
    )
}

#[derive(Debug, Deserialize)]
pub(super) struct RunsQuery {
    pub(super) limit: Option<i64>,
}

/// How many runs the history reads before keeping the routine's own. The thread takes a
/// person's replies too, so the page is filled from this many and then cut to `limit`.
pub(super) const RUNS_MAX: i64 = 100;

/// `GET /schedules/{id}/runs?limit=N` — what this routine started, and every firing it skipped,
/// newest first. Always an array: a routine that never ran is `[]`, never an object a client
/// would have to special-case. A run counts as the routine's only if its log recorded firing it;
/// a person replying in the routine's thread is no firing, and the clock must not be credited.
async fn schedule_runs(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let account_id = match owned_schedule(&state.agui, &headers, &id).await {
        Ok((_, account_id)) => account_id,
        Err(refusal) => return refusal,
    };
    let store = &state.agui.auth.store;
    let runs = match store
        .runs_for_thread_owned_by(id.as_str(), &account_id, RUNS_MAX)
        .await
    {
        Ok(runs) => runs,
        Err(error) => {
            tracing::error!(%error, routine = %id, "could not read a routine's runs");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    // The aggregate is read AFTER the runs, so a run fired between the two reads is labelled
    // rather than dropped from the page for want of its `Fired`.
    let loaded = match store.load_schedule(&id).await {
        Ok((loaded, _)) => loaded,
        Err(error) => {
            tracing::error!(%error, routine = %id, "could not read a routine");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    let cause = |run: &str| {
        let ran = |runs: &std::collections::BTreeSet<String>| runs.contains(run);
        let causes = [
            (&loaded.manual_runs, "manual"),
            (&loaded.webhook_runs, "webhook"),
            (&loaded.clock_runs, "clock"),
        ];
        causes
            .into_iter()
            .find(|(runs, _)| ran(runs))
            .map(|(_, cause)| cause)
    };
    let rows = history(&runs, query.limit, cause, &loaded.skipped);
    ([NO_STORE], Json(rows)).into_response()
}

/// One page of a routine's or a monitor's history, newest first, shared so the two cannot drift:
/// `{runId, cause, status, startedAtMs, endedAtMs}`, `limit` clamped to 1..=`RUNS_MAX` (default
/// 20). `cause_of` names what started a run, or `None` for one the owner never fired — a person
/// replying in its thread is no firing, and is left out. A skipped firing (#316) is a row too:
/// every field a run's has, null where it has none, and its own `at`, `state: "skipped"`,
/// `skipped` (the code) and `reason`.
///
/// The status words (`running`, `waiting`, `ok`, `error`) are the run history's own vocabulary as
/// #82 and #235 write it, not `RunStatus::as_str`; an exhaustive match, so a new status does not
/// compile until it is given a word.
pub(super) fn history(
    runs: &[opengrok_store::ThreadRun],
    limit: Option<i64>,
    cause_of: impl Fn(&str) -> Option<&'static str>,
    skipped: &[Skip],
) -> Vec<Value> {
    use opengrok_core::run::RunStatus;
    let limit = usize::try_from(limit.unwrap_or(20).clamp(1, RUNS_MAX)).unwrap_or(20);
    let ran = runs.iter().filter_map(|run| {
        let key = run.id.as_str();
        let cause = cause_of(key)?;
        let (status, ended) = match RunStatus::from_stored(&run.status) {
            RunStatus::Running => ("running", false),
            RunStatus::AwaitingApproval => ("waiting", false),
            RunStatus::Finished => ("ok", true),
            RunStatus::Failed | RunStatus::Stopped => ("error", true),
        };
        let row = json!({
            "runId": key,
            "cause": cause,
            "status": status,
            "startedAtMs": run.started_at_ms,
            "endedAtMs": ended.then_some(run.updated_at_ms),
        });
        Some((run.started_at_ms, row))
    });
    let skips = skipped.iter().map(|skip| {
        let row = json!({ "runId": null, "cause": skip.cause.as_str(), "status": null,
            "startedAtMs": null, "endedAtMs": null, "at": skip.at_ms, "state": "skipped",
            "skipped": skip.code, "reason": crate::autonomy::skip_reason(&skip.code) });
        (skip.at_ms, row)
    });
    let mut rows: Vec<(i64, Value)> = ran.chain(skips).collect();
    rows.sort_by_key(|(at_ms, _)| std::cmp::Reverse(*at_ms));
    rows.into_iter().take(limit).map(|(_, row)| row).collect()
}

async fn pause_schedule(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Pause { at_ms }).await
}

async fn resume_schedule(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    change_schedule(state, headers, id, |at_ms| ScheduleCommand::Resume {
        at_ms,
    })
    .await
}

async fn delete_schedule(
    State(state): State<HostState>,
    headers: HeaderMap,
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
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let id = ScheduleId::from_stored(id);
    let account_id = match owned_schedule(&state.agui, &headers, &id).await {
        Ok((_, account_id)) => account_id,
        Err(refusal) => return refusal,
    };
    let key = crate::hooks::mint_webhook_key();
    let secret_hash = crate::hooks::hash_webhook_key(&key);
    let at_ms = now_ms();
    // Through `mutate_schedule` rather than `change`: the answer is the new key, so the
    // aggregate AFTER the append is the thing being asked for — and the retry it does is worth
    // having here, where a rotation racing an edit is the ordinary case rather than a rare one.
    let rotated = desk::mutate_schedule(&state.agui, &account_id, &id, at_ms, |loaded| {
        let rotate = ScheduleCommand::RotateWebhookSecret {
            secret_hash: secret_hash.clone(),
            webhook_key: key.clone(),
            at_ms,
        };
        loaded
            .decide(rotate)
            .map_err(|why| (StatusCode::CONFLICT, why.to_string()))
    });
    let after = match rotated.await {
        Ok(after) => after,
        Err(refusal) => return refused(refusal),
    };
    let webhook = crate::hooks::webhook_trigger_json(&state, &after.hook_id, &after.webhook_key);
    let body = json!({ "id": id.as_str(), "name": after.name, "kind": after.kind.as_str(),
        "webhook": webhook });
    ([NO_STORE], Json(body)).into_response()
}
