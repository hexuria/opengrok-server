//! Autonomy: schedules and monitors — the server starting runs instead of waiting for one.
//!
//! Everything before this slice answers when a client asks. This module is the other half of the
//! mission: a coworker that acts at a written-down time (`sweep::schedules_forever`) or in
//! reaction to something the event log recorded (`sweep::monitors_forever`), with the laptop that
//! configured it long since closed.
//!
//! A FIRED RUN IS AN ORDINARY RUN. It goes through `run_conversation_within`, held to its org's
//! ceiling and its routine's own limits like any chat to theirs, is journaled by
//! `StoreJournal`, is owned by the account that created the schedule or monitor, holds a recovery
//! lease while it works, and is replayable at `GET /ag-ui/runs/{id}` — which is exactly how a
//! client that was away catches up on what its coworkers did alone.
//!
//! HOW THE PERSON LEARNS WHAT A ROUTINE DID (#177). There is no live channel to push on: the
//! desktop's `agents-automation` frame and the seam-A transcript a routine used to post into went
//! with P0-E, and this server does not know which thread a client paints as a coworker's chat.
//! So the answer is read, not pushed, from three places a client already reaches:
//! - `GET /schedules` — every routine row carries `lastRun {runId, status, startedAtMs,
//!   finishedAtMs, summary}`, built from the run journal on each read (`last_run`). A client
//!   polls it and compares `lastRun.runId` / `status` with what it last showed; the summary is the
//!   sentence to show ("Routine Inbox ran: …", "… is waiting for you: …", "… failed: …").
//! - `GET /ag-ui/threads/{scheduleId}` (or `/ag-ui/runs/{lastRun.runId}`) — the whole run, as the
//!   same AG-UI history a chat turn replays.
//! - `GET /ag-ui/approvals` — a card a routine raised, with `coworkerId` and `origin` so it can be
//!   put beside the right coworker; a form's card is minted exactly as a chat turn's is, so
//!   `POST /ag-ui/user-form/submit` answers it.

pub(crate) mod desk;
pub mod monitors;
pub mod routes;
pub mod sweep;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use opengrok_core::coworker::Coworker;
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::inference::SourceKind;
use opengrok_core::limits::RunLimits;
use opengrok_core::schedule::{FireCause, ScheduleCommand, ScheduleView, Skip};
use opengrok_harness::ModelEndpoint;
use opengrok_harness::local_proxy::{self, Route, Saved};
use opengrok_harness::{ChatMessage, RunBudget, RunContext, run_conversation_within};

use crate::agui::routes::{AgUiState, StoreJournal};
use crate::host_state::HostState;
use crate::now_ms;

/// How many runs one routine or one monitor may have in flight before another wake is refused.
///
/// Three is "a burst is fine, a stampede is not". A webhook is pressed by whoever holds its key; a
/// monitor is pressed by its owner's own log, where one bad deploy can write a hundred
/// `run-failed` in a span — and every press is a run that is billed and holds a recovery lease.
/// The clock sweep caps itself the same way one level up (`sweep::CLAIM_LIMIT`).
pub const MAX_RUNS_IN_FLIGHT: i64 = 3;

/// One run nobody asked for, described: who it is for, which coworker takes it, what it opens
/// with, and which thread journals it.
pub(crate) struct Firing {
    /// For the log line: `schedule …`, `monitor …`, `automation … (webhook)`.
    pub origin: String,
    pub account_id: AccountId,
    pub coworker_id: CoworkerId,
    pub prompt: String,
    pub thread_id: String,
    pub run_id: RunId,
    /// The routine's own limits; empty for a monitor, which sets none.
    pub run_limits: RunLimits,
    /// The Bot's message this turn answers (#314), its outbox row; `None` for a routine's or a
    /// monitor's. A message that cannot be had is refused in its thread where a routine is only
    /// logged: its row stays claimed, holding its pair, until a run is there to say why.
    pub message: Option<opengrok_store::BotMessageRow>,
}

