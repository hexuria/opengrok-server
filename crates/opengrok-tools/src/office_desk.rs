//! The office tools: a coworker reads and edits DOCX/XLSX/PPTX files on its own computer
//! through the `opengrok-office` engines, staged as proposals a person (or policy) reviews
//! before `office_accept` writes them back.
//!
//! The contract is transcribed from upstream BetterOffice's `packages/agents/mcp.ts` (the
//! Electron integration vendored it as `source/office/agents/mcp.ts`): the same tool names,
//! argument names, bounds, and pagination, so a client that learned one speaks the other.
//! Where upstream holds documents in one process, our sessions are `doc_session` rows keyed
//! `(box_id, path)` — an `office_propose` must outlive the turn that staged it.
//!
//! RUN AFTER THE BOX IS AWAKE, unlike the other desks: every verb but `office_close` reads or
//! writes box bytes, so `Executor::execute` resolves the target box and wakes it first, then
//! hands the desk the provider. Identity is still the context's; no argument names another
//! account's document — the desk answers as `context`'s account and coworker, and `document`
//! handles are row ids scoped to both.
//!
//! ONE CEILING ROW (`office`) switches all of them. Nothing here asks unconditionally:
//! `office_accept` is a write the model may self-serve, gated only where policy or a ceiling
//! `+`/`ask` rule puts a person in front of it — like `write_file`, not like `reset_computer`.

use serde_json::{Value, json};

use crate::{ToolContext, ToolResult};
use opengrok_box::Computer;

pub const OFFICE_FILES: &str = "office_files";
pub const OFFICE_OPEN: &str = "office_open";
pub const OFFICE_CREATE: &str = "office_create";
pub const OFFICE_OUTLINE: &str = "office_outline";
pub const OFFICE_GREP: &str = "office_grep";
pub const OFFICE_READ: &str = "office_read";
pub const OFFICE_CELLS: &str = "office_cells";
pub const OFFICE_RENDER: &str = "office_render";
pub const OFFICE_VERIFY: &str = "office_verify";
pub const OFFICE_CLOSE: &str = "office_close";
pub const OFFICE_PROPOSE: &str = "office_propose";
pub const OFFICE_PROPOSE_CELLS: &str = "office_propose_cells";
pub const OFFICE_REVIEW: &str = "office_review";
pub const OFFICE_ACCEPT: &str = "office_accept";
pub const OFFICE_REJECT: &str = "office_reject";
pub const OFFICE_EXPORT: &str = "office_export";

/// Every office tool, in the order they are offered: browse and open, the reads, the
/// proposal flow, then the verbs that leave bytes behind.
pub const TOOLS: [&str; 16] = [
    OFFICE_FILES,
    OFFICE_OPEN,
    OFFICE_CREATE,
    OFFICE_OUTLINE,
    OFFICE_GREP,
    OFFICE_READ,
    OFFICE_CELLS,
    OFFICE_RENDER,
    OFFICE_VERIFY,
    OFFICE_CLOSE,
    OFFICE_PROPOSE,
    OFFICE_PROPOSE_CELLS,
    OFFICE_REVIEW,
    OFFICE_ACCEPT,
    OFFICE_REJECT,
    OFFICE_EXPORT,
];

/// The ceiling row that switches them all (`GET`/`PUT /coworkers/{id}/ceiling`).
pub const ROW: &str = "office";
pub const ROW_LABEL: &str = "Office documents";
pub const ROW_DESCRIPTION: &str = "Create, read, search, edit and export .docx, .xlsx and .pptx files on this Bot's own \
     computer, as proposals you can review before they are written.";

/// The CUSTOM frame name a mutation emits so a watching client can follow the document live.
/// Its value is `{docId, path, kind, version, artifactId?, changed}` — thin metadata; the
/// client fetches bytes and pages through routes, never from the stream.
pub const OFFICE_DOC_EVENT: &str = "opengrok.officeDoc";

