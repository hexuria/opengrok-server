//! The office document fetch routes the `opengrok.officeDoc` CUSTOM frames point at (D9): the
//! frames are thin — id, path, version — and a watching client pulls the document's bytes and
//! rendered pages through here, under the same bearer as the rest of the account's routes.
//!
//! BYTES COME FROM THE BOX when the box can answer — the file is what a person would see if
//! they opened it, including edits made outside the tools. A box that died or was reset
//! underneath a row does NOT take the document with it: every committed version's bytes live
//! in `artifact` (see `doc_version_artifacts`), so an orphaned session keeps answering from
//! its newest kept snapshot — marked `X-Office-Stale` rather than passed off as live — and a
//! scope that has since gained a different live box rehydrates the session onto it, path and
//! row together.

use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use opengrok_core::id::CoworkerId;
use opengrok_office::Session;
use serde::Deserialize;
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

/// The session row the caller may see — theirs — and whether its stored box still serves
/// the document: the scope's current box, or another live computer the provider confirms
/// (a room's shared box the document was opened on, say). `None` does not distinguish: a
/// document of another account's and no document at all are both "no such document". An
/// orphaned box no longer closes the document: the version chain holds its bytes, so "dead
/// computer" is a degraded read, not a deletion.
async fn row_for(
    state: &AgUiState,
    headers: &HeaderMap,
    id: &str,
) -> Result<(opengrok_store::DocSessionRow, bool), Response> {
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
    let coworker = CoworkerId::from_stored(row.coworker_id.clone());
    let scoped = provision::scoped_box_row_for(state, &account, &coworker).await;
    let box_live = row_box_live(state, &row, scoped.as_ref()).await;
    Ok((row, box_live))
}

/// The provider that can answer for the row's stored box: the scope that claims the box NOW
/// — a shared computer is recorded under its own scope, not the document's owner's — else,
/// for a box nothing claims, the document's own scope's kind, which a replaced box almost
/// always kept. `None` when nobody can name the box's provider.
pub(crate) async fn provider_for_row_box(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
    scoped: Option<&provision::ScopedBoxRow>,
) -> Option<std::sync::Arc<dyn opengrok_box::Computer>> {
    if let Ok(Some((_, _, kind, org_id))) =
        state.auth.store.scoped_computer_of_box(&row.box_id).await
    {
        return provision::provider_for(state, org_id.as_deref(), &kind).await;
    }
    let scoped = scoped?;
    provision::provider_for(state, scoped.org_id.as_deref(), &scoped.kind).await
}

/// Whether the provider that serves the row's stored box says it is GONE — the desk's
/// `box_gone` asked of the right provider. `false` when nobody can answer: a missing
/// answer never moved a document, and a box that cannot be asked is not a dead one.
async fn row_box_gone(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
    scoped: Option<&provision::ScopedBoxRow>,
) -> bool {
    match provider_for_row_box(state, row, scoped).await {
        Some(provider) => crate::office_desk::box_gone(provider.as_ref(), &row.box_id).await,
        None => false,
    }
}

/// `box_live` for `row_for`: the stored box is the scope's, or a provider can still reach
/// it. A box nobody can answer for reads as dead — `boxLive` is the client's "the computer
/// still holds this" signal, and an unanswerable box is not that.
async fn row_box_live(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
    scoped: Option<&provision::ScopedBoxRow>,
) -> bool {
    if let Some(scoped) = scoped
        && scoped.box_id == row.box_id
    {
        return true;
    }
    match provider_for_row_box(state, row, scoped).await {
        Some(provider) => !crate::office_desk::box_gone(provider.as_ref(), &row.box_id).await,
        None => false,
    }
}

/// The newest kept snapshot of a session: `(bytes, version)` straight off the version chain —
/// the stand-in a dead box's head falls back to, and the bytes a rehydrate writes forward.
async fn newest_snapshot(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
) -> Option<(Vec<u8>, i64)> {
    let chain = state
        .auth
        .store
        .doc_version_artifacts(&row.account_id, &row.id)
        .await
        .ok()?;
    let artifact = chain.first()?;
    let version = artifact.meta["version"].as_i64()?;
    let (_, bytes) = state.auth.store.artifact_bytes(&artifact.id).await.ok()??;
    Some((bytes, version))
}

