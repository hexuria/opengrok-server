//! The staged-edit vocabulary the `office_*` tools persist on a `doc_session` row.
//!
//! A proposal is DATA, not a live object: it has to survive the session being reopened from
//! bytes between a `propose` on one turn and an `accept` on a later one, so it serializes to a
//! `doc_session.proposals` jsonb array and carries every fact its own application needs. The
//! per-change `old` guard is the staleness check — a document version only says *something*
//! moved, while a guard says whether the text this change targets still reads the same.

use serde::{Deserialize, Serialize};

use crate::Error;

/// One staged batch, returned by `office_propose`/`office_propose_cells` and listed by
/// `office_review`. The id is what `office_accept` and `office_reject` name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub id: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub status: ProposalStatus,
    /// The session version the changes were read against. Kept for review's "staged before
    /// version N" display; correctness comes from each change's `old` guard.
    pub version: i64,
    pub changes: Vec<Change>,
    /// Targets that failed their guard at the last apply attempt; empty while pending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProposalStatus {
    Pending,
    Accepted,
    Rejected,
}

/// One change inside a proposal. The variants carry the format-specific target plus the `old`
/// text/input the target read when the proposal was staged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Change {
    /// Replace the UTF-16 range `[start, end)` of the DOCX paragraph named by `para` (its
    /// w14 paraId) with `new`.
    DocxText {
        para: String,
        start: u32,
        end: u32,
        old: String,
        new: String,
    },
    /// Replace the UTF-16 range `[start, end)` of one PPTX story's projected text with `new`.
    PptxText {
        slide_id: String,
        shape_id: String,
        story_id: String,
        start: u32,
        end: u32,
        old: String,
        new: String,
    },
    /// Set one XLSX cell's whole input — what a person would type: `123` is a number,
    /// `=SUM(A1:A3)` a formula, anything else text; empty clears the cell. `old_input` is the
    /// input the cell carried when the proposal was staged.
    XlsxCell {
        sheet: u32,
        /// The A1 name, `B4`.
        cell: String,
        old_input: String,
        input: String,
    },
}

/// What a `grep` match encodes so a later `propose` can resolve it without a lookup table:
/// the paragraph/story ref, the matched slice's UTF-16 range and the matched text itself.
/// Self-describing rather than session-scoped because a session can reopen between the grep
/// that issued the handle and the propose that spends it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchHandle {
    /// `pid:<paraId>` / `p:<index>` for DOCX; `sp:<slideId>:<shapeId>:<storyId>` for PPTX.
    #[serde(rename = "r")]
    ref_: String,
    /// Start and end in UTF-16 units within the ref's text.
    #[serde(rename = "s")]
    start: u32,
    #[serde(rename = "e")]
    end: u32,
    /// The text the match covered — proposed `old` values must equal it.
    #[serde(rename = "t")]
    text: String,
}

/// Encode a match as an opaque `m1.` handle. Versioned: a `m2.` can change the payload
/// without old handles decoding wrongly.
pub fn encode_match(ref_: &str, start: u32, end: u32, text: &str) -> String {
    let handle = MatchHandle {
        ref_: ref_.to_string(),
        start,
        end,
        text: text.to_string(),
    };
    use base64::Engine;
    let payload = serde_json::to_vec(&handle).unwrap_or_default();
    format!(
        "m1.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    )
}

/// Decode a `m1.` handle back to (ref, start, end, text).
pub fn decode_match(handle: &str) -> Result<(String, u32, u32, String), Error> {
    use base64::Engine;
    let bad = || Error::Refused(format!("not a match handle: {handle}"));
    let payload = handle.strip_prefix("m1.").ok_or_else(bad)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| bad())?;
    let parsed: MatchHandle = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    Ok((parsed.ref_, parsed.start, parsed.end, parsed.text))
}

/// UTF-16 units, the offset unit the contract speaks: the engines and every client count in
/// them, so reads and edits agree on where a slice sits.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The byte offset where UTF-16 unit `unit` falls in `text`, or `text.len()` past the end.
pub fn utf16_to_byte(text: &str, unit: usize) -> usize {
    let mut seen = 0usize;
    for (offset, _) in text.char_indices() {
        if seen >= unit {
            return offset;
        }
        seen += char_len_utf16_at(text, offset);
    }
    text.len()
}

/// Slice `text` to UTF-16 units `[start, end)`. Unit edges landing inside a grapheme land on
/// the surrounding char boundary — a match the engine produced never does that, so the
/// guard that compares the slice to `old` is the only check needed.
pub fn utf16_slice(text: &str, start: usize, end: usize) -> &str {
    if start >= end || start >= utf16_len(text) {
        return "";
    }
    let from = utf16_to_byte(text, start);
    let to = utf16_to_byte(text, end.min(utf16_len(text)));
    text.get(from..to).unwrap_or("")
}

// `char::len_utf16` is stable; the At variant here exists only because iterating with byte
// offsets already has them.
fn char_len_utf16_at(text: &str, offset: usize) -> usize {
    text[offset..]
        .chars()
        .next()
        .map(char::len_utf16)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_match_handle_round_trips() -> Result<(), Error> {
        let handle = encode_match("pid:004A1B2C", 4, 9, "hello");
        let (ref_, start, end, text) = decode_match(&handle)?;
        assert_eq!(
            (ref_.as_str(), start, end, text.as_str()),
            ("pid:004A1B2C", 4, 9, "hello")
        );
        assert!(decode_match("nope").is_err());
        assert!(decode_match("m1.%%%").is_err());
        Ok(())
    }

    #[test]
    fn utf16_offsets_agree_with_emoji() {
        let text = "a🦀bc";
        assert_eq!(utf16_len(text), 5); // 🦀 is two units
        assert_eq!(utf16_slice(text, 1, 3), "🦀");
        assert_eq!(utf16_slice(text, 3, 5), "bc");
    }
}
