//! Durable pending user messages — NativeChat's follow-up queue, per thread, per account.
//!
//! REST next to `GET /ag-ui/threads/{id}` because that is how other clients already hydrate a
//! conversation. The payload is versioned (`v: 1`) so NativeChat can fail closed on a later
//! shape rather than guess. Identity is overwritten from the bearer, never taken from the body
//! (CLAUDE.md #7).
//!
//! Live delivery matches the existing AG-UI hydrate: `GET /ag-ui/threads/{id}` includes the
//! same array, and each mutation answers a CUSTOM `pending-user-message` envelope so a client
//! that already parses CUSTOM can apply create/edit/delete without a second decoder. There is
//! no thread-wide websocket today (`replica.rs` — the SSE bus is per-run and in-process); a
//! second machine polls GET.

use axum::extract::Path;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use opengrok_core::id::{AccountId, PendingUserMessageId};
use opengrok_store::{
    DrainResult, EnqueueResult, NewPendingUserMessage, PendingUserMessagePatch,
    PendingUserMessageRow, PgStore,
};
use opengrok_wire::agui::{Event, EventType, RunAgentInput};
use serde::Deserialize;
use serde_json::{Value, json};

use super::routes::{AgUiState, account_from_bearer, now_ms};

/// Payload version. Writes that name another number are refused unread so a v2 client cannot
/// be stored as v1 by accident.
pub const PAYLOAD_V: u32 = 1;

/// CUSTOM `name` NativeChat (and any other AG-UI client) keys to apply a queue mutation.
pub const CUSTOM_NAME: &str = "pending-user-message";

/// The most a queued send may be. Characters, because that is the unit the composer counts.
pub const MAX_CONTENT_CHARS: usize = 32_768;

/// Thread ids travel in URLs; a multi-megabyte path segment is not a thread id.
pub const MAX_THREAD_ID_CHARS: usize = 256;

/// Client bubble ids, recipe ids, skill ids — the same bound `forwardedProps.skill` uses.
pub const MAX_ID_CHARS: usize = 128;

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/ag-ui/threads/{thread_id}/pending", get(list).post(create))
        .route(
            "/ag-ui/threads/{thread_id}/pending/{id}",
            patch(edit).delete(cancel),
        )
        .with_state(state)
}

/// One follow-up as NativeChat hydrates it. `v` is on every object so a client can switch on
/// the version without wrapping.
pub fn message_json(row: &PendingUserMessageRow) -> Value {
    json!({
        "v": PAYLOAD_V,
        "id": row.id,
        "threadId": row.thread_id,
        "content": row.content,
        "replyTo": row.reply_to,
        "recipeId": row.recipe_id,
        "recipeValues": row.recipe_values,
        "skillId": row.skill_id,
        "clientMessageId": row.client_message_id,
        "status": row.status,
        "createdAtMs": row.created_at_ms,
        "updatedAtMs": row.updated_at_ms,
        "drainedAtMs": row.drained_at_ms,
        "drainedRunId": row.drained_run_id,
    })
}

/// The AG-UI CUSTOM envelope for one mutation. `message` is omitted on `canceled` — the id is
/// enough for the other machine to drop its bubble, and carrying the text after cancel would
/// be a second copy of a send the person took back.
pub fn custom_event(op: &str, thread_id: &str, row: Option<&PendingUserMessageRow>) -> Value {
    let mut value = json!({
        "v": PAYLOAD_V,
        "op": op,
        "threadId": thread_id,
    });
    if let Some(row) = row
        && let Some(object) = value.as_object_mut()
    {
        object.insert("message".to_string(), message_json(row));
    }
    // Timestamp matches every other AG-UI event this server emits.
    match serde_json::to_value(
        Event::new(EventType::Custom, now_ms())
            .with("name", CUSTOM_NAME)
            .with("value", value),
    ) {
        Ok(event) => event,
        Err(_) => json!({
            "type": "CUSTOM",
            "name": CUSTOM_NAME,
            "value": { "v": PAYLOAD_V, "op": op, "threadId": thread_id },
        }),
    }
}