/// What a rehydrate left on the scope's new box: the bytes, which version they are, and
/// whether that is older than the session's head — a reset box gets the newest KEPT copy,
/// which is the head only when the last mutation's snapshot landed.
pub(crate) struct Rehydrated {
    pub bytes: Vec<u8>,
    pub version: i64,
    pub stale: bool,
}

/// The session's box is not the scope's any more and the scope names a different one: give
/// that box the document and repoint the row, so a reset or replaced computer gets the
/// person's document back instead of stranding it — for the fetch routes and the agent's own
/// office tools alike.
///
/// A file already at the path is adopted only when it IS this document: the head the session
/// last saw (a box rebuilt on the same disk) or the newest kept snapshot. Anything else is
/// somebody else's file and is left alone. A missing file gets the newest kept snapshot
/// written. `None` — with the reason logged — when the scope has no other box, it will not
/// wake, the path holds other content or already has a session there, the probe could not
/// answer, or nothing was ever kept; the caller's stale read then still serves the document.
pub(crate) async fn rehydrate(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
) -> Option<Rehydrated> {
    let account = opengrok_core::id::AccountId::from_stored(row.account_id.clone());
    let coworker = CoworkerId::from_stored(row.coworker_id.clone());
    let scoped = provision::scoped_box_row_for(state, &account, &coworker).await?;
    if scoped.box_id == row.box_id {
        return None;
    }
    // Only a box the provider itself says is gone is moved off of. A stored box that is
    // simply not the scope's can also be a LIVE computer — a room's shared box the
    // document was opened on — and repointing a live box's session strands the file on
    // it while the row claims the new box has it.
    if !row_box_gone(state, row, Some(&scoped)).await {
        tracing::info!(
            doc = %row.id,
            box_id = %row.box_id,
            "rehydrate: the stored box is not gone; the session stays"
        );
        return None;
    }
    let (doc, to) = (row.id.as_str(), scoped.box_id.as_str());
    let store = &state.auth.store;
    // A row on the new box already owns this path — repointing over it would hand one file
    // two sessions, so the orphan stays stale-read; `office_open` adopts orphans before it
    // mints a row, so this is only a race or a session from before that rule.
    match store.doc_session_by_path(to, &row.path).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            tracing::info!(
                doc,
                to,
                "rehydrate: the new box already has a session on this path"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(%error, doc, "rehydrate: the path's session could not be read");
            return None;
        }
    }
    let provider = provision::provider_for(state, scoped.org_id.as_deref(), &scoped.kind).await?;
    if let Err(error) = provider.wake(to, std::time::Duration::from_secs(60)).await {
        tracing::warn!(%error, doc, to, "rehydrate: the new box did not wake");
        return None;
    }
    let recovered = if crate::office_desk::file_exists(provider.as_ref(), to, &row.path).await? {
        let existing = match provider.read_file_bytes(to, &row.path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, doc, to, "rehydrate: the file on the new box could not be read");
                return None;
            }
        };
        if crate::office_desk::sha256_hex(&existing) == row.content_sha256 {
            Rehydrated {
                bytes: existing,
                version: row.version,
                stale: false,
            }
        } else {
            let (kept, version) = newest_snapshot(state, row).await?;
            if existing != kept {
                tracing::info!(
                    doc,
                    to,
                    "rehydrate: a different file holds the path; left alone"
                );
                return None;
            }
            Rehydrated {
                bytes: existing,
                version,
                stale: version != row.version,
            }
        }
    } else {
        let (kept, version) = newest_snapshot(state, row).await?;
        if let Err(error) = provider.write_file_bytes(to, &row.path, &kept).await {
            tracing::warn!(%error, doc, to, "rehydrate: the kept copy could not be written");
            return None;
        }
        Rehydrated {
            bytes: kept,
            version,
            stale: version != row.version,
        }
    };
    match store
        .repoint_doc_session_box(doc, &row.box_id, to, chrono::Utc::now().timestamp_millis())
        .await
    {
        Ok(_) => Some(recovered),
        Err(error) => {
            tracing::warn!(%error, doc, to, "rehydrate: the session could not be repointed");
            None
        }
    }
}