/// Fire one run as this coworker, for this account, and see it through to its ending.
///
/// POLICY IS CHECKED AT FIRE TIME, NOT AT CREATION. A schedule written while permission existed
/// must stop the moment permission is revoked — the grant is asked the same question a client's
/// own request would be asked, every single firing.
///
/// TAKES THE HOST STATE because a firing can stop on a card, and a card is minted through it
/// (`emit_user_form_suspensions`) exactly as a chat turn's is. Without that the run parks with no
/// `entryId`, and the form nobody was watching for can never be submitted.
pub(crate) async fn fire(host: HostState, firing: Firing) {
    let state = host.agui.clone();
    let Firing {
        origin,
        account_id,
        coworker_id,
        prompt,
        thread_id,
        run_id,
        run_limits,
        message,
    } = firing;
    let message = message.as_ref();
    let policy = state
        .auth
        .store
        .policy_for(&account_id, &coworker_id)
        .await
        .unwrap_or_default();
    let decision = opengrok_policy::decide(
        &account_id,
        &coworker_id,
        opengrok_policy::Action::UseCoworker,
        &policy,
    );
    let refused = |why: String| crate::pairs::refused(&state, message, why);
    if let Some(reason) = decision.reason() {
        tracing::warn!(%origin, coworker = %coworker_id, %reason, "a firing was refused by policy");
        return refused(format!("This message was not delivered: {reason}.")).await;
    }

    let Ok((coworker, _)) = state.auth.store.load_coworker(&coworker_id).await else {
        tracing::warn!(%origin, coworker = %coworker_id, "a firing named a coworker that does not load");
        return refused("This message was not delivered: its Bot could not be read.".into()).await;
    };
    // Asked here as well as at the door: a routine made before its coworker was retired still
    // names it, and a retired coworker's key is revoked — its turn would bill the deployment.
    if coworker.retired || coworker.is_group() {
        tracing::warn!(%origin, coworker = %coworker_id, "a firing named a coworker that cannot take work");
        let why = format!(
            "This message was not delivered: {} can no longer take work.",
            coworker.name
        );
        return refused(why).await;
    }
    // The org's ceiling as it stands NOW, over the routine's own limits: one lowered after the
    // routine was saved still binds, since `and` only ever narrows.
    let limits =
        crate::agui::routes::run_limits(&state, &account_id, Some(&coworker_id), run_limits);
    let Some(limits) = limits.await else {
        tracing::warn!(%origin, "a firing was refused: its org's run ceiling could not be read");
        return refused("This message was not answered: the run limits could not be read.".into())
            .await;
    };

    let tools = crate::agui::routes::tools_for_coworker(
        &state,
        &account_id,
        &coworker_id,
        &[],
        &[],
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await;
    // Of the routine tools, only the listing: a routine must not make routines (#316).
    let tools = tools.map(opengrok_harness::ToolRunner::with_routines_listing_only);
    let tools = crate::skills::onto_any(&state, &account_id, &coworker_id, tools).await;
    let who = (&account_id, &coworker_id);
    let tools = crate::pairs::onto(&state, who, Some(run_id.as_str()), None, tools).await;
    let skills: String = tools.iter().map(|t| t.skills_line()).collect();

    // Composed once: a routine's turn is still this coworker's turn, its skills (#270) too. A
    // message's is the pair thread's: its words are the turn's user message, never the system's.
    let hirer = crate::persona::caller(&state, &account_id).await;
    let (route, line, asked, said) = match message {
        Some(row) => match crate::pairs::opening(&state, row, &hirer).await {
            Some((line, said)) => {
                let route = Route::for_message(coworker.source, &coworker.model);
                (route, line, crate::pairs::prompt(row, &run_id), said)
            }
            None => {
                return refused(
                    "This message could not be quoted, so it was not delivered.".into(),
                )
                .await;
            }
        },
        // A routine on its person's own plan goes there, as a live turn would (#316); a monitor,
        // whose thread is its own id, keeps the refusal in words (review of #334).
        None => (
            match thread_id.starts_with("mon_") {
                true => Route::for_monitor(coworker.source, &coworker.model),
                false => routine_route(&state, (&account_id, &run_id), &coworker).await,
            },
            crate::persona::routine_line(&hirer, chrono::Utc::now()),
            opengrok_core::run::routine_prompt(&run_id, &prompt),
            vec![ChatMessage::text("user", prompt)],
        ),
    };
    let system = crate::persona::system_message(
        &coworker.name,
        &crate::persona::of(&state, &coworker_id, coworker.role.clone()).await,
        route.asks(&coworker.model),
        Some(&(line + &skills)),
    );
    let mut journal = StoreJournal {
        state: state.clone(),
        thread_id: thread_id.clone(),
        account_id: Some(account_id.clone()),
        coworker_id: Some(coworker_id.clone()),
        model: None,
        effort: coworker.effort,
        inference_source: route.source(),
        system: Some(system.clone()),
        skill_id: None,
        offered_skills: tools.iter().flat_map(|t| t.offered_skills()).collect(),
        // The hirer's instruction is this turn's question. Journaled like a person's message, so
        // a routine that parks on a card resumes knowing what it was told to do.
        prompt: Some(asked),
        limits,
        generation: 0,
    };

    // Nobody is talking: a coworker on its own schedule acts for whoever hired it, on its own
    // model, effort, identity and standing role — the rules `run()` holds turns to.
    let who = (Some(&coworker_id), Some(&account_id));
    let asked = (route, coworker.model.clone(), coworker.effort);
    let request = crate::agui::routes::turn_request(&state, who, asked, Some(system), said).await;
    // The model the run asks, as a live turn captures it: on the person's plan it may be the
    // setting's rather than the pin (#316), and a carry-on asks again what was captured.
    journal.model = Some(request.model.clone());

    // THE CLAIM, as a turn's: a message's run id is its row's, which a drain and the sweep may
    // both reach for, and only the one whose `Started` commits runs it (`PairDelivery` Start).
    match journal.claim(run_id.as_str()).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => return tracing::warn!(%origin, %error, "a firing could not start its run"),
    }

    // Held while the run works, so the recovery sweep does not mistake a slow firing for an
    // abandoned run; dropped (or killed) when the process dies, which is when recovery should.
    let _lease = crate::recovery::Lease::new(crate::recovery::hold(state.clone(), run_id.clone()));

    let events = run_conversation_within(
        state.door.as_ref(),
        tools.as_ref(),
        &journal,
        request,
        RunContext::new(&thread_id, run_id.as_str(), now_ms()),
        RunBudget::held_to(&limits),
        None,
    )
    .await;

    tracing::info!(%origin, run = %run_id, events = events.len(), "fired a run nobody asked for");

    // Form cards only, as `continue_run` mints them: any other pause is answered from
    // `GET /ag-ui/approvals` over `/ag-ui/runs/{id}/answer` and needs no transcript card, and a
    // card nobody settles would sit pending for good.
    crate::agui::resume::emit_user_form_suspensions(&host, &coworker_id, &account_id, &events)
        .await;
}

