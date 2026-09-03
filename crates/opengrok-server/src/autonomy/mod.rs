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

/// Where a routine's run shows up for the person: the coworker's own chat, as a message from the
/// coworker, sent live. The run keeps its own thread (the schedule's id) for the pane's history;
/// this is the part a person actually reads. `None` for firings nobody needs told about.
pub(crate) struct Announce {
    pub gateway: crate::gateway::GatewayState,
    /// The routine's name — the message opens with it so the chat says WHY the coworker spoke.
    pub name: String,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One run nobody asked for, described: who it is for, which coworker takes it, what it opens
/// with, which thread journals it, and whether the person is told when it ends.
pub(crate) struct Firing {
    /// For the log line: `schedule …`, `monitor …`, `automation … (run now)`.
    pub origin: String,
    pub account_id: AccountId,
    pub coworker_id: CoworkerId,
    pub prompt: String,
    pub thread_id: String,
    pub run_id: RunId,
    pub announce: Option<Announce>,
}

/// Fire one run as this coworker, for this account, and see it through to its ending.
///
/// POLICY IS CHECKED AT FIRE TIME, NOT AT CREATION. A schedule written while permission existed
/// must stop the moment permission is revoked — the grant is asked the same question a client's
/// own request would be asked, every single firing.
pub(crate) async fn fire(state: AgUiState, firing: Firing) {
    let Firing {
        origin,
        account_id,
        coworker_id,
        prompt,
        thread_id,
        run_id,
        announce,
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

    let journal = StoreJournal {
        state: state.clone(),
        thread_id: thread_id.clone(),
        account_id: Some(account_id),
        coworker_id: Some(coworker_id.clone()),
        model: Some(coworker.model.clone()),
    };

    let request = ModelRequest {
        gateway_key: crate::spend::key_for(&state, &coworker_id).await,
        spend_scope: Some(coworker_id.as_str().to_string()),
        // The coworker's own model — the rule `run()` enforces holds for runs nobody asked for.
        model: coworker.model.clone(),
        // A routine's turn is still this coworker's turn: same identity, same standing role.
        system: Some(crate::persona::system_message(
            &coworker.name,
            &crate::persona::of(&state, &coworker_id, coworker.role.clone()).await,
            None,
        )),
        tools: Vec::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }],
    };

    // Held while the run works, so the recovery sweep does not mistake a slow firing for an
    // abandoned run; dropped (or killed) when the process dies, which is when recovery should.
    let _lease = crate::recovery::Lease::new(crate::recovery::hold(state.clone(), run_id.clone()));

    if let Some(announce) = &announce {
        // The roster shows the coworker thinking while its routine runs, the same as a turn.
        crate::gateway::live::set_running(
            &announce.gateway,
            coworker_id.as_str(),
            true,
            serde_json::json!({}),
        )
        .await;
    }

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

    if let Some(announce) = announce {
        announce_finished(&announce, &coworker_id, &events).await;
    }
}

/// Post the finished routine into the coworker's chat and refresh the Routines pane. The chat
/// line carries the routine's name and the answer's head (the run's own thread has the whole
/// thing); it is appended to the gateway transcript like any coworker message and emitted live.
async fn announce_finished(
    announce: &Announce,
    coworker_id: &CoworkerId,
    events: &[opengrok_wire::agui::Event],
) {
    let gateway = &announce.gateway;
    let mut text = String::new();
    for event in events {
        if event.event_type == opengrok_wire::agui::EventType::TextMessageContent
            && let Some(delta) = event.extra.get("delta").and_then(serde_json::Value::as_str)
        {
            text.push_str(delta);
        }
    }
    let head: String = text.trim().chars().take(200).collect();
    let content = if head.is_empty() {
        match crate::gateway::conversation::failure_sentence(events) {
            Some(why) => format!("Routine {} failed: {why}", announce.name),
            None => format!(
                "Routine {} ran and produced no answer. Its run log has the reason.",
                announce.name
            ),
        }
    } else if head.chars().count() < text.trim().chars().count() {
        format!("Routine {} ran: {head}…", announce.name)
    } else {
        format!("Routine {} ran: {head}", announce.name)
    };
    let at_ms = now_ms();
    let entry = serde_json::json!({
        "kind": "send-message",
        "id": format!("e_{}", uuid::Uuid::now_v7()),
        "message": { "type": "text", "content": content },
        "timestampMs": at_ms,
    });
    match gateway
        .agui
        .auth
        .store
        .append_gateway_entry(coworker_id, &entry, at_ms)
        .await
    {
        Ok(_) => {
            crate::gateway::live::emit_transcript(gateway, coworker_id.as_str(), "appended", entry);
        }
        Err(error) => {
            tracing::error!(%error, coworker = %coworker_id, "could not post a routine's result");
        }
    }
    let preview: String = text.chars().take(120).collect();
    crate::gateway::live::set_running(
        gateway,
        coworker_id.as_str(),
        false,
        serde_json::json!({ "lastMessagePreview": preview }),
    )
    .await;
    crate::gateway::lifecycle::emit_automations(gateway, coworker_id.as_str()).await;
}
