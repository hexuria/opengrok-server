//! Proposals: `office_propose` stages changes by resolving grep's match handles into guards;
//! `office_accept` verifies every guard against the current document, then applies — all of it
//! or none of it.
//!
//! The guards are the staleness story. A doc_session row's version says the file moved; a
//! change's `old` says whether the text *this change addresses* moved, which is the honest
//! test — an accept that refused on an untouched paragraph would make unrelated edits kill
//! each other.
//!
//! DOCX text edits land as a whole-paragraph replacement: the facades edit paragraphs, not
//! ranges, so `apply` splices the UTF-16 range into the paragraph's text and writes the result
//! back. Changes inside one paragraph apply highest-offset-first so earlier guards still read
//! the text they were staged against.

use betteroffice_docx::get_paragraph_text;
use betteroffice_pptx::{
    EditFailureCode, EditRequest, EditStep, ReadRequest, TextGuard, TextRange, TextTarget,
};
use betteroffice_xlsx::{CellInput, CellRef, SheetId};
use serde_json::Value;

use crate::proposals::{Change, Proposal, ProposalStatus, decode_match, utf16_slice};
use crate::{Error, Session};

/// `office_propose` / `office_propose_cells` stage at most this many changes per proposal —
/// upstream's own cap, kept so batches stay reviewable.
pub const MAX_CHANGES: usize = 32;

/// What one `office_propose` edit carries: the `match` handle from grep, the replacement text.
pub struct TextEditInput {
    pub match_: String,
    pub new_text: String,
}

/// What one `office_propose_cells` edit carries: the `c:` cell handle, the input to type.
pub struct CellEditInput {
    pub cell: String,
    pub input: String,
}

impl Session {
    /// `office_propose` (DOCX and PPTX): resolve each `match` handle to a staged change,
    /// verifying the target still reads the matched text. Refuses XLSX — its verb is
    /// `office_propose_cells`, a separate entry point so a workbook can never get a text
    /// edit aimed at it.
    pub fn propose(
        &self,
        author: &str,
        note: &str,
        edits: &[TextEditInput],
        version: i64,
    ) -> Result<Proposal, Error> {
        if edits.is_empty() || edits.len() > MAX_CHANGES {
            return Err(Error::Refused(format!(
                "a proposal carries 1 to {MAX_CHANGES} edits"
            )));
        }
        let changes = match self {
            Self::Docx(document) => edits
                .iter()
                .map(|edit| docx_change(document, edit))
                .collect::<Result<Vec<_>, _>>()?,
            Self::Pptx(presentation) => edits
                .iter()
                .map(|edit| pptx_change(presentation, edit))
                .collect::<Result<Vec<_>, _>>()?,
            Self::Xlsx(_) => {
                return Err(Error::Refused(
                    "a workbook takes office_propose_cells".to_string(),
                ));
            }
        };
        Ok(Proposal {
            id: format!("prop_{}", uuid::Uuid::now_v7()),
            author: author.to_string(),
            note: note.to_string(),
            status: ProposalStatus::Pending,
            version,
            changes,
            stale_refs: Vec::new(),
        })
    }

    /// `office_propose_cells` (XLSX only): stage whole-cell replacements by cell handle,
    /// capturing each cell's current input as the change's guard.
    pub fn propose_cells(
        &self,
        author: &str,
        note: &str,
        edits: &[CellEditInput],
        version: i64,
    ) -> Result<Proposal, Error> {
        if edits.is_empty() || edits.len() > MAX_CHANGES {
            return Err(Error::Refused(format!(
                "a proposal carries 1 to {MAX_CHANGES} edits"
            )));
        }
        let Self::Xlsx(workbook) = self else {
            return Err(Error::Refused(
                "office_propose_cells requires an XLSX workbook".to_string(),
            ));
        };
        let mut changes = Vec::with_capacity(edits.len());
        for edit in edits {
            let (sheet, a1) = parse_cell(&edit.cell)?;
            let cell =
                CellRef::parse_a1(&a1).map_err(|_| Error::Refused(format!("not a cell: {a1}")))?;
            let current = workbook.cell(SheetId(sheet), cell)?;
            changes.push(Change::XlsxCell {
                sheet,
                cell: a1,
                old_input: current.input,
                input: edit.input.clone(),
            });
        }
        Ok(Proposal {
            id: format!("prop_{}", uuid::Uuid::now_v7()),
            author: author.to_string(),
            note: note.to_string(),
            status: ProposalStatus::Pending,
            version,
            changes,
            stale_refs: Vec::new(),
        })
    }

