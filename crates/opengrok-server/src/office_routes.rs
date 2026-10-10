//! The office document fetch routes the `opengrok.officeDoc` CUSTOM frames point at (D9): the
//! frames are thin — id, path, version — and a watching client pulls the document's bytes and
//! rendered pages through here, under the same bearer as the rest of the account's routes.
//!
//! BYTES COME FROM THE BOX, not from the session row: the file is what a person would see if
//! they opened it, including any edits made outside the tools. A row whose box has been reset
//! underneath it (the stored `box_id` is no longer the scope's) answers 404 — the document it
//! named is gone, and pretending the stale copy is live would be worse.

use axum::extract::{Path, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use opengrok_core::id::CoworkerId;
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::agui::provision;
use crate::agui::routes::account_from_bearer;

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/office/docs/{id}", get(detail))
        .route("/office/docs/{id}/bytes", get(bytes))
        .route("/office/docs/{id}/pages/{*page}", get(page_png))
        .with_state(state)
}

/// The MIME a kind's bytes are served as.
fn mime_of(kind: opengrok_office::Kind) -> &'static str {
    match kind {
        opengrok_office::Kind::Docx => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        opengrok_office::Kind::Xlsx => {
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        }
        opengrok_office::Kind::Pptx => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
    }
}

/// The session row the caller may see — theirs, and only if its stored box is still the
/// coworker's scope's live one. `None` does not distinguish: a document on a reset box, a
/// document of another account's, and no document at all are all "no such document".
async fn row_for(
    state: &AgUiState,
    headers: &HeaderMap,
    id: &str,
) -> Result<opengrok_store::DocSessionRow, Response> {
    let Some(account) = account_from_bearer(state, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    let row = match state.auth.store.doc_session(id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "no such document").into_response()),
        Err(error) => {
            return Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response());
        }
    };
    if row.account_id != account.as_str() {
        return Err((StatusCode::NOT_FOUND, "no such document").into_response());
    }
    // The session's box must still be the coworker's live one — a reset leaves the row naming a
    // destroyed box whose files no longer exist.
    let coworker = CoworkerId::from_stored(row.coworker_id.clone());
    let live = provision::scoped_box_row_for(state, &account, &coworker).await;
    match live {
        Some(scoped) if scoped.box_id == row.box_id => {}
        _ => return Err((StatusCode::NOT_FOUND, "no such document").into_response()),
    }
    Ok(row)
}

/// The row's provider and the file's current bytes — one box read for the bytes and page
/// routes. A box that will not answer is a 503 with the provider's words, matching how the
/// box-bound routes fail.
async fn bytes_for(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
) -> Result<Vec<u8>, Response> {
    let coworker = CoworkerId::from_stored(row.coworker_id.clone());
    let account = opengrok_core::id::AccountId::from_stored(row.account_id.clone());
    let scoped = provision::scoped_box_row_for(state, &account, &coworker)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no such document").into_response())?;
    let provider = provision::provider_for(state, scoped.org_id.as_deref(), &scoped.kind)
        .await
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "no provider for the document's computer",
            )
                .into_response()
        })?;
    // A fetch that finds the box asleep wakes it rather than failing: the document window is
    // open because the person is looking at it, and 60 s is a fair wait for a wake.
    provider
        .wake(&row.box_id, std::time::Duration::from_secs(60))
        .await
        .map_err(|error| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("the computer could not be woken: {error}"),
            )
                .into_response()
        })?;
    provider
        .read_file_bytes(&row.box_id, &row.path)
        .await
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response())
}

/// `GET /office/docs/{id}` — the session as the document window wants it: identity, kind,
/// version, and the proposals still pending so the window can draw their diffs.
async fn detail(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let row = match row_for(&state, &headers, &id).await {
        Ok(row) => row,
        Err(response) => return response,
    };
    let proposals: Vec<Value> = row
        .proposals
        .as_array()
        .map(|list| {
            list.iter()
                .map(|p| {
                    json!({
                        "id": p["id"],
                        "author": p["author"],
                        "note": p["note"],
                        "status": p["status"],
                        "changes": p["changes"].as_array().map_or(0, Vec::len),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Json(json!({
        "docId": row.id,
        "path": row.path,
        "kind": row.kind,
        "version": row.version,
        "contentSha256": row.content_sha256,
        "proposals": proposals,
        "createdAt": row.created_at_ms,
        "updatedAt": row.updated_at_ms,
    }))
    .into_response()
}

/// `GET /office/docs/{id}/bytes` — the document's bytes as the box has them now.
async fn bytes(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let row = match row_for(&state, &headers, &id).await {
        Ok(row) => row,
        Err(response) => return response,
    };
    let kind = opengrok_office::Kind::from_filename(&row.path);
    let bytes = match bytes_for(&state, &row).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let filename = row.path.rsplit('/').next().unwrap_or(&row.path).to_string();
    let headers = [
        (
            CONTENT_TYPE,
            mime_of(kind.unwrap_or(opengrok_office::Kind::Docx)).to_string(),
        ),
        (CONTENT_LENGTH, bytes.len().to_string()),
        (
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ),
    ];
    (headers, bytes).into_response()
}

/// `GET /office/docs/{id}/pages/{page}.png` — one rendered page/slide/used-range, one-based
/// like the `office_render` tool's `page`.
async fn page_png(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((id, page)): Path<(String, String)>,
) -> Response {
    // `…/pages/12.png`: a segment that is a number AND its extension cannot be one Axum
    // parameter, so the whole `12.png` arrives as the wildcard and is split here. A segment
    // that is not `N.png` is simply not this route's page.
    let Some(page) = page.strip_suffix(".png").and_then(|n| n.parse().ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let page: u32 = page;
    let row = match row_for(&state, &headers, &id).await {
        Ok(row) => row,
        Err(response) => return response,
    };
    let kind = match opengrok_office::Kind::from_filename(&row.path) {
        Some(kind) => kind,
        None => {
            return (StatusCode::UNPROCESSABLE_ENTITY, "not an office document").into_response();
        }
    };
    let bytes = match bytes_for(&state, &row).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    // The engines are `!Send`; the render is a sync block after the awaits.
    let index = usize::try_from(page.saturating_sub(1)).unwrap_or(usize::MAX);
    let image =
        opengrok_office::Session::open(&bytes, kind).and_then(|session| session.render_png(index));
    match image {
        Ok(image) => (
            [
                (CONTENT_TYPE, "image/png".to_string()),
                (CONTENT_LENGTH, image.bytes.len().to_string()),
            ],
            image.bytes,
        )
            .into_response(),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the page could not be rendered: {error}"),
        )
            .into_response(),
    }
}
