//! Seam B's send: `SendGrokBotUserMessage`, riding the same turn machinery as the gateway.
//!
//! The mock's contract (`grok-bot-handlers.ts sendGrokBotUserMessage`): the user's text lands in
//! the transcript, an assistant reply follows, the send is recorded so `GetGrokBotSendStatus`
//! can answer, and the response says `dispatched` with a BOX delivery. Ours differs from the
//! mock in exactly one way, deliberately: the reply is a real turn on the coworker's own model
//! instead of a canned line — the mock proves the shape, the server does the work.
//!
//! Idempotency keys on `(account, agentId:messageId)` in the same ledger the gateway uses —
//! the client retries sends, and a retried send must not run the turn twice.

use serde_json::{Value, json};

use opengrok_core::id::{AccountId, CoworkerId};

use crate::gateway::GatewayState;
use crate::gateway::conversation;
use crate::gateway::live;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn entry_id() -> String {
    format!("e_{}", uuid::Uuid::now_v7())
}

fn connect_ok(body: Value) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn connect_error(
    status: axum::http::StatusCode,
    code: &str,
    message: &str,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({ "code": code, "message": message }).to_string(),
    )
        .into_response()
}

fn accepted_response(message_id: &str) -> axum::response::Response {
    connect_ok(json!({
        // Enum names transcribed from grok_bot_pb.ts.
        "dispatched": true,
        "mode": "GROK_BOT_TEMPORAL_HARNESS_MODE_BOX",
        "delivery": "GROK_BOT_USER_MESSAGE_DELIVERY_ACCEPTED_BOX",
        "workflowId": format!("og-{message_id}"),
    }))
}

pub async fn send(
    state: &GatewayState,
    account_id: &AccountId,
    args: &Value,
) -> axum::response::Response {
    let Some(agent) = args
        .get("agentId")
        .and_then(Value::as_str)
        .filter(|agent| !agent.is_empty())
    else {
        return connect_error(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_argument",
            "agentId is required",
        );
    };
    let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
    let message_id = args
        .get("messageId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(entry_id);

    let coworker_id = CoworkerId::from_stored(agent.to_string());
    let Ok((coworker, _)) = state.agui.auth.store.load_coworker(&coworker_id).await else {
        return connect_error(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            "no such agent",
        );
    };

    // The ledger first: a retried send answers accepted and runs nothing.
    let slot = format!("seamb:{}", account_id.as_str());
    let nonce = format!("{agent}:{message_id}");
    let digest = conversation::input_digest(&json!({ "prompt": text }), agent);
    let user_id = message_id.clone();
    let record = json!({
        "agentId": agent,
        "messageId": message_id,
        "echoEntryId": user_id,
        "acceptedAtMs": now_ms(),
    });
    match state
        .agui
        .auth
        .store
        .accept_nonce(&slot, &nonce, &digest, &record, now_ms())
        .await
    {
        Ok(Ok(stored)) => {
            if stored.get("acceptedAtMs") != record.get("acceptedAtMs") {
                return accepted_response(&message_id);
            }
        }
        Ok(Err(())) => {
            // Same messageId, different text: the mock never sees this (the client keys retries
            // correctly), but absorbing it would send words the user rewrote.
            return connect_error(
                axum::http::StatusCode::CONFLICT,
                "aborted",
                "messageId was reused with different input",
            );
        }
        Err(error) => {
            tracing::error!(%error, "seam B acceptance ledger unreachable");
            return connect_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "ledger unavailable",
            );
        }
    }

    // The user's message, durably — the id the client sent IS the entry id, so the echo the
    // client looks for by `echoEntryId` is findable.
    let user_entry = json!({
        "kind": "message",
        "id": user_id,
        "role": "user",
        "content": text,
        "isStreaming": false,
        "timestampMs": now_ms(),
    });
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(&coworker_id, account_id, &user_entry, now_ms())
        .await
    {
        tracing::error!(%error, "seam B could not append the user's message");
        return connect_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "transcript unavailable",
        );
    }
    live::emit_transcript(state, agent, "appended", user_entry);

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
        .append_gateway_entry(&coworker_id, account_id, &placeholder, now_ms())
        .await
    {
        Ok(seq) => seq,
        Err(error) => {
            tracing::error!(%error, "seam B could not append the answer placeholder");
            return connect_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "transcript unavailable",
            );
        }
    };
    live::emit_transcript(state, agent, "appended", placeholder);
    live::set_running(state, agent, true, json!({})).await;

    let history = conversation::history_for(state, &coworker_id, account_id).await;
    tokio::spawn(conversation::run_turn(
        state.clone(),
        account_id.clone(),
        coworker_id,
        coworker.model.clone(),
        history,
        conversation::Answer {
            id: answer_id,
            seq: answer_seq,
            reply_to: None,
        },
    ));

    accepted_response(&message_id)
}