    /// Apply a proposal's changes atomically. Every guard is checked BEFORE anything is
    /// written; a single stale target refuses the whole proposal and names the refs, because
    /// half a staged batch is the worst outcome a review flow can produce.
    ///
    /// `tracked` (DOCX) would write the replacements as Word tracked changes; the facade's
    /// paragraph edit does not run in suggesting mode, so it is refused rather than silently
    /// ignored.
    pub fn apply(&mut self, proposal: &Proposal, tracked: bool) -> Result<Vec<String>, Error> {
        match proposal.status {
            ProposalStatus::Pending => {}
            ProposalStatus::Accepted => {
                return Err(Error::Refused(format!(
                    "proposal {} is already accepted",
                    proposal.id
                )));
            }
            ProposalStatus::Rejected => {
                return Err(Error::Refused(format!(
                    "proposal {} is rejected",
                    proposal.id
                )));
            }
        }
        match self {
            Self::Docx(_) => apply_docx(self, proposal, tracked),
            Self::Pptx(_) => apply_pptx(self, proposal),
            Self::Xlsx(_) => apply_xlsx(self, proposal),
        }
    }

    /// The proposal rendered without applying it: `office_render`/`office_verify`'s `proposal`
    /// argument reopens the saved bytes into a THROWAWAY session and applies there, so the
    /// live session and the file both stay untouched.
    pub fn applied_copy(
        bytes: &[u8],
        kind: crate::Kind,
        proposal: &Proposal,
    ) -> Result<Session, Error> {
        let mut session = Session::open(bytes, kind)?;
        session.apply(proposal, false)?;
        Ok(session)
    }

