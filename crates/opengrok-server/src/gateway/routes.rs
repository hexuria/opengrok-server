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
use opengrok_core::id::CoworkerId;
use serde_json::{Value, json};

use super::{GatewayState, refuse};

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/events", get(events))
        .route("/api/{method}", post(command))
        .route("/avatars/{id}", get(avatar_bytes))
        .with_state(state)
}

/// `GET /avatars/<id>[?v=]` — the bytes behind a slim roster's `avatarVersion`. Raw image bytes,
/// not JSON; 404 when the coworker has no stored avatar.
async fn avatar_bytes(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    use base64::Engine as _;
    if let Some((code, message)) = refuse(&state, &headers) {
        return refusal(code, message);
    }
    let profile = state
        .agui
        .auth
        .store
        .seamb_profile(&opengrok_core::id::CoworkerId::from_stored(id))
        .await
        .ok()
        .flatten();
    let Some(data_url) = profile
        .as_ref()
        .and_then(|profile| profile.get("avatarDataUrl"))
        .and_then(Value::as_str)
    else {
        return (StatusCode::NOT_FOUND, "no avatar").into_response();
    };
    let Some(encoded) = data_url.strip_prefix("data:image/png;base64,") else {
        return (StatusCode::NOT_FOUND, "no avatar").into_response();
    };
    match base64::engine::general_purpose::STANDARD.decode(encoded) {
        Ok(bytes) => (StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "no avatar").into_response(),
    }
}

/// The host's own words when sharing is off for the account (`cross-user-sharing/
/// extension.ts:15`). The renderer shows it as the error of the room or invite it asked for.
const SHARING_DISABLED_MESSAGE: &str = "Sharing isn't enabled for your account.";