pub fn is_office_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// One office call, arguments read and checked as far as they go — the shapes upstream's
/// zod schemas enforce, so a call that could never run is told why now rather than after a
/// policy card.
#[derive(Debug, Clone)]
pub enum Ask {
    /// `office_files`: list office files under `directory` (defaults to `~/office`).
    Files {
        directory: Option<String>,
        offset: usize,
    },
    /// `office_open`: open a path, returning its `document` handle.
    Open { path: String },
    /// `office_create`: the payload section (`docx`/`xlsx`/`pptx`) the path's extension names.
    Create { path: String, args: Value },
    /// `office_outline`: the document's addressable surface, paged.
    Outline {
        document: String,
        story: Option<String>,
        headings_only: bool,
        offset: usize,
        limit: usize,
    },
    /// `office_grep`: literal text, matches carrying opaque handles for `office_propose`.
    Grep {
        document: String,
        query: String,
        case_sensitive: bool,
        story: Option<String>,
        limit: usize,
        cursor: Option<String>,
    },
    /// `office_read`: one ref's text, paged by `start`.
    Read {
        document: String,
        ref_: String,
        start: usize,
        length: usize,
        field: Option<String>,
    },
    /// `office_cells` (XLSX): a rectangular A1 range on a sheet.
    Cells {
        document: String,
        sheet: String,
        range: String,
        offset: usize,
        limit: usize,
    },
    /// `office_render`: a page/slide/sheet as PNG; `proposal` renders it unapplied.
    Render {
        document: String,
        page: u32,
        proposal: Option<String>,
    },
    /// `office_verify`: save and reopen the live document, or a proposal's would-be result.
    Verify {
        document: String,
        proposal: Option<String>,
    },
    /// `office_close`: drop the session; the file stays.
    Close { document: String },
    /// `office_propose` (DOCX/PPTX): stage `{match, newText}` edits.
    Propose {
        document: String,
        author: String,
        note: String,
        edits: Vec<TextEditArg>,
    },
    /// `office_propose_cells` (XLSX): stage `{cell, input}` edits.
    ProposeCells {
        document: String,
        author: String,
        note: String,
        edits: Vec<CellEditArg>,
    },
    /// `office_review`: list proposals, or inspect one.
    Review {
        document: String,
        proposal: Option<String>,
    },
    /// `office_accept`: apply a pending proposal atomically. `tracked` is DOCX's.
    Accept {
        document: String,
        proposal: String,
        tracked: bool,
    },
    /// `office_reject`: discard a pending proposal untouched.
    Reject { document: String, proposal: String },
    /// `office_export`: write the (possibly proposal-applied) document to a NEW path, never
    /// overwriting, and hand the person the exported file.
    Export {
        document: String,
        path: String,
        proposal: Option<String>,
    },
}

/// One `office_propose` edit as the model writes it.
#[derive(Debug, Clone)]
pub struct TextEditArg {
    pub match_: String,
    pub new_text: String,
}

/// One `office_propose_cells` edit.
#[derive(Debug, Clone)]
pub struct CellEditArg {
    pub cell: String,
    pub input: String,
}

/// What a desk's `answer` returns: the JSON result for the model, plus the CUSTOM frames a
/// mutation emits (`opengrok.officeDoc`) so a client watching the document sees the change as
/// it lands rather than on a poll. `image` carries `office_render`'s PNG back to the model.
pub struct Reply {
    pub result: Value,
    pub customs: Vec<Value>,
    pub image: Option<crate::ToolImage>,
}

impl Reply {
    pub fn just(result: Value) -> Self {
        Self {
            result,
            customs: Vec::new(),
            image: None,
        }
    }
}

/// The server side of the office tools: sessions, bytes, proposals and artifacts, answered
/// as `context`'s account and coworker on `box_io` — the already-resolved, already-awake box
/// provider for THIS call's target box.
#[async_trait::async_trait]
pub trait OfficeDesk: Send + Sync {
    async fn answer(
        &self,
        context: &ToolContext,
        box_io: &dyn Computer,
        box_id: &str,
        ask: Ask,
    ) -> Result<Reply, String>;