    /// `office_review`'s inspection of one proposal: each change with what its target reads
    /// NOW, so the review shows staleness instead of discovering it at accept.
    pub fn inspect(&self, proposal: &Proposal) -> Result<Value, Error> {
        let mut changes = Vec::new();
        let mut stale = Vec::new();
        for change in &proposal.changes {
            let (target, old, new, fresh) = match change {
                Change::DocxText {
                    para,
                    start,
                    end,
                    old,
                    new,
                } => {
                    let Self::Docx(document) = self else {
                        return Err(Error::Refused(
                            "a docx proposal cannot ride another kind of session".to_string(),
                        ));
                    };
                    let fresh = document
                        .paragraph(para.as_str())
                        .map(get_paragraph_text)
                        .map(|text| utf16_slice(&text, *start as usize, *end as usize).to_string());
                    (
                        format!("pid:{para}[{start}..{end}]"),
                        old.clone(),
                        new.clone(),
                        fresh,
                    )
                }
                Change::PptxText {
                    slide_id,
                    shape_id,
                    story_id,
                    start,
                    end,
                    old,
                    new,
                } => {
                    let Self::Pptx(presentation) = self else {
                        return Err(Error::Refused(
                            "a pptx proposal cannot ride another kind of session".to_string(),
                        ));
                    };
                    let fresh = presentation
                        .read_content(&ReadRequest { slide_ids: None })
                        .ok()
                        .and_then(|outcome| outcome.ok())
                        .and_then(|read| {
                            read.stories
                                .iter()
                                .find(|s| {
                                    s.slide_id == *slide_id
                                        && s.shape_id == *shape_id
                                        && s.story_id == *story_id
                                })
                                .map(|s| {
                                    utf16_slice(&s.text, *start as usize, *end as usize).to_string()
                                })
                        });
                    (
                        format!("{slide_id}/{story_id}[{start}..{end}]"),
                        old.clone(),
                        new.clone(),
                        fresh,
                    )
                }
                Change::XlsxCell {
                    sheet,
                    cell,
                    old_input,
                    input,
                } => {
                    let Self::Xlsx(workbook) = self else {
                        return Err(Error::Refused(
                            "an xlsx proposal cannot ride another kind of session".to_string(),
                        ));
                    };
                    let fresh = CellRef::parse_a1(cell)
                        .ok()
                        .and_then(|parsed| workbook.cell(SheetId(*sheet), parsed).ok())
                        .map(|edit| edit.input);
                    (
                        format!("c:{sheet}:{cell}"),
                        old_input.clone(),
                        input.clone(),
                        fresh,
                    )
                }
            };
            let is_stale = fresh.as_deref() != Some(old.as_str());
            if is_stale {
                stale.push(target.clone());
            }
            changes.push(serde_json::json!({
                "target": target,
                "old": old,
                "new": new,
                "now": fresh,
                "stale": is_stale,
            }));
        }
        Ok(serde_json::json!({
            "id": proposal.id,
            "author": proposal.author,
            "note": proposal.note,
            "status": match proposal.status {
                ProposalStatus::Pending => "pending",
                ProposalStatus::Accepted => "accepted",
                ProposalStatus::Rejected => "rejected",
            },
            "changes": changes,
            "staleRefs": stale,
        }))
    }
}

/// One change against `document`, resolved at propose time: the paragraph must still exist
/// and still read the matched text at the handle's offsets — a handle that fails that check
/// is told to grep again rather than staged blind.
fn docx_change(
    document: &betteroffice_docx::Document,
    edit: &TextEditInput,
) -> Result<Change, Error> {
    let (ref_, start, end, text) = decode_match(&edit.match_)?;
    let Some(id) = ref_.strip_prefix("pid:") else {
        return Err(Error::Refused(format!(
            "match {ref_} targets a paragraph without a stable id"
        )));
    };
    let paragraph = document
        .paragraph(id)
        .ok_or_else(|| Error::Refused(format!("paragraph {id} is gone")))?;
    let current = get_paragraph_text(paragraph);
    if utf16_slice(&current, start as usize, end as usize) != text.as_str() {
        return Err(Error::Refused(format!(
            "the paragraph at {ref_} no longer reads what the match found; grep again"
        )));
    }
    Ok(Change::DocxText {
        para: id.to_string(),
        start,
        end,
        old: text,
        new: edit.new_text.clone(),
    })
}

/// One change against `presentation`, resolved the same way against the story's live text.
fn pptx_change(
    presentation: &betteroffice_pptx::Presentation,
    edit: &TextEditInput,
) -> Result<Change, Error> {
    let (ref_, start, end, text) = decode_match(&edit.match_)?;
    let Some((slide, shape, story)) = ref_.strip_prefix("sp:").and_then(|rest| {
        let mut parts = rest.splitn(3, ':');
        Some((
            parts.next()?.to_string(),
            parts.next()?.to_string(),
            parts.next()?.to_string(),
        ))
    }) else {
        return Err(Error::Refused(format!(
            "match {ref_} does not name a pptx story"
        )));
    };
    // read_content answers a double Result: io-style failure outside, an edit refusal inside.
    let read = presentation
        .read_content(&ReadRequest { slide_ids: None })?
        .map_err(|refusal| Error::Refused(refusal.failure.message.clone()))?;
    let story_text = read
        .stories
        .iter()
        .find(|s| s.slide_id == slide && s.shape_id == shape && s.story_id == story)
        .ok_or_else(|| Error::Refused(format!("story {story} is gone")))?;
    if utf16_slice(&story_text.text, start as usize, end as usize) != text.as_str() {
        return Err(Error::Refused(format!(
            "the story at {ref_} no longer reads what the match found; grep again"
        )));
    }
    Ok(Change::PptxText {
        slide_id: slide,
        shape_id: shape,
        story_id: story,
        start,
        end,
        old: text,
        new: edit.new_text.clone(),
    })
}

