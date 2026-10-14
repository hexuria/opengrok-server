//! Host settings and AG-UI streaming responses.

use super::*;

/// `?coworker=cw_…`: whose computer the egress-tunnel question is about.
#[derive(Deserialize)]
pub struct HostSettingsQuery {
    coworker: Option<String>,
}

/// The host's settings record, plus whether the egress tunnel is live for the coworker asked
/// about. What the desktop verbs `getHostSettings` and `isEgressTunnelAvailable` answered, on
/// the door NativeChat already uses and under its own account token — so the seam-A door can
/// close without the settings page losing its host.
///
/// `egressTunnelAvailable` is host intent AND that coworker's box advertising the tunnel. No
/// coworker named, not this account's, no computer, or no box report → false: a client must not
/// paint the toggle live until a laptop client is attached.
pub async fn host_settings(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HostSettingsQuery>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    host_settings_reply(&state, &account_id, query.coworker.as_deref()).await
}

/// A partial record: the keys given replace the host's and the rest stay, the merge
/// `setHostSettings` did. Answers the whole record, so the client reads back what it set.
pub async fn patch_host_settings(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HostSettingsQuery>,
    Json(patch): Json<serde_json::Value>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let Some(patch) = patch.as_object() else {
        return (StatusCode::BAD_REQUEST, "a settings patch is a JSON object").into_response();
    };
    let Some(lock) = state.host_settings.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this host keeps no settings",
        )
            .into_response();
    };
    // Saved before it is believed: a record the store refused would be undone by the next
    // restart, so the person is told now rather than finding the switch off tomorrow.
    let mut next = lock
        .lock()
        .ok()
        .map(|settings| settings.clone())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(crate::host_state::default_settings);
    if let Some(record) = next.as_object_mut() {
        for (key, value) in patch {
            record.insert(key.clone(), value.clone());
        }
    }
    if let Err(error) = state
        .auth
        .store
        .save_host_settings(&next, chrono::Utc::now().timestamp_millis())
        .await
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the settings could not be saved: {error}"),
        )
            .into_response();
    }
    if let Ok(mut settings) = lock.lock() {
        *settings = next;
    }
    host_settings_reply(&state, &account_id, query.coworker.as_deref()).await
}

async fn host_settings_reply(
    state: &AgUiState,
    account_id: &AccountId,
    coworker: Option<&str>,
) -> Response {
    let mut record = state
        .host_settings
        .as_ref()
        .and_then(|lock| lock.lock().ok().map(|value| value.clone()))
        .unwrap_or_else(crate::host_state::default_settings);
    let available = egress_tunnel_available_for(state, account_id, coworker).await;
    if let Some(record) = record.as_object_mut() {
        record.insert(
            "egressTunnelAvailable".to_string(),
            serde_json::Value::Bool(available),
        );
    }
    Json(record).into_response()
}

/// Host intent, then the coworker: it must be named, be this account's, have a computer in its
/// scope, and that box must say the tunnel is ready. Each miss is a plain false, never an error
/// — the settings page still has its record to show. The box is the SCOPE's live one, through
/// its own provider, the same one the Computer pane and a turn's tools use; this once read the
/// id frozen on the coworker's row with the boot-time provider and disagreed with both.
async fn egress_tunnel_available_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker: Option<&str>,
) -> bool {
    if !state.egress_tunnel_enabled() {
        return false;
    }
    let Some(coworker) = coworker else {
        return false;
    };
    let coworker_id = CoworkerId::from_stored(coworker.to_string());
    let owns = state
        .auth
        .store
        .coworkers_for(account_id)
        .await
        .map(|roster| roster.iter().any(|view| view.id == coworker_id))
        .unwrap_or(false);
    if !owns {
        return false;
    }
    let Some(scoped) = provision::scoped_box_for(state, account_id, &coworker_id).await else {
        return false;
    };
    let cap = opengrok_box::shown_egress(scoped.computer.as_ref(), &scoped.box_id).await;
    opengrok_box::EgressTunnel::advertised(true, cap)
}

/// The message a reply points at, as the one bracketed line `reply_context` writes for the
/// desktop's own transcript — so a reply reads the same whichever door the turn came through.
///
/// `replyTo` is either the quoted message's id or NativeChat's object (`messageId`, `preview`,
/// `isMe`). The quoted message itself is preferred, with all of its words; the preview is what is
/// left when the client has since dropped the message from the array it sends.
fn reply_quote(
    message: &opengrok_wire::agui::Message,
    sent: &[opengrok_wire::agui::Message],
) -> Option<String> {
    let reply_to = message.extra.get("replyTo")?;
    let (id, preview, is_me) = match reply_to {
        serde_json::Value::String(id) => (Some(id.as_str()), None, None),
        serde_json::Value::Object(_) => (
            reply_to
                .get("messageId")
                .and_then(serde_json::Value::as_str),
            reply_to.get("preview").and_then(serde_json::Value::as_str),
            reply_to.get("isMe").and_then(serde_json::Value::as_bool),
        ),
        _ => return None,
    };
    let quoted = id.and_then(|id| sent.iter().find(|candidate| candidate.id == id));
    let quoted_words =
        quoted.and_then(|quoted| quoted.content.as_ref().map(|content| content.text()));
    let text = quoted_words.as_deref().or(preview)?;
    // Whose words are being quoted, from the model's side of the conversation: the person's own
    // earlier message, or the coworker's.
    let from_person = quoted
        .map(|quoted| quoted.role == "user")
        .or(is_me)
        .unwrap_or(false);
    let who = if from_person {
        "their own earlier message"
    } else {
        "your earlier message"
    };
    crate::agui::resume::reply_quote_line(who, text)
}

