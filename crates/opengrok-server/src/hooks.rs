//! Inbound HTTP for a routine whose wake is a webhook.
//!
//! THIS IS NOT Box `/webhooks`. Those docs under `docs/box/` are outbound lifecycle callbacks
//! on the coworker's computer. A routine webhook is the other direction: an external app POSTs
//! here, we check a bearer we minted, and we fire the same `autonomy::fire` path Test run and
//! the cron sweep already use.
//!
//! The route is deliberately NOT behind any account token: the caller is a todo app (or a curl),
//! not a signed-in person. Auth is the hook's own key.
//!
//! MINTING A HOOK LIVES HERE, beside the route that checks what it minted. The only door that
//! ever created a webhook wake was the desktop's `createAgentAutomation`, and it went with seam A;
//! `POST /schedules` with `"kind": "webhook"` is what writes one now, and it calls the four
//! functions below. Keeping the mint and the check in one file is the point: the key is minted
//! here, hashed here with `hash_webhook_key`, and compared here with `key_matches` — three things
//! that have to agree, and would agree by coincidence if they lived in three places.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use opengrok_core::id::{HookId, RunId};
use opengrok_core::schedule::{FireCause, ScheduleCommand};

use crate::autonomy::routes::mutate_schedule;
use crate::host_state::HostState;

/// Largest JSON body we will attach to a wake. A webhook is a ping-or-payload, not a file drop.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub fn router(state: HostState) -> Router {
    Router::new()
        .route("/hooks/{hook_id}", post(inbound))
        .with_state(state)
}

/// A fresh inbound bearer. `og_` matches the placeholder NativeChat already shows; 32 random
/// bytes is the same entropy as a refresh token.
pub(crate) fn mint_webhook_key() -> String {
    use rand::RngExt;
    let bytes: [u8; 32] = rand::rng().random();
    format!("og_{}", hex(&bytes))
}

pub(crate) fn mint_hook_id() -> String {
    HookId::new().as_str().to_string()
}

