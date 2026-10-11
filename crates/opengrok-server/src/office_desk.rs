//! The office tools' desk: `opengrok_tools::office_desk`'s calls answered with the
//! `doc_session` rows and the box bytes, through the `opengrok-office` engines.
//!
//! THE ROW IS THE SESSION (D7). Bytes live on the box and snapshots in `artifact`; the row
//! carries the hash the session last saw, its mutation count, and the proposals still pending.
//! A proposal on a row survives the turn that staged it, which is the whole point — review can
//! come tomorrow.
//!
//! EVERY VERB RE-READS THE FILE. `content_sha256` on the row is never trusted to still match:
//! a person or another tool may have written the file since, and when they did the session
//! re-bases — new hash, version bump — and every proposal staged before it is stale by
//! construction (its `version` no longer matches) and by its guards (the text it targeted may
//! not read the same).
//!
//! EVERY MUTATION SNAPSHOTS. Accept and export write the bytes to `artifact` too — a person
//! can always get back to what the file was when the agent changed it, and the export is the
//! attachment the reply hands them.

use std::sync::Arc;

use opengrok_box::Computer;
use opengrok_core::id::DocSessionId;
use opengrok_office::{
    CellEditInput, Error as OfficeError, Kind, Proposal, ProposalStatus, Session, TextEditInput,
};
use opengrok_store::postgres::ArtifactRow;
use opengrok_tools::office_desk::{Ask, OfficeDesk, Reply};
use opengrok_tools::{ToolContext, ToolImage};
use serde_json::{Value, json};
use sha2::Digest;

use crate::agui::AgUiState;

/// The office desk. `state` is the same `AgUiState` the pane's routes hold — the store and
/// the box resolution live on it.
pub struct Tools {
    pub state: AgUiState,
}

/// `office_files`' default browse root, per the settled design (D10): anywhere is legal, the
/// listing starts here.
const DEFAULT_DIRECTORY: &str = "~/office";

/// The largest Office file the tools will hold: documents and slides are small; this ceiling
/// is a box read's protection, not a product decision.
const MAX_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;

/// The MIME types the export artifacts carry.
fn mime_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Kind::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Kind::Pptx => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    }
}

/// sha256 of the bytes, lowercase hex — the `content_sha256` every session row carries.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Shell-quote a string for embedding in a `sh -c` command the model half-chose: wrap in
/// single quotes, escape each inner quote. The box runs GNU/Linux, so `find`/`stat` are GNU.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A box failure, in words a model can act on — the same phrasing the box tools' own
/// `describe` uses in opengrok-tools.
fn describe_box(error: &opengrok_box::BoxError) -> String {
    match error {
        opengrok_box::BoxError::NoSuchBox => "that computer no longer exists".to_string(),
        opengrok_box::BoxError::Secret(reason) => reason.clone(),
        opengrok_box::BoxError::Unreachable(detail) => {
            format!("the computer is unreachable: {detail}")
        }
        opengrok_box::BoxError::Interrupted(detail) => {
            format!("the connection to the computer was lost before it answered: {detail}")
        }
        opengrok_box::BoxError::Refused { status, body } => {
            format!("the computer refused the request ({status}): {body}")
        }
    }
}

/// Expand a leading `~` on a path the way the person's shell would — asking the box for its
/// `$HOME` once. Any other path is used as it was written; office paths are absolute.
pub(crate) async fn expand_home(
    box_io: &dyn Computer,
    box_id: &str,
    path: &str,
) -> Result<String, String> {
    if !path.starts_with('~') {
        return Ok(path.to_string());
    }
    let out = box_io
        .run(box_id, "printf '%s' \"$HOME\"", 10)
        .await
        .map_err(|e| describe_box(&e))?;
    if out.exit_code != 0 || out.stdout.trim().is_empty() {
        return Err("the box could not say what its home directory is".to_string());
    }
    let home = out.stdout.trim_end_matches('/');
    Ok(format!("{}{}", home, &path[1..]))
}

/// The refusal JSON a desk error becomes — `{code, message, details?}` like upstream's
/// `DocumentToolError`, so a model can branch on `code` rather than parse the sentence.
fn refusal(code: &str, message: impl Into<String>, details: Option<Value>) -> String {
    let mut body = json!({ "code": code, "message": message.into() });
    if let Some(details) = details {
        body["details"] = details;
    }
    body.to_string()
}

/// Map an engine error to a refusal string. `StaleTargets` is the one with structure a model
/// needs — the refs to re-read before it re-proposes.
fn engine_refusal(error: &OfficeError) -> String {
    match error {
        OfficeError::StaleTargets(refs) => refusal(
            "STALE_TARGETS",
            "one or more targets moved since the proposal was staged; read them again and \
             re-create the proposal",
            Some(json!({ "staleRefs": refs })),
        ),
        OfficeError::Refused(message) => refusal("REFUSED", message.clone(), None),
        other => refusal("ENGINE_ERROR", other.to_string(), None),
    }
}

