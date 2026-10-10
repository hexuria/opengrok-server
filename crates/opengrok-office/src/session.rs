use betteroffice_docx::{
    Document, ExportOptions as DocxExportOptions, RevisionView as DocxRevisionView,
};
use betteroffice_pptx::{PptxExportOptions, Presentation, RenderOptions as PptxRenderOptions};
use betteroffice_xlsx::{RenderOptions as XlsxRenderOptions, SheetId, Workbook};

use crate::{Error, Kind, fonts};

/// One open document. DOCX and PPTX get the vendored fonts at open rather than at first
/// render: a document that never renders still pays once, and a document that renders twice
/// never pays twice — and an unregistered font is a render-time refusal, which is the worst
/// place to learn the registration was skipped.
// The facades are kilobyte-scale structs, so the variants are boxed: a Session moves between
// the tool call that opened it and the store row that owns it, and an 11 KB enum makes every
// one of those moves a memcpy.
pub enum Session {
    Docx(Box<Document>),
    Xlsx(Box<Workbook>),
    Pptx(Box<Presentation>),
}

/// A rasterized page, slide or used-range, ready to ride a tool result's image field.
#[derive(Debug)]
pub struct RenderedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Session {
    pub fn open(bytes: &[u8], kind: Kind) -> Result<Self, Error> {
        match kind {
            Kind::Docx => {
                let mut document = Document::open(bytes)?;
                fonts::register_in_docx(&mut document)?;
                Ok(Self::Docx(Box::new(document)))
            }
            Kind::Xlsx => Ok(Self::Xlsx(Box::new(Workbook::open_recalculated(
                bytes,
                betteroffice_xlsx::CalculationOptions::default(),
            )?))),
            Kind::Pptx => {
                let mut presentation = Presentation::open(bytes)?;
                fonts::register_in_pptx(&mut presentation)?;
                Ok(Self::Pptx(Box::new(presentation)))
            }
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Self::Docx(_) => Kind::Docx,
            Self::Xlsx(_) => Kind::Xlsx,
            Self::Pptx(_) => Kind::Pptx,
        }
    }

    /// The document as the model reads it: bounded, anchored markdown. XLSX refuses — its
    /// readable surface is cells and sheet metadata, which the tools expose separately.
    pub fn markdown(&self) -> Result<String, Error> {
        match self {
            Self::Docx(document) => Ok(document
                .export_markdown(&DocxExportOptions::new(DocxRevisionView::Accepted))?
                .markdown),
            Self::Pptx(presentation) => {
                match presentation.export_markdown(&PptxExportOptions::default())? {
                    Ok(read) => Ok(read.content.markdown),
                    Err(refusal) => Err(Error::Refused(refusal.failure.to_string())),
                }
            }
            Self::Xlsx(_) => Err(Error::XlsxNoMarkdown),
        }
    }

    /// Page/slide/sheet `index`, zero-based. DOCX refuses until the editing-session layout
    /// pipeline is wired — see [`Error::DocxLayoutNotWired`].
    pub fn render_png(&self, index: usize) -> Result<RenderedImage, Error> {
        match self {
            Self::Docx(_) => Err(Error::DocxLayoutNotWired),
            Self::Xlsx(workbook) => {
                let sheet = SheetId(
                    u32::try_from(index)
                        .map_err(|_| Error::Refused("sheet index does not fit u32".to_string()))?,
                );
                let png = workbook.render_sheet(sheet, &XlsxRenderOptions::default())?;
                Ok(RenderedImage {
                    bytes: png.bytes,
                    width: png.width,
                    height: png.height,
                })
            }
            Self::Pptx(presentation) => {
                let png = presentation.render_png(index, &PptxRenderOptions::default())?;
                Ok(RenderedImage {
                    bytes: png.bytes,
                    width: png.width,
                    height: png.height,
                })
            }
        }
    }

    pub fn save(&self) -> Result<Vec<u8>, Error> {
        match self {
            Self::Docx(document) => Ok(document.save()?),
            Self::Xlsx(workbook) => Ok(workbook.save()?),
            Self::Pptx(presentation) => Ok(presentation.save()?),
        }
    }
}
