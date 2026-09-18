//! Site-login credential protocol — AG-UI REST and gateway twin.
//!
//! WHY THIS IS NOT THE VAULT. Connector credentials stay in `opengrok-store::Vault`. A site
//! password must never be stored, journaled, or shown to the model. NativeChat brokers the
//! login out of agent view; the box gets cookies/session only. `credential.result` status
//! `filled` means that authenticated session is ready — not that a password was typed.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_tools::credential::{
    CredentialStatus, origin_from_form, result_from, scrub_secret_keys, tool_result_content,
    username_from_shared,
};
use opengrok_tools::user_form::FormRequest;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::GatewayState;
use super::user_form::{journal_agui_custom, pending_suspended, resume_settled};

pub fn agui_router(state: GatewayState) -> Router {
    Router::new()
        .route("/ag-ui/credential/result", post(agui_result))
        .with_state(state)
}

async fn agui_result(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = crate::agui::routes::account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = submit_credential_result(&state, &args, &account_id).await;
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

pub async fn result_for_caller(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(account) = state
        .agui
        .auth
        .store
        .account_by_email(caller)
        .await
        .ok()
        .flatten()
    else {
        return (
            401,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    submit_credential_result(state, args, &account.id).await
}

/// `POST /ag-ui/credential/result` and gateway `submitCredentialResult`.
/// `{ status: filled|denied|missing|error, credentialId?, requestId?, agentId }`.
/// Status only. A password in the body is dropped, never stored.
/// LOCKED: `filled` means the authenticated session is ready (cookies/profile applied to the
/// box after NativeChat broker). It does NOT mean a password was typed into the box.
/// `session_established` is accepted as an alias and canonicalized to `filled`.
pub async fn submit_credential_result(
    state: &GatewayState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let scrubbed = scrub_secret_keys(args);
    let Some(agent_id) = scrubbed
        .get("agentId")
        .or_else(|| scrubbed.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
    else {
        return (400, json!({ "error": "agentId is required" }));
    };
    let coworker_id = CoworkerId::from_stored(agent_id.clone());
    if !may_use(state, account_id, &coworker_id).await {
        return (200, Value::Null);
    }
    let Some(parsed) = result_from(&scrubbed) else {
        return (
            400,
            json!({ "error": "status must be filled, denied, missing, or error" }),
        );
    };

    let Some((run_id, _run, _seq, pending)) = pending_suspended(
        state,
        account_id,
        &coworker_id,
        opengrok_core::run::SuspendReason::Credential,
    )
    .await
    else {
        return (404, json!({ "error": "no pending credential request" }));
    };
    if let Some(request_id) = parsed.request_id.as_deref()
        && request_id != pending.call_id
    {
        return (
            400,
            json!({ "error": "requestId does not match the pending call" }),
        );
    }

    if parsed.status.session_ready()
        && let Some(credential_id) = parsed.credential_id.as_deref()
    {
        let origin = opengrok_tools::credential::origin_of(&pending.arguments).unwrap_or_default();
        let username =
            opengrok_tools::credential::username_of(&pending.arguments).unwrap_or_default();
        if !origin.is_empty()
            && let Err(error) = state
                .agui
                .auth
                .store
                .upsert_credential_hint(
                    account_id,
                    &coworker_id,
                    &origin,
                    &username,
                    credential_id,
                    now_ms(),
                )
                .await
        {
            tracing::error!(%error, "could not remember credential metadata");
        }
    }

    let content = tool_result_content(parsed.status);
    let resumed = resume_settled(
        state,
        account_id,
        &coworker_id,
        &agent_id,
        opengrok_core::run::SuspendReason::Credential,
        content,
    )
    .await;
    if !resumed {
        return (
            409,
            json!({ "error": "the credential request was already answered" }),
        );
    }
    (
        200,
        json!({
            "ok": true,
            "status": parsed.status.as_str(),
            "requestId": pending.call_id,
            "runId": run_id.as_str(),
        }),
    )
}

/// After a successful user-form fill, ask NativeChat to save origin+username. Never a password.
pub async fn offer_save_after_submit(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    _agent_id: &str,
    form_entry_id: &str,
    form: &FormRequest,
    shared: &BTreeMap<String, String>,
) {
    let Some(origin) = origin_from_form(form) else {
        return;
    };
    let username = username_from_shared(shared);
    let frame = opengrok_tools::credential::offer_save_frame(&origin, &username, form_entry_id);
    journal_agui_custom(
        state,
        account_id,
        coworker_id,
        opengrok_core::run::SuspendReason::UserForm,
        frame,
    )
    .await;
}

pub fn spawn_credential_hold_timeout(
    state: GatewayState,
    account_id: AccountId,
    coworker_id: CoworkerId,
    agent_id: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(super::user_form::FORM_HOLD_TIMEOUT).await;
        timeout_credential_request(&state, &account_id, &coworker_id, &agent_id).await;
    });
}

async fn timeout_credential_request(
    state: &GatewayState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    agent_id: &str,
) -> bool {
    if pending_suspended(
        state,
        account_id,
        coworker_id,
        opengrok_core::run::SuspendReason::Credential,
    )
    .await
    .is_none()
    {
        return false;
    }
    resume_settled(
        state,
        account_id,
        coworker_id,
        agent_id,
        opengrok_core::run::SuspendReason::Credential,
        tool_result_content(CredentialStatus::Missing),
    )
    .await
}

async fn may_use(state: &GatewayState, account_id: &AccountId, coworker_id: &CoworkerId) -> bool {
    state
        .agui
        .auth
        .store
        .may_use_coworker(account_id, coworker_id)
        .await
        .unwrap_or(false)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use opengrok_tools::credential::offer_save_payload;

    #[test]
    fn a_result_body_with_a_password_is_scrubbed_before_parse() {
        let dirty = json!({
            "agentId": "cw_1",
            "status": "filled",
            "credentialId": "cred_1",
            "password": "s3cret-should-never-land"
        });
        let clean = scrub_secret_keys(&dirty);
        assert!(clean.get("password").is_none(), "{clean}");
        let parsed = result_from(&clean).expect("status");
        assert_eq!(parsed.status, CredentialStatus::Filled);
        assert_eq!(parsed.credential_id.as_deref(), Some("cred_1"));
        let alias = result_from(&json!({
            "agentId": "cw_1",
            "status": "session_established"
        }))
        .expect("session_established is an alias of filled");
        assert_eq!(alias.status, CredentialStatus::Filled);
        assert_eq!(alias.status.as_str(), "filled");
    }

    #[test]
    fn offer_save_payload_is_origin_username_and_entry_only() {
        let payload = offer_save_payload("example.com", "ada", "e_1");
        assert_eq!(payload["origin"], "example.com");
        assert_eq!(payload["username"], "ada");
        assert_eq!(payload["formEntryId"], "e_1");
        assert_eq!(payload.as_object().map(|o| o.len()), Some(3));
    }
}