/// Snapshot for `GET /ag-ui/threads/{id}`: the live queue, plus one CUSTOM per item so a client
/// that already walks CUSTOM frames can hydrate without a second decoder. `op` is `snapshot`
/// because this is the current set, not a log of mutations; an id that disappears between two
/// GETs was canceled or drained (a new run in `runs` is how to tell drained from canceled).
pub async fn thread_pending_json(
    store: &PgStore,
    thread_id: &str,
    account: &AccountId,
) -> Result<Value, String> {
    let rows = store
        .pending_user_messages(thread_id, account)
        .await
        .map_err(|error| error.to_string())?;
    let events: Vec<Value> = rows
        .iter()
        .map(|row| custom_event("snapshot", thread_id, Some(row)))
        .collect();
    Ok(json!({
        "pendingUserMessages": rows.iter().map(message_json).collect::<Vec<_>>(),
        "pendingEvents": events,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteBody {
    /// Absent is v1, for a client that has not started sending the field yet. Present and not
    /// 1 is refused.
    v: Option<u32>,
    content: Option<String>,
    /// PRESENCE-AWARE, so PATCH can tell omit (keep) from `null` (clear): absent is `None` and
    /// `null` is `Some(Value::Null)`. Plain `Option<Value>` reads both as `None`, and a clear
    /// silently kept the old option. POST treats `Some(Value::Null)` as absent.
    #[serde(default, deserialize_with = "present")]
    reply_to: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    recipe_id: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    recipe_values: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    skill_id: Option<Value>,
    client_message_id: Option<String>,
    /// Ignored. The path names the thread; a body that disagrees is a client bug we refuse.
    thread_id: Option<String>,
    /// Ignored. The bearer names the account (CLAUDE.md #7).
    #[allow(dead_code)]
    #[serde(default)]
    account_id: Option<String>,
}

fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn optional_string_id<'a>(
    label: &str,
    value: Option<&'a Value>,
) -> Result<Option<&'a str>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            optional_id_ok(label, Some(text).filter(|id| !id.is_empty()))?;
            Ok(Some(text).filter(|id| !id.is_empty()))
        }
        Some(_) => Err(format!("{label} is a string or null")),
    }
}

fn version_ok(v: Option<u32>) -> Option<String> {
    match v {
        None | Some(PAYLOAD_V) => None,
        Some(other) => Some(format!(
            "pending user messages are v:{PAYLOAD_V}; this client sent v:{other}"
        )),
    }
}

fn thread_id_ok(thread_id: &str) -> bool {
    !thread_id.is_empty() && thread_id.len() <= MAX_THREAD_ID_CHARS
}

fn content_ok(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Some("a pending user message needs content".to_string());
    }
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Some(format!("content is at most {MAX_CONTENT_CHARS} characters"));
    }
    None
}

