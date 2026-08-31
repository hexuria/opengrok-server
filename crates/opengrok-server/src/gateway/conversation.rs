//! P4: one conversation, from the real client's point of view.
//!
//! `sendPrompt` answers `{accepted: true}` the moment the turn is DURABLY accepted — never when
//! it completes — because the client only retries a send against an endpoint that has proven
//! nonce dedupe, and a slow answer here is a stuck composer there. The idempotency ledger is
//! Postgres, not memory: a repeated nonce with the same input digest is answered `accepted`
//! again without running anything, and the same nonce with DIFFERENT input is refused loudly
//! (`NONCE_DIGEST_MISMATCH`) — silently absorbing it would send a message the user rewrote.
//!
//! The turn itself is the harness we already have: the coworker's own model through
//! `run_conversation`, journaled like every other run. The transcript entries here are the
//! client-shaped projection of that turn, and the choreography — user echo with `clientNonce`,
//! streaming placeholder, final update, roster pulses — is `client-grok-bot.md` §2.4 step for
//! step.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use opengrok_core::id::{CoworkerId, RunId};
use opengrok_harness::{ChatMessage, ModelRequest, run_conversation};

use super::{GatewayState, live};
use crate::agui::routes::StoreJournal;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn entry_id() -> String {
    format!("e_{}", uuid::Uuid::now_v7())
}

/// The named agent, or the one the client last opened.
pub fn agent_or_active(state: &GatewayState, args: &Value) -> Option<String> {
    // The client names the agent under different keys on different verbs: `agentId` on sendPrompt,
    // but the transcript reads (getAgentTranscriptTail / openAgentTail / getAgentThread) pass it as
    // `id` or `threadId`. Accept them all before falling back to the last-opened agent — otherwise a
    // read that DID name its agent was answered "no agent named and none active" and the reply could
    // never paint.
    ["agentId", "id", "threadId", "agent_id"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            state
                .active_agent
                .lock()
                .ok()
                .and_then(|active| active.clone())
        })
}

