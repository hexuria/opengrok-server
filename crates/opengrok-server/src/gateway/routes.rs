//! The gateway's three surfaces: `/health`, `/events`, and `POST /api/{method}`.
//!
//! Reply mechanics the client depends on (`client-grok-bot.md` §2.0), all honoured here:
//! body is `JSON.stringify(result ?? null)`; every JSON reply carries `x-sand-mint-dedupe: 1`;
//! an error is `{"error": …}` where `< 500` means "the command failed, tell the user" and
//! `>= 500` means "the gateway is unreachable, retry"; an unknown method is a **404**; an empty
//! request body parses as `{}`.

use std::convert::Infallible;

use axum::Router;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use futures::stream;
use serde_json::{Value, json};

use super::{GatewayState, refuse, summaries};

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/events", get(events))
        .route("/api/{method}", post(command))
        .with_state(state)
}

/// A JSON reply, stamped the way the client checks: `x-sand-mint-dedupe: 1` on every one.
fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::HeaderName::from_static("x-sand-mint-dedupe"), "1"),
        ],
        body.to_string(),
    )
        .into_response()
}

fn refusal(code: u16, message: &str) -> Response {
    reply(
        StatusCode::from_u16(code).unwrap_or(StatusCode::FORBIDDEN),
        json!({ "error": message }),
    )
}

/// `GET /health` — the supervisor probes this on a 1500 ms deadline and only accepts
/// `ok === true`. The busy flag is real: it reports whether any run is live right now.
async fn health(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if let Some((code, message)) = refuse(&state, &headers) {
        return refusal(code, message);
    }
    let busy = state
        .agui
        .auth
        .store
        .running_runs()
        .await
        .map(|n| n > 0)
        .unwrap_or(false);
    reply(
        StatusCode::OK,
        json!({
            "ok": true,
            "pid": std::process::id(),
            "isBusy": busy,
            "activeAgentId": null,
            "startedAt": state.started_at_ms,
            "lastBusyAtMs": state.started_at_ms,
        }),
    )
}

#[derive(Debug, serde::Deserialize)]
struct EventsQuery {
    /// `?channels=a,b,c` — which channels this subscriber wants; absent means all of them.
    channels: Option<String>,
}

