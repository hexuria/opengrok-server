//! `/account/inference-source`: where a person's turns are answered, and the reads a turn makes
//! to find out (`local_proxy::Saved`). The setting is the account's
//! (`AccountEvent::InferenceSourceSet`); the proxy's key is sealed in the vault and never leaves
//! this process in a reply or a log. The rules are `opengrok_harness::local_proxy` and
//! `opengrok_core::inference`. By the loopback it works only where the proxy runs on the
//! server's own machine; anywhere else the person's own Mac carries the call, on the two
//! `/inference-relay` routes below (`opengrok_harness::relay`, #292).

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use opengrok_core::account::AccountCommand;
use opengrok_core::id::AccountId;
use opengrok_core::inference::{InferenceSource, TurnSource};
use opengrok_harness::local_proxy::{self, KeyChange};
use opengrok_harness::relay::{Piped, Refused};
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::host_state::HostState;

pub fn router(state: AgUiState) -> Router {
    let route = get(get_source).put(put_source);
    Router::new()
        .route("/account/inference-source", route)
        .with_state(state)
}

/// The Mac relay's routes. They take the daemon token local-exec enrols a machine with, and
/// nothing else: it names the account and the machine, which only ever serves that account.
pub fn relay_router(host: HostState) -> Router {
    let answer = post(relay_response);
    Router::new()
        .route("/inference-relay/requests", get(relay_requests))
        .route("/inference-relay/responses/{request_id}", answer)
        .with_state(host)
}

type Answer = Result<Response, Response>;

/// The account and machine a daemon token names, or the 401 a relay route answers without one.
async fn machine(host: &HostState, headers: &HeaderMap) -> Result<(String, String), Response> {
    let named = crate::local_exec::daemon_from_bearer(&host.agui.auth, headers).await;
    named.ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "enrol this machine first"))
}

/// `GET /inference-relay/requests` — a person's Mac holds this open to carry their turns
/// (`RelayFrame`). Opening it is the Mac saying it can: sends held for it go now (`drain_held`).
async fn relay_requests(State(host): State<HostState>, headers: HeaderMap) -> Answer {
    let (account, machine) = machine(&host, &headers).await?;
    let frames = host.agui.auth.relay.connect(&account, &machine);
    // A TOKEN RETIRED AS ITS STREAM OPENED KEEPS NO STREAM: revoke and re-enrolment close the
    // machine's stream after its row changes, and this connect may have come after that close.
    // Dropped here, the stream is closed to the broker before it is sent a frame.
    self::machine(&host, &headers).await?;
    let held = crate::agui::pending::drain_held(host.clone(), AccountId::from_stored(account));
    tokio::spawn(held);
    let frames = frames.map(|frame| Ok::<_, Infallible>(Event::default().data(frame)));
    Ok(Sse::new(frames).into_response())
}

/// `POST /inference-relay/responses/{request_id}` — the answer to one frame, from the machine it
/// went to: opencodex's SSE, piped into the run as it arrives, or JSON (a model list, `{error}`).
/// 204; 404 for an id nothing waits on; 409 answered already; 401; 413 past `MAX_ANSWER_BYTES`.
async fn relay_response(
    State(host): State<HostState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    body: axum::body::Body,
) -> Answer {
    let (account, machine) = machine(&host, &headers).await?;
    let answering = host.agui.auth.relay.answer(&account, &machine, &request_id);
    let answering = answering.map_err(|refused| match refused {
        Refused::Unknown => refuse(StatusCode::NOT_FOUND, "nothing waits on that id"),
        Refused::Answered => refuse(StatusCode::CONFLICT, "that was answered already"),
        Refused::NotYours => refuse(StatusCode::UNAUTHORIZED, "not sent to this machine"),
    })?;
    let piped = answering.pipe(streams(&headers), body.into_data_stream());
    match piped.await {
        Piped::Accepted => Ok(StatusCode::NO_CONTENT.into_response()),
        Piped::TooLarge => Err(refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "that answer is too large",
        )),
    }
}

/// Whether a body is SSE by its `Content-Type`, which, as every media type, is case-insensitive
/// (RFC 9110 §8.3.1): a Mac that says `Text/Event-Stream` is streaming all the same.
pub(crate) fn streams(headers: &HeaderMap) -> bool {
    const SSE: &[u8] = b"text/event-stream";
    let kind = headers.get(CONTENT_TYPE).map(HeaderValue::as_bytes);
    let head = kind.and_then(|kind| kind.get(..SSE.len()));
    head.is_some_and(|head| head.eq_ignore_ascii_case(SSE))
}

/// The account's connected Mac and the label it was enrolled under, for `relay` on a read.
async fn mac(state: &AgUiState, account: &AccountId) -> Option<(String, Option<String>)> {
    let machine = state.auth.relay.connected(account.as_str())?;
    let enrolled = state.auth.store.list_daemons(account.as_str()).await;
    let mut rows = enrolled.unwrap_or_default().into_iter();
    let label = rows.find_map(|(id, label, ..)| (id == machine).then_some(label));
    Some((machine, label.filter(|label| !label.is_empty())))
}

/// Where the proxy's key is sealed. The vault binds this id into the ciphertext, so it is part of
/// the credential: a new spelling for an existing row loses the key rather than moving it.
fn key_id(account: &AccountId) -> String {
    format!("inference-proxy-key:{}", account.as_str())
}

