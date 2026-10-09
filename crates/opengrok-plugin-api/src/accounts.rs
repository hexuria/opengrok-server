//! A person's accounts beside their installs (#359): renaming one, and which one each Bot uses for
//! a service. Shapes agreed with NativeChat for #185: camelCase, every refusal `{"error"}`.
//! Signing in, reconnecting and removing an account stay with the OAuth flow in the server; axum
//! merges this `PATCH /connections/{id}` with its `DELETE`.
use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, put},
};
use opengrok_core::connection::ConnectionError;
use opengrok_core::id::CoworkerId;
use opengrok_integrations::accounts::{self, PinError, RenameError};
use serde::Deserialize;

use crate::{RegistryState, refused};

pub(crate) fn router() -> Router<RegistryState> {
    Router::new()
        .route("/connections/pins", get(pins))
        .route("/connections/{id}", patch(rename))
        .route(
            "/coworkers/{coworker_id}/pins/{connector}",
            put(pin).delete(unpin),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Label {
    label: String,
}

/// `{"label"}`, trimmed; 422 when that leaves it empty or longer than 80 characters. The reply is
/// the account as `GET /connections` lists it.
async fn rename(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: Result<Json<Label>, JsonRejection>,
) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    let Ok(Json(Label { label })) = request else {
        return refused(422, &ConnectionError::BadLabel.to_string());
    };
    let at_ms = chrono::Utc::now().timestamp_millis();
    match accounts::rename(&s.store, &account, &id, label, at_ms).await {
        Ok(view) => Json(view).into_response(),
        Err(RenameError::NotFound) => refused(404, "no such connection"),
        Err(RenameError::Refused(error @ ConnectionError::BadLabel)) => {
            refused(422, &error.to_string())
        }
        Err(RenameError::Refused(error)) => refused(409, &error.to_string()),
        Err(RenameError::Store(_)) => refused(503, "the account could not be renamed"),
    }
}

/// `[{"coworkerId", "connector", "connectionId"}]` for the caller's Bots.
async fn pins(State(s): State<RegistryState>, headers: HeaderMap) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    match accounts::pins(&s.store, &account).await {
        Ok(pins) => Json(pins).into_response(),
        Err(_) => refused(503, "pins could not be read"),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Choice {
    connection_id: String,
}

/// `{"connectionId"}`; the reply is the pin. 404 for a Bot that is not the caller's, 422 for an
/// account that Bot cannot use or that is for another service.
async fn pin(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    Path((coworker, connector)): Path<(String, String)>,
    request: Result<Json<Choice>, JsonRejection>,
) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    let Ok(Json(Choice { connection_id })) = request else {
        return refused(422, "send connectionId only");
    };
    let bot = CoworkerId::from_stored(coworker);
    match accounts::pin(&s.store, &account, &bot, &connector, &connection_id).await {
        Ok(pin) => Json(pin).into_response(),
        Err(error) => pin_refused(&error),
    }
}

/// 204 whether or not there was a pin: either way the Bot is now unpinned for that service.
async fn unpin(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    Path((coworker, connector)): Path<(String, String)>,
) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    let bot = CoworkerId::from_stored(coworker);
    match accounts::unpin(&s.store, &account, &bot, &connector).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => pin_refused(&error),
    }
}

fn pin_refused(error: &PinError) -> Response {
    match error {
        PinError::NoSuchBot => refused(404, "no such coworker"),
        PinError::Unusable(_) => refused(422, &error.to_string()),
        PinError::Store(_) => refused(503, "the pin could not be saved"),
    }
}