/// A `doc_session` row `context` may use: its account's, its coworker's, on this box. Any
/// other answer is "no such document" — the handle's existence on another account is not
/// something a refusal should confirm.
fn owned<'a>(
    context: &ToolContext,
    box_id: &str,
    row: &'a opengrok_store::DocSessionRow,
) -> Result<&'a opengrok_store::DocSessionRow, String> {
    if row.account_id == context.account_id.as_str()
        && row.coworker_id == context.coworker_id.as_str()
        && row.box_id == box_id
    {
        Ok(row)
    } else {
        Err(refusal(
            "NOT_FOUND",
            format!("no document {} is open here", row.id),
            None,
        ))
    }
}

/// The proposals on a row, decoded. `proposals` is the desk's own JSON — a decode failure is
/// a bug, but one that must read as an engine error, not a panic.
fn proposals_of(row: &opengrok_store::DocSessionRow) -> Result<Vec<Proposal>, String> {
    serde_json::from_value(row.proposals.clone()).map_err(|error| {
        refusal(
            "ENGINE_ERROR",
            format!("a proposal could not be read: {error}"),
            None,
        )
    })
}

fn proposals_value(proposals: &[Proposal]) -> Result<Value, String> {
    serde_json::to_value(proposals).map_err(|error| {
        refusal(
            "ENGINE_ERROR",
            format!("a proposal could not be stored: {error}"),
            None,
        )
    })
}

/// The `opengrok.officeDoc` frame a mutation emits: thin — who, what changed, where the bytes
/// are; the client fetches anything bigger through the fetch routes.
pub(crate) fn frame(
    row: &opengrok_store::DocSessionRow,
    kind: Kind,
    changed: Value,
    artifact_id: Option<&str>,
) -> Value {
    json!({
        "docId": row.id,
        "path": row.path,
        "kind": kind.extension(),
        "version": row.version,
        "artifactId": artifact_id,
        "changed": changed,
    })
}