/// The input digest the ledger compares. Mirrors the client's field list
/// (`prompt-acceptance-ledger.ts:34-49`): agentId, prompt, richText, replyToId, isFork, and the
/// attachment name/path arrays.
pub fn input_digest(args: &Value, agent_id: &str) -> String {
    let mut hasher = Sha256::new();
    let mut take = |value: &str| {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    };
    take(agent_id);
    take(
        args.get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    take(
        args.get("richText")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    take(
        args.get("replyToId")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    take(
        if args.get("isFork").and_then(Value::as_bool).unwrap_or(false) {
            "1"
        } else {
            "0"
        },
    );
    for key in ["attachmentPaths", "attachmentNames"] {
        if let Some(items) = args.get(key).and_then(Value::as_array) {
            for item in items {
                take(item.as_str().unwrap_or_default());
            }
        }
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Accept, echo, and run — the whole sendPrompt path after auth.
///
/// Returns `(status, body)` so `routes.rs` can wrap it in the wire mechanics.
pub async fn send_prompt(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(prompt) = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return (400, json!({ "error": "sendPrompt needs a prompt" }));
    };
    let Some(nonce) = args
        .get("clientNonce")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        // Required for retry-safety; a send with no nonce cannot be deduped and must not be
        // accepted as if it could be.
        return (400, json!({ "error": "sendPrompt needs a clientNonce" }));
    };
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };

    let coworker_id = CoworkerId::from_stored(agent_id.clone());
    let Ok((coworker, _)) = state.agui.auth.store.load_coworker(&coworker_id).await else {
        return (404, json!({ "error": format!("no agent {agent_id}") }));
    };
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (
            500,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };

    // The ledger speaks first. A duplicate is answered `accepted` with nothing re-run; a nonce
    // reused for different input is the one refusal that must never be quiet.
    let digest = input_digest(args, &agent_id);
    let record = json!({
        "clientNonce": nonce,
        "agentId": agent_id,
        "acceptedAtMs": now_ms(),
    });
    match state
        .agui
        .auth
        .store
        .accept_nonce(caller, &nonce, &digest, &record, now_ms())
        .await
    {
        Ok(Ok(stored)) => {
            if stored.get("acceptedAtMs") != record.get("acceptedAtMs") {
                // The earlier acceptance stands; the turn it accepted is already running or done.
                return (200, json!({ "accepted": true }));
            }
        }
        Ok(Err(())) => {
            return (409, json!({ "error": "NONCE_DIGEST_MISMATCH" }));
        }
        Err(error) => {
            tracing::error!(%error, "the acceptance ledger is unreachable");
            return (500, json!({ "error": "acceptance ledger unavailable" }));
        }
    }

    // The user's own message, durably in the transcript and echoed over SSE carrying the
    // clientNonce — that echo is what settles the optimistic bubble in the renderer.
    let user_entry = json!({
        "kind": "message",
        "id": entry_id(),
        "role": "user",
        "content": prompt,
        "isStreaming": false,
        "timestampMs": now_ms(),
        "clientNonce": nonce,
    });
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(&coworker_id, &user_entry, now_ms())
        .await
    {
        tracing::error!(%error, "could not append the user's message");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    live::emit_transcript(state, &agent_id, "appended", user_entry.clone());

    // The streaming placeholder the answer will grow into.
    let answer_id = entry_id();
    let placeholder = json!({
        "kind": "send-message",
        "id": answer_id,
        "message": { "type": "text", "content": "" },
        "timestampMs": now_ms(),
        "streaming": true,
    });
    let answer_seq = match state
        .agui
        .auth
        .store
        .append_gateway_entry(&coworker_id, &placeholder, now_ms())
        .await
    {
        Ok(seq) => seq,
        Err(error) => {
            tracing::error!(%error, "could not append the answer placeholder");
            return (500, json!({ "error": "transcript unavailable" }));
        }
    };
    live::emit_transcript(state, &agent_id, "appended", placeholder);

    live::set_running(state, &agent_id, true, json!({})).await;

    // The turn, off this request's clock. `accepted` means accepted, not answered.
    let task_state = state.clone();
    let history = history_for(state, &coworker_id).await;
    tokio::spawn(run_turn(
        task_state,
        account.id,
        coworker_id,
        coworker.model.clone(),
        history,
        answer_id,
        answer_seq,
    ));

    (200, json!({ "accepted": true }))
}

/// `getForeverBoxStatus` — the caller's agent's LIVE box health, in the client's `BoxStatus` shape
/// (`{agentId, state, vncUrl}`). This is the signal that stops the app spinning "Booting up the
/// computer" forever: a running box says `running`, a released one `absent`, and a dead one its real
/// word (`exited`/`stopped`/…), so a box that died says it died instead of pretending to boot.
///
/// `vncUrl` is ALWAYS null: our boxes are headless (shell + files, no screen). The client renders a
/// running-but-headless box honestly on its side; we never invent a screen URL that does not exist.
pub async fn box_status(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    use crate::agui::provision;

    let Some(agent_id) = agent_or_active(state, args) else {
        // No agent named and none active — nothing to report. Null is a shape the client accepts.
        return (200, Value::Null);
    };
    let absent = || json!({ "agentId": agent_id, "state": "absent", "vncUrl": Value::Null });
    // A box is reported per SCOPE (per-account shares one box across the account's agents), so
    // resolve the scope's box for the caller — but only for an agent that actually exists. An
    // unknown id replays to a default (empty-named) coworker; that is no coworker, so `absent`,
    // rather than borrowing the account's shared box.
    match state
        .agui
        .auth
        .store
        .load_coworker(&CoworkerId::from_stored(agent_id.clone()))
        .await
    {
        Ok((coworker, _)) if !coworker.name.is_empty() => {}
        _ => return (200, absent()),
    }
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (200, absent());
    };
    let (mode, org_id) = provision::resolve_mode(&state.agui, &account.id).await;
    let (scope, scope_id, _) =
        provision::scope_for(&mode, account.id.as_str(), org_id.as_deref(), &agent_id);
    let Ok(Some((box_id, kind, stopped))) = state
        .agui
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
    else {
        return (200, absent());
    };

    // A box we ourselves paused (idle-stop) is authoritatively "stopped" — no need to probe, and it
    // avoids a race where the provider has not yet reflected the stop. Otherwise ask the provider for
    // the box's real word; a provider we cannot build (e.g. the org key was cleared) means the box is
    // effectively unreachable, which reads as "absent".
    let live_state = if stopped {
        "stopped".to_string()
    } else {
        match provision::provider_for(&state.agui, org_id.as_deref(), &kind).await {
            Some(provider) => provider
                .state(&box_id)
                .await
                .unwrap_or_else(|_| "unknown".to_string()),
            None => "absent".to_string(),
        }
    };

    (
        200,
        json!({ "agentId": agent_id, "state": live_state, "vncUrl": Value::Null }),
    )
}

/// What `box_control` should do to the caller's agent's box.
pub enum BoxAction {
    /// `ensureForeverBox` — make the box RUNNING: resume an idle/stopped one in place, or provision
    /// and assign a fresh one when there is none.
    Ensure,
    /// `handBackForeverBox` — release the box: stop it (disk kept, billing paused). Recoverable by
    /// Ensure or the next message.
    HandBack,
    /// `resetForeverBox` — destroy the box and provision a fresh one in its place.
    Reset,
}

/// The box-control verbs, for real — no stub that lies "running". ensure/handBack/reset ACT on the
/// caller's agent's box and answer with its resulting live `BoxStatus`. A box brought up in place
/// keeps the same id, so the coworker's executor binding stays valid; provisioning a NEW box (Ensure
/// from absent, or Reset) re-assigns the coworker so the binding follows. `updateForeverBox` has no
/// image-update mechanism for our boxes, so it reports the current status honestly rather than
/// claiming a change that did not happen — its handler simply calls `box_status`.
pub async fn box_control(
    state: &GatewayState,
    args: &Value,
    caller: &str,
    action: BoxAction,
) -> (u16, Value) {
    use crate::agui::provision;

    let Some(agent_id) = agent_or_active(state, args) else {
        return (200, Value::Null);
    };
    let coworker_id = CoworkerId::from_stored(agent_id.clone());
    let error_status = |code: String, message: String| {
        json!({
            "agentId": agent_id,
            "state": "absent",
            "vncUrl": Value::Null,
            "computerError": { "code": code, "message": message },
        })
    };
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (
            200,
            error_status("unknown".into(), "no such account".into()),
        );
    };
    let (mode, org_id) = provision::resolve_mode(&state.agui, &account.id).await;
    let (scope, scope_id, _) =
        provision::scope_for(&mode, account.id.as_str(), org_id.as_deref(), &agent_id);
    let existing = state
        .agui
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
        .ok()
        .flatten();

    match action {
        BoxAction::HandBack => {
            if let Some((box_id, kind, _)) = &existing
                && let Some(provider) =
                    provision::provider_for(&state.agui, org_id.as_deref(), kind).await
            {
                let _ = provider.stop(box_id).await;
                let _ = state
                    .agui
                    .auth
                    .store
                    .mark_scoped_stopped(scope, &scope_id)
                    .await;
            }
        }
        BoxAction::Ensure => match &existing {
            Some((box_id, kind, stopped)) => {
                if let Some(provider) =
                    provision::provider_for(&state.agui, org_id.as_deref(), kind).await
                {
                    let running = provider
                        .state(box_id)
                        .await
                        .map(|state| state == "running")
                        .unwrap_or(false);
                    if *stopped || !running {
                        let _ = provider.resume(box_id).await;
                        let _ = state
                            .agui
                            .auth
                            .store
                            .mark_scoped_used(scope, &scope_id, now_ms())
                            .await;
                    }
                }
            }
            None => {
                if let Err((code, message)) = reprovision(state, &account.id, &coworker_id).await {
                    return (200, error_status(code, message));
                }
            }
        },
        BoxAction::Reset => {
            if let Some((box_id, kind, _)) = &existing
                && let Some(provider) =
                    provision::provider_for(&state.agui, org_id.as_deref(), kind).await
            {
                let _ = provider.destroy(box_id).await;
            }
            let _ = state
                .agui
                .auth
                .store
                .clear_scoped_computer(scope, &scope_id)
                .await;
            if let Err((code, message)) = reprovision(state, &account.id, &coworker_id).await {
                return (200, error_status(code, message));
            }
        }
    }

    // Answer with the box's real resulting state.
    box_status(state, args, caller).await
}

/// Provision (or re-provision) a coworker's box and PERSIST the re-assignment to its aggregate, so
/// the executor — which binds `coworker.computer()`, not the scope mapping — points at the new box.
/// Returns the provisioning error as `(code, message)` if it could not get a box.
async fn reprovision(
    state: &GatewayState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
) -> Result<(), (String, String)> {
    use opengrok_core::coworker::CoworkerView;

    let Ok((mut coworker, seq)) = state.agui.auth.store.load_coworker(coworker_id).await else {
        return Err(("unknown".into(), "could not load the coworker".into()));
    };
    let at_ms = now_ms();
    let provisioned = crate::agui::provision::ensure_computer_for(
        &state.agui,
        account_id,
        coworker_id,
        &mut coworker,
        at_ms,
    )
    .await;
    if let Some(error) = provisioned.error {
        return Err(error);
    }
    let view = CoworkerView {
        id: coworker_id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: false,
        updated_at_ms: at_ms,
    };
    let _ = state
        .agui
        .auth
        .store
        .append_coworker(coworker_id, account_id, seq, &provisioned.events, &view)
        .await;
    Ok(())
}

/// The transcript so far, as chat messages — so a coworker remembers its own conversation
/// rather than greeting every message as its first.
pub(crate) async fn history_for(state: &GatewayState, coworker: &CoworkerId) -> Vec<ChatMessage> {
    let Ok(entries) = state.agui.auth.store.gateway_transcript(coworker).await else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry.get("kind").and_then(Value::as_str) {
            Some("message") => Some(ChatMessage {
                role: entry
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_string(),
                content: entry
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            Some("send-message") => {
                let content = entry
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if content.is_empty() {
                    None
                } else {
                    Some(ChatMessage {
                        role: "assistant".to_string(),
                        content: content.to_string(),
                    })
                }
            }
            _ => None,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn(
    state: GatewayState,
    account_id: opengrok_core::id::AccountId,
    coworker_id: CoworkerId,
    model: String,
    messages: Vec<ChatMessage>,
    answer_id: String,
    answer_seq: i64,
) {
    let agent_id = coworker_id.as_str().to_string();
    let run_id = RunId::new();
    let thread_id = format!("gateway-{agent_id}");

    let tools =
        crate::agui::routes::tools_for_coworker(&state.agui, &account_id, &coworker_id).await;
    let journal = StoreJournal {
        state: state.agui.clone(),
        thread_id: thread_id.clone(),
        account_id: Some(account_id),
        coworker_id: Some(coworker_id.clone()),
    };
    let request = ModelRequest {
        model,
        system: None,
        messages,
    };

    let _lease =
        crate::recovery::Lease::new(crate::recovery::hold(state.agui.clone(), run_id.clone()));
    let events = run_conversation(
        state.agui.door.as_ref(),
        tools.as_ref(),
        &journal,
        request,
        &thread_id,
        run_id.as_str(),
        now_ms(),
    )
    .await;

    // The answer is whatever the run's message deltas add up to; a run that produced nothing
    // still ends its bubble, with the failure said out loud rather than a spinner forever.
    let mut text = String::new();
    for event in &events {
        // Only message content: TOOL_CALL_ARGS frames carry a `delta` too, and gluing tool
        // arguments into the visible answer would be nonsense the user reads.
        if event.event_type == opengrok_wire::agui::EventType::TextMessageContent
            && let Some(delta) = event.extra.get("delta").and_then(Value::as_str)
        {
            text.push_str(delta);
        }
    }
    if text.is_empty() {
        text = "The turn produced no answer. Its run log has the reason.".to_string();
    }

    let final_entry = json!({
        "kind": "send-message",
        "id": answer_id,
        "message": { "type": "text", "content": text },
        "timestampMs": now_ms(),
    });
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker_id, answer_seq, &final_entry)
        .await
    {
        tracing::error!(%error, "could not finalise the answer entry");
    }
    live::emit_transcript(&state, &agent_id, "updated", final_entry);

    let preview: String = text.chars().take(120).collect();
    live::set_running(
        &state,
        &agent_id,
        false,
        json!({
            "lastMessagePreview": preview,
            "lastEntry": { "kind": "text", "text": preview },
        }),
    )
    .await;
}

/// `openAgentTail` / `getAgentTranscriptTail` / windows / pages — every read is the same tail
/// with different dressing.
pub async fn transcript_reply(
    state: &GatewayState,
    args: &Value,
    activate: bool,
    with_thread_counts: bool,
) -> (u16, Value) {
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };
    let coworker = CoworkerId::from_stored(agent_id.clone());
    let before = args.get("beforeSeq").and_then(Value::as_i64);
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(200)
        .clamp(1, 1000);

    let (entries, next_before) = match state
        .agui
        .auth
        .store
        .gateway_tail(&coworker, before, limit)
        .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "could not read a transcript tail");
            return (500, json!({ "error": "transcript unavailable" }));
        }
    };

    if activate {
        if let Ok(mut active) = state.active_agent.lock() {
            *active = Some(agent_id.clone());
        }
        // Opening an agent is a roster fact (the active pip moved), and §8.3 allows the
        // escalation to a full emit.
        live::emit_roster(state).await;
    }

    let mut reply = json!({ "entries": entries });
    if let Some(next) = next_before {
        reply["nextBeforeSeq"] = json!(next);
    }
    if with_thread_counts {
        // Required by the window validator even when no entry is threaded: absent means the
        // whole reply is malformed and the call rejects.
        reply["threadCounts"] = json!({});
    }
    (200, reply)
}

