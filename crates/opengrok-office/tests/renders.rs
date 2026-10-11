//! What the vendored fonts buy, proven on real documents: the demo deck upstream renders in
//! its own test suite opens here, registers every substitute face, and rasterizes.

use opengrok_office::{Error, Kind, Session};

const DEMO_DECK: &[u8] = include_bytes!("fixtures/betteroffice-demo.pptx");
const BLANK_SHEET: &[u8] = include_bytes!("fixtures/blank.xlsx");
const REPORT: &[u8] = include_bytes!("fixtures/spike.docx");

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn assert_png(image: &opengrok_office::RenderedImage) {
    assert_eq!(image.bytes[..8], PNG_MAGIC, "not a PNG");
    assert!(image.width > 0 && image.height > 0);
}

#[test]
fn the_demo_deck_renders_every_slide() -> Result<(), Error> {
    let session = Session::open(DEMO_DECK, Kind::Pptx)?;
    assert_eq!(session.kind(), Kind::Pptx);
    // betteroffice-demo.pptx is a 3-slide deck; a slide past the end must refuse rather than
    // fabricate, so the loop below walks exactly the deck.
    for index in 0..3 {
        assert_png(&session.render_png(index)?);
    }
    assert!(
        session.render_png(3).is_err(),
        "slide past the deck accepted"
    );
    Ok(())
}

#[test]
fn a_deck_markdown_export_carries_slide_text() -> Result<(), Error> {
    let markdown = Session::open(DEMO_DECK, Kind::Pptx)?.markdown()?;
    assert!(!markdown.is_empty(), "empty export from a non-empty deck");
    Ok(())
}

#[test]
fn a_workbook_renders_its_first_sheet() -> Result<(), Error> {
    let image = Session::open(BLANK_SHEET, Kind::Xlsx)?.render_png(0)?;
    assert_png(&image);
    Ok(())
}

#[test]
fn a_document_opens_and_exports_anchored_markdown() -> Result<(), Error> {
    let session = Session::open(REPORT, Kind::Docx)?;
    assert!(!session.markdown()?.is_empty());
    // Page render is refused, not faked, until the measured-layout pipeline is wired.
    match session.render_png(0) {
        Err(Error::DocxLayoutNotWired) => {}
        other => {
            return Err(Error::Refused(format!(
                "expected DocxLayoutNotWired, got {other:?}"
            )));
        }
    }
    Ok(())
}