/// The row's provider and the file's current bytes — one box read for the bytes and page
/// routes. The provider is the one that OWNS the stored box — a shared computer's own
/// scope, or the document's scope for a box nothing claims — so a document on a live
/// non-scoped box is read from it, not through the wrong provider. A box that will not
/// answer is a 503 with the provider's words, matching how the box-bound routes fail.
async fn bytes_for(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
) -> Result<Vec<u8>, Response> {
    let coworker = CoworkerId::from_stored(row.coworker_id.clone());
    let account = opengrok_core::id::AccountId::from_stored(row.account_id.clone());
    let scoped = provision::scoped_box_row_for(state, &account, &coworker).await;
    let provider = provider_for_row_box(state, row, scoped.as_ref())
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

/// `?version=` on the bytes and page routes: which version's bytes to serve. Absent or the
/// live head means the box's current file; anything older resolves to the version's kept
/// snapshot — a version no snapshot survived is a 404, not a quiet render of the wrong
/// bytes.
#[derive(Deserialize)]
struct VersionQuery {
    version: Option<i64>,
}

/// One page of an answer: the bytes, the version they actually are, and whether they came
/// from a stand-in snapshot rather than the live box — the `X-Office-Version` and
/// `X-Office-Stale` headers a client reads to say "this is what it last was" honestly.
struct Served {
    bytes: Vec<u8>,
    version: i64,
    stale: bool,
}

impl Served {
    /// The headers every answer built from these bytes carries — `/bytes` and the rendered
    /// pages alike, so a page drawn from a stand-in is as honest about it as the file is.
    fn headers(&self) -> Vec<(axum::http::header::HeaderName, String)> {
        let mut headers = vec![(
            axum::http::header::HeaderName::from_static("x-office-version"),
            self.version.to_string(),
        )];
        if self.stale {
            headers.push((
                axum::http::header::HeaderName::from_static("x-office-stale"),
                "1".to_string(),
            ));
        }
        headers
    }
}

/// A `?version=` the chain does not hold — named as absent, never answered with other bytes.
fn not_kept(version: i64) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("version {version} of this document was not kept"),
    )
        .into_response()
}

/// The bytes a `?version=` names: the live head off the box, an older version off its
/// snapshot artifact. `bytes_for` is only for the head — and only first: when the box cannot
/// answer, a live replacement rehydrates the file back onto it, else the newest kept snapshot
/// stands in as `stale`. Anything else comes from the version chain the desk's snapshots
/// write.
async fn versioned_bytes(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
    version: Option<i64>,
) -> Result<Served, Response> {
    let Some(version) = version.filter(|v| *v != row.version) else {
        return head_bytes(state, row).await;
    };
    let chain = state
        .auth
        .store
        .doc_version_artifacts(&row.account_id, &row.id)
        .await
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response())?;
    let Some(artifact) = chain
        .iter()
        .find(|a| a.meta["version"].as_i64() == Some(version))
    else {
        return Err(not_kept(version));
    };
    match state
        .auth
        .store
        .artifact_bytes(&artifact.id)
        .await
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response())?
    {
        Some((_, bytes)) => Ok(Served {
            bytes,
            version,
            stale: false,
        }),
        None => Err(not_kept(version)),
    }
}

/// The head's bytes: the box's current file when it answers; a live replacement box gets the
/// session rehydrated onto it; a dead one leaves the newest kept snapshot standing in — stale
/// but still the person's document. With no snapshot at all, the box's own error is the honest
/// answer.
async fn head_bytes(
    state: &AgUiState,
    row: &opengrok_store::DocSessionRow,
) -> Result<Served, Response> {
    let box_error = match bytes_for(state, row).await {
        Ok(bytes) => {
            return Ok(Served {
                bytes,
                version: row.version,
                stale: false,
            });
        }
        Err(error) => error,
    };
    if let Some(recovered) = rehydrate(state, row).await {
        return Ok(Served {
            bytes: recovered.bytes,
            version: recovered.version,
            stale: recovered.stale,
        });
    }
    match newest_snapshot(state, row).await {
        Some((bytes, version)) => Ok(Served {
            bytes,
            version,
            stale: true,
        }),
        None => Err(box_error),
    }
}

