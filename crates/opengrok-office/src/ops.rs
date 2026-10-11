//! The read verbs the `office_*` tools expose: outline, grep, read, cells, verify. All return
//! `serde_json::Value` shaped for the tool result — the desk serializes them straight through,
//! so these functions own the contract's field names.
//!
//! Offsets and lengths are UTF-16 units, matching the engines and the upstream contract; a
//! `nextOffset`/`nextCursor` is `null` unless more rows exist. A `cursor` is the count of
//! matches already returned, `c<n>`: cheap to hold, stable across a reopen.

use betteroffice_docx::{Paragraph, get_paragraph_text};
use betteroffice_pptx::{FindRequest, ReadRequest};
use betteroffice_xlsx::{CellRange, CellRef, SheetId};
use serde_json::{Value, json};

use crate::proposals::{encode_match, utf16_len, utf16_slice};
use crate::{Error, Session};

/// Upper bound on rows a paged call may ask for in one go (upstream's own ceiling).
pub const MAX_LIMIT: usize = 100;
/// Paragraph/story text preview length in outlines, in UTF-16 units.
const PREVIEW: usize = 160;
/// Context shown around a grep match, in UTF-16 units each side.
const CONTEXT: usize = 40;
/// office_cells caps the rectangle read per call; more cells need the next page.
const MAX_CELLS: usize = 2000;

