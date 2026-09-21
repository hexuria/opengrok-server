//! Resuming a suspended run, and everything on both sides of the pause: finding a suspension in a
//! batch of AG-UI events, minting its card, stamping the entry id the AG-UI submit route answers
//! against, stopping a parked run, and picking the model back up once a person has answered.
//!
//! This is what is left of P4. The send path around it — `sendPrompt`, the streaming sink, the
//! roster pulses, the turn guard — went with seam A; AG-UI drives every turn now, and what stays
//! is the human-in-the-loop machinery AG-UI, the MCP door and the hooks all suspend and resume
//! through.

use serde_json::{Value, json};

use opengrok_core::id::{CoworkerId, RunId};
use opengrok_harness::ModelRequest;

use crate::host_state::HostState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn entry_id() -> String {
    format!("e_{}", uuid::Uuid::now_v7())
}

/// Stop every parked HITL run for this coworker and settle unresolved user-form / live
/// handoff chrome without resuming the model. New user text then starts a fresh turn.
pub(crate) async fn interrupt_parked_hitl(
    state: &HostState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    by: &str,
) -> usize {
    let Ok(run_ids) = state.agui.auth.store.awaiting_approval(account_id).await else {
        return 0;
    };
    let mut stopped = 0usize;
    for run_id in run_ids {
        if stop_parked_run(state, account_id, coworker_id, &run_id, by).await {
            stopped += 1;
        }
    }
    if stopped > 0 {
        super::user_form::dismiss_unresolved_on_interrupt(state, account_id, coworker_id).await;
    }
    stopped
}

async fn stop_parked_run(
    state: &HostState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    run_id: &opengrok_core::id::RunId,
    by: &str,
) -> bool {
    for _ in 0..5 {
        let Ok((mut run, seq)) = state.agui.auth.store.load_run(run_id).await else {
            return false;
        };
        if !run_belongs_to(&run, coworker_id) {
            return false;
        }
        if run.status != opengrok_core::run::RunStatus::AwaitingApproval {
            return false;
        }
        let at_ms = now_ms();
        let mut events = Vec::new();
        let frames = [
            opengrok_wire::agui::Event::new(opengrok_wire::agui::EventType::Custom, at_ms)
                .with("name", "run-stopped")
                .with("threadId", run.thread_id.clone())
                .with("runId", run_id.as_str()),
            opengrok_wire::agui::Event::new(opengrok_wire::agui::EventType::RunFinished, at_ms)
                .with("threadId", run.thread_id.clone())
                .with("runId", run_id.as_str()),
        ];
        for frame in &frames {
            let Ok(payload) = serde_json::to_value(frame) else {
                continue;
            };
            let Ok(decided) = run.decide(opengrok_core::run::RunCommand::Emit { payload, at_ms })
            else {
                continue;
            };
            for event in &decided {
                run.apply(event);
            }
            events.extend(decided);
        }
        let Ok(stopped) = run.decide(opengrok_core::run::RunCommand::Stop {
            by: by.to_string(),
            at_ms,
        }) else {
            return false;
        };
        for event in &stopped {
            run.apply(event);
        }
        events.extend(stopped);
        let view = opengrok_core::run::RunView {
            id: run_id.clone(),
            thread_id: run.thread_id.clone(),
            status: run.status,
            event_count: run.emitted.len() as i64,
            updated_at_ms: at_ms,
        };
        match state
            .agui
            .auth
            .store
            .append_run(run_id, seq, &events, &view, Some(account_id))
            .await
        {
            Ok(_) => return true,
            Err(opengrok_store::StoreError::Conflict) => continue,
            Err(error) => {
                tracing::error!(%error, run = %run_id, "could not interrupt a parked run");
                return false;
            }
        }
    }
    false
}

/// How much of a quoted message the model is shown; a reply to a long answer names the answer,
/// it does not replay it.
const REPLY_QUOTE_CHARS: usize = 1_000;

