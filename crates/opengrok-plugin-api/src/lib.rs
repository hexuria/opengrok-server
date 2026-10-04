//! Registry API introduced by #356; NativeChat #184 transcribes these shapes.
use opengrok_core::id::AccountId;
use std::sync::Arc;
pub type Authenticate = Arc<dyn Fn(&HeaderMap) -> Option<AccountId> + Send + Sync>;
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
};
use opengrok_integrations::{installed, registry::Registry};
use serde::Deserialize;

#[derive(Clone)]
pub struct RegistryState {
    pub store: opengrok_store::PgStore,
    pub vault: Option<Arc<opengrok_store::Vault>>,
    pub authenticate: Authenticate,
    pub registry: Option<Registry>,
    pub reserved_names: std::collections::BTreeSet<String>,
}
/// Recording tests supply a local registry; deployment always uses GitHub's fixed roots.
pub fn router(state: RegistryState) -> Router {
    Router::new()
        .route("/plugins/catalog", get(catalog))
        .route("/plugins/catalog/{name}", get(detail))
        .route("/plugins/installations", get(list).post(install))
        .route("/plugins/installations/{name}", delete(uninstall))
        .route(
            "/plugins/installations/{name}/credentials/{connector}",
            put(credential),
        )
        .with_state(state)
}
fn refused(status: u16, why: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(serde_json::json!({"error": why})),
    )
        .into_response()
}
#[derive(Deserialize)]
struct Pin {
    revision: Option<String>,
}
async fn catalog(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    Query(pin): Query<Pin>,
) -> Response {
    if (s.authenticate)(&headers).is_none() {
        return refused(401, "sign in first");
    }
    let Some(registry) = &s.registry else {
        return refused(503, "plugin registry is not configured correctly");
    };
    match registry.catalog(pin.revision.as_deref()).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(error) => refused(502, &error.to_string()),
    }
}
async fn detail(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(pin): Query<Pin>,
) -> Response {
    if (s.authenticate)(&headers).is_none() {
        return refused(401, "sign in first");
    }
    let Some(registry) = &s.registry else {
        return refused(503, "plugin registry is unavailable");
    };
    let catalog = match registry.catalog(pin.revision.as_deref()).await {
        Ok(c) => c,
        Err(e) => return refused(502, &e.to_string()),
    };
    let Some(entry) = catalog.plugins.iter().find(|e| e.name == name) else {
        return refused(404, "no such plugin");
    };
    match registry.bundle(entry).await {
        Ok(bundle) => Json(serde_json::json!({"entry": entry, "registryRevision": catalog.revision, "parts": bundle.parts, "connectors": bundle.connectors()})).into_response(),
        Err(error) => refused(422, &error.to_string()),
    }
}
async fn list(State(s): State<RegistryState>, headers: HeaderMap) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    match installed::list(&s.store, &account).await {
        Ok(rows) => Json(rows).into_response(),
        Err(_) => refused(503, "installed plugins could not be read"),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Install {
    name: String,
    registry_revision: String,
}
async fn install(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    request: Result<Json<Install>, JsonRejection>,
) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    let request = match request {
        Ok(Json(request)) => request,
        Err(_) => return refused(422, "send name and registryRevision only"),
    };
    if s.reserved_names.contains(&request.name) {
        return refused(409, "plugin name is reserved by this deployment");
    }
    if !opengrok_integrations::registry::revision_ok(&request.registry_revision) {
        return refused(422, "choose a full registry commit SHA");
    }
    let Some(registry) = &s.registry else {
        return refused(503, "plugin registry is unavailable");
    };
    let catalog = match registry.catalog(Some(&request.registry_revision)).await {
        Ok(c) => c,
        Err(e) => return refused(502, &e.to_string()),
    };
    let Some(entry) = catalog.plugins.iter().find(|e| e.name == request.name) else {
        return refused(404, "no such plugin");
    };
    let bundle = match registry.bundle(entry).await {
        Ok(b) => b,
        Err(e) => return refused(422, &e.to_string()),
    };
    match installed::install(&s.store, &account, &catalog, entry, &bundle, chrono::Utc::now().timestamp_millis()).await {
        Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({"name": entry.name, "revision": entry.revision, "registryRevision": catalog.revision, "parts": bundle.parts}))).into_response(),
        Err(opengrok_store::StoreError::Conflict) => refused(409, "already installed; uninstall before explicitly choosing a new revision"),
        Err(_) => refused(503, "plugin could not be installed"),
    }
}
async fn uninstall(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    match installed::uninstall(&s.store, &account, &name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => refused(404, "no such installation"),
        Err(_) => refused(503, "plugin could not be uninstalled"),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    token: String,
}
async fn credential(
    State(s): State<RegistryState>,
    headers: HeaderMap,
    Path((name, connector)): Path<(String, String)>,
    request: Result<Json<Credential>, JsonRejection>,
) -> Response {
    let Some(account) = (s.authenticate)(&headers) else {
        return refused(401, "sign in first");
    };
    let request = match request {
        Ok(Json(request)) => request,
        Err(_) => return refused(422, "send a token only"),
    };
    if request.token.is_empty()
        || request.token.len() > 16384
        || request.token.contains(['\r', '\n'])
    {
        return refused(
            422,
            "token must be nonempty, single-line and at most 16384 bytes",
        );
    }
    let Some(vault) = &s.vault else {
        return refused(503, "credential vault is unavailable");
    };
    match installed::credential(
        &s.store,
        vault,
        &account,
        &name,
        &connector,
        &request.token,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => refused(404, "no such installed connector"),
        Err(_) => refused(503, "credential could not be saved"),
    }
}