    /// `office_close` only ends a session row — no box bytes, so it answers without one and a
    /// sleeping box can never keep a document stuck open.
    async fn close(&self, context: &ToolContext, document: &str) -> Result<Reply, String>;
}

/// What `Executor::execute` holds before its gates for an office call: the call read — every
/// argument parsed, so a malformed call is refused before policy is asked about it.
pub fn admit((name, arguments): (&str, &Value)) -> Result<Ask, String> {
    read(name, arguments)
}

/// Carry out an admitted call: the desk's JSON and CUSTOM frames, or the refusal, as a result
/// either way. The frames ride the result so the run loop emits them with the result's own
/// ordering — after the call's frame, inside the turn's stream.
pub async fn run(
    desk: &dyn OfficeDesk,
    context: &ToolContext,
    box_io: &dyn Computer,
    box_id: &str,
    call_id: &str,
    ask: Ask,
) -> ToolResult {
    match desk.answer(context, box_io, box_id, ask).await {
        Ok(reply) => {
            let mut result = ToolResult::ok(call_id, reply.result.to_string());
            if let Some(image) = reply.image {
                result = result.with_image(image);
            }
            for frame in reply.customs {
                result.customs.push((OFFICE_DOC_EVENT.to_string(), frame));
            }
            result
        }
        Err(why) => ToolResult::refused(call_id, why),
    }
}

/// `office_close`, run where the other server desks run — before any box is resolved, because
/// ending a session touches none.
pub async fn run_close(
    desk: &dyn OfficeDesk,
    context: &ToolContext,
    call_id: &str,
    document: &str,
) -> ToolResult {
    match desk.close(context, document).await {
        // A closed document is a change the watching client must hear as much as an edit is.
        Ok(reply) => {
            let mut result = ToolResult::ok(call_id, reply.result.to_string());
            for frame in reply.customs {
                result.customs.push((OFFICE_DOC_EVENT.to_string(), frame));
            }
            result
        }
        Err(why) => ToolResult::refused(call_id, why),
    }
}

