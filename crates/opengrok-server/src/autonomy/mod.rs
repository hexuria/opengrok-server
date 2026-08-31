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

pub mod routes;
pub mod sweep;

use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_harness::{ChatMessage, ModelRequest, run_conversation};

use crate::agui::routes::{AgUiState, StoreJournal};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Fire one run as this coworker, for this account, and see it through to its ending.
///
/// POLICY IS CHECKED AT FIRE TIME, NOT AT CREATION. A schedule written while permission existed
/// must stop the moment permission is revoked — the grant is asked the same question a client's
/// own request would be asked, every single firing.
pub(crate) async fn fire(
    state: AgUiState,
    origin: String,
    account_id: AccountId,
    coworker_id: CoworkerId,
    prompt: String,
    thread_id: String,
    run_id: RunId,
) {
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

    let tools = crate::agui::routes::tools_for_coworker(&state, &account_id, &coworker_id).await;

    let journal = StoreJournal {
        state: state.clone(),
        thread_id: thread_id.clone(),
        account_id: Some(account_id),
        coworker_id: Some(coworker_id),
    };

    let request = ModelRequest {
        // The coworker's own model — the rule `run()` enforces holds for runs nobody asked for.
        model: coworker.model.clone(),
        system: None,
        tools: Vec::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }],
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
}
