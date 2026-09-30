//! `/account/inference-source`: where a person's turns are answered, and the two reads a turn
//! makes to find out (`local_proxy::Saved`). The setting is the account's
//! (`AccountEvent::InferenceSourceSet`); the proxy's key is sealed in the vault and never leaves
//! this process in a reply or a log. The rules are `opengrok_harness::local_proxy` and
//! `opengrok_core::inference`. IT WORKS ONLY WHERE THE PROXY RUNS ON THE SERVER'S OWN MACHINE: a
//! remote deployment needs a relay (NativeChat carrying the call, as local-exec does).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use opengrok_core::account::AccountCommand;
use opengrok_core::id::AccountId;
use opengrok_core::inference::{InferenceSource, SourceKind};
use opengrok_harness::local_proxy::{self, KeyChange};
use serde_json::{Value, json};

use crate::agui::AgUiState;

pub fn router(state: AgUiState) -> Router {
    let route = get(get_source).put(put_source);
    Router::new()
        .route("/account/inference-source", route)
        .with_state(state)
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
) -> Result<Option<SourceKind>, Box<Response>> {
    let refused = |why| Box::new(refuse(StatusCode::BAD_REQUEST, format!("{field} {why}")));
    SourceKind::named(value).map_err(refused)
}

fn refuse(status: StatusCode, sentence: impl Into<String>) -> Response {
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
}

/// `GET /account/inference-source` — the signed-in person's own setting (`local_proxy::described`).
async fn get_source(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    match crate::account_api::caller(&state.auth, &headers).await {
        Ok((_, account, _)) => {
            Json(local_proxy::described(&account.inference_source).await).into_response()
        }
        Err(refusal) => signed_out(refusal),
    }
}

/// `PUT /account/inference-source` — `{kind, baseUrl?, localModel?, apiKey?}` (what each does is
/// `local_proxy::apply`), answered as `GET` answers.
async fn put_source(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match save(&state, &headers, &body).await {
        Ok(saved) => Json(local_proxy::described(&saved).await).into_response(),
        Err(refusal) => refusal,
    }
}

async fn save(
    state: &AgUiState,
    headers: &HeaderMap,
    body: &Value,
) -> Result<InferenceSource, Response> {
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
    Ok(after.inference_source)
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