/// A source a request names (`inferenceSource`, `?source=`), or the 400 saying what it may name.
pub(crate) fn named(
    value: Option<&Value>,
    field: &str,
) -> Result<Option<TurnSource>, Box<Response>> {
    TurnSource::named(value, field).map_err(|why| Box::new(refuse(StatusCode::BAD_REQUEST, why)))
}

pub(crate) fn refuse(status: StatusCode, sentence: impl Into<String>) -> Response {
    (status, Json(json!({ "error": sentence.into() }))).into_response()
}

/// A caller `account_api::caller` turned away, as JSON: the app reads a bare-text refusal as a
/// proxy in front of the server, and loses the sentence (#262).
fn signed_out(_: Response) -> Response {
    refuse(
        StatusCode::UNAUTHORIZED,
        "sign in to see your inference source",
    )
}

#[async_trait::async_trait]
impl local_proxy::Saved for AgUiState {
    async fn setting(&self, account: &AccountId) -> Option<InferenceSource> {
        let read = self.auth.store.load_account(account).await;
        let read =
            read.inspect_err(|error| tracing::warn!(%error, "an inference source was not read"));
        read.ok().map(|(account, _)| account.inference_source)
    }

    /// A store fault stays in the log: the reason a person reads is one they can act on.
    async fn key(&self, account: &AccountId, saved: bool) -> Result<Option<String>, String> {
        let vault = match (saved, self.vault.as_ref()) {
            (false, _) => return Ok(None),
            (true, None) => return Err("this server has no vault to open it with".to_string()),
            (true, Some(vault)) => vault,
        };
        let id = key_id(account);
        match self.auth.store.open_credential(vault, &id).await {
            Ok(Some(key)) => Ok(Some(key)),
            Ok(None) => Err("it is missing".to_string()),
            Err(opengrok_store::StoreError::Unopenable(why)) => Err(why),
            Err(error) => {
                tracing::error!(%error, "a proxy key could not be read");
                Err("it could not be read".to_string())
            }
        }
    }

    fn relay(&self) -> std::sync::Arc<opengrok_harness::relay::RelayBroker> {
        self.auth.relay.clone()
    }
}

/// `GET /account/inference-source` — the signed-in person's own setting (`local_proxy::described`).
async fn get_source(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    match crate::account_api::caller(&state.auth, &headers).await {
        Ok((id, account, _)) => described(&state, &id, &account.inference_source).await,
        Err(refusal) => signed_out(refusal),
    }
}

async fn described(state: &AgUiState, id: &AccountId, source: &InferenceSource) -> Response {
    Json(local_proxy::described(source, mac(state, id).await).await).into_response()
}

/// `PUT /account/inference-source` — `{kind, via?, baseUrl?, localModel?, apiKey?, relay?}` (what
/// each does is `local_proxy::apply`), answered as `GET` answers.
async fn put_source(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match save(&state, &headers, &body).await {
        Ok((id, saved)) => described(&state, &id, &saved).await,
        Err(refusal) => refusal,
    }
}

async fn save(
    state: &AgUiState,
    headers: &HeaderMap,
    body: &Value,
) -> Result<(AccountId, InferenceSource), Response> {
    let caller = crate::account_api::caller(&state.auth, headers).await;
    let (id, account, seq) = caller.map_err(signed_out)?;
    let (source, key) = local_proxy::apply(&account.inference_source, body)
        .map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?;
    if let KeyChange::Set(key) = &key {
        seal(state, &id, key).await?;
    }
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = account
        .decide(AccountCommand::SetInferenceSource { source, at_ms })
        .map_err(|why| refuse(StatusCode::UNPROCESSABLE_ENTITY, why.to_string()))?;
    let after = crate::account_api::persist(&state.auth, &id, account, seq, &events).await?;
    // DROPPED ONLY ONCE THE SETTING SAYS THERE IS NO KEY: a delete that fails leaves a row no turn
    // reads, where the other order could leave a setting naming a key that is gone.
    if let KeyChange::Clear = key
        && let Err(error) = state.auth.store.delete_secret(&key_id(&id)).await
    {
        tracing::warn!(%error, "a cleared proxy key could not be dropped");
    }
    Ok((id, after.inference_source))
}

/// Seal the proxy's key before the setting says there is one. With no vault there is nowhere to
/// keep it, and nothing is saved.
async fn seal(state: &AgUiState, account: &AccountId, key: &str) -> Result<(), Response> {
    let Some(vault) = state.vault.as_ref() else {
        return Err(refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no vault (OG_CREDENTIAL_KEK) to keep your proxy's key in, so nothing \
             was saved; save the setting without a key, or ask whoever runs this server to set one",
        ));
    };
    let (id, at_ms) = (key_id(account), chrono::Utc::now().timestamp_millis());
    let kept = match vault.seal(&id, key) {
        Ok(sealed) => state.auth.store.put_secret(&id, &sealed, at_ms).await,
        Err(error) => Err(error),
    };
    kept.map_err(|error| {
        tracing::error!(%error, "a proxy key could not be sealed");
        let why = "your proxy's key could not be kept, so nothing was saved; try again in a moment";
        refuse(StatusCode::SERVICE_UNAVAILABLE, why)
    })
}
