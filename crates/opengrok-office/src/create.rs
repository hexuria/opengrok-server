//! `office_create`: bytes for a NEW document from a declared payload — the tool never
//! overwrites, so this only ever produces bytes the desk writes to a path it has already
//! checked is free.
//!
//! XLSX and PPTX start from the same blank seeds upstream's `create.ts` uses (vendored in
//! `seeds/`); DOCX has no facade-level create, so its package is templated here — a minimal
//! document part plus the four styles the payload speaks — and zipped with the crates' own
//! OPC writer rather than a second zip stack.

use betteroffice_pptx::{EditCtx, Presentation, ShapeDraft, ShapeRect, TextStyle};
use betteroffice_xlsx::{CellInput, CellRef, SheetId, Workbook};
use serde::{Deserialize, Serialize};

use crate::{Error, Kind};

/// A seeded blank workbook — the same file upstream's `createXlsxBytes` opens.
const BLANK_XLSX: &[u8] = include_bytes!("seeds/blank.xlsx");
/// A seeded blank 4:3 presentation — the same file upstream's `createPptxBytes` opens.
const BLANK_PPTX: &[u8] = include_bytes!("seeds/blank.pptx");

/// The `docx` payload of `office_create`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocxCreate {
    pub paragraphs: Vec<DocxParagraph>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocxParagraph {
    pub text: String,
    #[serde(default)]
    pub style: Option<DocxStyle>,
    #[serde(default)]
    pub bold: Option<bool>,
    #[serde(default)]
    pub italic: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DocxStyle {
    Title,
    Heading1,
    Heading2,
    Normal,
}

/// The `xlsx` payload: rows of typed inputs (`42` a number, `=SUM(A1:A3)` a formula).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct XlsxCreate {
    pub rows: Vec<Vec<String>>,
}

