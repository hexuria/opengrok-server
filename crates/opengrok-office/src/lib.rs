//! The OOXML engine behind the `office_*` tools.
//!
//! Every DOCX, XLSX and PPTX the server opens goes through [`Session`], which registers the
//! vendored metric-compatible fonts in `fonts/` before a document renders: both raster paths
//! refuse text they cannot measure, and a slim deploy host carries no fonts at all — the same
//! reason `chrono-tz` compiles its database in rather than trusting the host's tzdata.
//!
//! What this crate is not: box file I/O (opengrok-tools reads and writes the working copy),
//! session and proposal persistence (opengrok-store's `doc_sessions`), or the tool surface
//! itself (opengrok-tools' executor). Keeping the engine I/O-free is what lets a render run
//! under the executor without touching the network.

mod error;
mod fonts;
mod kind;
mod session;

pub use error::Error;
pub use kind::Kind;
pub use session::{RenderedImage, Session};