impl Session {
    /// `office_outline`: the document's addressable surface. DOCX lists paragraphs, PPTX the
    /// stories behind slides' text, XLSX its sheets. `headings_only` and `story` are DOCX's.
    pub fn outline(
        &self,
        story: Option<&str>,
        headings_only: bool,
        offset: usize,
        limit: usize,
    ) -> Result<Value, Error> {
        let limit = limit.clamp(1, MAX_LIMIT);
        match self {
            Self::Docx(document) => {
                if story.is_some_and(|s| s != "body") {
                    // Stories beyond the body (headers, notes) exist in the model but are not
                    // addressable through paragraphs(); say so rather than return an empty
                    // page that reads as "no headings".
                    return Err(Error::Refused(
                        "only the body story is outline-addressable".to_string(),
                    ));
                }
                let rows: Vec<Value> = document
                    .paragraphs()
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, paragraph)| {
                        let style = paragraph
                            .formatting
                            .as_ref()
                            .and_then(|f| f.style_id.clone());
                        if headings_only
                            && !style
                                .as_deref()
                                .is_some_and(|s| s.starts_with("Heading") || s == "Title")
                        {
                            return None;
                        }
                        let text = get_paragraph_text(paragraph);
                        Some(json!({
                            "ref": docx_ref(paragraph, index),
                            "story": "body",
                            "index": index,
                            "style": style,
                            "text": utf16_slice(&text, 0, PREVIEW),
                            "length": utf16_len(&text),
                        }))
                    })
                    .collect();
                Ok(paged(&rows, offset, limit, "paragraphs"))
            }
            Self::Pptx(presentation) => {
                // read_content answers a double Result: io failure outside, refusal inside.
                let read = presentation
                    .read_content(&ReadRequest {
                        slide_ids: story.map(|s| vec![s.to_string()]),
                    })?
                    .map_err(|refusal| Error::Refused(refusal.failure.message.clone()))?;
                let rows: Vec<Value> = read
                    .stories
                    .iter()
                    .map(|s| {
                        json!({
                            "ref": pptx_ref(&s.slide_id, &s.shape_id, &s.story_id),
                            "slideId": s.slide_id,
                            "shapeId": s.shape_id,
                            "storyId": s.story_id,
                            "paragraphs": s.paragraphs.len(),
                            "text": utf16_slice(&s.text, 0, PREVIEW),
                            "length": utf16_len(&s.text),
                        })
                    })
                    .collect();
                Ok(paged(&rows, offset, limit, "stories"))
            }
            Self::Xlsx(workbook) => {
                let info = workbook.sheet_info()?;
                let mut rows = Vec::new();
                for (index, name) in info.sheet_names.iter().enumerate() {
                    let id = SheetId(index as u32);
                    let used = workbook
                        .sheet(id)
                        .ok()
                        .and_then(|sheet| sheet.used_range())
                        .map(|range| range.to_a1());
                    rows.push(json!({
                        "sheetId": format!("s{index}"),
                        "name": name,
                        "usedRange": used,
                    }));
                }
                Ok(
                    json!({ "sheets": rows, "activeSheet": usize::try_from(info.active_sheet.0).unwrap_or(0) }),
                )
            }
        }
    }

    /// `office_grep`: literal text with bounded context. DOCX/PPTX matches carry `match`
    /// handles `office_propose` resolves; XLSX searches cells' typed inputs (the string a
    /// person would see in the formula bar), case-sensitively like upstream.
    pub fn grep(
        &self,
        query: &str,
        case_sensitive: bool,
        story: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Value, Error> {
        if query.is_empty() {
            return Err(Error::Refused("query must not be empty".to_string()));
        }
        let limit = limit.clamp(1, MAX_LIMIT);
        let skip = parse_cursor(cursor)?;
        match self {
            Self::Docx(document) => {
                if story.is_some_and(|s| s != "body") {
                    return Err(Error::Refused(
                        "only the body story is searchable".to_string(),
                    ));
                }
                let mut matches = Vec::new();
                let mut scanned = 0usize;
                'outer: for (index, paragraph) in document.paragraphs().into_iter().enumerate() {
                    let text = get_paragraph_text(paragraph);
                    for (start_u16, end_u16) in find_all(&text, query, case_sensitive) {
                        scanned += 1;
                        if scanned <= skip {
                            continue;
                        }
                        if matches.len() >= limit {
                            break 'outer;
                        }
                        let ref_ = docx_ref(paragraph, index);
                        matches.push(json!({
                            "match": encode_match(&ref_, start_u16 as u32, end_u16 as u32,
                                utf16_slice(&text, start_u16, end_u16)),
                            "ref": ref_,
                            "start": start_u16,
                            "end": end_u16,
                            "text": utf16_slice(&text, start_u16, end_u16),
                            "context": utf16_slice(&text, start_u16.saturating_sub(CONTEXT),
                                end_u16 + CONTEXT),
                            "contextStart": start_u16.saturating_sub(CONTEXT),
                        }));
                    }
                }
                Ok(json!({
                    "matches": matches,
                    "truncated": matches.len() >= limit,
                    "nextCursor": if matches.len() >= limit {
                        Value::String(format!("c{}", skip + matches.len()))
                    } else {
                        Value::Null
                    },
                }))
            }
            Self::Pptx(presentation) => {
                let outcome = presentation
                    .find_text(&FindRequest {
                        text: query.to_string(),
                        within: story.map(|slide| betteroffice_pptx::FindScope {
                            slide_id: slide.to_string(),
                            shape_id: None,
                            story_id: None,
                        }),
                        limit: Some((skip + limit + 1) as u32),
                    })?
                    .map_err(|refusal| Error::Refused(refusal.failure.message.clone()))?;
                let mut matches = Vec::new();
                for found in outcome.matches.iter().skip(skip).take(limit) {
                    let range = &found.range;
                    matches.push(json!({
                        "match": encode_match(
                            &pptx_ref(&range.slide_id, &range.shape_id, &range.story_id),
                            range.start, range.end, &found.text),
                        "ref": pptx_ref(&range.slide_id, &range.shape_id, &range.story_id),
                        "slideId": range.slide_id,
                        "start": range.start,
                        "end": range.end,
                        "text": found.text,
                    }));
                }
                let truncated = outcome.matches.len() > skip + matches.len() || outcome.truncated;
                Ok(json!({
                    "matches": matches,
                    "truncated": truncated,
                    "nextCursor": if truncated {
                        Value::String(format!("c{}", skip + matches.len()))
                    } else {
                        Value::Null
                    },
                }))
            }
            Self::Xlsx(workbook) => {
                let info = workbook.sheet_info()?;
                let sheet_indexes: Vec<usize> = match story {
                    Some(sheet) => vec![parse_sheet(sheet)?],
                    None => (0..info.sheet_names.len()).collect(),
                };
                let mut matches = Vec::new();
                let mut scanned = 0usize;
                'outer: for index in sheet_indexes {
                    let sheet = SheetId(index as u32);
                    let Some(range) = workbook
                        .sheet(sheet)
                        .ok()
                        .and_then(|sheet| sheet.used_range())
                    else {
                        continue;
                    };
                    for cell_ref in cells_of(&range) {
                        let edit = workbook.cell(sheet, cell_ref)?;
                        // Displayed values upstream; our readable surface is the typed input.
                        if edit.input.is_empty() || !edit.input.contains(query) {
                            continue;
                        }
                        scanned += 1;
                        if scanned <= skip {
                            continue;
                        }
                        if matches.len() >= limit {
                            break 'outer;
                        }
                        let handle = cell_ref.to_a1();
                        matches.push(json!({
                            "match": format!("c:{index}:{handle}"),
                            "cell": format!("c:{index}:{handle}"),
                            "sheetId": format!("s{index}"),
                            "input": edit.input,
                        }));
                    }
                }
                Ok(json!({
                    "matches": matches,
                    "truncated": matches.len() >= limit,
                    "nextCursor": if matches.len() >= limit {
                        Value::String(format!("c{}", skip + matches.len()))
                    } else {
                        Value::Null
                    },
                }))
            }
        }
    }

    /// `office_read`: page a ref's text. DOCX/PPTX refs come from outline/grep; XLSX takes a
    /// `c:<sheet>:<A1>` cell handle and `field` selects input (`displayText`, the default),
    /// `formula`, or `value` — all the same string today, kept so callers can say what they
    /// meant when the engines expose a distinct rendered value.
    pub fn read(
        &self,
        ref_: &str,
        start: usize,
        length: usize,
        field: Option<&str>,
    ) -> Result<Value, Error> {
        let length = length.clamp(1, 16000);
        match self {
            Self::Docx(document) => {
                if field.is_some() {
                    return Err(Error::Refused(
                        "field is an XLSX-only read option".to_string(),
                    ));
                }
                let paragraph = docx_paragraph(document, ref_)?;
                let text = get_paragraph_text(paragraph);
                let style = paragraph
                    .formatting
                    .as_ref()
                    .and_then(|f| f.style_id.clone());
                let slice = utf16_slice(&text, start, start + length);
                Ok(json!({
                    "ref": ref_,
                    "text": slice,
                    "start": start,
                    "length": utf16_len(slice),
                    "nextStart": if start + utf16_len(slice) < utf16_len(&text) {
                        Value::from(start + utf16_len(slice))
                    } else {
                        Value::Null
                    },
                    "style": style,
                    "totalLength": utf16_len(&text),
                }))
            }
            Self::Pptx(presentation) => {
                if field.is_some() {
                    return Err(Error::Refused(
                        "field is an XLSX-only read option".to_string(),
                    ));
                }
                let read = presentation
                    .read_content(&ReadRequest { slide_ids: None })?
                    .map_err(|refusal| Error::Refused(refusal.failure.message.clone()))?;
                if ref_.starts_with("sp:") {
                    // The ids in a ref each carry their own colons, so it is never split back
                    // apart: it is recognised whole against the stories it could name.
                    let Some(story_text) = read
                        .stories
                        .iter()
                        .find(|s| pptx_ref(&s.slide_id, &s.shape_id, &s.story_id) == ref_)
                    else {
                        return Err(Error::Refused(format!("no such story: {ref_}")));
                    };
                    let text = &story_text.text;
                    let slice = utf16_slice(text, start, start + length);
                    return Ok(json!({
                        "ref": ref_,
                        "text": slice,
                        "start": start,
                        "length": utf16_len(slice),
                        "nextStart": if start + utf16_len(slice) < utf16_len(text) {
                            Value::from(start + utf16_len(slice))
                        } else {
                            Value::Null
                        },
                        "totalLength": utf16_len(text),
                        "paragraphs": story_text.paragraphs.len(),
                    }));
                }
                Err(Error::Refused(format!("unreadable pptx ref: {ref_}")))
            }
            Self::Xlsx(workbook) => {
                let (sheet_index, a1) = parse_cell_handle(ref_)?;
                let cell = CellRef::parse_a1(&a1)
                    .map_err(|_| Error::Refused(format!("not a cell: {a1}")))?;
                let edit = workbook.cell(SheetId(sheet_index as u32), cell)?;
                Ok(json!({
                    "ref": ref_,
                    "text": edit.input,
                    "start": 0,
                    "length": utf16_len(&edit.input),
                    "nextStart": Value::Null,
                    "isFormula": edit.is_formula,
                    "field": field.unwrap_or("displayText"),
                }))
            }
        }
    }

    /// `office_cells` (XLSX only): a rectangular A1 range on a sheet, handles included.
    pub fn cells(
        &self,
        sheet: &str,
        range: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Value, Error> {
        let Self::Xlsx(workbook) = self else {
            return Err(Error::Refused(
                "office_cells requires an XLSX workbook".to_string(),
            ));
        };
        let sheet_index = parse_sheet(sheet)?;
        let range = CellRange::parse_a1(range)
            .map_err(|_| Error::Refused(format!("not an A1 range: {range}")))?;
        let rows = workbook.range_cells(SheetId(sheet_index as u32), range)?;
        // Flatten row-major and page over the flat list — a 200x10 sheet reads as a hundred
        // rows of twenty cells either way, and one paging model is easier to follow.
        let flat: Vec<Value> = rows
            .into_iter()
            .flat_map(|row| row.into_iter())
            .map(|edit| {
                json!({
                    "cell": format!("c:{sheet_index}:{}", edit.cell.to_a1()),
                    "input": edit.input,
                    "isFormula": edit.is_formula,
                })
            })
            .collect();
        Ok(paged(
            &flat,
            offset.min(flat.len()),
            limit.min(MAX_CELLS),
            "cells",
        ))
    }

    /// `office_verify`: save and reopen in memory — the round-trip a written file has to
    /// survive to count as a document rather than bytes we emitted.
    pub fn verify(&self) -> Result<Value, Error> {
        let bytes = self.save()?;
        let reopened = Session::open(&bytes, self.kind())?;
        let mut checks = vec!["saved", "reopened"];
        let mut detail = json!({
            "ok": true,
            "bytes": bytes.len(),
            "kind": self.kind().extension(),
        });
        match reopened {
            Self::Docx(document) => {
                checks.push("paragraphs");
                detail["paragraphs"] = json!(document.paragraphs().len());
            }
            Self::Xlsx(workbook) => {
                checks.push("sheets");
                detail["sheets"] = json!(workbook.sheet_count());
            }
            Self::Pptx(presentation) => {
                checks.push("slides");
                detail["slides"] = json!(presentation.slides().len());
            }
        }
        detail["checks"] = json!(checks);
        Ok(detail)
    }
}

