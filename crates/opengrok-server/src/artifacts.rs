//! Artifacts: images and videos attached to messages, and screenshots and recordings from recipe runs.
//! The store holds both plaintext in Postgres bytea columns and metadata in jsonb, with no
//! encryption. The vault exists for credentials that open other systems; every message is already
//! plaintext in events, so sealing a screenshot while the conversation beside it is in the clear
//! buys nothing and costs a decrypt on every thumbnail page.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use opengrok_core::id::AccountId;
use opengrok_store::ArtifactRow;
use serde::Deserialize;
use serde_json::json;

use crate::agui::AgUiState;
use crate::agui::routes::account_from_bearer;

/// The longest an artifact may be after decoding the base64, to keep storage sane.
const MAX_ARTIFACT_BYTES: usize = 25 * 1024 * 1024;

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route(
            "/artifacts",
            // Axum refuses a body over 2 MB by default, and it refuses it BEFORE the handler
            // runs — so without this every artifact worth storing would be rejected with a
            // message about length rather than the sentence below about the 25 MiB cap. The
            // allowance is base64, which is about four bytes for every three, plus room for the
            // rest of the object. Raised on this ONE route: nothing else here takes a big body,
            // and a server-wide limit would be a server-wide invitation.
            post(create).layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/artifacts/{id}", get(detail).delete(remove))
        .route("/artifacts/{id}/bytes", get(read_bytes))
        .with_state(state)
}

/// What the request body may weigh: the 25 MiB cap once base64 has grown it by a third, and a
/// little for the field names around it.
const MAX_UPLOAD_BYTES: usize = MAX_ARTIFACT_BYTES / 3 * 4 + 64 * 1024;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRequest {
    kind: String,
    mime: String,
    filename: String,
    base64: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    recipe_id: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    step_index: Option<i32>,
    #[serde(default)]
    meta: Option<serde_json::Value>,
}

/// `POST /artifacts` — store an image or video, returning its row.
async fn create(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(request): Json<CreateRequest>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    // Validate mime type: image or video only.
    if !request.mime.starts_with("image/") && !request.mime.starts_with("video/") {
        return (
            StatusCode::BAD_REQUEST,
            "only images and videos are accepted",
        )
            .into_response();
    }

    // Decode base64 and validate size.
    let bytes = match base64::engine::general_purpose::STANDARD.decode(&request.base64) {
        Ok(decoded) => decoded,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid base64").into_response(),
    };

    if bytes.len() > MAX_ARTIFACT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "artifacts must be under 25 MiB",
        )
            .into_response();
    }

    // Mint a new id: art_ prefix + UUID v7.
    let id = format!("art_{}", uuid::Uuid::now_v7());
    let at_ms = now_ms();

    let row = ArtifactRow {
        id: id.clone(),
        account_id: account.as_str().to_string(),
        kind: request.kind,
        mime: request.mime,
        filename: request.filename,
        size_bytes: bytes.len() as i64,
        recipe_id: request.recipe_id,
        run_id: request.run_id,
        step_index: request.step_index,
        thread_id: request.thread_id,
        meta: request.meta.unwrap_or(json!({})),
        created_at_ms: at_ms,
        deleted_at_ms: None,
    };

    // Store in the database.
    let store = &state.auth.store;
    if let Err(error) = store.put_artifact(&row, &bytes).await {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    Json(row).into_response()
}

/// `GET /artifacts/{id}` — the artifact row (without bytes), owned by the caller.
async fn detail(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    let store = &state.auth.store;
    let row = match store.artifact(&id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            // Do not distinguish between missing, deleted, or not owned.
            return (StatusCode::NOT_FOUND, "no such artifact").into_response();
        }
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    // Check ownership. This is the only thing standing between two accounts and the bytes.
    if row.account_id != account.as_str() {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    }

    // Ignore deleted artifacts (same as missing).
    if row.deleted_at_ms.is_some() {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    }

    Json(row).into_response()
}