/// `GET /office/docs/{id}` — the session as the document window wants it: identity, kind,
/// version, the version chain the picker lists, and the proposals still pending so the
/// window can draw their diffs.
async fn detail(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (row, box_live) = match row_for(&state, &headers, &id).await {
        Ok(found) => found,
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
    let mut versions: Vec<Value> = match state
        .auth
        .store
        .doc_version_artifacts(&row.account_id, &row.id)
        .await
    {
        Ok(chain) => chain
            .iter()
            .map(|artifact| {
                json!({
                    "version": artifact.meta["version"],
                    "artifactId": artifact.id,
                    "at": artifact.created_at_ms,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    // The live head is always viewable through the box even when no snapshot carries it —
    // a session minted before the chain existed, or the snapshot that failed.
    if !versions
        .iter()
        .any(|v| v["version"].as_i64() == Some(row.version))
    {
        versions.insert(
            0,
            json!({"version": row.version, "artifactId": null, "at": row.updated_at_ms}),
        );
    }
    Json(json!({
        "docId": row.id,
        "path": row.path,
        "kind": row.kind,
        "version": row.version,
        "contentSha256": row.content_sha256,
        "proposals": proposals,
        "versions": versions,
        "boxLive": box_live,
        "createdAt": row.created_at_ms,
        "updatedAt": row.updated_at_ms,
    }))
    .into_response()
}

/// The `Content-Disposition` a download carries, RFC 6266 + RFC 5987: a conservative ASCII
/// `filename` for old readers — anything outside printable ASCII, plus `"` and `\`, becomes
/// `_` — and a `filename*` that percent-encodes every byte outside attr-char so the real
/// name survives intact. Hand-rolled: pulling a crate in for one header is not worth it.
fn content_disposition(filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && (0x20..0x7f).contains(&(c as u32)) && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in filename.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if keep {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// `GET /office/docs/{id}/bytes` — the document's bytes: the box's current file, or the
/// kept copy of the version `?version=` names.
async fn bytes(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(version): Query<VersionQuery>,
) -> Response {
    let (row, _box_live) = match row_for(&state, &headers, &id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let kind = opengrok_office::Kind::from_filename(&row.path);
    let served = match versioned_bytes(&state, &row, version.version).await {
        Ok(served) => served,
        Err(response) => return response,
    };
    let filename = row.path.rsplit('/').next().unwrap_or(&row.path).to_string();
    let mut headers = vec![
        (
            CONTENT_TYPE,
            mime_of(kind.unwrap_or(opengrok_office::Kind::Docx)).to_string(),
        ),
        (CONTENT_LENGTH, served.bytes.len().to_string()),
        (CONTENT_DISPOSITION, content_disposition(&filename)),
    ];
    headers.extend(served.headers());
    (axum::response::AppendHeaders(headers), served.bytes).into_response()
}

/// `GET /office/docs/{id}/pages/{page}.png` — one rendered page/slide/used-range, one-based
/// like the `office_render` tool's `page`. `?version=` renders a kept version's bytes, so
/// the window can show what the document was, not only what it is.
async fn page_png(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((id, page)): Path<(String, String)>,
    Query(version): Query<VersionQuery>,
) -> Response {
    // `…/pages/12.png`: a segment that is a number AND its extension cannot be one Axum
    // parameter, so the whole `12.png` arrives as the wildcard and is split here. A segment
    // that is not `N.png` is simply not this route's page.
    let Some(page) = page.strip_suffix(".png").and_then(|n| n.parse().ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let page: u32 = page;
    let (row, _box_live) = match row_for(&state, &headers, &id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let kind = match opengrok_office::Kind::from_filename(&row.path) {
        Some(kind) => kind,
        None => {
            return (StatusCode::UNPROCESSABLE_ENTITY, "not an office document").into_response();
        }
    };
    let served = match versioned_bytes(&state, &row, version.version).await {
        Ok(served) => served,
        Err(response) => return response,
    };
    // The engines are `!Send`; the render is a sync block after the awaits.
    let index = usize::try_from(page.saturating_sub(1)).unwrap_or(usize::MAX);
    let opened = opengrok_office::Session::open(&served.bytes, kind);
    // The count rides every answer the open produced — the 422 of an out-of-range ask
    // included, so a pager learns the bound from the probe that found it, not only from
    // pages that rendered.
    let pages = opened.as_ref().ok().and_then(Session::page_count);
    let mut headers = served.headers();
    if let Some(pages) = pages {
        headers.push((
            axum::http::header::HeaderName::from_static("x-office-pages"),
            pages.to_string(),
        ));
    }
    match opened.and_then(|session| session.render_png(index)) {
        Ok(image) => (
            axum::response::AppendHeaders(
                [
                    vec![
                        (CONTENT_TYPE, "image/png".to_string()),
                        (CONTENT_LENGTH, image.bytes.len().to_string()),
                    ],
                    headers,
                ]
                .concat(),
            ),
            image.bytes,
        )
            .into_response(),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::response::AppendHeaders(headers),
            format!("the page could not be rendered: {error}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
#[path = "../tests/unit/office_routes.rs"]
mod tests;