fn apply_docx(
    session: &mut Session,
    proposal: &Proposal,
    tracked: bool,
) -> Result<Vec<String>, Error> {
    if tracked {
        return Err(Error::Refused(
            "tracked changes need the editing session's suggesting mode, which the paragraph \
             facade does not drive; accept without tracked, or export and track in Word"
                .to_string(),
        ));
    }
    let Session::Docx(document) = session else {
        return Err(Error::Refused("not a docx session".to_string()));
    };
    // Verify every guard before writing anything — a half-applied proposal is worse than a
    // refused one, and naming all the stale refs tells the model exactly what to re-read.
    let mut stale = Vec::new();
    for change in &proposal.changes {
        let Change::DocxText {
            para,
            start,
            end,
            old,
            ..
        } = change
        else {
            return Err(Error::Refused(
                "a docx session only applies docx changes".to_string(),
            ));
        };
        let holds = document
            .paragraph(para.as_str())
            .map(get_paragraph_text)
            .map(|text| utf16_slice(&text, *start as usize, *end as usize) == old.as_str())
            .unwrap_or(false);
        if !holds {
            stale.push(format!("pid:{para}[{start}..{end}]"));
        }
    }
    if !stale.is_empty() {
        return Err(Error::StaleTargets(stale));
    }
    // Group by paragraph; splice highest-offset-first inside each so a change's stored
    // offsets stay true until its own splice runs.
    let mut by_para: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (index, change) in proposal.changes.iter().enumerate() {
        if let Change::DocxText { para, .. } = change {
            by_para.entry(para.clone()).or_default().push(index);
        }
    }
    for (para, mut indexes) in by_para {
        indexes.sort_by(|a, b| {
            let start_of = |i: usize| match &proposal.changes[i] {
                Change::DocxText { start, .. } => *start,
                _ => 0,
            };
            start_of(*b).cmp(&start_of(*a))
        });
        let text = document
            .paragraph(&para)
            .map(get_paragraph_text)
            .ok_or_else(|| Error::Refused(format!("paragraph {para} went away mid-apply")))?;
        let mut units: Vec<u16> = text.encode_utf16().collect();
        for index in indexes {
            if let Change::DocxText {
                start, end, new, ..
            } = &proposal.changes[index]
            {
                units.splice(*start as usize..*end as usize, new.encode_utf16());
            }
        }
        let replaced = String::from_utf16(&units)
            .map_err(|_| Error::Refused("an edit split a UTF-16 pair".to_string()))?;
        document
            .replace_paragraph_text(&para, &replaced)
            .map_err(|error| Error::Refused(format!("paragraph {para}: {error}")))?;
    }
    Ok((0..proposal.changes.len())
        .map(|index| format!("change {index} applied"))
        .collect())
}