impl Tools {
    /// The session row for `document`, ownership-checked — `None`-safe: a handle that is not
    /// this account's is not this account's business.
    async fn session_row(
        &self,
        context: &ToolContext,
        box_id: &str,
        document: &str,
    ) -> Result<opengrok_store::DocSessionRow, String> {
        let row = self
            .state
            .auth
            .store
            .doc_session(document)
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the session could not be read: {error}"),
                    None,
                )
            })?
            .ok_or_else(|| refusal("NOT_FOUND", format!("no document {document} is open"), None))?;
        owned(context, box_id, &row).cloned()
    }

    /// Read the file's CURRENT bytes, and re-base the row when they no longer match the hash
    /// the session last saw: new hash, version bump, `externalChange` reported to the caller.
    /// A proposal staged before the drift keeps its `version` and so reads stale.
    ///
    /// Bytes, not a `Session`: the facades hold `yrs` undo callbacks and are `!Send`, so a
    /// session may never outlive a sync block — every verb opens it fresh after this returns.
    async fn fresh(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        row: &opengrok_store::DocSessionRow,
    ) -> Result<(Vec<u8>, opengrok_store::DocSessionRow, bool), String> {
        let bytes = box_io
            .read_file_bytes(box_id, &row.path)
            .await
            .map_err(|e| describe_box(&e))?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(refusal(
                "REFUSED",
                format!(
                    "{} is larger than the {} MiB the tools hold",
                    row.path,
                    MAX_DOCUMENT_BYTES / 1024 / 1024
                ),
                None,
            ));
        }
        let hash = sha256_hex(&bytes);
        if hash == row.content_sha256 {
            return Ok((bytes, row.clone(), false));
        }
        // The file moved outside the tools: the session is still valid (same file, same
        // handle) but everything staged on it is now measured against this content.
        let rebased = opengrok_store::DocSessionRow {
            content_sha256: hash,
            version: row.version + 1,
            updated_at_ms: crate::now_ms(),
            ..row.clone()
        };
        self.state
            .auth
            .store
            .save_doc_session(
                &row.id,
                row.version,
                &rebased.content_sha256,
                rebased.version,
                &rebased.proposals,
                rebased.updated_at_ms,
            )
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the session could not re-base: {error}"),
                    None,
                )
            })?;
        // Drift is a version too — the picker's chain has a hole wherever a bump left no
        // snapshot. Best-effort like open's v0: a snapshot that fails loses one entry, and
        // the verb that noticed the drift still has its answer.
        if let Ok(kind) = Self::kind_of(&rebased) {
            let filename = rebased.path.rsplit('/').next().unwrap_or(&rebased.path);
            if let Err(error) = self
                .snapshot(context, &rebased, kind, &bytes, filename, "external")
                .await
            {
                tracing::warn!(%error, doc = %rebased.id, "a drifted version could not be snapshotted");
            }
        }
        Ok((bytes, rebased, true))
    }

    /// Open a session in a sync block — the caller's guarantee that no `.await` follows while
    /// the session lives is what keeps `answer` `Send`.
    fn open_row(row: &opengrok_store::DocSessionRow, bytes: &[u8]) -> Result<Session, String> {
        let kind = Kind::from_filename(&row.path).ok_or_else(|| {
            refusal(
                "UNSUPPORTED_FORMAT",
                "the path is not .docx, .xlsx or .pptx",
                None,
            )
        })?;
        Session::open(bytes, kind).map_err(|e| engine_refusal(&e))
    }

    /// The session's kind, off the stored extension.
    fn kind_of(row: &opengrok_store::DocSessionRow) -> Result<Kind, String> {
        Kind::from_filename(&row.path)
            .ok_or_else(|| refusal("UNSUPPORTED_FORMAT", "bad session kind", None))
    }

    /// Store `bytes` as an artifact of `via` for this session — every mutation leaves one, so
    /// a person can always return to a known copy. Export's is attached under the thread so
    /// the transcript draws it on the bot's reply.
    async fn snapshot(
        &self,
        context: &ToolContext,
        row: &opengrok_store::DocSessionRow,
        kind: Kind,
        bytes: &[u8],
        filename: &str,
        via: &str,
    ) -> Result<String, String> {
        let id = format!("art_{}", uuid::Uuid::now_v7());
        let artifact = ArtifactRow {
            id: id.clone(),
            account_id: context.account_id.to_string(),
            kind: "office".to_string(),
            mime: mime_of(kind).to_string(),
            filename: filename.to_string(),
            size_bytes: bytes.len() as i64,
            recipe_id: None,
            run_id: context.run_id.clone(),
            step_index: None,
            thread_id: context.thread_id.clone(),
            meta: json!({
                "docSessionId": row.id,
                "path": row.path,
                "via": via,
                "version": row.version,
            }),
            created_at_ms: crate::now_ms(),
            deleted_at_ms: None,
        };
        self.state
            .auth
            .store
            .put_artifact(&artifact, bytes)
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the snapshot could not be stored: {error}"),
                    None,
                )
            })?;
        Ok(id)
    }

    /// `office_files`: the office files and subdirectories under `directory` on the box, plus
    /// the documents this account already has open here.
    async fn files(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        directory: Option<&str>,
        offset: usize,
    ) -> Result<Value, String> {
        let directory = expand_home(box_io, box_id, directory.unwrap_or(DEFAULT_DIRECTORY)).await?;
        // Directories first, then the office files inside them — one find each, GNU syntax,
        // depth-bounded so a big home tree cannot drown the listing.
        let command = format!(
            "if [ -d {dir} ]; then \
               find {dir} -mindepth 1 -maxdepth 4 -type d ! -name '.*' -print; \
               find {dir} -maxdepth 5 -type f \\( -iname '*.docx' -o -iname '*.xlsx' -o -iname '*.pptx' \\) -printf '%p\\t%s\\t%T@\\n'; \
             fi",
            dir = shq(&directory)
        );
        let out = box_io
            .run(box_id, &command, 30)
            .await
            .map_err(|e| describe_box(&e))?;
        if out.exit_code != 0 {
            return Err(refusal(
                "REFUSED",
                format!("the directory could not be listed: {}", out.stderr.trim()),
                None,
            ));
        }
        let mut directories = Vec::new();
        let mut files = Vec::new();
        for line in out.stdout.lines() {
            if let Some((path, size, modified)) = line.split_once('\t').and_then(|(p, rest)| {
                rest.split_once('\t')
                    .map(|(s, t)| (p, s.parse::<i64>().unwrap_or(0), t))
            }) {
                files.push(json!({
                    "path": path,
                    "kind": Kind::from_filename(path).map(|k| k.extension()),
                    "sizeBytes": size,
                    "dateModified": modified,
                }));
            } else if !line.is_empty() {
                directories.push(Value::from(line));
            }
        }
        let total = files.len();
        let files: Vec<Value> = files.into_iter().skip(offset).take(100).collect();
        let open = self
            .state
            .auth
            .store
            .doc_sessions_for(context.account_id.as_str(), context.coworker_id.as_str())
            .await
            .map_err(|error| refusal("ENGINE_ERROR", format!("open documents could not be read: {error}"), None))?
            .into_iter()
            .filter(|row| row.box_id == box_id)
            .map(|row| {
                json!({ "document": row.id, "path": row.path, "kind": row.kind, "version": row.version })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "directory": directory,
            "files": files,
            "directories": directories,
            "open": open,
            "nextOffset": if total > offset + 100 { Value::from(offset + 100) } else { Value::Null },
        }))
    }

    /// `office_open`: read the file, open it in the engine, and return (or reuse) the session
    /// row that keeps it a document across turns.
    async fn open(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        path: &str,
    ) -> Result<(Value, Vec<Value>), String> {
        let path = expand_home(box_io, box_id, path).await?;
        let kind = Kind::from_filename(&path).ok_or_else(|| {
            refusal(
                "UNSUPPORTED_FORMAT",
                "the path must end in .docx, .xlsx or .pptx",
                None,
            )
        })?;
        let bytes = box_io
            .read_file_bytes(box_id, &path)
            .await
            .map_err(|error| refusal("NOT_FOUND", describe_box(&error), None))?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(refusal(
                "REFUSED",
                format!(
                    "{path} is larger than the {} MiB the tools hold",
                    MAX_DOCUMENT_BYTES / 1024 / 1024
                ),
                None,
            ));
        }
        let hash = sha256_hex(&bytes);
        // A second open of the same path returns the same handle — the session, not the open,
        // is what proposals key on. Its stored bytes are re-checked in case it moved since.
        if let Some(existing) = self
            .state
            .auth
            .store
            .doc_session_by_path(box_id, &path)
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the session could not be read: {error}"),
                    None,
                )
            })?
        {
            let row = owned(context, box_id, &existing)?.clone();
            let (bytes, row, external) = self.fresh(context, box_io, box_id, &row).await?;
            let body = {
                let session = Self::open_row(&row, &bytes)?;
                let (counts, capabilities) = Self::describe_parts(&session);
                self.describe_open((counts, capabilities), &row, external)
            };
            return Ok((
                body,
                vec![frame(&row, kind, json!({ "type": "open" }), None)],
            ));
        }
        let (counts, capabilities) = {
            let session = Session::open(&bytes, kind).map_err(|e| engine_refusal(&e))?;
            Self::describe_parts(&session)
        };
        let now = crate::now_ms();
        let row = opengrok_store::DocSessionRow {
            id: DocSessionId::new().to_string(),
            account_id: context.account_id.to_string(),
            coworker_id: context.coworker_id.to_string(),
            box_id: box_id.to_string(),
            path: path.clone(),
            kind: kind.extension().to_string(),
            content_sha256: hash,
            version: 0,
            proposals: json!([]),
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.state
            .auth
            .store
            .put_doc_session(&row)
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the session could not be opened: {error}"),
                    None,
                )
            })?;
        // v0 — the version the session opened on, so the picker's chain starts at what the
        // file was before anything touched it. Best-effort: a snapshot that fails leaves
        // the chain without its first entry, not a failed open.
        let filename = row.path.rsplit('/').next().unwrap_or(&row.path);
        if let Err(error) = self
            .snapshot(context, &row, kind, &bytes, filename, "office_open")
            .await
        {
            tracing::warn!(%error, doc = %row.id, "a session's first version could not be snapshotted");
        }
        let body = self.describe_open((counts, capabilities), &row, false);
        Ok((
            body,
            vec![frame(&row, kind, json!({ "type": "open" }), None)],
        ))
    }

    /// `office_open`'s result body: the handle, the format, what the format can do, and how
    /// much of it there is to page through.
    fn describe_open(
        &self,
        parts: (Value, Value),
        row: &opengrok_store::DocSessionRow,
        external_change: bool,
    ) -> Value {
        let (counts, capabilities) = parts;
        json!({
            "document": row.id,
            "path": row.path,
            "format": row.kind,
            "version": row.version,
            "counts": counts,
            "capabilities": capabilities,
            "externalChange": external_change,
        })
    }

    /// The count and capability halves of `office_open`'s reply — computed while a Session
    /// still lives, so `describe_open` itself needs none.
    fn describe_parts(session: &Session) -> (Value, Value) {
        match session {
            Session::Docx(document) => (
                json!({ "paragraphs": document.paragraphs().len() }),
                json!([
                    "outline", "grep", "read", "propose", "review", "accept", "reject", "verify",
                    "export"
                ]),
            ),
            Session::Xlsx(workbook) => (
                json!({ "sheets": workbook.sheet_count() }),
                json!([
                    "outline",
                    "grep",
                    "read",
                    "cells",
                    "proposeCells",
                    "review",
                    "accept",
                    "reject",
                    "verify",
                    "render",
                    "export"
                ]),
            ),
            Session::Pptx(presentation) => (
                json!({ "slides": presentation.slides().len() }),
                json!([
                    "outline", "grep", "read", "propose", "review", "accept", "reject", "verify",
                    "render", "export"
                ]),
            ),
        }
    }

    /// `office_create`: bytes from the payload, written to a path checked free.
    async fn create(
        &self,
        _context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        path: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let path = expand_home(box_io, box_id, path).await?;
        let kind = Kind::from_filename(&path).ok_or_else(|| {
            refusal(
                "UNSUPPORTED_FORMAT",
                "the path must end in .docx, .xlsx or .pptx",
                None,
            )
        })?;
        // Never overwrite — the same rule export keeps.
        if box_io.read_file_bytes(box_id, &path).await.is_ok() {
            return Err(refusal(
                "EEXIST",
                format!("{path} already exists; create never overwrites"),
                None,
            ));
        }
        let bytes = opengrok_office::create_bytes(kind, args).map_err(|e| engine_refusal(&e))?;
        box_io
            .write_file_bytes(box_id, &path, &bytes)
            .await
            .map_err(|e| describe_box(&e))?;
        Ok(json!({ "path": path, "bytes": bytes.len(), "format": kind.extension() }))
    }

    /// One proposal found on `row` by id — pending or not (review inspects either).
    fn proposal_on(
        row: &opengrok_store::DocSessionRow,
        proposal: &str,
    ) -> Result<(Vec<Proposal>, usize), String> {
        let proposals = proposals_of(row)?;
        let index = proposals
            .iter()
            .position(|p| p.id == proposal)
            .ok_or_else(|| {
                refusal(
                    "NOT_FOUND",
                    format!("no proposal {proposal} on {}", row.id),
                    None,
                )
            })?;
        Ok((proposals, index))
    }

    /// `office_propose` / `office_propose_cells`: stage the edits against a FRESH session and
    /// persist the proposal on the row — a later turn's accept finds it there.
    #[allow(clippy::too_many_arguments)]
    async fn propose(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        document: &str,
        author: &str,
        note: &str,
        edits: AskEdits<'_>,
    ) -> Result<(Value, Vec<Value>), String> {
        let row = self.session_row(context, box_id, document).await?;
        let (bytes, row, external) = self.fresh(context, box_io, box_id, &row).await?;
        let kind = Self::kind_of(&row)?;
        // The session lives only inside this block: drop-tracking follows scopes, not
        // `drop()` calls, and a Session that reached the awaits below is not Send.
        let proposal = {
            let session = Self::open_row(&row, &bytes)?;
            match edits {
                AskEdits::Text(edits) => session.propose(
                    author,
                    note,
                    &edits
                        .iter()
                        .map(|e| TextEditInput {
                            match_: e.match_.clone(),
                            new_text: e.new_text.clone(),
                        })
                        .collect::<Vec<_>>(),
                    row.version,
                ),
                AskEdits::Cells(edits) => session.propose_cells(
                    author,
                    note,
                    &edits
                        .iter()
                        .map(|e| CellEditInput {
                            cell: e.cell.clone(),
                            input: e.input.clone(),
                        })
                        .collect::<Vec<_>>(),
                    row.version,
                ),
            }
        }
        .map_err(|e| engine_refusal(&e))?;
        let mut proposals = proposals_of(&row)?;
        proposals.push(proposal.clone());
        self.state
            .auth
            .store
            .save_doc_session(
                &row.id,
                row.version,
                &row.content_sha256,
                row.version,
                &proposals_value(&proposals)?,
                crate::now_ms(),
            )
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the proposal could not be stored: {error}"),
                    None,
                )
            })?;
        let body = json!({
            "document": row.id,
            "proposal": proposal.id,
            "changes": proposal.changes.len(),
            "version": row.version,
            "externalChange": external,
            "note": "review with office_review, then office_accept applies it and writes the file",
        });
        Ok((
            body,
            vec![frame(
                &row,
                kind,
                json!({ "type": "proposal", "proposalId": proposal.id }),
                None,
            )],
        ))
    }

    /// `office_accept`: verify the proposal's guards against the FRESH document, apply, write
    /// the box file, bump the row, snapshot the artifact. Atomic in the engine; the row's
    /// version check keeps two racing accepts from both landing.
    async fn accept(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        document: &str,
        proposal_id: &str,
        tracked: bool,
    ) -> Result<(Value, Vec<Value>), String> {
        let row = self.session_row(context, box_id, document).await?;
        let (bytes, row, _external) = self.fresh(context, box_io, box_id, &row).await?;
        let kind = Self::kind_of(&row)?;
        let (mut proposals, index) = Self::proposal_on(&row, proposal_id)?;
        let proposal = &proposals[index];
        if proposal.status != ProposalStatus::Pending {
            return Err(refusal(
                "STALE_PROPOSAL",
                format!("proposal {proposal_id} is not pending"),
                None,
            ));
        }
        // Staged before the session's current version? Then the document moved under it —
        // per-change guards still run, but the caller must know this is a re-based accept.
        let staged_version = proposal.version;
        let outcome = {
            let mut session = Self::open_row(&row, &bytes)?;
            match session.apply(&proposals[index], tracked) {
                Ok(applied) => session.save().map(|out| (applied, out)),
                Err(error) => Err(error),
            }
        };
        match outcome {
            Ok((applied, out_bytes)) => {
                box_io
                    .write_file_bytes(box_id, &row.path, &out_bytes)
                    .await
                    .map_err(|e| describe_box(&e))?;
                proposals[index].status = ProposalStatus::Accepted;
                let version = row.version + 1;
                let saved_row = opengrok_store::DocSessionRow {
                    content_sha256: sha256_hex(&out_bytes),
                    version,
                    updated_at_ms: crate::now_ms(),
                    ..row.clone()
                };
                self.state
                    .auth
                    .store
                    .save_doc_session(
                        &row.id,
                        row.version,
                        &saved_row.content_sha256,
                        version,
                        &proposals_value(&proposals)?,
                        saved_row.updated_at_ms,
                    )
                    .await
                    .map_err(|error| {
                        refusal(
                            "CONFLICT",
                            format!("the session moved under the accept; retry it: {error}"),
                            None,
                        )
                    })?;
                let filename = row.path.rsplit('/').next().unwrap_or(&row.path);
                let artifact_id = self
                    .snapshot(
                        context,
                        &saved_row,
                        kind,
                        &out_bytes,
                        filename,
                        "office_accept",
                    )
                    .await?;
                let body = json!({
                    "document": row.id,
                    "proposal": proposal_id,
                    "accepted": true,
                    "applied": applied,
                    "version": version,
                    "stagedVersion": staged_version,
                    "artifactId": artifact_id,
                });
                Ok((
                    body,
                    vec![frame(
                        &saved_row,
                        kind,
                        json!({ "type": "edit", "proposalId": proposal_id }),
                        Some(&artifact_id),
                    )],
                ))
            }
            Err(error @ OfficeError::StaleTargets(_)) => {
                // The row records which targets went stale so `office_review` shows it without
                // another apply attempt.
                if let OfficeError::StaleTargets(refs) = &error {
                    proposals[index].stale_refs = refs.clone();
                    let _ = self
                        .state
                        .auth
                        .store
                        .save_doc_session(
                            &row.id,
                            row.version,
                            &row.content_sha256,
                            row.version,
                            &proposals_value(&proposals)?,
                            crate::now_ms(),
                        )
                        .await;
                }
                Err(engine_refusal(&error))
            }
            Err(error) => Err(engine_refusal(&error)),
        }
    }

    /// `office_export`: write the document (a proposal's applied copy, or the accepted state)
    /// to a NEW path, never overwriting; the bytes become an artifact this thread's reply can
    /// hand to the person.
    async fn export(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        document: &str,
        path: &str,
        proposal_id: Option<&str>,
    ) -> Result<(Value, Vec<Value>), String> {
        let row = self.session_row(context, box_id, document).await?;
        let (bytes, row, _external) = self.fresh(context, box_io, box_id, &row).await?;
        let kind = Self::kind_of(&row)?;
        let path = expand_home(box_io, box_id, path).await?;
        if Kind::from_filename(&path) != Some(kind) {
            return Err(refusal(
                "UNSUPPORTED_FORMAT",
                format!("the export path must end in .{}", kind.extension()),
                None,
            ));
        }
        // A proposal exports its would-be result without accepting — `applied_copy` applies
        // on bytes never written back, so the live document and file stay untouched. All of
        // this is a sync block: the session it opens is !Send and must not reach an await.
        let out_bytes = match proposal_id {
            Some(proposal_id) => {
                let (proposals, index) = Self::proposal_on(&row, proposal_id)?;
                let proposal = &proposals[index];
                if proposal.status != ProposalStatus::Pending {
                    return Err(refusal(
                        "STALE_PROPOSAL",
                        format!("proposal {proposal_id} is not pending"),
                        None,
                    ));
                }
                let copy = Session::applied_copy(&bytes, kind, proposal)
                    .map_err(|e| engine_refusal(&e))?;
                copy.save().map_err(|e| engine_refusal(&e))?
            }
            None => Self::open_row(&row, &bytes)?
                .save()
                .map_err(|e| engine_refusal(&e))?,
        };
        if box_io.read_file_bytes(box_id, &path).await.is_ok() {
            return Err(refusal(
                "EEXIST",
                format!("{path} already exists; export never overwrites"),
                None,
            ));
        }
        box_io
            .write_file_bytes(box_id, &path, &out_bytes)
            .await
            .map_err(|e| describe_box(&e))?;
        let filename = path.rsplit('/').next().unwrap_or(path.as_str());
        let artifact_id = self
            .snapshot(context, &row, kind, &out_bytes, filename, "office_export")
            .await?;
        let body = json!({
            "document": row.id,
            "path": path,
            "bytes": bytes.len(),
            "format": kind.extension(),
            "artifactId": artifact_id,
            "proposal": proposal_id,
        });
        Ok((
            body,
            vec![frame(
                &row,
                kind,
                json!({ "type": "export", "exportPath": path, "proposalId": proposal_id }),
                Some(&artifact_id),
            )],
        ))
    }
}

