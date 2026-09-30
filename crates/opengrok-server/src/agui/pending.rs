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
use futures::StreamExt;
use opengrok_core::id::{AccountId, CoworkerId, PendingUserMessageId};
use opengrok_core::inference::{InferenceSource, SourceKind, TurnSource, Via};
use opengrok_harness::local_proxy::Saved;
use opengrok_store::{
    DrainKey, DrainResult, EnqueueResult, NewPendingUserMessage, PendingUserMessagePatch,
    PendingUserMessageRow,
};
use opengrok_wire::agui::{Event, EventType, RunAgentInput};
use serde::Deserialize;
use serde_json::{Value, json};

use super::routes::{AgUiState, account_from_bearer, now_ms, unavailable};

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
    let mut message = json!({
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
    });
    // Only when the send named one: absent is the account's setting, which is not the row's to say.
    // As it was named: the word alone, or `{kind, via}`.
    if let Some(source) = &row.inference_source {
        let named = TurnSource::from_stored(source).map(TurnSource::to_value);
        message["inferenceSource"] = named.unwrap_or_else(|| json!(source));
    }
    message
}

/// What `heldFor` says of a queued send its person's Mac would carry while no Mac is connected.
const HELD_FOR: &str = "relay_offline";

fn named(row: &PendingUserMessageRow) -> Option<TurnSource> {
    TurnSource::from_stored(row.inference_source.as_deref()?)
}

/// Whether queued sends wait for the person's Mac (`heldFor`): one still queued goes by their
/// Mac — by its own source, else the turn's, else the account's setting — and none is connected.
/// Read per reply and never stored: the moment a Mac is back, no send reads as held.
pub(crate) struct Held(Option<InferenceSource>);

impl Held {
    pub(crate) async fn now(state: &AgUiState, account: &AccountId) -> Self {
        match state.auth.relay.connected(account.as_str()) {
            Some(_) => Self(None),
            None => Self(state.setting(account).await),
        }
    }

    fn of(&self, row: &PendingUserMessageRow, chosen: Option<TurnSource>) -> Option<&'static str> {
        let way = self.0.as_ref()?.resolve(named(row).or(chosen));
        (way == (SourceKind::LocalProxy, Via::Mac) && row.status == "pending").then_some(HELD_FOR)
    }
}

/// A snapshot's rows and their CUSTOM frames, each carrying `heldFor` when it waits for a Mac.
fn snapshot(thread: &str, rows: &[PendingUserMessageRow], held: &Held) -> (Vec<Value>, Vec<Value>) {
    let each = |row| {
        let mut message = message_json(row);
        let mut event = custom_event("snapshot", thread, Some(row));
        if let Some(why) = held.of(row, None) {
            message["heldFor"] = json!(why);
            event["value"]["message"]["heldFor"] = json!(why);
        }
        (message, event)
    };
    rows.iter().map(each).unzip()
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
    state: &AgUiState,
    thread_id: &str,
    account: &AccountId,
) -> Result<Value, String> {
    let rows = state.auth.store.pending_user_messages(thread_id, account);
    let rows = rows.await.map_err(|error| error.to_string())?;
    let (messages, events) = snapshot(thread_id, &rows, &Held::now(state, account).await);
    Ok(json!({ "pendingUserMessages": messages, "pendingEvents": events }))
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
    /// The send's own source pick, as `forwardedProps.inferenceSource` names it on a live turn.
    #[serde(default, deserialize_with = "present")]
    inference_source: Option<Value>,
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

/// The options a write names, each read as the turn that fires it will read it.
struct Options<'a> {
    recipe_id: Option<&'a str>,
    skill_id: Option<&'a str>,
    /// Refused unless a wire word or `{kind, via}`, as on a live turn: read as the gateway, a
    /// misspelt `local-proxy` would bill a key the person chose not to use. As the row keeps it.
    source: Option<String>,
}

/// A create's or an edit's options, or the sentence its 400 says.
fn options(body: &WriteBody) -> Result<Options<'_>, String> {
    if !reply_to_ok(body.reply_to.as_ref()) {
        return Err("replyTo is a message id, an object, or null".to_string());
    }
    let source = TurnSource::named(body.inference_source.as_ref(), "inferenceSource")?;
    Ok(Options {
        recipe_id: optional_string_id("recipeId", body.recipe_id.as_ref())?,
        skill_id: optional_string_id("skillId", body.skill_id.as_ref())?,
        source: source.map(TurnSource::stored),
    })
}