/// The 429 for a routine that already has `MAX_RUNS_IN_FLIGHT` unfinished runs of its owner's,
/// or `None` to go ahead. Shared by the webhook door and "run now": a person mashing the button is
/// as much a stampede of billed runs as a retry loop at the other end of a hook.
///
/// ONLY THE OWNER'S RUNS COUNT. Thread ids are the client's to choose, so anybody who learned a
/// routine's id could otherwise park three runs on it and hold its owner at 429 for good.
///
/// A BRAKE, NOT A LOCK. Two wakes in the same millisecond can both read the same number, and a
/// run counts only once it journals its first row, after policy and the coworker load. What this
/// exists to stop is the thousandth press, not the fourth.
pub(crate) async fn too_busy(
    state: &AgUiState,
    account_id: &AccountId,
    thread_id: &str,
) -> Option<Response> {
    // Unfinished runs are the newest, so a hundred rows reach every one that could matter.
    match state
        .auth
        .store
        .runs_for_thread_owned_by(thread_id, account_id, 100)
        .await
    {
        Ok(runs) => {
            let in_flight = runs
                .iter()
                .filter(|run| {
                    !opengrok_core::run::RunStatus::from_stored(&run.status).is_terminal()
                })
                .count();
            if i64::try_from(in_flight).unwrap_or(i64::MAX) >= MAX_RUNS_IN_FLIGHT {
                tracing::warn!(routine = %thread_id, %in_flight, "refused a wake: too much already running");
                return Some(json_reply(
                    StatusCode::TOO_MANY_REQUESTS,
                    "this routine already has three runs in flight; wait for one to end",
                ));
            }
            None
        }
        Err(error) => {
            tracing::error!(%error, routine = %thread_id, "could not count a routine's runs in flight");
            Some(json_reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage failed",
            ))
        }
    }
}

/// Start the run whose `Fired` the log already holds (`sweep.rs` says why the event goes first),
/// and answer `202 {accepted, runId}` — the half of the webhook door and both "run now"s that is
/// the same. The coworker comes from the aggregate that append produced, so it is the one the
/// routine or monitor has NOW, not whichever one a client last listed — and so do its limits.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_fired(
    host: &HostState,
    coworker_id: Option<CoworkerId>,
    account_id: AccountId,
    thread_id: &str,
    run_id: RunId,
    prompt: String,
    origin: String,
    run_limits: RunLimits,
) -> Response {
    let Some(coworker_id) = coworker_id else {
        return json_reply(
            StatusCode::CONFLICT,
            "nothing here names a coworker to run it",
        );
    };
    let reply = serde_json::json!({ "accepted": true, "runId": run_id.as_str() });
    tokio::spawn(fire(
        host.clone(),
        Firing {
            origin,
            account_id,
            coworker_id,
            prompt,
            thread_id: thread_id.to_string(),
            run_id,
            run_limits,
            message: None,
        },
    ));
    (StatusCode::ACCEPTED, Json(reply)).into_response()
}