/// Proposals carry text edits or cell edits — the two `office_propose*` verbs' payload.
enum AskEdits<'a> {
    Text(&'a [opengrok_tools::office_desk::TextEditArg]),
    Cells(&'a [opengrok_tools::office_desk::CellEditArg]),
}

#[async_trait::async_trait]
impl OfficeDesk for Tools {
    async fn answer(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        ask: Ask,
    ) -> Result<Reply, String> {
        match ask {
            Ask::Files { directory, offset } => self
                .files(context, box_io, box_id, directory.as_deref(), offset)
                .await
                .map(Reply::just),
            Ask::Open { path } => {
                let (result, customs) = self.open(context, box_io, box_id, &path).await?;
                Ok(Reply {
                    result,
                    customs,
                    image: None,
                })
            }
            Ask::Create { path, args } => self
                .create(context, box_io, box_id, &path, &args)
                .await
                .map(Reply::just),
            Ask::Outline {
                document,
                story,
                headings_only,
                offset,
                limit,
            } => {
                let row = self.session_row(context, box_id, &document).await?;
                let (bytes, _, external) = self.fresh(context, box_io, box_id, &row).await?;
                let session = Self::open_row(&row, &bytes)?;
                let mut result = session
                    .outline(story.as_deref(), headings_only, offset, limit)
                    .map_err(|e| engine_refusal(&e))?;
                result["externalChange"] = json!(external);
                Ok(Reply::just(result))
            }
            Ask::Grep {
                document,
                query,
                case_sensitive,
                story,
                limit,
                cursor,
            } => {
                let row = self.session_row(context, box_id, &document).await?;
                let (bytes, _, external) = self.fresh(context, box_io, box_id, &row).await?;
                let session = Self::open_row(&row, &bytes)?;
                let mut result = session
                    .grep(
                        &query,
                        case_sensitive,
                        story.as_deref(),
                        limit,
                        cursor.as_deref(),
                    )
                    .map_err(|e| engine_refusal(&e))?;
                result["externalChange"] = json!(external);
                Ok(Reply::just(result))
            }
            Ask::Read {
                document,
                ref_,
                start,
                length,
                field,
            } => {
                let row = self.session_row(context, box_id, &document).await?;
                let (bytes, _, external) = self.fresh(context, box_io, box_id, &row).await?;
                let session = Self::open_row(&row, &bytes)?;
                let mut result = session
                    .read(&ref_, start, length, field.as_deref())
                    .map_err(|e| engine_refusal(&e))?;
                result["externalChange"] = json!(external);
                Ok(Reply::just(result))
            }
            Ask::Cells {
                document,
                sheet,
                range,
                offset,
                limit,
            } => {
                let row = self.session_row(context, box_id, &document).await?;
                let (bytes, _, external) = self.fresh(context, box_io, box_id, &row).await?;
                let session = Self::open_row(&row, &bytes)?;
                let mut result = session
                    .cells(&sheet, &range, offset, limit)
                    .map_err(|e| engine_refusal(&e))?;
                result["externalChange"] = json!(external);
                Ok(Reply::just(result))
            }
            Ask::Render {
                document,
                page,
                proposal,
            } => {
                let row = self.session_row(context, box_id, &document).await?;
                let (bytes, row, _external) = self.fresh(context, box_io, box_id, &row).await?;
                // A proposal renders its would-be page on a copy, never the live session.
                // Nothing here awaits until after the session is dropped.
                let session = match proposal.as_deref() {
                    Some(proposal_id) => {
                        let (proposals, index) = Self::proposal_on(&row, proposal_id)?;
                        let proposal = &proposals[index];
                        if proposal.status != ProposalStatus::Pending {
                            return Err(refusal(
                                "STALE_PROPOSAL",
                                format!("proposal {proposal_id} is not pending"),
                                None,
                            ));
                        }
                        Session::applied_copy(&bytes, Self::kind_of(&row)?, proposal)
                            .map_err(|e| engine_refusal(&e))?
                    }
                    None => Self::open_row(&row, &bytes)?,
                };
                // The contract's `page` is one-based; the engines' index is zero-based.
                let index = usize::try_from(page.saturating_sub(1))
                    .map_err(|_| refusal("INVALID_ARGUMENT", "page overflows usize", None))?;
                let image = session.render_png(index).map_err(|e| engine_refusal(&e))?;
                let page_count = match &session {
                    Session::Xlsx(workbook) => Some(workbook.sheet_count()),
                    Session::Pptx(presentation) => Some(presentation.slides().len()),
                    Session::Docx(_) => None,
                };
                let mut reply = Reply::just(json!({
                    "document": row.id,
                    "page": page,
                    "pageCount": page_count,
                    "width": image.width,
                    "height": image.height,
                }));
                use base64::Engine as _;
                reply.image = Some(ToolImage {
                    mime: "image/png".to_string(),
                    base64: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
                    width: image.width,
                    height: image.height,
                    // The model's eyes, like a computer screenshot: the person sees the document
                    // live through the officeDoc frames and the fetch routes, not a PNG in the
                    // transcript.
                    visibility: opengrok_tools::ImageVisibility::Agent,
                });
                Ok(reply)
            }
            Ask::Verify { document, proposal } => {
                let row = self.session_row(context, box_id, &document).await?;
                let (bytes, row, _external) = self.fresh(context, box_io, box_id, &row).await?;
                let result = match proposal.as_deref() {
                    Some(proposal_id) => {
                        let (proposals, index) = Self::proposal_on(&row, proposal_id)?;
                        Session::applied_copy(&bytes, Self::kind_of(&row)?, &proposals[index])
                            .and_then(|s| s.verify())
                            .map_err(|e| engine_refusal(&e))?
                    }
                    None => Self::open_row(&row, &bytes)?
                        .verify()
                        .map_err(|e| engine_refusal(&e))?,
                };
                Ok(Reply::just(result))
            }
            Ask::Propose {
                document,
                author,
                note,
                edits,
            } => {
                let (result, customs) = self
                    .propose(
                        context,
                        box_io,
                        box_id,
                        &document,
                        &author,
                        &note,
                        AskEdits::Text(&edits),
                    )
                    .await?;
                Ok(Reply {
                    result,
                    customs,
                    image: None,
                })
            }
            Ask::ProposeCells {
                document,
                author,
                note,
                edits,
            } => {
                let (result, customs) = self
                    .propose(
                        context,
                        box_io,
                        box_id,
                        &document,
                        &author,
                        &note,
                        AskEdits::Cells(&edits),
                    )
                    .await?;
                Ok(Reply {
                    result,
                    customs,
                    image: None,
                })
            }
            Ask::Review { document, proposal } => {
                let row = self.session_row(context, box_id, &document).await?;
                match proposal.as_deref() {
                    None => {
                        let proposals = proposals_of(&row)?;
                        let list: Vec<Value> = proposals
                            .iter()
                            .map(|p| {
                                json!({
                                    "id": p.id,
                                    "author": p.author,
                                    "note": p.note,
                                    "status": p.status,
                                    "version": p.version,
                                    "changes": p.changes.len(),
                                    "staleRefs": p.stale_refs,
                                })
                            })
                            .collect();
                        Ok(Reply::just(
                            json!({ "document": row.id, "proposals": list }),
                        ))
                    }
                    Some(proposal_id) => {
                        let (bytes, row, _external) =
                            self.fresh(context, box_io, box_id, &row).await?;
                        let session = Self::open_row(&row, &bytes)?;
                        let (proposals, index) = Self::proposal_on(&row, proposal_id)?;
                        let mut result = session
                            .inspect(&proposals[index])
                            .map_err(|e| engine_refusal(&e))?;
                        result["document"] = json!(row.id);
                        result["sessionVersion"] = json!(row.version);
                        Ok(Reply::just(result))
                    }
                }
            }
            Ask::Accept {
                document,
                proposal,
                tracked,
            } => {
                let (result, customs) = self
                    .accept(context, box_io, box_id, &document, &proposal, tracked)
                    .await?;
                Ok(Reply {
                    result,
                    customs,
                    image: None,
                })
            }
            Ask::Reject { document, proposal } => {
                let row = self.session_row(context, box_id, &document).await?;
                let (mut proposals, index) = Self::proposal_on(&row, &proposal)?;
                if proposals[index].status != ProposalStatus::Pending {
                    return Err(refusal(
                        "STALE_PROPOSAL",
                        format!("proposal {proposal} is not pending"),
                        None,
                    ));
                }
                proposals[index].status = ProposalStatus::Rejected;
                self.state
                    .auth
                    .store
                    .save_doc_session(
                        &row.id,
                        row.version,
                        &row.content_sha256,
                        row.version,
                        &proposals_value(&proposals)?,
                        crate::now_ms(),
                    )
                    .await
                    .map_err(|error| {
                        refusal(
                            "ENGINE_ERROR",
                            format!("the proposal could not be rejected: {error}"),
                            None,
                        )
                    })?;
                let kind = Kind::from_filename(&row.path)
                    .ok_or_else(|| refusal("UNSUPPORTED_FORMAT", "bad session kind", None))?;
                Ok(Reply {
                    result: json!({ "document": row.id, "proposal": proposal, "rejected": true }),
                    customs: vec![frame(
                        &row,
                        kind,
                        json!({ "type": "proposal-discarded", "proposalId": proposal }),
                        None,
                    )],
                    image: None,
                })
            }
            Ask::Export {
                document,
                path,
                proposal,
            } => {
                let (result, customs) = self
                    .export(
                        context,
                        box_io,
                        box_id,
                        &document,
                        &path,
                        proposal.as_deref(),
                    )
                    .await?;
                Ok(Reply {
                    result,
                    customs,
                    image: None,
                })
            }
            Ask::Close { .. } => Err(refusal(
                "ENGINE_ERROR",
                "office_close answers before the box — this path cannot be reached",
                None,
            )),
        }
    }

    async fn close(&self, context: &ToolContext, document: &str) -> Result<Reply, String> {
        // `context.box_id` cannot scope this one — close runs before a box is even resolved —
        // so ownership is account+coworker, and the row's own box_id stays what it is.
        let row = self
            .state
            .auth
            .store
            .doc_session(document)
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the session could not be read: {error}"),
                    None,
                )
            })?
            .ok_or_else(|| refusal("NOT_FOUND", format!("no document {document} is open"), None))?;
        if row.account_id != context.account_id.as_str()
            || row.coworker_id != context.coworker_id.as_str()
        {
            return Err(refusal(
                "NOT_FOUND",
                format!("no document {document} is open here"),
                None,
            ));
        }
        let closed = self
            .state
            .auth
            .store
            .close_doc_session(document)
            .await
            .map_err(|error| {
                refusal(
                    "ENGINE_ERROR",
                    format!("the session could not be closed: {error}"),
                    None,
                )
            })?;
        if !closed {
            return Err(refusal(
                "NOT_FOUND",
                format!("no document {document} is open"),
                None,
            ));
        }
        let kind = Kind::from_filename(&row.path)
            .ok_or_else(|| refusal("UNSUPPORTED_FORMAT", "bad session kind", None))?;
        Ok(Reply {
            result: json!({ "document": document, "closed": true }),
            customs: vec![frame(&row, kind, json!({ "type": "closed" }), None)],
            image: None,
        })
    }
}

/// `ArtifactRow` construction needs the store visible; re-export what the desk touches so the
/// caller does not chase the type.
pub fn desk(state: AgUiState) -> Arc<dyn OfficeDesk> {
    Arc::new(Tools { state })
}