/// Signed-in owner of this thread, or the same 404 `GET /ag-ui/threads/{id}` gives for a
/// missing thread, another account's, and no token — so a pending id is not a probe.
async fn caller_on_thread(
    state: &AgUiState,
    headers: &HeaderMap,
    thread_id: &str,
) -> Result<AccountId, Response> {
    let none = || (StatusCode::NOT_FOUND, "no such thread").into_response();
    let account = account_from_bearer(state, headers).filter(|_| thread_id_ok(thread_id));
    let account = account.ok_or_else(none)?;
    match state
        .auth
        .store
        .account_owns_thread(thread_id, &account)
        .await
    {
        Ok(true) => Ok(account),
        Ok(false) => Err(none()),
        Err(error) => Err(unavailable(error)),
    }
}

fn bad(why: impl IntoResponse) -> Response {
    (StatusCode::BAD_REQUEST, why).into_response()
}

fn listed(thread_id: &str, rows: &[PendingUserMessageRow], held: &Held) -> Value {
    let (messages, events) = snapshot(thread_id, rows, held);
    json!({ "v": PAYLOAD_V, "threadId": thread_id, "pendingUserMessages": messages,
            "pendingEvents": events })
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
) -> Result<Response, Response> {
    let account = caller_on_thread(&state, &headers, &thread_id).await?;
    let store = &state.auth.store;
    let rows = store.pending_user_messages(&thread_id, &account).await;
    let held = Held::now(&state, &account).await;
    Ok(Json(listed(&thread_id, &rows.map_err(unavailable)?, &held)).into_response())
}

/// `POST /ag-ui/threads/{thread_id}/pending`
async fn create(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(body): Json<WriteBody>,
) -> Result<Response, Response> {
    let account = caller_on_thread(&state, &headers, &thread_id).await?;
    if let Some(why) = version_ok(body.v) {
        return Err(bad(why));
    }
    if body
        .thread_id
        .as_ref()
        .is_some_and(|named| *named != thread_id)
    {
        return Err(bad("threadId in the body must match the path"));
    }
    let content = body.content.as_deref();
    let content = content.ok_or_else(|| bad("a pending user message needs content"))?;
    if let Some(why) = content_ok(content) {
        return Err(bad(why));
    }
    let options = options(&body).map_err(bad)?;
    let client_message_id = body.client_message_id.as_deref().map(str::trim);
    let client_message_id = client_message_id.filter(|id| !id.is_empty());
    optional_id_ok("clientMessageId", client_message_id).map_err(bad)?;
    let recipe_values = body.recipe_values.as_ref().filter(|value| !value.is_null());
    let reply_to = body.reply_to.as_ref().filter(|value| !value.is_null());
    let id = PendingUserMessageId::new();
    let new = NewPendingUserMessage {
        id: id.as_str(),
        thread_id: &thread_id,
        account_id: account.as_str(),
        content,
        reply_to,
        recipe_id: options.recipe_id,
        recipe_values,
        skill_id: options.skill_id,
        client_message_id,
        inference_source: options.source.as_deref(),
    };
    let created = |row| Json(mutated("created", &thread_id, Some(row)));
    let (retry, store) = ("another writer got there first; retry", &state.auth.store);
    Ok(
        match store.enqueue_pending_user_message(new, now_ms()).await {
            Ok(EnqueueResult::Created(row)) => (StatusCode::CREATED, created(&row)).into_response(),
            Ok(EnqueueResult::Existing(row)) => created(&row).into_response(),
            Ok(EnqueueResult::AlreadyConsumed(row)) => already_consumed(&thread_id, &row),
            Err(opengrok_store::StoreError::Conflict) => {
                (StatusCode::CONFLICT, retry).into_response()
            }
            Err(error) => unavailable(error),
        },
    )
}