/// How every quote line opens. A client that spells the quote into the message itself — NativeChat
/// does, because that is all a server without this field would read — is recognised by it, so the
/// context is not said twice.
pub(crate) const REPLY_QUOTE_OPENING: &str = "[Replying to";

/// The one sentence a quote is written as: an AG-UI message with a `replyTo` on it. Shared with
/// the AG-UI door so the two cannot drift, and so a message that carries the sentence already can
/// be told apart from one that does not.
///
/// `None` when the quoted message had no words to quote.
pub(crate) fn reply_quote_line(who: &str, text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let clipped = if text.chars().count() > REPLY_QUOTE_CHARS {
        let head: String = text.chars().take(REPLY_QUOTE_CHARS).collect();
        format!("{head}…")
    } else {
        text.to_string()
    };
    Some(format!("{REPLY_QUOTE_OPENING} {who}: \"{clipped}\"]"))
}

fn answer_entry(id: &str, text: &str, reply_to: Option<&str>) -> Value {
    let mut entry = json!({
        "kind": "send-message",
        "id": id,
        "message": { "type": "text", "content": text },
        "timestampMs": now_ms(),
    });
    if let Some(reply_to) = reply_to {
        entry["replyTo"] = json!(reply_to);
    }
    entry
}

/// The suspension a run's events carry, if any: which call is waiting, with what, and WHY. The
/// reason picks the card — the tool name no longer can, since two cards can come from one tool.
pub(crate) struct Suspension {
    pub(crate) call_id: String,
    pub(crate) tool: String,
    pub(crate) arguments: Value,
    pub(crate) reason: opengrok_core::run::SuspendReason,
    /// The gate's sentence, when it gave one — a policy grant's reason.
    pub(crate) why: Option<String>,
}

pub(crate) fn is_suspend_custom(name: Option<&str>) -> bool {
    name == Some("run-awaiting-approval")
}

pub(crate) fn find_suspension(events: &[opengrok_wire::agui::Event]) -> Option<Suspension> {
    find_suspensions(events).into_iter().next()
}