/// A user message with the quote it answers ahead of it.
///
/// IDEMPOTENT ON PURPOSE. A client that cannot rely on this field spells the quote into `content`
/// itself — NativeChat does, so a reply works against a server that predates `replyTo` — and
/// saying it twice is worse than not reading the field at all.
pub(in crate::agui) fn with_reply_context(
    content: &str,
    message: &opengrok_wire::agui::Message,
    sent: &[opengrok_wire::agui::Message],
) -> String {
    if content
        .trim_start()
        .starts_with(crate::agui::resume::REPLY_QUOTE_OPENING)
    {
        return content.to_string();
    }
    match reply_quote(message, sent) {
        Some(quote) => format!("{quote}\n\n{content}"),
        None => content.to_string(),
    }
}

/// AG-UI messages to the model's vocabulary, with the files this turn's messages carry already
/// read (#229).
///
/// Roles the model door does not understand are dropped rather than passed through: a provider
/// that rejects an unknown role fails the whole turn, and AG-UI carries roles (`developer`) that
/// have no place in a chat completion.
pub(crate) fn to_chat_messages_with(
    input: &RunAgentInput,
    attached: &super::attachments::Attached,
) -> Vec<ChatMessage> {
    input
        .messages
        .iter()
        .filter_map(|message| chat_message(message, &input.messages, attached))
        .collect()
}

/// One AG-UI message in the model's vocabulary; `sent` is what a reply's quote is looked up in.
///
/// Transcribed from `@ag-ui/core` 0.0.57 (dist/index.js:4-13, 97-113): an assistant message may
/// carry `toolCalls: [{id, type: "function", function: {name, arguments}}]` and no content at all,
/// and a `tool` message answers one by `toolCallId`. Both stay tool calls here, because the model
/// has to see its own calls to read their results (#189); a call-only assistant message used to
/// be dropped for having no words.
pub(crate) fn chat_message(
    message: &opengrok_wire::agui::Message,
    sent: &[opengrok_wire::agui::Message],
    attached: &super::attachments::Attached,
) -> Option<ChatMessage> {
    let content = message.text();
    match message.role.as_str() {
        "assistant" => {
            let calls: Vec<opengrok_harness::ToolCallRef> = message
                .extra
                .get("toolCalls")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|call| {
                    let field = |key: &str| call.pointer(key).and_then(serde_json::Value::as_str);
                    Some(opengrok_harness::ToolCallRef {
                        id: field("/id")?.to_string(),
                        name: field("/function/name")?.to_string(),
                        arguments: field("/function/arguments").unwrap_or("{}").to_string(),
                    })
                })
                .collect();
            (message.content.is_some() || !calls.is_empty())
                .then(|| ChatMessage::calls(content, calls))
        }
        // Only a person replies: the field on anything else is not a quote the model should be
        // read to.
        // The files a person attached ride with their words (#229): pictures as pictures, the
        // rest as words the model is told it can or cannot read.
        "user" => message.content.is_some().then(|| {
            let (text, images) =
                attached.render(message, with_reply_context(&content, message, sent));
            let mut chat = ChatMessage::text("user", text);
            chat.images = images;
            chat
        }),
        "system" => message
            .content
            .is_some()
            .then(|| ChatMessage::text("system", content.clone())),
        // NativeChat continues a frontend tool by POSTing the result as a tool message.
        "tool" => Some(ChatMessage::tool_result(
            message
                .extra
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(message.id.as_str()),
            content,
        )),
        _ => None,
    }
}

/// Live AG-UI frames, forwarded as they are produced. Dropping the HTTP body closes the
/// channel; the spawned turn still runs so a disconnect does not abandon the journal.
///
/// A UserForm CUSTOM is stamped with `entryId` here, **before** the frame is sent: mint the
/// gateway card first (same `user_form_card` / sanitize as `sendPrompt`), then forward
/// `name: run-awaiting-approval` + `reason: user-form` + that id. NativeChat never watches
/// the transcript live stream, so an after-the-fact append does not unblock them.
///
/// `request_user_form` TOOL_CALL frames are held until that stamp: NativeChat paints Website
/// login from TOOL_CALL and falls back to the raw `toolCallId` (`call-…`) when `entryId` is
/// missing. Streaming those frames during the model completion is how a second same-title
/// card stayed `call-*-1` with Continue that could not submit.
pub(super) struct AgUiSink {
    pub(super) tx: tokio::sync::mpsc::UnboundedSender<Event>,
    pub(super) gateway: crate::host_state::HostState,
    pub(super) coworker_id: Option<CoworkerId>,
    pub(super) account_id: Option<opengrok_core::id::AccountId>,
    pub(super) thread_id: String,
    /// The plugins this turn's message tagged: a form-mode card may be opened on one of them
    /// although it is switched off, as its tools may be called.
    pub(super) tagged: Vec<String>,
    pub(super) form_hold: Mutex<crate::agui::user_form::UserFormSseHold>,
}

