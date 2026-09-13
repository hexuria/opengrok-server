//! The org-admin surface for computer credentials — box.ascii.dev, grok-box, and (later) Windows 365.
//!
//! Per the identity model, box/W365 credentials belong to the ORGANIZATION, not a person, and are
//! configured by the org admin on the dashboard — never entered in the desktop client. The key is
//! sealed in the vault and never leaves the server: these endpoints return only WHICH kinds are
//! configured, never the secret. Provisioning opens the org's key at box-create time.
//!
//! grok-box has no vendor API key. The sealed value is the guest image name (or `"enabled"`). The
//! per-box `BOX_TOKEN` is minted at create and never stored here — it must not reach the browser.
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
/// it is server-provided and needs no credential. grok-box IS here even though it also runs on the
/// server host: enabling it is how an admin chooses the self-hosted guest over debian-via-exec or
/// box.ascii.dev, and the sealed value is the image name, never a BOX_TOKEN.
const CONFIGURABLE: &[(&str, &str)] = &[
    ("ascii", "box.ascii.dev"),
    ("grok-box", "grok-box (self-hosted)"),
    ("windows365", "Windows 365"),
];

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
    let (org_id, _, _) = match admin_org(&state.auth, &headers).await {
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
    /// The box.ascii.dev API key. Empty for grok-box (that kind has no vendor key).
    #[serde(default)]
    api_key: String,
    /// grok-box guest image. Empty means the deployment default (`OG_GROK_BOX_IMAGE` / `grok-box:local`).
    #[serde(default)]
    image: String,
}

/// `POST /admin/computers/{kind}` — set the org's credential for a kind. Sealed in the vault.
/// For grok-box the sealed value is the image name, not a BOX_TOKEN (those are minted per box).
async fn set(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Json(body): Json<SetCredential>,
) -> Response {
    let (org_id, _, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    match kind.as_str() {
        "ascii" => set_ascii(&state, org_id.as_str(), &body.api_key).await,
        "grok-box" => set_grok_box(&state, org_id.as_str(), &body.image).await,
        _ => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "only box.ascii.dev (ascii) and grok-box can be configured this way yet",
        )
            .into_response(),
    }
}

async fn set_ascii(state: &AgUiState, org_id: &str, api_key: &str) -> Response {
    let Some(vault) = state.vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the credential vault is not configured on this server (set OG_CREDENTIAL_KEK)",
        )
            .into_response();
    };
    let key = api_key.trim();
    if key.is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "an API key is required").into_response();
    }
    match state
        .auth
        .store
        .set_org_computer_secret(vault, org_id, "ascii", key, now_ms())
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

async fn set_grok_box(state: &AgUiState, org_id: &str, image: &str) -> Response {
    if !crate::agui::provision::local_docker_allowed() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "grok-box is a self-hosted computer; this hosted deployment does not run guest containers on the API host",
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
    let image = image.trim();
    if image.contains('\n') || image.contains('\0') || image.len() > 256 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "that is not a usable grok-box image name",
        )
            .into_response();
    }
    let stored = if image.is_empty() {
        opengrok_box::grok_box::DEFAULT_IMAGE
    } else {
        image
    };
    match state
        .auth
        .store
        .set_org_computer_secret(vault, org_id, "grok-box", stored, now_ms())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not store the grok-box image");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not store the configuration",
            )
                .into_response()
        }
    }
}

/// `POST /admin/computers/{kind}/test` — prove the org's computer works by provisioning a
/// throwaway box and destroying it. For ascii this is the confidence check when a key is saved.
/// For grok-box it is "the image is present, Docker can run it, and GET /v1/ready answers".
/// The reply never includes BOX_TOKEN.
async fn test(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
) -> Response {
    let (org_id, _, _) = match admin_org(&state.auth, &headers).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    match kind.as_str() {
        "ascii" => test_ascii(&state, org_id.as_str()).await,
        "grok-box" => test_grok_box(&state, org_id.as_str()).await,
        _ => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "only box.ascii.dev (ascii) and grok-box can be tested yet",
        )
            .into_response(),
    }
}

async fn test_ascii(state: &AgUiState, org_id: &str) -> Response {
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
        .org_computer_secret(vault, org_id, "ascii")
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

async fn test_grok_box(state: &AgUiState, org_id: &str) -> Response {
    if !crate::agui::provision::local_docker_allowed() {
        return Json(json!({
            "ok": false,
            "detail": "grok-box is a self-hosted computer; this hosted deployment does not run guest containers on the API host.",
        }))
        .into_response();
    }
    let lookup = crate::agui::provision::lookup_provider(state, Some(org_id), "grok-box").await;
    let Some(provider) = lookup.computer else {
        let message = lookup
            .error
            .map(|(_, message)| message)
            .unwrap_or_else(|| "grok-box is not available on this server".to_string());
        return Json(json!({ "ok": false, "detail": message })).into_response();
    };
    match provider.create(None).await {
        Ok(box_id) => {
            let woke = provider
                .wake(&box_id, std::time::Duration::from_secs(90))
                .await;
            let screen = match &woke {
                Ok(state) if state == "running" => provider
                    .screen_url(&box_id)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "(no screen yet)".to_string()),
                _ => String::new(),
            };
            let destroyed = provider.destroy(&box_id).await;
            // Never include BOX_TOKEN. The screen URL may carry the independent VNC password —
            // that is what the desktop viewer uses — but we still do not log it.
            match (woke, destroyed) {
                (Ok(state), Ok(())) if state == "running" => Json(json!({
                    "ok": true,
                    "detail": format!("Created, reached ready, and destroyed a grok-box. Screen: {screen}"),
                }))
                .into_response(),
                (Ok(state), Ok(())) => Json(json!({
                    "ok": false,
                    "detail": format!("Created a grok-box but it did not become ready (last state: {state}). Destroyed it."),
                }))
                .into_response(),
                (Err(error), _) => Json(json!({
                    "ok": false,
                    "detail": format!("Created a grok-box but it did not become ready: {error}. Destroyed it."),
                }))
                .into_response(),
                (_, Err(error)) => Json(json!({
                    "ok": false,
                    "detail": format!("Created a grok-box but could not delete it: {error}."),
                }))
                .into_response(),
            }
        }
        Err(error) => Json(json!({
            "ok": false,
            "detail": format!("Could not create a grok-box: {error}. Build the guest image from hexuria/box (`docker compose build`) and set OG_GROK_BOX_IMAGE if it is not tagged grok-box:local."),
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
    let (org_id, _, _) = match admin_org(&state.auth, &headers).await {
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
    let (org_id, _, _) = match admin_org(&state.auth, &headers).await {
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
    let (org_id, _, _) = match admin_org(&state.auth, &headers).await {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::CONFIGURABLE;

    #[test]
    fn grok_box_is_a_configurable_kind_and_ascii_still_is() {
        assert!(
            CONFIGURABLE.iter().any(|(kind, _)| *kind == "ascii"),
            "ascii must stay configurable"
        );
        assert!(
            CONFIGURABLE.iter().any(|(kind, _)| *kind == "grok-box"),
            "grok-box must be configurable from the admin console"
        );
    }
}