/// The ref a DOCX paragraph answers to: its w14 paraId when it has one (`pid:`), else its
/// position in `paragraphs()` (`p:`). Positional refs read fine but cannot be edit targets —
/// propose refuses them, because a paragraph that moves renames its own address.
fn docx_ref(paragraph: &Paragraph, index: usize) -> String {
    match &paragraph.para_id {
        Some(id) => format!("pid:{id}"),
        None => format!("p:{index}"),
    }
}

/// The ref a PPTX story answers to.
pub(crate) fn pptx_ref(slide_id: &str, shape_id: &str, story_id: &str) -> String {
    format!("sp:{slide_id}:{shape_id}:{story_id}")
}

/// Resolve a `pid:`/`p:` ref to a paragraph.
fn docx_paragraph<'a>(
    document: &'a betteroffice_docx::Document,
    ref_: &str,
) -> Result<&'a Paragraph, Error> {
    if let Some(id) = ref_.strip_prefix("pid:") {
        return document
            .paragraph(id)
            .ok_or_else(|| Error::Refused(format!("no such paragraph: {ref_}")));
    }
    if let Some(index) = ref_.strip_prefix("p:") {
        let index: usize = index
            .parse()
            .map_err(|_| Error::Refused(format!("no such paragraph: {ref_}")))?;
        return document
            .paragraphs()
            .into_iter()
            .nth(index)
            .ok_or_else(|| Error::Refused(format!("no such paragraph: {ref_}")));
    }
    Err(Error::Refused(format!("not a docx ref: {ref_}")))
}