fn json_reply(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// A firing's way to its model (#316), a routine's or a monitor's: THE GATEWAY for a coworker
/// off its person's own plan, whatever its hirer chose for their own turns (#294). One whose own
/// `source` is `local_proxy` answers on that plan alone, so its firing goes there exactly as a
/// live turn on it would, by the setting's way, and never to the gateway in its place.
pub(crate) async fn routine_route(
    saved: &dyn Saved,
    (account, run_id): (&AccountId, &RunId),
    coworker: &Coworker,
) -> Route {
    let own = (coworker.source, Some(coworker.model.clone()));
    match coworker.source == Some(SourceKind::LocalProxy) {
        true => local_proxy::route(saved, Some(account), None, None, run_id.as_str(), own).await,
        false => Route::Gateway,
    }
}

/// A skipped firing's code, and the sentence its row says it in, by the way its plan goes.
pub(crate) const SKIPPED: [(&str, &str); 2] = [
    (
        "relay_offline",
        "Skipped: your computer was off, so your plan couldn't answer",
    ),
    ("proxy_down", "Skipped: your plan's proxy didn't answer"),
];

/// Why `route` would find nobody to answer a firing now, as its skip's code and sentence: the
/// person's Mac holds no relay stream, or their proxy does not answer `/healthz`. `None` on the
/// gateway, and for a refusal in words, which a live turn on that setting gets too.
pub(crate) async fn unreachable_by(route: &Route) -> Option<(&'static str, &'static str)> {
    let Route::LocalProxy { endpoint, .. } = route else {
        return None;
    };
    let (up, way) = match endpoint {
        ModelEndpoint::Relay(to) => (to.broker.connected(&to.account).is_some(), 0),
        ModelEndpoint::Proxy { base_url, .. } => (local_proxy::healthy(base_url).await, 1),
        ModelEndpoint::Unavailable { .. } => (true, 0),
    };
    (!up).then_some(SKIPPED[way])
}

/// Why a routine's firing now would find nobody to answer it (`unreachable_by`), its coworker
/// read as it is now. `None` is a firing that runs. Asked before the `Fired` is written, by the
/// clock, a hook and "run now" alike, so a skip starts nothing and asks no model at all.
pub(crate) async fn unreachable(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    run_id: &RunId,
) -> Option<(&'static str, &'static str)> {
    let (coworker, _) = state.auth.store.load_coworker(coworker_id).await.ok()?;
    unreachable_by(&routine_route(state, (account_id, run_id), &coworker).await).await
}

/// What a firing writes: its `Fired`, or the `Skipped` that `unreachable` gave the words for.
pub(crate) fn firing(
    skip: Option<(&str, &str)>,
    cause: FireCause,
    run_id: &RunId,
) -> ScheduleCommand {
    let at_ms = now_ms();
    match skip {
        Some((code, _)) => ScheduleCommand::Skip(Skip {
            cause,
            code: code.to_string(),
            at_ms,
        }),
        None => ScheduleCommand::Fire {
            run_id: run_id.clone(),
            cause,
            at_ms,
        },
    }
}

/// The sentence a skip's code is said in, on its history row and its `lastRun`.
pub(crate) fn skip_reason(code: &str) -> &'static str {
    let said = SKIPPED.iter().find(|(known, _)| *known == code);
    said.map_or("Skipped", |(_, why)| why)
}