/// `PATCH /ag-ui/threads/{thread_id}/pending/{id}`
async fn edit(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((thread_id, id)): Path<(String, String)>,
    Json(body): Json<WriteBody>,
) -> Result<Response, Response> {
    let account = caller_on_thread(&state, &headers, &thread_id).await?;
    if let Some(why) = version_ok(body.v) {
        return Err(bad(why));
    }
    if let Some(why) = body.content.as_deref().and_then(content_ok) {
        return Err(bad(why));
    }
    let options = options(&body).map_err(bad)?;
    // serde: missing field vs JSON null. `replyTo: null` clears; omitting keeps.
    fn given(value: &Option<Value>) -> Option<Option<&Value>> {
        value
            .is_some()
            .then(|| value.as_ref().filter(|v| !v.is_null()))
    }
    let patch = PendingUserMessagePatch {
        content: body.content.as_deref(),
        reply_to: given(&body.reply_to),
        recipe_id: body.recipe_id.is_some().then_some(options.recipe_id),
        recipe_values: given(&body.recipe_values),
        skill_id: body.skill_id.is_some().then_some(options.skill_id),
        inference_source: given(&body.inference_source).map(|_| options.source.as_deref()),
    };
    let store = &state.auth.store;
    let edited = store.update_pending_user_message(&id, &account, &thread_id, patch, now_ms());
    Ok(match edited.await.map_err(unavailable)? {
        Some(row) => Json(mutated("edited", &thread_id, Some(&row))).into_response(),
        None => (StatusCode::NOT_FOUND, "no such pending user message").into_response(),
    })
}

/// `DELETE /ag-ui/threads/{thread_id}/pending/{id}` — idempotent. A drained, cancelled, or
/// never-heard-of id on a thread this account owns is the same 204. Another account's thread
/// stays "no such thread".
async fn cancel(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((thread_id, id)): Path<(String, String)>,
) -> Result<Response, Response> {
    let account = caller_on_thread(&state, &headers, &thread_id).await?;
    let store = &state.auth.store;
    let gone = store
        .delete_pending_user_message(&id, &account, &thread_id)
        .await;
    gone.map_err(unavailable)?;
    Ok((StatusCode::OK, Json(mutated("canceled", &thread_id, None))).into_response())
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
        .as_ref()
        .map(opengrok_wire::agui::Content::text)
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
/// `Ok` means this turn may start: we drained a row, this run already drained it (retry), or the
/// turn was never a queued send; it carries the row's source pick, when the send made one.
/// Anything else is a conflict the client can show — or a send held for its Mac.
///
/// A SEND ITS MAC WOULD CARRY STAYS QUEUED WHILE NO MAC IS CONNECTED: drained, it could only fail
/// `relay_offline`, or go a way the person did not choose. The drain's own check leaves it; it is
/// answered 202 as it now is (`heldFor`), and the server sends it once the Mac is back.
pub async fn consume_for_turn(
    state: &AgUiState,
    account: &AccountId,
    input: &RunAgentInput,
    chosen: Option<TurnSource>,
) -> Result<Option<TurnSource>, Response> {
    let pending_id = pending_id_from(input);
    if pending_id.is_some() && !input.messages.iter().any(|message| message.role == "user") {
        return Err(bad("a queued send needs a user message"));
    }
    let (thread_id, run_id) = (input.thread_id.as_str(), input.run_id.as_str());
    let key = match (&pending_id, last_user_message_id(input)) {
        (Some(id), _) => DrainKey::Id(id),
        (None, Some(bubble)) => DrainKey::ClientMessageId(bubble),
        (None, None) => return Ok(None),
    };
    let held = Held::now(state, account).await;
    let fires = |row: &_| matches_turn(row, input) && held.of(row, chosen).is_none();
    let store = &state.auth.store;
    let drained =
        store.drain_pending_user_message(key, account, thread_id, run_id, now_ms(), fires);
    match drained.await {
        Ok(DrainResult::Drained(row) | DrainResult::AlreadyThisRun(row)) => Ok(named(&row)),
        Ok(DrainResult::Stale(row))
            if held.of(&row, chosen).is_some() && matches_turn(&row, input) =>
        {
            let mut event = custom_event("edited", thread_id, Some(&row));
            event["value"]["message"]["heldFor"] = json!(HELD_FOR);
            let why = "Your Mac isn't connected, so this message stays queued and goes when it \
                       reconnects.";
            let body = json!({ "v": PAYLOAD_V, "id": row.id, "heldFor": HELD_FOR, "message": why,
                               "event": event });
            Err((StatusCode::ACCEPTED, Json(body)).into_response())
        }
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
            Err(conflict(json!({
                "v": PAYLOAD_V,
                "error": "stale-pending-message",
                "id": row.id,
                "runId": row.drained_run_id,
                "message": message,
                "event": custom_event(op, thread_id, Some(&row)),
            })))
        }
        Ok(DrainResult::Missing) if pending_id.is_none() => Ok(None),
        Ok(DrainResult::Missing) => Err(conflict(json!({
            "v": PAYLOAD_V,
            "error": "not-pending",
            "id": pending_id,
            "event": custom_event("canceled", thread_id, None),
        }))),
        Ok(DrainResult::AlreadyConsumed(row)) => Err(already_consumed(thread_id, &row)),
        Err(error) => Err(unavailable(error)),
    }
}