/// The `pptx` payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PptxCreate {
    pub slides: Vec<PptxSlide>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PptxSlide {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub bullets: Option<Vec<String>>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// `office_create` for `kind`: the payload is the format's own section of the arguments —
/// a `.docx` path that carries `xlsx:` rows is a caller bug and refused.
pub fn create_bytes(kind: Kind, args: &serde_json::Value) -> Result<Vec<u8>, Error> {
    match kind {
        Kind::Docx => {
            let payload: DocxCreate = required_section(args, "docx")?;
            if payload.paragraphs.is_empty() || payload.paragraphs.len() > 5000 {
                return Err(Error::Refused(
                    "docx requires 1 to 5000 paragraphs".to_string(),
                ));
            }
            create_docx(&payload.paragraphs)
        }
        Kind::Xlsx => {
            let payload: XlsxCreate = required_section(args, "xlsx")?;
            if payload.rows.is_empty()
                || !payload.rows.iter().any(|row| !row.is_empty())
                || payload.rows.len() > 4096
                || payload.rows.iter().any(|row| row.len() > 256)
            {
                return Err(Error::Refused(
                    "xlsx requires rows: string[][], at most 4096x256 cells".to_string(),
                ));
            }
            create_xlsx(&payload.rows)
        }
        Kind::Pptx => {
            let payload: PptxCreate = required_section(args, "pptx")?;
            if payload.slides.is_empty() || payload.slides.len() > 200 {
                return Err(Error::Refused("pptx requires 1 to 200 slides".to_string()));
            }
            create_pptx(&payload.slides)
        }
    }
}

/// `args` is the whole tool-args object; the payload for `kind` is its `<format>` member.
fn required_section<T: for<'de> Deserialize<'de>>(
    args: &serde_json::Value,
    name: &str,
) -> Result<T, Error> {
    let Some(section) = args.get(name) else {
        return Err(Error::Refused(format!(
            "a .{name} path requires a {name} payload"
        )));
    };
    serde_json::from_value(section.clone())
        .map_err(|error| Error::Refused(format!("bad {name} payload: {error}")))
}

/// The style knobs upstream's `createDocxBytes` maps each style name to (sizes are
/// half-points, spacing twentieths of a point — the units WordML speaks).
struct DocxStyleDef {
    id: &'static str,
    bold: bool,
    size: u32,
    outline: Option<u32>,
    before: u32,
    after: u32,
}

fn style_def(style: Option<DocxStyle>) -> DocxStyleDef {
    match style.unwrap_or(DocxStyle::Normal) {
        DocxStyle::Title => DocxStyleDef {
            id: "Title",
            bold: true,
            size: 64,
            outline: None,
            before: 0,
            after: 240,
        },
        DocxStyle::Heading1 => DocxStyleDef {
            id: "Heading1",
            bold: true,
            size: 40,
            outline: Some(0),
            before: 240,
            after: 120,
        },
        DocxStyle::Heading2 => DocxStyleDef {
            id: "Heading2",
            bold: true,
            size: 32,
            outline: Some(1),
            before: 160,
            after: 80,
        },
        DocxStyle::Normal => DocxStyleDef {
            id: "Normal",
            bold: false,
            size: 24,
            outline: None,
            before: 0,
            after: 0,
        },
    }
}

/// Build a complete .docx package: the content types, the package rel, the document part, a
/// styles part, and the document's rels to it — the minimum `Document::open` and Word both
/// take as a document.
fn create_docx(paragraphs: &[DocxParagraph]) -> Result<Vec<u8>, Error> {
    let mut body = String::new();
    for paragraph in paragraphs {
        let def = style_def(paragraph.style);
        body.push_str("<w:p><w:pPr>");
        if def.id != "Normal" {
            body.push_str(&format!("<w:pStyle w:val=\"{}\"/>", def.id));
        }
        if def.before > 0 || def.after > 0 {
            body.push_str(&format!(
                "<w:spacing w:before=\"{}\" w:after=\"{}\"/>",
                def.before, def.after
            ));
        }
        if let Some(level) = def.outline {
            body.push_str(&format!("<w:outlineLvl w:val=\"{level}\"/>"));
        }
        body.push_str("</w:pPr><w:r><w:rPr>");
        if paragraph.bold.unwrap_or(def.bold) {
            body.push_str("<w:b/>");
        }
        if paragraph.italic.unwrap_or(false) {
            body.push_str("<w:i/>");
        }
        body.push_str(&format!("<w:sz w:val=\"{}\"/></w:rPr>", def.size));
        body.push_str(&format!(
            "<w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
            escape_xml(&paragraph.text)
        ));
    }

    let parts = [
        (
            "[Content_Types].xml".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.to_vec(),
        ),
        (
            "_rels/.rels".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_vec(),
        ),
        (
            "word/_rels/document.xml.rels".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#.to_vec(),
        ),
        (
            "word/styles.xml".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:uiPriority w:val="9"/><w:qFormat/><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:uiPriority w:val="9"/><w:qFormat/><w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style></w:styles>"#.to_vec(),
        ),
        (
            "word/document.xml".to_string(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#
            )
            .into_bytes(),
        ),
    ];
    // The package crates.io knows as betteroffice-opc ships its lib as `ooxml_opc` — the same
    // name the engine crates import it by.
    ooxml_opc::rezip_parts(&parts).map_err(Error::Refused)
}

/// Escape the characters XML text cannot carry raw — `&` first or the later escapes would be
/// re-escaped, then the brackets that close markup, then the quotes so a run can sit inside
/// attribute-bearing templates safely.
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Rows of typed inputs onto the seeded workbook's first sheet.
fn create_xlsx(rows: &[Vec<String>]) -> Result<Vec<u8>, Error> {
    let mut workbook =
        Workbook::open_recalculated(BLANK_XLSX, betteroffice_xlsx::CalculationOptions::default())?;
    let edits: Vec<CellInput> = rows
        .iter()
        .enumerate()
        .flat_map(|(row, cells)| {
            cells.iter().enumerate().map(move |(col, input)| CellInput {
                cell: CellRef::new(row as u32, col as u32),
                input: input.clone(),
            })
        })
        .collect();
    workbook.edit_cells(
        SheetId(0),
        &edits,
        betteroffice_xlsx::CalculationOptions::default(),
    )?;
    Ok(workbook.save()?)
}

/// Slides onto the seeded 4:3 deck — the same rectangles upstream's `createPptxBytes` draws.
fn create_pptx(slides: &[PptxSlide]) -> Result<Vec<u8>, Error> {
    const TITLE: ShapeRect = ShapeRect {
        x: 457200,
        y: 274320,
        width: 8229600,
        height: 822960,
    };
    const BODY: ShapeRect = ShapeRect {
        x: 457200,
        y: 1371600,
        width: 8229600,
        height: 5029200,
    };
    let presentation = Presentation::open(BLANK_PPTX)?;
    let ctx = EditCtx::local("opengrok-office");
    for (index, slide) in slides.iter().enumerate() {
        let receipt = presentation.insert_slide(&ctx, index as u32, None)?;
        if let Some(title) = &slide.title {
            presentation.add_text_box(
                &ctx,
                &receipt.slide_id,
                &ShapeDraft {
                    name: "Title".to_string(),
                    rect: TITLE,
                    text: title.clone(),
                    style: TextStyle {
                        bold: Some(true),
                        font_size_pt: Some(32.0),
                        ..TextStyle::default()
                    },
                },
            )?;
        }
        if let Some(bullets) = &slide.bullets
            && !bullets.is_empty()
        {
            presentation.add_text_box(
                &ctx,
                &receipt.slide_id,
                &ShapeDraft {
                    name: "Body".to_string(),
                    rect: BODY,
                    text: bullets.join("\n"),
                    style: TextStyle {
                        font_size_pt: Some(18.0),
                        ..TextStyle::default()
                    },
                },
            )?;
        }
        if let Some(notes) = &slide.notes {
            presentation.set_slide_notes(&ctx, &receipt.slide_id, notes)?;
        }
    }
    Ok(presentation.save()?)
}
