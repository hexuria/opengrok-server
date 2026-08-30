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
    args.get("agentId")
        .and_then(Value::as_str)
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
pub async fn send_prompt(state: &GatewayState, args: &Value) -> (u16, Value) {
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
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(&state.email).await else {
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
        .accept_nonce(&state.email, &nonce, &digest, &record, now_ms())
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
pub async fn acceptance_status(state: &GatewayState, args: &Value) -> (u16, Value) {
    let Some(nonce) = args.get("clientNonce").and_then(Value::as_str) else {
        return (
            400,
            json!({ "error": "promptAcceptanceStatus needs a clientNonce" }),
        );
    };
    match state
        .agui
        .auth
        .store
        .nonce_record(&state.email, nonce)
        .await
    {
        Ok(Some(record)) => (200, json!({ "outcome": "found", "record": record })),
        Ok(None) => (200, json!({ "outcome": "not-found" })),
        Err(error) => {
            tracing::error!(%error, "the acceptance ledger is unreachable");
            (500, json!({ "error": "acceptance ledger unavailable" }))
        }
    }
}