fn apply_pptx(session: &mut Session, proposal: &Proposal) -> Result<Vec<String>, Error> {
    let Session::Pptx(presentation) = session else {
        return Err(Error::Refused("not a pptx session".to_string()));
    };
    // Each step carries a TextGuard so the engine itself refuses a range that no longer reads
    // `old`; the request versions against THIS session's token so a document reopened since
    // the proposal was staged is not wedged by a nonce it never saw.
    let mut steps = Vec::with_capacity(proposal.changes.len());
    for change in &proposal.changes {
        let Change::PptxText {
            slide_id,
            shape_id,
            story_id,
            start,
            end,
            old,
            new,
        } = change
        else {
            return Err(Error::Refused(
                "a pptx session only applies pptx changes".to_string(),
            ));
        };
        steps.push(EditStep::ReplaceText {
            target: TextTarget::Range(TextRange {
                slide_id: slide_id.clone(),
                shape_id: shape_id.clone(),
                story_id: story_id.clone(),
                start: *start,
                end: *end,
            }),
            text: new.clone(),
            expect: Some(TextGuard { text: old.clone() }),
        });
    }
    let outcome = presentation.apply_edits(&EditRequest {
        expect_version: presentation.version(),
        source: betteroffice_pptx::EditSource::Agent,
        history: betteroffice_pptx::EditHistory::None,
        steps,
    })?;
    match outcome {
        Ok(application) => {
            if !application.applied {
                return Err(Error::Refused("every step was a no-op".to_string()));
            }
            Ok(application
                .receipts
                .iter()
                .enumerate()
                .map(|(index, receipt)| format!("step {index}: {receipt:?}"))
                .collect())
        }
        Err(refusal) => {
            let is_stale = matches!(
                refusal.failure.code,
                EditFailureCode::StaleVersion
                    | EditFailureCode::MissingTarget
                    | EditFailureCode::ContentMismatch
            );
            let target = refusal
                .failure
                .target
                .as_ref()
                .map(|t| format!("{t:?}"))
                .unwrap_or_default();
            if is_stale {
                Err(Error::StaleTargets(vec![target]))
            } else {
                Err(Error::Refused(format!(
                    "{:?}: {}",
                    refusal.failure.code, refusal.failure.message
                )))
            }
        }
    }
}

fn apply_xlsx(session: &mut Session, proposal: &Proposal) -> Result<Vec<String>, Error> {
    let Session::Xlsx(workbook) = session else {
        return Err(Error::Refused("not an xlsx session".to_string()));
    };
    // Verify every guard first; then edit_cells applies the batch atomically per sheet.
    let mut stale = Vec::new();
    for change in &proposal.changes {
        let Change::XlsxCell {
            sheet,
            cell,
            old_input,
            ..
        } = change
        else {
            return Err(Error::Refused(
                "an xlsx session only applies xlsx changes".to_string(),
            ));
        };
        let parsed =
            CellRef::parse_a1(cell).map_err(|_| Error::Refused(format!("not a cell: {cell}")))?;
        let current = workbook.cell(SheetId(*sheet), parsed)?;
        if current.input != *old_input {
            stale.push(format!("c:{sheet}:{cell}"));
        }
    }
    if !stale.is_empty() {
        return Err(Error::StaleTargets(stale));
    }
    let mut sheets: std::collections::BTreeMap<u32, Vec<CellInput>> =
        std::collections::BTreeMap::new();
    for change in &proposal.changes {
        if let Change::XlsxCell {
            sheet, cell, input, ..
        } = change
            && let Ok(parsed) = CellRef::parse_a1(cell)
        {
            sheets.entry(*sheet).or_default().push(CellInput {
                cell: parsed,
                input: input.clone(),
            });
        }
    }
    for (sheet, batch) in sheets {
        workbook.edit_cells(
            SheetId(sheet),
            &batch,
            betteroffice_xlsx::CalculationOptions::default(),
        )?;
    }
    Ok((0..proposal.changes.len())
        .map(|index| format!("change {index} applied"))
        .collect())
}

/// `c:<sheet>:<A1>` → (sheet index, A1). Kept beside the ops module's writer so both halves
/// of the handle live in this crate.
fn parse_cell(handle: &str) -> Result<(u32, String), Error> {
    let bad = || Error::Refused(format!("not a cell handle: {handle}"));
    let rest = handle.strip_prefix("c:").ok_or_else(bad)?;
    let (sheet, a1) = rest.split_once(':').ok_or_else(bad)?;
    Ok((sheet.parse().map_err(|_| bad())?, a1.to_string()))
}