/// `GET /artifacts/{id}/bytes` — the bytes themselves, with content-type and content-disposition.
async fn read_bytes(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    let store = &state.auth.store;
    let (row, bytes) = match store.artifact_bytes(&id).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            // Do not distinguish between missing, deleted, or not owned.
            return (StatusCode::NOT_FOUND, "no such artifact").into_response();
        }
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    // Check ownership. This is the only thing standing between two accounts and the bytes.
    if row.account_id != account.as_str() {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    }

    // Ignore deleted artifacts (same as missing).
    if row.deleted_at_ms.is_some() {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    }

    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};

    let headers = [
        (CONTENT_TYPE, row.mime.clone()),
        (CONTENT_LENGTH, bytes.len().to_string()),
        (
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", row.filename),
        ),
    ];

    (headers, bytes).into_response()
}

/// `DELETE /artifacts/{id}` — soft delete, owned by the caller.
async fn remove(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    let store = &state.auth.store;
    let row = match store.artifact(&id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            // Do not distinguish between missing, deleted, or not owned.
            return (StatusCode::NOT_FOUND, "no such artifact").into_response();
        }
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    // Check ownership. This is the only thing standing between two accounts and the bytes.
    if row.account_id != account.as_str() {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    }

    // Ignore already-deleted artifacts.
    if row.deleted_at_ms.is_some() {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    }

    let at_ms = now_ms();
    if let Err(error) = store.soft_delete_artifact(&id, at_ms).await {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// One artifact a box says it produced, as the box describes it.
pub struct BoxArtifact<'a> {
    pub recipe_id: &'a str,
    pub run_id: &'a str,
    /// Which step produced it, when one did. A recording belongs to the whole run.
    pub step_index: Option<i32>,
    pub kind: &'a str,
    /// Where it sits on the box's own filesystem.
    pub path: &'a str,
    pub mime: &'a str,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// Take one artifact off a box and keep it: read the bytes, write the row.
///
/// The CALLER passes the provider it already resolved. Looking one up in here would mean
/// guessing this bot's computer kind, and a guess that misses does not fail loudly — it returns
/// no provider, and the run quietly stores nothing while reporting success.
pub async fn store_box_artifact(
    state: &AgUiState,
    account: &AccountId,
    provider: &std::sync::Arc<dyn opengrok_box::Computer>,
    box_id: &str,
    found: BoxArtifact<'_>,
) -> Result<ArtifactRow, String> {
    let bytes = provider
        .read_file_bytes(box_id, found.path)
        .await
        .map_err(|error| format!("could not read {} off the box: {error}", found.path))?;
    if bytes.is_empty() {
        return Err(format!("{} was empty on the box", found.path));
    }
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "{} is {} bytes, over the {MAX_ARTIFACT_BYTES} byte limit",
            found.path,
            bytes.len()
        ));
    }

    let id = format!("art_{}", uuid::Uuid::now_v7());
    let at_ms = now_ms();

    // Width and height travel with the row so a viewer can lay out before the bytes arrive.
    let mut meta = serde_json::Map::new();
    if let Some(width) = found.width {
        meta.insert("width".to_string(), json!(width));
    }
    if let Some(height) = found.height {
        meta.insert("height".to_string(), json!(height));
    }
    let meta = serde_json::Value::Object(meta);

    let filename = std::path::Path::new(found.path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact")
        .to_string();

    let row = ArtifactRow {
        id: id.clone(),
        account_id: account.as_str().to_string(),
        kind: found.kind.to_string(),
        mime: found.mime.to_string(),
        filename,
        size_bytes: bytes.len() as i64,
        recipe_id: Some(found.recipe_id.to_string()),
        run_id: Some(found.run_id.to_string()),
        step_index: found.step_index,
        thread_id: None,
        meta,
        created_at_ms: at_ms,
        deleted_at_ms: None,
    };

    state
        .auth
        .store
        .put_artifact(&row, &bytes)
        .await
        .map_err(|error| error.to_string())?;

    Ok(row)
}