fn conflict(body: Value) -> Response {
    (StatusCode::CONFLICT, Json(body)).into_response()
}

/// The 409 for a send a turn already fired: NativeChat puts its bubble right from `event`.
fn already_consumed(thread_id: &str, row: &PendingUserMessageRow) -> Response {
    conflict(json!({
        "v": PAYLOAD_V,
        "error": "already-consumed",
        "id": row.id,
        "runId": row.drained_run_id,
        "event": custom_event("drained", thread_id, Some(row)),
    }))
}

/// THE RECONNECT TRIGGER. A person's Mac opened its relay stream: each thread whose queue begins
/// with sends held for it sends them now, oldest first, one turn at a time, as the person's own
/// (`routes::turn`). Their app fires a send when the run before it ends; one held for an absent
/// Mac had no such moment, and would wait for good (CLAUDE.md #5). A thread stops at a run in
/// flight, which fires its own queue, and at a send that starts no turn (held again, changed).
pub(crate) async fn drain_held(host: crate::host_state::HostState, account: AccountId) {
    let held = Held(host.agui.setting(&account).await);
    let Ok(rows) = host
        .agui
        .auth
        .store
        .pending_user_messages_of(&account)
        .await
    else {
        return;
    };
    let mut threads: std::collections::BTreeMap<String, Vec<_>> = Default::default();
    for row in rows {
        threads.entry(row.thread_id.clone()).or_default().push(row);
    }
    let sends = threads
        .into_values()
        .map(|queue| send_held(&host, &account, &held, queue));
    futures::future::join_all(sends).await;
}

/// One thread's queue, while its oldest send is held, each once the turn before it has ended.
async fn send_held(
    host: &crate::host_state::HostState,
    account: &AccountId,
    held: &Held,
    queue: Vec<PendingUserMessageRow>,
) {
    let store = &host.agui.auth.store;
    for row in queue.iter().take_while(|row| held.of(row, None).is_some()) {
        let newest = store
            .runs_for_thread_owned_by(&row.thread_id, account, 1)
            .await;
        let Some(newest) = newest.ok().and_then(|runs| runs.into_iter().next()) else {
            return;
        };
        let idle = opengrok_core::run::RunStatus::from_stored(&newest.status).is_terminal();
        let (true, Ok((run, _))) = (idle, store.load_run(&newest.id).await) else {
            return;
        };
        let (who, input) = (
            (Some(account.clone()), None),
            queued_turn(row, run.coworker_id),
        );
        let Some(input) = input else {
            return;
        };
        let answered = super::routes::turn(host.clone(), who, input).await;
        let kind = answered.headers().get(axum::http::header::CONTENT_TYPE);
        if !kind.is_some_and(|kind| kind.as_bytes().starts_with(b"text/event-stream")) {
            return;
        }
        let mut frames = answered.into_body().into_data_stream();
        while frames.next().await.is_some() {}
    }
}

/// A held send as the turn its app would have fired, read as a POST is: what `matches_turn`
/// holds a drain to, and the coworker the thread last spoke with.
fn queued_turn(row: &PendingUserMessageRow, coworker: Option<CoworkerId>) -> Option<RunAgentInput> {
    let bubble = row.client_message_id.as_deref().unwrap_or(&row.id);
    let message = json!({ "id": bubble, "role": "user", "content": row.content,
                          "replyTo": row.reply_to });
    let props = json!({ "pendingId": row.id, "coworkerId": coworker, "recipe": row.recipe_id,
                        "recipeValues": row.recipe_values, "skill": row.skill_id });
    let run_id = uuid::Uuid::now_v7().to_string();
    let body = json!({ "threadId": row.thread_id, "runId": run_id, "messages": [message],
                       "forwardedProps": props });
    serde_json::from_value(body).ok()
}

#[cfg(test)]
#[path = "../../tests/unit/pending.rs"]
mod tests;