/// `GET /events` — the transport the client holds open for its whole life.
///
/// The framing is mandatory, not stylistic: `retry: 1000` first, then a `:ping` comment at
/// least every 15 s, because the client aborts the stream after 35 s of silence and reconnects
/// forever. Ten seconds leaves a full heartbeat of margin over a slow write. Frames are the
/// `{channel, payload}` envelope, filtered per-subscriber by the `channels` parameter.
async fn events(
    State(state): State<GatewayState>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some((code, message)) = refuse(&state, &headers) {
        return refusal(code, message);
    }

    let wanted: Option<std::collections::HashSet<String>> = query
        .channels
        .as_deref()
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| raw.split(',').map(|name| name.trim().to_string()).collect());
    let subscriber = state.events_tx.subscribe();

    // The opening: the mandatory retry line, then a complete roster snapshot so a reconnecting
    // client is seeded before its own listAgents lands.
    let snapshot = {
        let rows = super::live::roster_rows(&state).await.unwrap_or_default();
        let payload = json!({
            "activeAgentId": state.active_agent.lock().ok().and_then(|a| a.clone()),
            "agents": rows,
            "ordered": super::live::ordered(&state, "roster"),
            "coverage": { "kind": "complete-roster" },
        });
        frame("agents", &payload, wanted.as_ref())
    };
    let opening =
        stream::once(async move { Ok::<_, Infallible>(format!("retry: 1000\n\n{snapshot}")) });

    let live = stream::unfold(subscriber, |mut subscriber| async move {
        loop {
            match subscriber.recv().await {
                Ok((channel, payload)) => {
                    return Some((Ok::<_, Infallible>((channel, payload)), subscriber));
                }
                // Lagged: frames were dropped for this slow subscriber. Keep going — the client
                // resyncs from sequence gaps; that is what the ordered stamps are for.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let filter = wanted.clone();
    let live = live.map(move |result| {
        result.map(|(channel, payload)| frame(&channel, &payload, filter.as_ref()))
    });

    let pings = stream::unfold((), |()| async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        Some((Ok::<_, Infallible>(":ping\n\n".to_string()), ()))
    });

    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::body::Body::from_stream(opening.chain(futures::stream::select(live, pings))),
    )
        .into_response()
}

/// One SSE data frame in the `{channel, payload}` envelope — or nothing, when the subscriber
/// did not ask for the channel.
fn frame(
    channel: &str,
    payload: &Value,
    wanted: Option<&std::collections::HashSet<String>>,
) -> String {
    if let Some(wanted) = wanted
        && !wanted.contains(channel)
    {
        return String::new();
    }
    format!(
        "data: {}\n\n",
        json!({ "channel": channel, "payload": payload })
    )
}

/// `POST /api/{method}` — the command surface.
async fn command(
    State(state): State<GatewayState>,
    Path(method): Path<String>,
    RawQuery(_): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some((code, message)) = refuse(&state, &headers) {
        return refusal(code, message);
    }

    // An empty body is `{}` (`parseCommandArgs`); a malformed one is a command error, not a
    // gateway outage, so it answers < 500.
    let args: Value = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(_) => return refusal(400, "arguments are not JSON"),
        }
    };

    match method.as_str() {
        // ---- the roster ----
        "listAgents" => match roster(&state).await {
            Ok(rows) => reply(StatusCode::OK, Value::Array(rows)),
            Err(error) => {
                tracing::error!(%error, "listAgents could not read the roster");
                refusal(500, "roster unavailable")
            }
        },
        "countAgents" => match roster(&state).await {
            Ok(rows) => reply(StatusCode::OK, json!(rows.len())),
            Err(error) => {
                tracing::error!(%error, "countAgents could not read the roster");
                refusal(500, "roster unavailable")
            }
        },

        // ---- trays and feature gates: honest empties, correct container types ----
        "getTrays" => reply(StatusCode::OK, json!([])),
        "dismissTray" | "clearTrays" => reply(StatusCode::OK, Value::Null),
        "isAgentNetworkEnabled" | "isGlobalSearchEnabled" | "isEgressTunnelAvailable" => {
            reply(StatusCode::OK, json!(false))
        }

        // ---- host settings: set must echo the whole record, the resync chain reads it back ----
        "getHostSettings" => reply(StatusCode::OK, settings_snapshot(&state)),
        "setHostSettings" => {
            if let Ok(mut settings) = state.settings.lock()
                && let (Some(record), Some(patch)) = (settings.as_object_mut(), args.as_object())
            {
                for (key, value) in patch {
                    record.insert(key.clone(), value.clone());
                }
            }
            reply(StatusCode::OK, settings_snapshot(&state))
        }

        // ---- the resync chain's remaining steps: answered, honestly empty ----
        "getBoxSecretsStatus" => reply(StatusCode::OK, json!({})),
        "setBoxSecrets" | "setWindowFocused" => reply(StatusCode::OK, Value::Null),

        // ---- computer status: null is the well-formed "no forever box" answer ----
        "getForeverBoxStatus" => reply(StatusCode::OK, Value::Null),
        "getHostStatus" => reply(
            StatusCode::OK,
            json!({
                "isBusy": false,
                // Transcribed from `host-gateway-api.ts:8-11`.
                "capabilities": ["orderedReplicasV1", "sendAcceptanceV1"],
            }),
        ),

        // ---- P4: one conversation (slice 8) ----
        "sendPrompt" => {
            let (code, body) = super::conversation::send_prompt(&state, &args).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "promptAcceptanceStatus" => {
            let (code, body) = super::conversation::acceptance_status(&state, &args).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        // Tails and pages: the same read, dressed four ways. `open*` also marks the agent active.
        "openAgentTail" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, true, false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getAgentTranscriptTail" | "getAgentTranscriptPage" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, false, false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "openAgentWindowed" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, true, true).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getAgentTranscriptWindow" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, false, true).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getAgentTranscript" => {
            let (code, body) = super::conversation::full_transcript(&state, &args, false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getTranscript" => {
            let (code, body) =
                super::conversation::full_transcript(&state, &json!({}), false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "openAgent" => {
            let (code, body) = super::conversation::full_transcript(&state, &args, true).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        // No branching yet: a thread is its root, and the outline is empty. Honest empties in the
        // container types the renderer validates.
        "getAgentThread" => reply(StatusCode::OK, json!({ "entries": [] })),
        "getConversationOutline" => reply(StatusCode::OK, json!([])),

        // ---- everything else, exactly as the shipped host words it ----
        other => refusal(404, &format!("unknown gateway method: {other}")),
    }
}

fn settings_snapshot(state: &GatewayState) -> Value {
    state
        .settings
        .lock()
        .map(|settings| settings.clone())
        .unwrap_or_else(|_| super::default_settings())
}

/// The roster: the gateway account's coworkers, newest first, as §8.1 rows.
async fn roster(state: &GatewayState) -> Result<Vec<Value>, opengrok_store::StoreError> {
    let Some(account) = state.agui.auth.store.account_by_email(&state.email).await? else {
        // Nobody has signed in as the gateway's person yet: an empty roster, not an error —
        // the client shows onboarding, which is the truthful screen.
        return Ok(Vec::new());
    };
    let coworkers = state.agui.auth.store.coworkers_for(&account.id).await?;
    Ok(coworkers
        .iter()
        .filter(|view| !view.retired)
        .map(summaries::summary)
        .collect())
}