/// Every HITL CUSTOM in the batch, in order. Same-completion stacked `request_user_form`
/// calls each emit one; dropping all but the first is how extra Website login cards
/// painted without an `entryId`.
pub(crate) fn find_suspensions(events: &[opengrok_wire::agui::Event]) -> Vec<Suspension> {
    let mut found = Vec::new();
    for event in events {
        if event.event_type != opengrok_wire::agui::EventType::Custom
            || !is_suspend_custom(event.extra.get("name").and_then(Value::as_str))
        {
            continue;
        }
        let call_id = event
            .extra
            .get("callId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if call_id.is_empty() {
            continue;
        }
        if found
            .iter()
            .any(|item: &Suspension| item.call_id == call_id)
        {
            continue;
        }
        found.push(Suspension {
            call_id,
            tool: event
                .extra
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: event.extra.get("arguments").cloned().unwrap_or(Value::Null),
            reason: opengrok_core::run::SuspendReason::from_stored(
                event
                    .extra
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            why: event
                .extra
                .get("why")
                .and_then(Value::as_str)
                .filter(|why| !why.is_empty())
                .map(str::to_string),
        });
    }
    found
}

/// The card for a suspension, or `None` when this kind of pause has no card yet. requestId =
/// callId for both cards, threaded back onto the run when the card is answered so every gate
/// converges on one id.
pub(crate) fn card_for(suspension: &Suspension) -> Option<Value> {
    use opengrok_core::run::SuspendReason;
    match suspension.reason {
        // The machine owner's consent: the four-button `local-tool-permission` card, byte-identical
        // to what shipped before reasons existed.
        SuspendReason::ExecConsent if suspension.tool == opengrok_tools::USER_MACHINE_SHELL => {
            let command = suspension
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(json!({
                "kind": "send-message",
                "id": entry_id(),
                "timestampMs": now_ms(),
                "message": {
                    "type": "local-tool-permission",
                    "ask": {
                        "requestId": suspension.call_id,
                        "status": "pending",
                        "action": "run-command",
                        "target": command,
                    },
                },
            }))
        }
        SuspendReason::AutoReview => Some(crate::cards::auto_review_card(
            &entry_id(),
            &suspension.call_id,
            "pending",
            &suspension.tool,
            &suspension.arguments,
            Some(
                suspension
                    .why
                    .as_deref()
                    .filter(|why| !why.is_empty())
                    .unwrap_or(opengrok_tools::review::REVIEW_ASK_REASON),
            ),
            now_ms(),
        )),
        // A policy grant's "needs a human yes": the same auto-review card, carrying the grant's
        // reason and no proposed rule. Answered by `resolveAutoReviewApproval`, which routes the
        // yes to the GATE (not the judge) by this reason.
        SuspendReason::PolicyApproval => Some(crate::cards::policy_approval_card(
            &entry_id(),
            &suspension.call_id,
            "pending",
            &suspension.tool,
            &suspension.arguments,
            suspension.why.as_deref(),
            now_ms(),
        )),
        SuspendReason::UserForm => Some(crate::cards::user_form_card(
            &entry_id(),
            &suspension.arguments,
            now_ms(),
            &suspension.call_id,
        )),
        // Reverse-exec consent on anything but user_machine_shell: nothing renders it.
        _ => None,
    }
}

/// Append a suspension's card and pause the agent. `true` when a card went out; the caller then
/// returns without finalising the turn as an answer.
///
/// Also the AG-UI door (`POST /ag-ui`): NativeChat never calls `sendPrompt` and never watches
/// the gateway transcript live stream, so `AgUiSink` mints this card **before** the CUSTOM
/// frame and stamps `entryId` on it. `POST /ag-ui/user-form/submit` uses that same id.
pub(crate) async fn emit_suspension(
    state: &HostState,
    coworker_id: &CoworkerId,
    account: &opengrok_core::id::AccountId,
    agent_id: &str,
    suspension: &Suspension,
) -> bool {
    let card = card_for(suspension);
    if card.is_none() {
        tracing::warn!(
            tool = %suspension.tool,
            reason = suspension.reason.as_str(),
            "a run suspended for a reason that has no card yet; the turn ends as an answer"
        );
        return false;
    }
    if let Some(card) = card
        && let Err(error) = state
            .agui
            .auth
            .store
            .append_gateway_entry(coworker_id, account, &card, now_ms())
            .await
    {
        tracing::error!(%error, "could not append the suspension card entry");
    }
    if suspension.reason == opengrok_core::run::SuspendReason::UserForm {
        super::user_form::spawn_form_hold_timeout(
            state.clone(),
            account.clone(),
            coworker_id.clone(),
            agent_id.to_string(),
        );
    }
    true
}

/// Mint a card for every HITL CUSTOM in the batch. `true` when at least one pause
/// should hold the turn (a card went out).
/// The answer path mints only form cards: an approval raised by a continued run is answered
/// over `/ag-ui/runs/{id}/answer` and needs no gateway card, and a card nobody settles would
/// sit pending in the transcript for good.
pub(crate) async fn emit_user_form_suspensions(
    state: &HostState,
    coworker_id: &CoworkerId,
    account: &opengrok_core::id::AccountId,
    agent_id: &str,
    events: &[opengrok_wire::agui::Event],
) -> bool {
    let mut held = false;
    for suspension in find_suspensions(events)
        .into_iter()
        .filter(|s| s.reason == opengrok_core::run::SuspendReason::UserForm)
    {
        if emit_suspension(state, coworker_id, account, agent_id, &suspension).await {
            held = true;
        }
    }
    held
}

pub(crate) async fn emit_suspensions(
    state: &HostState,
    coworker_id: &CoworkerId,
    account: &opengrok_core::id::AccountId,
    agent_id: &str,
    events: &[opengrok_wire::agui::Event],
) -> bool {
    let mut held = false;
    for suspension in find_suspensions(events) {
        if emit_suspension(state, coworker_id, account, agent_id, &suspension).await {
            held = true;
        }
    }
    held
}

/// NativeChat is AG-UI-first and never watches the gateway transcript live stream. When this
/// CUSTOM is `run-awaiting-approval` / `reason: user-form`, append the gateway card **first** so
/// the id is stable, then stamp `extra.entryId` (and `formRequest`, the sanitised schema the card
/// already carries) onto the event. The SSE frame NativeChat receives therefore has the same id
/// `POST /ag-ui/user-form/submit` needs. Other CUSTOM reasons
/// are left untouched. Idempotent if `entryId` is already present.
pub(crate) async fn stamp_user_form_entry_id(
    state: &HostState,
    coworker_id: &CoworkerId,
    account: &opengrok_core::id::AccountId,
    event: &mut opengrok_wire::agui::Event,
) -> Option<String> {
    if event.event_type != opengrok_wire::agui::EventType::Custom {
        return None;
    }
    if event.extra.get("name").and_then(Value::as_str) != Some("run-awaiting-approval") {
        return None;
    }
    if event.extra.get("reason").and_then(Value::as_str) != Some("user-form") {
        return None;
    }
    if let Some(existing) = event
        .extra
        .get("entryId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return Some(existing.to_string());
    }
    let suspension = find_suspension(std::slice::from_ref(event))?;
    let card = card_for(&suspension)?;
    let entry_id = card.get("id").and_then(Value::as_str)?.to_string();
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(coworker_id, account, &card, now_ms())
        .await
    {
        tracing::error!(
            %error,
            "could not append the user-form card; not stamping entryId"
        );
        return None;
    }
    super::user_form::spawn_form_hold_timeout(
        state.clone(),
        account.clone(),
        coworker_id.clone(),
        coworker_id.as_str().to_string(),
    );
    apply_user_form_stamp(&mut event.extra, entry_id, true)
}

/// Stamp CUSTOM extra only after the card is in the transcript. Stamping a ghost
/// `entryId` is how NativeChat POSTs submit and gets Null (the collapse blocker).
pub(crate) fn apply_user_form_stamp(
    extra: &mut opengrok_wire::agui::Extra,
    entry_id: String,
    appended: bool,
) -> Option<String> {
    if !appended {
        return None;
    }
    extra.insert("entryId".to_string(), json!(entry_id.clone()));
    // Same sanitised schema the card stores as `message.formRequest`. CUSTOM already has it
    // as `arguments`; this alias is the field name TurnAssembler / the card already use.
    if extra.get("formRequest").is_none()
        && let Some(schema) = extra.get("arguments").cloned()
    {
        extra.insert("formRequest".to_string(), schema);
    }
    Some(entry_id)
}

/// The sentence a failed run leaves for the person, from the run's own failure event: the
/// gateway's words when it refused (the `error.message` inside its JSON body, when it carries
/// one — "no subscription credential for xai on this route" rather than the whole body), the
/// harness's otherwise; capped. `None` when the run did not fail.
pub(crate) fn failure_sentence(events: &[opengrok_wire::agui::Event]) -> Option<String> {
    let message = events
        .iter()
        .rev()
        .find(|event| event.event_type == opengrok_wire::agui::EventType::RunError)?
        .extra
        .get("message")
        .and_then(Value::as_str)?
        .trim();
    if message.is_empty() {
        return None;
    }
    let said = match message.find('{') {
        Some(at) => {
            let inner = serde_json::from_str::<Value>(&message[at..])
                .ok()
                .and_then(|body| {
                    body.pointer("/error/message")
                        .or_else(|| body.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            match inner {
                Some(inner) => format!("{} {inner}", message[..at].trim_end()),
                None => message.to_string(),
            }
        }
        None => message.to_string(),
    };
    let said = said.trim().trim_end_matches('.');
    let capped: String = said.chars().take(300).collect();
    Some(if capped.chars().count() < said.chars().count() {
        format!("{capped}…")
    } else {
        capped
    })
}

/// Whether a run is this agent's to answer: its own, or a run on this agent's own thread. The
/// second arm also matches a member's run inside a ROOM, which is how `in_a_room` below can tell
/// one apart — rooms are gone, but rows written before they went are not.
pub(crate) fn run_belongs_to(run: &opengrok_core::run::Run, agent: &CoworkerId) -> bool {
    run.coworker_id
        .as_ref()
        .is_some_and(|owner| owner.as_str() == agent.as_str())
        || run.thread_id == format!("gateway-{}", agent.as_str())
}

/// A run answered under an agent that is not its owner is a member's run inside that room.
pub(crate) fn in_a_room(run: &opengrok_core::run::Run, agent: &CoworkerId) -> bool {
    run.coworker_id
        .as_ref()
        .is_some_and(|owner| owner.as_str() != agent.as_str())
}

/// A resumed run continues where it lives — which, since the rooms went with seam A, is always
/// the coworker's own transcript.
///
/// A run that belongs to a ROOM is not resumed at all: it is left parked, with a line in the log
/// saying so. No surviving door can create a room, so the only runs that can take this branch are
/// rows written before the deletion, and the honest thing to do with one is to leave it alone
/// rather than replay a member's turn into a transcript nobody can read.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resume_where_it_lives(
    in_a_room: bool,
    state: HostState,
    account_id: opengrok_core::id::AccountId,
    run_id: RunId,
    coworker_id: CoworkerId,
    agent_id: String,
    pending: opengrok_core::run::PendingApproval,
    resumed_seq: u32,
    outcome: opengrok_harness::ResumeOutcome,
) {
    if in_a_room {
        tracing::warn!(
            run = %run_id,
            coworker = %coworker_id,
            "this run belongs to a room, and rooms were deleted with seam A; leaving it parked"
        );
        return;
    }
    resume_suspended_run(
        state,
        account_id,
        run_id,
        coworker_id,
        agent_id,
        pending,
        resumed_seq,
        outcome,
    )
    .await;
}

/// Resume an approved suspended run: re-run the conversation with the approved tool call (which
/// makes `user_machine_shell` dispatch instead of re-asking), then land the model's summary in
/// the transcript as an ordinary bot message.
#[allow(clippy::too_many_arguments)]
async fn resume_suspended_run(
    state: HostState,
    account_id: opengrok_core::id::AccountId,
    run_id: RunId,
    coworker_id: CoworkerId,
    agent_id: String,
    pending: opengrok_core::run::PendingApproval,
    resumed_seq: u32,
    outcome: opengrok_harness::ResumeOutcome,
) {
    let Ok((run, _)) = state.agui.auth.store.load_run(&run_id).await else {
        return;
    };
    let Ok((coworker, _)) = state.agui.auth.store.load_coworker(&coworker_id).await else {
        return;
    };
    // The runner carries the answered call id — as a GATE approval (the machine owner's or the
    // policy's card) or a REVIEW approval, by the suspension's reason. A review yes skips the
    // judge and releases nothing else; a gate yes is what makes user_machine_shell dispatch.
    let (gate_yes, review_yes): (&[String], &[String]) = match pending.reason {
        opengrok_core::run::SuspendReason::AutoReview => {
            (&[], std::slice::from_ref(&pending.call_id))
        }
        _ => (std::slice::from_ref(&pending.call_id), &[]),
    };
    let Some(runner) = crate::agui::routes::tools_for_coworker(
        &state.agui,
        &account_id,
        &coworker_id,
        gate_yes,
        review_yes,
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await
    else {
        return;
    };
    // A yes on a leave-box action is the person's consent to leave through the tunnel for the
    // rest of this run: one card per run, not one per click.
    let runner = runner.with_egress_consented(
        pending.reason == opengrok_core::run::SuspendReason::AutoReview
            && opengrok_tools::leaves_the_box(&pending.tool),
    );
    // The system message this turn OPENED with, not a fresh composition: a role or title edited
    // while the person was answering the card must not change the coworker halfway through the
    // turn. A run journalled before this was captured has none and composes one, as before.
    let system = match run.system_for_resume() {
        Some(captured) => captured,
        None => crate::persona::system_message(
            &coworker.name,
            &crate::persona::of(&state.agui, &coworker_id, coworker.role.clone()).await,
            None,
        ),
    };
    let journal = crate::agui::routes::StoreJournal {
        state: state.agui.clone(),
        thread_id: run.thread_id.clone(),
        account_id: Some(account_id.clone()),
        coworker_id: Some(coworker_id.clone()),
        model: run.model.clone(),
        system: Some(system.clone()),
    };
    let request = ModelRequest {
        gateway_key: crate::spend::key_for(&state.agui, &coworker_id, &account_id).await,
        spend_scope: Some(coworker_id.as_str().to_string()),
        // The person who answered the card is the person this continuation is for.
        spend_actor: Some(account_id.as_str().to_string()),
        model: run.pin_for_resume(&coworker.model),
        // A resumed run carries the SAME system message as the turn it continues. It used to
        // carry none at all, so a coworker lost both its identity and the whose-computer
        // discipline at the moment a person had just intervened — the worst possible moment to
        // start claiming work on the box happened on their machine.
        system: Some(system),
        messages: crate::agui::routes::conversation_from(&run),
        tools: Vec::new(),
    };

    let events = opengrok_harness::resume_conversation(
        state.agui.door.as_ref(),
        &runner,
        &journal,
        request,
        opengrok_harness::RunContext::new(&run.thread_id, run_id.as_str(), now_ms()),
        opengrok_harness::Resumption {
            approved: opengrok_tools::ToolCall {
                id: pending.call_id,
                name: pending.tool,
                arguments: pending.arguments,
            },
            message_seq: resumed_seq,
            outcome,
        },
    )
    .await;

    let mut text = String::new();
    for event in &events {
        if event.event_type == opengrok_wire::agui::EventType::TextMessageContent
            && let Some(delta) = event.extra.get("delta").and_then(Value::as_str)
        {
            text.push_str(delta);
        }
    }
    if text.is_empty()
        && let Some(why) = failure_sentence(&events)
    {
        text = format!("The turn failed: {why}");
    }
    if !text.is_empty() {
        // No reply link: the AG-UI door journals the person's message on the RUN, not into the
        // gateway transcript, so a resumed answer has no transcript row to point back at.
        let answer = answer_entry(&entry_id(), &text, None);
        if let Ok(_seq) = state
            .agui
            .auth
            .store
            .append_gateway_entry(&coworker_id, &account_id, &answer, now_ms())
            .await
        {
            tracing::debug!(coworker = %coworker_id, "a resumed turn's answer landed");
        }
    }
    // A resumed run may suspend AGAIN — a second command, or the next reviewed tool. It gets its
    // card exactly like the first turn did; without this the run paused with nothing to press.
    emit_suspensions(&state, &coworker_id, &account_id, &agent_id, &events).await;
}

#[cfg(test)]
mod stamp_tests {
    use super::apply_user_form_stamp;
    use serde_json::json;

    #[test]
    fn a_failed_append_does_not_stamp_entry_id() {
        let mut extra = serde_json::Map::new();
        extra.insert("name".into(), json!("run-awaiting-approval"));
        extra.insert("reason".into(), json!("user-form"));
        extra.insert(
            "arguments".into(),
            json!({ "title": "Sign in", "fields": [] }),
        );
        assert!(apply_user_form_stamp(&mut extra, "e_ghost".into(), false).is_none());
        assert!(
            extra.get("entryId").is_none(),
            "ghost entryId is the collapse blocker: {extra:?}"
        );
        assert_eq!(
            apply_user_form_stamp(&mut extra, "e_1".into(), true).as_deref(),
            Some("e_1")
        );
        assert_eq!(extra["entryId"], "e_1");
        assert_eq!(extra["formRequest"], extra["arguments"]);
    }
}