fn optional_id_ok(label: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    if value.len() > MAX_ID_CHARS {
        return Err(format!("{label} is at most {MAX_ID_CHARS} characters"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(format!("{label} is not shaped like an id"));
    }
    Ok(())
}

fn reply_to_ok(value: Option<&Value>) -> bool {
    matches!(
        value,
        None | Some(Value::Null) | Some(Value::String(_)) | Some(Value::Object(_))
    )
}

/// Signed-in owner of this thread, or the same 404 `GET /ag-ui/threads/{id}` gives for a
/// missing thread, another account's, and no token — so a pending id is not a probe.
async fn caller_on_thread(
    state: &AgUiState,
    headers: &HeaderMap,
    thread_id: &str,
) -> Result<AccountId, Response> {
    if !thread_id_ok(thread_id) {
        return Err((StatusCode::NOT_FOUND, "no such thread").into_response());
    }
    let Some(account) = account_from_bearer(state, headers) else {
        return Err((StatusCode::NOT_FOUND, "no such thread").into_response());
    };
    match state
        .auth
        .store
        .account_owns_thread(thread_id, &account)
        .await
    {
        Ok(true) => Ok(account),
        Ok(false) => Err((StatusCode::NOT_FOUND, "no such thread").into_response()),
        Err(error) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()),
    }
}

fn listed(thread_id: &str, rows: &[PendingUserMessageRow]) -> Value {
    json!({
        "v": PAYLOAD_V,
        "threadId": thread_id,
        "pendingUserMessages": rows.iter().map(message_json).collect::<Vec<_>>(),
        "pendingEvents": rows
            .iter()
            .map(|row| custom_event("snapshot", thread_id, Some(row)))
            .collect::<Vec<_>>(),
    })
}

fn mutated(op: &str, thread_id: &str, row: Option<&PendingUserMessageRow>) -> Value {
    let mut body = json!({
        "v": PAYLOAD_V,
        "threadId": thread_id,
        "event": custom_event(op, thread_id, row),
    });
    if let Some(row) = row
        && let Some(object) = body.as_object_mut()
    {
        object.insert("pendingUserMessage".to_string(), message_json(row));
    }
    body
}

/// `GET /ag-ui/threads/{thread_id}/pending`
async fn list(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Response {
    let account = match caller_on_thread(&state, &headers, &thread_id).await {
        Ok(account) => account,
        Err(refusal) => return refusal,
    };
    match state
        .auth
        .store
        .pending_user_messages(&thread_id, &account)
        .await
    {
        Ok(rows) => Json(listed(&thread_id, &rows)).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// `POST /ag-ui/threads/{thread_id}/pending`
async fn create(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(body): Json<WriteBody>,
) -> Response {
    let account = match caller_on_thread(&state, &headers, &thread_id).await {
        Ok(account) => account,
        Err(refusal) => return refusal,
    };
    if let Some(why) = version_ok(body.v) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    if let Some(named) = body.thread_id.as_deref()
        && named != thread_id
    {
        return (
            StatusCode::BAD_REQUEST,
            "threadId in the body must match the path",
        )
            .into_response();
    }
    let Some(content) = body.content.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            "a pending user message needs content",
        )
            .into_response();
    };
    if let Some(why) = content_ok(content) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    if !reply_to_ok(body.reply_to.as_ref()) {
        return (
            StatusCode::BAD_REQUEST,
            "replyTo is a message id, an object, or null",
        )
            .into_response();
    }
    let client_message_id = body
        .client_message_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if let Err(why) = optional_id_ok("clientMessageId", client_message_id) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    let recipe_id = match optional_string_id("recipeId", body.recipe_id.as_ref()) {
        Ok(id) => id,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    let skill_id = match optional_string_id("skillId", body.skill_id.as_ref()) {
        Ok(id) => id,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    let recipe_values = body.recipe_values.as_ref().filter(|value| !value.is_null());
    let reply_to = body.reply_to.as_ref().filter(|value| !value.is_null());
    let id = PendingUserMessageId::new();
    let at_ms = now_ms();
    match state
        .auth
        .store
        .enqueue_pending_user_message(
            NewPendingUserMessage {
                id: id.as_str(),
                thread_id: &thread_id,
                account_id: account.as_str(),
                content,
                reply_to,
                recipe_id,
                recipe_values,
                skill_id,
                client_message_id,
            },
            at_ms,
        )
        .await
    {
        Ok(EnqueueResult::Created(row)) => (
            StatusCode::CREATED,
            Json(mutated("created", &thread_id, Some(&row))),
        )
            .into_response(),
        Ok(EnqueueResult::Existing(row)) => {
            Json(mutated("created", &thread_id, Some(&row))).into_response()
        }
        Ok(EnqueueResult::AlreadyConsumed(row)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "v": PAYLOAD_V,
                "error": "already-consumed",
                "id": row.id,
                "runId": row.drained_run_id,
                "event": custom_event("drained", &thread_id, Some(&row)),
            })),
        )
            .into_response(),
        Err(opengrok_store::StoreError::Conflict) => (
            StatusCode::CONFLICT,
            "another writer got there first; retry",
        )
            .into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// `PATCH /ag-ui/threads/{thread_id}/pending/{id}`
async fn edit(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((thread_id, id)): Path<(String, String)>,
    Json(body): Json<WriteBody>,
) -> Response {
    let account = match caller_on_thread(&state, &headers, &thread_id).await {
        Ok(account) => account,
        Err(refusal) => return refusal,
    };
    if let Some(why) = version_ok(body.v) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    if let Some(content) = body.content.as_deref()
        && let Some(why) = content_ok(content)
    {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    if !reply_to_ok(body.reply_to.as_ref()) {
        return (
            StatusCode::BAD_REQUEST,
            "replyTo is a message id, an object, or null",
        )
            .into_response();
    }
    let recipe_id = match optional_string_id("recipeId", body.recipe_id.as_ref()) {
        Ok(id) => id,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    let skill_id = match optional_string_id("skillId", body.skill_id.as_ref()) {
        Ok(id) => id,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    // serde: missing field vs JSON null. `replyTo: null` clears; omitting keeps.
    let reply_to = if body.reply_to.is_some() {
        Some(body.reply_to.as_ref().filter(|value| !value.is_null()))
    } else {
        None
    };
    let recipe_id_patch = if body.recipe_id.is_some() {
        Some(recipe_id)
    } else {
        None
    };
    let skill_id_patch = if body.skill_id.is_some() {
        Some(skill_id)
    } else {
        None
    };
    let recipe_values = if body.recipe_values.is_some() {
        Some(body.recipe_values.as_ref().filter(|value| !value.is_null()))
    } else {
        None
    };
    match state
        .auth
        .store
        .update_pending_user_message(
            &id,
            &account,
            &thread_id,
            PendingUserMessagePatch {
                content: body.content.as_deref(),
                reply_to,
                recipe_id: recipe_id_patch,
                recipe_values,
                skill_id: skill_id_patch,
            },
            now_ms(),
        )
        .await
    {
        Ok(Some(row)) => Json(mutated("edited", &thread_id, Some(&row))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such pending user message").into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// `DELETE /ag-ui/threads/{thread_id}/pending/{id}` — idempotent. A drained, cancelled, or
/// never-heard-of id on a thread this account owns is the same 204. Another account's thread
/// stays "no such thread".
async fn cancel(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((thread_id, id)): Path<(String, String)>,
) -> Response {
    let account = match caller_on_thread(&state, &headers, &thread_id).await {
        Ok(account) => account,
        Err(refusal) => return refusal,
    };
    match state
        .auth
        .store
        .delete_pending_user_message(&id, &account, &thread_id)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(mutated("canceled", &thread_id, None))).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// What `POST /ag-ui` named as the queued send it is firing, if anything.
pub fn pending_id_from(input: &RunAgentInput) -> Option<String> {
    let props = &input.forwarded_props;
    for key in ["pendingId", "pendingUserMessageId"] {
        if let Some(id) = props
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }
    }
    None
}

/// The last user message's id — NativeChat's bubble id, and the natural key we stored as
/// `clientMessageId` when the send was queued.
pub fn last_user_message_id(input: &RunAgentInput) -> Option<&str> {
    input
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.id.as_str())
        .filter(|id| !id.is_empty())
}

/// Compare what will actually be sent, allowing only the exact legacy reply rendering.
fn matches_turn(row: &PendingUserMessageRow, input: &RunAgentInput) -> bool {
    let Some(message) = input
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
    else {
        return false;
    };
    let reply = message
        .extra
        .get("replyTo")
        .filter(|value| !value.is_null());
    if reply != row.reply_to.as_ref().filter(|value| !value.is_null()) {
        return false;
    }
    let rendered = super::routes::with_reply_context(&row.content, message, &input.messages);
    if !message
        .content
        .as_deref()
        .is_some_and(|text| text == row.content || text == rendered)
    {
        return false;
    }
    // THE TURN'S OWN READERS, NOT A COPY OF THEIR RULES. What counts as "no recipe" or "no
    // skill" is whatever the turn will act on; a second reading here drifted from it once
    // already, refusing values that ride along with no recipe and a recipe that is not a string.
    let saved_recipe = saved_id(row.recipe_id.as_deref()).map(|id| {
        (
            id.to_string(),
            super::routes::recipe_values_from(row.recipe_values.as_ref()),
        )
    });
    if super::routes::chosen_recipe_from(input) != saved_recipe {
        return false;
    }
    let saved_skill = saved_id(row.skill_id.as_deref());
    match super::routes::chosen_skill_from(input) {
        None => saved_skill.is_none(),
        Some(super::routes::ChosenSkill::Id(id)) => saved_skill == Some(id.as_str()),
        Some(super::routes::ChosenSkill::Unusable(_)) => false,
    }
}

fn saved_id(id: Option<&str>) -> Option<&str> {
    id.map(str::trim).filter(|id| !id.is_empty())
}

/// Consume a queued send as this turn, or refuse so two machines cannot both fire it.
///
/// `Ok(())` means this turn may start: we drained a row, this run already drained it (retry),
/// or the turn was never a queued send. Anything else is a conflict the client can show.
pub async fn consume_for_turn(
    store: &PgStore,
    account: &AccountId,
    input: &RunAgentInput,
) -> Result<(), Response> {
    if pending_id_from(input).is_some()
        && !input.messages.iter().any(|message| message.role == "user")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "a queued send needs a user message",
        )
            .into_response());
    }
    let thread_id = input.thread_id.as_str();
    let run_id = input.run_id.as_str();
    let at_ms = now_ms();
    let result = if let Some(pending_id) = pending_id_from(input) {
        store
            .drain_pending_user_message(&pending_id, account, thread_id, run_id, at_ms, |row| {
                matches_turn(row, input)
            })
            .await
    } else if let Some(client_message_id) = last_user_message_id(input) {
        store
            .drain_pending_user_message_by_client_id(
                client_message_id,
                account,
                thread_id,
                run_id,
                at_ms,
                |row| matches_turn(row, input),
            )
            .await
    } else {
        return Ok(());
    };
    match result {
        Ok(DrainResult::Drained(_) | DrainResult::AlreadyThisRun(_)) => Ok(()),
        Ok(DrainResult::Stale(row)) => {
            // A same-run retry that changed its words is not a refresh away from sending: this
            // run already spent the row, so there is nothing left to send.
            let (op, message) = if row.status == "pending" {
                (
                    "edited",
                    "This queued message changed. Refresh it before sending again.",
                )
            } else {
                (
                    "drained",
                    "This run already sent this queued message with different words.",
                )
            };
            Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "v": PAYLOAD_V,
                    "error": "stale-pending-message",
                    "id": row.id,
                    "runId": row.drained_run_id,
                    "message": message,
                    "event": custom_event(op, thread_id, Some(&row)),
                })),
            )
                .into_response())
        }
        Ok(DrainResult::Missing) if pending_id_from(input).is_none() => Ok(()),
        Ok(DrainResult::Missing) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "v": PAYLOAD_V,
                "error": "not-pending",
                "id": pending_id_from(input),
                "event": custom_event("canceled", thread_id, None),
            })),
        )
            .into_response()),
        Ok(DrainResult::AlreadyConsumed(row)) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "v": PAYLOAD_V,
                "error": "already-consumed",
                "id": row.id,
                "runId": row.drained_run_id,
                "event": custom_event("drained", thread_id, Some(&row)),
            })),
        )
            .into_response()),
        Err(error) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn row() -> PendingUserMessageRow {
        PendingUserMessageRow {
            id: "pum_1".to_string(),
            thread_id: "th_1".to_string(),
            account_id: "acct_1".to_string(),
            content: "later".to_string(),
            reply_to: Some(json!({"messageId": "m1", "preview": "hi"})),
            recipe_id: Some("rec_1".to_string()),
            recipe_values: Some(json!({"q": "x"})),
            skill_id: Some("skl_1".to_string()),
            client_message_id: Some("msg_1".to_string()),
            status: "pending".to_string(),
            created_at_ms: 10,
            updated_at_ms: 20,
            drained_at_ms: None,
            drained_run_id: None,
        }
    }

    #[test]
    fn the_payload_is_versioned_and_camel_cased() {
        let json = message_json(&row());
        assert_eq!(json["v"], PAYLOAD_V);
        assert_eq!(json["threadId"], "th_1");
        assert_eq!(json["clientMessageId"], "msg_1");
        assert_eq!(json["recipeId"], "rec_1");
        assert_eq!(json["skillId"], "skl_1");
        assert_eq!(json["createdAtMs"], 10);
        assert_eq!(json["replyTo"]["messageId"], "m1");
    }

    #[test]
    fn a_custom_event_names_the_op_inside_value() {
        let event = custom_event("created", "th_1", Some(&row()));
        assert_eq!(event["type"], "CUSTOM");
        assert_eq!(event["name"], CUSTOM_NAME);
        assert_eq!(event["value"]["v"], PAYLOAD_V);
        assert_eq!(event["value"]["op"], "created");
        assert_eq!(event["value"]["threadId"], "th_1");
        assert_eq!(event["value"]["message"]["id"], "pum_1");
    }

    #[test]
    fn cancel_omits_the_message_so_the_text_is_not_re_sent() {
        let event = custom_event("canceled", "th_1", None);
        assert!(event["value"].get("message").is_none(), "{event}");
        assert_eq!(event["value"]["op"], "canceled");
    }

    #[test]
    fn pending_id_prefers_the_short_name() {
        let mut input = RunAgentInput {
            thread_id: "t".to_string(),
            run_id: "r".to_string(),
            parent_run_id: None,
            state: json!({}),
            messages: vec![],
            tools: json!({}),
            context: json!({}),
            forwarded_props: json!({
                "pendingId": "pum_a",
                "pendingUserMessageId": "pum_b",
            }),
            extra: Default::default(),
        };
        assert_eq!(pending_id_from(&input).as_deref(), Some("pum_a"));
        input.forwarded_props = json!({ "pendingUserMessageId": "pum_b" });
        assert_eq!(pending_id_from(&input).as_deref(), Some("pum_b"));
        input.forwarded_props = json!({ "pendingId": "" });
        assert_eq!(pending_id_from(&input), None);
    }
}