/// `s<index>` → the SheetId position in the workbook's sheet list.
fn parse_sheet(sheet: &str) -> Result<usize, Error> {
    sheet
        .strip_prefix('s')
        .and_then(|index| index.parse().ok())
        .ok_or_else(|| Error::Refused(format!("not a sheet handle: {sheet}")))
}

/// `c:<sheet>:<A1>` → (sheet index, A1 string).
fn parse_cell_handle(handle: &str) -> Result<(usize, String), Error> {
    let bad = || Error::Refused(format!("not a cell handle: {handle}"));
    let rest = handle.strip_prefix("c:").ok_or_else(bad)?;
    let (sheet, a1) = rest.split_once(':').ok_or_else(bad)?;
    Ok((sheet.parse().map_err(|_| bad())?, a1.to_string()))
}

/// `c<n>` → n; anything else is a stale cursor, refused rather than silently restarting.
fn parse_cursor(cursor: Option<&str>) -> Result<usize, Error> {
    match cursor {
        None => Ok(0),
        Some(c) => c
            .strip_prefix('c')
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| Error::Refused(format!("not a cursor: {c}"))),
    }
}

/// Page `rows` to [offset, offset+limit) and report `nextOffset` honestly.
fn paged(rows: &[Value], offset: usize, limit: usize, key: &str) -> Value {
    let page: Vec<Value> = rows.iter().skip(offset).take(limit).cloned().collect();
    let next = offset + page.len();
    json!({
        key: page,
        "total": rows.len(),
        "nextOffset": if next < rows.len() { Value::from(next) } else { Value::Null },
    })
}

/// Every (start, end) UTF-16 span of `query` in `text`. `case_sensitive` is honoured per
/// format: XLSX always searches typed inputs case-sensitively (upstream's contract), DOCX
/// and PPTX default to insensitive.
fn find_all(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    if case_sensitive {
        let mut from = 0;
        while let Some(at) = text[from..].find(query) {
            let byte = from + at;
            spans.push((
                utf16_len(&text[..byte]),
                utf16_len(&text[..byte + query.len()]),
            ));
            from = byte + query.len();
        }
    } else {
        let hay = text.to_lowercase();
        let needle = query.to_lowercase();
        let mut from = 0;
        while let Some(at) = hay[from..].find(&needle) {
            let byte = from + at;
            spans.push((
                utf16_len(&hay[..byte]),
                utf16_len(&hay[..byte + needle.len()]),
            ));
            from = byte + needle.len();
        }
    }
    spans
}

/// Every cell inside a used range, row-major.
fn cells_of(range: &CellRange) -> Vec<CellRef> {
    let mut cells = Vec::new();
    for row in range.start.row..=range.end.row.min(range.start.row + MAX_CELLS as u32) {
        for col in range.start.col..=range.end.col {
            cells.push(CellRef::new(row, col));
        }
    }
    cells
}
