//! Autonomy: schedules and monitors — the server starting runs instead of waiting for one.
//!
//! Everything before this slice answers when a client asks. This module is the other half of the
//! mission: a coworker that acts at a written-down time (`sweep::schedules_forever`) or in
//! reaction to something the event log recorded (`sweep::monitors_forever`), with the laptop that
//! configured it long since closed.
//!
//! A FIRED RUN IS AN ORDINARY RUN. It goes through `run_conversation`, is journaled by
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

pub mod routes;
pub mod sweep;

use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_harness::{ChatMessage, ModelRequest, run_conversation};

use crate::agui::routes::{AgUiState, StoreJournal};
use crate::host_state::HostState;

/// How many runs one routine or one monitor may have in flight before another wake is refused.
///
/// Three is "a burst is fine, a stampede is not". A webhook is pressed by whoever holds its key; a
/// monitor is pressed by its owner's own log, where one bad deploy can write a hundred
/// `run-failed` in a span — and every press is a run that is billed and holds a recovery lease.
/// The clock sweep caps itself the same way one level up (`sweep::CLAIM_LIMIT`).
pub const MAX_RUNS_IN_FLIGHT: i64 = 3;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

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
    } = firing;
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
    if let Some(reason) = decision.reason() {
        tracing::warn!(%origin, coworker = %coworker_id, %reason, "a firing was refused by policy");
        return;
    }

    let Ok((coworker, _)) = state.auth.store.load_coworker(&coworker_id).await else {
        tracing::warn!(%origin, coworker = %coworker_id, "a firing named a coworker that does not load");
        return;
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

    // Composed once: a routine's turn is still this coworker's turn.
    let system = crate::persona::system_message(
        &coworker.name,
        &crate::persona::of(&state, &coworker_id, coworker.role.clone()).await,
        Some(&crate::persona::routine_line(
            &crate::persona::caller(&state, &account_id).await,
            chrono::Utc::now(),
        )),
    );
    let journal = StoreJournal {
        state: state.clone(),
        thread_id: thread_id.clone(),
        account_id: Some(account_id.clone()),
        coworker_id: Some(coworker_id.clone()),
        model: Some(coworker.model.clone()),
        system: Some(system.clone()),
        skill_id: None,
        // The hirer's instruction is this turn's question. Journaled like a person's message, so
        // a routine that parks on a card resumes knowing what it was told to do.
        prompt: Some(crate::agui::history::routine_prompt(&run_id, &prompt)),
    };

    let request = ModelRequest {
        gateway_key: crate::spend::key_for(&state, &coworker_id, &account_id).await,
        spend_scope: Some(coworker_id.as_str().to_string()),
        // Nobody is talking: a coworker acting on its own schedule acts for whoever hired it.
        spend_actor: Some(account_id.as_str().to_string()),
        // The coworker's own model — the rule `run()` enforces holds for runs nobody asked for.
        model: coworker.model.clone(),
        // A routine's turn is still this coworker's turn: same identity, same standing role.
        system: Some(system.clone()),
        tools: Vec::new(),
        messages: vec![ChatMessage::text("user", prompt)],
    };

    // Held while the run works, so the recovery sweep does not mistake a slow firing for an
    // abandoned run; dropped (or killed) when the process dies, which is when recovery should.
    let _lease = crate::recovery::Lease::new(crate::recovery::hold(state.clone(), run_id.clone()));

    let events = run_conversation(
        state.door.as_ref(),
        tools.as_ref(),
        &journal,
        request,
        &thread_id,
        run_id.as_str(),
        now_ms(),
    )
    .await;

    tracing::info!(%origin, run = %run_id, events = events.len(), "fired a run nobody asked for");

    // Form cards only, as `continue_run` mints them: any other pause is answered from
    // `GET /ag-ui/approvals` over `/ag-ui/runs/{id}/answer` and needs no transcript card, and a
    // card nobody settles would sit pending for good.
    crate::agui::resume::emit_user_form_suspensions(&host, &coworker_id, &account_id, &events)
        .await;
}

/// A routine's newest run, as its row on `GET /schedules` carries it: `null` for a routine that
/// has never run.
///
/// READ FROM THE JOURNAL ON EVERY LISTING, NOT WRITTEN WHEN THE RUN ENDS. A routine that stops on
/// a card finishes on a different code path days later (`continue_run`, a form's submit), and a
/// sentence written at the end of `fire` would say "waiting" forever. Reading it follows the run
/// wherever it ends. Owner-scoped and hidden-aware like `GET /ag-ui/threads/{id}`, so a run the
/// person hid is not summarised back at them.
pub(crate) async fn last_run(
    state: &AgUiState,
    account_id: &AccountId,
    schedule_id: &str,
    name: &str,
) -> Result<Option<serde_json::Value>, opengrok_store::StoreError> {
    let Some(newest) = state
        .auth
        .store
        .runs_for_thread_owned_by(schedule_id, account_id, 1)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let (run, _) = state.auth.store.load_run(&newest.id).await?;
    Ok(Some(serde_json::json!({
        "runId": newest.id.as_str(),
        "status": run.status.as_str(),
        "startedAtMs": newest.started_at_ms,
        "finishedAtMs": run.status.is_terminal().then_some(newest.updated_at_ms),
        "summary": run_summary(name, &run),
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
