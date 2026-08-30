//! The org-admin surface for computer credentials — box.ascii.dev and (later) Windows 365.
//!
//! Per the identity model, box/W365 credentials belong to the ORGANIZATION, not a person, and are
//! configured by the org admin on the dashboard — never entered in the desktop client. The key is
//! sealed in the vault and never leaves the server: these endpoints return only WHICH kinds are
//! configured, never the secret. Provisioning opens the org's key at box-create time.
//!
//! Admin-only: every route is gated by `account_api::admin_org`, the same check the user/invite
//! admin endpoints use (cookie session or bearer, caller must be their org's admin).

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::account_api::admin_org;
use crate::agui::AgUiState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The computer kinds an org admin can configure, and their display labels. Local VM is NOT here —
/// it is server-provided and needs no credential.
const CONFIGURABLE: &[(&str, &str)] = &[("ascii", "box.ascii.dev"), ("windows365", "Windows 365")];

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/admin/computers", get(status))
        .route("/admin/computers/{kind}", post(set).delete(clear))
        .with_state(state)
}

/// `GET /admin/computers` — which org-configurable kinds are set up. Secrets never appear here.
async fn status(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    let (org_id, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let configured = state
        .auth
        .store
        .org_computer_kinds(org_id.as_str())
        .await
        .unwrap_or_default();
    let computers: Vec<_> = CONFIGURABLE
        .iter()
        .map(|(kind, label)| {
            json!({
                "kind": kind,
                "label": label,
                "configured": configured.iter().any(|k| k == kind),
            })
        })
        .collect();
    Json(json!({ "computers": computers })).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetCredential {
    /// The box.ascii.dev API key. (Windows 365 needs a richer body; not accepted yet.)
    api_key: String,
}

/// `POST /admin/computers/{kind}` — set the org's credential for a kind. Sealed in the vault.
async fn set(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Json(body): Json<SetCredential>,
) -> Response {
    let (org_id, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    // Only box.ascii.dev is a single-key credential today; Windows 365 needs its own richer form.
    if kind != "ascii" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "only box.ascii.dev (ascii) can be configured this way yet",
        )
            .into_response();
    }
    let Some(vault) = state.vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the credential vault is not configured on this server (set OG_CREDENTIAL_KEK)",
        )
            .into_response();
    };
    let key = body.api_key.trim();
    if key.is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "an API key is required").into_response();
    }
    match state
        .auth
        .store
        .set_org_computer_secret(vault, org_id.as_str(), &kind, key, now_ms())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not store an org computer credential");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not store the credential",
            )
                .into_response()
        }
    }
}

/// `DELETE /admin/computers/{kind}` — clear the org's credential for a kind.
async fn clear(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
) -> Response {
    let (org_id, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    match state
        .auth
        .store
        .clear_org_computer_secret(org_id.as_str(), &kind)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not clear the credential",
        )
            .into_response(),
    }
}