fn read(name: &str, arguments: &Value) -> Result<Ask, String> {
    let bad = |what: &str| format!("bad arguments: {what}");
    let get = |key: &str| arguments.get(key);
    let required = |key: &str| -> Result<String, String> {
        get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| bad(&format!("`{key}` is required")))
    };
    let bounded_string = |key: &str, max: usize| -> Result<String, String> {
        let value = required(key)?;
        if value.len() > max {
            return Err(bad(&format!("`{key}` must be at most {max} characters")));
        }
        Ok(value)
    };
    let usize_arg = |key: &str, default: usize| -> Result<usize, String> {
        match get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => value
                .as_u64()
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                .ok_or_else(|| bad(&format!("`{key}` must be a non-negative integer"))),
        }
    };
    match name {
        OFFICE_FILES => Ok(Ask::Files {
            directory: get("directory").and_then(Value::as_str).map(str::to_string),
            offset: usize_arg("offset", 0)?,
        }),
        OFFICE_OPEN => Ok(Ask::Open {
            path: required("path")?,
        }),
        OFFICE_CREATE => Ok(Ask::Create {
            path: required("path")?,
            args: arguments.clone(),
        }),
        OFFICE_OUTLINE => Ok(Ask::Outline {
            document: required("document")?,
            story: get("story").and_then(Value::as_str).map(str::to_string),
            headings_only: get("headingsOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            offset: usize_arg("offset", 0)?,
            limit: usize_arg("limit", 100)?,
        }),
        OFFICE_GREP => {
            let query = bounded_string("query", 1000)?;
            Ok(Ask::Grep {
                document: required("document")?,
                query,
                case_sensitive: get("caseSensitive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                story: get("story").and_then(Value::as_str).map(str::to_string),
                limit: usize_arg("limit", 100)?,
                cursor: get("cursor").and_then(Value::as_str).map(str::to_string),
            })
        }
        OFFICE_READ => Ok(Ask::Read {
            document: required("document")?,
            ref_: required("ref")?,
            start: usize_arg("start", 0)?,
            length: usize_arg("length", 4000)?,
            field: match get("field").and_then(Value::as_str) {
                Some(field @ ("displayText" | "formula" | "value")) => Some(field.to_string()),
                Some(other) => {
                    return Err(bad(&format!(
                        "field is displayText, formula or value, not {other}"
                    )));
                }
                None => None,
            },
        }),
        OFFICE_CELLS => Ok(Ask::Cells {
            document: required("document")?,
            sheet: required("sheet")?,
            range: bounded_string("range", 40)?,
            offset: usize_arg("offset", 0)?,
            limit: usize_arg("limit", 100)?,
        }),
        OFFICE_RENDER => Ok(Ask::Render {
            document: required("document")?,
            page: u32::try_from(usize_arg("page", 1)?).map_err(|_| bad("page overflows u32"))?,
            proposal: get("proposal").and_then(Value::as_str).map(str::to_string),
        }),
        OFFICE_VERIFY => Ok(Ask::Verify {
            document: required("document")?,
            proposal: get("proposal").and_then(Value::as_str).map(str::to_string),
        }),
        OFFICE_CLOSE => Ok(Ask::Close {
            document: required("document")?,
        }),
        OFFICE_PROPOSE => {
            let edits = get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| bad("`edits` is required"))?
                .iter()
                .map(|edit| {
                    Ok(TextEditArg {
                        match_: edit
                            .get("match")
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad("every edit needs `match`"))?
                            .to_string(),
                        new_text: edit
                            .get("newText")
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad("every edit needs `newText`"))?
                            .to_string(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if edits.is_empty() || edits.len() > 32 {
                return Err(bad("`edits` carries 1 to 32 entries"));
            }
            if edits.iter().any(|edit| edit.new_text.len() > 16000) {
                return Err(bad("a newText may be at most 16000 characters"));
            }
            Ok(Ask::Propose {
                document: required("document")?,
                author: bounded_string("author", 200).unwrap_or_else(|_| "bot".to_string()),
                note: get("note")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_default(),
                edits,
            })
        }
        OFFICE_PROPOSE_CELLS => {
            let edits = get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| bad("`edits` is required"))?
                .iter()
                .map(|edit| {
                    Ok(CellEditArg {
                        cell: edit
                            .get("cell")
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad("every edit needs `cell`"))?
                            .to_string(),
                        input: edit
                            .get("input")
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad("every edit needs `input`"))?
                            .to_string(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if edits.is_empty() || edits.len() > 32 {
                return Err(bad("`edits` carries 1 to 32 entries"));
            }
            if edits.iter().any(|edit| edit.input.len() > 16000) {
                return Err(bad("an input may be at most 16000 characters"));
            }
            Ok(Ask::ProposeCells {
                document: required("document")?,
                author: bounded_string("author", 200).unwrap_or_else(|_| "bot".to_string()),
                note: get("note")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_default(),
                edits,
            })
        }
        OFFICE_REVIEW => Ok(Ask::Review {
            document: required("document")?,
            proposal: get("proposal").and_then(Value::as_str).map(str::to_string),
        }),
        OFFICE_ACCEPT => Ok(Ask::Accept {
            document: required("document")?,
            proposal: required("proposal")?,
            tracked: get("tracked").and_then(Value::as_bool).unwrap_or(false),
        }),
        OFFICE_REJECT => Ok(Ask::Reject {
            document: required("document")?,
            proposal: required("proposal")?,
        }),
        OFFICE_EXPORT => Ok(Ask::Export {
            document: required("document")?,
            path: required("path")?,
            proposal: get("proposal").and_then(Value::as_str).map(str::to_string),
        }),
        other => Err(format!("there is no office tool called {other}")),
    }
}

/// What a tool says it is for, as a turn offers it and a ceiling describes it; the group's
/// row for `ROW`. Transcribed from upstream's descriptions, cut to the same words where they
/// still hold.
pub fn description(name: &str) -> Option<&'static str> {
    Some(match name {
        OFFICE_FILES => {
            "List DOCX, XLSX, and PPTX files and subdirectories under a directory on THIS BOT'S \
             OWN computer (defaults to ~/office), plus open document IDs. Start here when the \
             path is unknown."
        }
        OFFICE_OPEN => {
            "Open an Office file on this Bot's computer and return its document ID, format, and \
             capabilities."
        }
        OFFICE_CREATE => {
            "Create a NEW .docx, .xlsx, or .pptx on this Bot's computer; existing files are \
             never overwritten. Format comes from the path extension and requires the matching \
             payload: docx.paragraphs [{text, style: title|heading1|heading2|normal, bold, \
             italic}]; xlsx.rows string[][] of raw inputs (\"42\" number, \"=SUM(A1:A3)\" \
             formula, other text); pptx.slides [{title, bullets: string[], notes}]. The parent \
             directory must exist."
        }
        OFFICE_OUTLINE => {
            "List DOCX paragraphs, PPTX slide paragraphs, or XLSX sheets (sheetId and name). \
             Filter story; headingsOnly is DOCX-only. Paginate with nextOffset."
        }
        OFFICE_GREP => {
            "Find literal text with bounded context. DOCX/PPTX return match IDs for \
             office_propose; XLSX returns cell handles for office_propose_cells, searching \
             displayed values case-sensitively. story filters a DOCX/PPTX story or XLSX \
             sheetId. Follow nextCursor; if truncated remains true, narrow query or story."
        }
        OFFICE_READ => {
            "Read a ref from outline/grep/cells: a DOCX/PPTX paragraph or XLSX cell display \
             text. Default 4000 UTF-16 units; follow nextStart. field selects displayText \
             (default), formula, or value for XLSX."
        }
        OFFICE_CELLS => {
            "XLSX only: read a rectangular A1 range on a sheetId from office_outline. Includes \
             empty cells, inputs, formula flags, and cell handles. Follow nextOffset."
        }
        OFFICE_RENDER => {
            "Render a page as PNG using the document engine. page is one-based: a DOCX page, a \
             PPTX slide, or an XLSX sheet index. Supply proposal to see its proposed result \
             without changing the live document. Returns pageCount where the format has pages."
        }
        OFFICE_VERIFY => {
            "Save and reopen the current document or a pending proposal in memory. Reports \
             completed checks explicitly; does not assert semantic or visual correctness."
        }
        OFFICE_CLOSE => {
            "Release an open document and its proposals. Export any work you want to keep first."
        }
        OFFICE_PROPOSE => {
            "DOCX/PPTX: stage 1-32 text replacements without changing the document. XLSX uses \
             office_propose_cells. First grep for the exact text you want to replace, then copy \
             the desired occurrence's match ID. Supply edits [{match: ID_FROM_GREP, newText: \
             REPLACEMENT}]. To change part of a sentence, grep that part; do not reuse a match \
             for the whole sentence. New text inherits formatting at its start. No \
             paragraph/embedded-content/tracked-change crossings."
        }
        OFFICE_PROPOSE_CELLS => {
            "XLSX only: stage 1-32 whole-cell inputs using cell handles copied from \
             office_cells, office_read, or office_grep. input is what a user types: 123 for a \
             number, =SUM(A1:A3) for a formula, or text. Empty input clears a cell. Review \
             before accepting/exporting."
        }
        OFFICE_REVIEW => {
            "List proposals, or supply proposal to inspect exact before/after changes, \
             attribution, status, and staleness. A stale proposal must be re-created from \
             fresh reads."
        }
        OFFICE_ACCEPT => {
            "Apply a proposal to the open document atomically and write the file back. Stale \
             targets are rejected. Export afterwards to hand the person a new file."
        }
        OFFICE_REJECT => "Discard a pending proposal without editing the document.",
        OFFICE_EXPORT => {
            "Write a new file of the opened format (.docx, .xlsx, .pptx) on this Bot's \
             computer and attach it to your reply for the person. Existing files are never \
             overwritten. Supply proposal to export a proposed result without accepting it; \
             omit proposal to export accepted edits."
        }
        ROW => ROW_DESCRIPTION,
        _ => return None,
    })
}

/// The function definition a turn offers for `name`.
pub fn schema(name: &str) -> Option<Value> {
    let description = description(name)?;
    let document = json!({ "type": "string", "minLength": 1, "maxLength": 128,
        "description": "Open document ID returned by office_open." });
    let proposal = json!({ "type": "string", "minLength": 1, "maxLength": 128,
        "description": "Proposal ID returned by office_propose or office_propose_cells." });
    let path = json!({ "type": "string", "minLength": 1, "maxLength": 4096,
        "description": "Absolute path on the bot's own box." });
    let offset = json!({ "type": "integer", "minimum": 0 });
    let limit = json!({ "type": "integer", "minimum": 1, "maximum": 100 });
    let (properties, required) = match name {
        OFFICE_FILES => (
            json!({
                "directory": { "type": "string", "minLength": 1, "maxLength": 4096,
                    "description": "Directory to list; defaults to ~/office." },
                "offset": offset,
            }),
            json!([]),
        ),
        OFFICE_OPEN => (json!({ "path": path }), json!(["path"])),
        OFFICE_CREATE => (
            json!({
                "path": path,
                "docx": { "type": "object", "properties": {
                    "paragraphs": { "type": "array", "minItems": 1, "maxItems": 5000, "items": {
                        "type": "object", "properties": {
                            "text": { "type": "string", "maxLength": 16000 },
                            "style": { "type": "string", "enum": ["title", "heading1", "heading2", "normal"] },
                            "bold": { "type": "boolean" },
                            "italic": { "type": "boolean" },
                        }, "required": ["text"], "additionalProperties": false } } },
                    "additionalProperties": false },
                "xlsx": { "type": "object", "properties": {
                    "rows": { "type": "array", "minItems": 1, "maxItems": 4096, "items": {
                        "type": "array", "maxItems": 256,
                        "items": { "type": "string", "maxLength": 16000 } } } },
                    "additionalProperties": false },
                "pptx": { "type": "object", "properties": {
                    "slides": { "type": "array", "minItems": 1, "maxItems": 200, "items": {
                        "type": "object", "properties": {
                            "title": { "type": "string", "maxLength": 16000 },
                            "bullets": { "type": "array", "maxItems": 50,
                                "items": { "type": "string", "maxLength": 16000 } },
                            "notes": { "type": "string", "maxLength": 16000 },
                        }, "additionalProperties": false } } },
                    "additionalProperties": false },
            }),
            json!(["path"]),
        ),
        OFFICE_OUTLINE => (
            json!({
                "document": document,
                "story": { "type": "string", "minLength": 1, "maxLength": 4096,
                    "description": "DOCX: only 'body'. PPTX: a slide id. XLSX: a sheetId." },
                "headingsOnly": { "type": "boolean" },
                "offset": offset, "limit": limit,
            }),
            json!(["document"]),
        ),
        OFFICE_GREP => (
            json!({
                "document": document,
                "query": { "type": "string", "minLength": 1, "maxLength": 1000 },
                "caseSensitive": { "type": "boolean" },
                "story": { "type": "string", "minLength": 1, "maxLength": 4096 },
                "limit": limit,
                "cursor": { "type": "string", "minLength": 1, "maxLength": 128 },
            }),
            json!(["document", "query"]),
        ),
        OFFICE_READ => (
            json!({
                "document": document,
                "ref": { "type": "string", "minLength": 1, "maxLength": 128 },
                "start": offset,
                "length": { "type": "integer", "minimum": 1, "maximum": 16000 },
                "field": { "type": "string", "enum": ["displayText", "formula", "value"],
                    "description": "XLSX only." },
            }),
            json!(["document", "ref"]),
        ),
        OFFICE_CELLS => (
            json!({
                "document": document,
                "sheet": { "type": "string", "minLength": 1, "maxLength": 128 },
                "range": { "type": "string", "minLength": 1, "maxLength": 40,
                    "description": "A1 range such as A1:D20." },
                "offset": offset, "limit": limit,
            }),
            json!(["document", "sheet", "range"]),
        ),
        OFFICE_RENDER => (
            json!({
                "document": document,
                "page": { "type": "integer", "minimum": 1, "maximum": 100000 },
                "proposal": proposal,
            }),
            json!(["document"]),
        ),
        OFFICE_VERIFY => (
            json!({ "document": document, "proposal": proposal }),
            json!(["document"]),
        ),
        OFFICE_CLOSE => (json!({ "document": document }), json!(["document"])),
        OFFICE_PROPOSE => (
            json!({
                "document": document,
                "author": { "type": "string", "minLength": 1, "maxLength": 200 },
                "note": { "type": "string", "maxLength": 2000 },
                "edits": { "type": "array", "minItems": 1, "maxItems": 32, "items": {
                    "type": "object", "properties": {
                        "match": { "type": "string", "minLength": 1, "maxLength": 128,
                            "description": "A match handle from office_grep." },
                        "newText": { "type": "string", "maxLength": 16000 },
                    }, "required": ["match", "newText"], "additionalProperties": false } },
            }),
            json!(["document", "edits"]),
        ),
        OFFICE_PROPOSE_CELLS => (
            json!({
                "document": document,
                "author": { "type": "string", "minLength": 1, "maxLength": 200 },
                "note": { "type": "string", "maxLength": 2000 },
                "edits": { "type": "array", "minItems": 1, "maxItems": 32, "items": {
                    "type": "object", "properties": {
                        "cell": { "type": "string", "minLength": 1, "maxLength": 128,
                            "description": "A cell handle from office_cells, office_read or office_grep." },
                        "input": { "type": "string", "maxLength": 16000,
                            "description": "What a user types: 123, =SUM(A1:A3), or text. Empty clears the cell." },
                    }, "required": ["cell", "input"], "additionalProperties": false } },
            }),
            json!(["document", "edits"]),
        ),
        OFFICE_REVIEW => (
            json!({ "document": document, "proposal": proposal }),
            json!(["document"]),
        ),
        OFFICE_ACCEPT => (
            json!({
                "document": document,
                "proposal": proposal,
                "tracked": { "type": "boolean",
                    "description": "DOCX only: write the replacements as Word tracked changes." },
            }),
            json!(["document", "proposal"]),
        ),
        OFFICE_REJECT => (
            json!({ "document": document, "proposal": proposal }),
            json!(["document", "proposal"]),
        ),
        OFFICE_EXPORT => (
            json!({ "document": document, "path": path, "proposal": proposal }),
            json!(["document", "path"]),
        ),
        _ => return None,
    };
    let parameters = json!({ "type": "object", "properties": properties,
        "required": required, "additionalProperties": false });
    Some(json!({ "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters } }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Every office tool has its words, a schema, and a parse that refuses junk early.
    #[test]
    fn every_office_tool_reads_and_describes_itself() {
        for name in TOOLS {
            assert!(description(name).is_some(), "{name}");
            assert!(schema(name).is_some(), "{name}");
        }
        assert!(read(OFFICE_OPEN, &json!({})).is_err());
        assert!(read(OFFICE_READ, &json!({"document": "d", "ref": "r"})).is_ok());
        assert!(
            read(
                OFFICE_READ,
                &json!({"document": "d", "ref": "r", "field": "bogus"})
            )
            .is_err()
        );
        let propose = read(
            OFFICE_PROPOSE,
            &json!({"document": "d", "edits": [{"match": "m1.x", "newText": "hi"}]}),
        );
        assert!(matches!(propose, Ok(Ask::Propose { .. })));
        assert!(read(OFFICE_PROPOSE, &json!({"document": "d", "edits": []})).is_err());
        assert!(read(OFFICE_GREP, &json!({"document": "d", "query": "x"})).is_ok());
    }
}