/// `getAgentTranscript` / `getTranscript` / `openAgent` — the unbounded array forms.
pub async fn full_transcript(state: &GatewayState, args: &Value, activate: bool) -> (u16, Value) {
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };
    let coworker = CoworkerId::from_stored(agent_id.clone());
    match state.agui.auth.store.gateway_transcript(&coworker).await {
        Ok(entries) => {
            if activate {
                if let Ok(mut active) = state.active_agent.lock() {
                    *active = Some(agent_id);
                }
                live::emit_roster(state).await;
            }
            (200, Value::Array(entries))
        }
        Err(error) => {
            tracing::error!(%error, "could not read a transcript");
            (500, json!({ "error": "transcript unavailable" }))
        }
    }
}

/// `promptAcceptanceStatus` — what the ledger remembers about a nonce.
pub async fn acceptance_status(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(nonce) = args.get("clientNonce").and_then(Value::as_str) else {
        return (
            400,
            json!({ "error": "promptAcceptanceStatus needs a clientNonce" }),
        );
    };
    match state.agui.auth.store.nonce_record(caller, nonce).await {
        Ok(Some(record)) => (200, json!({ "outcome": "found", "record": record })),
        Ok(None) => (200, json!({ "outcome": "not-found" })),
        Err(error) => {
            tracing::error!(%error, "the acceptance ledger is unreachable");
            (500, json!({ "error": "acceptance ledger unavailable" }))
        }
    }
}