/// A routine's newest run, or its newest skipped firing when that came after it (#316), as its
/// row on `GET /schedules` carries it: `null` for a routine that has done neither.
///
/// READ FROM THE JOURNAL ON EVERY LISTING, NOT WRITTEN WHEN THE RUN ENDS. A routine that stops on
/// a card finishes on a different code path days later (`continue_run`, a form's submit), and a
/// sentence written at the end of `fire` would say "waiting" forever. Reading it follows the run
/// wherever it ends. Owner-scoped and hidden-aware like `GET /ag-ui/threads/{id}`, so a run the
/// person hid is not summarised back at them. A skip keeps every field a run's has, null where
/// it has none, with its sentence as the summary.
pub(crate) async fn last_run(
    state: &AgUiState,
    account_id: &AccountId,
    view: &ScheduleView,
) -> Result<Option<serde_json::Value>, opengrok_store::StoreError> {
    let store = &state.auth.store;
    let newest = store
        .runs_for_thread_owned_by(&view.id, account_id, 1)
        .await?;
    let newest = newest.into_iter().next();
    let ran_at = newest.as_ref().map_or(i64::MIN, |run| run.started_at_ms);
    if let Some(skip) = view.last_skip.as_ref().filter(|skip| skip.at_ms > ran_at) {
        let why = skip_reason(&skip.code);
        return Ok(Some(serde_json::json!({ "runId": null, "status": null,
            "startedAtMs": null, "finishedAtMs": null, "summary": why, "at": skip.at_ms,
            "cause": skip.cause.as_str(), "state": "skipped", "skipped": skip.code,
            "reason": why })));
    }
    let Some(newest) = newest else {
        return Ok(None);
    };
    let (run, _) = state.auth.store.load_run(&newest.id).await?;
    Ok(Some(serde_json::json!({
        "runId": newest.id.as_str(),
        "status": run.status.as_str(),
        "startedAtMs": newest.started_at_ms,
        "finishedAtMs": run.status.is_terminal().then_some(newest.updated_at_ms),
        "summary": run_summary(&view.name, &run),
    })))
}

/// What a routine's run came to, in one sentence that opens with the routine's name — so a
/// person reading it knows WHY the coworker spoke.
///
/// A RUN WAITING ON A CARD IS NOT A RUN THAT SAID NOTHING. It used to be announced as "ran and
/// produced no answer", which sends the person to a log instead of to the card that is waiting
/// for them; it now says it is waiting, and on what.
pub(crate) fn run_summary(name: &str, run: &opengrok_core::run::Run) -> String {
    use opengrok_core::run::RunStatus;
    match run.status {
        RunStatus::Running => format!("Routine {name} is running."),
        RunStatus::AwaitingApproval => match run.pending.as_ref() {
            Some(pending) => format!(
                "Routine {name} is waiting for you: {}",
                crate::agui::routes::waiting_why(run, pending)
            ),
            None => format!("Routine {name} is waiting for you."),
        },
        RunStatus::Stopped => format!("Routine {name} was stopped before it finished."),
        RunStatus::Failed => {
            let frames: Vec<opengrok_wire::agui::Event> = run
                .emitted
                .iter()
                .filter_map(|frame| serde_json::from_value(frame.clone()).ok())
                .collect();
            match crate::agui::resume::failure_sentence(&frames).or_else(|| run.failure.clone()) {
                Some(why) => format!("Routine {name} failed: {why}"),
                None => format!("Routine {name} failed. Its run log has the reason."),
            }
        }
        RunStatus::Finished => {
            let text = last_answer(&run.emitted);
            let text = text.trim();
            let head: String = text.chars().take(200).collect();
            if head.is_empty() {
                format!("Routine {name} ran and produced no answer. Its run log has the reason.")
            } else if head.chars().count() < text.chars().count() {
                format!("Routine {name} ran: {head}…")
            } else {
                format!("Routine {name} ran: {head}")
            }
        }
    }
}

/// The text of the run's LAST assistant message. A routine that stopped on a card and carried on
/// said something before the card ("I need you to sign in") and its answer after it; the answer
/// is what the person needs, and a prompt the journal replays as a user message is never it.
fn last_answer(emitted: &[serde_json::Value]) -> String {
    let field = |frame: &serde_json::Value, key: &str| {
        frame
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let mut from_the_person = std::collections::HashSet::new();
    let mut current: Option<String> = None;
    let mut text = String::new();
    for frame in emitted {
        match field(frame, "type").as_deref() {
            Some("TEXT_MESSAGE_START") if field(frame, "role").as_deref() == Some("user") => {
                if let Some(id) = field(frame, "messageId") {
                    from_the_person.insert(id);
                }
            }
            Some("TEXT_MESSAGE_CONTENT") => {
                let id = field(frame, "messageId");
                if id.as_ref().is_some_and(|id| from_the_person.contains(id)) {
                    continue;
                }
                if id.is_some() && id != current {
                    text.clear();
                    current = id;
                }
                if let Some(delta) = field(frame, "delta") {
                    text.push_str(&delta);
                }
            }
            _ => {}
        }
    }
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../../tests/unit/autonomy.rs"]
mod tests;