impl AgUiSink {
    fn send(&self, event: Event) -> bool {
        self.tx.send(event).is_ok()
    }

    fn hold_or_pass(&self, event: Event) -> Option<Event> {
        let Ok(mut hold) = self.form_hold.lock() else {
            return Some(event);
        };
        hold.push(event)
    }

    fn release_form(&self, call_id: &str, entry_id: Option<&str>) -> Vec<Event> {
        let Ok(mut hold) = self.form_hold.lock() else {
            return Vec::new();
        };
        hold.release_for(call_id, entry_id)
    }

    fn release_held_forms(&self, clean: bool) -> Vec<Event> {
        let Ok(mut hold) = self.form_hold.lock() else {
            return Vec::new();
        };
        hold.release_at_end(clean)
    }
}

#[async_trait::async_trait]
impl EventSink for AgUiSink {
    async fn emit(&self, events: &[Event]) {
        for event in events {
            let mut event = event.clone();
            if crate::agui::user_form::is_live_user_form_custom(&event) {
                if let (Some(coworker_id), Some(account_id)) = (&self.coworker_id, &self.account_id)
                {
                    crate::agui::resume::stamp_user_form_entry_id(
                        &self.gateway,
                        coworker_id,
                        account_id,
                        &mut event,
                    )
                    .await;
                }
                let call_id = event
                    .extra
                    .get("callId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let entry_id = event
                    .extra
                    .get("entryId")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                for held in self.release_form(&call_id, entry_id.as_deref()) {
                    if !self.send(held) {
                        return;
                    }
                }
                if !self.send(event) {
                    return;
                }
                continue;
            }
            // A form-mode call answered at once: its card is minted now, from the plugin, and
            // its held frames go out carrying the card's id, then the card itself.
            let form_call = self
                .form_hold
                .lock()
                .ok()
                .and_then(|h| h.form_mode_call(&event));
            if let (Some((call_id, arguments)), Some(coworker), Some(account)) =
                (form_call, &self.coworker_id, &self.account_id)
            {
                let who = (account, coworker);
                let thread = (self.thread_id.as_str(), self.tagged.as_slice());
                let entry = opengrok_integrations::form_mode::mint(
                    &self.gateway,
                    who,
                    thread,
                    &call_id,
                    &arguments,
                )
                .await;
                let entry_id = entry
                    .as_ref()
                    .and_then(|e| e.get("id"))
                    .and_then(|id| id.as_str());
                let mut out = self.release_form(&call_id, entry_id);
                if let Some(id) = entry_id {
                    event
                        .extra
                        .insert("entryId".to_string(), serde_json::json!(id));
                }
                out.push(event);
                let frame = entry
                    .as_ref()
                    .map(opengrok_tools::user_form::replay::agui_user_form_frame);
                out.extend(frame.and_then(|frame| serde_json::from_value(frame).ok()));
                if !out.into_iter().all(|event| self.send(event)) {
                    return;
                }
                continue;
            }
            if matches!(
                event.event_type,
                opengrok_wire::agui::EventType::RunFinished
                    | opengrok_wire::agui::EventType::RunError
            ) {
                let clean = event.event_type == opengrok_wire::agui::EventType::RunFinished;
                for held in self.release_held_forms(clean) {
                    if !self.send(held) {
                        return;
                    }
                }
                if !self.send(event) {
                    return;
                }
                continue;
            }
            if let Some(event) = self.hold_or_pass(event)
                && !self.send(event)
            {
                return;
            }
        }
    }
}

/// Wrap an event stream in the SSE response openbot expects.
pub(super) fn sse<S, E>(events: S) -> Response
where
    S: Stream<Item = Result<Event, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    use futures::StreamExt;

    let body = events.map(|event| {
        event.map(|mut event| {
            // Every card that leaves live says what its call will do (#249).
            crate::cards::stamp_summary(&mut event);
            // An event that will not serialise is dropped rather than allowed to panic mid-run;
            // `to_sse_frame` returns None and the stream continues.
            event.to_sse_frame().unwrap_or_default()
        })
    });

    streaming(axum::body::Body::from_stream(
        crate::shutdown::until_shutdown(body),
    ))
}

/// An SSE response. Without these headers a proxy may buffer the whole run and deliver it at the
/// end, which looks exactly like a server that never streamed.
pub(super) fn streaming(body: axum::body::Body) -> Response {
    let headers = [
        (header::CONTENT_TYPE, "text/event-stream"),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONNECTION, "keep-alive"),
        (HeaderName::from_static("x-accel-buffering"), "no"),
    ];
    (StatusCode::OK, headers, body).into_response()
}
