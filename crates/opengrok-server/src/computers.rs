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
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::account_api::admin_org;
use crate::agui::AgUiState;
use opengrok_box::Computer;

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
        .route("/admin/computers/{kind}/test", post(test))
        .route("/admin/computers/mode", get(get_mode).put(set_mode))
        .route(
            "/admin/computers/mode/account/{id}",
            put(set_account_mode).delete(clear_account_mode),
        )
        .with_state(state)
}

/// `GET /admin/computers` — which org-configurable kinds are set up. Secrets never appear here.
async fn status(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    let (org_id, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let configured = match state.vault.as_ref() {
        Some(vault) => state
            .auth
            .store
            .org_computer_kinds_openable(vault, org_id.as_str())
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
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

/// `POST /admin/computers/{kind}/test` — prove the org's credential works by provisioning a
/// throwaway box and destroying it. This is the confidence check when a key is saved, and it
/// settles box.ascii.dev's wire details by observation: if create+delete round-trips, the guessed
/// create-reply id field and DELETE header are right.
async fn test(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
) -> Response {
    let (org_id, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    if kind != "ascii" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "only box.ascii.dev (ascii) can be tested yet",
        )
            .into_response();
    }
    let Some(vault) = state.vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the credential vault is not configured on this server",
        )
            .into_response();
    };
    let key = match state
        .auth
        .store
        .org_computer_secret(vault, org_id.as_str(), "ascii")
        .await
    {
        Ok(Some(key)) => key,
        Ok(None) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "save a box.ascii.dev key first, then test it",
            )
                .into_response();
        }
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not read the key").into_response();
        }
    };

    let boxes = opengrok_box::AsciiBoxes::new(key);
    match boxes.create(Some(60)).await {
        Ok(box_id) => {
            let destroyed = boxes.destroy(&box_id).await;
            tracing::info!(
                box_id = %box_id,
                destroyed = destroyed.is_ok(),
                "box.ascii.dev test connection round-trip"
            );
            match destroyed {
                Ok(()) => Json(json!({
                    "ok": true,
                    "detail": "Created and destroyed a box — box.ascii.dev is reachable and the key works.",
                }))
                .into_response(),
                Err(error) => Json(json!({
                    "ok": false,
                    "detail": format!("Created a box ({box_id}) but could not delete it: {error}."),
                }))
                .into_response(),
            }
        }
        Err(error) => Json(json!({
            "ok": false,
            "detail": format!("Could not create a box: {error}"),
        }))
        .into_response(),
    }
}

const VALID_MODES: &[&str] = &["per-org", "per-account", "per-bot"];

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMode {
    mode: String,
}

/// `GET /admin/computers/mode` — the org's default sharing mode (built-in default: per-account).
async fn get_mode(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    let (org_id, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let mode = state
        .auth
        .store
        .sharing_mode("org", org_id.as_str())
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "per-account".to_string());
    Json(json!({ "mode": mode, "modes": VALID_MODES })).into_response()
}

/// `PUT /admin/computers/mode` — set the org's default sharing mode.
async fn set_mode(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(body): Json<SetMode>,
) -> Response {
    let (org_id, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    if !VALID_MODES.contains(&body.mode.as_str()) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown sharing mode").into_response();
    }
    if let Err(_error) = state
        .auth
        .store
        .set_sharing_mode("org", org_id.as_str(), &body.mode, now_ms())
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not set the mode").into_response();
    }
    // Eager warming for per-org ONLY: the whole org shares ONE box, so provisioning it now (rather
    // than at the first bot) is one box for many people, and it is ready before anyone's first bot.
    // per-account / per-bot stay lazy at first-need — idle-stop makes an eager per-seat box no
    // cheaper than a lazy one (it would just be stopped unused), so eager there buys nothing.
    let warmed = if body.mode == "per-org" {
        match crate::agui::provision::ensure_scope_box(
            &state,
            Some(org_id.as_str()),
            "org",
            org_id.as_str(),
            now_ms(),
        )
        .await
        {
            Ok(box_id) => json!({ "provisioned": true, "boxId": box_id }),
            // Non-fatal: the mode is set; the box just could not warm yet (e.g. no org key). The
            // first bot will try again, and the reason is surfaced for the admin.
            Err((code, message)) => json!({
                "provisioned": false,
                "computerError": { "code": code, "message": message },
            }),
        }
    } else {
        json!({ "provisioned": false })
    };
    Json(json!({ "mode": body.mode, "warm": warmed })).into_response()
}

/// `PUT /admin/computers/mode/account/{id}` — override the sharing mode for one member.
async fn set_account_mode(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SetMode>,
) -> Response {
    if let Err(refusal) = admin_org(&state.auth, &headers).await {
        return refusal;
    }
    if !VALID_MODES.contains(&body.mode.as_str()) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown sharing mode").into_response();
    }
    match state
        .auth
        .store
        .set_sharing_mode("account", &id, &body.mode, now_ms())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not set the override",
        )
            .into_response(),
    }
}

/// `DELETE /admin/computers/mode/account/{id}` — clear a member's override (fall back to the org default).
async fn clear_account_mode(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(refusal) = admin_org(&state.auth, &headers).await {
        return refusal;
    }
    match state.auth.store.clear_sharing_mode("account", &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not clear the override",
        )
            .into_response(),
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