pub(crate) fn hash_webhook_key(key: &str) -> String {
    hex(Sha256::digest(key.as_bytes()).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

pub(crate) fn key_matches(secret_hash: &str, presented: &str) -> bool {
    let presented_hash = hash_webhook_key(presented);
    presented_hash.len() == secret_hash.len()
        && presented_hash
            .as_bytes()
            .ct_eq(secret_hash.as_bytes())
            .into()
}

/// The POST URL the editor shows. Prefers the advertised public gateway address so a loopback
/// bind never leaks into a URL a phone or a SaaS app would POST to.
pub(crate) fn hook_url(state: &HostState, hook_id: &str) -> String {
    let advertised = state
        .public_gateway_url
        .as_deref()
        .filter(|url| !url.is_empty())
        .or_else(|| {
            let url = state.agui.auth.public_url.as_str();
            (!url.is_empty()).then_some(url)
        })
        .unwrap_or("");
    let base = advertised.trim_end_matches('/');
    if base.is_empty() {
        format!("/hooks/{hook_id}")
    } else {
        format!("{base}/hooks/{hook_id}")
    }
}

pub(crate) fn webhook_trigger_json(state: &HostState, hook_id: &str, key: &str) -> Value {
    let url = hook_url(state, hook_id);
    json!({
        "type": "webhook",
        "url": url,
        "key": key,
        "header": format!("Authorization: Bearer {key}"),
    })
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn inbound(
    State(state): State<HostState>,
    Path(hook_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let found = match state.agui.auth.store.schedule_for_hook(&hook_id).await {
        Ok(found) => found,
        Err(error) => {
            tracing::error!(%error, hook = %hook_id, "could not look up an inbound hook");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "storage failed");
        }
    };
    let Some((schedule_id, account_id)) = found else {
        return json_error(StatusCode::NOT_FOUND, "no such hook");
    };

    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let Ok((loaded, _)) = state.agui.auth.store.load_schedule(&schedule_id).await else {
        return json_error(StatusCode::NOT_FOUND, "no such hook");
    };
    if loaded.kind != opengrok_core::schedule::WakeKind::Webhook
        || !key_matches(&loaded.secret_hash, presented)
    {
        // Unknown hook already 404'd from the view. A wrong or missing key, or a row whose
        // aggregate is no longer a webhook, must not distinguish themselves: 401 either way.
        return json_error(StatusCode::UNAUTHORIZED, "bad token");
    }

    let payload = match webhook_payload(&body) {
        Ok(payload) => payload,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    let run_id = RunId::new();
    let at_ms = now_ms();
    let after = match mutate_schedule(&state, &account_id, &schedule_id, at_ms, |loaded| {
        loaded
            .decide(ScheduleCommand::Fire {
                run_id: run_id.clone(),
                cause: FireCause::Webhook,
                at_ms,
            })
            .map_err(|reason| {
                let code = if reason == opengrok_core::schedule::ScheduleError::Paused {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::BAD_REQUEST
                };
                (code.as_u16(), json!({ "error": reason.to_string() }))
            })
    })
    .await
    {
        Ok(after) => after,
        Err((code, body)) => {
            return (
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body.to_string(),
            )
                .into_response();
        }
    };

    let Some(coworker_id) = after.coworker_id.clone() else {
        return json_error(StatusCode::CONFLICT, "that routine has no coworker");
    };
    let prompt = wake_prompt(&after.prompt, payload.as_ref());
    tokio::spawn(crate::autonomy::fire(
        state.agui.clone(),
        crate::autonomy::Firing {
            origin: format!("automation {schedule_id} (webhook)"),
            account_id: account_id.clone(),
            coworker_id: coworker_id.clone(),
            prompt,
            thread_id: schedule_id.as_str().to_string(),
            run_id: run_id.clone(),
            announce: Some(crate::autonomy::Announce {
                gateway: state.clone(),
                name: after.name.clone(),
            }),
        },
    ));
    (
        StatusCode::ACCEPTED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({ "accepted": true, "runId": run_id.as_str() }).to_string(),
    )
        .into_response()
}

/// Empty (or whitespace-only) is a pure ping. Anything else must be JSON, or we refuse rather
/// than stuff an opaque blob into the coworker's prompt.
fn webhook_payload(body: &Bytes) -> Result<Option<Value>, &'static str> {
    if body.len() > MAX_BODY_BYTES {
        return Err("webhook body is too large");
    }
    if body.is_empty() || body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(None);
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Err("body must be JSON"),
    }
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({ "error": message }).to_string(),
    )
        .into_response()
}

/// The wake a fired hook hands the coworker: the routine's own instruction, and the caller's body
/// fenced beneath it.
///
/// THE FENCE IS CLOSED ON PURPOSE. A `<webhook_event>` with no `</webhook_event>` leaves nothing
/// marking where the caller's text stops — harmless only while the payload happens to be the last
/// thing in the prompt, and wrong the moment anything is appended after it. The body arrives from
/// whoever holds the hook key, so the boundary is the whole reason for wrapping it.
fn wake_prompt(prompt: &str, payload: Option<&Value>) -> String {
    match payload {
        None => prompt.to_string(),
        Some(event) => format!("{prompt}\n\n<webhook_event>\n{event}\n</webhook_event>"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn an_empty_body_is_a_ping_and_json_is_kept() {
        assert!(webhook_payload(&Bytes::new()).expect("empty").is_none());
        assert!(
            webhook_payload(&Bytes::from_static(b"  \n"))
                .expect("whitespace")
                .is_none()
        );
        let value = webhook_payload(&Bytes::from_static(br#"{"item":"milk"}"#))
            .expect("json")
            .expect("some");
        assert_eq!(value["item"], "milk");
        assert_eq!(
            webhook_payload(&Bytes::from_static(b"not json")).expect_err("refuse"),
            "body must be JSON"
        );
    }

    #[test]
    fn a_payload_is_fenced_on_both_sides() {
        let event = serde_json::json!({ "event": "push", "commits": 3 });
        let wake = wake_prompt("Summarise it.", Some(&event));
        assert!(
            wake.starts_with("Summarise it.\n\n<webhook_event>\n"),
            "{wake}"
        );
        assert!(
            wake.ends_with("\n</webhook_event>"),
            "the fence must close, or nothing marks where the caller's body ends: {wake}"
        );
        assert_eq!(wake.matches("<webhook_event>").count(), 1);
        assert_eq!(wake.matches("</webhook_event>").count(), 1);

        // A ping carries no body, so it gets no fence at all.
        assert_eq!(wake_prompt("Summarise it.", None), "Summarise it.");
    }

    #[test]
    fn a_rotated_hash_does_not_match_the_old_key() {
        let key = "og_abc";
        let hash = hash_webhook_key(key);
        assert!(key_matches(&hash, key));
        assert!(!key_matches(&hash, "og_other"));
        assert!(!key_matches(&hash, ""));
    }
}