/// `EMPTY_SAND_SHARING_STATE` (`shared/agents/sharing.ts:43`), the state the host emits when
/// the multiplayer gate is off — which it is, on this server, for everybody.
fn sharing_disabled_state() -> Value {
    json!({
        "isEnabled": false,
        "selfAuthId": Value::Null,
        "pendingJoinRequests": [],
        "rooms": [],
        "typingUsers": [],
    })
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
///
/// UNAUTHENTICATED, on purpose. The client's reachability probe (`host-supervisor.ts`
/// `fetchHealth`) sends no Authorization header — upstream's gateway serves health openly, and a
/// liveness endpoint whose whole job is to answer "I am up" must be probeable without a token, or a
/// bearered host looks permanently unreachable the instant the SSE stream drops. It reveals only
/// that the server is up (plus pid/busy/startedAt — nothing secret), so it sits OUTSIDE `refuse`
/// while every driving surface (`/api`, `/events`) stays behind it.
///
/// Token-free is not origin-free. The supervisor sends no Authorization AND no Origin (it is
/// Electron main, not a page); a browser page that learned this host still gets nothing — not
/// even "up" — which is the rule the gateway smoke asserts for every path. Dropping `refuse` for
/// the bearer took the Origin block with it by accident; this keeps only the half that was meant.
async fn health(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if headers.get(axum::http::header::ORIGIN).is_some() {
        return refusal(403, "browser origins are not served");
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
    // Connect and disconnect are logged with the request id and the live subscriber count, so
    // "was the stream up at 03:16, and for whom" is one grep. The guard rides in the stream's
    // state and logs when the body is dropped — which is how a client going away shows up here;
    // there is no other signal.
    let guard = StreamGuard {
        id: crate::request_id(&headers),
        channels: query.channels.clone().unwrap_or_default(),
        tx: state.events_tx.clone(),
        opened: std::time::Instant::now(),
    };
    tracing::info!(
        id = %guard.id,
        channels = %guard.channels,
        subscribers = state.events_tx.receiver_count(),
        "events: stream opened"
    );

    // The opening: the mandatory retry line, then a complete roster snapshot so a reconnecting
    // client is seeded before its own listAgents lands. It carries the CURRENT roster sequence,
    // not a fresh one: this frame goes to this subscriber alone, and a sequence minted here was a
    // gap for every other open stream (they saw N, N+2 and resynced the roster after every send).
    // The replica installs a snapshot as its baseline at `throughSequence = ordered.sequence` and
    // ignores one older than what it has, so "current" is exactly right for the opener and
    // invisible to everyone else. `coverage.kind` stays `complete-roster`: it is what the replica
    // checks first.
    //
    // NEVER A ROSTER THE SERVER DID NOT READ. With the database down the read fails, and a
    // failure turned into "zero rows, complete, current" told every reconnecting desktop that
    // it has no coworkers — it installed the empty snapshot as its baseline and dropped the
    // roster it was showing (timed twice on 2 Sep 2026: the page painted its cache within a
    // second of a reload and lost it the instant the stream reconnected). An empty success is
    // the dangerous reply (CLAUDE.md #3): when the read fails the opener is the retry line alone,
    // said in the log, and the client's own listAgents — which answers 500, not an empty array —
    // is what it acts on.
    //
    // And a client that connected DURING the outage would otherwise never be seeded: nothing is
    // emitted while the store is down (every emitter reads first and gives up), and when the
    // store returns no frame says so — the page would sit on the retry line, refusing every
    // unstamped roster, until the stream happened to reconnect. So a failed opener keeps trying
    // for this stream alone (1 s, 2 s, 4 s, then every 5 s) and sends the snapshot the moment a
    // read succeeds, stamped `current` like the opener's own. The task ends with the stream: it
    // checks the channel before every read and the receiver is dropped with the body.
    let (late_tx, late_rx) = tokio::sync::mpsc::channel::<String>(1);
    let snapshot = match super::live::roster_rows(&state).await {
        Ok(rows) => {
            let payload = json!({
                "activeAgentId": state.active_agent.lock().ok().and_then(|a| a.clone()),
                "agents": rows,
                "ordered": super::live::current(&state, "roster"),
                "coverage": { "kind": "complete-roster" },
            });
            frame("agents", &payload, wanted.as_ref())
        }
        Err(error) => {
            tracing::error!(
                id = %guard.id,
                %error,
                "events: the roster could not be read; opening without a snapshot rather than with an empty one"
            );
            let retry_state = state.clone();
            let retry_wanted = wanted.clone();
            let id = guard.id.clone();
            tokio::spawn(async move {
                let mut wait_secs = 1u64;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                    if late_tx.is_closed() {
                        return;
                    }
                    match super::live::roster_rows(&retry_state).await {
                        Ok(rows) => {
                            let payload = json!({
                                "activeAgentId": retry_state.active_agent.lock().ok().and_then(|a| a.clone()),
                                "agents": rows,
                                "ordered": super::live::current(&retry_state, "roster"),
                                "coverage": { "kind": "complete-roster" },
                            });
                            tracing::info!(
                                id = %id,
                                rows = payload["agents"].as_array().map_or(0, Vec::len),
                                "events: the roster read recovered; late snapshot sent"
                            );
                            let _ = late_tx
                                .send(frame("agents", &payload, retry_wanted.as_ref()))
                                .await;
                            return;
                        }
                        Err(_) => wait_secs = (wait_secs * 2).min(5),
                    }
                }
            });
            String::new()
        }
    };
    let opening =
        stream::once(async move { Ok::<_, Infallible>(format!("retry: 1000\n\n{snapshot}")) });
    // Ends when the retry task sends or the sender drops — at once on the Ok path above.
    let late = stream::unfold(late_rx, |mut late_rx| async move {
        late_rx
            .recv()
            .await
            .map(|text| (Ok::<_, Infallible>(text), late_rx))
    });

    // State is `(subscriber, guard)` in that order on purpose: tuple fields drop in order, so the
    // receiver is gone before the guard counts what is left.
    let live = stream::unfold((subscriber, guard), |(mut subscriber, guard)| async move {
        loop {
            match subscriber.recv().await {
                Ok((channel, payload)) => {
                    return Some((Ok::<_, Infallible>((channel, payload)), (subscriber, guard)));
                }
                // Lagged: frames were dropped for this slow subscriber. Keep going — the client
                // resyncs from the sequence gap; that is what the ordered stamps are for. Said
                // out loud, because a silent drop looks exactly like "the server never sent the
                // reply". The channel cannot say which replica keys the lost frames carried.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    tracing::warn!(
                        id = %guard.id,
                        dropped,
                        "events: subscriber lagged; frames dropped, the client will see a gap"
                    );
                    continue;
                }
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
        axum::body::Body::from_stream(opening.chain(futures::stream::select(
            futures::stream::select(live, pings),
            late,
        ))),
    )
        .into_response()
}

/// Logs the close of one `/events` stream when the body it rides in is dropped.
struct StreamGuard {
    id: String,
    channels: String,
    tx: tokio::sync::broadcast::Sender<(String, Value)>,
    opened: std::time::Instant,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        tracing::info!(
            id = %self.id,
            channels = %self.channels,
            subscribers = self.tx.receiver_count(),
            open_secs = self.opened.elapsed().as_secs(),
            "events: stream closed"
        );
    }
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

    // The per-account pivot: whose account this call is FOR. The caller's own account when it sends
    // a valid account token in `ACCOUNT_HEADER`, else the `OG_GATEWAY_EMAIL` fallback — so a client
    // that has not yet learned to send the header keeps working. Resolved once, per request.
    let caller = super::caller_email(&state, &headers).await;

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

    // AUTHORISATION, once, for every verb that names a coworker. Before this, seam A checked
    // that the CALLER was somebody and never that the coworker was theirs: any signed-in person
    // who knew an id could read another person's transcript or send a prompt as them. That was
    // survivable only because ids were not discoverable, and the roster is about to make them
    // discoverable on purpose.
    //
    // The refusal is whatever the verb ALREADY says when it has never heard of an agent — never
    // a 403, and not a uniform 404 either. Both halves matter: 403 says "it exists and is not
    // yours", and a 404 where the verb's own not-found answer is `null` is the same disclosure
    // one step removed, because a stranger can then tell a real id from an invented one by which
    // shape comes back. So refusal and never-heard-of-it are the same reply, per verb.
    //
    // STILL OPEN, and written here rather than only in a pull request: the refusal answers from a
    // table immediately, while a genuine unknown id answers the same shape after the handler has
    // been to the store and come back empty. Same bytes, different latency, so the two are still
    // sortable by somebody who can time replies. The fix is not a matching dummy lookup — that
    // matches only until either query changes, and nothing makes them change together. It is to
    // make authorisation part of the LOOKUP (`coworker_for(caller, id) -> Option<_>`, `None` for
    // both cases), so there is no second path to keep matching and no table to keep in step.
    // Roadmap 19.5.
    if let Some(agent) = names_a_coworker(&method, &args) {
        let coworker = CoworkerId::from_stored(agent);
        // Two levels, and the difference IS what sharing means. Talking to a shared coworker is
        // what the owner granted; changing it is not. Asking `may_use` for both would make
        // sharing a write grant and would contradict the `canManage: false` the roster tells the
        // colleague — a colleague could rename, retire, re-avatar or re-automate a coworker they
        // were only invited to talk to.
        let allowed = if NEEDS_OWNERSHIP.contains(&method.as_str()) {
            owns(&state, &caller, &coworker).await
        } else {
            may_use(&state, &caller, &coworker).await
        };
        match allowed {
            Ok(true) => {}
            Ok(false) => {
                let (code, body) = never_heard_of_it(&method);
                return reply(
                    StatusCode::from_u16(code).unwrap_or(StatusCode::NOT_FOUND),
                    body,
                );
            }
            Err(()) => return refusal(500, "storage failed"),
        }
    }

    match method.as_str() {
        // ---- the roster ----
        "listAgents" => match roster(&state, &caller).await {
            Ok(mut rows) => {
                // §2.2, the slim-avatar variant: when the client says slim, the bytes stay home
                // and the version is the pointer it fetches /avatars/<id> with.
                let slim = headers
                    .get("x-sand-slim-avatars")
                    .and_then(|value| value.to_str().ok())
                    == Some("1");
                if slim {
                    for row in &mut rows {
                        row["avatarDataUrl"] = Value::Null;
                    }
                }
                reply(StatusCode::OK, Value::Array(rows))
            }
            Err(error) => {
                tracing::error!(%error, "listAgents could not read the roster");
                refusal(500, "roster unavailable")
            }
        },
        "countAgents" => match roster(&state, &caller).await {
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
        "getForeverBoxStatus" => {
            let (code, body) = super::conversation::box_status(&state, &args, &caller).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
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
            let (code, body) = super::conversation::send_prompt(&state, &args, &caller).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "resolveLocalToolPermission" => {
            let (code, body) =
                super::conversation::resolve_local_tool_permission(&state, &args, &caller).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "resolveAutoReviewApproval" => {
            let (code, body) =
                super::conversation::resolve_auto_review_approval(&state, &args, &caller).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "stopAgentTurn" => {
            let (code, body) = super::conversation::stop_agent_turn(&state, &args, &caller).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "promptAcceptanceStatus" => {
            let (code, body) = super::conversation::acceptance_status(&state, &args, &caller).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        // Tails and pages: the same read, dressed four ways. `open*` also marks the agent active.
        "openAgentTail" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, &caller, true, false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getAgentTranscriptTail" | "getAgentTranscriptPage" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, &caller, false, false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "openAgentWindowed" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, &caller, true, true).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getAgentTranscriptWindow" => {
            let (code, body) =
                super::conversation::transcript_reply(&state, &args, &caller, false, true).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getAgentTranscript" => {
            let (code, body) =
                super::conversation::full_transcript(&state, &args, &caller, false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "getTranscript" => {
            let (code, body) =
                super::conversation::full_transcript(&state, &json!({}), &caller, false).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "openAgent" => {
            let (code, body) =
                super::conversation::full_transcript(&state, &args, &caller, true).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        // No branching yet: a thread is its root, and the outline is empty. Honest empties in the
        // container types the renderer validates.
        "getAgentThread" => reply(StatusCode::OK, json!({ "entries": [] })),
        "getConversationOutline" => reply(StatusCode::OK, json!([])),

        // ---- P5: the agent lifecycle (slice 11) ----
        "createAgent" => wrap(super::lifecycle::create_agent(&state, &args, &caller).await),
        "updateAgent" => wrap(super::lifecycle::update_agent(&state, &args).await),
        "deleteAgent" => {
            let ids: Vec<String> = args
                .get("id")
                .and_then(Value::as_str)
                .map(|id| vec![id.to_string()])
                .unwrap_or_default();
            wrap(super::lifecycle::delete_agents(&state, &ids, &caller).await)
        }
        "deleteAgents" => {
            let ids: Vec<String> = args
                .get("ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            wrap(super::lifecycle::delete_agents(&state, &ids, &caller).await)
        }
        "duplicateAgent" => wrap(super::lifecycle::duplicate_agent(&state, &args).await),
        "searchAgents" => wrap(super::lifecycle::search_agents(&state, &args).await),
        "searchMedia" => reply(StatusCode::OK, json!([])),
        "setAgentAvatarBytes" => wrap(super::lifecycle::set_avatar(&state, &args).await),
        "getAgentAvatar" => wrap(super::lifecycle::get_avatar(&state, &args).await),
        // The shipped host answers undefined and does nothing; keeping the no-op IS the contract.
        "setAgentNotificationsEnabled" => reply(StatusCode::OK, Value::Null),
        "setAgentUnread"
        | "setAgentNotifyOnUpdates"
        | "setAgentHiddenFromSidebar"
        | "kickstartAgent" => reply(StatusCode::OK, Value::Null),
        // Groups (`plan-rooms.md` §2): a coworker with members; the createAgent reply shape.
        "createGroup" => wrap(super::lifecycle::create_group(&state, &args, &caller).await),
        "setGroupMembers" => {
            wrap(super::lifecycle::set_group_members(&state, &args, &caller).await)
        }

        // ---- P6: entry mutation ----
        "reactToMessage" => {
            let emoji = args
                .get("emoji")
                .and_then(Value::as_str)
                .unwrap_or("👍")
                .to_string();
            wrap(
                super::lifecycle::mutate_entry(&state, &args, &caller, move |entry| {
                    let reactions = entry
                        .as_object_mut()
                        .map(|map| map.entry("reactions").or_insert_with(|| json!([])));
                    if let Some(Value::Array(reactions)) = reactions {
                        reactions.push(json!({ "emoji": emoji, "by": "me" }));
                    }
                })
                .await,
            )
        }
        "respondToWidget" => {
            let value = args.get("value").cloned().unwrap_or(Value::Null);
            wrap(
                super::lifecycle::mutate_entry(&state, &args, &caller, move |entry| {
                    if let Some(map) = entry.as_object_mut() {
                        map.insert("respondedValue".to_string(), value);
                    }
                })
                .await,
            )
        }
        "dismissWidget" => wrap(
            super::lifecycle::mutate_entry(&state, &args, &caller, |entry| {
                if let Some(map) = entry.as_object_mut() {
                    map.insert("widgetDismissed".to_string(), json!(true));
                }
            })
            .await,
        ),
        "deleteTranscriptEntries" => {
            wrap(super::lifecycle::delete_entries(&state, &args, &caller).await)
        }
        "submitSecret" | "appendConnectorCard" => reply(StatusCode::OK, Value::Null),

        // ---- P9: automations are slice 6's schedules wearing the client's names ----
        "getAgentAutomations" | "listAllAutomations" => {
            wrap(super::lifecycle::get_automations(&state, &args).await)
        }
        "createAgentAutomation" => wrap(super::lifecycle::create_automation(&state, &args).await),
        // An edit UPDATES the row. This verb used to be routed to create, so every edit in the
        // desktop's Routines pane made a second schedule.
        "updateAgentAutomation" => wrap(super::lifecycle::update_automation(&state, &args).await),
        "setAgentAutomationEnabled" => {
            // `isEnabled` is the desktop's spelling (routines/controller.ts:54); `enabled` the
            // smoke's. Absent means enable, as before.
            let action = if args
                .get("isEnabled")
                .or_else(|| args.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                "enable"
            } else {
                "disable"
            };
            wrap(super::lifecycle::change_automation(&state, &args, action).await)
        }
        "deleteAgentAutomation" => {
            wrap(super::lifecycle::change_automation(&state, &args, "delete").await)
        }
        "runAgentAutomationNow" => wrap(super::lifecycle::run_automation_now(&state, &args).await),
        // Workflows, memories, subagents, async tasks: the honest empties in the right containers.
        "getAgentWorkflows" => reply(StatusCode::OK, json!([])),
        "getSubagents" | "getAsyncTasks" => reply(StatusCode::OK, json!([])),
        "getAgentMemories" => reply(StatusCode::OK, json!([])),
        "deleteAgentMemory" | "clearAgentMemories" => reply(StatusCode::OK, Value::Null),

        // ---- Shared rooms (cross-account, `plan-rooms.md` §3): not served, said in the client's
        // own shapes. The renderer projects every state reply through `projectSharingState`
        // (`shared-room/model.ts`: isEnabled boolean, selfAuthId string|null, three arrays) and
        // rejects anything else as "malformed"; the host answers the disabled case with the
        // same empty state, `{status:"error", message}` for the verbs that make a room or an
        // invite, and nothing for typing (`cross-user-sharing/extension.ts`). Transcribed, not
        // invented: an invented `{rooms, invites, requests}` passed only because the bridge
        // once required no more than a record.
        "getSharingState"
        | "respondToRoomJoinRequest"
        | "addOwnAgentToSharedRoom"
        | "removeOwnAgentFromSharedRoom"
        | "leaveSharedRoom" => reply(StatusCode::OK, sharing_disabled_state()),
        "createRoomFromAgent" | "createRoomInvite" | "joinSharedRoom" | "createSharedRoom" => {
            reply(
                StatusCode::OK,
                json!({ "status": "error", "message": SHARING_DISABLED_MESSAGE }),
            )
        }
        "setSharedRoomTyping" => reply(StatusCode::OK, Value::Null),
        "getAgentChannels" => reply(StatusCode::OK, json!({ "channels": [] })),
        "getListenerIntegrations" => reply(StatusCode::OK, json!({})),
        "listBoxMcpServers" => reply(StatusCode::OK, json!({ "servers": [] })),

        // ---- P7: attachments wait on the artifacts store, and say so ----
        "uploadAttachment"
        | "readAttachmentImage"
        | "readAttachmentText"
        | "readAttachmentChunk" => refusal(
            400,
            "attachments are not stored by this server yet (artifacts is a planned slice)",
        ),

        // ---- P8: the skills catalogue is the plugin catalogue we already curate ----
        "skillsCatalog" => {
            let skills: Vec<Value> = state
                .agui
                .plugins
                .values()
                .flat_map(|plugin| {
                    let plugin_name = plugin.manifest.name.clone();
                    plugin.skills.iter().map(move |skill| {
                        json!({
                            "name": skill.name,
                            "plugin": plugin_name,
                            "description": skill.description,
                        })
                    })
                })
                .collect();
            reply(StatusCode::OK, Value::Array(skills))
        }
        "getPluginSyncStatus" => reply(
            StatusCode::OK,
            json!({ "plugins": state.agui.plugins.len(), "synced": true }),
        ),
        "syncPluginSkills" | "completeMcpOAuth" => reply(StatusCode::OK, Value::Null),
        "getSkillPublishTargets" => reply(StatusCode::OK, json!([])),
        "publishSkill" | "resyncPublishedSkill" | "unpublishSkill" => {
            refusal(400, "skill publishing is not supported by this server yet")
        }
        "listRoutedMcpTools" => reply(StatusCode::OK, json!([])),
        "refreshMcp" | "executeRoutedMcpTool" => refusal(
            400,
            "routed MCP runs through a coworker's own connections on this server; drive it from a run",
        ),

        // ---- P10: the box control surface, over what the deployment actually has ----
        // With no computer provider configured, null is the well-formed truth the validator
        // accepts; the lifecycle verbs are accepted no-ops so a UI click is not an error banner.
        "getCloudAgentInfo" => reply(StatusCode::OK, Value::Null),
        // The box-control verbs, for real (no "running" stub): each acts on the caller's agent's box
        // and answers with its true resulting state. updateForeverBox has no image-update mechanism
        // for our boxes, so it reports the current status honestly rather than faking an update.
        "ensureForeverBox" => {
            let (code, body) = super::conversation::box_control(
                &state,
                &args,
                &caller,
                super::conversation::BoxAction::Ensure,
            )
            .await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "handBackForeverBox" => {
            let (code, body) = super::conversation::box_control(
                &state,
                &args,
                &caller,
                super::conversation::BoxAction::HandBack,
            )
            .await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "resetForeverBox" => {
            let (code, body) = super::conversation::box_control(
                &state,
                &args,
                &caller,
                super::conversation::BoxAction::Reset,
            )
            .await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "updateForeverBox" => {
            let (code, body) = super::conversation::box_status(&state, &args, &caller).await;
            reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            )
        }
        "autoUpdateBoxNow"
        | "snapshotBoxStoreNow"
        | "clearBoxStoreNow"
        | "setBoxMigrating"
        | "prepareBoxForRecreate"
        | "resumeBoxAfterRecreate"
        | "updateHostNow" => reply(StatusCode::OK, Value::Null),
        "getBoxStoreStatus" => reply(StatusCode::OK, Value::Null),

        // ---- everything else, exactly as the shipped host words it ----
        other => refusal(404, &format!("unknown gateway method: {other}")),
    }
}

/// `(status, body)` from a lifecycle helper into the gateway's reply mechanics.
fn wrap(result: (u16, Value)) -> Response {
    let (code, body) = result;
    reply(
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body,
    )
}

fn settings_snapshot(state: &GatewayState) -> Value {
    state
        .settings
        .lock()
        .map(|settings| settings.clone())
        .unwrap_or_else(|_| super::default_settings())
}

/// The roster: the gateway account's coworkers as live §8.1 rows. An empty roster is a truthful
/// answer, not an error — the client shows onboarding.
async fn roster(
    state: &GatewayState,
    caller: &str,
) -> Result<Vec<Value>, opengrok_store::StoreError> {
    super::live::roster_rows_for(state, caller).await
}

/// Verbs that carry an `agentId` and answer a CONSTANT — an honest empty in the container the
/// renderer expects, naming no coworker and reading nothing. They are not gated, because a 404
/// here would not protect anything and WOULD divert the client: the renderer projects a sharing
/// reply through `projectSharingState` and rejects anything else as malformed (CLAUDE.md #1,
/// and the third fact in its header: reply shapes matter as much as replies).
///
/// A verb earns a place on this list only by returning a literal. The default is the other way
/// round — anything naming a coworker is checked — so a verb added later is gated until somebody
/// deliberately decides it answers nothing.
///
/// KEEP WHOLE MATCH ARMS TOGETHER, and `tests/against_constant_verbs.rs` enforces it. Four verbs
/// share one `Value::Null` arm with `setAgentUnread`; exempting some and gating the rest would
/// make identical verbs answer differently for the same id. That is not theoretical — the client
/// passes `args.id` on all four (`source/host/host-gateway-api.ts:361-367`), so a gated sibling
/// would answer 404 to a colleague on a shared coworker where its arm-mate answers a shape.
///
/// This list is a SECOND COPY of something the match statement already knows, and a second copy
/// with nothing forcing agreement is how catalogues drift. The test is that mechanism.
pub const ANSWERS_A_CONSTANT: &[&str] = &[
    "addOwnAgentToSharedRoom",
    "appendConnectorCard",
    "clearAgentMemories",
    "createRoomFromAgent",
    "createRoomInvite",
    "createSharedRoom",
    "deleteAgentMemory",
    "getAgentChannels",
    "getAgentMemories",
    "getAgentThread",
    "getAgentWorkflows",
    "getAsyncTasks",
    "getConversationOutline",
    "getSharingState",
    "getSubagents",
    "joinSharedRoom",
    "kickstartAgent",
    "leaveSharedRoom",
    "removeOwnAgentFromSharedRoom",
    "respondToRoomJoinRequest",
    "setAgentHiddenFromSidebar",
    "setAgentNotificationsEnabled",
    "setAgentNotifyOnUpdates",
    "setAgentUnread",
    "submitSecret",
];

/// Ids that are definitely NOT a coworker's. `id` on the wire means different things on
/// different verbs — the coworker on `openAgent` and the transcript reads, the AUTOMATION on
/// `setAgentAutomationEnabled` — so the fallback below has to tell them apart. It does it by the
/// prefix every id in `opengrok-core::id` carries, and it is a DENYLIST on purpose: an id whose
/// shape is not recognised is treated as a coworker's and checked, which is the narrow side.
/// Listing the coworker-naming verbs instead would leave a new one unchecked by default.
///
/// Learned from `slice15-lifecycle-smoke.sh`, which disabled an automation and got "no such
/// agent" back: the gate had read a schedule id as a coworker id and refused a verb that never
/// named a coworker at all.
const NOT_A_COWORKER_ID: &[&str] = &[
    "acct_", "box_", "e_", "mon_", "org_", "pr_", "run_", "sched_", "sess_",
];

/// The coworker a verb acts on, when it names one. Reads `agentId` first (the client's name for
/// it on nearly every verb) and then `id`, which `openAgent` and the transcript reads use.
///
/// Verbs that CREATE a coworker are excluded by having no id to name yet; `deleteAgents` takes a
/// list and is gated per id inside `lifecycle`, not here.
fn names_a_coworker(method: &str, args: &Value) -> Option<String> {
    if method == "createAgent" || method == "createGroup" || ANSWERS_A_CONSTANT.contains(&method) {
        return None;
    }
    if let Some(agent) = args.get("agentId").and_then(Value::as_str) {
        return (!agent.is_empty()).then(|| agent.to_string());
    }
    args.get("id")
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && !NOT_A_COWORKER_ID
                    .iter()
                    .any(|prefix| id.starts_with(prefix))
        })
        .map(str::to_string)
}

/// Owner, or shared with an org this caller is in. Errors are `Err(())` so the caller answers
/// 500 rather than treating a broken read as permission — a broken condition on an allow may
/// only ever narrow (CLAUDE.md #8).
async fn may_use(state: &GatewayState, caller: &str, coworker: &CoworkerId) -> Result<bool, ()> {
    let account = match state.agui.auth.store.account_by_email(caller).await {
        Ok(Some(account)) => account,
        Ok(None) => return Ok(false),
        Err(error) => {
            tracing::error!(%error, "could not resolve the caller for a coworker check");
            return Err(());
        }
    };
    state
        .agui
        .auth
        .store
        .may_use_coworker(&account.id, coworker)
        .await
        .map_err(|error| {
            tracing::error!(%error, coworker = %coworker.as_str(), "could not check whether the caller may use this coworker");
        })
}

/// What a verb answers when it has never heard of an agent, which is also what it answers to
/// somebody who may not use one. Transcribed from the verbs themselves, not chosen: `updateAgent`
/// answers `null` for an unknown id (`slice15-lifecycle-smoke.sh` asserts exactly that) and
/// `getAgentAvatar` answers a two-null object, so a uniform 404 would both break the client and
/// leak which ids are real.
///
/// A THIRD COPY of something the handlers know, like `ANSWERS_A_CONSTANT` — and this one has no
/// test that can derive it, because "what this returns for an unknown id" is not visible in a
/// match arm. The smokes are the mechanism: they assert the shapes for ids that do not exist,
/// and this table has to give the same ones.
fn never_heard_of_it(method: &str) -> (u16, Value) {
    match method {
        "updateAgent"
        | "setGroupMembers"
        | "setAgentAvatarBytes"
        | "runAgentAutomationNow"
        | "reactToMessage"
        | "respondToWidget"
        | "dismissWidget" => (200, Value::Null),
        "getAgentAvatar" => (200, json!({ "dataUrl": null, "version": null })),
        _ => (404, json!({ "error": "no such agent" })),
    }
}

/// Verbs that CHANGE a coworker rather than use it. These need ownership, not permission to
/// talk: sharing lets a colleague have a conversation, and the roster promises them
/// `canManage: false`. Checked as a denylist would be wrong here — a verb missing from this
/// list falls back to `may_use`, which is the WIDER answer — so anything that mutates a
/// coworker's configuration must be added, and `tests/against_visibility.rs` pins the two that
/// were found the hard way.
///
/// `deleteAgents` is NOT here because it takes a list of ids rather than one, so the extractor
/// below cannot see it; it is filtered to the caller's own inside `lifecycle::delete_agents`.
pub const NEEDS_OWNERSHIP: &[&str] = &[
    "createAgentAutomation",
    "deleteAgent",
    "deleteAgentAutomation",
    "duplicateAgent",
    "getAgentAutomations",
    "runAgentAutomationNow",
    "setAgentAutomationEnabled",
    "setAgentAvatarBytes",
    "setGroupMembers",
    "updateAgent",
    "updateAgentAutomation",
];

/// Does this caller OWN the coworker. Ownership, never org visibility — the narrow question,
/// for the verbs that change something.
async fn owns(state: &GatewayState, caller: &str, coworker: &CoworkerId) -> Result<bool, ()> {
    let account = match state.agui.auth.store.account_by_email(caller).await {
        Ok(Some(account)) => account,
        Ok(None) => return Ok(false),
        Err(error) => {
            tracing::error!(%error, "could not resolve the caller for an ownership check");
            return Err(());
        }
    };
    match state.agui.auth.store.coworker_owner(coworker).await {
        Ok(owner) => Ok(owner.is_some_and(|owner| owner == account.id)),
        Err(error) => {
            tracing::error!(%error, coworker = %coworker.as_str(), "could not read a coworker's owner");
            Err(())
        }
    }
}
