//! A person's saved site logins, kept on the server so they follow them to every Mac.
//!
//! The rows are the person's own: every route reads the account from the bearer and never
//! from the body, and a row that is not theirs is "no such row". The password is sealed in
//! the vault under a key that carries the account id, and it leaves the server in exactly one
//! place — `POST /site-logins/{id}/reveal`, which NativeChat calls after the person passed
//! Touch ID on their Mac, to fill a login card. A list never carries it.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use opengrok_core::id::AccountId;
use serde_json::{Value, json};

use crate::host_state::HostState;

pub fn agui_router(state: HostState) -> Router {
    Router::new()
        .route("/site-logins", get(list).post(save))
        .route("/site-logins/{id}", delete(remove))
        .route("/site-logins/{id}/reveal", post(reveal))
        .with_state(state)
}

/// Longest origin, username or password the vault takes. Nothing legitimate is longer, and a
/// bound keeps a runaway client from filling the vault.
const MAX_FIELD_CHARS: usize = 512;

fn reply(code: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

fn signed_in(state: &HostState, headers: &HeaderMap) -> Option<AccountId> {
    crate::agui::routes::account_from_bearer(&state.agui, headers)
}

fn sign_in_first() -> Response {
    (StatusCode::UNAUTHORIZED, "sign in first").into_response()
}

fn no_vault() -> Response {
    reply(
        503,
        json!({ "error": "the credential vault is not configured on this server (set OG_CREDENTIAL_KEK)" }),
    )
}

fn row_json(row: &opengrok_store::SiteLoginRow) -> Value {
    json!({
        "id": row.id,
        "origin": row.origin,
        "username": row.username,
        "label": row.label,
        "createdAtMs": row.created_at_ms,
        "updatedAtMs": row.updated_at_ms,
    })
}

async fn list(State(state): State<HostState>, headers: HeaderMap) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    match state.agui.auth.store.site_logins(&account_id).await {
        Ok(rows) => reply(200, Value::Array(rows.iter().map(row_json).collect())),
        Err(error) => {
            tracing::error!(%error, "could not list site logins");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

/// `{ origin, username, password }`. The origin is stored as NativeChat keys it (the
/// registrable site, lowercase); the server does not second-guess it.
async fn save(
    State(state): State<HostState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    let Some(vault) = state.agui.vault.as_deref() else {
        return no_vault();
    };
    let field = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.chars().count() <= MAX_FIELD_CHARS)
            .map(str::to_string)
    };
    let (Some(origin), Some(username), Some(password)) =
        (field("origin"), field("username"), field("password"))
    else {
        return reply(
            400,
            json!({ "error": "origin, username and password are required" }),
        );
    };
    let origin = origin.to_ascii_lowercase();
    match state
        .agui
        .auth
        .store
        .upsert_site_login(
            vault,
            &account_id,
            &origin,
            &username,
            &password,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
    {
        Ok(row) => {
            tracing::info!(account = %account_id, origin = %row.origin, "saved a site login");
            reply(200, row_json(&row))
        }
        Err(error) => {
            tracing::error!(%error, "could not save a site login");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

async fn remove(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    match state
        .agui
        .auth
        .store
        .delete_site_login(&account_id, &id)
        .await
    {
        Ok(true) => reply(200, json!({ "ok": true, "id": id })),
        Ok(false) => reply(404, json!({ "error": "no such site login" })),
        Err(error) => {
            tracing::error!(%error, "could not delete a site login");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

/// The one place a saved password leaves the server: to the owner's own app, which asked
/// after Touch ID. Logged without the value.
async fn reveal(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    let Some(vault) = state.agui.vault.as_deref() else {
        return no_vault();
    };
    match state
        .agui
        .auth
        .store
        .open_site_login(vault, &account_id, &id)
        .await
    {
        Ok(Some(password)) => {
            tracing::info!(account = %account_id, id = %id, "revealed a site login to its owner's app");
            reply(200, json!({ "id": id, "password": password }))
        }
        Ok(None) => reply(404, json!({ "error": "no such site login" })),
        Err(error) => {
            tracing::error!(%error, "could not open a site login");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}
