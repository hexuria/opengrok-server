use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("docx: {0}")]
    Docx(#[from] betteroffice_docx::Error),

    #[error("xlsx: {0}")]
    Xlsx(#[from] betteroffice_xlsx::Error),

    #[error("pptx: {0}")]
    Pptx(#[from] betteroffice_pptx::Error),

    /// DOCX pagination is measured: `Document::render_png` takes a `DisplayList` it does not
    /// build, and the producer is the `docx_edit` engine session (`build_display_list_frame`).
    /// That wiring lands with the editing surface rather than here, because a session that
    /// only renders should not pay for the CRDT machinery.
    #[error("docx page render needs the editing-session layout pipeline, which is not wired yet")]
    DocxLayoutNotWired,

    /// XLSX's readable export is cells and sheet metadata, not Markdown; the office_read and
    /// office_cells tools go through those instead.
    #[error("xlsx has no markdown export; read cells instead")]
    XlsxNoMarkdown,

    /// An engine refusal is a value, not an exception: it reaches the model as a result it
    /// can reason about rather than killing the call.
    #[error("refused: {0}")]
    Refused(String),
}
